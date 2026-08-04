use crate::{LifecycleChangeContext, LifecycleProjection, RuntimeStore, StoreError};
use needle_core::{
    CanonicalHasher, ChangeId, DevelopmentLifecycle, Digest, LifecyclePhase, LifecycleReason,
    LifecycleStatus, LifecycleTransition, LifecycleUsage, PatchId, RoleProfileProvenance,
    VerificationArtifact, VerificationStatus,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const LIFECYCLE_ADAPTER_REQUEST_SCHEMA: &str = "needle.lifecycle-adapter-request/1";
pub const LIFECYCLE_ADAPTER_OUTCOME_SCHEMA: &str = "needle.lifecycle-adapter-outcome/1";
pub const MAX_LIFECYCLE_ADAPTER_REQUEST_BYTES: usize = 128 * 1024;
pub const MAX_LIFECYCLE_ADAPTER_OUTCOME_BYTES: usize = 128 * 1024;
pub const MAX_LIFECYCLE_ADAPTER_DETAIL_BYTES: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleRemainingBudget {
    pub worker_turns: u32,
    pub output_tokens: u64,
    pub cost_microusd: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecyclePhaseAdapterRequest {
    pub schema: String,
    pub invocation_id: Digest,
    pub expected_state_digest: Digest,
    pub phase: LifecyclePhase,
    pub profile: RoleProfileProvenance,
    pub remaining_budget: LifecycleRemainingBudget,
    pub lifecycle: DevelopmentLifecycle,
    pub change: LifecycleChangeContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_verification: Option<VerificationArtifact>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleAdapterFailure {
    pub code: String,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleAdapterCleanup {
    pub succeeded: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<LifecycleAdapterFailure>,
}

impl LifecycleAdapterCleanup {
    pub fn succeeded() -> Self {
        Self { succeeded: true, failure: None }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleAdapterResult {
    Completed { transition: Box<LifecycleTransition> },
    Failed { failure: LifecycleAdapterFailure },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecyclePhaseAdapterOutcome {
    pub schema: String,
    pub invocation_id: Digest,
    pub expected_state_digest: Digest,
    pub phase: LifecyclePhase,
    pub usage: LifecycleUsage,
    pub result: LifecycleAdapterResult,
    pub cleanup: LifecycleAdapterCleanup,
}

/// A phase adapter is invoked with a deterministic `invocation_id`. Concrete
/// adapters must make that identity idempotent: repeating the same request may
/// return the recorded outcome, but must not repeat its external side effect.
/// The adapter receives no lifecycle-store transition capability.
pub trait LifecyclePhaseAdapter {
    fn invoke(&self, request: &LifecyclePhaseAdapterRequest) -> LifecyclePhaseAdapterOutcome;
}

#[derive(Clone, Copy)]
pub struct LifecyclePhaseAdapters<'a> {
    pub explore: &'a dyn LifecyclePhaseAdapter,
    pub implement: &'a dyn LifecyclePhaseAdapter,
    pub test: &'a dyn LifecyclePhaseAdapter,
    pub review: &'a dyn LifecyclePhaseAdapter,
    pub verify: &'a dyn LifecyclePhaseAdapter,
}

impl LifecyclePhaseAdapters<'_> {
    fn for_phase(&self, phase: LifecyclePhase) -> Option<&dyn LifecyclePhaseAdapter> {
        match phase {
            LifecyclePhase::Explore => Some(self.explore),
            LifecyclePhase::Implement => Some(self.implement),
            LifecyclePhase::Test => Some(self.test),
            LifecyclePhase::Review => Some(self.review),
            LifecyclePhase::Verify => Some(self.verify),
            LifecyclePhase::Apply => None,
        }
    }
}

pub trait LifecycleCancellation {
    fn is_cancelled(&self, change_id: &ChangeId, expected_state_digest: Digest) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NeverCancel;

impl LifecycleCancellation for NeverCancel {
    fn is_cancelled(&self, _change_id: &ChangeId, _expected_state_digest: Digest) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleExecutionDisposition {
    Advanced,
    RepairResumed,
    AwaitingApproval,
    Terminal,
    Cancelled,
    BudgetExhausted,
    AdapterFailed,
    CleanupFailed,
    InvalidAdapterOutput,
    StaleState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleExecutionOutcome {
    pub disposition: LifecycleExecutionDisposition,
    pub projection: LifecycleProjection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<Digest>,
    pub adapter_usage: LifecycleUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<LifecycleAdapterCleanup>,
}

#[derive(Debug, Error)]
pub enum LifecycleExecutionError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("lifecycle adapter contract could not be serialized")]
    ContractSerialization,
}

pub struct LifecycleExecutionKernel<'a> {
    store: &'a RuntimeStore,
    adapters: LifecyclePhaseAdapters<'a>,
    cancellation: &'a dyn LifecycleCancellation,
}

impl<'a> LifecycleExecutionKernel<'a> {
    pub fn new(
        store: &'a RuntimeStore,
        adapters: LifecyclePhaseAdapters<'a>,
        cancellation: &'a dyn LifecycleCancellation,
    ) -> Self {
        Self { store, adapters, cancellation }
    }

    pub fn step(
        &self,
        change_id: &ChangeId,
    ) -> Result<LifecycleExecutionOutcome, LifecycleExecutionError> {
        execute_step(self.store, &self.adapters, self.cancellation, change_id)
    }
}

trait LifecycleKernelStore {
    fn replay(&self, change_id: &ChangeId) -> Result<LifecycleProjection, StoreError>;
    fn change_context(
        &self,
        change_id: &ChangeId,
    ) -> Result<Option<LifecycleChangeContext>, StoreError>;
    fn latest_verification(
        &self,
        change_id: &ChangeId,
    ) -> Result<Option<VerificationArtifact>, StoreError>;
    fn parent_transition(
        &self,
        change_id: &ChangeId,
        expected_state_digest: Digest,
        transition: LifecycleTransition,
    ) -> Result<LifecycleProjection, StoreError>;
    fn begin_repair(&self, change_id: &ChangeId, patch_id: PatchId) -> Result<(), StoreError>;
}

impl LifecycleKernelStore for RuntimeStore {
    fn replay(&self, change_id: &ChangeId) -> Result<LifecycleProjection, StoreError> {
        self.replay_lifecycle(change_id)
    }

    fn change_context(
        &self,
        change_id: &ChangeId,
    ) -> Result<Option<LifecycleChangeContext>, StoreError> {
        self.lifecycle_change_context(change_id)
    }

    fn latest_verification(
        &self,
        change_id: &ChangeId,
    ) -> Result<Option<VerificationArtifact>, StoreError> {
        self.latest_verification_artifact(change_id)
    }

    fn parent_transition(
        &self,
        change_id: &ChangeId,
        expected_state_digest: Digest,
        transition: LifecycleTransition,
    ) -> Result<LifecycleProjection, StoreError> {
        self.parent_transition_lifecycle(change_id, expected_state_digest, transition)
    }

    fn begin_repair(&self, change_id: &ChangeId, patch_id: PatchId) -> Result<(), StoreError> {
        self.begin_change_repair(change_id, patch_id)
    }
}

fn execute_step<S: LifecycleKernelStore>(
    store: &S,
    adapters: &LifecyclePhaseAdapters<'_>,
    cancellation: &dyn LifecycleCancellation,
    change_id: &ChangeId,
) -> Result<LifecycleExecutionOutcome, LifecycleExecutionError> {
    let projection = store.replay(change_id)?;
    if projection.lifecycle.status.terminal() {
        return Ok(outcome(LifecycleExecutionDisposition::Terminal, projection));
    }
    if projection.lifecycle.phase == LifecyclePhase::Apply {
        return Ok(outcome(LifecycleExecutionDisposition::AwaitingApproval, projection));
    }
    let expected_state_digest = projection.state_digest;
    if cancellation.is_cancelled(change_id, expected_state_digest) {
        return commit_terminal(
            store,
            change_id,
            projection,
            LifecycleExecutionDisposition::Cancelled,
            LifecycleTransition::Cancel {
                reason: fixed_reason("cancelled", b"lifecycle cancelled before adapter invocation"),
            },
            None,
            LifecycleUsage::default(),
            None,
        );
    }
    if projection.lifecycle.status == LifecycleStatus::RepairReserved {
        return resume_repair(store, change_id, projection);
    }
    if projection.lifecycle.status != LifecycleStatus::Active {
        return Ok(outcome(LifecycleExecutionDisposition::Terminal, projection));
    }
    let remaining_budget = remaining_budget(&projection.lifecycle);
    if remaining_budget.worker_turns == 0
        || remaining_budget.output_tokens == 0
        || remaining_budget.cost_microusd == 0
    {
        return commit_terminal(
            store,
            change_id,
            projection,
            LifecycleExecutionDisposition::BudgetExhausted,
            LifecycleTransition::Fail {
                reason: fixed_reason("budget_exhausted", b"lifecycle aggregate budget exhausted"),
            },
            None,
            LifecycleUsage::default(),
            None,
        );
    }
    let Some(profile) =
        projection.lifecycle.spec.profiles.for_phase(projection.lifecycle.phase).cloned()
    else {
        return invalid_output(
            store,
            change_id,
            projection,
            None,
            LifecycleUsage::default(),
            None,
            b"active lifecycle phase has no frozen profile",
        );
    };
    let Some(change) = store.change_context(change_id)? else {
        return Err(StoreError::LifecycleNotFound(format!(
            "{change_id}: immutable change request"
        ))
        .into());
    };
    if change.source_snapshot != projection.lifecycle.source_snapshot {
        return Err(StoreError::LifecycleCorruption(format!(
            "{change_id}: lifecycle and request source differ"
        ))
        .into());
    }
    let repair_verification = repair_verification(store, &projection)?;
    let invocation_id = invocation_id(&projection);
    let request = LifecyclePhaseAdapterRequest {
        schema: LIFECYCLE_ADAPTER_REQUEST_SCHEMA.to_owned(),
        invocation_id,
        expected_state_digest,
        phase: projection.lifecycle.phase,
        profile,
        remaining_budget,
        lifecycle: projection.lifecycle.clone(),
        change,
        repair_verification,
    };
    if serde_json::to_vec(&request)
        .map_err(|_| LifecycleExecutionError::ContractSerialization)?
        .len()
        > MAX_LIFECYCLE_ADAPTER_REQUEST_BYTES
    {
        return invalid_output(
            store,
            change_id,
            projection,
            Some(invocation_id),
            LifecycleUsage::default(),
            None,
            b"lifecycle adapter request exceeds the byte bound",
        );
    }
    let adapter =
        adapters.for_phase(request.phase).expect("active worker phases have an injected adapter");
    let adapter_outcome = adapter.invoke(&request);
    finish_adapter_step(store, cancellation, change_id, projection, request, adapter_outcome)
}

fn resume_repair<S: LifecycleKernelStore>(
    store: &S,
    change_id: &ChangeId,
    projection: LifecycleProjection,
) -> Result<LifecycleExecutionOutcome, LifecycleExecutionError> {
    if projection.lifecycle.phase != LifecyclePhase::Verify {
        return Err(StoreError::LifecycleCorruption(format!(
            "{change_id}: repair is outside verify"
        ))
        .into());
    }
    let patch_id = projection
        .lifecycle
        .patch
        .as_ref()
        .map(|patch| patch.patch_id)
        .ok_or_else(|| StoreError::LifecycleCorruption(format!("{change_id}: repair patch")))?;
    match store.begin_repair(change_id, patch_id) {
        Ok(()) => {
            let resumed = store.replay(change_id)?;
            Ok(outcome(LifecycleExecutionDisposition::RepairResumed, resumed))
        }
        Err(error @ (StoreError::ChangeConflict(_) | StoreError::LifecycleConflict(_))) => {
            let current = store.replay(change_id)?;
            if current.state_digest != projection.state_digest {
                Ok(outcome(LifecycleExecutionDisposition::StaleState, current))
            } else {
                Err(error.into())
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn finish_adapter_step<S: LifecycleKernelStore>(
    store: &S,
    cancellation: &dyn LifecycleCancellation,
    change_id: &ChangeId,
    projection: LifecycleProjection,
    request: LifecyclePhaseAdapterRequest,
    adapter_outcome: LifecyclePhaseAdapterOutcome,
) -> Result<LifecycleExecutionOutcome, LifecycleExecutionError> {
    let serialized = serde_json::to_vec(&adapter_outcome)
        .map_err(|_| LifecycleExecutionError::ContractSerialization)?;
    if serialized.len() > MAX_LIFECYCLE_ADAPTER_OUTCOME_BYTES
        || !adapter_outcome_matches(&request, &adapter_outcome)
        || !usage_within(&adapter_outcome.usage, &request.remaining_budget)
    {
        return invalid_output(
            store,
            change_id,
            projection,
            Some(request.invocation_id),
            adapter_outcome.usage,
            Some(adapter_outcome.cleanup),
            b"adapter outcome identity, shape, or bounds are invalid",
        );
    }
    if !adapter_outcome.cleanup.succeeded {
        let detail = adapter_outcome
            .cleanup
            .failure
            .as_ref()
            .map(problem_detail)
            .unwrap_or_else(|| b"adapter cleanup failed without detail".to_vec());
        return commit_terminal(
            store,
            change_id,
            projection,
            LifecycleExecutionDisposition::CleanupFailed,
            LifecycleTransition::Fail { reason: fixed_reason("adapter_cleanup_failed", detail) },
            Some(request.invocation_id),
            adapter_outcome.usage,
            Some(adapter_outcome.cleanup),
        );
    }
    if cancellation.is_cancelled(change_id, request.expected_state_digest) {
        return commit_terminal(
            store,
            change_id,
            projection,
            LifecycleExecutionDisposition::Cancelled,
            LifecycleTransition::Cancel {
                reason: fixed_reason("cancelled", b"lifecycle cancelled after adapter invocation"),
            },
            Some(request.invocation_id),
            adapter_outcome.usage,
            Some(adapter_outcome.cleanup),
        );
    }
    match adapter_outcome.result {
        LifecycleAdapterResult::Failed { failure } => commit_terminal(
            store,
            change_id,
            projection,
            LifecycleExecutionDisposition::AdapterFailed,
            LifecycleTransition::Fail {
                reason: LifecycleReason::new(failure.code, failure.detail.as_bytes())
                    .expect("validated adapter failure has a bounded reason"),
            },
            Some(request.invocation_id),
            adapter_outcome.usage,
            Some(adapter_outcome.cleanup),
        ),
        LifecycleAdapterResult::Completed { transition } => {
            if !completion_matches(&request, &transition, &adapter_outcome.usage) {
                return invalid_output(
                    store,
                    change_id,
                    projection,
                    Some(request.invocation_id),
                    adapter_outcome.usage,
                    Some(adapter_outcome.cleanup),
                    b"adapter completion differs from the active phase, profile, or budget",
                );
            }
            commit_completion(
                store,
                change_id,
                projection,
                request.invocation_id,
                *transition,
                adapter_outcome.usage,
                adapter_outcome.cleanup,
            )
        }
    }
}

fn commit_completion<S: LifecycleKernelStore>(
    store: &S,
    change_id: &ChangeId,
    projection: LifecycleProjection,
    invocation_id: Digest,
    transition: LifecycleTransition,
    usage: LifecycleUsage,
    cleanup: LifecycleAdapterCleanup,
) -> Result<LifecycleExecutionOutcome, LifecycleExecutionError> {
    match store.parent_transition(change_id, projection.state_digest, transition) {
        Ok(next) => {
            let disposition = if next.lifecycle.status.terminal() {
                LifecycleExecutionDisposition::Terminal
            } else if next.lifecycle.phase == LifecyclePhase::Apply
                && next.lifecycle.status == LifecycleStatus::AwaitingApproval
            {
                LifecycleExecutionDisposition::AwaitingApproval
            } else {
                LifecycleExecutionDisposition::Advanced
            };
            Ok(LifecycleExecutionOutcome {
                disposition,
                projection: next,
                invocation_id: Some(invocation_id),
                adapter_usage: usage,
                cleanup: Some(cleanup),
            })
        }
        Err(error @ (StoreError::Lifecycle(_) | StoreError::LifecycleConflict(_))) => {
            let current = store.replay(change_id)?;
            if current.state_digest != projection.state_digest {
                return Ok(LifecycleExecutionOutcome {
                    disposition: LifecycleExecutionDisposition::StaleState,
                    projection: current,
                    invocation_id: Some(invocation_id),
                    adapter_usage: usage,
                    cleanup: Some(cleanup),
                });
            }
            invalid_output(
                store,
                change_id,
                projection,
                Some(invocation_id),
                usage,
                Some(cleanup),
                error.to_string().as_bytes(),
            )
        }
        Err(error) => Err(error.into()),
    }
}

fn invalid_output<S: LifecycleKernelStore>(
    store: &S,
    change_id: &ChangeId,
    projection: LifecycleProjection,
    invocation_id: Option<Digest>,
    usage: LifecycleUsage,
    cleanup: Option<LifecycleAdapterCleanup>,
    detail: impl AsRef<[u8]>,
) -> Result<LifecycleExecutionOutcome, LifecycleExecutionError> {
    commit_terminal(
        store,
        change_id,
        projection,
        LifecycleExecutionDisposition::InvalidAdapterOutput,
        LifecycleTransition::Fail { reason: fixed_reason("adapter_invalid_output", detail) },
        invocation_id,
        usage,
        cleanup,
    )
}

#[allow(clippy::too_many_arguments)]
fn commit_terminal<S: LifecycleKernelStore>(
    store: &S,
    change_id: &ChangeId,
    projection: LifecycleProjection,
    disposition: LifecycleExecutionDisposition,
    transition: LifecycleTransition,
    invocation_id: Option<Digest>,
    usage: LifecycleUsage,
    cleanup: Option<LifecycleAdapterCleanup>,
) -> Result<LifecycleExecutionOutcome, LifecycleExecutionError> {
    match store.parent_transition(change_id, projection.state_digest, transition) {
        Ok(next) => Ok(LifecycleExecutionOutcome {
            disposition,
            projection: next,
            invocation_id,
            adapter_usage: usage,
            cleanup,
        }),
        Err(error @ StoreError::LifecycleConflict(_)) => {
            let current = store.replay(change_id)?;
            if current.state_digest != projection.state_digest {
                Ok(LifecycleExecutionOutcome {
                    disposition: LifecycleExecutionDisposition::StaleState,
                    projection: current,
                    invocation_id,
                    adapter_usage: usage,
                    cleanup,
                })
            } else {
                Err(error.into())
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn adapter_outcome_matches(
    request: &LifecyclePhaseAdapterRequest,
    outcome: &LifecyclePhaseAdapterOutcome,
) -> bool {
    outcome.schema == LIFECYCLE_ADAPTER_OUTCOME_SCHEMA
        && outcome.invocation_id == request.invocation_id
        && outcome.expected_state_digest == request.expected_state_digest
        && outcome.phase == request.phase
        && cleanup_valid(&outcome.cleanup)
        && match &outcome.result {
            LifecycleAdapterResult::Completed { .. } => true,
            LifecycleAdapterResult::Failed { failure } => problem_valid(failure),
        }
}

fn cleanup_valid(cleanup: &LifecycleAdapterCleanup) -> bool {
    cleanup.succeeded == cleanup.failure.is_none()
        && cleanup.failure.as_ref().is_none_or(problem_valid)
}

fn problem_valid(problem: &LifecycleAdapterFailure) -> bool {
    !problem.code.is_empty()
        && problem.code.len() <= needle_core::MAX_LIFECYCLE_REASON_CODE_BYTES
        && problem
            .code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
        && problem.detail.len() <= MAX_LIFECYCLE_ADAPTER_DETAIL_BYTES
}

fn completion_matches(
    request: &LifecyclePhaseAdapterRequest,
    transition: &LifecycleTransition,
    usage: &LifecycleUsage,
) -> bool {
    let phase_matches = matches!(
        (request.phase, transition),
        (LifecyclePhase::Explore, LifecycleTransition::CompleteExplore { .. })
            | (LifecyclePhase::Implement, LifecycleTransition::CompleteImplement { .. })
            | (LifecyclePhase::Test, LifecycleTransition::CompleteTest { .. })
            | (LifecyclePhase::Review, LifecycleTransition::CompleteReview { .. })
            | (LifecyclePhase::Verify, LifecycleTransition::CompleteVerify { .. })
    );
    let Some(worker) = transition.worker() else {
        return false;
    };
    phase_matches
        && worker.profile == request.profile
        && worker.worker_depth == 1
        && worker.logical_worker_spawns == 1
        && worker.usage == *usage
        && usage.worker_turns > 0
}

fn usage_within(usage: &LifecycleUsage, remaining: &LifecycleRemainingBudget) -> bool {
    usage.worker_turns <= remaining.worker_turns
        && usage.output_tokens <= remaining.output_tokens
        && usage.cost_microusd <= remaining.cost_microusd
}

fn remaining_budget(lifecycle: &DevelopmentLifecycle) -> LifecycleRemainingBudget {
    LifecycleRemainingBudget {
        worker_turns: lifecycle
            .spec
            .budget
            .max_worker_turns
            .saturating_sub(lifecycle.usage.worker_turns),
        output_tokens: lifecycle
            .spec
            .budget
            .max_output_tokens
            .saturating_sub(lifecycle.usage.output_tokens),
        cost_microusd: lifecycle
            .spec
            .budget
            .max_cost_microusd
            .saturating_sub(lifecycle.usage.cost_microusd),
    }
}

fn repair_verification<S: LifecycleKernelStore>(
    store: &S,
    projection: &LifecycleProjection,
) -> Result<Option<VerificationArtifact>, LifecycleExecutionError> {
    if !projection.lifecycle.repair_consumed {
        return Ok(None);
    }
    let verification =
        store.latest_verification(&projection.lifecycle.change_id)?.ok_or_else(|| {
            StoreError::LifecycleCorruption(format!(
                "{}: consumed repair has no verification artifact",
                projection.lifecycle.change_id
            ))
        })?;
    if verification.change_id != projection.lifecycle.change_id
        || verification.verdict != VerificationStatus::Repairable
        || !verification.is_canonical()
    {
        return Err(StoreError::LifecycleCorruption(format!(
            "{}: consumed repair verification is invalid",
            projection.lifecycle.change_id
        ))
        .into());
    }
    Ok(Some(verification))
}

fn invocation_id(projection: &LifecycleProjection) -> Digest {
    let mut hasher = CanonicalHasher::new(b"needle-lifecycle-adapter-invocation");
    hasher.field_digest(projection.lifecycle.id.0);
    hasher.field_digest(projection.state_digest);
    hasher.field_str(projection.lifecycle.phase.as_str());
    hasher.finish()
}

fn problem_detail(problem: &LifecycleAdapterFailure) -> Vec<u8> {
    serde_json::to_vec(problem).unwrap_or_else(|_| b"invalid adapter failure detail".to_vec())
}

fn fixed_reason(code: &'static str, detail: impl AsRef<[u8]>) -> LifecycleReason {
    LifecycleReason::new(code, detail).expect("fixed lifecycle reason is bounded")
}

fn outcome(
    disposition: LifecycleExecutionDisposition,
    projection: LifecycleProjection,
) -> LifecycleExecutionOutcome {
    LifecycleExecutionOutcome {
        disposition,
        projection,
        invocation_id: None,
        adapter_usage: LifecycleUsage::default(),
        cleanup: None,
    }
}

#[cfg(test)]
#[path = "lifecycle_executor/tests.rs"]
mod tests;
