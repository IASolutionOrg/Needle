use super::{RoleProfileStateRecord, RuntimeStore};
use crate::StoreError;
use needle_core::{Digest, RoleProfileState};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleProfileAuditOperation {
    Create,
    Revise,
    Activate,
    Deactivate,
}

impl RoleProfileAuditOperation {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Revise => "revise",
            Self::Activate => "activate",
            Self::Deactivate => "deactivate",
        }
    }

    fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "create" => Ok(Self::Create),
            "revise" => Ok(Self::Revise),
            "activate" => Ok(Self::Activate),
            "deactivate" => Ok(Self::Deactivate),
            _ => {
                Err(StoreError::RoleProfileCorruption(format!("unknown audit operation `{value}`")))
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleProfileAuditRecord {
    pub audit_id: u64,
    pub profile_id: needle_core::RoleProfileId,
    pub revision: u64,
    pub definition_digest: Digest,
    pub operation: RoleProfileAuditOperation,
    pub prior_state: Option<RoleProfileState>,
    pub resulting_state: RoleProfileState,
    pub prior_state_digest: Option<Digest>,
    pub resulting_state_digest: Digest,
    pub prior_active_revision: Option<u64>,
    pub prior_active_digest: Option<Digest>,
    pub resulting_active_revision: Option<u64>,
    pub resulting_active_digest: Option<Digest>,
    pub created_unix_ms: u64,
}

impl RuntimeStore {
    pub fn read_role_profile_audit(
        &self,
        profile_id: &needle_core::RoleProfileId,
        limit: usize,
    ) -> Result<Vec<RoleProfileAuditRecord>, StoreError> {
        if limit > 100 {
            return Err(StoreError::RoleProfileValidation(
                "audit limit must be at most 100".to_owned(),
            ));
        }
        self.initialize()?;
        let connection = self.connection()?;
        let exists: Option<i64> = connection
            .query_row(
                "SELECT 1 FROM role_profiles WHERE profile_id=?1",
                [profile_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::RoleProfileNotFound(profile_id.to_string()));
        }
        let mut statement = connection.prepare(
            "SELECT audit_id, revision, definition_digest, operation, prior_state, resulting_state,
                    prior_state_digest, resulting_state_digest, prior_active_revision,
                    prior_active_digest, resulting_active_revision, resulting_active_digest,
                    created_unix_ms
             FROM role_profile_audit WHERE profile_id=?1 ORDER BY audit_id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![profile_id.as_str(), limit as u64], |row| {
            parse_audit_row(row, profile_id)
        })?;
        let parsed = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut result = Vec::new();
        for record in parsed {
            let stored_digest: Option<String> = connection
                .query_row(
                    "SELECT definition_digest FROM role_profile_revisions WHERE profile_id=?1 AND revision=?2",
                    params![profile_id.as_str(), record.revision],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(stored_digest) = stored_digest else {
                return Err(StoreError::RoleProfileCorruption(
                    "audit row references a missing revision".to_owned(),
                ));
            };
            if stored_digest != record.definition_digest.to_string() {
                return Err(StoreError::RoleProfileCorruption(
                    "audit digest disagrees with revision digest".to_owned(),
                ));
            }
            result.push(record);
        }
        Ok(result)
    }
}

pub(super) struct RoleProfileAuditInput<'a> {
    pub(super) profile_id: &'a str,
    pub(super) revision: u64,
    pub(super) definition_digest: Digest,
    pub(super) operation: RoleProfileAuditOperation,
    pub(super) prior_state: Option<RoleProfileState>,
    pub(super) resulting_state: RoleProfileState,
    pub(super) prior_state_digest: Option<Digest>,
    pub(super) resulting: &'a RoleProfileStateRecord,
    pub(super) prior_active_revision: Option<u64>,
    pub(super) prior_active_digest: Option<Digest>,
    pub(super) now: u64,
}

pub(super) fn insert_audit(
    transaction: &Transaction<'_>,
    input: RoleProfileAuditInput<'_>,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO role_profile_audit(
            profile_id, revision, definition_digest, operation, prior_state, resulting_state,
            prior_state_digest, resulting_state_digest, prior_active_revision, prior_active_digest,
            resulting_active_revision, resulting_active_digest, created_unix_ms
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            input.profile_id,
            input.revision,
            input.definition_digest.to_string(),
            input.operation.as_str(),
            input.prior_state.map(state_name),
            state_name(input.resulting_state),
            input.prior_state_digest.map(|digest| digest.to_string()),
            input.resulting.state_digest.to_string(),
            input.prior_active_revision,
            input.prior_active_digest.map(|digest| digest.to_string()),
            input.resulting.active_revision,
            input.resulting.active_definition_digest.map(|digest| digest.to_string()),
            input.now,
        ],
    )?;
    Ok(())
}

