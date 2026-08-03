use super::{RuntimeStore, StoreError};
use needle_core::{Digest, RoleProfileDefinition, RoleProfileRevision, RoleProfileState};
use rusqlite::{Connection, OptionalExtension, params};

#[path = "role_profiles/audit.rs"]
mod audit;
#[path = "role_profiles/state.rs"]
mod state;
use audit::{RoleProfileAuditInput, insert_audit};
pub use audit::{RoleProfileAuditOperation, RoleProfileAuditRecord};
pub use state::RoleProfileStateRecord;
use state::{
    compare_state, derived_revision_state, load_state, revision_with_state, validation_error,
};

impl RuntimeStore {
    pub fn create_role_profile(
        &self,
        definition: RoleProfileDefinition,
    ) -> Result<RoleProfileRevision, StoreError> {
        validate_definition(&definition)?;
        self.initialize()?;
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let profile_id = definition.profile_id.as_str();
        let exists: Option<i64> = transaction
            .query_row("SELECT 1 FROM role_profiles WHERE profile_id=?1", [profile_id], |row| {
                row.get(0)
            })
            .optional()?;
        if exists.is_some() {
            return Err(StoreError::RoleProfileConflict(format!(
                "profile `{profile_id}` already exists"
            )));
        }
        let now = super::now_ms();
        let definition_json = serde_json::to_string(&definition)?;
        transaction.execute(
            "INSERT INTO role_profiles(profile_id, role, created_unix_ms)
             VALUES(?1, ?2, ?3)",
            params![profile_id, definition.role.as_str(), now,],
        )?;
        transaction.execute(
            "INSERT INTO role_profile_revisions(profile_id, revision, definition_digest, definition_json, created_unix_ms, activated_unix_ms)
             VALUES(?1, 1, ?2, ?3, ?4, NULL)",
            params![profile_id, definition.definition_digest.to_string(), definition_json, now],
        )?;
        transaction.execute(
            "INSERT INTO role_profile_state(profile_id, latest_revision, active_revision, state_generation, updated_unix_ms)
             VALUES(?1, 1, NULL, 0, ?2)",
            params![profile_id, now],
        )?;
        let state = load_state(&transaction, profile_id)?;
        insert_audit(
            &transaction,
            RoleProfileAuditInput {
                profile_id,
                revision: 1,
                definition_digest: definition.definition_digest,
                operation: RoleProfileAuditOperation::Create,
                prior_state: None,
                resulting_state: RoleProfileState::Draft,
                prior_state_digest: None,
                resulting: &state,
                prior_active_revision: None,
                prior_active_digest: None,
                now,
            },
        )?;
        transaction.commit()?;
        revision_with_state(
            definition.profile_id.clone(),
            1,
            definition,
            RoleProfileState::Draft,
            now,
            None,
        )
    }

