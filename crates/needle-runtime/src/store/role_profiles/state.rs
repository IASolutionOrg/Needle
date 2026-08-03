use crate::StoreError;
use needle_core::{
    CanonicalHasher, CodexRole, Digest, RoleProfileDefinition, RoleProfileRevision,
    RoleProfileState, RoleProfileValidationError,
};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleProfileStateRecord {
    pub profile_id: needle_core::RoleProfileId,
    pub latest_revision: u64,
    pub latest_definition_digest: Digest,
    pub active_revision: Option<u64>,
    pub active_definition_digest: Option<Digest>,
    pub state_generation: u64,
    pub updated_unix_ms: u64,
    pub state_digest: Digest,
}

impl RoleProfileStateRecord {
    pub fn state(&self) -> RoleProfileState {
        if self.active_revision.is_some() {
            RoleProfileState::Active
        } else {
            RoleProfileState::Draft
        }
    }
}

pub(super) fn validation_error(error: RoleProfileValidationError) -> StoreError {
    StoreError::RoleProfileValidation(error.to_string())
}

pub(super) fn compare_state(
    state: &RoleProfileStateRecord,
    expected: Digest,
) -> Result<(), StoreError> {
    if state.state_digest != expected {
        return Err(StoreError::RoleProfileConflict("stale role-profile state digest".to_owned()));
    }
    Ok(())
}

pub(super) fn revision_with_state(
    profile_id: needle_core::RoleProfileId,
    revision: u64,
    definition: RoleProfileDefinition,
    state: RoleProfileState,
    created_unix_ms: u64,
    activated_unix_ms: Option<u64>,
) -> Result<RoleProfileRevision, StoreError> {
    let revision = RoleProfileRevision {
        profile_id,
        revision,
        definition,
        state,
        created_unix_ms,
        activated_unix_ms,
    };
    revision.validate().map_err(validation_error)?;
    Ok(revision)
}

pub(super) fn derived_revision_state(
    state: &RoleProfileStateRecord,
    revision: u64,
) -> RoleProfileState {
    if state.active_revision == Some(revision) {
        RoleProfileState::Active
    } else if state.latest_revision == revision {
        RoleProfileState::Draft
    } else {
        RoleProfileState::Inactive
    }
}

pub(super) fn load_state<C: std::ops::Deref<Target = Connection>>(
    connection: &C,
    profile_id: &str,
) -> Result<RoleProfileStateRecord, StoreError> {
    let profile: Option<String> = connection
        .query_row("SELECT role FROM role_profiles WHERE profile_id=?1", [profile_id], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(role) = profile else {
        return Err(StoreError::RoleProfileNotFound(profile_id.to_owned()));
    };
    parse_role(&role)?;
    let (state_latest_revision, active_revision, generation, updated):
        (u64, Option<u64>, u64, u64) = connection
        .query_row(
            "SELECT latest_revision, active_revision, state_generation, updated_unix_ms FROM role_profile_state WHERE profile_id=?1",
            [profile_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::RoleProfileCorruption("profile state row is missing".to_owned())
        })?;
    let max_revision: Option<u64> = connection.query_row(
        "SELECT MAX(revision) FROM role_profile_revisions WHERE profile_id=?1",
        [profile_id],
        |row| row.get(0),
    )?;
    if max_revision != Some(state_latest_revision) {
        return Err(StoreError::RoleProfileCorruption(
            "latest state pointer is not the newest revision".to_owned(),
        ));
    }
    if let Some(active_revision) = active_revision
        && active_revision > state_latest_revision
    {
        return Err(StoreError::RoleProfileCorruption(
            "active state pointer is newer than latest revision".to_owned(),
        ));
    }
    if generation < state_latest_revision.saturating_sub(1) {
        return Err(StoreError::RoleProfileCorruption(
            "state generation is older than the revision history".to_owned(),
        ));
    }
    let (latest_definition, _, _) =
        super::load_revision(connection, profile_id, state_latest_revision).map_err(|error| {
            match error {
                StoreError::RoleProfileNotFound(_) => StoreError::RoleProfileCorruption(
                    "latest state pointer targets a missing revision".to_owned(),
                ),
                other => other,
            }
        })?;
    if latest_definition.role.as_str() != role {
        return Err(StoreError::RoleProfileCorruption(
            "profile identity/latest digest disagrees with revision".to_owned(),
        ));
    }
    let active_definition_digest = if let Some(active_revision) = active_revision {
        let (active_definition, _, activated) =
            super::load_revision(connection, profile_id, active_revision).map_err(|error| {
                match error {
                    StoreError::RoleProfileNotFound(_) => StoreError::RoleProfileCorruption(
                        "active state pointer targets a missing revision".to_owned(),
                    ),
                    other => other,
                }
            })?;
        if activated.is_none() {
            return Err(StoreError::RoleProfileCorruption(
                "active state points to a revision without activation metadata".to_owned(),
            ));
        }
        Some(active_definition.definition_digest)
    } else {
        None
    };
    let profile_id =
        needle_core::RoleProfileId::new(profile_id.to_owned()).map_err(validation_error)?;
    let state_digest = compute_state_digest(
        &profile_id,
        state_latest_revision,
        latest_definition.definition_digest,
        active_revision,
        active_definition_digest,
        generation,
    );
    Ok(RoleProfileStateRecord {
        profile_id,
        latest_revision: state_latest_revision,
        latest_definition_digest: latest_definition.definition_digest,
        active_revision,
        active_definition_digest,
        state_generation: generation,
        updated_unix_ms: updated,
        state_digest,
    })
}

fn compute_state_digest(
    profile_id: &needle_core::RoleProfileId,
    latest_revision: u64,
    latest_digest: Digest,
    active_revision: Option<u64>,
    active_digest: Option<Digest>,
    generation: u64,
) -> Digest {
    let mut hasher = CanonicalHasher::new(b"needle-codex-role-profile-state-v1");
    hasher.field_str(profile_id.as_str());
    hasher.field_bytes(&latest_revision.to_le_bytes());
    hasher.field_digest(latest_digest);
    match (active_revision, active_digest) {
        (Some(revision), Some(digest)) => {
            hasher.field_u8(1);
            hasher.field_bytes(&revision.to_le_bytes());
            hasher.field_digest(digest);
        }
        (None, None) => hasher.field_u8(0),
        _ => hasher.field_u8(255),
    }
    hasher.field_bytes(&generation.to_le_bytes());
    hasher.finish()
}

fn parse_role(value: &str) -> Result<CodexRole, StoreError> {
    match value {
        "explorer" => Ok(CodexRole::Explorer),
        "implementer" => Ok(CodexRole::Implementer),
        "test_runner" => Ok(CodexRole::TestRunner),
        "reviewer" => Ok(CodexRole::Reviewer),
        "verifier" => Ok(CodexRole::Verifier),
        "auditor" => Ok(CodexRole::Auditor),
        _ => Err(StoreError::RoleProfileCorruption(format!("stored role `{value}` is invalid"))),
    }
}
