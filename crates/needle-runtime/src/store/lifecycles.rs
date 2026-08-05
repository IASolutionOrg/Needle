use super::*;
use needle_core::{
    ApprovalDecisionSource, ChangeApplyId, ChangeApplyStatus, ChangeId, ChangeRequest, CodexRole,
    CommandExecutionEvidence, DevelopmentLifecycle, Digest, LifecycleApplyApproval, LifecycleError,
    LifecycleEvent, LifecyclePhase, LifecycleReason, LifecycleSpec, LifecycleStatus,
    LifecycleTerminalOutcome, LifecycleTransition, LifecycleUsage, LifecycleWorkerProfiles,
    PatchId, ReviewArtifact, RoleProfileProvenance, VerificationArtifact, VerificationArtifactId,
};
use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

type LifecycleChangeAnchors =
    (String, String, u64, bool, Option<String>, Option<u64>, Option<String>);
type LifecycleProjectionRow = (String, String, String, String, u64);

pub const MAX_LIFECYCLE_LIST_LIMIT: usize = 100;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleProjection {
    pub lifecycle: DevelopmentLifecycle,
    pub state_digest: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleSummaryRecord {
    pub lifecycle_id: needle_core::LifecycleId,
    pub change_id: ChangeId,
    pub source_snapshot: Digest,
    pub phase: LifecyclePhase,
    pub status: LifecycleStatus,
    pub state_digest: Digest,
    pub generation: u64,
    pub usage: LifecycleUsage,
    pub terminal_outcome: Option<LifecycleTerminalOutcome>,
    pub terminal_reason: Option<LifecycleReason>,
    pub created_unix_ms: u64,
    pub updated_unix_ms: u64,
}

impl LifecycleSummaryRecord {
    fn from_projection(projection: LifecycleProjection) -> Self {
        let lifecycle = projection.lifecycle;
        Self {
            lifecycle_id: lifecycle.id,
            change_id: lifecycle.change_id,
            source_snapshot: lifecycle.source_snapshot,
            phase: lifecycle.phase,
            status: lifecycle.status,
            state_digest: projection.state_digest,
            generation: lifecycle.generation,
            usage: lifecycle.usage,
            terminal_outcome: lifecycle.terminal_outcome,
            terminal_reason: lifecycle.terminal_reason,
            created_unix_ms: lifecycle.created_unix_ms,
            updated_unix_ms: lifecycle.updated_unix_ms,
        }
    }
}

impl LifecycleProjection {
    fn new(lifecycle: DevelopmentLifecycle) -> Result<Self, StoreError> {
        lifecycle.validate()?;
        let state_digest = lifecycle.state_digest();
        Ok(Self { lifecycle, state_digest })
    }
}

impl RuntimeStore {
    pub fn create_lifecycle(
        &self,
        change_id: &ChangeId,
        spec: LifecycleSpec,
    ) -> Result<LifecycleProjection, StoreError> {
        self.initialize()?;
        spec.validate()?;
        let now = now_ms();
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        validate_profile_set(&transaction, &spec.profiles)?;
        if lifecycle_projection_in_transaction(&transaction, change_id)?.is_some() {
            return Err(StoreError::LifecycleConflict(format!(
                "{change_id}: lifecycle already exists"
            )));
        }
        let anchors: Option<LifecycleChangeAnchors> = transaction
            .query_row(
                "SELECT source_snapshot_digest, state, latest_patch_revision,
                        repair_attempted, role_profile_id, role_profile_revision,
                        role_profile_definition_digest
                 FROM change_requests WHERE change_id=?1",
                [change_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            source,
            state,
            latest_patch_revision,
            repair_attempted,
            profile_id,
            profile_revision,
            profile_digest,
        )) = anchors
        else {
            return Err(StoreError::LifecycleNotFound(format!(
                "{change_id}: immutable change request"
            )));
        };
        if state != "requested" || latest_patch_revision != 0 || repair_attempted {
            return Err(StoreError::LifecycleConflict(format!(
                "{change_id}: lifecycle must be created before implementation"
            )));
        }
        let source_snapshot = Digest::parse(&source)
            .map_err(|_| StoreError::LifecycleCorruption(format!("{change_id}: source digest")))?;
        validate_test_plan_certificates(&transaction, &spec, source_snapshot)?;
        let request_profile = parse_profile_anchor(profile_id, profile_revision, profile_digest)?;
        if request_profile.as_ref() != Some(&spec.profiles.implement) {
            return Err(StoreError::LifecycleConflict(format!(
                "{change_id}: request implementer profile differs from lifecycle"
            )));
        }
        let lifecycle = DevelopmentLifecycle::new(change_id.clone(), source_snapshot, spec, now)?;
        let projection = LifecycleProjection::new(lifecycle)?;
        let state_json = serde_json::to_string(&projection.lifecycle)?;
        transaction.execute(
            "INSERT INTO change_lifecycles(
                lifecycle_id, change_id, source_snapshot_digest, state_digest,
                generation, state_json, created_unix_ms, updated_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, 0, ?5, ?6, ?6)",
            params![
                projection.lifecycle.id.to_string(),
                change_id.to_string(),
                source_snapshot.to_string(),
                projection.state_digest.to_string(),
                state_json,
                now,
            ],
        )?;
        let event = LifecycleEvent::created(&projection.lifecycle)?;
        insert_lifecycle_event(&transaction, &event, "lifecycle_created")?;
        transaction.commit()?;
        Ok(projection)
    }

    pub fn lifecycle(
        &self,
        change_id: &ChangeId,
    ) -> Result<Option<LifecycleProjection>, StoreError> {
        let connection = self.connection()?;
        lifecycle_projection_in_transaction(&*connection, change_id)
    }

    /// Return validated lifecycle summaries ordered by canonical change identity.
    ///
    /// Every selected lifecycle is replayed against its bounded journal before
    /// projection, so corruption is returned instead of hidden by a partial list.
    pub fn list_lifecycle_summaries(
        &self,
        limit: usize,
    ) -> Result<Vec<LifecycleSummaryRecord>, StoreError> {
        if limit == 0 || limit > MAX_LIFECYCLE_LIST_LIMIT {
            return Err(StoreError::LifecycleQuery(format!(
                "limit must be between 1 and {MAX_LIFECYCLE_LIST_LIMIT}"
            )));
        }
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let ids = {
            let mut statement = transaction.prepare(
                "SELECT change_id FROM change_lifecycles ORDER BY change_id ASC LIMIT ?1",
            )?;
            statement
                .query_map([limit as u64], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut summaries = Vec::with_capacity(ids.len());
        for raw_id in ids {
            let change_id = ChangeId::parse(&raw_id).map_err(|_| {
                StoreError::LifecycleCorruption("lifecycle list change identity".to_owned())
            })?;
            let projection = replay_lifecycle_in_transaction(&transaction, &change_id)?;
            summaries.push(LifecycleSummaryRecord::from_projection(projection));
        }
        transaction.commit()?;
        Ok(summaries)
    }

    pub fn lifecycle_events(
        &self,
        change_id: &ChangeId,
    ) -> Result<Vec<LifecycleEvent>, StoreError> {
        let connection = self.connection()?;
        lifecycle_events_in_connection(&connection, change_id)
    }

    pub fn replay_lifecycle(
        &self,
        change_id: &ChangeId,
    ) -> Result<LifecycleProjection, StoreError> {
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let projection = replay_lifecycle_in_transaction(&transaction, change_id)?;
        transaction.commit()?;
        Ok(projection)
    }

    /// Return the canonical ordered journal after replaying it against the
    /// persisted lifecycle projection in one read transaction.
    pub fn replay_lifecycle_events(
        &self,
        change_id: &ChangeId,
    ) -> Result<Vec<LifecycleEvent>, StoreError> {
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
        let (_, events) = replay_lifecycle_and_events_in_transaction(&transaction, change_id)?;
        transaction.commit()?;
        Ok(events)
    }

    /// Parent-owned transition boundary. Worker outputs are data carried by a
    /// transition; workers do not receive this store capability.
    pub fn parent_transition_lifecycle(
        &self,
        change_id: &ChangeId,
        expected_state_digest: Digest,
        transition: LifecycleTransition,
    ) -> Result<LifecycleProjection, StoreError> {
        if matches!(
            transition,
            LifecycleTransition::ConsumeRepair
                | LifecycleTransition::ApproveApply { .. }
                | LifecycleTransition::StartApply { .. }
                | LifecycleTransition::FinishApply { .. }
        ) {
            return Err(StoreError::LifecycleConflict(format!(
                "{change_id}: transition is reserved for a typed parent operation"
            )));
        }
        let now = now_ms();
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let projection = lifecycle_projection_in_transaction(&transaction, change_id)?
            .ok_or_else(|| StoreError::LifecycleNotFound(change_id.to_string()))?;
        if projection.state_digest != expected_state_digest {
            return Err(StoreError::LifecycleConflict(format!(
                "{change_id}: state digest changed"
            )));
        }
        validate_transition_artifacts(&transaction, &projection.lifecycle, &transition)?;
        let result =
            persist_transition(&transaction, &projection, expected_state_digest, transition, now)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Record an explicit user approval against the exact verified lifecycle
    /// projection. The state digest prevents an approval from floating to a
    /// later patch or verification revision.
    pub fn approve_lifecycle_apply(
        &self,
        change_id: &ChangeId,
        expected_state_digest: Digest,
        source: ApprovalDecisionSource,
    ) -> Result<LifecycleProjection, StoreError> {
        if source != ApprovalDecisionSource::WebUser {
            return Err(StoreError::LifecycleConflict(format!(
                "{change_id}: apply approval must come from an explicit web user decision"
            )));
        }
        let projection = self
            .lifecycle(change_id)?
            .ok_or_else(|| StoreError::LifecycleNotFound(change_id.to_string()))?;
        if projection.state_digest != expected_state_digest
            || projection.lifecycle.phase != LifecyclePhase::Apply
            || projection.lifecycle.status != LifecycleStatus::AwaitingApproval
        {
            return Err(StoreError::LifecycleConflict(format!(
                "{change_id}: lifecycle is not awaiting approval at this digest"
            )));
        }
        let patch = projection.lifecycle.patch.as_ref().ok_or(LifecycleError::MissingArtifact)?;
        let verification =
            projection.lifecycle.verification.as_ref().ok_or(LifecycleError::MissingArtifact)?;
        let approval = LifecycleApplyApproval::new(
            expected_state_digest,
            patch.patch_id,
            verification.verification_id,
            source,
            now_ms(),
        );
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = lifecycle_projection_in_transaction(&transaction, change_id)?
            .ok_or_else(|| StoreError::LifecycleNotFound(change_id.to_string()))?;
        let result = persist_transition(
            &transaction,
            &current,
            expected_state_digest,
            LifecycleTransition::ApproveApply { approval },
            now_ms(),
        )?;
        transaction.commit()?;
        Ok(result)
    }
}

fn validate_profile_set(
    transaction: &Transaction<'_>,
    profiles: &LifecycleWorkerProfiles,
) -> Result<(), StoreError> {
    for (phase, role) in [
        (LifecyclePhase::Explore, CodexRole::Explorer),
        (LifecyclePhase::Implement, CodexRole::Implementer),
        (LifecyclePhase::Test, CodexRole::TestRunner),
        (LifecyclePhase::Review, CodexRole::Reviewer),
        (LifecyclePhase::Verify, CodexRole::Verifier),
    ] {
        let provenance = profiles.for_phase(phase).ok_or(LifecycleError::ProfileMismatch)?;
        let matches: u64 = transaction.query_row(
            "SELECT COUNT(*)
             FROM role_profiles p
             JOIN role_profile_state s ON s.profile_id=p.profile_id
             JOIN role_profile_revisions r
               ON r.profile_id=p.profile_id AND r.revision=s.active_revision
             WHERE p.profile_id=?1 AND p.role=?2 AND s.active_revision=?3
               AND r.definition_digest=?4 AND r.activated_unix_ms IS NOT NULL",
            params![
                provenance.profile_id.as_str(),
                role.as_str(),
                provenance.revision,
                provenance.definition_digest.to_string(),
            ],
            |row| row.get(0),
        )?;
        if matches != 1 {
            return Err(StoreError::LifecycleConflict(format!(
                "{}: profile is not bound to {}",
                provenance.profile_id,
                phase.as_str()
            )));
        }
    }
    Ok(())
}

fn validate_test_plan_certificates(
    transaction: &Transaction<'_>,
    spec: &LifecycleSpec,
    source_snapshot: Digest,
) -> Result<(), StoreError> {
    for binding in &spec.test_plans {
        let row: Option<(String, String, String, String)> = transaction
            .query_row(
                "SELECT a.artifact_id, a.request_id, a.artifact_json, c.certificate_json
                 FROM artifact_validation_certificates c
                 JOIN artifacts a ON a.artifact_id=c.artifact_id
                 JOIN artifact_requests r ON r.request_id=a.request_id
                 WHERE c.certificate_id=?1 AND a.format_revision=2
                   AND r.source_digest=?2",
                params![binding.certificate_digest.to_string(), source_snapshot.to_string(),],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((artifact_id, request_id, artifact_json, certificate_json)) = row else {
            return Err(StoreError::LifecycleConflict(
                "lifecycle test plan has no persisted validation certificate".to_owned(),
            ));
        };
        let (_, worker_artifact) =
            parse_semantic_artifact(&artifact_id, &request_id, &artifact_json)?;
        crate::semantic_validation::validate_parent_owned_test_plan_binding(
            &worker_artifact,
            &binding.plan,
        )
        .map_err(|_| {
            StoreError::LifecycleConflict(
                "lifecycle test plan differs from its certified artifact".to_owned(),
            )
        })?;
        let certificate: needle_core::ArtifactValidationCertificate =
            serde_json::from_str(&certificate_json)?;
        if certificate.id.digest() != binding.certificate_digest
            || certificate.artifact.to_string() != artifact_id
            || certificate.test_plan_evidence.is_none()
            || !validation_certificate_is_structurally_canonical(&certificate)
        {
            return Err(StoreError::LifecycleConflict(
                "lifecycle test-plan certificate identity or evidence status is invalid".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validation_certificate_is_structurally_canonical(
    certificate: &needle_core::ArtifactValidationCertificate,
) -> bool {
    certificate.dependency_checks_digest == certificate.coverage.dependency_manifest_digest
        && certificate.id
            == crate::semantic_validation::validation_certificate_id(
                certificate.artifact,
                &certificate.input_artifacts,
                &certificate.evidence_ids,
                &certificate.coverage,
                certificate.validator_definition,
                certificate.test_plan_evidence,
            )
}

fn parse_semantic_artifact(
    stored_artifact_id: &str,
    stored_request_id: &str,
    artifact_json: &str,
) -> Result<(needle_core::Artifact, needle_core::SemanticWorkerArtifact), StoreError> {
    let artifact: needle_core::Artifact = serde_json::from_str(artifact_json)?;
    let worker_artifact: needle_core::SemanticWorkerArtifact =
        serde_json::from_value(artifact.payload.clone())?;
    let canonical_id =
        worker_artifact
            .canonical_artifact_id(artifact.contract.definition_digest)
            .ok_or_else(|| StoreError::LifecycleCorruption("semantic artifact bound".to_owned()))?;
    if artifact.id.to_string() != stored_artifact_id
        || artifact.request_id.to_string() != stored_request_id
        || artifact.contract.schema_id != needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID
        || artifact.contract.kind != worker_artifact.kind()
        || artifact.contract.cache_scope != artifact.dependency_manifest.scope
        || canonical_id.digest() != artifact.id
    {
        return Err(StoreError::LifecycleCorruption("semantic artifact identity".to_owned()));
    }
    Ok((artifact, worker_artifact))
}

fn parse_profile_anchor(
    profile_id: Option<String>,
    revision: Option<u64>,
    digest: Option<String>,
) -> Result<Option<RoleProfileProvenance>, StoreError> {
    match (profile_id, revision, digest) {
        (None, None, None) => Ok(None),
        (Some(profile_id), Some(revision), Some(digest)) => Ok(Some(
            RoleProfileProvenance::new(
                needle_core::RoleProfileId::new(profile_id)
                    .map_err(|_| StoreError::LifecycleCorruption("profile id".to_owned()))?,
                revision,
                Digest::parse(&digest)
                    .map_err(|_| StoreError::LifecycleCorruption("profile digest".to_owned()))?,
            )
            .map_err(|_| StoreError::LifecycleCorruption("profile anchor".to_owned()))?,
        )),
        _ => Err(StoreError::LifecycleCorruption("partial role-profile anchor".to_owned())),
    }
}

trait LifecycleConnection {
    fn query_projection_row(
        &self,
        change_id: &ChangeId,
    ) -> rusqlite::Result<Option<LifecycleProjectionRow>>;
}

impl LifecycleConnection for rusqlite::Connection {
    fn query_projection_row(
        &self,
        change_id: &ChangeId,
    ) -> rusqlite::Result<Option<LifecycleProjectionRow>> {
        self.query_row(
            "SELECT lifecycle_id, source_snapshot_digest, state_digest, state_json, generation
             FROM change_lifecycles WHERE change_id=?1",
            [change_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()
    }
}

impl LifecycleConnection for Transaction<'_> {
    fn query_projection_row(
        &self,
        change_id: &ChangeId,
    ) -> rusqlite::Result<Option<LifecycleProjectionRow>> {
        self.query_row(
            "SELECT lifecycle_id, source_snapshot_digest, state_digest, state_json, generation
             FROM change_lifecycles WHERE change_id=?1",
            [change_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()
    }
}

fn lifecycle_projection_in_transaction<C: LifecycleConnection>(
    connection: &C,
    change_id: &ChangeId,
) -> Result<Option<LifecycleProjection>, StoreError> {
    let Some((lifecycle_id, source_snapshot, stored_state_digest, state_json, generation)) =
        connection.query_projection_row(change_id)?
    else {
        return Ok(None);
    };
    let lifecycle: DevelopmentLifecycle = serde_json::from_str(&state_json)?;
    lifecycle.validate().map_err(StoreError::from)?;
    let state_digest = lifecycle.state_digest();
    if lifecycle.change_id != *change_id
        || lifecycle.id.to_string() != lifecycle_id
        || lifecycle.source_snapshot.to_string() != source_snapshot
        || state_digest.to_string() != stored_state_digest
        || lifecycle.generation != generation
    {
        return Err(StoreError::LifecycleCorruption(change_id.to_string()));
    }
    Ok(Some(LifecycleProjection { lifecycle, state_digest }))
}

fn lifecycle_events_in_connection(
    connection: &rusqlite::Connection,
    change_id: &ChangeId,
) -> Result<Vec<LifecycleEvent>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT payload_json, payload_digest FROM change_events
         WHERE change_id=?1 AND lifecycle_sequence IS NOT NULL
         ORDER BY lifecycle_sequence LIMIT ?2",
    )?;
    let events = statement
        .query_map(params![change_id.to_string(), needle_core::MAX_LIFECYCLE_EVENTS + 1], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .map(|row| {
            let (json, stored_digest) = row?;
            if Digest::blake3(json.as_bytes()).to_string() != stored_digest {
                return Err(StoreError::LifecycleCorruption(format!(
                    "{change_id}: lifecycle event payload digest"
                )));
            }
            let event: LifecycleEvent = serde_json::from_str(&json).map_err(|_| {
                StoreError::LifecycleCorruption(format!("{change_id}: lifecycle event JSON"))
            })?;
            if event.change_id != *change_id {
                return Err(StoreError::LifecycleCorruption(format!(
                    "{change_id}: lifecycle event change identity"
                )));
            }
            Ok(event)
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    if events.len() > needle_core::MAX_LIFECYCLE_EVENTS {
        return Err(StoreError::LifecycleCorruption(format!("{change_id}: lifecycle event count")));
    }
    Ok(events)
}

fn replay_lifecycle_in_transaction(
    transaction: &Transaction<'_>,
    change_id: &ChangeId,
) -> Result<LifecycleProjection, StoreError> {
    replay_lifecycle_and_events_in_transaction(transaction, change_id)
        .map(|(projection, _)| projection)
}

fn replay_lifecycle_and_events_in_transaction(
    transaction: &Transaction<'_>,
    change_id: &ChangeId,
) -> Result<(LifecycleProjection, Vec<LifecycleEvent>), StoreError> {
    let persisted = lifecycle_projection_in_transaction(transaction, change_id)?
        .ok_or_else(|| StoreError::LifecycleNotFound(change_id.to_string()))?;
    let events = lifecycle_events_in_connection(transaction, change_id)?;
    let replayed = DevelopmentLifecycle::replay(&events)?;
    let replayed = LifecycleProjection::new(replayed)?;
    if replayed != persisted {
        return Err(StoreError::LifecycleCorruption(format!(
            "{change_id}: event replay differs from current projection"
        )));
    }
    Ok((replayed, events))
}

fn persist_transition(
    transaction: &Transaction<'_>,
    projection: &LifecycleProjection,
    expected_state_digest: Digest,
    transition: LifecycleTransition,
    created_unix_ms: u64,
) -> Result<LifecycleProjection, StoreError> {
    if projection.state_digest != expected_state_digest {
        return Err(StoreError::LifecycleConflict(format!(
            "{}: state digest changed",
            projection.lifecycle.change_id
        )));
    }
    let (next, event) = projection.lifecycle.transition(transition, created_unix_ms)?;
    let next = LifecycleProjection::new(next)?;
    let changed = transaction.execute(
        "UPDATE change_lifecycles
         SET state_digest=?2, generation=?3, state_json=?4, updated_unix_ms=?5
         WHERE change_id=?1 AND state_digest=?6 AND generation=?7",
        params![
            projection.lifecycle.change_id.to_string(),
            next.state_digest.to_string(),
            next.lifecycle.generation,
            serde_json::to_string(&next.lifecycle)?,
            created_unix_ms,
            expected_state_digest.to_string(),
            projection.lifecycle.generation,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::LifecycleConflict(format!(
            "{}: concurrent transition",
            projection.lifecycle.change_id
        )));
    }
    insert_lifecycle_event(transaction, &event, "lifecycle_transitioned")?;
    Ok(next)
}

fn insert_lifecycle_event(
    transaction: &Transaction<'_>,
    event: &LifecycleEvent,
    event_type: &str,
) -> Result<(), StoreError> {
    let payload_json = serde_json::to_string(event)?;
    let payload_digest = Digest::blake3(payload_json.as_bytes());
    transaction.execute(
        "INSERT INTO change_events(
            change_id, event_type, payload_digest, payload_json,
            created_unix_ms, lifecycle_sequence
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            event.change_id.to_string(),
            event_type,
            payload_digest.to_string(),
            payload_json,
            event.created_unix_ms,
            event.sequence,
        ],
    )?;
    Ok(())
}

fn validate_transition_artifacts(
    transaction: &Transaction<'_>,
    lifecycle: &DevelopmentLifecycle,
    transition: &LifecycleTransition,
) -> Result<(), StoreError> {
    match transition {
        LifecycleTransition::CompleteExplore { artifacts, .. } => {
            for artifact in artifacts {
                let row: Option<(String, String, String, String)> = transaction
                    .query_row(
                        "SELECT a.artifact_id, a.request_id, a.artifact_json, c.certificate_json
                         FROM artifacts a
                         JOIN artifact_requests r ON r.request_id=a.request_id
                         JOIN artifact_validation_certificates c ON c.artifact_id=a.artifact_id
                         WHERE a.artifact_id=?1 AND a.format_revision=2
                           AND r.source_digest=?2
                         ORDER BY c.issued_unix_ms DESC, c.certificate_id LIMIT 1",
                        params![artifact.id.to_string(), lifecycle.source_snapshot.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?;
                let Some((artifact_id, request_id, json, certificate_json)) = row else {
                    return Err(StoreError::LifecycleConflict(format!(
                        "{}: exploration artifact is unavailable",
                        lifecycle.change_id
                    )));
                };
                let (_, worker_artifact) =
                    parse_semantic_artifact(&artifact_id, &request_id, &json)?;
                if matches!(worker_artifact, needle_core::SemanticWorkerArtifact::TestPlan { .. }) {
                    return Err(StoreError::LifecycleConflict(format!(
                        "{}: test-plan artifact cannot satisfy exploration",
                        lifecycle.change_id
                    )));
                }
                let certificate: needle_core::ArtifactValidationCertificate =
                    serde_json::from_str(&certificate_json)?;
                if certificate.artifact.to_string() != artifact.id.to_string()
                    || !validation_certificate_is_structurally_canonical(&certificate)
                {
                    return Err(StoreError::LifecycleConflict(format!(
                        "{}: exploration certificate identity is invalid",
                        lifecycle.change_id
                    )));
                }
            }
        }
        LifecycleTransition::CompleteImplement { patch, .. } => {
            let row: Option<(u32, String)> = transaction
                .query_row(
                    "SELECT p.revision, p.source_snapshot_digest
                     FROM patch_artifacts p
                     JOIN change_requests c
                       ON c.change_id=p.change_id AND c.latest_patch_revision=p.revision
                     WHERE p.change_id=?1 AND p.patch_id=?2",
                    params![lifecycle.change_id.to_string(), patch.patch_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if row != Some((patch.revision, lifecycle.source_snapshot.to_string())) {
                return Err(StoreError::LifecycleConflict(format!(
                    "{}: patch is not the latest source-bound revision",
                    lifecycle.change_id
                )));
            }
        }
        LifecycleTransition::CompleteTest { results, .. } => {
            for (binding, result) in lifecycle.spec.test_plans.iter().zip(results) {
                let Some(evidence_id) = result.evidence_id.as_deref() else {
                    continue;
                };
                let evidence_json: Option<String> = transaction
                    .query_row(
                        "SELECT evidence_json FROM command_evidence WHERE evidence_id=?1",
                        [evidence_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(evidence_json) = evidence_json else {
                    return Err(StoreError::LifecycleConflict(format!(
                        "{}: test evidence is unavailable",
                        lifecycle.change_id
                    )));
                };
                let evidence: CommandExecutionEvidence = serde_json::from_str(&evidence_json)?;
                if evidence.source_snapshot_digest != lifecycle.source_snapshot
                    || evidence.runner != binding.plan.runner
                    || evidence.argv != binding.plan.argv
                    || evidence.test_identifier.as_deref()
                        != Some(binding.plan.test_identifier.as_str())
                {
                    return Err(StoreError::LifecycleConflict(format!(
                        "{}: test evidence differs from the frozen plan",
                        lifecycle.change_id
                    )));
                }
                if result.passed {
                    crate::validate_test_evidence(&binding.plan, &evidence).map_err(|_| {
                        StoreError::LifecycleConflict(format!(
                            "{}: passing test evidence is invalid",
                            lifecycle.change_id
                        ))
                    })?;
                } else if result.executed
                    && evidence.exit_status == Some(0)
                    && evidence.infrastructure_failure.is_none()
                {
                    return Err(StoreError::LifecycleConflict(format!(
                        "{}: failed test result contradicts evidence",
                        lifecycle.change_id
                    )));
                }
            }
        }
        LifecycleTransition::CompleteReview { review, .. } => {
            validate_review_artifact(transaction, lifecycle, review)?;
        }
        LifecycleTransition::CompleteVerify { worker, verification } => {
            let json: Option<String> = transaction
                .query_row(
                    "SELECT artifact_json FROM verification_artifacts
                     WHERE verification_id=?1 AND change_id=?2 AND patch_id=?3",
                    params![
                        verification.verification_id.to_string(),
                        lifecycle.change_id.to_string(),
                        verification.patch_id.to_string(),
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(json) = json else {
                return Err(StoreError::LifecycleConflict(format!(
                    "{}: verification artifact is unavailable",
                    lifecycle.change_id
                )));
            };
            let artifact: VerificationArtifact = serde_json::from_str(&json)?;
            if !artifact.is_canonical()
                || artifact.id != verification.verification_id
                || artifact.verdict != verification.verdict
                || artifact.verifier_definition != worker.profile.definition_digest
                || lifecycle.patch.as_ref().map(|patch| patch.patch_id) != Some(artifact.patch_id)
            {
                return Err(StoreError::LifecycleConflict(format!(
                    "{}: verification artifact is not current and canonical",
                    lifecycle.change_id
                )));
            }
        }
        LifecycleTransition::Cancel { .. } | LifecycleTransition::Fail { .. } => {}
        LifecycleTransition::ConsumeRepair
        | LifecycleTransition::ApproveApply { .. }
        | LifecycleTransition::StartApply { .. }
        | LifecycleTransition::FinishApply { .. } => {
            return Err(StoreError::LifecycleConflict(format!(
                "{}: reserved lifecycle transition",
                lifecycle.change_id
            )));
        }
    }
    Ok(())
}

fn validate_review_artifact(
    transaction: &Transaction<'_>,
    lifecycle: &DevelopmentLifecycle,
    review: &ReviewArtifact,
) -> Result<(), StoreError> {
    let request_json: String = transaction.query_row(
        "SELECT request_json FROM change_requests WHERE change_id=?1",
        [lifecycle.change_id.to_string()],
        |row| row.get(0),
    )?;
    let request: ChangeRequest = serde_json::from_str(&request_json)?;
    let expected = request.acceptance_criteria.iter().map(Digest::blake3).collect::<BTreeSet<_>>();
    let observed = review
        .acceptance_coverage
        .iter()
        .map(|coverage| coverage.criterion_digest)
        .collect::<BTreeSet<_>>();
    if expected != observed {
        return Err(StoreError::LifecycleConflict(format!(
            "{}: review coverage differs from acceptance criteria",
            lifecycle.change_id
        )));
    }
    Ok(())
}

pub(super) fn require_lifecycle_worker_phase(
    transaction: &Transaction<'_>,
    change_id: &ChangeId,
    phase: LifecyclePhase,
    provenance: Option<&RoleProfileProvenance>,
) -> Result<bool, StoreError> {
    let Some(projection) = lifecycle_projection_in_transaction(transaction, change_id)? else {
        return Ok(false);
    };
    if projection.lifecycle.phase != phase
        || projection.lifecycle.status != LifecycleStatus::Active
        || projection.lifecycle.spec.profiles.for_phase(phase) != provenance
    {
        return Err(StoreError::LifecycleConflict(format!(
            "{change_id}: worker write is outside the active lifecycle phase"
        )));
    }
    Ok(true)
}

pub(super) fn consume_lifecycle_repair(
    transaction: &Transaction<'_>,
    change_id: &ChangeId,
    patch_id: PatchId,
    verification_id: VerificationArtifactId,
    created_unix_ms: u64,
) -> Result<(), StoreError> {
    let Some(projection) = lifecycle_projection_in_transaction(transaction, change_id)? else {
        return Ok(());
    };
    if projection.lifecycle.patch.as_ref().map(|patch| patch.patch_id) != Some(patch_id)
        || projection
            .lifecycle
            .verification
            .as_ref()
            .map(|verification| verification.verification_id)
            != Some(verification_id)
    {
        return Err(StoreError::LifecycleConflict(format!(
            "{change_id}: repair artifacts differ from lifecycle"
        )));
    }
    persist_transition(
        transaction,
        &projection,
        projection.state_digest,
        LifecycleTransition::ConsumeRepair,
        created_unix_ms,
    )?;
    Ok(())
}

pub(super) fn fail_lifecycle(
    transaction: &Transaction<'_>,
    change_id: &ChangeId,
    reason: LifecycleReason,
    created_unix_ms: u64,
) -> Result<(), StoreError> {
    let Some(projection) = lifecycle_projection_in_transaction(transaction, change_id)? else {
        return Ok(());
    };
    if projection.lifecycle.status != LifecycleStatus::Active
        || !matches!(
            projection.lifecycle.phase,
            LifecyclePhase::Explore | LifecyclePhase::Implement
        )
    {
        return Err(StoreError::LifecycleConflict(format!(
            "{change_id}: preparation failure is outside an active preparation phase"
        )));
    }
    persist_transition(
        transaction,
        &projection,
        projection.state_digest,
        LifecycleTransition::Fail { reason },
        created_unix_ms,
    )?;
    Ok(())
}

pub(super) fn begin_lifecycle_apply(
    transaction: &Transaction<'_>,
    change_id: &ChangeId,
    expected_state_digest: Option<Digest>,
    apply_id: ChangeApplyId,
    patch_id: PatchId,
    verification_id: VerificationArtifactId,
    created_unix_ms: u64,
) -> Result<(), StoreError> {
    let lifecycle = lifecycle_projection_in_transaction(transaction, change_id)?;
    match (lifecycle, expected_state_digest) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(StoreError::LifecycleNotFound(change_id.to_string())),
        (Some(_), None) => Err(StoreError::LifecycleConflict(format!(
            "{change_id}: lifecycle apply requires its current state digest"
        ))),
        (Some(projection), Some(expected)) => {
            if projection.lifecycle.patch.as_ref().map(|patch| patch.patch_id) != Some(patch_id)
                || projection
                    .lifecycle
                    .verification
                    .as_ref()
                    .map(|verification| verification.verification_id)
                    != Some(verification_id)
            {
                return Err(StoreError::LifecycleConflict(format!(
                    "{change_id}: apply artifacts differ from approved lifecycle"
                )));
            }
            persist_transition(
                transaction,
                &projection,
                expected,
                LifecycleTransition::StartApply { apply_id },
                created_unix_ms,
            )?;
            Ok(())
        }
    }
}

pub(super) fn finish_lifecycle_apply(
    transaction: &Transaction<'_>,
    change_id: &ChangeId,
    apply_id: ChangeApplyId,
    status: ChangeApplyStatus,
    completed_unix_ms: u64,
) -> Result<(), StoreError> {
    let Some(projection) = lifecycle_projection_in_transaction(transaction, change_id)? else {
        return Ok(());
    };
    persist_transition(
        transaction,
        &projection,
        projection.state_digest,
        LifecycleTransition::FinishApply { apply_id, status },
        completed_unix_ms,
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "lifecycles/tests.rs"]
mod tests;
