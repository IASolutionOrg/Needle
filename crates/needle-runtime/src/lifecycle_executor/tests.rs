use super::*;
use needle_core::{
    AcceptanceCoverage, AcceptanceStatus, ChangeRequest, LifecycleAcceptanceReview,
    LifecycleArtifactKind, LifecycleArtifactRef, LifecycleBudget, LifecyclePatchRef,
    LifecycleReviewVerdict, LifecycleSpec, LifecycleTestPlanBinding, LifecycleTestResult,
    LifecycleVerificationRef, LifecycleWorkerCompletion, LifecycleWorkerProfiles, ReviewArtifact,
    RoleProfileId, TestPlan,
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
};

fn profile(name: &str) -> RoleProfileProvenance {
    RoleProfileProvenance::new(
        RoleProfileId::new(name).unwrap(),
        1,
        Digest::blake3(name.as_bytes()),
    )
    .unwrap()
}

fn lifecycle_spec(max_worker_turns: u32) -> LifecycleSpec {
    LifecycleSpec {
        worker_depth_limit: 1,
        profiles: LifecycleWorkerProfiles {
            explore: profile("kernel.explore"),
            implement: profile("kernel.implement"),
            test: profile("kernel.test"),
            review: profile("kernel.review"),
            verify: profile("kernel.verify"),
        },
        budget: LifecycleBudget {
            max_worker_turns,
            max_output_tokens: 100,
            max_cost_microusd: 100,
            max_concurrent_workers: 1,
        },
        test_plans: vec![LifecycleTestPlanBinding {
            plan: TestPlan {
                runner: "cargo".to_owned(),
                argv: vec!["cargo".to_owned(), "test".to_owned(), "kernel".to_owned()],
                cwd_relative: ".".to_owned(),
                test_identifier: "kernel".to_owned(),
                requires_approval: true,
                execution_evidence_id: None,
            },
            certificate_digest: Digest::blake3(b"kernel-test-certificate"),
        }],
    }
}

fn context(change_id: &ChangeId, source_snapshot: Digest) -> LifecycleChangeContext {
    let request = ChangeRequest {
        task: format!("Execute {change_id} through the lifecycle kernel."),
        acceptance_criteria: vec!["the bounded lifecycle completes".to_owned()],
        allowed_paths: Vec::new(),
        artifact_ids: Vec::new(),
        claim_ids: Vec::new(),
        constraints: Vec::new(),
    };
    LifecycleChangeContext {
        request_digest: request.digest(source_snapshot),
        request,
        repository_id: Digest::blake3(b"kernel-repository"),
        source_snapshot,
    }
}

fn projection(lifecycle: DevelopmentLifecycle) -> LifecycleProjection {
    LifecycleProjection { state_digest: lifecycle.state_digest(), lifecycle }
}

struct FakeStore {
    projection: RefCell<LifecycleProjection>,
    context: LifecycleChangeContext,
    latest_verification: RefCell<Option<VerificationArtifact>>,
    repair_calls: Cell<u32>,
    fail_next_parent_commit: Cell<bool>,
    concurrent_transition: RefCell<Option<LifecycleTransition>>,
    corrupt_replay: Cell<bool>,
}

impl FakeStore {
    fn new(max_worker_turns: u32) -> (Self, ChangeId) {
        let change_id = ChangeId::from_digest(Digest::blake3(b"kernel-change"));
        let source_snapshot = Digest::blake3(b"kernel-source");
        let lifecycle = DevelopmentLifecycle::new(
            change_id.clone(),
            source_snapshot,
            lifecycle_spec(max_worker_turns),
            1,
        )
        .unwrap();
        (
            Self {
                projection: RefCell::new(projection(lifecycle)),
                context: context(&change_id, source_snapshot),
                latest_verification: RefCell::new(None),
                repair_calls: Cell::new(0),
                fail_next_parent_commit: Cell::new(false),
                concurrent_transition: RefCell::new(None),
                corrupt_replay: Cell::new(false),
            },
            change_id,
        )
    }

