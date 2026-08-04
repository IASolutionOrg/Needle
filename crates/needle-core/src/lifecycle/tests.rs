use super::*;
use crate::{
    AcceptanceCoverage, AcceptanceStatus, RoleProfileId, RoleProfileProvenance, TestPlan,
    VerificationArtifact, VerificationPlanResult,
};

fn profile(name: &str) -> RoleProfileProvenance {
    RoleProfileProvenance::new(
        RoleProfileId::new(name).unwrap(),
        1,
        Digest::blake3(name.as_bytes()),
    )
    .unwrap()
}

fn spec() -> LifecycleSpec {
    LifecycleSpec {
        worker_depth_limit: 1,
        profiles: LifecycleWorkerProfiles {
            explore: profile("lifecycle.explorer"),
            implement: profile("lifecycle.implementer"),
            test: profile("lifecycle.test-runner"),
            review: profile("lifecycle.reviewer"),
            verify: profile("lifecycle.verifier"),
        },
        budget: LifecycleBudget {
            max_worker_turns: 10,
            max_output_tokens: 10_000,
            max_cost_microusd: 100_000,
            max_concurrent_workers: 1,
        },
        test_plans: vec![LifecycleTestPlanBinding {
            plan: TestPlan {
                runner: "cargo".to_owned(),
                argv: vec![
                    "cargo".to_owned(),
                    "test".to_owned(),
                    "--offline".to_owned(),
                    "focused".to_owned(),
                    "--".to_owned(),
                    "--exact".to_owned(),
                ],
                cwd_relative: ".".to_owned(),
                test_identifier: "focused".to_owned(),
                requires_approval: true,
                execution_evidence_id: None,
            },
            certificate_digest: Digest::blake3(b"test-certificate"),
        }],
    }
}

fn worker(profile: &RoleProfileProvenance) -> LifecycleWorkerCompletion {
    LifecycleWorkerCompletion {
        profile: profile.clone(),
        worker_depth: 1,
        logical_worker_spawns: 1,
        usage: LifecycleUsage { worker_turns: 1, output_tokens: 10, cost_microusd: 10 },
    }
}

fn lifecycle() -> DevelopmentLifecycle {
    DevelopmentLifecycle::new(
        ChangeId::from_digest(Digest::blake3(b"change")),
        Digest::blake3(b"source"),
        spec(),
        1,
    )
    .unwrap()
}

fn coverage() -> Vec<LifecycleAcceptanceReview> {
    vec![LifecycleAcceptanceReview::new(
        b"focused behavior works",
        AcceptanceStatus::Addressed,
        b"bounded evidence",
    )]
}

fn verification_coverage() -> Vec<AcceptanceCoverage> {
    vec![AcceptanceCoverage {
        criterion: "focused behavior works".to_owned(),
        status: AcceptanceStatus::Addressed,
        evidence: "bounded evidence".to_owned(),
    }]
}

fn advance_to_verify(mut state: DevelopmentLifecycle) -> DevelopmentLifecycle {
    let source_snapshot = state.source_snapshot;
    (state, _) = state
        .transition(
            LifecycleTransition::CompleteExplore {
                worker: worker(&state.spec.profiles.explore),
                artifacts: vec![LifecycleArtifactRef {
                    kind: LifecycleArtifactKind::Exploration,
                    id: Digest::blake3(b"exploration"),
                    source_snapshot,
                }],
            },
            2,
        )
        .unwrap();
    let patch_material: &[u8] = if state.repair_consumed { b"repaired-patch" } else { b"patch" };
    let patch_id = PatchId(Digest::blake3(patch_material));
    (state, _) = state
        .transition(
            LifecycleTransition::CompleteImplement {
                worker: worker(&state.spec.profiles.implement),
                patch: LifecyclePatchRef {
                    patch_id,
                    revision: if state.repair_consumed { 2 } else { 1 },
                },
            },
            3,
        )
        .unwrap();
    let binding = state.spec.test_plans[0].clone();
    (state, _) = state
        .transition(
            LifecycleTransition::CompleteTest {
                worker: worker(&state.spec.profiles.test),
                results: vec![LifecycleTestResult {
                    plan_digest: binding.plan_digest(),
                    certificate_digest: binding.certificate_digest,
                    available: true,
                    executed: true,
                    passed: true,
                    evidence_id: Some("evidence:focused".to_owned()),
                    failure_code: None,
                }],
            },
            4,
        )
        .unwrap();
    let review = ReviewArtifact::new(
        state.change_id.clone(),
        patch_id,
        LifecycleReviewVerdict::Approved,
        coverage(),
        Vec::new(),
        state.spec.profiles.review.definition_digest,
        5,
    )
    .unwrap();
    state
        .transition(
            LifecycleTransition::CompleteReview {
                worker: worker(&state.spec.profiles.review),
                review,
            },
            5,
        )
        .unwrap()
        .0
}

