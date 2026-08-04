use crate::{ProofCandidate, built_in_presets, built_in_routes};
use needle_core::{
    ApprovalDecision, ApprovalDecisionSource, ApprovalRequest, Artifact, ArtifactRequest,
    ArtifactValidationCertificate, CacheLookup, CacheResolution, CapabilityClass, CapabilityMode,
    CommandClassification, CommandExecutionEvidence, Digest, EvidenceFailurePolicy,
    MainTurnOutcome, ModelPolicy, MultiNeedPolicy, Need, NeedCacheEntry, NeedCacheIdentity,
    NeedFragment, NeedIr, NeedStep, NeedStepRelation, NeedStepState, Preset,
    ReuseSufficiencyCertificate, RoleProfileId, RoleProfileProvenance, RoleProfileRevision, Route,
    SelectedPlan, SemanticInterrupt, TestPlan, WorkerConfig, WorkerFailure, WorkerOutcome,
    WorkerProfile, built_in_capability_classes, built_in_claim_capability_classes,
    built_in_predicate_contracts, built_in_route_contracts, built_in_route_plans,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::fs;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[path = "store/changes.rs"]
mod changes;
#[path = "store/claims.rs"]
mod claims;
#[path = "store/lifecycles.rs"]
mod lifecycles;
#[path = "store/role_profiles.rs"]
mod role_profiles;

pub use changes::{
    ChangeAttemptRecord, LifecycleChangeContext, PatchFileBlob, PreparedChangeRecord,
};
pub use lifecycles::LifecycleProjection;
pub use role_profiles::{
    RoleProfileAuditOperation, RoleProfileAuditRecord, RoleProfileStateRecord,
};

pub struct NeedShadowWrite<'a> {
    pub session_id: &'a str,
    pub turn_id: &'a str,
    pub transport_digest: Digest,
    pub parser_definition_digest: Digest,
    pub prompt_profile_digest: Digest,
    pub need_ir: &'a NeedIr,
    pub need: &'a Need,
    pub fragments: &'a [NeedFragment],
}

const MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    checksum TEXT NOT NULL,
    applied_unix_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS presets (
    id TEXT PRIMARY KEY,
    definition_digest TEXT NOT NULL,
    definition_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS routes (
    id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL,
    priority INTEGER NOT NULL,
    definition_digest TEXT NOT NULL,
    definition_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    turn_id TEXT,
    root_task TEXT,
    prompt_profile_digest TEXT NOT NULL,
    route_set_digest TEXT NOT NULL,
    model TEXT,
    cwd TEXT,
    updated_unix_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS worker_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    identity_digest TEXT NOT NULL,
    model TEXT NOT NULL,
    reasoning TEXT NOT NULL,
    status TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    input_tokens INTEGER,
    cached_input_tokens INTEGER,
    output_tokens INTEGER,
    result_digest TEXT,
    created_unix_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS cache_entries (
    identity_digest TEXT PRIMARY KEY,
    logical_digest TEXT NOT NULL,
    source_digest TEXT NOT NULL,
    entry_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    hit_count INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS cache_entries_logical ON cache_entries(logical_digest);
CREATE TABLE IF NOT EXISTS worker_leases (
    identity_digest TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    expires_unix_ms INTEGER NOT NULL
);
"#;

const MIGRATION_V2: &str = r#"
ALTER TABLE worker_runs ADD COLUMN failure_code TEXT;
ALTER TABLE worker_runs ADD COLUMN failure_diagnostic TEXT;
ALTER TABLE worker_runs ADD COLUMN discarded_facts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE worker_runs ADD COLUMN logical_worker_spawns INTEGER NOT NULL DEFAULT 1;
ALTER TABLE worker_runs ADD COLUMN worker_turns INTEGER NOT NULL DEFAULT 1;
ALTER TABLE worker_runs ADD COLUMN repair_performed INTEGER NOT NULL DEFAULT 0;
ALTER TABLE worker_runs ADD COLUMN worker_session_id TEXT;
ALTER TABLE worker_runs ADD COLUMN session_cleanup_success INTEGER;
"#;

const MIGRATION_V3: &str = r#"
CREATE TABLE definitions (
    definition_digest TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    definition_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    UNIQUE(kind, id, revision)
);
CREATE TABLE route_plans (
    definition_digest TEXT PRIMARY KEY REFERENCES definitions(definition_digest),
    route_key TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE artifact_requests (
    request_id TEXT PRIMARY KEY,
    logical_id TEXT NOT NULL,
    source_digest TEXT NOT NULL,
    contract_id TEXT NOT NULL,
    route_key TEXT NOT NULL,
    request_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX artifact_requests_logical ON artifact_requests(logical_id);
CREATE TABLE artifacts (
    artifact_id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL REFERENCES artifact_requests(request_id),
    contract_id TEXT NOT NULL,
    artifact_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX artifacts_request ON artifacts(request_id);
CREATE TABLE artifact_inputs (
    request_id TEXT NOT NULL REFERENCES artifact_requests(request_id),
    position INTEGER NOT NULL,
    artifact_id TEXT NOT NULL,
    PRIMARY KEY(request_id, position)
);
CREATE TABLE dependencies (
    artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),
    path TEXT NOT NULL,
    content_digest TEXT NOT NULL,
    byte_start INTEGER,
    byte_end INTEGER,
    claims_json TEXT NOT NULL,
    PRIMARY KEY(artifact_id, path, content_digest)
);
CREATE TABLE validations (
    artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),
    validator TEXT NOT NULL,
    validator_revision INTEGER NOT NULL,
    status TEXT NOT NULL,
    evidence_digest TEXT NOT NULL,
    validated_unix_ms INTEGER NOT NULL,
    PRIMARY KEY(artifact_id, validator, validator_revision, evidence_digest)
);
CREATE TABLE execution_attempts (
    attempt_id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL REFERENCES artifact_requests(request_id),
    status TEXT NOT NULL,
    attempt_json TEXT NOT NULL,
    started_unix_ms INTEGER NOT NULL,
    completed_unix_ms INTEGER
);
CREATE TABLE command_evidence (
    evidence_id TEXT PRIMARY KEY,
    attempt_id TEXT,
    approval_id TEXT NOT NULL,
    evidence_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE TABLE request_mappings (
    legacy_identity_digest TEXT PRIMARY KEY,
    request_id TEXT NOT NULL REFERENCES artifact_requests(request_id),
    revalidated_artifact_id TEXT REFERENCES artifacts(artifact_id),
    created_unix_ms INTEGER NOT NULL
);
CREATE TABLE negative_attempts (
    attempt_identity TEXT PRIMARY KEY,
    failure_code TEXT NOT NULL,
    failure_json TEXT NOT NULL,
    expires_unix_ms INTEGER NOT NULL
);
CREATE TABLE usage_records (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id TEXT,
    route_key TEXT NOT NULL,
    usage_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE TABLE route_promotions (
    route_key TEXT NOT NULL,
    worker_profile_digest TEXT NOT NULL,
    evidence_digest TEXT NOT NULL,
    promoted_unix_ms INTEGER NOT NULL,
    PRIMARY KEY(route_key, worker_profile_digest)
);
CREATE TABLE approval_requests (
    approval_id TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    expires_unix_ms INTEGER NOT NULL,
    request_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX approval_requests_status ON approval_requests(status, expires_unix_ms);
CREATE TABLE approval_decisions (
    approval_id TEXT PRIMARY KEY REFERENCES approval_requests(approval_id),
    decision TEXT NOT NULL,
    source TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    decided_unix_ms INTEGER NOT NULL
);
CREATE TABLE artifact_leases (
    request_id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    expires_unix_ms INTEGER NOT NULL
);
"#;

const MIGRATION_V4: &str = r#"
ALTER TABLE sessions ADD COLUMN route_set_json TEXT;
"#;

const MIGRATION_V5: &str = r#"
ALTER TABLE sessions ADD COLUMN need_grammar_digest TEXT;
CREATE TABLE predicate_contracts (
    definition_digest TEXT PRIMARY KEY,
    predicate TEXT NOT NULL,
    definition_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE TABLE route_contracts (
    definition_digest TEXT PRIMARY KEY,
    route_key TEXT NOT NULL,
    definition_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX route_contracts_route ON route_contracts(route_key, created_unix_ms DESC);
CREATE TABLE capability_classes (
    id TEXT PRIMARY KEY,
    predicate TEXT NOT NULL,
    mode TEXT NOT NULL,
    definition_digest TEXT NOT NULL,
    definition_json TEXT NOT NULL,
    evidence_digest TEXT,
    updated_unix_ms INTEGER NOT NULL
);
CREATE INDEX capability_classes_predicate ON capability_classes(predicate, mode);
CREATE TABLE need_ir_records (
    record_id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    transport_digest TEXT NOT NULL,
    format_revision INTEGER NOT NULL,
    parser_definition_digest TEXT NOT NULL,
    prompt_profile_digest TEXT NOT NULL,
    route_key TEXT NOT NULL,
    need_id TEXT NOT NULL REFERENCES needs(need_id),
    ir_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    UNIQUE(session_id, turn_id, transport_digest)
);
CREATE TABLE needs (
    need_id TEXT PRIMARY KEY,
    route_key TEXT NOT NULL,
    world_digest TEXT NOT NULL,
    need_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE TABLE need_fragments (
    fragment_id TEXT PRIMARY KEY,
    need_id TEXT NOT NULL REFERENCES needs(need_id),
    fragment_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX need_fragments_need ON need_fragments(need_id);
CREATE TABLE need_obligations (
    need_id TEXT NOT NULL REFERENCES needs(need_id),
    obligation_id TEXT NOT NULL,
    predicate TEXT NOT NULL,
    required INTEGER NOT NULL,
    obligation_json TEXT NOT NULL,
    PRIMARY KEY(need_id, obligation_id, required)
);
CREATE INDEX need_obligations_lookup ON need_obligations(predicate, obligation_id);
CREATE TABLE residual_intents (
    need_id TEXT PRIMARY KEY REFERENCES needs(need_id),
    residual_json TEXT NOT NULL
);
CREATE TABLE subjects (
    subject_id TEXT PRIMARY KEY,
    repository_lineage TEXT NOT NULL,
    kind TEXT NOT NULL,
    canonical_name TEXT NOT NULL,
    subject_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    UNIQUE(repository_lineage, kind, canonical_name)
);
"#;

const MIGRATION_V6: &str = r#"
ALTER TABLE artifact_requests ADD COLUMN format_revision INTEGER NOT NULL DEFAULT 1;
ALTER TABLE artifact_requests ADD COLUMN demand_id TEXT;
ALTER TABLE artifact_requests ADD COLUMN semantic_policy_digest TEXT;
ALTER TABLE artifact_requests ADD COLUMN dependency_context_digest TEXT;
ALTER TABLE artifacts ADD COLUMN format_revision INTEGER NOT NULL DEFAULT 1;
CREATE TABLE semantic_worlds (
    world_digest TEXT PRIMARY KEY,
    world_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE TABLE dependency_manifests (
    manifest_digest TEXT PRIMARY KEY,
    manifest_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE TABLE artifact_origins (
    artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),
    request_id TEXT NOT NULL REFERENCES artifact_requests(request_id),
    route_key TEXT NOT NULL,
    need_id TEXT,
    observed_unix_ms INTEGER NOT NULL,
    PRIMARY KEY(artifact_id, request_id, route_key)
);
CREATE TABLE artifact_validation_certificates (
    certificate_id TEXT PRIMARY KEY,
    artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),
    validator_definition_digest TEXT NOT NULL,
    dependency_manifest_digest TEXT NOT NULL,
    world_digest TEXT NOT NULL REFERENCES semantic_worlds(world_digest),
    certificate_json TEXT NOT NULL,
    issued_unix_ms INTEGER NOT NULL
);
CREATE INDEX artifact_validation_artifact
    ON artifact_validation_certificates(artifact_id, issued_unix_ms DESC);
CREATE TABLE coverage_entries (
    certificate_id TEXT NOT NULL REFERENCES artifact_validation_certificates(certificate_id),
    artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),
    obligation_id TEXT NOT NULL,
    predicate TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    world_digest TEXT NOT NULL,
    obligation_json TEXT NOT NULL,
    evidence_json TEXT NOT NULL,
    PRIMARY KEY(certificate_id, obligation_id)
);
CREATE INDEX coverage_exact_lookup
    ON coverage_entries(predicate, subject_id, world_digest, obligation_id);
CREATE INDEX coverage_subject_lookup
    ON coverage_entries(predicate, subject_id, world_digest);
CREATE TABLE contradiction_records (
    contradiction_id TEXT PRIMARY KEY,
    predicate TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    world_digest TEXT NOT NULL,
    status TEXT NOT NULL,
    artifact_ids_json TEXT NOT NULL,
    updated_unix_ms INTEGER NOT NULL
);
CREATE INDEX contradictions_active
    ON contradiction_records(predicate, subject_id, world_digest, status);
"#;

const MIGRATION_V7: &str = r#"
CREATE TABLE reuse_sufficiency_certificates (
    certificate_id TEXT PRIMARY KEY,
    need_id TEXT NOT NULL REFERENCES needs(need_id),
    engine_definition_digest TEXT NOT NULL,
    certificate_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE TABLE satisfaction_steps (
    certificate_id TEXT NOT NULL
        REFERENCES reuse_sufficiency_certificates(certificate_id),
    position INTEGER NOT NULL,
    obligation_id TEXT NOT NULL,
    artifact_id TEXT NOT NULL,
    validation_certificate_id TEXT NOT NULL,
    PRIMARY KEY(certificate_id, position)
);
CREATE INDEX satisfaction_obligation
    ON satisfaction_steps(obligation_id, certificate_id);
CREATE TABLE selected_plans (
    plan_id TEXT PRIMARY KEY,
    need_id TEXT NOT NULL REFERENCES needs(need_id),
    resolution TEXT NOT NULL,
    plan_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE TABLE plan_candidates (
    plan_id TEXT NOT NULL REFERENCES selected_plans(plan_id),
    position INTEGER NOT NULL,
    candidate_json TEXT NOT NULL,
    selected INTEGER NOT NULL,
    PRIMARY KEY(plan_id, position)
);
CREATE TABLE proof_accounting (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    need_id TEXT NOT NULL,
    plan_id TEXT,
    parse_micros INTEGER NOT NULL,
    lookup_micros INTEGER NOT NULL,
    validation_micros INTEGER NOT NULL,
    planning_micros INTEGER NOT NULL,
    projection_micros INTEGER NOT NULL,
    allocation_count INTEGER,
    allocated_bytes INTEGER,
    stale_candidates INTEGER NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX proof_accounting_need ON proof_accounting(need_id, created_unix_ms DESC);
CREATE TABLE capability_mode_audit (
    audit_id INTEGER PRIMARY KEY AUTOINCREMENT,
    capability_id TEXT NOT NULL,
    previous_mode TEXT NOT NULL,
    new_mode TEXT NOT NULL,
    definition_digest TEXT NOT NULL,
    evidence_digest TEXT,
    changed_unix_ms INTEGER NOT NULL
);
CREATE INDEX capability_mode_audit_capability
    ON capability_mode_audit(capability_id, changed_unix_ms DESC);
"#;

const MIGRATION_V8: &str = r#"
ALTER TABLE sessions ADD COLUMN multi_need_policy_json TEXT;
ALTER TABLE sessions ADD COLUMN multi_need_policy_digest TEXT;
CREATE TABLE need_steps (
    need_step_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    turn_id TEXT NOT NULL,
    coordination TEXT NOT NULL,
    relation TEXT NOT NULL,
    need_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    request_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    UNIQUE(session_id, ordinal)
);
CREATE INDEX need_steps_session ON need_steps(session_id, ordinal);
CREATE TABLE need_step_events (
    event_id INTEGER PRIMARY KEY AUTOINCREMENT,
    need_step_id TEXT NOT NULL REFERENCES need_steps(need_step_id),
    state TEXT NOT NULL,
    event_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX need_step_events_step ON need_step_events(need_step_id, event_id);
CREATE TABLE need_step_artifacts (
    need_step_id TEXT NOT NULL REFERENCES need_steps(need_step_id),
    artifact_id TEXT NOT NULL,
    proof_id TEXT,
    role TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    PRIMARY KEY(need_step_id, artifact_id, role)
);
CREATE TABLE main_turn_observations (
    observation_id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    need_step_id TEXT,
    status TEXT NOT NULL,
    delivery TEXT,
    usage_json TEXT NOT NULL,
    tools_json TEXT NOT NULL,
    main_discovery_tainted INTEGER NOT NULL,
    outcome_json TEXT,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX main_turn_observations_session
    ON main_turn_observations(session_id, observation_id);
"#;

const MIGRATION_V9: &str = r#"
ALTER TABLE sessions ADD COLUMN ended_unix_ms INTEGER;
CREATE INDEX sessions_ended ON sessions(ended_unix_ms, updated_unix_ms);
"#;

const MIGRATION_V10: &str = r#"
CREATE TABLE need_step_requests (
    need_step_id TEXT PRIMARY KEY NOT NULL REFERENCES need_steps(need_step_id),
    session_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    raw_message TEXT NOT NULL,
    semantic_interrupt_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX need_step_requests_session
    ON need_step_requests(session_id, need_step_id);
"#;

const MIGRATION_V11: &str = r#"
ALTER TABLE sessions ADD COLUMN transport TEXT;
ALTER TABLE sessions ADD COLUMN transport_definition_digest TEXT;
ALTER TABLE sessions ADD COLUMN semantic_definition_digest TEXT;
ALTER TABLE need_step_requests ADD COLUMN transport TEXT;
ALTER TABLE need_step_requests ADD COLUMN request_format TEXT;
"#;

const MIGRATION_V12: &str = r#"
CREATE TABLE semantic_claims (
    claim_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    contract_definition_digest TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE TABLE claim_origins (
    claim_id TEXT NOT NULL REFERENCES semantic_claims(claim_id),
    artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),
    validation_certificate_id TEXT NOT NULL
        REFERENCES artifact_validation_certificates(certificate_id),
    subject_id TEXT NOT NULL,
    world_digest TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    PRIMARY KEY(claim_id, artifact_id, validation_certificate_id)
);
CREATE INDEX claim_origins_artifact
    ON claim_origins(artifact_id, validation_certificate_id);
CREATE TABLE claim_relations (
    relation_id TEXT PRIMARY KEY,
    from_claim_id TEXT NOT NULL REFERENCES semantic_claims(claim_id),
    to_claim_id TEXT NOT NULL REFERENCES semantic_claims(claim_id),
    relation_kind TEXT NOT NULL,
    relation_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX claim_relations_from
    ON claim_relations(from_claim_id, relation_kind, to_claim_id);
CREATE TABLE claim_dependencies (
    claim_id TEXT NOT NULL REFERENCES semantic_claims(claim_id),
    path TEXT NOT NULL,
    content_digest TEXT NOT NULL,
    byte_start INTEGER,
    byte_end INTEGER,
    PRIMARY KEY(claim_id, path, content_digest)
);
CREATE TABLE claim_validation_certificates (
    certificate_id TEXT PRIMARY KEY,
    claim_id TEXT NOT NULL REFERENCES semantic_claims(claim_id),
    origin_artifact_id TEXT NOT NULL REFERENCES artifacts(artifact_id),
    origin_validation_certificate_id TEXT NOT NULL
        REFERENCES artifact_validation_certificates(certificate_id),
    subject_id TEXT NOT NULL,
    world_digest TEXT NOT NULL,
    validator_definition_digest TEXT NOT NULL,
    certificate_json TEXT NOT NULL,
    issued_unix_ms INTEGER NOT NULL
);
CREATE INDEX claim_validation_claim
    ON claim_validation_certificates(claim_id, issued_unix_ms DESC);
CREATE TABLE claim_coverage_entries (
    certificate_id TEXT NOT NULL
        REFERENCES claim_validation_certificates(certificate_id),
    claim_id TEXT NOT NULL REFERENCES semantic_claims(claim_id),
    obligation_id TEXT NOT NULL,
    predicate TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    world_digest TEXT NOT NULL,
    coverage_json TEXT NOT NULL,
    PRIMARY KEY(certificate_id, obligation_id)
);
CREATE INDEX claim_coverage_lookup
    ON claim_coverage_entries(predicate, subject_id, world_digest, obligation_id);
CREATE TABLE claim_set_certificates (
    certificate_id TEXT PRIMARY KEY,
    need_id TEXT NOT NULL REFERENCES needs(need_id),
    engine_definition_digest TEXT NOT NULL,
    world_digest TEXT NOT NULL,
    certificate_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE TABLE claim_set_members (
    certificate_id TEXT NOT NULL REFERENCES claim_set_certificates(certificate_id),
    position INTEGER NOT NULL,
    claim_id TEXT NOT NULL REFERENCES semantic_claims(claim_id),
    claim_validation_certificate_id TEXT NOT NULL
        REFERENCES claim_validation_certificates(certificate_id),
    PRIMARY KEY(certificate_id, position)
);
CREATE TABLE claim_contradiction_members (
    contradiction_id TEXT NOT NULL
        REFERENCES contradiction_records(contradiction_id),
    claim_id TEXT NOT NULL REFERENCES semantic_claims(claim_id),
    PRIMARY KEY(contradiction_id, claim_id)
);
CREATE TABLE operator_cost_observations (
    observation_id INTEGER PRIMARY KEY AUTOINCREMENT,
    artifact_kind TEXT NOT NULL,
    worker_model TEXT NOT NULL,
    worker_reasoning TEXT NOT NULL,
    service_tier TEXT NOT NULL,
    schema_digest TEXT NOT NULL,
    validator_definition_digest TEXT NOT NULL,
    pricing_digest TEXT NOT NULL,
    requested_kind_count INTEGER NOT NULL CHECK(requested_kind_count = 1),
    cost_microusd INTEGER NOT NULL,
    execution_attempt_id TEXT,
    evidence_digest TEXT NOT NULL UNIQUE,
    observed_unix_ms INTEGER NOT NULL
);
CREATE INDEX operator_cost_lookup
    ON operator_cost_observations(
        artifact_kind, worker_model, worker_reasoning, service_tier,
        schema_digest, validator_definition_digest, pricing_digest,
        observed_unix_ms DESC
    );
"#;

const MIGRATION_V13: &str = r#"
CREATE TABLE change_requests (
    change_id TEXT PRIMARY KEY,
    request_digest TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    source_snapshot_digest TEXT NOT NULL,
    state TEXT NOT NULL,
    request_json TEXT NOT NULL,
    latest_patch_revision INTEGER NOT NULL DEFAULT 0,
    repair_attempted INTEGER NOT NULL DEFAULT 0,
    created_unix_ms INTEGER NOT NULL,
    updated_unix_ms INTEGER NOT NULL
);
CREATE INDEX change_requests_repository
    ON change_requests(repository_id, updated_unix_ms DESC);
CREATE TABLE change_events (
    event_id INTEGER PRIMARY KEY AUTOINCREMENT,
    change_id TEXT NOT NULL REFERENCES change_requests(change_id),
    event_type TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX change_events_change ON change_events(change_id, event_id);
CREATE TABLE patch_artifacts (
    patch_id TEXT PRIMARY KEY,
    change_id TEXT NOT NULL REFERENCES change_requests(change_id),
    revision INTEGER NOT NULL,
    source_snapshot_digest TEXT NOT NULL,
    patch_digest TEXT NOT NULL,
    artifact_json TEXT NOT NULL,
    manifest_json TEXT NOT NULL,
    declared_output_json TEXT NOT NULL,
    discrepancies_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    UNIQUE(change_id, revision)
);
CREATE INDEX patch_artifacts_change
    ON patch_artifacts(change_id, revision DESC);
CREATE TABLE patch_files (
    patch_id TEXT NOT NULL REFERENCES patch_artifacts(patch_id),
    path TEXT NOT NULL,
    operation TEXT NOT NULL,
    before_digest TEXT,
    after_digest TEXT,
    before_blob BLOB,
    after_blob BLOB,
    PRIMARY KEY(patch_id, path)
);
CREATE TABLE verification_artifacts (
    verification_id TEXT PRIMARY KEY,
    change_id TEXT NOT NULL REFERENCES change_requests(change_id),
    patch_id TEXT NOT NULL REFERENCES patch_artifacts(patch_id),
    verdict TEXT NOT NULL,
    artifact_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX verification_artifacts_change
    ON verification_artifacts(change_id, created_unix_ms DESC);
CREATE TABLE change_attempts (
    attempt_id INTEGER PRIMARY KEY AUTOINCREMENT,
    change_id TEXT NOT NULL REFERENCES change_requests(change_id),
    patch_id TEXT,
    role TEXT NOT NULL,
    attempt_json TEXT NOT NULL,
    usage_json TEXT NOT NULL,
    cost_microusd INTEGER,
    created_unix_ms INTEGER NOT NULL
);
CREATE INDEX change_attempts_change ON change_attempts(change_id, attempt_id);
CREATE TABLE change_applies (
    apply_id TEXT PRIMARY KEY,
    change_id TEXT NOT NULL REFERENCES change_requests(change_id),
    patch_id TEXT NOT NULL REFERENCES patch_artifacts(patch_id),
    repository_root TEXT NOT NULL,
    pre_snapshot_digest TEXT NOT NULL,
    post_snapshot_digest TEXT,
    status TEXT NOT NULL,
    journal_json TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL,
    completed_unix_ms INTEGER
);
CREATE INDEX change_applies_change ON change_applies(change_id, created_unix_ms DESC);
"#;

const MIGRATION_V14: &str = r#"
CREATE TABLE role_profiles (
    profile_id TEXT NOT NULL PRIMARY KEY CHECK(length(profile_id) BETWEEN 1 AND 64),
    role TEXT NOT NULL CHECK(role IN ('explorer','implementer','test_runner','reviewer','verifier','auditor')),
    created_unix_ms INTEGER NOT NULL CHECK(created_unix_ms >= 0)
);
CREATE TABLE role_profile_revisions (
    profile_id TEXT NOT NULL REFERENCES role_profiles(profile_id),
    revision INTEGER NOT NULL CHECK(revision > 0),
    definition_digest TEXT NOT NULL CHECK(length(definition_digest) = 67),
    definition_json TEXT NOT NULL CHECK(length(definition_json) <= 32768),
    created_unix_ms INTEGER NOT NULL CHECK(created_unix_ms >= 0),
    activated_unix_ms INTEGER CHECK(activated_unix_ms IS NULL OR activated_unix_ms >= 0),
    PRIMARY KEY(profile_id, revision),
    UNIQUE(definition_digest),
    UNIQUE(profile_id, definition_digest)
);
CREATE INDEX role_profile_revisions_digest ON role_profile_revisions(definition_digest);
CREATE INDEX role_profile_revisions_history ON role_profile_revisions(profile_id, revision DESC);
CREATE TABLE role_profile_state (
    profile_id TEXT NOT NULL PRIMARY KEY REFERENCES role_profiles(profile_id),
    latest_revision INTEGER NOT NULL CHECK(latest_revision > 0),
    active_revision INTEGER CHECK(active_revision IS NULL OR active_revision > 0),
    state_generation INTEGER NOT NULL CHECK(state_generation >= 0),
    updated_unix_ms INTEGER NOT NULL CHECK(updated_unix_ms >= 0),
    FOREIGN KEY(profile_id, latest_revision)
        REFERENCES role_profile_revisions(profile_id, revision),
    FOREIGN KEY(profile_id, active_revision)
        REFERENCES role_profile_revisions(profile_id, revision)
);
CREATE INDEX role_profile_state_active ON role_profile_state(active_revision);
CREATE TABLE role_profile_audit (
    audit_id INTEGER PRIMARY KEY AUTOINCREMENT,
    profile_id TEXT NOT NULL REFERENCES role_profiles(profile_id),
    revision INTEGER NOT NULL CHECK(revision > 0),
    definition_digest TEXT NOT NULL CHECK(length(definition_digest) = 67),
    operation TEXT NOT NULL CHECK(operation IN ('create','revise','activate','deactivate')),
    prior_state TEXT CHECK(prior_state IS NULL OR prior_state IN ('draft','active','inactive')),
    resulting_state TEXT NOT NULL CHECK(resulting_state IN ('draft','active','inactive')),
    prior_state_digest TEXT CHECK(prior_state_digest IS NULL OR length(prior_state_digest) = 67),
    resulting_state_digest TEXT NOT NULL CHECK(length(resulting_state_digest) = 67),
    prior_active_revision INTEGER CHECK(prior_active_revision IS NULL OR prior_active_revision > 0),
    prior_active_digest TEXT CHECK(prior_active_digest IS NULL OR length(prior_active_digest) = 67),
    resulting_active_revision INTEGER CHECK(resulting_active_revision IS NULL OR resulting_active_revision > 0),
    resulting_active_digest TEXT CHECK(resulting_active_digest IS NULL OR length(resulting_active_digest) = 67),
    created_unix_ms INTEGER NOT NULL CHECK(created_unix_ms >= 0)
);
CREATE INDEX role_profile_audit_profile ON role_profile_audit(profile_id, audit_id DESC);
CREATE INDEX role_profile_audit_digest ON role_profile_audit(definition_digest, audit_id DESC);
CREATE TRIGGER role_profile_revisions_immutable
BEFORE UPDATE OF profile_id, revision, definition_digest, definition_json, created_unix_ms
ON role_profile_revisions
WHEN NEW.profile_id <> OLD.profile_id
  OR NEW.revision <> OLD.revision
  OR NEW.definition_digest <> OLD.definition_digest
  OR NEW.definition_json <> OLD.definition_json
  OR NEW.created_unix_ms <> OLD.created_unix_ms
BEGIN
    SELECT RAISE(ABORT, 'role profile revision identity is immutable');
END;
CREATE TRIGGER role_profile_revisions_no_delete
BEFORE DELETE ON role_profile_revisions
BEGIN
    SELECT RAISE(ABORT, 'role profile revision history is immutable');
END;
CREATE TRIGGER role_profile_audit_append_only
BEFORE UPDATE ON role_profile_audit
BEGIN
    SELECT RAISE(ABORT, 'role profile audit is append-only');
END;
CREATE TRIGGER role_profile_audit_no_delete
BEFORE DELETE ON role_profile_audit
BEGIN
    SELECT RAISE(ABORT, 'role profile audit is append-only');
END;
"#;

const MIGRATION_V15: &str = r#"
ALTER TABLE sessions ADD COLUMN role_profile_id TEXT;
ALTER TABLE sessions ADD COLUMN role_profile_revision INTEGER;
ALTER TABLE sessions ADD COLUMN role_profile_definition_digest TEXT;
ALTER TABLE worker_runs ADD COLUMN role_profile_id TEXT;
ALTER TABLE worker_runs ADD COLUMN role_profile_revision INTEGER;
ALTER TABLE worker_runs ADD COLUMN role_profile_definition_digest TEXT;
ALTER TABLE change_attempts ADD COLUMN role_profile_id TEXT;
ALTER TABLE change_attempts ADD COLUMN role_profile_revision INTEGER;
ALTER TABLE change_attempts ADD COLUMN role_profile_definition_digest TEXT;
ALTER TABLE change_requests ADD COLUMN role_profile_id TEXT;
ALTER TABLE change_requests ADD COLUMN role_profile_revision INTEGER;
ALTER TABLE change_requests ADD COLUMN role_profile_definition_digest TEXT;
CREATE TRIGGER sessions_role_profile_provenance_all_or_none_insert
BEFORE INSERT ON sessions
WHEN (NEW.role_profile_id IS NULL) != (NEW.role_profile_revision IS NULL)
  OR (NEW.role_profile_id IS NULL) != (NEW.role_profile_definition_digest IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'session role-profile provenance must be all NULL or all set');
END;
CREATE TRIGGER sessions_role_profile_provenance_all_or_none_update
BEFORE UPDATE OF role_profile_id, role_profile_revision, role_profile_definition_digest
ON sessions
WHEN (NEW.role_profile_id IS NULL) != (NEW.role_profile_revision IS NULL)
  OR (NEW.role_profile_id IS NULL) != (NEW.role_profile_definition_digest IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'session role-profile provenance must be all NULL or all set');
END;
CREATE TRIGGER sessions_role_profile_provenance_immutable
BEFORE UPDATE OF role_profile_id, role_profile_revision, role_profile_definition_digest
ON sessions
WHEN NEW.role_profile_id IS NOT OLD.role_profile_id
  OR NEW.role_profile_revision IS NOT OLD.role_profile_revision
  OR NEW.role_profile_definition_digest IS NOT OLD.role_profile_definition_digest
BEGIN
    SELECT RAISE(ABORT, 'session role-profile provenance is immutable');
END;
CREATE TRIGGER worker_runs_role_profile_provenance_all_or_none_insert
BEFORE INSERT ON worker_runs
WHEN (NEW.role_profile_id IS NULL) != (NEW.role_profile_revision IS NULL)
  OR (NEW.role_profile_id IS NULL) != (NEW.role_profile_definition_digest IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'worker-run role-profile provenance must be all NULL or all set');
END;
CREATE TRIGGER worker_runs_role_profile_provenance_all_or_none_update
BEFORE UPDATE OF role_profile_id, role_profile_revision, role_profile_definition_digest
ON worker_runs
WHEN (NEW.role_profile_id IS NULL) != (NEW.role_profile_revision IS NULL)
  OR (NEW.role_profile_id IS NULL) != (NEW.role_profile_definition_digest IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'worker-run role-profile provenance must be all NULL or all set');
END;
CREATE TRIGGER change_attempts_role_profile_provenance_all_or_none_insert
BEFORE INSERT ON change_attempts
WHEN (NEW.role_profile_id IS NULL) != (NEW.role_profile_revision IS NULL)
  OR (NEW.role_profile_id IS NULL) != (NEW.role_profile_definition_digest IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'change-attempt role-profile provenance must be all NULL or all set');
END;
CREATE TRIGGER change_attempts_role_profile_provenance_all_or_none_update
BEFORE UPDATE OF role_profile_id, role_profile_revision, role_profile_definition_digest
ON change_attempts
WHEN (NEW.role_profile_id IS NULL) != (NEW.role_profile_revision IS NULL)
  OR (NEW.role_profile_id IS NULL) != (NEW.role_profile_definition_digest IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'change-attempt role-profile provenance must be all NULL or all set');
END;
CREATE TRIGGER change_requests_role_profile_provenance_all_or_none_insert
BEFORE INSERT ON change_requests
WHEN (NEW.role_profile_id IS NULL) != (NEW.role_profile_revision IS NULL)
  OR (NEW.role_profile_id IS NULL) != (NEW.role_profile_definition_digest IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'change-request role-profile provenance must be all NULL or all set');
END;
CREATE TRIGGER change_requests_role_profile_provenance_all_or_none_update
BEFORE UPDATE OF role_profile_id, role_profile_revision, role_profile_definition_digest
ON change_requests
WHEN (NEW.role_profile_id IS NULL) != (NEW.role_profile_revision IS NULL)
  OR (NEW.role_profile_id IS NULL) != (NEW.role_profile_definition_digest IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'change-request role-profile provenance must be all NULL or all set');
END;
CREATE TRIGGER change_requests_role_profile_provenance_immutable
BEFORE UPDATE OF role_profile_id, role_profile_revision, role_profile_definition_digest
ON change_requests
WHEN NEW.role_profile_id IS NOT OLD.role_profile_id
  OR NEW.role_profile_revision IS NOT OLD.role_profile_revision
  OR NEW.role_profile_definition_digest IS NOT OLD.role_profile_definition_digest
BEGIN
    SELECT RAISE(ABORT, 'change-request role-profile provenance is immutable');
END;
"#;

const MIGRATION_V16: &str = r#"
CREATE TABLE change_lifecycles (
    lifecycle_id TEXT NOT NULL UNIQUE CHECK(length(lifecycle_id) = 67),
    change_id TEXT NOT NULL PRIMARY KEY REFERENCES change_requests(change_id),
    source_snapshot_digest TEXT NOT NULL CHECK(length(source_snapshot_digest) = 67),
    state_digest TEXT NOT NULL CHECK(length(state_digest) = 67),
    generation INTEGER NOT NULL CHECK(generation >= 0),
    state_json TEXT NOT NULL CHECK(length(state_json) <= 65536),
    created_unix_ms INTEGER NOT NULL CHECK(created_unix_ms >= 0),
    updated_unix_ms INTEGER NOT NULL CHECK(updated_unix_ms >= created_unix_ms)
);
ALTER TABLE change_events ADD COLUMN lifecycle_sequence INTEGER;
CREATE UNIQUE INDEX change_events_lifecycle_sequence
    ON change_events(change_id, lifecycle_sequence)
    WHERE lifecycle_sequence IS NOT NULL;
CREATE TRIGGER change_lifecycles_transition_shape
BEFORE UPDATE ON change_lifecycles
WHEN NEW.lifecycle_id <> OLD.lifecycle_id
  OR NEW.change_id <> OLD.change_id
  OR NEW.source_snapshot_digest <> OLD.source_snapshot_digest
  OR NEW.created_unix_ms <> OLD.created_unix_ms
  OR NEW.generation <> OLD.generation + 1
  OR NEW.updated_unix_ms < OLD.updated_unix_ms
BEGIN
    SELECT RAISE(ABORT, 'invalid lifecycle projection transition');
END;
CREATE TRIGGER change_lifecycles_no_delete
BEFORE DELETE ON change_lifecycles
BEGIN
    SELECT RAISE(ABORT, 'lifecycle projections are durable');
END;
CREATE TRIGGER change_events_no_update
BEFORE UPDATE ON change_events
BEGIN
    SELECT RAISE(ABORT, 'change events are append-only');
END;
CREATE TRIGGER change_events_no_delete
BEFORE DELETE ON change_events
BEGIN
    SELECT RAISE(ABORT, 'change events are append-only');
END;
CREATE TRIGGER lifecycle_event_payload_bound
BEFORE INSERT ON change_events
WHEN NEW.lifecycle_sequence IS NOT NULL AND (
    length(NEW.event_type) NOT BETWEEN 1 AND 64
    OR length(NEW.payload_digest) != 67
    OR length(NEW.payload_json) > 65536
)
BEGIN
    SELECT RAISE(ABORT, 'lifecycle event exceeds persisted bounds');
END;
"#;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database operation failed: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("configuration serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("TOML serialization failed: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("TOML parsing failed: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("configuration I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("database migration checksum mismatch")]
    MigrationChecksum,
    #[error("required setting `{0}` is missing")]
    MissingSetting(&'static str),
    #[error("invalid evidence failure policy `{0}`")]
    EvidenceFailurePolicy(String),
    #[error("invalid stored digest: {0}")]
    Digest(String),
    #[error("configuration definition digest mismatch for `{0}`")]
    DefinitionDigest(String),
    #[error("artifact identity mismatch: {0}")]
    ArtifactIdentity(String),
    #[error("approval `{0}` is already resolved or its payload changed")]
    ApprovalConflict(String),
    #[error("approval `{0}` has expired")]
    ApprovalExpired(String),
    #[error("need-step request is invalid: {0}")]
    NeedStepRequest(String),
    #[error("operator cost observation is invalid: {0}")]
    OperatorCostObservation(String),
    #[error("change `{0}` conflicts with an existing immutable request")]
    ChangeConflict(String),
    #[error("patch artifact is invalid: {0}")]
    PatchArtifact(String),
    #[error("role profile validation failed: {0}")]
    RoleProfileValidation(String),
    #[error("role profile storage is corrupt: {0}")]
    RoleProfileCorruption(String),
    #[error("role profile operation conflicts: {0}")]
    RoleProfileConflict(String),
    #[error("role profile was not found: {0}")]
    RoleProfileNotFound(String),
    #[error("lifecycle validation failed: {0}")]
    Lifecycle(#[from] needle_core::LifecycleError),
    #[error("lifecycle operation conflicts: {0}")]
    LifecycleConflict(String),
    #[error("lifecycle was not found: {0}")]
    LifecycleNotFound(String),
    #[error("stored lifecycle is corrupt: {0}")]
    LifecycleCorruption(String),
    #[error("database connection lock was poisoned")]
    ConnectionLock,
}

#[derive(Clone)]
pub struct RuntimeStore {
    path: PathBuf,
    connection: Arc<Mutex<Option<Connection>>>,
    semantic_cache: Arc<Mutex<SemanticObjectCache>>,
}

#[derive(Default)]
struct SemanticObjectCache {
    values: BTreeMap<String, (Artifact, ArtifactValidationCertificate, Digest)>,
    insertion_order: VecDeque<String>,
}

impl SemanticObjectCache {
    const CAPACITY: usize = 256;

    fn get(&self, id: &str) -> Option<(Artifact, ArtifactValidationCertificate, Digest)> {
        self.values.get(id).cloned()
    }

    fn insert(&mut self, id: String, value: (Artifact, ArtifactValidationCertificate, Digest)) {
        if let Some(existing) = self.values.get_mut(&id) {
            *existing = value;
            return;
        }
        while self.values.len() >= Self::CAPACITY {
            if let Some(evicted) = self.insertion_order.pop_front() {
                self.values.remove(&evicted);
            }
        }
        self.insertion_order.push_back(id.clone());
        self.values.insert(id, value);
    }
}

impl fmt::Debug for RuntimeStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("RuntimeStore").field("path", &self.path).finish()
    }
}

struct ConnectionGuard<'a>(MutexGuard<'a, Option<Connection>>);

impl Deref for ConnectionGuard<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("connection is initialized")
    }
}

impl DerefMut for ConnectionGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("connection is initialized")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRecord {
    pub session_id: String,
    pub turn_id: Option<String>,
    pub root_task: Option<String>,
    pub prompt_profile_digest: Digest,
    pub need_grammar_digest: Digest,
    pub transport: Option<String>,
    pub transport_definition_digest: Option<Digest>,
    pub semantic_definition_digest: Option<Digest>,
    pub route_set_digest: Digest,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub route_set: Vec<Route>,
    pub multi_need_policy: MultiNeedPolicy,
    pub multi_need_policy_digest: Digest,
    pub role_profile_provenance: Option<RoleProfileProvenance>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MainTurnObservationRecord {
    pub session_id: String,
    pub turn_id: String,
    pub need_step_id: Option<Digest>,
    pub status: String,
    pub delivery: Option<String>,
    pub usage_json: String,
    pub tools_json: String,
    pub main_discovery_tainted: bool,
    pub outcome: Option<MainTurnOutcome>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NeedStepEventRecord {
    pub event_id: u64,
    pub session_id: String,
    pub need_step_id: Digest,
    pub state: NeedStepState,
    pub event: serde_json::Value,
    pub created_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NeedStepRequestRecord {
    pub need_step_id: Digest,
    pub session_id: String,
    pub request_digest: Digest,
    pub raw_message: String,
    pub semantic_interrupt: Option<SemanticInterrupt>,
    pub need_ir: Option<needle_core::NeedIr>,
    pub transport: Option<String>,
    pub request_format: Option<String>,
    pub created_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSettings {
    pub codex_executable: String,
    pub worker_model: String,
    pub worker_reasoning: String,
    pub worker_timeout_seconds: u64,
    #[serde(default)]
    pub evidence_failure_policy: EvidenceFailurePolicy,
    #[serde(default)]
    pub trusted_test_execution: bool,
    #[serde(default)]
    pub multi_need_policy: MultiNeedPolicy,
}

impl RuntimeSettings {
    pub fn worker_config(&self) -> WorkerConfig {
        WorkerConfig {
            executable: self.codex_executable.clone(),
            model: self.worker_model.clone(),
            reasoning: self.worker_reasoning.clone(),
            service_tier: None,
            timeout_seconds: self.worker_timeout_seconds,
            evidence_failure_policy: self.evidence_failure_policy,
            role_profile_provenance: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConfigExport {
    pub format_revision: u32,
    pub settings: RuntimeSettings,
    pub presets: Vec<Preset>,
    pub routes: Vec<Route>,
    #[serde(default)]
    pub model_policy: Option<ModelPolicy>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheRecord {
    pub identity_digest: Digest,
    pub logical_digest: Digest,
    pub source_digest: Digest,
    pub created_unix_ms: u64,
    pub hit_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerRunRecord {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub result_digest: Option<Digest>,
    pub failure_code: Option<String>,
    pub failure_diagnostic: Option<String>,
    pub discarded_facts: u32,
    pub logical_worker_spawns: u32,
    pub worker_turns: u32,
    pub repair_performed: bool,
    pub worker_session_id: Option<String>,
    pub session_cleanup_success: Option<bool>,
    pub role_profile_provenance: Option<RoleProfileProvenance>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RoutePromotionRecord {
    pub route_key: String,
    pub worker_profile_digest: Digest,
    pub evidence_digest: Digest,
    pub promoted_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegativeAttemptRecord {
    pub attempt_identity: Digest,
    pub failure_code: String,
    pub failure_json: String,
    pub expires_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct NeedShadowRecord {
    pub need_ir: NeedIr,
    pub need: Need,
    pub fragments: Vec<NeedFragment>,
    pub parser_definition_digest: Digest,
    pub prompt_profile_digest: Digest,
    pub created_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCostObservation {
    pub route_key: String,
    pub cost_microusd: u64,
    pub source: String,
    pub evidence_digest: Digest,
    pub observed_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorCostObservation {
    pub artifact_kind: String,
    pub worker_model: String,
    pub worker_reasoning: String,
    pub service_tier: String,
    pub schema_digest: Digest,
    pub validator_definition_digest: Digest,
    pub pricing_digest: Digest,
    pub requested_kind_count: u8,
    pub cost_microusd: u64,
    pub execution_attempt_id: Option<Digest>,
    pub evidence_digest: Digest,
    pub observed_unix_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperatorCostKey<'a> {
    pub artifact_kind: &'a str,
    pub worker_model: &'a str,
    pub worker_reasoning: &'a str,
    pub service_tier: &'a str,
    pub schema_digest: Digest,
    pub validator_definition_digest: Digest,
    pub pricing_digest: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofAccountingRecord {
    pub need_id: needle_core::NeedId,
    pub plan_id: Option<needle_core::SelectedPlanId>,
    pub parse_micros: u64,
    pub lookup_micros: u64,
    pub validation_micros: u64,
    pub planning_micros: u64,
    pub projection_micros: u64,
    pub allocation_count: Option<u64>,
    pub allocated_bytes: Option<u64>,
    pub stale_candidates: u64,
    pub created_unix_ms: u64,
}

impl RuntimeStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            connection: Arc::new(Mutex::new(None)),
            semantic_cache: Arc::new(Mutex::new(SemanticObjectCache::default())),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn initialize(&self) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut connection = self.connection()?;
        connection.execute_batch(MIGRATION_V1)?;
        let checksum = Digest::blake3(MIGRATION_V1).to_string();
        let existing: Option<String> = connection
            .query_row("SELECT checksum FROM schema_migrations WHERE version = 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        if existing.as_deref().is_some_and(|value| value != checksum) {
            return Err(StoreError::MigrationChecksum);
        }
        connection.execute(
            "INSERT OR IGNORE INTO schema_migrations(version, checksum, applied_unix_ms) VALUES(1, ?1, ?2)",
            params![checksum, now_ms()],
        )?;
        let v2_checksum = Digest::blake3(MIGRATION_V2).to_string();
        let existing_v2: Option<String> = connection
            .query_row("SELECT checksum FROM schema_migrations WHERE version = 2", [], |row| {
                row.get(0)
            })
            .optional()?;
        if existing_v2.as_deref().is_some_and(|value| value != v2_checksum) {
            return Err(StoreError::MigrationChecksum);
        }
        if existing_v2.is_none() {
            let transaction = connection.transaction()?;
            transaction.execute_batch(MIGRATION_V2)?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, checksum, applied_unix_ms) VALUES(2, ?1, ?2)",
                params![v2_checksum, now_ms()],
            )?;
            transaction.commit()?;
        }
        apply_migration(&mut connection, 3, MIGRATION_V3)?;
        apply_migration(&mut connection, 4, MIGRATION_V4)?;
        apply_migration(&mut connection, 5, MIGRATION_V5)?;
        apply_migration(&mut connection, 6, MIGRATION_V6)?;
        apply_migration(&mut connection, 7, MIGRATION_V7)?;
        apply_migration(&mut connection, 8, MIGRATION_V8)?;
        apply_migration(&mut connection, 9, MIGRATION_V9)?;
        apply_migration(&mut connection, 10, MIGRATION_V10)?;
        apply_migration(&mut connection, 11, MIGRATION_V11)?;
        apply_migration(&mut connection, 12, MIGRATION_V12)?;
        apply_migration(&mut connection, 13, MIGRATION_V13)?;
        apply_migration(&mut connection, 14, MIGRATION_V14)?;
        apply_migration(&mut connection, 15, MIGRATION_V15)?;
        apply_migration(&mut connection, 16, MIGRATION_V16)?;
        connection.execute(
            "INSERT OR IGNORE INTO settings(key, value) VALUES('utility_gate_passed', '0')",
            [],
        )?;
        connection.execute(
            "INSERT OR IGNORE INTO settings(key, value) VALUES('evidence_failure_policy', 'discard_invalid_fact')",
            [],
        )?;
        connection.execute(
            "INSERT OR IGNORE INTO settings(key, value) VALUES('trusted_test_execution', '0')",
            [],
        )?;
        connection.execute(
            "INSERT OR IGNORE INTO settings(key, value) VALUES('multi_need_policy', ?1)",
            [serde_json::to_string(&MultiNeedPolicy::default())?],
        )?;
        Ok(())
    }

    pub fn initialize_defaults(&self, settings: &RuntimeSettings) -> Result<(), StoreError> {
        self.initialize()?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        for (key, value) in [
            ("codex_executable", settings.codex_executable.as_str()),
            ("worker_model", settings.worker_model.as_str()),
            ("worker_reasoning", settings.worker_reasoning.as_str()),
            ("evidence_failure_policy", settings.evidence_failure_policy.as_str()),
        ] {
            transaction.execute(
                "INSERT INTO settings(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            )?;
        }
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES('worker_timeout_seconds', ?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![settings.worker_timeout_seconds.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES('trusted_test_execution', ?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![if settings.trusted_test_execution { "1" } else { "0" }],
        )?;
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES('multi_need_policy', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [serde_json::to_string(&settings.multi_need_policy)?],
        )?;
        let default_policy = ModelPolicy::FixedOrder {
            profiles: vec![WorkerProfile::new(
                "codex",
                settings.worker_model.clone(),
                settings.worker_reasoning.clone(),
                None,
            )],
            repair_once: true,
            native_fallback: true,
        };
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES('model_policy', ?1)
             ON CONFLICT(key) DO NOTHING",
            [serde_json::to_string(&default_policy)?],
        )?;
        for preset in built_in_presets() {
            transaction.execute(
                "INSERT INTO presets(id, definition_digest, definition_json) VALUES(?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET definition_digest=excluded.definition_digest, definition_json=excluded.definition_json",
                params![preset.id, preset.definition_digest.to_string(), serde_json::to_string(&preset)?],
            )?;
        }
        for route in built_in_routes() {
            transaction.execute(
                "INSERT INTO routes(id, enabled, priority, definition_digest, definition_json) VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET enabled=excluded.enabled, priority=excluded.priority, definition_digest=excluded.definition_digest, definition_json=excluded.definition_json",
                params![route.id, route.enabled, route.priority, route.definition_digest.to_string(), serde_json::to_string(&route)?],
            )?;
        }
        for plan in built_in_route_plans() {
            let definition_json = serde_json::to_string(&plan)?;
            transaction.execute(
                "INSERT OR IGNORE INTO definitions(
                    definition_digest, kind, id, revision, definition_json, created_unix_ms
                 ) VALUES(?1, 'route_plan', ?2, ?3, ?4, ?5)",
                params![
                    plan.definition_digest.to_string(),
                    plan.id,
                    plan.revision,
                    definition_json,
                    now_ms(),
                ],
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO route_plans(definition_digest, route_key, enabled)
                 VALUES(?1, ?2, 1)",
                params![plan.definition_digest.to_string(), plan.route_key.as_str()],
            )?;
        }
        for contract in built_in_predicate_contracts() {
            transaction.execute(
                "INSERT OR IGNORE INTO predicate_contracts(
                    definition_digest, predicate, definition_json, created_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4)",
                params![
                    contract.definition_digest.to_string(),
                    format!("{:?}", contract.predicate),
                    serde_json::to_string(&contract)?,
                    now_ms(),
                ],
            )?;
        }
        for contract in built_in_route_contracts() {
            transaction.execute(
                "INSERT OR IGNORE INTO route_contracts(
                    definition_digest, route_key, definition_json, created_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4)",
                params![
                    contract.definition_digest.to_string(),
                    contract.route.as_str(),
                    serde_json::to_string(&contract)?,
                    now_ms(),
                ],
            )?;
        }
        for class in
            built_in_capability_classes().into_iter().chain(built_in_claim_capability_classes())
        {
            transaction.execute(
                "INSERT INTO capability_classes(
                    id, predicate, mode, definition_digest, definition_json, updated_unix_ms
                 ) VALUES(?1, ?2, 'shadow', ?3, ?4, ?5)
                 ON CONFLICT(id) DO NOTHING",
                params![
                    class.id,
                    format!("{:?}", class.predicate),
                    class.definition_digest.to_string(),
                    serde_json::to_string(&class)?,
                    now_ms(),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn record_need_shadow(&self, record: NeedShadowWrite<'_>) -> Result<(), StoreError> {
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let created = now_ms();
        transaction.execute(
            "INSERT OR IGNORE INTO needs(
                need_id, route_key, world_digest, need_json, created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                record.need.id.to_string(),
                record
                    .need_ir
                    .route_hint
                    .as_ref()
                    .map(needle_core::NeedKey::as_str)
                    .unwrap_or_default(),
                record.need.world.id().to_string(),
                serde_json::to_string(record.need)?,
                created,
            ],
        )?;
        for subject in &record.need.subjects {
            transaction.execute(
                "INSERT OR IGNORE INTO subjects(
                    subject_id, repository_lineage, kind, canonical_name, subject_json,
                    created_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    subject.id.to_string(),
                    subject.repository_lineage.to_string(),
                    format!("{:?}", subject.kind),
                    subject.canonical_name,
                    serde_json::to_string(subject)?,
                    created,
                ],
            )?;
        }
        for (required, obligations) in
            [(true, &record.need.required), (false, &record.need.preferred)]
        {
            for obligation in obligations {
                transaction.execute(
                    "INSERT OR IGNORE INTO need_obligations(
                        need_id, obligation_id, predicate, required, obligation_json
                     ) VALUES(?1, ?2, ?3, ?4, ?5)",
                    params![
                        record.need.id.to_string(),
                        obligation.id.to_string(),
                        format!("{:?}", obligation.predicate),
                        required,
                        serde_json::to_string(obligation)?,
                    ],
                )?;
            }
        }
        if let Some(residual) = &record.need.residual {
            transaction.execute(
                "INSERT OR REPLACE INTO residual_intents(need_id, residual_json) VALUES(?1, ?2)",
                params![record.need.id.to_string(), serde_json::to_string(residual)?],
            )?;
        }
        for fragment in record.fragments {
            transaction.execute(
                "INSERT OR IGNORE INTO need_fragments(
                    fragment_id, need_id, fragment_json, created_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4)",
                params![
                    fragment.id.to_string(),
                    record.need.id.to_string(),
                    serde_json::to_string(fragment)?,
                    created,
                ],
            )?;
        }
        transaction.execute(
            "INSERT OR IGNORE INTO need_ir_records(
                session_id, turn_id, transport_digest, format_revision,
                parser_definition_digest, prompt_profile_digest, route_key, need_id, ir_json,
                created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.session_id,
                record.turn_id,
                record.transport_digest.to_string(),
                record.need_ir.format_revision,
                record.parser_definition_digest.to_string(),
                record.prompt_profile_digest.to_string(),
                record
                    .need_ir
                    .route_hint
                    .as_ref()
                    .map(needle_core::NeedKey::as_str)
                    .unwrap_or_default(),
                record.need.id.to_string(),
                serde_json::to_string(record.need_ir)?,
                created,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn need_shadows(&self, limit: u32) -> Result<Vec<NeedShadowRecord>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare_cached(
            "SELECT r.ir_json, n.need_json, r.parser_definition_digest,
                    r.prompt_profile_digest, r.created_unix_ms
             FROM need_ir_records r
             JOIN needs n ON n.need_id=r.need_id
             ORDER BY r.created_unix_ms DESC, r.record_id DESC
             LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.min(500)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u64>(4)?,
            ))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (ir_json, need_json, parser, profile, created_unix_ms) = row?;
            let need: Need = serde_json::from_str(&need_json)?;
            let mut fragment_statement = connection.prepare_cached(
                "SELECT fragment_json FROM need_fragments
                 WHERE need_id=?1 ORDER BY fragment_id",
            )?;
            let fragments = fragment_statement
                .query_map([need.id.to_string()], |row| row.get::<_, String>(0))?
                .map(|value| {
                    value
                        .map_err(StoreError::from)
                        .and_then(|json| serde_json::from_str(&json).map_err(StoreError::from))
                })
                .collect::<Result<Vec<_>, _>>()?;
            records.push(NeedShadowRecord {
                need_ir: serde_json::from_str(&ir_json)?,
                need,
                fragments,
                parser_definition_digest: parse_digest(&parser)?,
                prompt_profile_digest: parse_digest(&profile)?,
                created_unix_ms,
            });
        }
        Ok(records)
    }

    pub fn needs(&self, limit: u32) -> Result<Vec<Need>, StoreError> {
        self.json_rows(&format!(
            "SELECT need_json FROM needs ORDER BY created_unix_ms DESC LIMIT {}",
            limit.min(500)
        ))
    }

    pub fn need(&self, id: &str) -> Result<Option<Need>, StoreError> {
        let connection = self.connection()?;
        let json = connection
            .query_row("SELECT need_json FROM needs WHERE need_id=?1", [id], |row| row.get(0))
            .optional()?;
        json.map(|value: String| serde_json::from_str(&value).map_err(StoreError::from)).transpose()
    }

    pub fn subjects(&self, limit: u32) -> Result<Vec<needle_core::Subject>, StoreError> {
        self.json_rows(&format!(
            "SELECT subject_json FROM subjects ORDER BY created_unix_ms DESC LIMIT {}",
            limit.min(500)
        ))
    }

    pub fn subject(
        &self,
        id: needle_core::SubjectId,
    ) -> Result<Option<needle_core::Subject>, StoreError> {
        let connection = self.connection()?;
        let json = connection
            .query_row(
                "SELECT subject_json FROM subjects WHERE subject_id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|value: String| serde_json::from_str(&value).map_err(StoreError::from)).transpose()
    }

    pub fn selected_plan(&self, id: &str) -> Result<Option<SelectedPlan>, StoreError> {
        let connection = self.connection()?;
        let json = connection
            .query_row("SELECT plan_json FROM selected_plans WHERE plan_id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        json.map(|value: String| serde_json::from_str(&value).map_err(StoreError::from)).transpose()
    }

    pub fn selected_plans(&self, limit: u32) -> Result<Vec<SelectedPlan>, StoreError> {
        self.json_rows(&format!(
            "SELECT plan_json FROM selected_plans ORDER BY created_unix_ms DESC LIMIT {}",
            limit.min(500)
        ))
    }

    pub fn proof_certificate(
        &self,
        id: &str,
    ) -> Result<Option<ReuseSufficiencyCertificate>, StoreError> {
        let connection = self.connection()?;
        let json = connection
            .query_row(
                "SELECT certificate_json FROM reuse_sufficiency_certificates
                 WHERE certificate_id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|value: String| serde_json::from_str(&value).map_err(StoreError::from)).transpose()
    }

    pub fn proof_certificates(
        &self,
        limit: u32,
    ) -> Result<Vec<ReuseSufficiencyCertificate>, StoreError> {
        self.json_rows(&format!(
            "SELECT certificate_json FROM reuse_sufficiency_certificates
             ORDER BY created_unix_ms DESC LIMIT {}",
            limit.min(500)
        ))
    }

    pub fn validation_certificate(
        &self,
        id: &str,
    ) -> Result<Option<ArtifactValidationCertificate>, StoreError> {
        let connection = self.connection()?;
        let json = connection
            .query_row(
                "SELECT certificate_json FROM artifact_validation_certificates
                 WHERE certificate_id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|value: String| serde_json::from_str(&value).map_err(StoreError::from)).transpose()
    }

    pub fn validation_certificate_for_artifact(
        &self,
        artifact_id: &str,
    ) -> Result<Option<ArtifactValidationCertificate>, StoreError> {
        let connection = self.connection()?;
        let json = connection
            .query_row(
                "SELECT certificate_json FROM artifact_validation_certificates
                 WHERE artifact_id=?1 ORDER BY issued_unix_ms DESC LIMIT 1",
                [artifact_id],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|value: String| serde_json::from_str(&value).map_err(StoreError::from)).transpose()
    }

    pub fn covered_obligations_for_artifact(
        &self,
        artifact_id: needle_core::ArtifactId,
        need: &Need,
    ) -> Result<Vec<needle_core::ObligationId>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare_cached(
            "SELECT obligation_json FROM coverage_entries
             WHERE artifact_id=?1 AND world_digest=?2 ORDER BY obligation_id",
        )?;
        let rows = statement
            .query_map(params![artifact_id.to_string(), need.world.id().to_string()], |row| {
                row.get::<_, String>(0)
            })?;
        let mut covered = Vec::new();
        for row in rows {
            let provided: needle_core::Obligation = serde_json::from_str(&row?)?;
            for requested in &need.required {
                if provided.satisfies(requested) {
                    covered.push(requested.id);
                }
            }
        }
        covered.sort();
        covered.dedup();
        Ok(covered)
    }

    pub fn semantic_artifact(&self, id: &str) -> Result<Option<Artifact>, StoreError> {
        let connection = self.connection()?;
        let json = connection
            .query_row("SELECT artifact_json FROM artifacts WHERE artifact_id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        json.map(|value: String| serde_json::from_str(&value).map_err(StoreError::from)).transpose()
    }

    pub fn semantic_artifact_source_digest(
        &self,
        id: needle_core::ArtifactId,
    ) -> Result<Option<Digest>, StoreError> {
        let connection = self.connection()?;
        let digest: Option<String> = connection
            .query_row(
                "SELECT r.source_digest FROM artifacts a
                 JOIN artifact_requests r ON r.request_id=a.request_id
                 WHERE a.artifact_id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        digest.map(|value| Digest::parse(&value).map_err(|_| StoreError::Digest(value))).transpose()
    }

    pub fn repository_root_for_need(&self, id: &str) -> Result<Option<PathBuf>, StoreError> {
        let connection = self.connection()?;
        let cwd = connection
            .query_row(
                "SELECT s.cwd FROM need_ir_records n
                 JOIN sessions s ON s.session_id=n.session_id
                 WHERE n.need_id=?1 AND s.cwd IS NOT NULL
                 ORDER BY n.created_unix_ms DESC LIMIT 1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(cwd.map(PathBuf::from))
    }

    pub fn capability_classes(&self) -> Result<Vec<CapabilityClass>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare_cached("SELECT mode, definition_json FROM capability_classes ORDER BY id")?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (mode, json) = row?;
            let mut class: CapabilityClass = serde_json::from_str(&json)?;
            class.mode = parse_capability_mode(&mode)?;
            Ok(class)
        })
        .collect()
    }

    pub fn set_capability_mode(
        &self,
        id: &str,
        expected_definition: Digest,
        mode: CapabilityMode,
        evidence_digest: Option<Digest>,
    ) -> Result<Option<CapabilityClass>, StoreError> {
        if matches!(mode, CapabilityMode::Advisory | CapabilityMode::Authoritative)
            && evidence_digest.is_none()
        {
            return Err(StoreError::DefinitionDigest(format!(
                "capability `{id}` requires evidence"
            )));
        }
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let stored: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT definition_digest, definition_json, mode
                 FROM capability_classes WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((definition, json, previous_mode)) = stored else {
            return Ok(None);
        };
        if definition != expected_definition.to_string() {
            return Err(StoreError::DefinitionDigest(format!("capability `{id}` changed")));
        }
        transaction.execute(
            "UPDATE capability_classes
             SET mode=?1, evidence_digest=?2, updated_unix_ms=?3
             WHERE id=?4 AND definition_digest=?5",
            params![
                capability_mode_name(mode),
                evidence_digest.map(|value| value.to_string()),
                now_ms(),
                id,
                definition,
            ],
        )?;
        transaction.execute(
            "INSERT INTO capability_mode_audit(
                capability_id, previous_mode, new_mode, definition_digest, evidence_digest,
                changed_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                previous_mode,
                capability_mode_name(mode),
                definition,
                evidence_digest.map(|value| value.to_string()),
                now_ms(),
            ],
        )?;
        transaction.commit()?;
        let mut class: CapabilityClass = serde_json::from_str(&json)?;
        class.mode = mode;
        Ok(Some(class))
    }

    pub fn settings(&self) -> Result<RuntimeSettings, StoreError> {
        let connection = self.connection()?;
        let get = |key: &'static str| -> Result<String, StoreError> {
            connection
                .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| row.get(0))
                .optional()?
                .ok_or(StoreError::MissingSetting(key))
        };
        Ok(RuntimeSettings {
            codex_executable: get("codex_executable")?,
            worker_model: get("worker_model")?,
            worker_reasoning: get("worker_reasoning")?,
            worker_timeout_seconds: get("worker_timeout_seconds")?
                .parse()
                .map_err(|_| StoreError::MissingSetting("worker_timeout_seconds"))?,
            evidence_failure_policy: parse_evidence_failure_policy(&get(
                "evidence_failure_policy",
            )?)?,
            trusted_test_execution: get("trusted_test_execution")? == "1",
            multi_need_policy: serde_json::from_str(&get("multi_need_policy")?)?,
        })
    }

    pub fn set_runtime_settings(&self, settings: &RuntimeSettings) -> Result<(), StoreError> {
        let safe_token = |value: &str, maximum: usize| {
            !value.is_empty()
                && value.len() <= maximum
                && value
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || ".-_".contains(character))
        };
        if !safe_token(&settings.worker_model, 128)
            || !safe_token(&settings.worker_reasoning, 32)
            || !(1..=3_600).contains(&settings.worker_timeout_seconds)
            || !settings.multi_need_policy.validate()
        {
            return Err(StoreError::DefinitionDigest("runtime_settings".to_owned()));
        }
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (key, value) in [
            ("worker_model", settings.worker_model.as_str()),
            ("worker_reasoning", settings.worker_reasoning.as_str()),
            ("evidence_failure_policy", settings.evidence_failure_policy.as_str()),
        ] {
            transaction.execute(
                "INSERT INTO settings(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            )?;
        }
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES('worker_timeout_seconds', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [settings.worker_timeout_seconds.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES('trusted_test_execution', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [if settings.trusted_test_execution { "1" } else { "0" }],
        )?;
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES('multi_need_policy', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [serde_json::to_string(&settings.multi_need_policy)?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn utility_gate_passed(&self) -> Result<bool, StoreError> {
        let connection = self.connection()?;
        let value: Option<String> = connection
            .query_row("SELECT value FROM settings WHERE key='utility_gate_passed'", [], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(value.as_deref() == Some("1"))
    }

    pub fn model_policy(&self) -> Result<ModelPolicy, StoreError> {
        let connection = self.connection()?;
        let value: Option<String> = connection
            .query_row("SELECT value FROM settings WHERE key='model_policy'", [], |row| row.get(0))
            .optional()?;
        if let Some(value) = value {
            return Ok(serde_json::from_str(&value)?);
        }
        drop(connection);
        let settings = self.settings()?;
        Ok(ModelPolicy::FixedOrder {
            profiles: vec![WorkerProfile::new(
                "codex",
                settings.worker_model,
                settings.worker_reasoning,
                None,
            )],
            repair_once: true,
            native_fallback: true,
        })
    }

    pub fn set_model_policy(&self, policy: &ModelPolicy) -> Result<(), StoreError> {
        let profiles = match policy {
            ModelPolicy::FixedOrder { profiles, .. } => profiles,
            ModelPolicy::CheapestValidatedFirst { promoted_profiles, .. } => promoted_profiles,
        };
        let safe_token = |value: &str, maximum: usize| {
            !value.is_empty()
                && value.len() <= maximum
                && value
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || ".-_".contains(character))
        };
        if profiles.is_empty()
            || profiles.iter().any(|profile| {
                profile.platform != "codex"
                    || !safe_token(&profile.model, 128)
                    || !safe_token(&profile.reasoning, 32)
                    || profile.service_tier.as_deref().is_some_and(|tier| !safe_token(tier, 32))
            })
        {
            return Err(StoreError::DefinitionDigest("model_policy".to_owned()));
        }
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO settings(key, value) VALUES('model_policy', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [serde_json::to_string(policy)?],
        )?;
        Ok(())
    }

    pub fn promoted_profile_digests(
        &self,
        route_key: &str,
    ) -> Result<std::collections::BTreeSet<Digest>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT worker_profile_digest FROM route_promotions WHERE route_key=?1
             ORDER BY worker_profile_digest",
        )?;
        let rows = statement.query_map([route_key], |row| row.get::<_, String>(0))?;
        rows.map(|row| parse_digest(&row?)).collect()
    }

    pub fn promote_route_profile(
        &self,
        route_key: &str,
        profile: &WorkerProfile,
        evidence_digest: Digest,
    ) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO route_promotions(
                route_key, worker_profile_digest, evidence_digest, promoted_unix_ms
             ) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(route_key, worker_profile_digest) DO UPDATE SET
                evidence_digest=excluded.evidence_digest,
                promoted_unix_ms=excluded.promoted_unix_ms",
            params![
                route_key,
                profile.definition_digest.to_string(),
                evidence_digest.to_string(),
                now_ms(),
            ],
        )?;
        Ok(())
    }

    pub fn route_promotions(&self) -> Result<Vec<RoutePromotionRecord>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT route_key, worker_profile_digest, evidence_digest, promoted_unix_ms
             FROM route_promotions ORDER BY route_key, worker_profile_digest",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u64>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (route_key, profile, evidence, promoted_unix_ms) = row?;
            Ok(RoutePromotionRecord {
                route_key,
                worker_profile_digest: parse_digest(&profile)?,
                evidence_digest: parse_digest(&evidence)?,
                promoted_unix_ms,
            })
        })
        .collect()
    }

    pub fn negative_attempt(
        &self,
        attempt_identity: Digest,
    ) -> Result<Option<NegativeAttemptRecord>, StoreError> {
        let connection = self.connection()?;
        connection
            .execute("DELETE FROM negative_attempts WHERE expires_unix_ms <= ?1", [now_ms()])?;
        connection
            .query_row(
                "SELECT failure_code, failure_json, expires_unix_ms
                 FROM negative_attempts WHERE attempt_identity=?1",
                [attempt_identity.to_string()],
                |row| {
                    Ok(NegativeAttemptRecord {
                        attempt_identity,
                        failure_code: row.get(0)?,
                        failure_json: row.get(1)?,
                        expires_unix_ms: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn record_negative_attempt(
        &self,
        attempt_identity: Digest,
        failure_code: &str,
        failure_json: &str,
        expires_unix_ms: u64,
    ) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO negative_attempts(
                attempt_identity, failure_code, failure_json, expires_unix_ms
             ) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(attempt_identity) DO UPDATE SET
                failure_code=excluded.failure_code,
                failure_json=excluded.failure_json,
                expires_unix_ms=excluded.expires_unix_ms",
            params![attempt_identity.to_string(), failure_code, failure_json, expires_unix_ms],
        )?;
        Ok(())
    }

    pub fn mark_utility_gate_passed(&self) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO settings(key, value) VALUES('utility_gate_passed', '1')
             ON CONFLICT(key) DO UPDATE SET value='1'",
            [],
        )?;
        Ok(())
    }

    pub fn routes(&self) -> Result<Vec<Route>, StoreError> {
        self.json_rows("SELECT definition_json FROM routes ORDER BY id")
    }

    pub fn preset(&self, id: &str) -> Result<Option<Preset>, StoreError> {
        let connection = self.connection()?;
        let json: Option<String> = connection
            .query_row("SELECT definition_json FROM presets WHERE id = ?1", [id], |row| row.get(0))
            .optional()?;
        json.map(|value| serde_json::from_str(&value).map_err(StoreError::from)).transpose()
    }

    pub fn set_route_enabled(&self, id: &str, enabled: bool) -> Result<bool, StoreError> {
        let connection = self.connection()?;
        let json: Option<String> = connection
            .query_row("SELECT definition_json FROM routes WHERE id=?1", [id], |row| row.get(0))
            .optional()?;
        let Some(json) = json else {
            return Ok(false);
        };
        let mut route: Route = serde_json::from_str(&json)?;
        route.enabled = enabled;
        Ok(connection.execute(
            "UPDATE routes SET enabled=?2, definition_json=?3 WHERE id=?1",
            params![id, enabled, serde_json::to_string(&route)?],
        )? == 1)
    }

    pub fn record_session_start(
        &self,
        session_id: &str,
        prompt_profile_digest: Digest,
        model: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<(), StoreError> {
        self.record_session_start_for_transport(
            session_id,
            prompt_profile_digest,
            model,
            cwd,
            "hook",
            needle_core::need_grammar_definition_digest(),
            Some(needle_core::need_grammar_definition_digest()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_session_start_for_transport(
        &self,
        session_id: &str,
        prompt_profile_digest: Digest,
        model: Option<&str>,
        cwd: Option<&str>,
        transport: &str,
        transport_definition_digest: Digest,
        need_grammar_digest: Option<Digest>,
    ) -> Result<(), StoreError> {
        let route_set = self.routes()?;
        let route_set_digest = route_set_digest(&route_set);
        let multi_need_policy = self.multi_need_policy()?;
        let multi_need_policy_digest = multi_need_policy.digest();
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO sessions(
                session_id, prompt_profile_digest, route_set_digest, model, cwd, updated_unix_ms,
                route_set_json, need_grammar_digest, multi_need_policy_json,
                multi_need_policy_digest, transport, transport_definition_digest,
                semantic_definition_digest
             )
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(session_id) DO NOTHING",
            params![
                session_id,
                prompt_profile_digest.to_string(),
                route_set_digest.to_string(),
                model,
                cwd,
                now_ms(),
                serde_json::to_string(&route_set)?,
                need_grammar_digest.map(|digest| digest.to_string()),
                serde_json::to_string(&multi_need_policy)?,
                multi_need_policy_digest.to_string(),
                transport,
                transport_definition_digest.to_string(),
                needle_core::need_ir_definition_digest().to_string(),
            ],
        )?;
        Ok(())
    }

    /// Starts a production session bound to the currently active revision of
    /// an explicitly selected role profile. The active lookup and immutable
    /// session insert share one immediate transaction.
    pub fn record_session_start_profiled(
        &self,
        session_id: &str,
        prompt_profile_digest: Digest,
        model: Option<&str>,
        cwd: Option<&str>,
        profile_id: &RoleProfileId,
    ) -> Result<(), StoreError> {
        self.record_session_start_for_transport_profiled(
            session_id,
            prompt_profile_digest,
            model,
            cwd,
            "hook",
            needle_core::need_grammar_definition_digest(),
            Some(needle_core::need_grammar_definition_digest()),
            profile_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_session_start_for_transport_profiled(
        &self,
        session_id: &str,
        prompt_profile_digest: Digest,
        model: Option<&str>,
        cwd: Option<&str>,
        transport: &str,
        transport_definition_digest: Digest,
        need_grammar_digest: Option<Digest>,
        profile_id: &RoleProfileId,
    ) -> Result<(), StoreError> {
        self.initialize()?;
        let route_set = self.routes()?;
        let route_set_digest = route_set_digest(&route_set);
        let multi_need_policy = self.multi_need_policy()?;
        let multi_need_policy_digest = multi_need_policy.digest();
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        let active_revision: Option<u64> = transaction
            .query_row(
                "SELECT active_revision FROM role_profile_state WHERE profile_id=?1",
                [profile_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(revision) = active_revision else {
            return Err(StoreError::RoleProfileConflict(format!(
                "profile {profile_id} has no active revision"
            )));
        };
        let (definition_json, row_digest, created_unix_ms, activated_unix_ms): (
            String,
            String,
            u64,
            Option<u64>,
        ) = transaction.query_row(
            "SELECT definition_json, definition_digest, created_unix_ms, activated_unix_ms
             FROM role_profile_revisions WHERE profile_id=?1 AND revision=?2",
            params![profile_id.as_str(), revision],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let definition: needle_core::RoleProfileDefinition =
            serde_json::from_str(&definition_json)?;
        definition
            .validate()
            .map_err(|error| StoreError::RoleProfileCorruption(error.to_string()))?;
        let parsed_row_digest = Digest::parse(&row_digest)
            .map_err(|error| StoreError::RoleProfileCorruption(error.to_string()))?;
        if definition.profile_id != *profile_id
            || definition.definition_digest != parsed_row_digest
            || activated_unix_ms.is_none()
        {
            return Err(StoreError::RoleProfileCorruption(
                "active role-profile revision identity or activation metadata is invalid"
                    .to_owned(),
            ));
        }
        let revision_record = RoleProfileRevision {
            profile_id: profile_id.clone(),
            revision,
            definition,
            state: needle_core::RoleProfileState::Active,
            created_unix_ms,
            activated_unix_ms,
        };
        let provenance = RoleProfileProvenance::from_revision(&revision_record)
            .map_err(|error| StoreError::RoleProfileCorruption(error.to_string()))?;
        transaction.execute(
            "INSERT INTO sessions(
                session_id, prompt_profile_digest, route_set_digest, model, cwd, updated_unix_ms,
                route_set_json, need_grammar_digest, multi_need_policy_json,
                multi_need_policy_digest, transport, transport_definition_digest,
                semantic_definition_digest, role_profile_id, role_profile_revision,
                role_profile_definition_digest
             )
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
             ON CONFLICT(session_id) DO NOTHING",
            params![
                session_id,
                prompt_profile_digest.to_string(),
                route_set_digest.to_string(),
                model,
                cwd,
                now_ms(),
                serde_json::to_string(&route_set)?,
                need_grammar_digest.map(|digest| digest.to_string()),
                serde_json::to_string(&multi_need_policy)?,
                multi_need_policy_digest.to_string(),
                transport,
                transport_definition_digest.to_string(),
                needle_core::need_ir_definition_digest().to_string(),
                provenance.profile_id.as_str(),
                provenance.revision,
                provenance.definition_digest.to_string(),
            ],
        )?;
        let existing: (Option<String>, Option<u64>, Option<String>) = transaction.query_row(
            "SELECT role_profile_id, role_profile_revision, role_profile_definition_digest
             FROM sessions WHERE session_id=?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let existing = parse_role_profile_provenance(existing)?;
        if existing.as_ref() != Some(&provenance) {
            return Err(StoreError::RoleProfileConflict(format!(
                "session {session_id} is already bound to a different role-profile revision"
            )));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Explicitly named legacy insertion helper retained for migration and
    /// backward-compatibility tests. Production entry points must use one of
    /// the profiled methods above.
    pub fn record_legacy_session_start(
        &self,
        session_id: &str,
        prompt_profile_digest: Digest,
        model: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<(), StoreError> {
        self.record_session_start(session_id, prompt_profile_digest, model, cwd)
    }

    pub fn record_user_prompt(
        &self,
        session_id: &str,
        turn_id: Option<&str>,
        prompt: &str,
        cwd: Option<&str>,
    ) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE sessions SET turn_id=?2, root_task=COALESCE(root_task, ?3), cwd=COALESCE(?4, cwd), updated_unix_ms=?5 WHERE session_id=?1",
            params![session_id, turn_id, prompt, cwd, now_ms()],
        )?;
        Ok(())
    }

    pub fn multi_need_policy(&self) -> Result<MultiNeedPolicy, StoreError> {
        let connection = self.connection()?;
        let value: Option<String> = connection
            .query_row("SELECT value FROM settings WHERE key='multi_need_policy'", [], |row| {
                row.get(0)
            })
            .optional()?;
        value
            .map(|json| serde_json::from_str(&json).map_err(StoreError::from))
            .transpose()
            .map(|value| value.unwrap_or_default())
    }

    pub fn session(&self, session_id: &str) -> Result<Option<SessionRecord>, StoreError> {
        let connection = self.connection()?;
        let value = connection
            .query_row(
                "SELECT session_id, turn_id, root_task, prompt_profile_digest, route_set_digest,
                        model, cwd, route_set_json, need_grammar_digest,
                        multi_need_policy_json, multi_need_policy_digest, transport,
                        transport_definition_digest, semantic_definition_digest,
                        role_profile_id, role_profile_revision,
                        role_profile_definition_digest
                 FROM sessions WHERE session_id=?1",
                [session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, Option<String>>(11)?,
                        row.get::<_, Option<String>>(12)?,
                        row.get::<_, Option<String>>(13)?,
                        row.get::<_, Option<String>>(14)?,
                        row.get::<_, Option<u64>>(15)?,
                        row.get::<_, Option<String>>(16)?,
                    ))
                },
            )
            .optional()?;
        value
            .map(
                |(
                    session_id,
                    turn_id,
                    root_task,
                    digest,
                    route_digest,
                    model,
                    cwd,
                    routes,
                    grammar,
                    multi_need_policy,
                    multi_need_policy_digest,
                    transport,
                    transport_definition_digest,
                    semantic_definition_digest,
                    role_profile_id,
                    role_profile_revision,
                    role_profile_definition_digest,
                )| {
                    let route_set = routes
                        .map(|value| serde_json::from_str(&value))
                        .transpose()?
                        .unwrap_or_default();
                    let role_profile_provenance = parse_role_profile_provenance((
                        role_profile_id,
                        role_profile_revision,
                        role_profile_definition_digest,
                    ))?;
                    if let Some(provenance) = &role_profile_provenance {
                        provenance.validate().map_err(|error| {
                            StoreError::RoleProfileCorruption(error.to_string())
                        })?;
                    }
                    Ok(SessionRecord {
                        session_id,
                        turn_id,
                        root_task,
                        prompt_profile_digest: Digest::parse(&digest)
                            .map_err(|error| StoreError::Digest(error.to_string()))?,
                        need_grammar_digest: grammar
                            .as_deref()
                            .map(Digest::parse)
                            .transpose()
                            .map_err(|error| StoreError::Digest(error.to_string()))?
                            .unwrap_or_else(legacy_need_grammar_definition_digest),
                        transport,
                        transport_definition_digest: transport_definition_digest
                            .as_deref()
                            .map(Digest::parse)
                            .transpose()
                            .map_err(|error| StoreError::Digest(error.to_string()))?,
                        semantic_definition_digest: semantic_definition_digest
                            .as_deref()
                            .map(Digest::parse)
                            .transpose()
                            .map_err(|error| StoreError::Digest(error.to_string()))?,
                        route_set_digest: Digest::parse(&route_digest)
                            .map_err(|error| StoreError::Digest(error.to_string()))?,
                        model,
                        cwd,
                        route_set,
                        multi_need_policy: multi_need_policy
                            .map(|value| serde_json::from_str(&value))
                            .transpose()?
                            .unwrap_or_default(),
                        multi_need_policy_digest: multi_need_policy_digest
                            .as_deref()
                            .map(Digest::parse)
                            .transpose()
                            .map_err(|error| StoreError::Digest(error.to_string()))?
                            .unwrap_or_else(|| MultiNeedPolicy::default().digest()),
                        role_profile_provenance,
                    })
                },
            )
            .transpose()
    }

    /// Resolve the exact historical revision frozen on a session. Active
    /// pointers are intentionally never consulted here.
    pub fn resolve_session_worker_config(
        &self,
        session_id: &str,
        executable: impl Into<String>,
    ) -> Result<WorkerConfig, StoreError> {
        let session = self
            .session(session_id)?
            .ok_or_else(|| StoreError::RoleProfileNotFound(format!("session {session_id}")))?;
        let provenance = session.role_profile_provenance.ok_or_else(|| {
            StoreError::RoleProfileConflict(
                "session has unknown role-profile provenance".to_owned(),
            )
        })?;
        let revision =
            self.read_role_profile_revision(&provenance.profile_id, provenance.revision)?;
        let actual = RoleProfileProvenance::from_revision(&revision)
            .map_err(|error| StoreError::RoleProfileCorruption(error.to_string()))?;
        if actual != provenance {
            return Err(StoreError::RoleProfileCorruption(
                "session role-profile provenance does not match historical revision".to_owned(),
            ));
        }
        revision
            .to_worker_config(executable)
            .map_err(|error| StoreError::RoleProfileCorruption(error.to_string()))
    }

    pub fn worker_config_for_session(
        &self,
        session_id: &str,
        executable: impl Into<String>,
    ) -> Result<WorkerConfig, StoreError> {
        self.resolve_session_worker_config(session_id, executable)
    }

    /// Checks a bounded provenance value against immutable historical storage.
    /// Current activation state is irrelevant; the exact revision and digest
    /// must still exist and validate.
    pub fn role_profile_provenance_is_historical(
        &self,
        provenance: &RoleProfileProvenance,
    ) -> Result<bool, StoreError> {
        provenance
            .validate()
            .map_err(|error| StoreError::RoleProfileCorruption(error.to_string()))?;
        match self.read_role_profile_revision_by_digest(
            &provenance.profile_id,
            provenance.definition_digest,
        ) {
            Ok(revision) => Ok(revision.revision == provenance.revision),
            Err(StoreError::RoleProfileNotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn record_need_step(
        &self,
        session_id: &str,
        step: &NeedStep,
        semantic_interrupt: &SemanticInterrupt,
        raw_message: &str,
    ) -> Result<(), StoreError> {
        if raw_message.len() > 16 * 1024 {
            return Err(StoreError::NeedStepRequest(
                "raw message exceeds the 16 KiB NeedIR input bound".to_owned(),
            ));
        }
        let parsed = SemanticInterrupt::parse(raw_message)
            .map_err(|error| StoreError::NeedStepRequest(error.to_string()))?
            .ok_or_else(|| {
                StoreError::NeedStepRequest("raw message contains no semantic interrupt".to_owned())
            })?;
        if parsed != *semantic_interrupt {
            return Err(StoreError::NeedStepRequest(
                "raw message does not match the supplied semantic interrupt".to_owned(),
            ));
        }
        let request_digest = semantic_interrupt.digest();
        let created_unix_ms = now_ms();
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO need_steps(
                need_step_id, session_id, ordinal, turn_id, coordination, relation, need_id,
                request_digest, request_json, created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                step.id.to_string(),
                session_id,
                step.ordinal,
                step.turn_id,
                step.coordination.as_str(),
                need_relation_name(step.relation),
                step.need_id.to_string(),
                request_digest.to_string(),
                serde_json::to_string(step)?,
                created_unix_ms,
            ],
        )?;
        transaction.execute(
            "INSERT INTO need_step_requests(
                need_step_id, session_id, request_digest, raw_message,
                semantic_interrupt_json, created_unix_ms, transport, request_format
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'hook', 'need_ir_text')",
            params![
                step.id.to_string(),
                session_id,
                request_digest.to_string(),
                raw_message,
                serde_json::to_string(semantic_interrupt)?,
                created_unix_ms,
            ],
        )?;
        append_need_step_event(&transaction, step.id, NeedStepState::Requested, "{}")?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_mcp_need_step(
        &self,
        session_id: &str,
        step: &NeedStep,
        request_digest: Digest,
        canonical_request_json: &str,
        need_ir: &needle_core::NeedIr,
    ) -> Result<(), StoreError> {
        if canonical_request_json.len() > needle_core::MAX_NEED_IR_BYTES {
            return Err(StoreError::NeedStepRequest(
                "canonical MCP request exceeds the 16 KiB bound".to_owned(),
            ));
        }
        let _: serde_json::Value = serde_json::from_str(canonical_request_json)?;
        let created_unix_ms = now_ms();
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO need_steps(
                need_step_id, session_id, ordinal, turn_id, coordination, relation, need_id,
                request_digest, request_json, created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                step.id.to_string(),
                session_id,
                step.ordinal,
                step.turn_id,
                step.coordination.as_str(),
                need_relation_name(step.relation),
                step.need_id.to_string(),
                request_digest.to_string(),
                serde_json::to_string(step)?,
                created_unix_ms,
            ],
        )?;
        transaction.execute(
            "INSERT INTO need_step_requests(
                need_step_id, session_id, request_digest, raw_message,
                semantic_interrupt_json, created_unix_ms, transport, request_format
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'mcp', 'json')",
            params![
                step.id.to_string(),
                session_id,
                request_digest.to_string(),
                canonical_request_json,
                serde_json::to_string(need_ir)?,
                created_unix_ms,
            ],
        )?;
        append_need_step_event(&transaction, step.id, NeedStepState::Requested, "{}")?;
        transaction.commit()?;
        Ok(())
    }

    pub fn append_need_step_event(
        &self,
        step_id: Digest,
        state: NeedStepState,
        event_json: &str,
    ) -> Result<(), StoreError> {
        let _: serde_json::Value = serde_json::from_str(event_json)?;
        let connection = self.connection()?;
        append_need_step_event(&connection, step_id, state, event_json)
    }

    pub fn attach_need_step_artifact(
        &self,
        step_id: Digest,
        artifact_id: needle_core::ArtifactId,
        proof_id: Option<needle_core::ReuseSufficiencyCertificateId>,
        role: &str,
    ) -> Result<(), StoreError> {
        if role.is_empty() || role.len() > 32 {
            return Err(StoreError::DefinitionDigest("need_step_artifact_role".to_owned()));
        }
        let connection = self.connection()?;
        connection.execute(
            "INSERT OR IGNORE INTO need_step_artifacts(
                need_step_id, artifact_id, proof_id, role, created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                step_id.to_string(),
                artifact_id.to_string(),
                proof_id.map(|value| value.to_string()),
                role,
                now_ms(),
            ],
        )?;
        Ok(())
    }

    pub fn need_steps(&self, session_id: &str) -> Result<Vec<NeedStep>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT request_json,
                    (SELECT state FROM need_step_events
                     WHERE need_step_id=need_steps.need_step_id ORDER BY event_id DESC LIMIT 1),
                    (SELECT event_json FROM need_step_events
                     WHERE need_step_id=need_steps.need_step_id ORDER BY event_id DESC LIMIT 1)
             FROM need_steps WHERE session_id=?1 ORDER BY ordinal",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut steps = Vec::new();
        for row in rows {
            let (json, state, event_json) = row?;
            let mut step: NeedStep = serde_json::from_str(&json)?;
            if let Some(snapshot) =
                event_json.as_deref().and_then(|json| serde_json::from_str::<NeedStep>(json).ok())
            {
                step = snapshot;
            }
            if let Some(state) = state {
                step.state = parse_need_step_state(&state)?;
            }
            let mut artifact_statement = connection.prepare(
                "SELECT artifact_id, proof_id FROM need_step_artifacts
                 WHERE need_step_id=?1 ORDER BY artifact_id, role",
            )?;
            let associations = artifact_statement.query_map([step.id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            for association in associations {
                let (artifact, proof) = association?;
                let artifact =
                    needle_core::ArtifactId(Digest::parse(&artifact).map_err(|error| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                    })?);
                if !step.artifacts.contains(&artifact) {
                    step.artifacts.push(artifact);
                }
                if step.proof.is_none() {
                    step.proof = proof
                        .as_deref()
                        .map(Digest::parse)
                        .transpose()
                        .map_err(|error| StoreError::Digest(error.to_string()))?
                        .map(needle_core::ReuseSufficiencyCertificateId);
                }
            }
            steps.push(step);
        }
        Ok(steps)
    }

    pub fn need_step(&self, step_id: Digest) -> Result<Option<NeedStep>, StoreError> {
        let connection = self.connection()?;
        let session_id = connection
            .query_row(
                "SELECT session_id FROM need_steps WHERE need_step_id=?1",
                [step_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        drop(connection);
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        Ok(self.need_steps(&session_id)?.into_iter().find(|step| step.id == step_id))
    }

    pub fn need_step_request(
        &self,
        step_id: Digest,
    ) -> Result<Option<NeedStepRequestRecord>, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT session_id, request_digest, raw_message, semantic_interrupt_json,
                        created_unix_ms, transport, request_format
                 FROM need_step_requests WHERE need_step_id=?1",
                [step_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, u64>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(
                    session_id,
                    request_digest,
                    raw_message,
                    semantic_interrupt_json,
                    created_unix_ms,
                    transport,
                    request_format,
                )| {
                    Ok(NeedStepRequestRecord {
                        need_step_id: step_id,
                        session_id,
                        request_digest: parse_digest(&request_digest)?,
                        raw_message,
                        semantic_interrupt: if transport.as_deref() == Some("mcp") {
                            None
                        } else {
                            Some(serde_json::from_str(&semantic_interrupt_json)?)
                        },
                        need_ir: if transport.as_deref() == Some("mcp") {
                            Some(serde_json::from_str(&semantic_interrupt_json)?)
                        } else {
                            None
                        },
                        transport,
                        request_format,
                        created_unix_ms,
                    })
                },
            )
            .transpose()
    }

    pub fn need_step_session_id(&self, step_id: Digest) -> Result<Option<String>, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT session_id FROM need_steps WHERE need_step_id=?1",
                [step_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn need_step_events(
        &self,
        session_id: Option<&str>,
        after_event_id: u64,
        limit: u16,
    ) -> Result<Vec<NeedStepEventRecord>, StoreError> {
        let connection = self.connection()?;
        let limit = u64::from(limit.clamp(1, 200));
        let sql = if session_id.is_some() {
            "SELECT e.event_id, s.session_id, e.need_step_id, e.state, e.event_json,
                    e.created_unix_ms
             FROM need_step_events e
             JOIN need_steps s ON s.need_step_id=e.need_step_id
             WHERE s.session_id=?1 AND e.event_id>?2
             ORDER BY e.event_id LIMIT ?3"
        } else {
            "SELECT e.event_id, s.session_id, e.need_step_id, e.state, e.event_json,
                    e.created_unix_ms
             FROM need_step_events e
             JOIN need_steps s ON s.need_step_id=e.need_step_id
             WHERE e.event_id>?1
             ORDER BY e.event_id LIMIT ?2"
        };
        let mut statement = connection.prepare(sql)?;
        let collect = |row: &rusqlite::Row<'_>| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, u64>(5)?,
            ))
        };
        let rows = if let Some(session_id) = session_id {
            statement.query_map(params![session_id, after_event_id, limit], collect)?
        } else {
            statement.query_map(params![after_event_id, limit], collect)?
        };
        rows.map(|row| {
            let (event_id, session_id, step_id, state, event_json, created_unix_ms) = row?;
            Ok(NeedStepEventRecord {
                event_id,
                session_id,
                need_step_id: parse_digest(&step_id)?,
                state: parse_need_step_state(&state)?,
                event: serde_json::from_str(&event_json)?,
                created_unix_ms,
            })
        })
        .collect()
    }

    pub fn main_turn_observations(
        &self,
        session_id: &str,
    ) -> Result<Vec<MainTurnObservationRecord>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT session_id, turn_id, need_step_id, status, delivery, usage_json, tools_json,
                    main_discovery_tainted, outcome_json
             FROM main_turn_observations WHERE session_id=?1 ORDER BY observation_id",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, bool>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?;
        rows.map(|row| {
            let (
                session_id,
                turn_id,
                need_step_id,
                status,
                delivery,
                usage_json,
                tools_json,
                main_discovery_tainted,
                outcome_json,
            ) = row?;
            Ok(MainTurnObservationRecord {
                session_id,
                turn_id,
                need_step_id: need_step_id.as_deref().map(parse_digest).transpose()?,
                status,
                delivery,
                usage_json,
                tools_json,
                main_discovery_tainted,
                outcome: outcome_json.as_deref().map(serde_json::from_str).transpose()?,
            })
        })
        .collect()
    }

    pub fn record_main_turn_observation(
        &self,
        observation: &MainTurnObservationRecord,
    ) -> Result<(), StoreError> {
        let _: serde_json::Value = serde_json::from_str(&observation.usage_json)?;
        let _: serde_json::Value = serde_json::from_str(&observation.tools_json)?;
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO main_turn_observations(
                session_id, turn_id, need_step_id, status, delivery, usage_json, tools_json,
                main_discovery_tainted, outcome_json, created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                observation.session_id,
                observation.turn_id,
                observation.need_step_id.map(|value| value.to_string()),
                observation.status,
                observation.delivery,
                observation.usage_json,
                observation.tools_json,
                observation.main_discovery_tainted,
                observation.outcome.as_ref().map(serde_json::to_string).transpose()?,
                now_ms(),
            ],
        )?;
        Ok(())
    }

    pub fn end_session(&self, session_id: &str) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE sessions
             SET root_task=NULL, turn_id=NULL, ended_unix_ms=?2, updated_unix_ms=?2
             WHERE session_id=?1",
            params![session_id, now_ms()],
        )?;
        Ok(())
    }

    pub fn cache_lookup(&self, identity: &NeedCacheIdentity) -> Result<CacheLookup, StoreError> {
        let Some(requested_provenance) = identity.role_profile_provenance.as_ref() else {
            return Ok(CacheLookup::Bypass("role-profile-provenance-unknown".to_owned()));
        };
        if !self.role_profile_provenance_is_historical(requested_provenance)? {
            return Ok(CacheLookup::Bypass("role-profile-provenance-invalid".to_owned()));
        }
        let connection = self.connection()?;
        let digest = identity.digest().to_string();
        let json: Option<String> = connection
            .query_row(
                "SELECT entry_json FROM cache_entries WHERE identity_digest=?1",
                [&digest],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(json) = json {
            let mut entry: NeedCacheEntry = serde_json::from_str(&json)?;
            if entry.identity.digest().to_string() != digest {
                return Err(StoreError::Digest(
                    "cache entry identity does not match its primary key".to_owned(),
                ));
            }
            if entry.identity.role_profile_provenance.as_ref() != Some(requested_provenance)
                || entry.worker_outcome.role_profile_provenance.as_ref()
                    != Some(requested_provenance)
            {
                return Err(StoreError::ArtifactIdentity(
                    "cache entry role-profile provenance is inconsistent".to_owned(),
                ));
            }
            connection.execute(
                "UPDATE cache_entries SET hit_count=hit_count+1 WHERE identity_digest=?1",
                [&digest],
            )?;
            entry.hit_count = entry.hit_count.saturating_add(1);
            return Ok(CacheLookup::Hit(Box::new(entry)));
        }
        let stale = connection
            .query_row(
                "SELECT 1 FROM cache_entries WHERE logical_digest=?1 LIMIT 1",
                [identity.logical_digest().to_string()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(if stale { CacheLookup::Stale } else { CacheLookup::Miss })
    }

    pub fn publish(&self, entry: &NeedCacheEntry) -> Result<(), StoreError> {
        let Some(provenance) = entry.identity.role_profile_provenance.as_ref() else {
            return Err(StoreError::ArtifactIdentity(
                "cannot publish cache entry without role-profile provenance".to_owned(),
            ));
        };
        if entry.worker_outcome.role_profile_provenance.as_ref() != Some(provenance) {
            return Err(StoreError::ArtifactIdentity(
                "cache identity and worker outcome role-profile provenance differ".to_owned(),
            ));
        }
        if !self.role_profile_provenance_is_historical(provenance)? {
            return Err(StoreError::ArtifactIdentity(
                "cache entry references an unknown role-profile revision".to_owned(),
            ));
        }
        let connection = self.connection()?;
        connection.execute(
            "INSERT OR REPLACE INTO cache_entries(identity_digest, logical_digest, source_digest, entry_json, created_unix_ms, hit_count)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                entry.identity.digest().to_string(),
                entry.identity.logical_digest().to_string(),
                entry.identity.source_snapshot_digest.to_string(),
                serde_json::to_string(entry)?,
                entry.created_unix_ms,
                entry.hit_count,
            ],
        )?;
        Ok(())
    }

    pub fn publish_artifact(
        &self,
        request: &ArtifactRequest,
        artifact: &Artifact,
    ) -> Result<(), StoreError> {
        let request_id = request.id();
        if artifact.request_id != request_id {
            return Err(StoreError::ArtifactIdentity(
                "artifact request id does not match request".to_owned(),
            ));
        }
        let computed = Artifact::compute_id(request_id, &artifact.contract, &artifact.payload)?;
        if artifact.id != computed {
            return Err(StoreError::ArtifactIdentity(
                "artifact id does not match semantic payload".to_owned(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT OR IGNORE INTO artifact_requests(
                request_id, logical_id, source_digest, contract_id, route_key, request_json,
                created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                request_id.to_string(),
                request.logical_id().to_string(),
                request.source_snapshot_digest.to_string(),
                request.contract_id,
                request.route_key.as_str(),
                serde_json::to_string(request)?,
                artifact.created_unix_ms,
            ],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO artifacts(
                artifact_id, request_id, contract_id, artifact_json, created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                artifact.id.to_string(),
                request_id.to_string(),
                artifact.contract.id,
                serde_json::to_string(artifact)?,
                artifact.created_unix_ms,
            ],
        )?;
        for (position, input) in request.input_artifact_ids.iter().enumerate() {
            transaction.execute(
                "INSERT OR IGNORE INTO artifact_inputs(request_id, position, artifact_id)
                 VALUES(?1, ?2, ?3)",
                params![request_id.to_string(), position, input.to_string()],
            )?;
        }
        for dependency in &artifact.dependency_manifest.dependencies {
            transaction.execute(
                "INSERT OR IGNORE INTO dependencies(
                    artifact_id, path, content_digest, byte_start, byte_end, claims_json
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    artifact.id.to_string(),
                    dependency.path,
                    dependency.content_digest.to_string(),
                    dependency.byte_start,
                    dependency.byte_end,
                    serde_json::to_string(&dependency.claims)?,
                ],
            )?;
        }
        for validation in &artifact.validations {
            transaction.execute(
                "INSERT OR IGNORE INTO validations(
                    artifact_id, validator, validator_revision, status, evidence_digest,
                    validated_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    artifact.id.to_string(),
                    validation.validator,
                    validation.validator_revision,
                    validation.status,
                    validation.evidence_digest.to_string(),
                    validation.validated_unix_ms,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn publish_semantic_artifact(
        &self,
        request: &ArtifactRequest,
        need: &Need,
        artifact: &Artifact,
        certificate: &ArtifactValidationCertificate,
    ) -> Result<(), StoreError> {
        let request_id = request.semantic_id().digest();
        if request.input_artifact_ids.len() > needle_core::MAX_NEED_INPUTS {
            return Err(StoreError::ArtifactIdentity(
                "semantic artifact request exceeds the input bound".to_owned(),
            ));
        }
        if artifact.request_id != request_id || certificate.artifact.digest() != artifact.id {
            return Err(StoreError::ArtifactIdentity(
                "semantic artifact origin or certificate does not match".to_owned(),
            ));
        }
        if artifact.contract.schema_id != needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID {
            return Err(StoreError::ArtifactIdentity(
                "semantic artifact content identity does not match".to_owned(),
            ));
        }
        let semantic_payload: needle_core::SemanticWorkerArtifact =
            serde_json::from_value(artifact.payload.clone())?;
        if semantic_payload
            .canonical_artifact_id(artifact.contract.definition_digest)
            .map(|id| id.digest())
            != Some(artifact.id)
        {
            return Err(StoreError::ArtifactIdentity(
                "semantic artifact canonical identity does not match".to_owned(),
            ));
        }
        let mut request_inputs = request.input_artifact_ids.clone();
        request_inputs.sort();
        let mut certificate_inputs =
            certificate.input_artifacts.iter().map(|input| input.digest()).collect::<Vec<_>>();
        certificate_inputs.sort();
        if request_inputs != certificate_inputs {
            return Err(StoreError::ArtifactIdentity(
                "semantic artifact inputs do not match the validation certificate".to_owned(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for input in &certificate.input_artifacts {
            let certified: u64 = transaction.query_row(
                "SELECT COUNT(*) FROM artifacts a
                 JOIN artifact_validation_certificates c ON c.artifact_id=a.artifact_id
                 WHERE a.artifact_id=?1 AND a.format_revision=2",
                [input.to_string()],
                |row| row.get(0),
            )?;
            if certified == 0 {
                return Err(StoreError::ArtifactIdentity(format!(
                    "semantic input `{input}` is not certified"
                )));
            }
        }
        for evidence_id in &certificate.evidence_ids {
            let exists: u64 = transaction.query_row(
                "SELECT COUNT(*) FROM command_evidence WHERE evidence_id=?1",
                [evidence_id],
                |row| row.get(0),
            )?;
            if exists == 0 {
                return Err(StoreError::ArtifactIdentity(format!(
                    "validation evidence `{evidence_id}` is not present"
                )));
            }
        }
        transaction.execute(
            "INSERT OR IGNORE INTO artifact_requests(
                request_id, logical_id, source_digest, contract_id, route_key, request_json,
                created_unix_ms, format_revision, demand_id
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 2, ?8)",
            params![
                request_id.to_string(),
                request.semantic_logical_id().to_string(),
                request.source_snapshot_digest.to_string(),
                request.contract_id,
                request.route_key.as_str(),
                serde_json::to_string(request)?,
                artifact.created_unix_ms,
                need.id.to_string(),
            ],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO artifacts(
                artifact_id, request_id, contract_id, artifact_json, created_unix_ms,
                format_revision
             ) VALUES(?1, ?2, ?3, ?4, ?5, 2)",
            params![
                artifact.id.to_string(),
                request_id.to_string(),
                artifact.contract.id,
                serde_json::to_string(artifact)?,
                artifact.created_unix_ms,
            ],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO artifact_origins(
                artifact_id, request_id, route_key, need_id, observed_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                artifact.id.to_string(),
                request_id.to_string(),
                request.route_key.as_str(),
                need.id.to_string(),
                artifact.created_unix_ms,
            ],
        )?;
        let world_digest = certificate.coverage.world.id();
        transaction.execute(
            "INSERT OR IGNORE INTO semantic_worlds(world_digest, world_json, created_unix_ms)
             VALUES(?1, ?2, ?3)",
            params![
                world_digest.to_string(),
                serde_json::to_string(&certificate.coverage.world)?,
                artifact.created_unix_ms,
            ],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO dependency_manifests(
                manifest_digest, manifest_json, created_unix_ms
             ) VALUES(?1, ?2, ?3)",
            params![
                certificate.coverage.dependency_manifest_digest.to_string(),
                serde_json::to_string(&artifact.dependency_manifest)?,
                artifact.created_unix_ms,
            ],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO artifact_validation_certificates(
                certificate_id, artifact_id, validator_definition_digest,
                dependency_manifest_digest, world_digest, certificate_json, issued_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                certificate.id.to_string(),
                artifact.id.to_string(),
                certificate.validator_definition.to_string(),
                certificate.coverage.dependency_manifest_digest.to_string(),
                world_digest.to_string(),
                serde_json::to_string(certificate)?,
                certificate.issued_unix_ms,
            ],
        )?;
        for entry in &certificate.coverage.entries {
            transaction.execute(
                "INSERT OR IGNORE INTO coverage_entries(
                    certificate_id, artifact_id, obligation_id, predicate, subject_id,
                    world_digest, obligation_json, evidence_json
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    certificate.id.to_string(),
                    artifact.id.to_string(),
                    entry.obligation.id.to_string(),
                    format!("{:?}", entry.obligation.predicate),
                    entry.obligation.subject.to_string(),
                    world_digest.to_string(),
                    serde_json::to_string(&entry.obligation)?,
                    serde_json::to_string(&entry.evidence)?,
                ],
            )?;
        }
        for dependency in &artifact.dependency_manifest.dependencies {
            transaction.execute(
                "INSERT OR IGNORE INTO dependencies(
                    artifact_id, path, content_digest, byte_start, byte_end, claims_json
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    artifact.id.to_string(),
                    dependency.path,
                    dependency.content_digest.to_string(),
                    dependency.byte_start,
                    dependency.byte_end,
                    serde_json::to_string(&dependency.claims)?,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn semantic_candidates(
        &self,
        need: &Need,
        exact_request_ids: &[Digest],
        source_snapshot_digest: Digest,
    ) -> Result<Vec<(Artifact, ArtifactValidationCertificate, bool, bool)>, StoreError> {
        if exact_request_ids.len() > needle_core::MAX_NEED_INPUTS {
            return Err(StoreError::ArtifactIdentity(
                "exact semantic request lookup exceeds the configured bound".to_owned(),
            ));
        }
        let connection = self.connection()?;
        let mut certificate_ids = std::collections::BTreeSet::new();
        let world = need.world.id().to_string();
        let mut statement = connection.prepare_cached(
            "SELECT certificate_id FROM coverage_entries
             WHERE predicate=?1 AND subject_id=?2 AND world_digest=?3
             ORDER BY certificate_id LIMIT 64",
        )?;
        'obligations: for obligation in &need.required {
            let rows = statement.query_map(
                params![
                    format!("{:?}", obligation.predicate),
                    obligation.subject.to_string(),
                    world,
                ],
                |row| row.get::<_, String>(0),
            )?;
            for row in rows {
                if certificate_ids.len() >= needle_core::MAX_PROOF_CANDIDATES {
                    break 'obligations;
                }
                certificate_ids.insert(row?);
            }
        }
        let mut candidates = Vec::with_capacity(certificate_ids.len());
        let mut load = connection.prepare_cached(
            "SELECT a.artifact_id, c.validator_definition_digest,
                    c.dependency_manifest_digest, c.world_digest,
                    a.artifact_json, c.certificate_json
             FROM artifact_validation_certificates c
             JOIN artifacts a ON a.artifact_id=c.artifact_id
             WHERE c.certificate_id=?1",
        )?;
        let mut origin_lookup = connection.prepare_cached(
            "SELECT 1 FROM artifact_origins
             WHERE artifact_id=?1 AND request_id=?2 LIMIT 1",
        )?;
        let mut source_lookup = connection.prepare_cached(
            "SELECT 1
             FROM artifact_origins origins
             JOIN artifact_requests requests ON requests.request_id=origins.request_id
             WHERE origins.artifact_id=?1 AND requests.source_digest=?2
             LIMIT 1",
        )?;
        let mut evidence_lookup = connection
            .prepare_cached("SELECT 1 FROM command_evidence WHERE evidence_id=?1 LIMIT 1")?;
        for certificate_id in certificate_ids {
            let pair: Option<(String, String, String, String, String, String)> = load
                .query_row([&certificate_id], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })
                .optional()?;
            if let Some((
                artifact_id,
                validator,
                dependency,
                stored_world,
                artifact_json,
                certificate_json,
            )) = pair
            {
                let mut row_hasher = needle_core::CanonicalHasher::new(b"semantic-cache-row");
                row_hasher.field_str(&artifact_json);
                row_hasher.field_str(&certificate_json);
                let row_digest = row_hasher.finish();
                let cached = self
                    .semantic_cache
                    .lock()
                    .map_err(|_| StoreError::ConnectionLock)?
                    .get(&certificate_id)
                    .filter(|(artifact, certificate, cached_row_digest)| {
                        artifact.id.to_string() == artifact_id
                            && certificate.id.to_string() == certificate_id
                            && certificate.validator_definition.to_string() == validator
                            && certificate.coverage.dependency_manifest_digest.to_string()
                                == dependency
                            && certificate.coverage.world.id().to_string() == stored_world
                            && *cached_row_digest == row_digest
                    });
                let (artifact, certificate) = match cached {
                    Some((artifact, certificate, _)) => (artifact, certificate),
                    None => {
                        let value: (Artifact, ArtifactValidationCertificate) = (
                            serde_json::from_str(&artifact_json)?,
                            serde_json::from_str(&certificate_json)?,
                        );
                        self.semantic_cache.lock().map_err(|_| StoreError::ConnectionLock)?.insert(
                            certificate_id.clone(),
                            (value.0.clone(), value.1.clone(), row_digest),
                        );
                        value
                    }
                };
                if artifact.id.to_string() != artifact_id
                    || certificate.id.to_string() != certificate_id
                    || certificate.validator_definition.to_string() != validator
                    || certificate.coverage.dependency_manifest_digest.to_string() != dependency
                    || certificate.coverage.world.id().to_string() != stored_world
                {
                    return Err(StoreError::ArtifactIdentity(
                        "semantic cache row key does not match its immutable payload".to_owned(),
                    ));
                }
                let mut evidence_complete = true;
                for evidence_id in &certificate.evidence_ids {
                    if evidence_lookup.query_row([evidence_id], |_| Ok(())).optional()?.is_none() {
                        evidence_complete = false;
                        break;
                    }
                }
                if !evidence_complete {
                    continue;
                }
                let mut exact_request = false;
                for request_id in exact_request_ids {
                    if origin_lookup
                        .query_row(params![artifact_id, request_id.to_string()], |_| Ok(()))
                        .optional()?
                        .is_some()
                    {
                        exact_request = true;
                        break;
                    }
                }
                let same_source = source_lookup
                    .query_row(params![artifact_id, source_snapshot_digest.to_string()], |_| Ok(()))
                    .optional()?
                    .is_some();
                candidates.push((artifact, certificate, exact_request, same_source));
            }
        }
        Ok(candidates)
    }

    pub fn semantic_artifact_has_source(
        &self,
        artifact_id: &str,
        source_snapshot_digest: Digest,
    ) -> Result<bool, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT 1
                 FROM artifact_origins origins
                 JOIN artifact_requests requests ON requests.request_id=origins.request_id
                 WHERE origins.artifact_id=?1 AND requests.source_digest=?2
                 LIMIT 1",
                params![artifact_id, source_snapshot_digest.to_string()],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(StoreError::from)
    }

    pub fn active_contradiction(&self, need: &Need) -> Result<bool, StoreError> {
        let connection = self.connection()?;
        for obligation in &need.required {
            let count: u64 = connection.query_row(
                "SELECT COUNT(*) FROM contradiction_records
                 WHERE predicate=?1 AND subject_id=?2 AND world_digest=?3 AND status='active'",
                params![
                    format!("{:?}", obligation.predicate),
                    obligation.subject.to_string(),
                    need.world.id().to_string(),
                ],
                |row| row.get(0),
            )?;
            if count > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn active_contradiction_count(&self) -> Result<u64, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT COUNT(*) FROM contradiction_records WHERE status='active'",
                [],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    pub fn record_contradiction(
        &self,
        predicate: needle_core::PredicateKind,
        subject: needle_core::SubjectId,
        world_digest: Digest,
        artifact_ids: &[needle_core::ArtifactId],
        active: bool,
    ) -> Result<Digest, StoreError> {
        let mut artifacts = artifact_ids.to_vec();
        artifacts.sort();
        artifacts.dedup();
        let mut hasher = needle_core::CanonicalHasher::new(b"contradiction");
        hasher.field_str(&format!("{predicate:?}"));
        hasher.field_digest(subject.digest());
        hasher.field_digest(world_digest);
        for artifact in &artifacts {
            hasher.field_digest(artifact.digest());
        }
        let id = hasher.finish();
        let connection = self.connection()?;
        connection.execute(
            "INSERT OR REPLACE INTO contradiction_records(
                contradiction_id, predicate, subject_id, world_digest, status,
                artifact_ids_json, updated_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id.to_string(),
                format!("{predicate:?}"),
                subject.to_string(),
                world_digest.to_string(),
                if active { "active" } else { "resolved" },
                serde_json::to_string(&artifacts)?,
                now_ms(),
            ],
        )?;
        Ok(id)
    }

    pub fn record_proof_plan(
        &self,
        plan: &SelectedPlan,
        resolution: &str,
        certificate: Option<&ReuseSufficiencyCertificate>,
        candidates: &[ProofCandidate],
    ) -> Result<(), StoreError> {
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let created = now_ms();
        transaction.execute(
            "INSERT OR REPLACE INTO selected_plans(
                plan_id, need_id, resolution, plan_json, created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                plan.id.to_string(),
                plan.need.to_string(),
                resolution,
                serde_json::to_string(plan)?,
                created,
            ],
        )?;
        for (position, candidate) in candidates.iter().enumerate() {
            transaction.execute(
                "INSERT OR REPLACE INTO plan_candidates(
                    plan_id, position, candidate_json, selected
                 ) VALUES(?1, ?2, ?3, ?4)",
                params![
                    plan.id.to_string(),
                    position,
                    serde_json::to_string(candidate)?,
                    plan.artifact_ids.contains(&candidate.artifact),
                ],
            )?;
        }
        if let Some(certificate) = certificate {
            transaction.execute(
                "INSERT OR REPLACE INTO reuse_sufficiency_certificates(
                    certificate_id, need_id, engine_definition_digest, certificate_json,
                    created_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![
                    certificate.id.to_string(),
                    certificate.need.to_string(),
                    certificate.engine_definition.to_string(),
                    serde_json::to_string(certificate)?,
                    created,
                ],
            )?;
            for (position, step) in certificate.satisfaction_steps.iter().enumerate() {
                transaction.execute(
                    "INSERT OR REPLACE INTO satisfaction_steps(
                        certificate_id, position, obligation_id, artifact_id,
                        validation_certificate_id
                     ) VALUES(?1, ?2, ?3, ?4, ?5)",
                    params![
                        certificate.id.to_string(),
                        position,
                        step.obligation.to_string(),
                        step.artifact.to_string(),
                        step.validation_certificate.to_string(),
                    ],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn record_proof_accounting(
        &self,
        record: &ProofAccountingRecord,
    ) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO proof_accounting(
                need_id, plan_id, parse_micros, lookup_micros, validation_micros,
                planning_micros, projection_micros, allocation_count, allocated_bytes,
                stale_candidates, created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                record.need_id.to_string(),
                record.plan_id.map(|id| id.to_string()),
                record.parse_micros,
                record.lookup_micros,
                record.validation_micros,
                record.planning_micros,
                record.projection_micros,
                record.allocation_count,
                record.allocated_bytes,
                record.stale_candidates,
                record.created_unix_ms,
            ],
        )?;
        Ok(())
    }

    pub fn proof_accounting(&self, limit: u32) -> Result<Vec<ProofAccountingRecord>, StoreError> {
        let connection = self.connection()?;
        let query = format!(
            "SELECT need_id, plan_id, parse_micros, lookup_micros, validation_micros,
                    planning_micros, projection_micros, allocation_count, allocated_bytes,
                    stale_candidates, created_unix_ms
             FROM proof_accounting ORDER BY created_unix_ms DESC, id DESC LIMIT {}",
            limit.min(500)
        );
        let mut statement = connection.prepare(&query)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
                row.get::<_, u64>(5)?,
                row.get::<_, u64>(6)?,
                row.get::<_, Option<u64>>(7)?,
                row.get::<_, Option<u64>>(8)?,
                row.get::<_, u64>(9)?,
                row.get::<_, u64>(10)?,
            ))
        })?;
        rows.map(|row| {
            let (
                need,
                plan,
                parse_micros,
                lookup_micros,
                validation_micros,
                planning_micros,
                projection_micros,
                allocation_count,
                allocated_bytes,
                stale_candidates,
                created_unix_ms,
            ) = row?;
            Ok(ProofAccountingRecord {
                need_id: needle_core::NeedId(parse_digest(&need)?),
                plan_id: plan
                    .as_deref()
                    .map(parse_digest)
                    .transpose()?
                    .map(needle_core::SelectedPlanId),
                parse_micros,
                lookup_micros,
                validation_micros,
                planning_micros,
                projection_micros,
                allocation_count,
                allocated_bytes,
                stale_candidates,
                created_unix_ms,
            })
        })
        .collect()
    }

    pub fn record_route_cost_observation(
        &self,
        observation: &RouteCostObservation,
    ) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO usage_records(attempt_id, route_key, usage_json, created_unix_ms)
             VALUES(NULL, ?1, ?2, ?3)",
            params![
                observation.route_key,
                serde_json::to_string(observation)?,
                observation.observed_unix_ms,
            ],
        )?;
        Ok(())
    }

    pub fn observed_route_cost(&self, route_key: &str) -> Result<Option<u64>, StoreError> {
        self.observed_route_cost_for_source(route_key, None)
    }

    pub fn record_operator_cost_observation(
        &self,
        observation: &OperatorCostObservation,
    ) -> Result<(), StoreError> {
        if observation.requested_kind_count != 1 {
            return Err(StoreError::OperatorCostObservation(
                "only single-kind worker observations are admissible".to_owned(),
            ));
        }
        if observation.artifact_kind.trim().is_empty()
            || observation.worker_model.trim().is_empty()
            || observation.worker_reasoning.trim().is_empty()
            || observation.service_tier.trim().is_empty()
        {
            return Err(StoreError::OperatorCostObservation(
                "kind and execution profile fields must be non-empty".to_owned(),
            ));
        }
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO operator_cost_observations(
                artifact_kind, worker_model, worker_reasoning, service_tier,
                schema_digest, validator_definition_digest, pricing_digest,
                requested_kind_count, cost_microusd, execution_attempt_id,
                evidence_digest, observed_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                observation.artifact_kind,
                observation.worker_model,
                observation.worker_reasoning,
                observation.service_tier,
                observation.schema_digest.to_string(),
                observation.validator_definition_digest.to_string(),
                observation.pricing_digest.to_string(),
                observation.requested_kind_count,
                observation.cost_microusd,
                observation.execution_attempt_id.map(|value| value.to_string()),
                observation.evidence_digest.to_string(),
                observation.observed_unix_ms,
            ],
        )?;
        Ok(())
    }

    pub fn observed_operator_cost(
        &self,
        key: &OperatorCostKey<'_>,
    ) -> Result<Option<u64>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare_cached(
            "SELECT cost_microusd FROM operator_cost_observations
             WHERE artifact_kind=?1 AND worker_model=?2 AND worker_reasoning=?3
               AND service_tier=?4 AND schema_digest=?5
               AND validator_definition_digest=?6 AND pricing_digest=?7
             ORDER BY observed_unix_ms DESC, observation_id DESC LIMIT 31",
        )?;
        let mut costs = statement
            .query_map(
                params![
                    key.artifact_kind,
                    key.worker_model,
                    key.worker_reasoning,
                    key.service_tier,
                    key.schema_digest.to_string(),
                    key.validator_definition_digest.to_string(),
                    key.pricing_digest.to_string(),
                ],
                |row| row.get::<_, u64>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        if costs.is_empty() {
            return Ok(None);
        }
        costs.sort_unstable();
        Ok(Some(costs[costs.len() / 2]))
    }

    pub fn observed_route_cost_by_source(
        &self,
        route_key: &str,
        source: &str,
    ) -> Result<Option<u64>, StoreError> {
        self.observed_route_cost_for_source(route_key, Some(source))
    }

    fn observed_route_cost_for_source(
        &self,
        route_key: &str,
        source: Option<&str>,
    ) -> Result<Option<u64>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare_cached(
            "SELECT usage_json FROM usage_records
             WHERE route_key=?1
               AND (?2 IS NULL OR json_extract(usage_json, '$.source')=?2)
             ORDER BY created_unix_ms DESC LIMIT 31",
        )?;
        let mut costs = statement
            .query_map(params![route_key, source], |row| row.get::<_, String>(0))?
            .filter_map(|row| row.ok())
            .filter_map(|json| serde_json::from_str::<RouteCostObservation>(&json).ok())
            .map(|observation| observation.cost_microusd)
            .collect::<Vec<_>>();
        if costs.is_empty() {
            return Ok(None);
        }
        costs.sort_unstable();
        Ok(Some(costs[costs.len() / 2]))
    }

    pub fn resolve_artifact(
        &self,
        request: &ArtifactRequest,
    ) -> Result<(CacheResolution, Option<Artifact>), StoreError> {
        let connection = self.connection()?;
        let request_id = request.id().to_string();
        let exact: Option<(String, String)> = connection
            .query_row(
                "SELECT artifact_id, artifact_json FROM artifacts
                 WHERE request_id=?1 ORDER BY created_unix_ms DESC LIMIT 1",
                [&request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored_id, json)) = exact {
            let artifact: Artifact = serde_json::from_str(&json)?;
            if artifact.id.to_string() != stored_id || artifact.request_id != request.id() {
                return Err(StoreError::ArtifactIdentity(
                    "stored artifact key does not match payload".to_owned(),
                ));
            }
            return Ok((
                CacheResolution::ExactHit {
                    artifact_id: artifact.id,
                    sufficiency_certificate_id: None,
                    selected_plan_id: None,
                    resolution_format_revision: None,
                },
                Some(artifact),
            ));
        }
        let stale: Option<String> = connection
            .query_row(
                "SELECT artifact_id FROM artifacts
                 WHERE request_id IN (
                    SELECT request_id FROM artifact_requests WHERE logical_id=?1
                 ) ORDER BY created_unix_ms DESC LIMIT 1",
                [request.logical_id().to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match stale {
            Some(artifact_id) => (
                CacheResolution::Stale {
                    artifact_id: parse_digest(&artifact_id)?,
                    reason: "source snapshot changed".to_owned(),
                },
                None,
            ),
            None => (CacheResolution::Miss, None),
        })
    }

    pub fn latest_logical_artifact(
        &self,
        request: &ArtifactRequest,
    ) -> Result<Option<Artifact>, StoreError> {
        let connection = self.connection()?;
        let stored: Option<(String, String)> = connection
            .query_row(
                "SELECT a.artifact_id, a.artifact_json
                 FROM artifacts a
                 JOIN artifact_requests r ON r.request_id = a.request_id
                 WHERE r.logical_id=?1
                 ORDER BY a.created_unix_ms DESC LIMIT 1",
                [request.logical_id().to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((stored_id, json)) = stored else {
            return Ok(None);
        };
        let artifact: Artifact = serde_json::from_str(&json)?;
        if artifact.id.to_string() != stored_id {
            return Err(StoreError::ArtifactIdentity(
                "stored logical artifact key does not match payload".to_owned(),
            ));
        }
        Ok(Some(artifact))
    }

    pub fn acquire_artifact_lease(
        &self,
        request_id: Digest,
        owner: &str,
        expires_unix_ms: u64,
    ) -> Result<bool, StoreError> {
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction
            .execute("DELETE FROM artifact_leases WHERE expires_unix_ms <= ?1", [now_ms()])?;
        let acquired = transaction.execute(
            "INSERT OR IGNORE INTO artifact_leases(request_id, owner, expires_unix_ms)
             VALUES(?1, ?2, ?3)",
            params![request_id.to_string(), owner, expires_unix_ms],
        )? == 1;
        transaction.commit()?;
        Ok(acquired)
    }

    pub fn release_artifact_lease(
        &self,
        request_id: Digest,
        owner: &str,
    ) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM artifact_leases WHERE request_id=?1 AND owner=?2",
            params![request_id.to_string(), owner],
        )?;
        Ok(())
    }

    pub fn enqueue_approval(&self, request: &ApprovalRequest) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO approval_requests(
                approval_id, status, payload_digest, expires_unix_ms, request_json,
                created_unix_ms
             ) VALUES(?1, 'pending', ?2, ?3, ?4, ?5)",
            params![
                request.id,
                request.payload_digest.to_string(),
                request.expires_unix_ms,
                serde_json::to_string(request)?,
                now_ms(),
            ],
        )?;
        Ok(())
    }

    pub fn pending_approvals(&self) -> Result<Vec<ApprovalRequest>, StoreError> {
        self.expire_approvals()?;
        self.json_rows(
            "SELECT request_json FROM approval_requests
             WHERE status='pending' ORDER BY created_unix_ms",
        )
    }

    pub fn approval(&self, id: &str) -> Result<Option<ApprovalRequest>, StoreError> {
        let connection = self.connection()?;
        let json: Option<String> = connection
            .query_row(
                "SELECT request_json FROM approval_requests WHERE approval_id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|value| serde_json::from_str(&value).map_err(StoreError::from)).transpose()
    }

    pub fn decide_approval(
        &self,
        id: &str,
        decision: ApprovalDecision,
        source: ApprovalDecisionSource,
        expected_payload_digest: Digest,
    ) -> Result<ApprovalRequest, StoreError> {
        let decided_unix_ms = now_ms();
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let json: Option<String> = transaction
            .query_row(
                "SELECT request_json FROM approval_requests
                 WHERE approval_id=?1 AND status='pending'",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(json) = json else {
            return Err(StoreError::ApprovalConflict(id.to_owned()));
        };
        let mut request: ApprovalRequest = serde_json::from_str(&json)?;
        if decided_unix_ms >= request.expires_unix_ms {
            transaction.execute(
                "UPDATE approval_requests SET status='timed_out' WHERE approval_id=?1",
                [id],
            )?;
            transaction.commit()?;
            return Err(StoreError::ApprovalExpired(id.to_owned()));
        }
        if !request.can_apply_decision(expected_payload_digest, decided_unix_ms) {
            return Err(StoreError::ApprovalConflict(id.to_owned()));
        }
        request.decision = Some(decision);
        request.decision_source = Some(source);
        request.decided_unix_ms = Some(decided_unix_ms);
        transaction.execute(
            "INSERT INTO approval_decisions(
                approval_id, decision, source, payload_digest, decided_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                serde_json::to_string(&decision)?,
                serde_json::to_string(&source)?,
                expected_payload_digest.to_string(),
                decided_unix_ms,
            ],
        )?;
        transaction.execute(
            "UPDATE approval_requests SET status='resolved', request_json=?2
             WHERE approval_id=?1 AND status='pending'",
            params![id, serde_json::to_string(&request)?],
        )?;
        transaction.commit()?;
        Ok(request)
    }

    pub fn expire_approvals(&self) -> Result<u64, StoreError> {
        let decided_unix_ms = now_ms();
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let expired = {
            let mut statement = transaction.prepare(
                "SELECT approval_id, request_json FROM approval_requests
                 WHERE status='pending' AND expires_unix_ms <= ?1",
            )?;
            let rows = statement.query_map([decided_unix_ms], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut count = 0_u64;
        for (id, json) in expired {
            let mut request: ApprovalRequest = serde_json::from_str(&json)?;
            request.classification = CommandClassification::Expired;
            request.decision = Some(ApprovalDecision::Decline);
            request.decision_source = Some(ApprovalDecisionSource::Timeout);
            request.decided_unix_ms = Some(decided_unix_ms);
            let updated = transaction.execute(
                "UPDATE approval_requests SET status='timed_out', request_json=?2
                 WHERE approval_id=?1 AND status='pending'",
                params![id, serde_json::to_string(&request)?],
            )?;
            if updated == 0 {
                continue;
            }
            transaction.execute(
                "INSERT INTO approval_decisions(
                    approval_id, decision, source, payload_digest, decided_unix_ms
                 ) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![
                    request.id,
                    serde_json::to_string(&ApprovalDecision::Decline)?,
                    serde_json::to_string(&ApprovalDecisionSource::Timeout)?,
                    request.payload_digest.to_string(),
                    decided_unix_ms,
                ],
            )?;
            count = count.saturating_add(1);
        }
        transaction.commit()?;
        Ok(count)
    }

    pub fn record_command_evidence(
        &self,
        attempt_id: Option<Digest>,
        evidence: &CommandExecutionEvidence,
    ) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO command_evidence(
                evidence_id, attempt_id, approval_id, evidence_json, created_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                evidence.id,
                attempt_id.map(|digest| digest.to_string()),
                evidence.approval_id,
                serde_json::to_string(evidence)?,
                now_ms(),
            ],
        )?;
        Ok(())
    }

    pub fn latest_command_evidence(
        &self,
        source_snapshot_digest: Digest,
        plan: &TestPlan,
    ) -> Result<Option<CommandExecutionEvidence>, StoreError> {
        const LOOKBACK_LIMIT: usize = 64;
        let connection = self.connection()?;
        let mut statement = connection.prepare_cached(
            "SELECT evidence_json FROM command_evidence
             ORDER BY created_unix_ms DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([LOOKBACK_LIMIT], |row| row.get::<_, String>(0))?;
        for row in rows {
            let evidence: CommandExecutionEvidence = serde_json::from_str(&row?)?;
            if evidence.source_snapshot_digest == source_snapshot_digest
                && evidence.runner == plan.runner
                && evidence.argv == plan.argv
                && evidence.test_identifier.as_deref() == Some(plan.test_identifier.as_str())
            {
                return Ok(Some(evidence));
            }
        }
        Ok(None)
    }

    pub fn record_semantic_validation_rejection(
        &self,
        request: &ArtifactRequest,
        need: &Need,
        artifact: &needle_core::SemanticWorkerArtifact,
        diagnostic: &str,
    ) -> Result<Digest, StoreError> {
        const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
        let request_id = request.semantic_id().digest();
        let payload = serde_json::to_value(artifact)?;
        let payload_bytes = serde_json::to_vec(&payload)?;
        let payload_digest = Digest::blake3(&payload_bytes);
        let bounded_payload = (payload_bytes.len() <= MAX_PAYLOAD_BYTES).then_some(payload);
        let timestamp = now_ms();
        let mut attempt_hasher =
            needle_core::CanonicalHasher::new(b"semantic-validation-rejection");
        attempt_hasher.field_digest(request_id);
        attempt_hasher.field_digest(payload_digest);
        attempt_hasher.field_bytes(&timestamp.to_le_bytes());
        attempt_hasher.field_bytes(
            &SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_le_bytes(),
        );
        let attempt_id = attempt_hasher.finish();
        let attempt = serde_json::json!({
            "schema": "needle.semantic-validation-rejection/1",
            "request_id": request_id,
            "artifact_kind": artifact.kind(),
            "payload": bounded_payload,
            "payload_digest": payload_digest,
            "diagnostic": bound_diagnostic(diagnostic),
        });
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT OR IGNORE INTO artifact_requests(
                request_id, logical_id, source_digest, contract_id, route_key, request_json,
                created_unix_ms, format_revision, demand_id
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 2, ?8)",
            params![
                request_id.to_string(),
                request.semantic_logical_id().to_string(),
                request.source_snapshot_digest.to_string(),
                request.contract_id,
                request.route_key.as_str(),
                serde_json::to_string(request)?,
                timestamp,
                need.id.to_string(),
            ],
        )?;
        transaction.execute(
            "INSERT INTO execution_attempts(
                attempt_id, request_id, status, attempt_json, started_unix_ms,
                completed_unix_ms
             ) VALUES(?1, ?2, 'semantic_validation_rejected', ?3, ?4, ?4)",
            params![
                attempt_id.to_string(),
                request_id.to_string(),
                serde_json::to_string(&attempt)?,
                timestamp,
            ],
        )?;
        transaction.commit()?;
        Ok(attempt_id)
    }

    pub fn record_worker_run(&self, entry: &NeedCacheEntry) -> Result<(), StoreError> {
        let outcome = &entry.worker_outcome;
        let provenance = match (
            entry.identity.role_profile_provenance.as_ref(),
            outcome.role_profile_provenance.as_ref(),
        ) {
            (Some(identity), Some(outcome)) if identity == outcome => {
                if !self.role_profile_provenance_is_historical(identity)? {
                    return Err(StoreError::ArtifactIdentity(
                        "worker run references an unknown role-profile revision".to_owned(),
                    ));
                }
                Some(identity)
            }
            (None, None) => None,
            _ => {
                return Err(StoreError::ArtifactIdentity(
                    "worker run identity and outcome role-profile provenance differ".to_owned(),
                ));
            }
        };
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO worker_runs(identity_digest, model, reasoning, status, duration_ms,
             input_tokens, cached_input_tokens, output_tokens, result_digest, created_unix_ms,
             failure_code, failure_diagnostic, discarded_facts, logical_worker_spawns,
             worker_turns, repair_performed, worker_session_id, session_cleanup_success,
             role_profile_id, role_profile_revision, role_profile_definition_digest)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, NULL, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                entry.identity.digest().to_string(),
                outcome.worker_model,
                outcome.worker_reasoning,
                outcome.process_status,
                outcome.duration_ms,
                outcome.input_tokens,
                outcome.cached_input_tokens,
                outcome.output_tokens,
                outcome.result.digest()?.to_string(),
                entry.created_unix_ms,
                outcome.discarded_facts,
                outcome.logical_worker_spawns,
                outcome.worker_turns,
                outcome.repair_performed,
                outcome.worker_session_id,
                outcome.session_cleanup_success,
                provenance.map(|value| value.profile_id.as_str()),
                provenance.map(|value| value.revision),
                provenance
                    .as_ref()
                    .map(|value| value.definition_digest.to_string()),
            ],
        )?;
        Ok(())
    }

    pub fn record_worker_failure(
        &self,
        identity: Digest,
        config: &WorkerConfig,
        failure: &WorkerFailure,
    ) -> Result<(), StoreError> {
        let provenance = match (
            config.role_profile_provenance.as_ref(),
            failure.role_profile_provenance.as_ref(),
        ) {
            (Some(config), Some(failure)) if config == failure => {
                if !self.role_profile_provenance_is_historical(config)? {
                    return Err(StoreError::ArtifactIdentity(
                        "worker failure references an unknown role-profile revision".to_owned(),
                    ));
                }
                Some(config)
            }
            (None, None) => None,
            _ => {
                return Err(StoreError::ArtifactIdentity(
                    "worker config and failure role-profile provenance differ".to_owned(),
                ));
            }
        };
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO worker_runs(identity_digest, model, reasoning, status, duration_ms,
             input_tokens, cached_input_tokens, output_tokens, result_digest, created_unix_ms,
             failure_code, failure_diagnostic, discarded_facts, logical_worker_spawns,
             worker_turns, repair_performed, worker_session_id, session_cleanup_success,
             role_profile_id, role_profile_revision, role_profile_definition_digest)
             VALUES(?1, ?2, ?3, 'failed', ?4, ?5, ?6, ?7, NULL, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                identity.to_string(),
                config.model,
                config.reasoning,
                failure.duration_ms,
                failure.input_tokens,
                failure.cached_input_tokens,
                failure.output_tokens,
                now_ms(),
                failure.code,
                bound_diagnostic(&failure.diagnostic),
                failure.discarded_facts,
                failure.logical_worker_spawns,
                failure.worker_turns,
                failure.repair_performed,
                failure.worker_session_id,
                failure.session_cleanup_success,
                provenance.map(|value| value.profile_id.as_str()),
                provenance.map(|value| value.revision),
                provenance.map(|value| value.definition_digest.to_string()),
            ],
        )?;
        Ok(())
    }

    pub fn record_outcome_failure(
        &self,
        identity: Digest,
        config: &WorkerConfig,
        outcome: &WorkerOutcome,
        code: &str,
        diagnostic: &str,
    ) -> Result<(), StoreError> {
        self.record_worker_failure(
            identity,
            config,
            &WorkerFailure {
                code: code.to_owned(),
                diagnostic: diagnostic.to_owned(),
                input_tokens: outcome.input_tokens,
                cached_input_tokens: outcome.cached_input_tokens,
                output_tokens: outcome.output_tokens,
                duration_ms: outcome.duration_ms,
                logical_worker_spawns: outcome.logical_worker_spawns,
                worker_turns: outcome.worker_turns,
                repair_performed: outcome.repair_performed,
                discarded_facts: outcome.discarded_facts,
                worker_session_id: outcome.worker_session_id.clone(),
                session_cleanup_success: outcome.session_cleanup_success,
                role_profile_provenance: outcome.role_profile_provenance.clone(),
            },
        )
    }

    pub fn acquire_lease(
        &self,
        identity: Digest,
        owner: &str,
        expires_unix_ms: u64,
    ) -> Result<bool, StoreError> {
        let mut connection = self.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM worker_leases WHERE expires_unix_ms <= ?1", [now_ms()])?;
        let acquired = transaction.execute(
            "INSERT OR IGNORE INTO worker_leases(identity_digest, owner, expires_unix_ms) VALUES(?1, ?2, ?3)",
            params![identity.to_string(), owner, expires_unix_ms],
        )? == 1;
        transaction.commit()?;
        Ok(acquired)
    }

    pub fn release_lease(&self, identity: Digest, owner: &str) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "DELETE FROM worker_leases WHERE identity_digest=?1 AND owner=?2",
            params![identity.to_string(), owner],
        )?;
        Ok(())
    }

    pub fn cache_records(&self) -> Result<Vec<CacheRecord>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT identity_digest, logical_digest, source_digest, created_unix_ms, hit_count
             FROM cache_entries ORDER BY created_unix_ms DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, u64>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (identity, logical, source, created_unix_ms, hit_count) = row?;
            Ok(CacheRecord {
                identity_digest: parse_digest(&identity)?,
                logical_digest: parse_digest(&logical)?,
                source_digest: parse_digest(&source)?,
                created_unix_ms,
                hit_count,
            })
        })
        .collect()
    }

    pub fn artifacts(&self) -> Result<Vec<Artifact>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT artifact_json FROM artifacts ORDER BY created_unix_ms DESC LIMIT 200",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            let json = row?;
            serde_json::from_str(&json).map_err(StoreError::from)
        })
        .collect()
    }

    pub fn execution_attempt_count(&self) -> Result<u64, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row("SELECT COUNT(*) FROM execution_attempts", [], |row| row.get(0))
            .map_err(StoreError::from)
    }

    pub fn semantic_validation_rejection_count(&self) -> Result<u64, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT COUNT(*) FROM execution_attempts
                 WHERE status='semantic_validation_rejected'",
                [],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    pub fn command_evidence_count(&self) -> Result<u64, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row("SELECT COUNT(*) FROM command_evidence", [], |row| row.get(0))
            .map_err(StoreError::from)
    }

    pub fn worker_run_count(&self) -> Result<u64, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row("SELECT COUNT(*) FROM worker_runs", [], |row| row.get(0))
            .map_err(StoreError::from)
    }

    pub fn latest_worker_run(&self) -> Result<Option<WorkerRunRecord>, StoreError> {
        let connection = self.connection()?;
        let value = connection
            .query_row(
                "SELECT input_tokens, cached_input_tokens, output_tokens, result_digest,
                 failure_code, failure_diagnostic, discarded_facts, logical_worker_spawns,
                 worker_turns, repair_performed, worker_session_id, session_cleanup_success,
                 role_profile_id, role_profile_revision, role_profile_definition_digest
                 FROM worker_runs ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<u64>>(0)?,
                        row.get::<_, Option<u64>>(1)?,
                        row.get::<_, Option<u64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, u32>(6)?,
                        row.get::<_, u32>(7)?,
                        row.get::<_, u32>(8)?,
                        row.get::<_, bool>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, Option<bool>>(11)?,
                        row.get::<_, Option<String>>(12)?,
                        row.get::<_, Option<u64>>(13)?,
                        row.get::<_, Option<String>>(14)?,
                    ))
                },
            )
            .optional()?;
        value
            .map(
                |(
                    input_tokens,
                    cached_input_tokens,
                    output_tokens,
                    result_digest,
                    failure_code,
                    failure_diagnostic,
                    discarded_facts,
                    logical_worker_spawns,
                    worker_turns,
                    repair_performed,
                    worker_session_id,
                    session_cleanup_success,
                    role_profile_id,
                    role_profile_revision,
                    role_profile_definition_digest,
                )| {
                    let role_profile_provenance = parse_role_profile_provenance((
                        role_profile_id,
                        role_profile_revision,
                        role_profile_definition_digest,
                    ))?;
                    Ok(WorkerRunRecord {
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                        result_digest: result_digest.as_deref().map(parse_digest).transpose()?,
                        failure_code,
                        failure_diagnostic,
                        discarded_facts,
                        logical_worker_spawns,
                        worker_turns,
                        repair_performed,
                        worker_session_id,
                        session_cleanup_success,
                        role_profile_provenance,
                    })
                },
            )
            .transpose()
    }

    pub fn worker_runs_after(
        &self,
        previous_count: u64,
    ) -> Result<Vec<WorkerRunRecord>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT input_tokens, cached_input_tokens, output_tokens, result_digest,
             failure_code, failure_diagnostic, discarded_facts, logical_worker_spawns,
             worker_turns, repair_performed, worker_session_id, session_cleanup_success,
             role_profile_id, role_profile_revision, role_profile_definition_digest
             FROM worker_runs ORDER BY id LIMIT -1 OFFSET ?1",
        )?;
        let rows = statement.query_map([previous_count], |row| {
            Ok((
                row.get::<_, Option<u64>>(0)?,
                row.get::<_, Option<u64>>(1)?,
                row.get::<_, Option<u64>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, u32>(7)?,
                row.get::<_, u32>(8)?,
                row.get::<_, bool>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<bool>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<u64>>(13)?,
                row.get::<_, Option<String>>(14)?,
            ))
        })?;
        rows.map(|row| {
            let (
                input_tokens,
                cached_input_tokens,
                output_tokens,
                result_digest,
                failure_code,
                failure_diagnostic,
                discarded_facts,
                logical_worker_spawns,
                worker_turns,
                repair_performed,
                worker_session_id,
                session_cleanup_success,
                role_profile_id,
                role_profile_revision,
                role_profile_definition_digest,
            ) = row?;
            let role_profile_provenance = parse_role_profile_provenance((
                role_profile_id,
                role_profile_revision,
                role_profile_definition_digest,
            ))?;
            Ok(WorkerRunRecord {
                input_tokens,
                cached_input_tokens,
                output_tokens,
                result_digest: result_digest.as_deref().map(parse_digest).transpose()?,
                failure_code,
                failure_diagnostic,
                discarded_facts,
                logical_worker_spawns,
                worker_turns,
                repair_performed,
                worker_session_id,
                session_cleanup_success,
                role_profile_provenance,
            })
        })
        .collect()
    }

    pub fn pending_worker_sessions(&self) -> Result<Vec<String>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT worker_session_id FROM worker_runs
             WHERE worker_session_id IS NOT NULL
               AND COALESCE(session_cleanup_success, 0)=0
             ORDER BY worker_session_id",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
    }

    pub fn mark_worker_session_cleaned(&self, session_id: &str) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE worker_runs SET session_cleanup_success=1
             WHERE worker_session_id=?1",
            [session_id],
        )?;
        Ok(())
    }

    pub fn cache_entry(&self, identity: Digest) -> Result<Option<NeedCacheEntry>, StoreError> {
        let connection = self.connection()?;
        let json: Option<String> = connection
            .query_row(
                "SELECT entry_json FROM cache_entries WHERE identity_digest=?1",
                [identity.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|value| serde_json::from_str(&value).map_err(StoreError::from)).transpose()
    }

    pub fn invalidate_cache(&self, identity: Digest) -> Result<bool, StoreError> {
        let connection = self.connection()?;
        Ok(connection.execute(
            "DELETE FROM cache_entries WHERE identity_digest=?1",
            [identity.to_string()],
        )? == 1)
    }

    pub fn invalidate_all_cache(&self) -> Result<u64, StoreError> {
        let connection = self.connection()?;
        connection
            .execute("DELETE FROM cache_entries", [])
            .map(|count| count.try_into().unwrap_or(u64::MAX))
            .map_err(StoreError::from)
    }

    pub fn export_toml(&self) -> Result<String, StoreError> {
        let export = ConfigExport {
            format_revision: needle_core::FORMAT_REVISION,
            settings: self.settings()?,
            presets: self.json_rows("SELECT definition_json FROM presets ORDER BY id")?,
            routes: self.routes()?,
            model_policy: Some(self.model_policy()?),
        };
        Ok(toml::to_string_pretty(&export)?)
    }

    pub fn import_toml(&self, input: &str) -> Result<(), StoreError> {
        let export: ConfigExport = toml::from_str(input)?;
        if export.format_revision != needle_core::FORMAT_REVISION {
            return Err(StoreError::MigrationChecksum);
        }
        for preset in &export.presets {
            if !preset.has_valid_definition_digest() {
                return Err(StoreError::DefinitionDigest(preset.id.clone()));
            }
        }
        for route in &export.routes {
            if !route.has_valid_definition_digest() {
                return Err(StoreError::DefinitionDigest(route.id.clone()));
            }
            if !export.presets.iter().any(|preset| preset.id == route.preset_id) {
                return Err(StoreError::DefinitionDigest(route.id.clone()));
            }
        }
        self.initialize()?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        for (key, value) in [
            ("codex_executable", export.settings.codex_executable.as_str()),
            ("worker_model", export.settings.worker_model.as_str()),
            ("worker_reasoning", export.settings.worker_reasoning.as_str()),
            ("evidence_failure_policy", export.settings.evidence_failure_policy.as_str()),
        ] {
            transaction.execute(
                "INSERT INTO settings(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            )?;
        }
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES('worker_timeout_seconds', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [export.settings.worker_timeout_seconds.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES('trusted_test_execution', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [if export.settings.trusted_test_execution { "1" } else { "0" }],
        )?;
        let model_policy = export.model_policy.unwrap_or_else(|| ModelPolicy::FixedOrder {
            profiles: vec![WorkerProfile::new(
                "codex",
                export.settings.worker_model.clone(),
                export.settings.worker_reasoning.clone(),
                None,
            )],
            repair_once: true,
            native_fallback: true,
        });
        let profiles = match &model_policy {
            ModelPolicy::FixedOrder { profiles, .. } => profiles,
            ModelPolicy::CheapestValidatedFirst { promoted_profiles, .. } => promoted_profiles,
        };
        if profiles.is_empty() || profiles.iter().any(|profile| profile.platform != "codex") {
            return Err(StoreError::DefinitionDigest("model_policy".to_owned()));
        }
        transaction.execute(
            "INSERT INTO settings(key, value) VALUES('model_policy', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [serde_json::to_string(&model_policy)?],
        )?;
        transaction.execute("DELETE FROM routes", [])?;
        transaction.execute("DELETE FROM presets", [])?;
        for preset in export.presets {
            transaction.execute(
                "INSERT OR REPLACE INTO presets(id, definition_digest, definition_json) VALUES(?1, ?2, ?3)",
                params![preset.id, preset.definition_digest.to_string(), serde_json::to_string(&preset)?],
            )?;
        }
        for route in export.routes {
            transaction.execute(
                "INSERT OR REPLACE INTO routes(id, enabled, priority, definition_digest, definition_json) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![route.id, route.enabled, route.priority, route.definition_digest.to_string(), serde_json::to_string(&route)?],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn connection(&self) -> Result<ConnectionGuard<'_>, StoreError> {
        let mut guard = self.connection.lock().map_err(|_| StoreError::ConnectionLock)?;
        if guard.is_none() {
            let connection = Connection::open(&self.path)?;
            connection.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 PRAGMA foreign_keys=ON;
                 PRAGMA trusted_schema=OFF;
                 PRAGMA busy_timeout=5000;",
            )?;
            *guard = Some(connection);
        }
        Ok(ConnectionGuard(guard))
    }

    fn json_rows<T: serde::de::DeserializeOwned>(&self, query: &str) -> Result<Vec<T>, StoreError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(query)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
}

fn parse_digest(value: &str) -> Result<Digest, StoreError> {
    Digest::parse(value).map_err(|error| StoreError::Digest(error.to_string()))
}

fn parse_role_profile_provenance(
    value: (Option<String>, Option<u64>, Option<String>),
) -> Result<Option<RoleProfileProvenance>, StoreError> {
    let (profile_id, revision, digest) = value;
    match (profile_id, revision, digest) {
        (None, None, None) => Ok(None),
        (Some(profile_id), Some(revision), Some(digest)) => {
            let profile_id = RoleProfileId::new(profile_id)
                .map_err(|error| StoreError::RoleProfileCorruption(error.to_string()))?;
            let digest = Digest::parse(&digest)
                .map_err(|error| StoreError::RoleProfileCorruption(error.to_string()))?;
            RoleProfileProvenance::new(profile_id, revision, digest)
                .map(Some)
                .map_err(|error| StoreError::RoleProfileCorruption(error.to_string()))
        }
        _ => Err(StoreError::RoleProfileCorruption(
            "role-profile provenance columns are partially populated".to_owned(),
        )),
    }
}

fn apply_migration(
    connection: &mut Connection,
    version: u32,
    migration: &str,
) -> Result<(), StoreError> {
    let checksum = Digest::blake3(migration).to_string();
    let existing: Option<String> = connection
        .query_row("SELECT checksum FROM schema_migrations WHERE version=?1", [version], |row| {
            row.get(0)
        })
        .optional()?;
    if existing.as_deref().is_some_and(|value| value != checksum) {
        return Err(StoreError::MigrationChecksum);
    }
    if existing.is_none() {
        let transaction = connection.transaction()?;
        transaction.execute_batch(migration)?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, checksum, applied_unix_ms)
             VALUES(?1, ?2, ?3)",
            params![version, checksum, now_ms()],
        )?;
        transaction.commit()?;
    }
    Ok(())
}

fn parse_evidence_failure_policy(value: &str) -> Result<EvidenceFailurePolicy, StoreError> {
    match value {
        "discard_invalid_fact" => Ok(EvidenceFailurePolicy::DiscardInvalidFact),
        "repair_once" => Ok(EvidenceFailurePolicy::RepairOnce),
        _ => Err(StoreError::EvidenceFailurePolicy(value.to_owned())),
    }
}

fn capability_mode_name(mode: CapabilityMode) -> &'static str {
    match mode {
        CapabilityMode::Disabled => "disabled",
        CapabilityMode::Shadow => "shadow",
        CapabilityMode::Advisory => "advisory",
        CapabilityMode::Authoritative => "authoritative",
    }
}

fn parse_capability_mode(value: &str) -> Result<CapabilityMode, StoreError> {
    match value {
        "disabled" => Ok(CapabilityMode::Disabled),
        "shadow" => Ok(CapabilityMode::Shadow),
        "advisory" => Ok(CapabilityMode::Advisory),
        "authoritative" => Ok(CapabilityMode::Authoritative),
        _ => Err(StoreError::DefinitionDigest(format!("invalid capability mode `{value}`"))),
    }
}

fn legacy_need_grammar_definition_digest() -> Digest {
    Digest::blake3(b"needle.need-grammar/legacy-v0.3")
}

fn need_relation_name(relation: NeedStepRelation) -> &'static str {
    match relation {
        NeedStepRelation::Repeat => "repeat",
        NeedStepRelation::Residual => "residual",
        NeedStepRelation::Extension => "extension",
        NeedStepRelation::Overlap => "overlap",
        NeedStepRelation::Independent => "independent",
        NeedStepRelation::Incompatible => "incompatible",
    }
}

fn need_step_state_name(state: NeedStepState) -> &'static str {
    match state {
        NeedStepState::Requested => "requested",
        NeedStepState::Queued => "queued",
        NeedStepState::Resolving => "resolving",
        NeedStepState::Resolved => "resolved",
        NeedStepState::Delivered => "delivered",
        NeedStepState::NativeFallback => "native_fallback",
        NeedStepState::Failed => "failed",
        NeedStepState::Cancelled => "cancelled",
    }
}