    fn apply(&self, transition: LifecycleTransition) -> Result<LifecycleProjection, StoreError> {
        let current = self.projection.borrow().clone();
        let (next, _) =
            current.lifecycle.transition(transition, current.lifecycle.updated_unix_ms + 1)?;
        let next = projection(next);
        *self.projection.borrow_mut() = next.clone();
        Ok(next)
    }
}

impl LifecycleKernelStore for FakeStore {
    fn replay(&self, change_id: &ChangeId) -> Result<LifecycleProjection, StoreError> {
        if self.corrupt_replay.get() {
            return Err(StoreError::LifecycleCorruption(format!(
                "{change_id}: injected replay corruption"
            )));
        }
        Ok(self.projection.borrow().clone())
    }

    fn change_context(
        &self,
        _change_id: &ChangeId,
    ) -> Result<Option<LifecycleChangeContext>, StoreError> {
        Ok(Some(self.context.clone()))
    }

    fn latest_verification(
        &self,
        _change_id: &ChangeId,
    ) -> Result<Option<VerificationArtifact>, StoreError> {
        Ok(self.latest_verification.borrow().clone())
    }

    fn parent_transition(
        &self,
        change_id: &ChangeId,
        expected_state_digest: Digest,
        transition: LifecycleTransition,
    ) -> Result<LifecycleProjection, StoreError> {
        let current = self.projection.borrow().clone();
        if current.state_digest != expected_state_digest {
            return Err(StoreError::LifecycleConflict(format!(
                "{change_id}: stale injected state"
            )));
        }
        if self.fail_next_parent_commit.replace(false) {
            return Err(StoreError::ConnectionLock);
        }
        if let Some(concurrent) = self.concurrent_transition.borrow_mut().take() {
            self.apply(concurrent)?;
            return Err(StoreError::LifecycleConflict(format!(
                "{change_id}: injected concurrent transition"
            )));
        }

        let verification_artifact = match &transition {
            LifecycleTransition::CompleteVerify { verification, .. } => {
                let artifact = verification_artifact(
                    &current.lifecycle,
                    &self.context.request,
                    verification.verdict,
                );
                if artifact.id != verification.verification_id {
                    return Err(StoreError::LifecycleCorruption(format!(
                        "{change_id}: fake verification identity"
                    )));
                }
                Some(artifact)
            }
            _ => None,
        };
        let next = self.apply(transition)?;
        if let Some(artifact) = verification_artifact {
            *self.latest_verification.borrow_mut() = Some(artifact);
        }
        Ok(next)
    }