fn verify_transition(
    state: &DevelopmentLifecycle,
    verdict: VerificationStatus,
    identity: &[u8],
) -> LifecycleTransition {
    LifecycleTransition::CompleteVerify {
        worker: worker(&state.spec.profiles.verify),
        verification: LifecycleVerificationRef {
            verification_id: VerificationArtifactId(Digest::blake3(identity)),
            patch_id: state.patch.as_ref().unwrap().patch_id,
            verdict,
        },
    }
}

fn candidate_worker_transition(
    state: &DevelopmentLifecycle,
    phase: LifecyclePhase,
) -> LifecycleTransition {
    let patch_id = state
        .patch
        .as_ref()
        .map(|patch| patch.patch_id)
        .unwrap_or(PatchId(Digest::blake3(b"candidate-patch")));
    match phase {
        LifecyclePhase::Explore => LifecycleTransition::CompleteExplore {
            worker: worker(&state.spec.profiles.explore),
            artifacts: vec![LifecycleArtifactRef {
                kind: LifecycleArtifactKind::Exploration,
                id: Digest::blake3(b"candidate-exploration"),
                source_snapshot: state.source_snapshot,
            }],
        },
        LifecyclePhase::Implement => LifecycleTransition::CompleteImplement {
            worker: worker(&state.spec.profiles.implement),
            patch: LifecyclePatchRef {
                patch_id,
                revision: if state.repair_consumed { 2 } else { 1 },
            },
        },
        LifecyclePhase::Test => {
            let binding = &state.spec.test_plans[0];
            LifecycleTransition::CompleteTest {
                worker: worker(&state.spec.profiles.test),
                results: vec![LifecycleTestResult {
                    plan_digest: binding.plan_digest(),
                    certificate_digest: binding.certificate_digest,
                    available: true,
                    executed: true,
                    passed: true,
                    evidence_id: Some("evidence:candidate".to_owned()),
                    failure_code: None,
                }],
            }
        }
        LifecyclePhase::Review => LifecycleTransition::CompleteReview {
            worker: worker(&state.spec.profiles.review),
            review: ReviewArtifact::new(
                state.change_id.clone(),
                patch_id,
                LifecycleReviewVerdict::Approved,
                coverage(),
                Vec::new(),
                state.spec.profiles.review.definition_digest,
                state.updated_unix_ms,
            )
            .unwrap(),
        },
        LifecyclePhase::Verify => LifecycleTransition::CompleteVerify {
            worker: worker(&state.spec.profiles.verify),
            verification: LifecycleVerificationRef {
                verification_id: VerificationArtifactId(Digest::blake3(b"candidate-verify")),
                patch_id,
                verdict: VerificationStatus::Verified,
            },
        },
        LifecyclePhase::Apply => unreachable!("apply has no worker completion"),
    }
}