    pub fn revise_role_profile(
        &self,
        profile_id: &needle_core::RoleProfileId,
        expected_state_digest: Digest,
        definition: RoleProfileDefinition,
    ) -> Result<RoleProfileRevision, StoreError> {
        validate_definition(&definition)?;
        if definition.profile_id != *profile_id {
            return Err(StoreError::RoleProfileValidation(
                "revision definition profile_id does not match the target profile".to_owned(),
            ));
        }
        self.initialize()?;
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state = load_state(&transaction, profile_id.as_str())?;
        if state.state_digest != expected_state_digest {
            return Err(StoreError::RoleProfileConflict(
                "stale role-profile state digest".to_owned(),
            ));
        }
        let stored_role: String = transaction.query_row(
            "SELECT role FROM role_profiles WHERE profile_id=?1",
            [profile_id.as_str()],
            |row| row.get(0),
        )?;
        if stored_role != definition.role.as_str() {
            return Err(StoreError::RoleProfileValidation(
                "revision role does not match the profile identity".to_owned(),
            ));
        }
        let duplicate: Option<i64> = transaction
            .query_row(
                "SELECT revision FROM role_profile_revisions WHERE definition_digest=?1",
                [definition.definition_digest.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if duplicate.is_some() {
            return Err(StoreError::RoleProfileConflict(
                "identical role-profile definition already exists".to_owned(),
            ));
        }
        let revision = state
            .latest_revision
            .checked_add(1)
            .ok_or_else(|| StoreError::RoleProfileCorruption("revision overflow".to_owned()))?;
        let now = super::now_ms();
        let definition_json = serde_json::to_string(&definition)?;
        transaction.execute(
            "INSERT INTO role_profile_revisions(profile_id, revision, definition_digest, definition_json, created_unix_ms, activated_unix_ms)
             VALUES(?1, ?2, ?3, ?4, ?5, NULL)",
            params![
                profile_id.as_str(),
                revision,
                definition.definition_digest.to_string(),
                definition_json,
                now,
            ],
        )?;
        let next_generation = state.state_generation.checked_add(1).ok_or_else(|| {
            StoreError::RoleProfileCorruption("state generation overflow".to_owned())
        })?;
        let changed = transaction.execute(
            "UPDATE role_profile_state SET latest_revision=?2, state_generation=?3, updated_unix_ms=?4 WHERE profile_id=?1 AND state_generation=?5",
            params![profile_id.as_str(), revision, next_generation, now, state.state_generation],
        )?;
        if changed != 1 {
            return Err(StoreError::RoleProfileCorruption(
                "role-profile state CAS did not update exactly one row".to_owned(),
            ));
        }
        let next_state = load_state(&transaction, profile_id.as_str())?;
        let prior_revision =
            load_revision(&transaction, profile_id.as_str(), state.latest_revision)?;
        let prior_state = derived_revision_state(&state, state.latest_revision);
        insert_audit(
            &transaction,
            RoleProfileAuditInput {
                profile_id: profile_id.as_str(),
                revision,
                definition_digest: definition.definition_digest,
                operation: RoleProfileAuditOperation::Revise,
                prior_state: Some(prior_state),
                resulting_state: RoleProfileState::Draft,
                prior_state_digest: Some(state.state_digest),
                resulting: &next_state,
                prior_active_revision: None,
                prior_active_digest: None,
                now,
            },
        )?;
        transaction.commit()?;
        let _ = prior_revision;
        revision_with_state(
            profile_id.clone(),
            revision,
            definition,
            RoleProfileState::Draft,
            now,
            None,
        )
    }

    pub fn read_role_profile_revision(
        &self,
        profile_id: &needle_core::RoleProfileId,
        revision: u64,
    ) -> Result<RoleProfileRevision, StoreError> {
        self.initialize()?;
        let connection = self.connection()?;
        let state = load_state(&connection, profile_id.as_str())?;
        let (definition, created, activated) =
            load_revision(&connection, profile_id.as_str(), revision)?;
        let result = revision_with_state(
            profile_id.clone(),
            revision,
            definition,
            derived_revision_state(&state, revision),
            created,
            activated,
        )?;
        Ok(result)
    }

    pub fn read_role_profile_revision_by_digest(
        &self,
        profile_id: &needle_core::RoleProfileId,
        digest: Digest,
    ) -> Result<RoleProfileRevision, StoreError> {
        self.initialize()?;
        let connection = self.connection()?;
        let revision: Option<u64> = connection
            .query_row(
                "SELECT revision FROM role_profile_revisions WHERE profile_id=?1 AND definition_digest=?2",
                params![profile_id.as_str(), digest.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(revision) = revision else {
            return Err(StoreError::RoleProfileNotFound(format!("{}@{}", profile_id, digest)));
        };
        drop(connection);
        self.read_role_profile_revision(profile_id, revision)
    }

    pub fn read_role_profile_revision_by_digest_global(
        &self,
        digest: Digest,
    ) -> Result<RoleProfileRevision, StoreError> {
        self.initialize()?;
        let connection = self.connection()?;
        let identity: Option<(String, u64)> = connection
            .query_row(
                "SELECT profile_id, revision FROM role_profile_revisions WHERE definition_digest=?1",
                [digest.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((profile_id, revision)) = identity else {
            return Err(StoreError::RoleProfileNotFound(digest.to_string()));
        };
        let id = needle_core::RoleProfileId::new(profile_id).map_err(validation_error)?;
        drop(connection);
        self.read_role_profile_revision(&id, revision)
    }

    pub fn list_role_profile_revisions(
        &self,
        profile_id: &needle_core::RoleProfileId,
    ) -> Result<Vec<RoleProfileRevision>, StoreError> {
        self.initialize()?;
        let connection = self.connection()?;
        let state = load_state(&connection, profile_id.as_str())?;
        let mut statement = connection.prepare(
            "SELECT revision, definition_json, definition_digest, created_unix_ms, activated_unix_ms
             FROM role_profile_revisions WHERE profile_id=?1 ORDER BY revision ASC",
        )?;
        let rows = statement.query_map([profile_id.as_str()], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, Option<u64>>(4)?,
            ))
        })?;
        let mut revisions = Vec::new();
        for row in rows {
            let (revision, json, digest, created, activated) = row?;
            let definition = parse_definition(&json, &digest, profile_id.as_str())?;
            revisions.push(revision_with_state(
                profile_id.clone(),
                revision,
                definition,
                derived_revision_state(&state, revision),
                created,
                activated,
            )?);
        }
        if revisions.is_empty() {
            return Err(StoreError::RoleProfileNotFound(profile_id.to_string()));
        }
        Ok(revisions)
    }

    pub fn role_profile_state(
        &self,
        profile_id: &needle_core::RoleProfileId,
    ) -> Result<RoleProfileStateRecord, StoreError> {
        self.initialize()?;
        let connection = self.connection()?;
        load_state(&connection, profile_id.as_str())
    }

    pub fn read_active_role_profile(
        &self,
        profile_id: &needle_core::RoleProfileId,
    ) -> Result<Option<RoleProfileRevision>, StoreError> {
        self.initialize()?;
        let connection = self.connection()?;
        let state = load_state(&connection, profile_id.as_str())?;
        let Some(revision) = state.active_revision else {
            return Ok(None);
        };
        let (definition, created, activated) =
            load_revision(&connection, profile_id.as_str(), revision)?;
        if activated.is_none() {
            return Err(StoreError::RoleProfileCorruption(
                "active state points to a revision without activation metadata".to_owned(),
            ));
        }
        Ok(Some(revision_with_state(
            profile_id.clone(),
            revision,
            definition,
            RoleProfileState::Active,
            created,
            activated,
        )?))
    }

    pub fn activate_role_profile(
        &self,
        profile_id: &needle_core::RoleProfileId,
        target_revision: u64,
        expected_state_digest: Digest,
    ) -> Result<RoleProfileRevision, StoreError> {
        self.activate_role_profile_inner(profile_id, target_revision, expected_state_digest, None)
    }

    pub fn activate_role_profile_checked(
        &self,
        profile_id: &needle_core::RoleProfileId,
        target_revision: u64,
        expected_state_digest: Digest,
        expected_active_digest: Option<Digest>,
    ) -> Result<RoleProfileRevision, StoreError> {
        self.activate_role_profile_inner(
            profile_id,
            target_revision,
            expected_state_digest,
            Some(expected_active_digest),
        )
    }

    fn activate_role_profile_inner(
        &self,
        profile_id: &needle_core::RoleProfileId,
        target_revision: u64,
        expected_state_digest: Digest,
        expected_active_digest: Option<Option<Digest>>,
    ) -> Result<RoleProfileRevision, StoreError> {
        self.initialize()?;
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state = load_state(&transaction, profile_id.as_str())?;
        compare_state(&state, expected_state_digest)?;
        if let Some(expected) = expected_active_digest
            && state.active_definition_digest != expected
        {
            return Err(StoreError::RoleProfileConflict(
                "stale active role-profile digest".to_owned(),
            ));
        }
        let (definition, created, old_activated) =
            load_revision(&transaction, profile_id.as_str(), target_revision)?;
        if state.active_revision == Some(target_revision) {
            if old_activated.is_none() {
                return Err(StoreError::RoleProfileCorruption(
                    "active pointer targets a revision without activation metadata".to_owned(),
                ));
            }
            transaction.commit()?;
            return revision_with_state(
                profile_id.clone(),
                target_revision,
                definition,
                RoleProfileState::Active,
                created,
                old_activated,
            );
        }
        let prior_state = derived_revision_state(&state, target_revision);
        let prior_active_revision = state.active_revision;
        let prior_active_digest = state.active_definition_digest;
        let now = super::now_ms();
        transaction.execute(
            "UPDATE role_profile_revisions SET activated_unix_ms=?3 WHERE profile_id=?1 AND revision=?2",
            params![profile_id.as_str(), target_revision, now],
        )?;
        let next_generation = state.state_generation.checked_add(1).ok_or_else(|| {
            StoreError::RoleProfileCorruption("state generation overflow".to_owned())
        })?;
        let changed = transaction.execute(
            "UPDATE role_profile_state SET active_revision=?2, state_generation=?3, updated_unix_ms=?4 WHERE profile_id=?1 AND state_generation=?5",
            params![profile_id.as_str(), target_revision, next_generation, now, state.state_generation],
        )?;
        if changed != 1 {
            return Err(StoreError::RoleProfileCorruption(
                "role-profile state CAS did not update exactly one row".to_owned(),
            ));
        }
        let next_state = load_state(&transaction, profile_id.as_str())?;
        insert_audit(
            &transaction,
            RoleProfileAuditInput {
                profile_id: profile_id.as_str(),
                revision: target_revision,
                definition_digest: definition.definition_digest,
                operation: RoleProfileAuditOperation::Activate,
                prior_state: Some(prior_state),
                resulting_state: RoleProfileState::Active,
                prior_state_digest: Some(state.state_digest),
                resulting: &next_state,
                prior_active_revision,
                prior_active_digest,
                now,
            },
        )?;
        transaction.commit()?;
        revision_with_state(
            profile_id.clone(),
            target_revision,
            definition,
            RoleProfileState::Active,
            created,
            Some(now),
        )
    }

    pub fn deactivate_role_profile(
        &self,
        profile_id: &needle_core::RoleProfileId,
        expected_state_digest: Digest,
    ) -> Result<RoleProfileRevision, StoreError> {
        self.deactivate_role_profile_inner(profile_id, expected_state_digest, None)
    }

    pub fn deactivate_role_profile_checked(
        &self,
        profile_id: &needle_core::RoleProfileId,
        expected_state_digest: Digest,
        expected_active_digest: Option<Digest>,
    ) -> Result<RoleProfileRevision, StoreError> {
        self.deactivate_role_profile_inner(
            profile_id,
            expected_state_digest,
            Some(expected_active_digest),
        )
    }

    fn deactivate_role_profile_inner(
        &self,
        profile_id: &needle_core::RoleProfileId,
        expected_state_digest: Digest,
        expected_active_digest: Option<Option<Digest>>,
    ) -> Result<RoleProfileRevision, StoreError> {
        self.initialize()?;
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state = load_state(&transaction, profile_id.as_str())?;
        compare_state(&state, expected_state_digest)?;
        if let Some(expected) = expected_active_digest
            && state.active_definition_digest != expected
        {
            return Err(StoreError::RoleProfileConflict(
                "stale active role-profile digest".to_owned(),
            ));
        }
        let Some(active_revision) = state.active_revision else {
            return Err(StoreError::RoleProfileConflict(
                "profile has no active revision".to_owned(),
            ));
        };
        let (definition, created, activated) =
            load_revision(&transaction, profile_id.as_str(), active_revision)?;
        if activated.is_none() {
            return Err(StoreError::RoleProfileCorruption(
                "active pointer targets a revision without activation metadata".to_owned(),
            ));
        }
        let prior_state = RoleProfileState::Active;
        let now = super::now_ms();
        let next_generation = state.state_generation.checked_add(1).ok_or_else(|| {
            StoreError::RoleProfileCorruption("state generation overflow".to_owned())
        })?;
        let changed = transaction.execute(
            "UPDATE role_profile_state SET active_revision=NULL, state_generation=?2, updated_unix_ms=?3 WHERE profile_id=?1 AND state_generation=?4",
            params![profile_id.as_str(), next_generation, now, state.state_generation],
        )?;
        if changed != 1 {
            return Err(StoreError::RoleProfileCorruption(
                "role-profile state CAS did not update exactly one row".to_owned(),
            ));
        }
        let next_state = load_state(&transaction, profile_id.as_str())?;
        let resulting_state = derived_revision_state(&next_state, active_revision);
        insert_audit(
            &transaction,
            RoleProfileAuditInput {
                profile_id: profile_id.as_str(),
                revision: active_revision,
                definition_digest: definition.definition_digest,
                operation: RoleProfileAuditOperation::Deactivate,
                prior_state: Some(prior_state),
                resulting_state,
                prior_state_digest: Some(state.state_digest),
                resulting: &next_state,
                prior_active_revision: state.active_revision,
                prior_active_digest: state.active_definition_digest,
                now,
            },
        )?;
        transaction.commit()?;
        revision_with_state(
            profile_id.clone(),
            active_revision,
            definition,
            resulting_state,
            created,
            activated,
        )
    }
}

fn validate_definition(definition: &RoleProfileDefinition) -> Result<(), StoreError> {
    definition.validate().map_err(validation_error)
}

fn load_revision<C: std::ops::Deref<Target = Connection>>(
    connection: &C,
    profile_id: &str,
    revision: u64,
) -> Result<(RoleProfileDefinition, u64, Option<u64>), StoreError> {
    let row: Option<(String, String, u64, Option<u64>)> = connection
        .query_row(
            "SELECT definition_json, definition_digest, created_unix_ms, activated_unix_ms
             FROM role_profile_revisions WHERE profile_id=?1 AND revision=?2",
            params![profile_id, revision],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((json, digest, created, activated)) = row else {
        return Err(StoreError::RoleProfileNotFound(format!("{profile_id}@{revision}")));
    };
    let definition = parse_definition(&json, &digest, profile_id)?;
    let stored_role: String = connection.query_row(
        "SELECT role FROM role_profiles WHERE profile_id=?1",
        [profile_id],
        |row| row.get(0),
    )?;
    if stored_role != definition.role.as_str() {
        return Err(StoreError::RoleProfileCorruption(
            "revision role disagrees with profile identity".to_owned(),
        ));
    }
    Ok((definition, created, activated))
}

fn parse_definition(
    json: &str,
    row_digest: &str,
    profile_id: &str,
) -> Result<RoleProfileDefinition, StoreError> {
    let definition: RoleProfileDefinition = serde_json::from_str(json).map_err(|error| {
        StoreError::RoleProfileCorruption(format!("definition JSON is invalid: {error}"))
    })?;
    definition.validate().map_err(|error| {
        StoreError::RoleProfileCorruption(format!("definition validation failed: {error}"))
    })?;
    if definition.profile_id.as_str() != profile_id {
        return Err(StoreError::RoleProfileCorruption(
            "definition profile_id disagrees with row identity".to_owned(),
        ));
    }
    let parsed_digest = Digest::parse(row_digest).map_err(|error| {
        StoreError::RoleProfileCorruption(format!("row definition digest is invalid: {error}"))
    })?;
    if parsed_digest != definition.definition_digest {
        return Err(StoreError::RoleProfileCorruption(
            "row definition digest disagrees with definition JSON".to_owned(),
        ));
    }
    Ok(definition)
}

#[cfg(test)]
#[path = "role_profiles/tests.rs"]
mod tests;