fn parse_need_step_state(value: &str) -> Result<NeedStepState, StoreError> {
    match value {
        "requested" => Ok(NeedStepState::Requested),
        "queued" => Ok(NeedStepState::Queued),
        "resolving" => Ok(NeedStepState::Resolving),
        "resolved" => Ok(NeedStepState::Resolved),
        "delivered" => Ok(NeedStepState::Delivered),
        "native_fallback" => Ok(NeedStepState::NativeFallback),
        "failed" => Ok(NeedStepState::Failed),
        "cancelled" => Ok(NeedStepState::Cancelled),
        _ => Err(StoreError::DefinitionDigest("need_step_state".to_owned())),
    }
}

fn append_need_step_event(
    connection: &Connection,
    step_id: Digest,
    state: NeedStepState,
    event_json: &str,
) -> Result<(), StoreError> {
    connection.execute(
        "INSERT INTO need_step_events(need_step_id, state, event_json, created_unix_ms)
         VALUES(?1, ?2, ?3, ?4)",
        params![step_id.to_string(), need_step_state_name(state), event_json, now_ms()],
    )?;
    Ok(())
}

fn bound_diagnostic(value: &str) -> String {
    const MAXIMUM: usize = 4096;
    let mut end = value.len().min(MAXIMUM);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub fn route_set_digest(routes: &[Route]) -> Digest {
    let mut routes = routes.iter().collect::<Vec<_>>();
    routes.sort_by(|left, right| left.id.as_bytes().cmp(right.id.as_bytes()));
    let mut canonical = String::from("needle-route-set\n");
    for route in routes {
        canonical.push_str(&route.id);
        canonical.push('\n');
        canonical.push_str(if route.enabled { "enabled\n" } else { "disabled\n" });
        canonical.push_str(&route.definition_digest.to_string());
        canonical.push('\n');
    }
    Digest::blake3(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{
        ApprovalDecision, ApprovalDecisionSource, ApprovalRequest, Artifact, ArtifactContract,
        ArtifactKind, ArtifactRequest, CacheResolution, CacheScope, CommandClassification,
        DependencyManifest, NeedDelivery, NeedId, NeedKey, NeedStep, NeedStepRelation,
        NeedStepState, ObligationId, RequestedPermissions, ReuseUnit, SemanticLocation,
        SemanticWorkerArtifact, SemanticWorld,
    };

    fn temporary_store() -> (PathBuf, RuntimeStore) {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "needle-runtime-store-{}-{}.sqlite3",
            std::process::id(),
            nanos
        ));
        (path.clone(), RuntimeStore::new(path))
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let (path, store) = temporary_store();
        let settings = RuntimeSettings {
            codex_executable: "codex".to_owned(),
            worker_model: "worker".to_owned(),
            worker_reasoning: "medium".to_owned(),
            worker_timeout_seconds: 180,
            evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
            trusted_test_execution: false,
            multi_need_policy: MultiNeedPolicy::default(),
        };
        store.initialize_defaults(&settings).unwrap();
        assert_eq!(store.routes().unwrap().len(), 3);
        let connection = Connection::open(&path).unwrap();
        let immutable_plans: u32 = connection
            .query_row("SELECT COUNT(*) FROM definitions WHERE kind='route_plan'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(immutable_plans, 3);
        drop(connection);
        let exported = store.export_toml().unwrap();
        assert!(exported.contains("trace.state-flow"));
        assert!(exported.contains("evidence_failure_policy = \"discard_invalid_fact\""));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn command_evidence_lookup_is_bounded_to_the_exact_snapshot_and_plan() {
        let (path, store) = temporary_store();
        store
            .initialize_defaults(&RuntimeSettings {
                codex_executable: "codex.exe".to_owned(),
                worker_model: "worker".to_owned(),
                worker_reasoning: "medium".to_owned(),
                worker_timeout_seconds: 30,
                evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
                trusted_test_execution: true,
                multi_need_policy: MultiNeedPolicy::default(),
            })
            .unwrap();
        let plan = TestPlan {
            runner: "cargo".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned(), "focused".to_owned()],
            cwd_relative: ".".to_owned(),
            test_identifier: "focused".to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        let snapshot = Digest::blake3(b"snapshot");
        let evidence = CommandExecutionEvidence {
            id: "command-evidence-exact".to_owned(),
            approval_id: "approval".to_owned(),
            argv: plan.argv.clone(),
            cwd: ".".to_owned(),
            source_snapshot_digest: snapshot,
            runner: "cargo".to_owned(),
            runner_version: None,
            exit_status: Some(0),
            duration_ms: 1,
            output_digest: Digest::blake3(b"output"),
            output_preview: "test focused ... ok\ntest result: ok. 1 passed".to_owned(),
            test_identifier: Some("focused".to_owned()),
            tests_executed: Some(1),
            infrastructure_failure: None,
        };
        store.record_command_evidence(None, &evidence).unwrap();
        assert_eq!(
            store.latest_command_evidence(snapshot, &plan).unwrap().unwrap().id,
            evidence.id
        );
        assert!(
            store
                .latest_command_evidence(Digest::blake3(b"different-snapshot"), &plan)
                .unwrap()
                .is_none()
        );
        drop(store);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejected_semantic_payload_is_persisted_as_a_bounded_execution_attempt() {
        let (path, store) = temporary_store();
        store
            .initialize_defaults(&RuntimeSettings {
                codex_executable: "codex.exe".to_owned(),
                worker_model: "worker".to_owned(),
                worker_reasoning: "medium".to_owned(),
                worker_timeout_seconds: 30,
                evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
                trusted_test_execution: false,
                multi_need_policy: MultiNeedPolicy::default(),
            })
            .unwrap();
        let repository_id = Digest::blake3(b"repo");
        let request = ArtifactRequest {
            contract_id: "needle.semantic.code-location".to_owned(),
            contract_revision: 2,
            repository_id,
            source_snapshot_digest: Digest::blake3(b"snapshot"),
            route_key: NeedKey::new("locate.implementation").unwrap(),
            normalized_request: "locate".to_owned(),
            semantic_fragment_id: None,
            input_artifact_ids: Vec::new(),
        };
        let need = Need {
            id: NeedId(Digest::blake3(b"need")),
            subjects: Vec::new(),
            required: Vec::new(),
            preferred: Vec::new(),
            semantic_constraints: Vec::new(),
            world: SemanticWorld {
                repository_lineage: repository_id,
                source_selector: "current".to_owned(),
                platform: "current".to_owned(),
                features: "default".to_owned(),
                configuration: None,
                toolchain: None,
            },
            input_artifacts: Vec::new(),
            residual: None,
            body_digest: Digest::blake3(b"body"),
            format_revision: 1,
        };
        let artifact = SemanticWorkerArtifact::CodeLocation {
            locations: vec![SemanticLocation {
                role: needle_core::LocationRole::Primary,
                path: "src/lib.rs".to_owned(),
                symbol: Some("answer".to_owned()),
                byte_start: None,
                byte_end: None,
            }],
            gaps: Vec::new(),
        };
        store
            .record_semantic_validation_rejection(&request, &need, &artifact, &"x".repeat(10_000))
            .unwrap();
        assert_eq!(store.execution_attempt_count().unwrap(), 1);
        assert_eq!(store.semantic_validation_rejection_count().unwrap(), 1);
        let connection = store.connection().unwrap();
        let (status, json): (String, String) = connection
            .query_row("SELECT status, attempt_json FROM execution_attempts LIMIT 1", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "semantic_validation_rejected");
        assert!(json.len() < 16 * 1024);
        assert!(json.contains("needle.semantic-validation-rejection/1"));
        drop(connection);
        drop(store);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn session_keeps_its_initial_prompt_route_and_grammar_definitions() {
        let (path, store) = temporary_store();
        let settings = RuntimeSettings {
            codex_executable: "codex".to_owned(),
            worker_model: "worker".to_owned(),
            worker_reasoning: "medium".to_owned(),
            worker_timeout_seconds: 180,
            evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
            trusted_test_execution: false,
            multi_need_policy: MultiNeedPolicy::default(),
        };
        store.initialize_defaults(&settings).unwrap();
        let initial = Digest::blake3(b"initial-profile");
        store.record_session_start("session", initial, Some("main"), Some("one")).unwrap();
        store
            .record_session_start(
                "session",
                Digest::blake3(b"changed-profile"),
                Some("other"),
                Some("two"),
            )
            .unwrap();
        let session = store.session("session").unwrap().unwrap();
        assert_eq!(session.prompt_profile_digest, initial);
        assert_eq!(session.need_grammar_digest, needle_core::need_grammar_definition_digest());
        assert_eq!(session.transport.as_deref(), Some("hook"));
        assert_eq!(
            session.semantic_definition_digest,
            Some(needle_core::need_ir_definition_digest())
        );
        assert_eq!(session.cwd.as_deref(), Some("one"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn migrated_session_without_grammar_digest_stays_legacy() {
        let (path, store) = temporary_store();
        store.initialize().unwrap();
        store
            .record_session_start("legacy-session", Digest::blake3(b"profile"), None, None)
            .unwrap();
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE sessions SET need_grammar_digest=NULL, semantic_definition_digest=NULL
                 WHERE session_id='legacy-session'",
                [],
            )
            .unwrap();

        let session = store.session("legacy-session").unwrap().unwrap();
        assert_eq!(session.need_grammar_digest, legacy_need_grammar_definition_digest());
        assert_ne!(session.need_grammar_digest, needle_core::need_grammar_definition_digest());
        assert_eq!(session.semantic_definition_digest, None);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn fresh_database_applies_all_checksummed_migrations() {
        let (path, store) = temporary_store();
        store.initialize().unwrap();
        let connection = Connection::open(&path).unwrap();
        let versions = connection
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .unwrap()
            .query_map([], |row| row.get::<_, u32>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(versions, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        let columns = connection
            .prepare("PRAGMA table_info(worker_runs)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(columns.contains(&"failure_code".to_owned()));
        assert!(columns.contains(&"worker_turns".to_owned()));
        assert!(columns.contains(&"session_cleanup_success".to_owned()));
        let v3_tables = connection
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type='table' AND name IN (
                    'artifacts', 'artifact_requests', 'approval_requests',
                    'approval_decisions', 'command_evidence'
                 ) ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(v3_tables.len(), 5);
        let v5_tables: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name IN (
                    'need_ir_records', 'needs', 'need_fragments', 'need_obligations',
                    'subjects', 'predicate_contracts', 'route_contracts',
                    'capability_classes'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v5_tables, 8);
        let proof_tables: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name IN (
                    'artifact_validation_certificates', 'coverage_entries',
                    'reuse_sufficiency_certificates', 'selected_plans',
                    'proof_accounting'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(proof_tables, 5);
        let v10_tables: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name='need_step_requests'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v10_tables, 1);
        let session_columns = connection
            .prepare("PRAGMA table_info(sessions)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for column in ["transport", "transport_definition_digest", "semantic_definition_digest"] {
            assert!(session_columns.iter().any(|value| value == column));
        }
        let request_columns = connection
            .prepare("PRAGMA table_info(need_step_requests)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(request_columns.iter().any(|value| value == "transport"));
        assert!(request_columns.iter().any(|value| value == "request_format"));
        let v12_tables: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name IN (
                    'semantic_claims', 'claim_origins', 'claim_relations',
                    'claim_dependencies', 'claim_validation_certificates',
                    'claim_coverage_entries', 'claim_set_certificates',
                    'claim_set_members', 'claim_contradiction_members',
                    'operator_cost_observations'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v12_tables, 10);
        let v13_tables: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name IN (
                    'change_requests', 'change_events', 'patch_artifacts',
                    'patch_files', 'verification_artifacts', 'change_attempts',
                    'change_applies'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v13_tables, 7);
        let v16_tables: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type='table' AND name='change_lifecycles'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v16_tables, 1);
        let event_columns = connection
            .prepare("PRAGMA table_info(change_events)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(event_columns.iter().any(|column| column == "lifecycle_sequence"));
        drop(connection);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn operator_cost_observations_are_single_kind_and_profile_exact() {
        let (path, store) = temporary_store();
        store.initialize().unwrap();
        let schema_digest = Digest::blake3(b"schema");
        let validator_digest = Digest::blake3(b"validator");
        let pricing_digest = Digest::blake3(b"pricing");
        let observation = OperatorCostObservation {
            artifact_kind: "test-plan".to_owned(),
            worker_model: "worker".to_owned(),
            worker_reasoning: "medium".to_owned(),
            service_tier: "default".to_owned(),
            schema_digest,
            validator_definition_digest: validator_digest,
            pricing_digest,
            requested_kind_count: 1,
            cost_microusd: 123,
            execution_attempt_id: None,
            evidence_digest: Digest::blake3(b"operator-cost-evidence"),
            observed_unix_ms: 10,
        };
        store.record_operator_cost_observation(&observation).unwrap();
        assert_eq!(
            store
                .observed_operator_cost(&OperatorCostKey {
                    artifact_kind: "test-plan",
                    worker_model: "worker",
                    worker_reasoning: "medium",
                    service_tier: "default",
                    schema_digest,
                    validator_definition_digest: validator_digest,
                    pricing_digest,
                })
                .unwrap(),
            Some(123)
        );
        assert_eq!(
            store
                .observed_operator_cost(&OperatorCostKey {
                    artifact_kind: "test-plan",
                    worker_model: "different-worker",
                    worker_reasoning: "medium",
                    service_tier: "default",
                    schema_digest,
                    validator_definition_digest: validator_digest,
                    pricing_digest,
                })
                .unwrap(),
            None
        );

        let mut invalid = observation;
        invalid.requested_kind_count = 2;
        invalid.evidence_digest = Digest::blake3(b"invalid-operator-cost-evidence");
        assert!(matches!(
            store.record_operator_cost_observation(&invalid),
            Err(StoreError::OperatorCostObservation(_))
        ));
        drop(store);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn multi_need_policy_is_session_immutable_and_steps_are_append_only() {
        let (path, store) = temporary_store();
        let settings = RuntimeSettings {
            codex_executable: "codex.exe".to_owned(),
            worker_model: "worker".to_owned(),
            worker_reasoning: "medium".to_owned(),
            worker_timeout_seconds: 30,
            evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
            trusted_test_execution: false,
            multi_need_policy: MultiNeedPolicy::default(),
        };
        store.initialize_defaults(&settings).unwrap();
        store
            .record_session_start("multi", Digest::blake3(b"profile"), Some("main"), Some("."))
            .unwrap();
        let mut updated = settings;
        updated.multi_need_policy.max_needs_per_task = 5;
        store.set_runtime_settings(&updated).unwrap();
        let session = store.session("multi").unwrap().unwrap();
        assert_eq!(session.multi_need_policy.max_needs_per_task, 3);
        assert_eq!(session.multi_need_policy_digest, MultiNeedPolicy::default().digest());

        let required = ObligationId(Digest::blake3(b"required"));
        let step = NeedStep {
            id: Digest::blake3(b"step"),
            ordinal: 1,
            turn_id: "turn-1".to_owned(),
            need_id: NeedId(Digest::blake3(b"need")),
            coordination: needle_core::NeedCoordination::WaitResponse,
            relation: NeedStepRelation::Independent,
            state: NeedStepState::Requested,
            required: vec![required],
            satisfied: Vec::new(),
            missing: vec![required],
            artifacts: Vec::new(),
            proof: None,
            delivery: Some(NeedDelivery::TurnStart),
            worker_avoided: false,
            main_discovery_tainted: false,
        };
        let raw_message = concat!(
            "@@need\n",
            "@route locate.implementation\n",
            "@subject cli-option:\"--glob-case-insensitive\"\n",
            "@require implementation-location selection=primary granularity=exact-location\n",
            "@world source=current features=default\n",
            "@project detail=compact\n",
            "\n",
            "Locate the implementation.\n",
            "@@end"
        );
        let semantic_interrupt = SemanticInterrupt::parse(raw_message).unwrap().unwrap();
        store.record_need_step("multi", &step, &semantic_interrupt, raw_message).unwrap();
        store.append_need_step_event(step.id, NeedStepState::Resolving, "{}").unwrap();
        store.append_need_step_event(step.id, NeedStepState::Delivered, "{}").unwrap();
        let restored = store.need_steps("multi").unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].state, NeedStepState::Delivered);
        assert_eq!(restored[0].missing, vec![required]);
        assert_eq!(store.need_step(step.id).unwrap(), Some(restored[0].clone()));
        let request = store.need_step_request(step.id).unwrap().unwrap();
        assert_eq!(request.request_digest, semantic_interrupt.digest());
        assert_eq!(request.raw_message, raw_message);
        assert_eq!(request.semantic_interrupt, Some(semantic_interrupt));
        assert_eq!(request.transport.as_deref(), Some("hook"));
        assert_eq!(request.request_format.as_deref(), Some("need_ir_text"));
        let event_records = store.need_step_events(Some("multi"), 0, 10).unwrap();
        assert_eq!(event_records.len(), 3);
        assert_eq!(event_records[2].state, NeedStepState::Delivered);
        store
            .record_main_turn_observation(&MainTurnObservationRecord {
                session_id: "multi".to_owned(),
                turn_id: "turn-1".to_owned(),
                need_step_id: Some(step.id),
                status: "delivered".to_owned(),
                delivery: Some("turn_start".to_owned()),
                usage_json: r#"{"input_tokens":10}"#.to_owned(),
                tools_json: r#"{"started":0}"#.to_owned(),
                main_discovery_tainted: false,
                outcome: None,
            })
            .unwrap();
        let observations = store.main_turn_observations("multi").unwrap();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].need_step_id, Some(step.id));
        store.end_session("multi").unwrap();
        assert!(store.session("multi").unwrap().is_some());
        assert!(store.session("multi").unwrap().unwrap().root_task.is_none());
        assert_eq!(store.need_steps("multi").unwrap().len(), 1);
        let events: u32 = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM need_step_events WHERE need_step_id=?1",
                [step.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events, 3);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn existing_v1_database_migrates_without_changing_v1_checksum() {
        let (path, store) = temporary_store();
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(MIGRATION_V1).unwrap();
        let v1_checksum = Digest::blake3(MIGRATION_V1).to_string();
        connection
            .execute(
                "INSERT INTO schema_migrations(version, checksum, applied_unix_ms)
                 VALUES(1, ?1, 1)",
                [&v1_checksum],
            )
            .unwrap();
        drop(connection);
        store.initialize().unwrap();
        let connection = Connection::open(&path).unwrap();
        let stored: String = connection
            .query_row("SELECT checksum FROM schema_migrations WHERE version=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored, v1_checksum);
        let v2: u32 = connection
            .query_row("SELECT version FROM schema_migrations WHERE version=2", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(v2, 2);
        drop(connection);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn evidence_policy_changes_worker_cache_identity() {
        let base = WorkerConfig {
            executable: "codex".to_owned(),
            model: "gpt-5.6-luna".to_owned(),
            reasoning: "medium".to_owned(),
            service_tier: None,
            timeout_seconds: 180,
            evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
            role_profile_provenance: None,
        };
        let mut repaired = base.clone();
        repaired.evidence_failure_policy = EvidenceFailurePolicy::RepairOnce;
        assert_ne!(base.digest(), repaired.digest());
        let mut priority = base.clone();
        priority.service_tier = Some("priority".to_owned());
        assert_ne!(base.digest(), priority.digest());
    }

    #[test]
    fn runtime_settings_update_is_validated_and_atomic() {
        let (path, store) = temporary_store();
        let initial = RuntimeSettings {
            codex_executable: "codex".to_owned(),
            worker_model: "gpt-5.6-luna".to_owned(),
            worker_reasoning: "medium".to_owned(),
            worker_timeout_seconds: 180,
            evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
            trusted_test_execution: false,
            multi_need_policy: MultiNeedPolicy::default(),
        };
        store.initialize_defaults(&initial).unwrap();
        let mut updated = initial.clone();
        updated.worker_model = "gpt-5.6-sol".to_owned();
        updated.worker_reasoning = "high".to_owned();
        updated.worker_timeout_seconds = 240;
        updated.evidence_failure_policy = EvidenceFailurePolicy::RepairOnce;
        updated.trusted_test_execution = true;
        store.set_runtime_settings(&updated).unwrap();
        assert_eq!(store.settings().unwrap(), updated);

        let mut invalid = updated.clone();
        invalid.worker_model = "model with spaces".to_owned();
        assert!(store.set_runtime_settings(&invalid).is_err());
        assert_eq!(store.settings().unwrap(), updated);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn stale_worker_session_can_be_recovered() {
        let (path, store) = temporary_store();
        store.initialize().unwrap();
        let config = WorkerConfig {
            executable: "codex".to_owned(),
            model: "gpt-5.6-luna".to_owned(),
            reasoning: "medium".to_owned(),
            service_tier: None,
            timeout_seconds: 180,
            evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
            role_profile_provenance: None,
        };
        store
            .record_worker_failure(
                Digest::blake3("identity"),
                &config,
                &WorkerFailure {
                    code: "cleanup".to_owned(),
                    diagnostic: "cleanup pending".to_owned(),
                    input_tokens: Some(10),
                    cached_input_tokens: Some(5),
                    output_tokens: Some(2),
                    duration_ms: 1,
                    logical_worker_spawns: 1,
                    worker_turns: 2,
                    repair_performed: true,
                    discarded_facts: 3,
                    worker_session_id: Some("session-1".to_owned()),
                    session_cleanup_success: Some(false),
                    role_profile_provenance: None,
                },
            )
            .unwrap();
        store
            .record_worker_failure(
                Digest::blake3("legacy-null-cleanup"),
                &config,
                &WorkerFailure {
                    code: "cleanup".to_owned(),
                    diagnostic: "cleanup outcome was not recorded".to_owned(),
                    input_tokens: Some(10),
                    cached_input_tokens: Some(5),
                    output_tokens: Some(2),
                    duration_ms: 1,
                    logical_worker_spawns: 1,
                    worker_turns: 2,
                    repair_performed: true,
                    discarded_facts: 3,
                    worker_session_id: Some("session-2".to_owned()),
                    session_cleanup_success: None,
                    role_profile_provenance: None,
                },
            )
            .unwrap();
        assert_eq!(store.pending_worker_sessions().unwrap(), vec!["session-1", "session-2"]);
        store.mark_worker_session_cleaned("session-1").unwrap();
        store.mark_worker_session_cleaned("session-2").unwrap();
        assert!(store.pending_worker_sessions().unwrap().is_empty());
        let run = store.latest_worker_run().unwrap().unwrap();
        assert_eq!(run.worker_turns, 2);
        assert!(run.repair_performed);
        assert_eq!(run.discarded_facts, 3);
        assert_eq!(run.session_cleanup_success, Some(true));
        assert_eq!(store.worker_runs_after(0).unwrap().len(), 2);
        assert_eq!(store.worker_runs_after(1).unwrap().len(), 1);
        let _ = fs::remove_file(path);
    }

    fn artifact_request(source: &str) -> ArtifactRequest {
        ArtifactRequest {
            contract_id: "evidence-brief".to_owned(),
            contract_revision: 1,
            repository_id: Digest::blake3("repo"),
            source_snapshot_digest: Digest::blake3(source),
            route_key: NeedKey::new("locate.implementation").unwrap(),
            normalized_request: "locate the implementation".to_owned(),
            semantic_fragment_id: None,
            input_artifact_ids: Vec::new(),
        }
    }

    fn artifact_for(request: &ArtifactRequest) -> Artifact {
        let contract = ArtifactContract::new(
            "evidence-brief",
            1,
            ArtifactKind::evidence_brief(),
            CacheScope::SnapshotExact,
        );
        let payload = serde_json::json!({"summary": "bounded"});
        Artifact {
            id: Artifact::compute_id(request.id(), &contract, &payload).unwrap(),
            request_id: request.id(),
            contract,
            payload,
            dependency_manifest: DependencyManifest {
                scope: CacheScope::SnapshotExact,
                observed_files_complete: false,
                dependencies: Vec::new(),
                gaps: Vec::new(),
            },
            validations: Vec::new(),
            created_unix_ms: now_ms(),
        }
    }

    #[test]
    fn exact_artifact_hit_preserves_id_and_stale_snapshot_never_hits() {
        let (path, store) = temporary_store();
        store.initialize().unwrap();
        let request = artifact_request("source-a");
        let artifact = artifact_for(&request);
        store.publish_artifact(&request, &artifact).unwrap();
        let (resolution, resolved) = store.resolve_artifact(&request).unwrap();
        assert_eq!(
            resolution,
            CacheResolution::ExactHit {
                artifact_id: artifact.id,
                sufficiency_certificate_id: None,
                selected_plan_id: None,
                resolution_format_revision: None,
            }
        );
        assert_eq!(resolved.unwrap().id, artifact.id);

        let changed = artifact_request("source-b");
        let (resolution, resolved) = store.resolve_artifact(&changed).unwrap();
        assert!(matches!(resolution, CacheResolution::Stale { .. }));
        assert!(resolved.is_none());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn negative_attempt_cache_is_exact_and_expires() {
        let (path, store) = temporary_store();
        store.initialize().unwrap();
        let identity = Digest::blake3("attempt-a");
        store
            .record_negative_attempt(
                identity,
                "semantic_validation",
                "{\"reason\":\"fixture\"}",
                now_ms().saturating_add(60_000),
            )
            .unwrap();
        assert_eq!(
            store.negative_attempt(identity).unwrap().unwrap().failure_code,
            "semantic_validation"
        );
        assert!(store.negative_attempt(Digest::blake3("attempt-b")).unwrap().is_none());
        store.record_negative_attempt(identity, "semantic_validation", "{}", now_ms()).unwrap();
        assert!(store.negative_attempt(identity).unwrap().is_none());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn route_cost_observations_keep_fresh_and_reuse_populations_separate() {
        let (path, store) = temporary_store();
        store.initialize().unwrap();
        for (source, cost, observed_unix_ms) in
            [("fresh", 90, 1), ("reuse", 7, 2), ("fresh", 110, 3), ("reuse", 5, 4)]
        {
            store
                .record_route_cost_observation(&RouteCostObservation {
                    route_key: "locate.implementation".to_owned(),
                    cost_microusd: cost,
                    source: source.to_owned(),
                    evidence_digest: Digest::blake3(format!("{source}-{cost}")),
                    observed_unix_ms,
                })
                .unwrap();
        }
        for observed_unix_ms in 5..40 {
            store
                .record_route_cost_observation(&RouteCostObservation {
                    route_key: "locate.implementation".to_owned(),
                    cost_microusd: 6,
                    source: "reuse".to_owned(),
                    evidence_digest: Digest::blake3(format!("reuse-{observed_unix_ms}")),
                    observed_unix_ms,
                })
                .unwrap();
        }

        assert_eq!(
            store.observed_route_cost_by_source("locate.implementation", "fresh").unwrap(),
            Some(110)
        );
        assert_eq!(
            store.observed_route_cost_by_source("locate.implementation", "reuse").unwrap(),
            Some(6)
        );
        assert_eq!(
            store.observed_route_cost_by_source("locate.implementation", "unknown").unwrap(),
            None
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn capability_promotion_requires_current_definition_and_evidence() {
        let (path, store) = temporary_store();
        store.initialize().unwrap();
        store
            .initialize_defaults(&RuntimeSettings {
                codex_executable: "codex".to_owned(),
                worker_model: "test".to_owned(),
                worker_reasoning: "low".to_owned(),
                worker_timeout_seconds: 1,
                evidence_failure_policy: needle_core::EvidenceFailurePolicy::DiscardInvalidFact,
                trusted_test_execution: false,
                multi_need_policy: MultiNeedPolicy::default(),
            })
            .unwrap();
        let classes = store.capability_classes().unwrap();
        assert_eq!(classes.len(), 6);
        let class = classes
            .iter()
            .find(|class| {
                class.reuse_unit == ReuseUnit::Artifact
                    && class.predicate == needle_core::PredicateKind::ImplementationLocation
            })
            .unwrap();

        assert!(matches!(
            store.set_capability_mode(
                &class.id,
                class.definition_digest,
                CapabilityMode::Authoritative,
                None,
            ),
            Err(StoreError::DefinitionDigest(_))
        ));
        assert!(matches!(
            store.set_capability_mode(
                &class.id,
                Digest::blake3(b"stale-definition"),
                CapabilityMode::Authoritative,
                Some(Digest::blake3(b"evidence")),
            ),
            Err(StoreError::DefinitionDigest(_))
        ));

        let promoted = store
            .set_capability_mode(
                &class.id,
                class.definition_digest,
                CapabilityMode::Authoritative,
                Some(Digest::blake3(b"evidence")),
            )
            .unwrap()
            .unwrap();
        assert_eq!(promoted.mode, CapabilityMode::Authoritative);
        let claim = store
            .capability_classes()
            .unwrap()
            .into_iter()
            .find(|candidate| {
                candidate.reuse_unit == ReuseUnit::Claim && candidate.predicate == class.predicate
            })
            .unwrap();
        assert_eq!(claim.mode, CapabilityMode::Shadow);
        let _ = fs::remove_file(path);
    }

    fn approval(id: &str, expires_unix_ms: u64) -> ApprovalRequest {
        let permissions = RequestedPermissions::default();
        let argv = vec!["cargo".to_owned(), "test".to_owned()];
        let cwd = "C:/sandbox";
        ApprovalRequest {
            id: id.to_owned(),
            protocol_request_id: serde_json::json!(1),
            protocol_approval_id: None,
            thread_id: "thread".to_owned(),
            turn_id: "turn".to_owned(),
            item_id: "item".to_owned(),
            argv: argv.clone(),
            command_display: Some(argv.join(" ")),
            cwd: cwd.to_owned(),
            reason: None,
            requested_permissions: permissions.clone(),
            route: "tests.relevant".to_owned(),
            repository_id: Digest::blake3("repo"),
            repository_root: cwd.to_owned(),
            expires_unix_ms,
            classification: CommandClassification::PendingUser,
            payload_digest: ApprovalRequest::compute_payload_digest(&argv, cwd, &permissions)
                .unwrap(),
            decision: None,
            decision_source: None,
            decided_unix_ms: None,
        }
    }

    #[test]
    fn approval_replay_and_payload_spoofing_fail_closed() {
        let (path, store) = temporary_store();
        store.initialize().unwrap();
        let request = approval("approval", now_ms() + 60_000);
        store.enqueue_approval(&request).unwrap();
        assert!(matches!(
            store.decide_approval(
                &request.id,
                ApprovalDecision::Accept,
                ApprovalDecisionSource::WebUser,
                Digest::blake3("spoofed"),
            ),
            Err(StoreError::ApprovalConflict(_))
        ));
        store
            .decide_approval(
                &request.id,
                ApprovalDecision::Accept,
                ApprovalDecisionSource::WebUser,
                request.payload_digest,
            )
            .unwrap();
        assert!(matches!(
            store.decide_approval(
                &request.id,
                ApprovalDecision::Accept,
                ApprovalDecisionSource::WebUser,
                request.payload_digest,
            ),
            Err(StoreError::ApprovalConflict(_))
        ));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn concurrent_approval_decisions_have_one_winner() {
        let (path, store) = temporary_store();
        store.initialize().unwrap();
        let request = approval("race", now_ms() + 60_000);
        store.enqueue_approval(&request).unwrap();
        let mut handles = Vec::new();
        for decision in [ApprovalDecision::Accept, ApprovalDecision::Decline] {
            let store = store.clone();
            let digest = request.payload_digest;
            handles.push(std::thread::spawn(move || {
                store.decide_approval("race", decision, ApprovalDecisionSource::WebUser, digest)
            }));
        }
        let results = handles.into_iter().map(|handle| handle.join().unwrap()).collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(StoreError::ApprovalConflict(_))))
                .count(),
            1
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn expired_approval_is_removed_from_pending_inbox() {
        let (path, store) = temporary_store();
        store.initialize().unwrap();
        store.enqueue_approval(&approval("expired", now_ms())).unwrap();
        assert!(store.pending_approvals().unwrap().is_empty());
        let expired = store.approval("expired").unwrap().unwrap();
        assert_eq!(expired.classification, CommandClassification::Expired);
        assert_eq!(expired.decision, Some(ApprovalDecision::Decline));
        assert_eq!(expired.decision_source, Some(ApprovalDecisionSource::Timeout));
        let _ = fs::remove_file(path);
    }
}