#[test]
fn exact_phase_order_and_distinct_review_verifier_reach_approval() {
    let mut state = lifecycle();
    let mut events = vec![LifecycleEvent::created(&state).unwrap()];
    let source = state.source_snapshot;
    let explore = LifecycleTransition::CompleteExplore {
        worker: worker(&state.spec.profiles.explore),
        artifacts: vec![LifecycleArtifactRef {
            kind: LifecycleArtifactKind::Exploration,
            id: Digest::blake3(b"exploration"),
            source_snapshot: source,
        }],
    };
    (state, _) = state.transition(explore, 2).unwrap();
    let patch_id = PatchId(Digest::blake3(b"patch"));
    (state, _) = state
        .transition(
            LifecycleTransition::CompleteImplement {
                worker: worker(&state.spec.profiles.implement),
                patch: LifecyclePatchRef { patch_id, revision: 1 },
            },
            3,
        )
        .unwrap();
    let binding = &state.spec.test_plans[0];
    (state, _) = state
        .transition(
            LifecycleTransition::CompleteTest {
                worker: worker(&state.spec.profiles.test),
                results: vec![LifecycleTestResult {
                    plan_digest: binding.plan_digest(),
                    certificate_digest: binding.certificate_digest,
                    available: true,
                    executed: true,
                    passed: true,
                    evidence_id: Some("evidence:focused".to_owned()),
                    failure_code: None,
                }],
            },
            4,
        )
        .unwrap();
    let review = ReviewArtifact::new(
        state.change_id.clone(),
        patch_id,
        LifecycleReviewVerdict::Approved,
        coverage(),
        Vec::new(),
        state.spec.profiles.review.definition_digest,
        5,
    )
    .unwrap();
    (state, _) = state
        .transition(
            LifecycleTransition::CompleteReview {
                worker: worker(&state.spec.profiles.review),
                review,
            },
            5,
        )
        .unwrap();
    let verification_id = VerificationArtifactId(Digest::blake3(b"verification"));
    let (next, event) = state
        .transition(
            LifecycleTransition::CompleteVerify {
                worker: worker(&state.spec.profiles.verify),
                verification: LifecycleVerificationRef {
                    verification_id,
                    patch_id,
                    verdict: VerificationStatus::Verified,
                },
            },
            6,
        )
        .unwrap();
    state = next;
    events.push(event);
    assert_eq!(state.phase, LifecyclePhase::Apply);
    assert_eq!(state.status, LifecycleStatus::AwaitingApproval);
    assert_ne!(state.review.as_ref().unwrap().id, verification_id.0);
    let approval = LifecycleApplyApproval::new(
        state.state_digest(),
        patch_id,
        verification_id,
        crate::ApprovalDecisionSource::WebUser,
        7,
    );
    (state, _) = state.transition(LifecycleTransition::ApproveApply { approval }, 7).unwrap();
    assert_eq!(state.status, LifecycleStatus::Approved);
    assert_eq!(events[0].sequence, 0);
}

#[test]
fn skips_duplicates_cycles_and_nested_workers_leave_state_unchanged() {
    let state = lifecycle();
    let original = state.clone();
    let patch = LifecycleTransition::CompleteImplement {
        worker: worker(&state.spec.profiles.implement),
        patch: LifecyclePatchRef { patch_id: PatchId(Digest::blake3(b"patch")), revision: 1 },
    };
    assert_eq!(state.transition(patch, 2), Err(LifecycleError::InvalidTransition));
    let mut nested = worker(&state.spec.profiles.explore);
    nested.logical_worker_spawns = 2;
    assert_eq!(
        state.transition(
            LifecycleTransition::CompleteExplore {
                worker: nested,
                artifacts: vec![LifecycleArtifactRef {
                    kind: LifecycleArtifactKind::Exploration,
                    id: Digest::blake3(b"artifact"),
                    source_snapshot: state.source_snapshot,
                }],
            },
            2,
        ),
        Err(LifecycleError::NestedWorker)
    );
    assert_eq!(state, original);

    let advanced = state
        .transition(
            LifecycleTransition::CompleteExplore {
                worker: worker(&state.spec.profiles.explore),
                artifacts: vec![LifecycleArtifactRef {
                    kind: LifecycleArtifactKind::Exploration,
                    id: Digest::blake3(b"artifact"),
                    source_snapshot: state.source_snapshot,
                }],
            },
            2,
        )
        .unwrap()
        .0;
    assert_eq!(
        advanced.transition(
            LifecycleTransition::CompleteImplement {
                worker: worker(&advanced.spec.profiles.implement),
                patch: LifecyclePatchRef {
                    patch_id: PatchId(Digest::blake3(b"stale-revision")),
                    revision: 2,
                },
            },
            3,
        ),
        Err(LifecycleError::InvalidArtifact)
    );
}