fn state_name(state: RoleProfileState) -> &'static str {
    match state {
        RoleProfileState::Draft => "draft",
        RoleProfileState::Active => "active",
        RoleProfileState::Inactive => "inactive",
    }
}

fn parse_state(value: Option<String>) -> Result<Option<RoleProfileState>, StoreError> {
    match value.as_deref() {
        None => Ok(None),
        Some("draft") => Ok(Some(RoleProfileState::Draft)),
        Some("active") => Ok(Some(RoleProfileState::Active)),
        Some("inactive") => Ok(Some(RoleProfileState::Inactive)),
        Some(other) => {
            Err(StoreError::RoleProfileCorruption(format!("stored state `{other}` is invalid")))
        }
    }
}

fn parse_digest(value: Option<String>, field: &str) -> Result<Option<Digest>, StoreError> {
    value
        .map(|value| {
            Digest::parse(&value).map_err(|error| {
                StoreError::RoleProfileCorruption(format!("{field} is invalid: {error}"))
            })
        })
        .transpose()
}

fn parse_audit_row(
    row: &Row<'_>,
    profile_id: &needle_core::RoleProfileId,
) -> rusqlite::Result<RoleProfileAuditRecord> {
    let audit_id: u64 = row.get(0)?;
    let revision: u64 = row.get(1)?;
    let definition_digest: String = row.get(2)?;
    let operation: String = row.get(3)?;
    let prior_state: Option<String> = row.get(4)?;
    let resulting_state: String = row.get(5)?;
    let prior_state_digest: Option<String> = row.get(6)?;
    let resulting_state_digest: String = row.get(7)?;
    let prior_active_revision: Option<u64> = row.get(8)?;
    let prior_active_digest: Option<String> = row.get(9)?;
    let resulting_active_revision: Option<u64> = row.get(10)?;
    let resulting_active_digest: Option<String> = row.get(11)?;
    let created_unix_ms: u64 = row.get(12)?;
    let definition_digest = sql_conversion(Digest::parse(&definition_digest).map_err(|error| {
        StoreError::RoleProfileCorruption(format!("audit definition digest is invalid: {error}"))
    }))?;
    let operation = sql_conversion(RoleProfileAuditOperation::parse(&operation))?;
    let prior_state = sql_conversion(parse_state(prior_state))?;
    let resulting_state = sql_conversion(parse_state(Some(resulting_state)))?.ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            "audit resulting state is missing".into(),
        )
    })?;
    let prior_state_digest =
        parse_digest(prior_state_digest, "audit prior state digest").map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    let resulting_state_digest =
        sql_conversion(Digest::parse(&resulting_state_digest).map_err(|error| {
            StoreError::RoleProfileCorruption(format!(
                "audit resulting state digest is invalid: {error}"
            ))
        }))?;
    let prior_active_digest = parse_digest(prior_active_digest, "audit prior active digest")
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    let resulting_active_digest = parse_digest(
        resulting_active_digest,
        "audit resulting active digest",
    )
    .map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(11, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(RoleProfileAuditRecord {
        audit_id,
        profile_id: profile_id.clone(),
        revision,
        definition_digest,
        operation,
        prior_state,
        resulting_state,
        prior_state_digest,
        resulting_state_digest,
        prior_active_revision,
        prior_active_digest,
        resulting_active_revision,
        resulting_active_digest,
        created_unix_ms,
    })
}

fn sql_conversion<T>(result: Result<T, StoreError>) -> rusqlite::Result<T> {
    result.map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}
