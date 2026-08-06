use super::{RuntimeStore, StoreError};
use needle_core::{CanonicalHasher, Digest, RoleProfileId};
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivationScope {
    Global,
    Repository { repository_root: String },
}

impl ActivationScope {
    pub fn global() -> Self {
        Self::Global
    }

    pub fn repository(path: &Path) -> Result<Self, StoreError> {
        let canonical = fs::canonicalize(path).map_err(|error| {
            StoreError::ActivationValidation(format!(
                "cannot resolve repository {}: {error}",
                path.display()
            ))
        })?;
        if !canonical.is_dir() {
            return Err(StoreError::ActivationValidation(format!(
                "repository is not a directory: {}",
                canonical.display()
            )));
        }
        let repository_root = canonical.to_str().ok_or_else(|| {
            StoreError::ActivationValidation(
                "repository path is not valid Unicode and cannot be persisted".to_owned(),
            )
        })?;
        Ok(Self::Repository { repository_root: repository_root.to_owned() })
    }

    fn key(&self) -> &str {
        match self {
            Self::Global => "global",
            Self::Repository { repository_root } => repository_root,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Repository { .. } => "repository",
        }
    }

    fn repository_root(&self) -> &str {
        match self {
            Self::Global => "",
            Self::Repository { repository_root } => repository_root,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActivationRecord {
    pub scope: ActivationScope,
    pub enabled: bool,
    pub role_profile_id: Option<RoleProfileId>,
    pub generation: u64,
    pub state_digest: Digest,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActivationStatus {
    pub enabled: bool,
    pub effective_scope: Option<ActivationScope>,
    pub role_profile_id: Option<RoleProfileId>,
    pub global: Option<ActivationRecord>,
    pub repository: Option<ActivationRecord>,
}

impl RuntimeStore {
    pub fn activation_status(
        &self,
        repository_root: &Path,
    ) -> Result<ActivationStatus, StoreError> {
        self.initialize()?;
        let repository_scope = ActivationScope::repository(repository_root)?;
        let connection = self.connection()?;
        let global = load_activation(&connection, &ActivationScope::Global)?;
        let repository = load_activation(&connection, &repository_scope)?;
        let effective = repository.as_ref().or(global.as_ref());
        Ok(ActivationStatus {
            enabled: effective.is_some_and(|record| record.enabled),
            effective_scope: effective.map(|record| record.scope.clone()),
            role_profile_id: effective.and_then(|record| record.role_profile_id.clone()),
            global,
            repository,
        })
    }

    pub fn global_activation(&self) -> Result<Option<ActivationRecord>, StoreError> {
        self.initialize()?;
        let connection = self.connection()?;
        load_activation(&connection, &ActivationScope::Global)
    }

    pub fn set_global_activation(
        &self,
        enabled: bool,
        role_profile_id: Option<&RoleProfileId>,
    ) -> Result<ActivationRecord, StoreError> {
        self.set_activation(ActivationScope::Global, enabled, role_profile_id, None, false)
    }

    pub fn set_repository_activation(
        &self,
        repository_root: &Path,
        enabled: bool,
        role_profile_id: Option<&RoleProfileId>,
    ) -> Result<ActivationRecord, StoreError> {
        self.set_activation(
            ActivationScope::repository(repository_root)?,
            enabled,
            role_profile_id,
            None,
            false,
        )
    }

    pub fn compare_and_set_activation(
        &self,
        scope: ActivationScope,
        enabled: bool,
        role_profile_id: Option<&RoleProfileId>,
        expected_state_digest: Option<Digest>,
    ) -> Result<ActivationRecord, StoreError> {
        self.set_activation(scope, enabled, role_profile_id, expected_state_digest, true)
    }

    fn set_activation(
        &self,
        scope: ActivationScope,
        enabled: bool,
        role_profile_id: Option<&RoleProfileId>,
        expected_state_digest: Option<Digest>,
        enforce_compare: bool,
    ) -> Result<ActivationRecord, StoreError> {
        self.initialize()?;
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing = load_activation(&transaction, &scope)?;
        if enforce_compare
            && existing.as_ref().map(|record| record.state_digest) != expected_state_digest
        {
            return Err(StoreError::ActivationConflict(
                "stale product activation state digest".to_owned(),
            ));
        }
        let selected_profile = role_profile_id
            .cloned()
            .or_else(|| existing.as_ref().and_then(|record| record.role_profile_id.clone()));
        if enabled {
            let profile_id = selected_profile.as_ref().ok_or_else(|| {
                StoreError::ActivationValidation(
                    "enabling Needle requires an active explorer role profile".to_owned(),
                )
            })?;
            let active: Option<i64> = transaction
                .query_row(
                    "SELECT state.active_revision
                     FROM role_profile_state state
                     JOIN role_profiles profile ON profile.profile_id=state.profile_id
                     WHERE state.profile_id=?1 AND state.active_revision IS NOT NULL
                       AND profile.role='explorer'",
                    [profile_id.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            if active.is_none() {
                return Err(StoreError::ActivationValidation(format!(
                    "role profile `{}` does not exist, is not active, or is not an explorer",
                    profile_id.as_str()
                )));
            }
        }
        if let Some(existing) = existing.as_ref()
            && existing.enabled == enabled
            && existing.role_profile_id == selected_profile
        {
            transaction.commit()?;
            return Ok(existing.clone());
        }
        let generation = existing.as_ref().map_or(Ok(0), |record| {
            record.generation.checked_add(1).ok_or_else(|| {
                StoreError::ActivationCorruption("activation generation overflow".to_owned())
            })
        })?;
        let now = super::now_ms();
        let state_digest =
            activation_state_digest(&scope, enabled, selected_profile.as_ref(), generation);
        let created = existing.as_ref().map_or(now, |record| record.created_unix_ms);
        if existing.is_some() {
            transaction.execute(
                "UPDATE product_activation
                 SET enabled=?2, role_profile_id=?3, generation=?4,
                     state_digest=?5, updated_unix_ms=?6
                 WHERE scope_key=?1",
                params![
                    scope.key(),
                    enabled,
                    selected_profile.as_ref().map(RoleProfileId::as_str),
                    generation,
                    state_digest.to_string(),
                    now,
                ],
            )?;
        } else {
            transaction.execute(
                "INSERT INTO product_activation(
                    scope_key, scope_kind, repository_root, enabled, role_profile_id,
                    generation, state_digest, created_unix_ms, updated_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                params![
                    scope.key(),
                    scope.kind(),
                    scope.repository_root(),
                    enabled,
                    selected_profile.as_ref().map(RoleProfileId::as_str),
                    generation,
                    state_digest.to_string(),
                    now,
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO product_activation_audit(
                scope_key, scope_kind, repository_root, operation,
                previous_state_digest, resulting_state_digest, role_profile_id, changed_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                scope.key(),
                scope.kind(),
                scope.repository_root(),
                if enabled { "enable" } else { "disable" },
                existing.as_ref().map(|record| record.state_digest.to_string()),
                state_digest.to_string(),
                selected_profile.as_ref().map(RoleProfileId::as_str),
                now,
            ],
        )?;
        transaction.commit()?;
        Ok(ActivationRecord {
            scope,
            enabled,
            role_profile_id: selected_profile,
            generation,
            state_digest,
            created_unix_ms: created,
            updated_unix_ms: now,
        })
    }
}

fn load_activation(
    connection: &rusqlite::Connection,
    scope: &ActivationScope,
) -> Result<Option<ActivationRecord>, StoreError> {
    connection
        .query_row(
            "SELECT scope_kind, repository_root, enabled, role_profile_id,
                    generation, state_digest, created_unix_ms, updated_unix_ms
             FROM product_activation WHERE scope_key=?1",
            [scope.key()],
            decode_activation,
        )
        .optional()?
        .map_or(Ok(None), |record| record.map(Some))
}

fn decode_activation(row: &Row<'_>) -> rusqlite::Result<Result<ActivationRecord, StoreError>> {
    let kind: String = row.get(0)?;
    let repository_root: String = row.get(1)?;
    let enabled: bool = row.get(2)?;
    let profile_id: Option<String> = row.get(3)?;
    let generation: u64 = row.get(4)?;
    let digest: String = row.get(5)?;
    let created_unix_ms: u64 = row.get(6)?;
    let updated_unix_ms: u64 = row.get(7)?;
    let scope = match kind.as_str() {
        "global" if repository_root.is_empty() => ActivationScope::Global,
        "repository" if !repository_root.is_empty() => {
            ActivationScope::Repository { repository_root }
        }
        _ => {
            return Ok(Err(StoreError::ActivationCorruption(
                "stored activation scope is invalid".to_owned(),
            )));
        }
    };
    let role_profile_id = match profile_id {
        Some(value) => match RoleProfileId::new(value) {
            Ok(value) => Some(value),
            Err(error) => return Ok(Err(StoreError::ActivationCorruption(error.to_string()))),
        },
        None => None,
    };
    let state_digest = match Digest::parse(&digest) {
        Ok(value) => value,
        Err(error) => return Ok(Err(StoreError::ActivationCorruption(error.to_string()))),
    };
    let expected = activation_state_digest(&scope, enabled, role_profile_id.as_ref(), generation);
    if state_digest != expected {
        return Ok(Err(StoreError::ActivationCorruption(
            "stored activation state digest does not match its projection".to_owned(),
        )));
    }
    Ok(Ok(ActivationRecord {
        scope,
        enabled,
        role_profile_id,
        generation,
        state_digest,
        created_unix_ms,
        updated_unix_ms,
    }))
}

fn activation_state_digest(
    scope: &ActivationScope,
    enabled: bool,
    role_profile_id: Option<&RoleProfileId>,
    generation: u64,
) -> Digest {
    let mut hasher = CanonicalHasher::new(b"needle-product-activation");
    hasher.field_str(scope.kind());
    hasher.field_str(scope.repository_root());
    hasher.field_u8(u8::from(enabled));
    if let Some(profile_id) = role_profile_id {
        hasher.field_u8(1);
        hasher.field_str(profile_id.as_str());
    } else {
        hasher.field_u8(0);
    }
    hasher.field_bytes(&generation.to_le_bytes());
    hasher.finish()
}

#[cfg(test)]
#[path = "activation/tests.rs"]
mod tests;