#[test]
fn transition_matrix_rejects_every_out_of_phase_worker_completion() {
    let mut states = vec![lifecycle()];
    let mut state = states[0].clone();
    for (phase, timestamp) in [
        (LifecyclePhase::Explore, 2),
        (LifecyclePhase::Implement, 3),
        (LifecyclePhase::Test, 4),
        (LifecyclePhase::Review, 5),
    ] {
        state = state.transition(candidate_worker_transition(&state, phase), timestamp).unwrap().0;
        states.push(state.clone());
    }
    assert_eq!(
        states.iter().map(|state| state.phase).collect::<Vec<_>>(),
        [
            LifecyclePhase::Explore,
            LifecyclePhase::Implement,
            LifecyclePhase::Test,
            LifecyclePhase::Review,
            LifecyclePhase::Verify,
        ]
    );
    for state in states {
        for phase in LifecyclePhase::ALL.into_iter().take(5) {
            if phase == state.phase {
                continue;
            }
            let original = state.clone();
            assert_eq!(
                state.transition(
                    candidate_worker_transition(&state, phase),
                    state.updated_unix_ms + 1,
                ),
                Err(LifecycleError::InvalidTransition),
                "{} must reject {} completion",
                state.phase.as_str(),
                phase.as_str(),
            );
            assert_eq!(state, original);
        }
    }
}

#[test]
fn unavailable_test_evidence_is_terminal_inconclusive() {
    let mut state = lifecycle();
    (state, _) = state
        .transition(
            LifecycleTransition::CompleteExplore {
                worker: worker(&state.spec.profiles.explore),
                artifacts: vec![LifecycleArtifactRef {
                    kind: LifecycleArtifactKind::Exploration,
                    id: Digest::blake3(b"artifact"),
                    source_snapshot: state.source_snapshot,
                }],
            },
            2,
        )
        .unwrap();
    (state, _) = state
        .transition(
            LifecycleTransition::CompleteImplement {
                worker: worker(&state.spec.profiles.implement),
                patch: LifecyclePatchRef {
                    patch_id: PatchId(Digest::blake3(b"patch")),
                    revision: 1,
                },
            },
            3,
        )
        .unwrap();
    let binding = &state.spec.test_plans[0];
    (state, _) = state
        .transition(
            LifecycleTransition::CompleteTest {
                worker: worker(&state.spec.profiles.test),
                results: vec![LifecycleTestResult {
                    plan_digest: binding.plan_digest(),
                    certificate_digest: binding.certificate_digest,
                    available: false,
                    executed: false,
                    passed: false,
                    evidence_id: None,
                    failure_code: Some("unavailable".to_owned()),
                }],
            },
            4,
        )
        .unwrap();
    assert_eq!(state.status, LifecycleStatus::Inconclusive);
    assert_eq!(state.terminal_outcome, Some(LifecycleTerminalOutcome::Inconclusive));
}

#[test]
fn replay_rejects_tampering() {
    let state = lifecycle();
    let mut events = vec![LifecycleEvent::created(&state).unwrap()];
    let (next, event) = state
        .transition(
            LifecycleTransition::Cancel {
                reason: LifecycleReason::new("cancelled", b"user cancelled").unwrap(),
            },
            2,
        )
        .unwrap();
    events.push(event);
    assert_eq!(DevelopmentLifecycle::replay(&events).unwrap(), next);
    let mut envelope_tamper = events.clone();
    envelope_tamper[0].phase = LifecyclePhase::Implement;
    assert_eq!(DevelopmentLifecycle::replay(&envelope_tamper), Err(LifecycleError::EventReplay));
    events[1].resulting_state_digest = Digest::blake3(b"tampered");
    assert_eq!(DevelopmentLifecycle::replay(&events), Err(LifecycleError::EventReplay));
}