    fn begin_repair(&self, change_id: &ChangeId, patch_id: PatchId) -> Result<(), StoreError> {
        self.repair_calls.set(self.repair_calls.get() + 1);
        let current = self.projection.borrow().clone();
        if current.lifecycle.patch.as_ref().map(|patch| patch.patch_id) != Some(patch_id) {
            return Err(StoreError::ChangeConflict(format!(
                "{change_id}: injected repair patch mismatch"
            )));
        }
        self.apply(LifecycleTransition::ConsumeRepair)?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum FakeAdapterMode {
    Success,
    RepairOnce,
    RejectVerification,
    WrongPhase,
    WrongProfile,
    BadWorkerDepth,
    Failure,
    CleanupFailure,
    OversizedFailure,
}

struct FakeAdapter {
    mode: FakeAdapterMode,
    calls: Cell<u32>,
    side_effects: RefCell<BTreeSet<Digest>>,
    cached: RefCell<BTreeMap<Digest, LifecyclePhaseAdapterOutcome>>,
    phases: RefCell<Vec<LifecyclePhase>>,
    repair_contexts: Cell<u32>,
}

impl FakeAdapter {
    fn new(mode: FakeAdapterMode) -> Self {
        Self {
            mode,
            calls: Cell::new(0),
            side_effects: RefCell::new(BTreeSet::new()),
            cached: RefCell::new(BTreeMap::new()),
            phases: RefCell::new(Vec::new()),
            repair_contexts: Cell::new(0),
        }
    }
}

impl LifecyclePhaseAdapter for FakeAdapter {
    fn invoke(&self, request: &LifecyclePhaseAdapterRequest) -> LifecyclePhaseAdapterOutcome {
        self.calls.set(self.calls.get() + 1);
        self.phases.borrow_mut().push(request.phase);
        if request.repair_verification.is_some() {
            self.repair_contexts.set(self.repair_contexts.get() + 1);
        }
        if let Some(outcome) = self.cached.borrow().get(&request.invocation_id) {
            return outcome.clone();
        }
        self.side_effects.borrow_mut().insert(request.invocation_id);

        let usage = LifecycleUsage { worker_turns: 1, output_tokens: 1, cost_microusd: 1 };
        let cleanup = match self.mode {
            FakeAdapterMode::CleanupFailure => LifecycleAdapterCleanup {
                succeeded: false,
                failure: Some(LifecycleAdapterFailure {
                    code: "cleanup_failed".to_owned(),
                    detail: "the fake adapter could not clean up".to_owned(),
                }),
            },
            _ => LifecycleAdapterCleanup::succeeded(),
        };
        let result = match self.mode {
            FakeAdapterMode::Failure => LifecycleAdapterResult::Failed {
                failure: LifecycleAdapterFailure {
                    code: "adapter_failed".to_owned(),
                    detail: "the fake adapter failed".to_owned(),
                },
            },
            FakeAdapterMode::OversizedFailure => LifecycleAdapterResult::Failed {
                failure: LifecycleAdapterFailure {
                    code: "adapter_failed".to_owned(),
                    detail: "x".repeat(MAX_LIFECYCLE_ADAPTER_DETAIL_BYTES + 1),
                },
            },
            FakeAdapterMode::WrongPhase => LifecycleAdapterResult::Completed {
                transition: Box::new(LifecycleTransition::CompleteImplement {
                    worker: worker(request, usage.clone(), 1, false),
                    patch: LifecyclePatchRef {
                        patch_id: PatchId(Digest::blake3(b"wrong-phase-patch")),
                        revision: 1,
                    },
                }),
            },
            mode => {
                let verify_status = match mode {
                    FakeAdapterMode::RepairOnce
                        if request.phase == LifecyclePhase::Verify
                            && !request.lifecycle.repair_consumed =>
                    {
                        VerificationStatus::Repairable
                    }
                    FakeAdapterMode::RejectVerification
                        if request.phase == LifecyclePhase::Verify =>
                    {
                        VerificationStatus::Rejected
                    }
                    _ => VerificationStatus::Verified,
                };
                LifecycleAdapterResult::Completed {
                    transition: Box::new(completion_transition(
                        request,
                        usage.clone(),
                        if matches!(mode, FakeAdapterMode::BadWorkerDepth) { 2 } else { 1 },
                        matches!(mode, FakeAdapterMode::WrongProfile),
                        verify_status,
                    )),
                }
            }
        };
        let outcome = LifecyclePhaseAdapterOutcome {
            schema: LIFECYCLE_ADAPTER_OUTCOME_SCHEMA.to_owned(),
            invocation_id: request.invocation_id,
            expected_state_digest: request.expected_state_digest,
            phase: request.phase,
            usage,
            result,
            cleanup,
        };
        self.cached.borrow_mut().insert(request.invocation_id, outcome.clone());
        outcome
    }
}

fn worker(
    request: &LifecyclePhaseAdapterRequest,
    usage: LifecycleUsage,
    worker_depth: u8,
    wrong_profile: bool,
) -> LifecycleWorkerCompletion {
    LifecycleWorkerCompletion {
        profile: if wrong_profile {
            profile("kernel.wrong-profile")
        } else {
            request.profile.clone()
        },
        worker_depth,
        logical_worker_spawns: 1,
        usage,
    }
}

fn completion_transition(
    request: &LifecyclePhaseAdapterRequest,
    usage: LifecycleUsage,
    worker_depth: u8,
    wrong_profile: bool,
    verify_status: VerificationStatus,
) -> LifecycleTransition {
    let worker = worker(request, usage, worker_depth, wrong_profile);
    match request.phase {
        LifecyclePhase::Explore => LifecycleTransition::CompleteExplore {
            worker,
            artifacts: vec![LifecycleArtifactRef {
                kind: LifecycleArtifactKind::Exploration,
                id: Digest::blake3(b"kernel-exploration"),
                source_snapshot: request.lifecycle.source_snapshot,
            }],
        },
        LifecyclePhase::Implement => LifecycleTransition::CompleteImplement {
            worker,
            patch: LifecyclePatchRef {
                patch_id: PatchId(Digest::blake3(if request.lifecycle.repair_consumed {
                    b"kernel-patch-2"
                } else {
                    b"kernel-patch-1"
                })),
                revision: if request.lifecycle.repair_consumed { 2 } else { 1 },
            },
        },
        LifecyclePhase::Test => {
            let binding = &request.lifecycle.spec.test_plans[0];
            LifecycleTransition::CompleteTest {
                worker,
                results: vec![LifecycleTestResult {
                    plan_digest: binding.plan_digest(),
                    certificate_digest: binding.certificate_digest,
                    available: true,
                    executed: true,
                    passed: true,
                    evidence_id: Some("fake:test-evidence".to_owned()),
                    failure_code: None,
                }],
            }
        }
        LifecyclePhase::Review => {
            let patch_id = request.lifecycle.patch.as_ref().unwrap().patch_id;
            let review = ReviewArtifact::new(
                request.lifecycle.change_id.clone(),
                patch_id,
                LifecycleReviewVerdict::Approved,
                vec![LifecycleAcceptanceReview::new(
                    request.change.request.acceptance_criteria[0].as_bytes(),
                    AcceptanceStatus::Addressed,
                    b"fake review evidence",
                )],
                Vec::new(),
                request.profile.definition_digest,
                request.lifecycle.updated_unix_ms,
            )
            .unwrap();
            LifecycleTransition::CompleteReview { worker, review }
        }
        LifecyclePhase::Verify => {
            let verification =
                verification_artifact(&request.lifecycle, &request.change.request, verify_status);
            LifecycleTransition::CompleteVerify {
                worker,
                verification: LifecycleVerificationRef {
                    verification_id: verification.id,
                    patch_id: verification.patch_id,
                    verdict: verification.verdict,
                },
            }
        }
        LifecyclePhase::Apply => unreachable!("the kernel never invokes an apply adapter"),
    }
}

fn verification_artifact(
    lifecycle: &DevelopmentLifecycle,
    request: &ChangeRequest,
    verdict: VerificationStatus,
) -> VerificationArtifact {
    let patch_id = lifecycle.patch.as_ref().unwrap().patch_id;
    let acceptance_coverage = vec![AcceptanceCoverage {
        criterion: request.acceptance_criteria[0].clone(),
        status: AcceptanceStatus::Addressed,
        evidence: "fake verification evidence".to_owned(),
    }];
    let findings = if verdict == VerificationStatus::Repairable {
        vec!["one bounded repair is required".to_owned()]
    } else {
        Vec::new()
    };
    let test_evidence_ids = Vec::new();
    let verifier_definition = lifecycle.spec.profiles.verify.definition_digest;
    let id = VerificationArtifact::compute_id(
        &lifecycle.change_id,
        patch_id,
        verdict,
        &acceptance_coverage,
        &findings,
        &test_evidence_ids,
        verifier_definition,
    );
    let artifact = VerificationArtifact {
        id,
        change_id: lifecycle.change_id.clone(),
        patch_id,
        verdict,
        acceptance_coverage,
        findings,
        test_evidence_ids,
        test_plan_results: Vec::new(),
        test_plans_over_cap: false,
        verifier_definition,
        created_unix_ms: lifecycle.updated_unix_ms,
    };
    assert!(artifact.is_canonical());
    artifact
}

fn adapters(adapter: &FakeAdapter) -> LifecyclePhaseAdapters<'_> {
    LifecyclePhaseAdapters {
        explore: adapter,
        implement: adapter,
        test: adapter,
        review: adapter,
        verify: adapter,
    }
}

fn step(
    store: &FakeStore,
    adapter: &FakeAdapter,
    cancellation: &dyn LifecycleCancellation,
    change_id: &ChangeId,
) -> Result<LifecycleExecutionOutcome, LifecycleExecutionError> {
    execute_step(store, &adapters(adapter), cancellation, change_id)
}

struct CancelOnCheck {
    cancel_on: u32,
    checks: Cell<u32>,
}

impl LifecycleCancellation for CancelOnCheck {
    fn is_cancelled(&self, _change_id: &ChangeId, _expected_state_digest: Digest) -> bool {
        let check = self.checks.get() + 1;
        self.checks.set(check);
        check == self.cancel_on
    }
}

#[test]
fn success_invokes_one_adapter_per_phase_and_stops_at_approval() {
    let (store, change_id) = FakeStore::new(10);
    let adapter = FakeAdapter::new(FakeAdapterMode::Success);
    let mut last = None;

    for _ in 0..5 {
        last = Some(step(&store, &adapter, &NeverCancel, &change_id).unwrap());
    }

    let last = last.unwrap();
    assert_eq!(last.disposition, LifecycleExecutionDisposition::AwaitingApproval);
    assert_eq!(last.projection.lifecycle.phase, LifecyclePhase::Apply);
    assert_eq!(last.projection.lifecycle.status, LifecycleStatus::AwaitingApproval);
    assert_eq!(last.projection.lifecycle.usage.worker_turns, 5);
    assert_eq!(adapter.calls.get(), 5);
    assert_eq!(adapter.side_effects.borrow().len(), 5);
    assert_eq!(
        adapter.phases.borrow().as_slice(),
        &[
            LifecyclePhase::Explore,
            LifecyclePhase::Implement,
            LifecyclePhase::Test,
            LifecyclePhase::Review,
            LifecyclePhase::Verify,
        ]
    );

    let stopped = step(&store, &adapter, &NeverCancel, &change_id).unwrap();
    assert_eq!(stopped.disposition, LifecycleExecutionDisposition::AwaitingApproval);
    assert_eq!(adapter.calls.get(), 5);
}

#[test]
fn repair_is_consumed_transactionally_once_then_resumes_implement() {
    let (store, change_id) = FakeStore::new(12);
    let adapter = FakeAdapter::new(FakeAdapterMode::RepairOnce);
    for _ in 0..5 {
        step(&store, &adapter, &NeverCancel, &change_id).unwrap();
    }
    assert_eq!(store.projection.borrow().lifecycle.status, LifecycleStatus::RepairReserved);

    let resumed = step(&store, &adapter, &NeverCancel, &change_id).unwrap();
    assert_eq!(resumed.disposition, LifecycleExecutionDisposition::RepairResumed);
    assert_eq!(resumed.projection.lifecycle.phase, LifecyclePhase::Implement);
    assert!(resumed.projection.lifecycle.repair_consumed);
    assert_eq!(store.repair_calls.get(), 1);
    assert_eq!(adapter.calls.get(), 5);

    for _ in 0..4 {
        step(&store, &adapter, &NeverCancel, &change_id).unwrap();
    }
    let finished = store.projection.borrow().clone();
    assert_eq!(finished.lifecycle.status, LifecycleStatus::AwaitingApproval);
    assert_eq!(finished.lifecycle.patch.as_ref().unwrap().revision, 2);
    assert_eq!(finished.lifecycle.usage.worker_turns, 9);
    assert_eq!(store.repair_calls.get(), 1);
    assert_eq!(adapter.calls.get(), 9);
    assert_eq!(adapter.repair_contexts.get(), 4);
}

#[test]
fn cancellation_wins_before_a_reserved_repair_is_consumed() {
    let (store, change_id) = FakeStore::new(12);
    let adapter = FakeAdapter::new(FakeAdapterMode::RepairOnce);
    for _ in 0..5 {
        step(&store, &adapter, &NeverCancel, &change_id).unwrap();
    }
    assert_eq!(store.projection.borrow().lifecycle.status, LifecycleStatus::RepairReserved);
    let cancellation = CancelOnCheck { cancel_on: 1, checks: Cell::new(0) };

    let outcome = step(&store, &adapter, &cancellation, &change_id).unwrap();

    assert_eq!(outcome.disposition, LifecycleExecutionDisposition::Cancelled);
    assert_eq!(outcome.projection.lifecycle.status, LifecycleStatus::Cancelled);
    assert_eq!(store.repair_calls.get(), 0);
    assert_eq!(adapter.calls.get(), 5);
}

#[test]
fn rejection_is_a_typed_terminal_completion() {
    let (store, change_id) = FakeStore::new(10);
    let adapter = FakeAdapter::new(FakeAdapterMode::RejectVerification);
    let mut outcome = None;
    for _ in 0..5 {
        outcome = Some(step(&store, &adapter, &NeverCancel, &change_id).unwrap());
    }
    let outcome = outcome.unwrap();
    assert_eq!(outcome.disposition, LifecycleExecutionDisposition::Terminal);
    assert_eq!(outcome.projection.lifecycle.status, LifecycleStatus::Failed);
    assert_eq!(
        outcome.projection.lifecycle.terminal_reason.as_ref().unwrap().code,
        "verification_rejected"
    );
}

#[test]
fn phase_profile_mismatch_and_bad_depth_fail_closed_as_invalid_output() {
    for mode in [
        FakeAdapterMode::WrongPhase,
        FakeAdapterMode::WrongProfile,
        FakeAdapterMode::BadWorkerDepth,
    ] {
        let (store, change_id) = FakeStore::new(10);
        let adapter = FakeAdapter::new(mode);
        let outcome = step(&store, &adapter, &NeverCancel, &change_id).unwrap();
        assert_eq!(outcome.disposition, LifecycleExecutionDisposition::InvalidAdapterOutput);
        assert_eq!(outcome.projection.lifecycle.status, LifecycleStatus::Failed);
        assert_eq!(
            outcome.projection.lifecycle.terminal_reason.as_ref().unwrap().code,
            "adapter_invalid_output"
        );
    }
}

#[test]
fn cancellation_before_and_after_adapter_is_fail_closed() {
    let (before_store, before_id) = FakeStore::new(10);
    let before_adapter = FakeAdapter::new(FakeAdapterMode::Success);
    let before = CancelOnCheck { cancel_on: 1, checks: Cell::new(0) };
    let outcome = step(&before_store, &before_adapter, &before, &before_id).unwrap();
    assert_eq!(outcome.disposition, LifecycleExecutionDisposition::Cancelled);
    assert_eq!(outcome.projection.lifecycle.status, LifecycleStatus::Cancelled);
    assert_eq!(before_adapter.calls.get(), 0);

    let (after_store, after_id) = FakeStore::new(10);
    let after_adapter = FakeAdapter::new(FakeAdapterMode::Success);
    let after = CancelOnCheck { cancel_on: 2, checks: Cell::new(0) };
    let outcome = step(&after_store, &after_adapter, &after, &after_id).unwrap();
    assert_eq!(outcome.disposition, LifecycleExecutionDisposition::Cancelled);
    assert_eq!(outcome.projection.lifecycle.status, LifecycleStatus::Cancelled);
    assert_eq!(outcome.adapter_usage.worker_turns, 1);
    assert_eq!(after_adapter.calls.get(), 1);
}

#[test]
fn exhausted_aggregate_budget_stops_before_another_adapter() {
    let (store, change_id) = FakeStore::new(1);
    let adapter = FakeAdapter::new(FakeAdapterMode::Success);
    step(&store, &adapter, &NeverCancel, &change_id).unwrap();

    let outcome = step(&store, &adapter, &NeverCancel, &change_id).unwrap();
    assert_eq!(outcome.disposition, LifecycleExecutionDisposition::BudgetExhausted);
    assert_eq!(outcome.projection.lifecycle.status, LifecycleStatus::Failed);
    assert_eq!(
        outcome.projection.lifecycle.terminal_reason.as_ref().unwrap().code,
        "budget_exhausted"
    );
    assert_eq!(adapter.calls.get(), 1);
}

#[test]
fn adapter_and_cleanup_failures_are_distinct_terminal_outcomes() {
    let (failed_store, failed_id) = FakeStore::new(10);
    let failed_adapter = FakeAdapter::new(FakeAdapterMode::Failure);
    let failed = step(&failed_store, &failed_adapter, &NeverCancel, &failed_id).unwrap();
    assert_eq!(failed.disposition, LifecycleExecutionDisposition::AdapterFailed);
    assert_eq!(
        failed.projection.lifecycle.terminal_reason.as_ref().unwrap().code,
        "adapter_failed"
    );

    let (cleanup_store, cleanup_id) = FakeStore::new(10);
    let cleanup_adapter = FakeAdapter::new(FakeAdapterMode::CleanupFailure);
    let cleanup = step(&cleanup_store, &cleanup_adapter, &NeverCancel, &cleanup_id).unwrap();
    assert_eq!(cleanup.disposition, LifecycleExecutionDisposition::CleanupFailed);
    assert_eq!(
        cleanup.projection.lifecycle.terminal_reason.as_ref().unwrap().code,
        "adapter_cleanup_failed"
    );
    assert!(!cleanup.cleanup.unwrap().succeeded);
}

#[test]
fn oversized_adapter_failure_is_bounded_and_rejected() {
    let (store, change_id) = FakeStore::new(10);
    let adapter = FakeAdapter::new(FakeAdapterMode::OversizedFailure);
    let outcome = step(&store, &adapter, &NeverCancel, &change_id).unwrap();
    assert_eq!(outcome.disposition, LifecycleExecutionDisposition::InvalidAdapterOutput);
    assert_eq!(
        outcome.projection.lifecycle.terminal_reason.as_ref().unwrap().code,
        "adapter_invalid_output"
    );
}

#[test]
fn stale_cas_does_not_overwrite_a_concurrent_parent_transition() {
    let (store, change_id) = FakeStore::new(10);
    let adapter = FakeAdapter::new(FakeAdapterMode::Success);
    store.concurrent_transition.replace(Some(LifecycleTransition::Cancel {
        reason: LifecycleReason::new("concurrent_cancel", b"concurrent parent won").unwrap(),
    }));

    let outcome = step(&store, &adapter, &NeverCancel, &change_id).unwrap();
    assert_eq!(outcome.disposition, LifecycleExecutionDisposition::StaleState);
    assert_eq!(outcome.projection.lifecycle.status, LifecycleStatus::Cancelled);
    assert!(outcome.projection.lifecycle.exploration_artifacts.is_empty());
}

#[test]
fn retry_reuses_invocation_identity_without_repeating_adapter_side_effect() {
    let (store, change_id) = FakeStore::new(10);
    let adapter = FakeAdapter::new(FakeAdapterMode::Success);
    let initial_digest = store.projection.borrow().state_digest;
    store.fail_next_parent_commit.set(true);

    let first = step(&store, &adapter, &NeverCancel, &change_id);
    assert!(matches!(first, Err(LifecycleExecutionError::Store(StoreError::ConnectionLock))));
    assert_eq!(store.projection.borrow().state_digest, initial_digest);

    let second = step(&store, &adapter, &NeverCancel, &change_id).unwrap();
    assert_eq!(second.disposition, LifecycleExecutionDisposition::Advanced);
    assert_eq!(second.projection.lifecycle.phase, LifecyclePhase::Implement);
    assert_eq!(adapter.calls.get(), 2);
    assert_eq!(adapter.side_effects.borrow().len(), 1);
    assert_eq!(
        adapter.phases.borrow().as_slice(),
        &[LifecyclePhase::Explore, LifecyclePhase::Explore]
    );
}

#[test]
fn replay_corruption_is_rejected_before_adapter_invocation() {
    let (store, change_id) = FakeStore::new(10);
    let adapter = FakeAdapter::new(FakeAdapterMode::Success);
    store.corrupt_replay.set(true);

    let outcome = step(&store, &adapter, &NeverCancel, &change_id);
    assert!(matches!(
        outcome,
        Err(LifecycleExecutionError::Store(StoreError::LifecycleCorruption(_)))
    ));
    assert_eq!(adapter.calls.get(), 0);
}
