use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use needle_core::{
    AcceptanceStatus, ApprovalDecisionSource, ChangeApplyStatus, ChangeId, Digest,
    LifecycleArtifactKind, LifecycleEvent, LifecycleEventKind, LifecycleId, LifecyclePhase,
    LifecycleReason, LifecycleReviewVerdict, LifecycleStatus, LifecycleTerminalOutcome,
    VerificationStatus,
};
use needle_runtime::{
    LifecycleProjection, LifecycleSummaryRecord, MAX_LIFECYCLE_LIST_LIMIT, StoreError,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::AppState;

const LIST_SCHEMA: &str = "needle.lifecycle-list/1";
const DETAIL_SCHEMA: &str = "needle.lifecycle-detail/1";
const EVENTS_SCHEMA: &str = "needle.lifecycle-events/1";
const ERROR_SCHEMA: &str = "needle.lifecycle-error/1";
const DEFAULT_LIMIT: usize = 50;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitQuery {
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct LifecycleApiError {
    schema: &'static str,
    code: &'static str,
    message: &'static str,
}

#[derive(Clone, Copy, Debug)]
struct LifecycleRejection {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl LifecycleRejection {
    fn into_response(self) -> Response {
        lifecycle_error(self.status, self.code, self.message)
    }
}

#[derive(Debug, Serialize)]
struct LifecycleUsageDto {
    worker_turns: u32,
    output_tokens: u64,
    cost_microusd: u64,
}

#[derive(Debug, Serialize)]
struct LifecycleReasonDto {
    code: String,
    detail_digest: Digest,
}

#[derive(Debug, Serialize)]
struct LifecycleSummaryDto {
    lifecycle_id: LifecycleId,
    change_id: ChangeId,
    source_snapshot: Digest,
    phase: LifecyclePhase,
    status: LifecycleStatus,
    state_digest: Digest,
    generation: u64,
    usage: LifecycleUsageDto,
    terminal_outcome: Option<LifecycleTerminalOutcome>,
    terminal_reason: Option<LifecycleReasonDto>,
    created_unix_ms: u64,
    updated_unix_ms: u64,
}

#[derive(Debug, Serialize)]
struct LifecycleBudgetDto {
    max_worker_turns: u32,
    max_output_tokens: u64,
    max_cost_microusd: u64,
    max_concurrent_workers: u8,
}

#[derive(Debug, Serialize)]
struct LifecycleProfileIdentityDto {
    phase: LifecyclePhase,
    profile_id: needle_core::RoleProfileId,
    revision: u64,
    definition_digest: Digest,
}

#[derive(Debug, Serialize)]
struct LifecycleTestPlanIdentityDto {
    plan_digest: Digest,
    certificate_digest: Digest,
}

#[derive(Debug, Serialize)]
struct LifecycleExplorationArtifactDto {
    kind: LifecycleArtifactKind,
    artifact_id: Digest,
    source_snapshot: Digest,
}

#[derive(Debug, Serialize)]
struct LifecyclePatchDto {
    patch_id: needle_core::PatchId,
    revision: u32,
}

#[derive(Debug, Serialize)]
struct LifecycleTestResultDto {
    plan_digest: Digest,
    certificate_digest: Digest,
    available: bool,
    executed: bool,
    passed: bool,
    evidence_id: Option<String>,
    failure_code: Option<String>,
}

#[derive(Debug, Serialize)]
struct LifecycleAcceptanceReviewDto {
    criterion_digest: Digest,
    status: AcceptanceStatus,
    evidence_digest: Digest,
}

#[derive(Debug, Serialize)]
struct LifecycleReviewDto {
    review_id: Digest,
    patch_id: needle_core::PatchId,
    verdict: LifecycleReviewVerdict,
    acceptance_coverage: Vec<LifecycleAcceptanceReviewDto>,
    findings: Vec<LifecycleReasonDto>,
    reviewer_definition: Digest,
    created_unix_ms: u64,
}

#[derive(Debug, Serialize)]
struct LifecycleVerificationDto {
    verification_id: needle_core::VerificationArtifactId,
    patch_id: needle_core::PatchId,
    verdict: VerificationStatus,
}

#[derive(Debug, Serialize)]
struct LifecycleApprovalDto {
    approval_id: Digest,
    approved_state_digest: Digest,
    patch_id: needle_core::PatchId,
    verification_id: needle_core::VerificationArtifactId,
    decision_source: ApprovalDecisionSource,
    decided_unix_ms: u64,
}

#[derive(Debug, Serialize)]
struct LifecycleRecoveryDto {
    apply_id: needle_core::ChangeApplyId,
    patch_id: needle_core::PatchId,
    status: ChangeApplyStatus,
    pre_snapshot: Digest,
    post_snapshot: Option<Digest>,
    created_unix_ms: u64,
    completed_unix_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct LifecycleCleanupDto {
    status: &'static str,
    reason: LifecycleReasonDto,
}

#[derive(Debug, Serialize)]
struct LifecycleArtifactsDto {
    exploration: Vec<LifecycleExplorationArtifactDto>,
    patch: Option<LifecyclePatchDto>,
    tests: Vec<LifecycleTestResultDto>,
    review: Option<LifecycleReviewDto>,
    verification: Option<LifecycleVerificationDto>,
}

#[derive(Debug, Serialize)]
struct LifecycleDetailDto {
    lifecycle_id: LifecycleId,
    change_id: ChangeId,
    source_snapshot: Digest,
    phase: LifecyclePhase,
    status: LifecycleStatus,
    state_digest: Digest,
    generation: u64,
    worker_depth_limit: u8,
    profiles: Vec<LifecycleProfileIdentityDto>,
    test_plans: Vec<LifecycleTestPlanIdentityDto>,
    budget: LifecycleBudgetDto,
    usage: LifecycleUsageDto,
    artifacts: LifecycleArtifactsDto,
    repair_reserved: bool,
    repair_consumed: bool,
    terminal_outcome: Option<LifecycleTerminalOutcome>,
    terminal_reason: Option<LifecycleReasonDto>,
    approval: Option<LifecycleApprovalDto>,
    apply_id: Option<needle_core::ChangeApplyId>,
    cleanup: Option<LifecycleCleanupDto>,
    recovery: Option<LifecycleRecoveryDto>,
    created_unix_ms: u64,
    updated_unix_ms: u64,
}

#[derive(Debug, Serialize)]
struct LifecycleEventActionDto {
    kind: &'static str,
    reason: Option<LifecycleReasonDto>,
    apply_status: Option<ChangeApplyStatus>,
}

#[derive(Debug, Serialize)]
struct LifecycleEventDto {
    lifecycle_id: LifecycleId,
    change_id: ChangeId,
    sequence: u64,
    phase: LifecyclePhase,
    status: LifecycleStatus,
    source_snapshot: Digest,
    profile_definition_digest: Option<Digest>,
    patch_id: Option<needle_core::PatchId>,
    verification_id: Option<needle_core::VerificationArtifactId>,
    prior_state_digest: Option<Digest>,
    resulting_state_digest: Digest,
    action: LifecycleEventActionDto,
    created_unix_ms: u64,
}

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/v1/lifecycles", get(list_lifecycles))
        .route("/api/v1/lifecycles/{id}", get(get_lifecycle))
        .route("/api/v1/lifecycles/{id}/events", get(get_lifecycle_events))
}

async fn list_lifecycles(
    State(state): State<AppState>,
    query: Result<Query<LimitQuery>, QueryRejection>,
) -> Response {
    let limit = match query {
        Ok(Query(query)) => match bounded_limit(query.limit) {
            Ok(limit) => limit,
            Err(rejection) => return rejection.into_response(),
        },
        Err(_) => {
            return lifecycle_error(StatusCode::BAD_REQUEST, "invalid_query", "query is invalid");
        }
    };
    match state.store.list_lifecycle_summaries(limit) {
        Ok(records) => {
            let items = records.into_iter().map(summary_dto).collect::<Vec<_>>();
            Json(json!({"schema": LIST_SCHEMA, "items": items, "limit": limit})).into_response()
        }
        Err(error) => store_error(error),
    }
}

async fn get_lifecycle(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let change_id = match parse_change_id(&id) {
        Ok(change_id) => change_id,
        Err(rejection) => return rejection.into_response(),
    };
    let projection = match state.store.replay_lifecycle(&change_id) {
        Ok(projection) => projection,
        Err(error) => return store_error(error),
    };
    let recovery = match state.store.latest_change_apply(&change_id) {
        Ok(record) => record.as_ref().map(recovery_dto),
        Err(error) => return store_error(error),
    };
    Json(json!({
        "schema": DETAIL_SCHEMA,
        "lifecycle": detail_dto(projection, recovery),
    }))
    .into_response()
}

async fn get_lifecycle_events(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let change_id = match parse_change_id(&id) {
        Ok(change_id) => change_id,
        Err(rejection) => return rejection.into_response(),
    };
    match state.store.replay_lifecycle_events(&change_id) {
        Ok(events) => {
            let items = events.iter().map(event_dto).collect::<Vec<_>>();
            Json(json!({
                "schema": EVENTS_SCHEMA,
                "change_id": change_id,
                "items": items,
                "limit": needle_core::MAX_LIFECYCLE_EVENTS,
            }))
            .into_response()
        }
        Err(error) => store_error(error),
    }
}

fn bounded_limit(limit: Option<usize>) -> Result<usize, LifecycleRejection> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIFECYCLE_LIST_LIMIT {
        return Err(LifecycleRejection {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_limit",
            message: "limit must be between 1 and 100",
        });
    }
    Ok(limit)
}

fn parse_change_id(id: &str) -> Result<ChangeId, LifecycleRejection> {
    ChangeId::parse(id).map_err(|_| LifecycleRejection {
        status: StatusCode::BAD_REQUEST,
        code: "invalid_change_id",
        message: "change ID is invalid",
    })
}

fn summary_dto(record: LifecycleSummaryRecord) -> LifecycleSummaryDto {
    LifecycleSummaryDto {
        lifecycle_id: record.lifecycle_id,
        change_id: record.change_id,
        source_snapshot: record.source_snapshot,
        phase: record.phase,
        status: record.status,
        state_digest: record.state_digest,
        generation: record.generation,
        usage: usage_dto(&record.usage),
        terminal_outcome: record.terminal_outcome,
        terminal_reason: record.terminal_reason.as_ref().map(reason_dto),
        created_unix_ms: record.created_unix_ms,
        updated_unix_ms: record.updated_unix_ms,
    }
}

fn detail_dto(
    projection: LifecycleProjection,
    recovery: Option<LifecycleRecoveryDto>,
) -> LifecycleDetailDto {
    let lifecycle = projection.lifecycle;
    let profiles = LifecyclePhase::ALL
        .into_iter()
        .take(5)
        .map(|phase| {
            let profile = lifecycle
                .spec
                .profiles
                .for_phase(phase)
                .expect("validated worker phases have frozen profiles");
            LifecycleProfileIdentityDto {
                phase,
                profile_id: profile.profile_id.clone(),
                revision: profile.revision,
                definition_digest: profile.definition_digest,
            }
        })
        .collect();
    let test_plans = lifecycle
        .spec
        .test_plans
        .iter()
        .map(|binding| LifecycleTestPlanIdentityDto {
            plan_digest: binding.plan_digest(),
            certificate_digest: binding.certificate_digest,
        })
        .collect();
    let artifacts = LifecycleArtifactsDto {
        exploration: lifecycle
            .exploration_artifacts
            .iter()
            .map(|artifact| LifecycleExplorationArtifactDto {
                kind: artifact.kind,
                artifact_id: artifact.id,
                source_snapshot: artifact.source_snapshot,
            })
            .collect(),
        patch: lifecycle
            .patch
            .as_ref()
            .map(|patch| LifecyclePatchDto { patch_id: patch.patch_id, revision: patch.revision }),
        tests: lifecycle
            .test_results
            .iter()
            .map(|result| LifecycleTestResultDto {
                plan_digest: result.plan_digest,
                certificate_digest: result.certificate_digest,
                available: result.available,
                executed: result.executed,
                passed: result.passed,
                evidence_id: result.evidence_id.clone(),
                failure_code: result.failure_code.clone(),
            })
            .collect(),
        review: lifecycle.review.as_ref().map(|review| LifecycleReviewDto {
            review_id: review.id,
            patch_id: review.patch_id,
            verdict: review.verdict,
            acceptance_coverage: review
                .acceptance_coverage
                .iter()
                .map(|coverage| LifecycleAcceptanceReviewDto {
                    criterion_digest: coverage.criterion_digest,
                    status: coverage.status,
                    evidence_digest: coverage.evidence_digest,
                })
                .collect(),
            findings: review.findings.iter().map(reason_dto).collect(),
            reviewer_definition: review.reviewer_definition,
            created_unix_ms: review.created_unix_ms,
        }),
        verification: lifecycle.verification.as_ref().map(|verification| {
            LifecycleVerificationDto {
                verification_id: verification.verification_id,
                patch_id: verification.patch_id,
                verdict: verification.verdict,
            }
        }),
    };
    let cleanup = lifecycle.terminal_reason.as_ref().and_then(|reason| {
        (reason.code == "adapter_cleanup_failed")
            .then(|| LifecycleCleanupDto { status: "failed", reason: reason_dto(reason) })
    });
    LifecycleDetailDto {
        lifecycle_id: lifecycle.id,
        change_id: lifecycle.change_id,
        source_snapshot: lifecycle.source_snapshot,
        phase: lifecycle.phase,
        status: lifecycle.status,
        state_digest: projection.state_digest,
        generation: lifecycle.generation,
        worker_depth_limit: lifecycle.spec.worker_depth_limit,
        profiles,
        test_plans,
        budget: LifecycleBudgetDto {
            max_worker_turns: lifecycle.spec.budget.max_worker_turns,
            max_output_tokens: lifecycle.spec.budget.max_output_tokens,
            max_cost_microusd: lifecycle.spec.budget.max_cost_microusd,
            max_concurrent_workers: lifecycle.spec.budget.max_concurrent_workers,
        },
        usage: usage_dto(&lifecycle.usage),
        artifacts,
        repair_reserved: lifecycle.repair_reserved,
        repair_consumed: lifecycle.repair_consumed,
        terminal_outcome: lifecycle.terminal_outcome,
        terminal_reason: lifecycle.terminal_reason.as_ref().map(reason_dto),
        approval: lifecycle.approval.as_ref().map(|approval| LifecycleApprovalDto {
            approval_id: approval.id,
            approved_state_digest: approval.approved_state_digest,
            patch_id: approval.patch_id,
            verification_id: approval.verification_id,
            decision_source: approval.decision_source,
            decided_unix_ms: approval.decided_unix_ms,
        }),
        apply_id: lifecycle.apply_id,
        cleanup,
        recovery,
        created_unix_ms: lifecycle.created_unix_ms,
        updated_unix_ms: lifecycle.updated_unix_ms,
    }
}

fn recovery_dto(record: &needle_core::ChangeApplyRecord) -> LifecycleRecoveryDto {
    LifecycleRecoveryDto {
        apply_id: record.id,
        patch_id: record.patch_id,
        status: record.status,
        pre_snapshot: record.pre_snapshot,
        post_snapshot: record.post_snapshot,
        created_unix_ms: record.created_unix_ms,
        completed_unix_ms: record.completed_unix_ms,
    }
}

fn usage_dto(usage: &needle_core::LifecycleUsage) -> LifecycleUsageDto {
    LifecycleUsageDto {
        worker_turns: usage.worker_turns,
        output_tokens: usage.output_tokens,
        cost_microusd: usage.cost_microusd,
    }
}

fn reason_dto(reason: &LifecycleReason) -> LifecycleReasonDto {
    LifecycleReasonDto { code: reason.code.clone(), detail_digest: reason.detail_digest }
}

fn event_dto(event: &LifecycleEvent) -> LifecycleEventDto {
    LifecycleEventDto {
        lifecycle_id: event.lifecycle_id,
        change_id: event.change_id.clone(),
        sequence: event.sequence,
        phase: event.phase,
        status: event.status,
        source_snapshot: event.source_snapshot,
        profile_definition_digest: event.profile_revision_digest,
        patch_id: event.patch_id,
        verification_id: event.verification_id,
        prior_state_digest: event.prior_state_digest,
        resulting_state_digest: event.resulting_state_digest,
        action: event_action_dto(&event.kind),
        created_unix_ms: event.created_unix_ms,
    }
}

fn event_action_dto(kind: &LifecycleEventKind) -> LifecycleEventActionDto {
    let (kind, reason, apply_status) = match kind {
        LifecycleEventKind::Created { .. } => ("created", None, None),
        LifecycleEventKind::Transitioned { transition } => match transition.as_ref() {
            needle_core::LifecycleTransition::CompleteExplore { .. } => {
                ("complete_explore", None, None)
            }
            needle_core::LifecycleTransition::CompleteImplement { .. } => {
                ("complete_implement", None, None)
            }
            needle_core::LifecycleTransition::CompleteTest { .. } => ("complete_test", None, None),
            needle_core::LifecycleTransition::CompleteReview { .. } => {
                ("complete_review", None, None)
            }
            needle_core::LifecycleTransition::CompleteVerify { .. } => {
                ("complete_verify", None, None)
            }
            needle_core::LifecycleTransition::ConsumeRepair => ("consume_repair", None, None),
            needle_core::LifecycleTransition::ApproveApply { .. } => ("approve_apply", None, None),
            needle_core::LifecycleTransition::StartApply { .. } => ("start_apply", None, None),
            needle_core::LifecycleTransition::FinishApply { status, .. } => {
                ("finish_apply", None, Some(*status))
            }
            needle_core::LifecycleTransition::Cancel { reason } => {
                ("cancel", Some(reason_dto(reason)), None)
            }
            needle_core::LifecycleTransition::Fail { reason } => {
                ("fail", Some(reason_dto(reason)), None)
            }
        },
    };
    LifecycleEventActionDto { kind, reason, apply_status }
}

fn store_error(error: StoreError) -> Response {
    match error {
        StoreError::LifecycleNotFound(_) => {
            lifecycle_error(StatusCode::NOT_FOUND, "not_found", "lifecycle was not found")
        }
        StoreError::LifecycleQuery(_) => {
            lifecycle_error(StatusCode::BAD_REQUEST, "invalid_limit", "lifecycle query is invalid")
        }
        StoreError::LifecycleCorruption(_) | StoreError::Lifecycle(_) | StoreError::Json(_) => {
            lifecycle_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "lifecycle_corruption",
                "stored lifecycle data is corrupt",
            )
        }
        _ => lifecycle_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_error",
            "lifecycle storage is unavailable",
        ),
    }
}

fn lifecycle_error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (status, Json(LifecycleApiError { schema: ERROR_SCHEMA, code, message })).into_response()
}

#[cfg(test)]
#[path = "lifecycles/tests.rs"]
mod tests;