#[test]
fn budget_profile_and_zero_usage_fail_without_mutation() {
    let state = lifecycle();
    let original = state.clone();
    let artifacts = vec![LifecycleArtifactRef {
        kind: LifecycleArtifactKind::Exploration,
        id: Digest::blake3(b"exploration"),
        source_snapshot: state.source_snapshot,
    }];
    let mut over_budget = worker(&state.spec.profiles.explore);
    over_budget.usage.worker_turns = state.spec.budget.max_worker_turns + 1;
    assert_eq!(
        state.transition(
            LifecycleTransition::CompleteExplore {
                worker: over_budget,
                artifacts: artifacts.clone(),
            },
            2,
        ),
        Err(LifecycleError::BudgetExceeded)
    );
    let mut wrong_profile = worker(&state.spec.profiles.explore);
    wrong_profile.profile = state.spec.profiles.review.clone();
    assert_eq!(
        state.transition(
            LifecycleTransition::CompleteExplore {
                worker: wrong_profile,
                artifacts: artifacts.clone(),
            },
            2,
        ),
        Err(LifecycleError::ProfileMismatch)
    );
    let mut zero_usage = worker(&state.spec.profiles.explore);
    zero_usage.usage.worker_turns = 0;
    assert_eq!(
        state
            .transition(LifecycleTransition::CompleteExplore { worker: zero_usage, artifacts }, 2,),
        Err(LifecycleError::InvalidWorkerCompletion)
    );
    assert_eq!(state, original);
}

#[test]
fn one_repair_is_consumed_and_second_request_fails_terminally() {
    let mut state = advance_to_verify(lifecycle());
    (state, _) = state
        .transition(verify_transition(&state, VerificationStatus::Repairable, b"repair-1"), 6)
        .unwrap();
    assert_eq!(state.status, LifecycleStatus::RepairReserved);
    (state, _) = state.transition(LifecycleTransition::ConsumeRepair, 7).unwrap();
    assert_eq!(state.phase, LifecyclePhase::Implement);
    assert!(state.repair_consumed);

    let patch_id = PatchId(Digest::blake3(b"repaired-patch"));
    (state, _) = state
        .transition(
            LifecycleTransition::CompleteImplement {
                worker: worker(&state.spec.profiles.implement),
                patch: LifecyclePatchRef { patch_id, revision: 2 },
            },
            8,
        )
        .unwrap();
    let binding = state.spec.test_plans[0].clone();
    (state, _) = state
        .transition(
            LifecycleTransition::CompleteTest {
                worker: worker(&state.spec.profiles.test),
                results: vec![LifecycleTestResult {
                    plan_digest: binding.plan_digest(),
                    certificate_digest: binding.certificate_digest,
                    available: true,
                    executed: true,
                    passed: true,
                    evidence_id: Some("evidence:repair".to_owned()),
                    failure_code: None,
                }],
            },
            9,
        )
        .unwrap();
    let review = ReviewArtifact::new(
        state.change_id.clone(),
        patch_id,
        LifecycleReviewVerdict::Approved,
        coverage(),
        Vec::new(),
        state.spec.profiles.review.definition_digest,
        10,
    )
    .unwrap();
    (state, _) = state
        .transition(
            LifecycleTransition::CompleteReview {
                worker: worker(&state.spec.profiles.review),
                review,
            },
            10,
        )
        .unwrap();
    (state, _) = state
        .transition(verify_transition(&state, VerificationStatus::Repairable, b"repair-2"), 11)
        .unwrap();
    assert_eq!(state.status, LifecycleStatus::Failed);
    assert_eq!(state.terminal_outcome, Some(LifecycleTerminalOutcome::Failed));
    assert_eq!(state.terminal_reason.as_ref().unwrap().code, "repair_limit_exhausted");
    assert_eq!(
        state.transition(LifecycleTransition::ConsumeRepair, 12),
        Err(LifecycleError::Terminal)
    );
}

