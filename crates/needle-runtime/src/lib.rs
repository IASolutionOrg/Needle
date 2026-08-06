//! Local, on-demand Needle product runtime.

mod approval;
mod artifact_cache;
mod changes;
mod claim_proof;
mod lifecycle_executor;
mod model_ladder;
mod orchestrator;
mod proof;
mod router;
mod sandbox;
mod semantic_claim_projection;
mod semantic_resolver;
mod semantic_validation;
mod snapshot;
mod store;

pub use orchestrator::{
    ResolveOutcome, ResolveRequest, RuntimeEngine, RuntimeError, WorkerExecutor,
};
pub use router::{RouteError, select_route};
pub use sandbox::{IsolatedCheckout, SandboxError};
pub use snapshot::{
    SnapshotError, bind_evidence_digests, capture_git_snapshot, validate_cached_need_result,
    validate_need_result,
};
pub use store::{
    ActivationRecord, ActivationScope, ActivationStatus, CacheRecord, ChangeAttemptRecord,
    ConfigExport, LifecycleChangeContext, LifecycleProjection, LifecycleSummaryRecord,
    MAX_LIFECYCLE_LIST_LIMIT, MainTurnObservationRecord, NeedShadowRecord, NeedShadowWrite,
    NeedStepEventRecord, NeedStepRequestRecord, NegativeAttemptRecord, OperatorCostKey,
    OperatorCostObservation, PatchFileBlob, PreparedChangeRecord, ProofAccountingRecord,
    RoleProfileAuditOperation, RoleProfileAuditRecord, RoleProfileStateRecord,
    RouteCostObservation, RoutePromotionRecord, RuntimeSettings, RuntimeStore, SessionRecord,
    StoreError, WorkerRunRecord,
};

use needle_core::{NeedKey, Preset, Route, RouteMatcher};

pub fn built_in_presets() -> Vec<Preset> {
    vec![
        Preset::new(
            "locate.implementation",
            "Locate implementation",
            "Locate the exact implementation bodies and focused supporting tests for the requested anchors. Return only claims backed by exact repository-relative evidence.",
        ),
        Preset::new(
            "trace.state-flow",
            "Trace state flow",
            "Complete all discovery needed to trace producer, carrier, transformation, consumer, precedence, and focused tests. Return the minimum sufficient snapshot-bound claims and exact repository-relative evidence so the main model can answer without repository access. Do not investigate edit loci or modification constraints unless the root task explicitly requests a change.",
        ),
        Preset::new(
            "tests.relevant",
            "Relevant tests",
            "Identify the exact relevant test cases and focused commands without executing them. Cite the test definitions and the implementation behavior they constrain.",
        ),
    ]
}

pub fn built_in_routes() -> Vec<Route> {
    ["locate.implementation", "trace.state-flow", "tests.relevant"]
        .into_iter()
        .enumerate()
        .map(|(index, key)| {
            Route::new(
                key,
                100 - index as i32,
                RouteMatcher {
                    platform: "codex".to_owned(),
                    main_model: "*".to_owned(),
                    need_key: NeedKey::new(key).expect("built-in need keys are valid"),
                    repository: "*".to_owned(),
                },
                key,
            )
        })
        .collect()
}
pub use approval::{
    ApprovalBroker, ApprovalContext, ApprovalError, command_evidence_from_output,
    parse_direct_argv, parse_read_only_command_argv, parse_test_command_argv,
    validate_test_evidence,
};
pub use artifact_cache::{ArtifactCache, ArtifactCacheError, PlanCacheResult};
pub use changes::{
    ChangeApplyError, ChangeMaterializationError, apply_lifecycle_change, apply_verified_change,
    materialize_patch_artifact, recover_pending_change_applies, validate_patch_artifact_base,
};
pub use claim_proof::{
    ClaimProofError, ClaimProofMaterial, build_claim_component_certificate,
    build_claim_set_certificate, claim_proof_engine_definition,
    claim_validation_certificate_is_fresh, replay_claim_set_certificate,
};
pub use lifecycle_executor::{
    LIFECYCLE_ADAPTER_OUTCOME_SCHEMA, LIFECYCLE_ADAPTER_REQUEST_SCHEMA, LifecycleAdapterCleanup,
    LifecycleAdapterFailure, LifecycleAdapterResult, LifecycleCancellation,
    LifecycleExecutionDisposition, LifecycleExecutionError, LifecycleExecutionKernel,
    LifecycleExecutionOutcome, LifecyclePhaseAdapter, LifecyclePhaseAdapterOutcome,
    LifecyclePhaseAdapterRequest, LifecyclePhaseAdapters, LifecycleRemainingBudget,
    MAX_LIFECYCLE_ADAPTER_DETAIL_BYTES, MAX_LIFECYCLE_ADAPTER_OUTCOME_BYTES,
    MAX_LIFECYCLE_ADAPTER_REQUEST_BYTES, NeverCancel,
};
pub use model_ladder::{
    LadderAttempt, LadderAttemptRecord, ModelLadder, ModelLadderError, ModelLadderOutcome,
};
pub use proof::{
    ClaimAdvisoryPlan, ClaimAdvisoryResolutionKind, ProofCandidate, ProofError, ProofPlan,
    ProofPlanner, ProofResolutionKind, SemanticCostEstimates, ValiditySelection,
};
pub use semantic_resolver::{
    CLAIM_PROOF_RESOLUTION_FORMAT_REVISION, PROOF_RESOLUTION_FORMAT_REVISION, SemanticResolver,
    SemanticResolverError, SemanticReuseDecision,
};
pub use semantic_validation::{
    SEMANTIC_VALIDATOR_REVISION, SemanticValidationError, ValidatedClaimSet,
    ValidatedSemanticArtifact, artifact_and_certificate_are_fresh, manifest_digest,
    validate_semantic_artifact, validate_semantic_artifact_with_trace, validate_semantic_result,
    validate_semantic_test_plan, validate_semantic_test_plan_with_evidence, validator_definition,
};