#[test]
fn stale_or_noncanonical_approval_cannot_unlock_apply() {
    let mut state = advance_to_verify(lifecycle());
    let verification_id = VerificationArtifactId(Digest::blake3(b"verified"));
    (state, _) = state
        .transition(verify_transition(&state, VerificationStatus::Verified, b"verified"), 6)
        .unwrap();
    let original = state.clone();
    let patch_id = state.patch.as_ref().unwrap().patch_id;
    let stale = LifecycleApplyApproval::new(
        Digest::blake3(b"stale-state"),
        patch_id,
        verification_id,
        crate::ApprovalDecisionSource::WebUser,
        7,
    );
    assert_eq!(
        state.transition(LifecycleTransition::ApproveApply { approval: stale }, 7),
        Err(LifecycleError::StaleApproval)
    );
    let mut noncanonical = LifecycleApplyApproval::new(
        state.state_digest(),
        patch_id,
        verification_id,
        crate::ApprovalDecisionSource::WebUser,
        7,
    );
    noncanonical.id = Digest::blake3(b"forged-approval");
    assert_eq!(
        state.transition(LifecycleTransition::ApproveApply { approval: noncanonical }, 7,),
        Err(LifecycleError::StaleApproval)
    );
    assert_eq!(state, original);
}

#[test]
fn impossible_projection_shape_is_rejected() {
    let mut state = lifecycle();
    state.phase = LifecyclePhase::Apply;
    state.status = LifecycleStatus::AwaitingApproval;
    assert_eq!(state.validate(), Err(LifecycleError::InvalidState));

    let mut approved_partial = coverage();
    approved_partial[0].status = AcceptanceStatus::Partial;
    assert_eq!(
        ReviewArtifact::new(
            state.change_id,
            PatchId(Digest::blake3(b"patch")),
            LifecycleReviewVerdict::Approved,
            approved_partial,
            Vec::new(),
            Digest::blake3(b"reviewer"),
            2,
        ),
        Err(LifecycleError::InvalidReview)
    );

    let mut absolute_path_plan = spec();
    absolute_path_plan.test_plans[0].plan.argv.push("/private/secret".to_owned());
    assert_eq!(absolute_path_plan.validate(), Err(LifecycleError::InvalidTestPlan));
}

#[test]
fn review_artifact_cannot_be_substituted_for_verification() {
    let patch_id = PatchId(Digest::blake3(b"patch"));
    let review = ReviewArtifact::new(
        ChangeId::from_digest(Digest::blake3(b"change")),
        patch_id,
        LifecycleReviewVerdict::Approved,
        coverage(),
        Vec::new(),
        Digest::blake3(b"reviewer"),
        1,
    )
    .unwrap();
    let verification = VerificationArtifact {
        id: VerificationArtifactId(review.id),
        change_id: review.change_id.clone(),
        patch_id,
        verdict: VerificationStatus::Verified,
        acceptance_coverage: verification_coverage(),
        findings: Vec::new(),
        test_evidence_ids: vec!["evidence:focused".to_owned()],
        test_plan_results: vec![VerificationPlanResult {
            plan_digest: Digest::blake3(b"plan"),
            runner: "cargo".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned()],
            cwd_relative: ".".to_owned(),
            test_identifier: "focused".to_owned(),
            expected: true,
            available: true,
            executed: true,
            passed: true,
            evidence_id: Some("evidence:focused".to_owned()),
            failure_reason: None,
        }],
        test_plans_over_cap: false,
        verifier_definition: Digest::blake3(b"verifier"),
        created_unix_ms: 1,
    };
    assert!(!verification.is_canonical());
}
