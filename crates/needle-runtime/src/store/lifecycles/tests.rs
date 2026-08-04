use super::*;
use needle_core::{
    AcceptanceCoverage, AcceptanceStatus, AllowedPath, AllowedPathScope, ApprovalDecisionSource,
    Artifact, ArtifactContract, ArtifactValidationCertificate, CacheScope, ChangeApplyId,
    ChangeApplyRecord, ChangeApplyStatus, CodexHost, CommandExecutionEvidence, CommandPolicy,
    CoverageManifest, DependencyManifest, FallbackPolicy, FilesystemPolicy,
    LifecycleAcceptanceReview, LifecycleArtifactKind, LifecycleArtifactRef, LifecycleBudget,
    LifecyclePatchRef, LifecycleReviewVerdict, LifecycleTestPlanBinding, LifecycleTestResult,
    LifecycleUsage, LifecycleVerificationRef, LifecycleWorkerCompletion, LocationRole,
    NetworkPolicy, PatchArtifact, PatchFile, PatchOperation, ReasoningLevel, RepairPolicy,
    ReviewArtifact, RoleProfileBudget, RoleProfileDefinition, RoleProfileDefinitionInput,
    RoleProfileState, SemanticLocation, SemanticWorkerArtifact, SemanticWorld, ServiceTier,
    TestPlan, TestPlanEvidenceStatus, TestPolicy, ToolPolicy, VerificationArtifact,
    VerificationPlanResult, VerificationStatus, VerificationTestProjection,
};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_path(name: &str) -> PathBuf {
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir()
        .join(format!("needle-lifecycle-{name}-{}-{suffix}.sqlite3", std::process::id()))
}

fn active_profile(store: &RuntimeStore, id: &str, role: CodexRole) -> RoleProfileProvenance {
    let test_role = matches!(role, CodexRole::TestRunner | CodexRole::Verifier);
    let implementer = role == CodexRole::Implementer;
    let definition = RoleProfileDefinition::new(RoleProfileDefinitionInput {
        profile_id: needle_core::RoleProfileId::new(id).unwrap(),
        role,
        host: CodexHost::Codex,
        model: "offline-lifecycle-worker".to_owned(),
        reasoning: ReasoningLevel::Medium,
        service_tier: ServiceTier::Default,
        timeout_seconds: 30,
        budget: RoleProfileBudget {
            max_turns: 2,
            max_output_tokens: 1_200,
            max_cost_microusd: 10_000,
        },
        prompt_profile_digest: Digest::blake3(format!("{id}:prompt")),
        output_contract_digest: Digest::blake3(format!("{id}:output")),
        tool_policy: if implementer { ToolPolicy::IsolatedWrite } else { ToolPolicy::ReadOnly },
        command_policy: if test_role {
            CommandPolicy::CertifiedTests
        } else {
            CommandPolicy::Denied
        },
        filesystem_policy: if implementer {
            FilesystemPolicy::DisposableCheckout
        } else {
            FilesystemPolicy::ReadOnlyCheckout
        },
        network_policy: NetworkPolicy::Denied,
        test_policy: if test_role { TestPolicy::Certified } else { TestPolicy::Disabled },
        repair_policy: if implementer { RepairPolicy::Once } else { RepairPolicy::None },
        fallback_policy: FallbackPolicy::Disabled,
        concurrency: 1,
        route_assignments: Vec::new(),
    })
    .unwrap();
    let revision = store.create_role_profile(definition).unwrap();
    let state = store.role_profile_state(&revision.profile_id).unwrap();
    let revision = store
        .activate_role_profile(&revision.profile_id, revision.revision, state.state_digest)
        .unwrap();
    assert_eq!(revision.state, RoleProfileState::Active);
    RoleProfileProvenance::from_revision(&revision).unwrap()
}

fn worker(profile: &RoleProfileProvenance) -> LifecycleWorkerCompletion {
    LifecycleWorkerCompletion {
        profile: profile.clone(),
        worker_depth: 1,
        logical_worker_spawns: 1,
        usage: LifecycleUsage { worker_turns: 1, output_tokens: 10, cost_microusd: 10 },
    }
}

fn seed_test_plan_certificate(
    store: &RuntimeStore,
    name: &str,
    source: Digest,
    plan: &TestPlan,
) -> Digest {
    let request_id = Digest::blake3(format!("{name}:test-plan-request"));
    let dependency_digest = Digest::blake3(format!("{name}:dependency-manifest"));
    let world = SemanticWorld {
        repository_lineage: Digest::blake3(format!("{name}:repository")),
        source_selector: source.to_string(),
        platform: "test".to_owned(),
        features: "offline".to_owned(),
        configuration: None,
        toolchain: None,
    };
    let worker_artifact = SemanticWorkerArtifact::TestPlan {
        runner: plan.runner.clone(),
        argv: plan.argv.clone(),
        cwd_relative: plan.cwd_relative.clone(),
        identifiers: vec![plan.test_identifier.clone()],
        selection: "focused".to_owned(),
        evidence_paths: Vec::new(),
    };
    let contract = ArtifactContract::semantic(
        format!("{name}.test-plan"),
        1,
        worker_artifact.kind(),
        CacheScope::SnapshotExact,
    );
    let artifact_id = worker_artifact.canonical_artifact_id(contract.definition_digest).unwrap();
    let artifact = Artifact {
        id: artifact_id.digest(),
        request_id,
        contract,
        payload: serde_json::to_value(&worker_artifact).unwrap(),
        dependency_manifest: DependencyManifest {
            scope: CacheScope::SnapshotExact,
            observed_files_complete: true,
            dependencies: Vec::new(),
            gaps: Vec::new(),
        },
        validations: Vec::new(),
        created_unix_ms: 1,
    };
    let coverage = CoverageManifest {
        entries: Vec::new(),
        world: world.clone(),
        dependency_manifest_digest: dependency_digest,
    };
    let validator_definition = Digest::blake3(format!("{name}:validator"));
    let certificate_id = crate::semantic_validation::validation_certificate_id(
        artifact_id,
        &[],
        &[],
        &coverage,
        validator_definition,
        Some(TestPlanEvidenceStatus::Located),
    );
    let certificate_digest = certificate_id.digest();
    let certificate = ArtifactValidationCertificate {
        id: certificate_id,
        artifact: artifact_id,
        input_artifacts: Vec::new(),
        evidence_ids: Vec::new(),
        test_plan_evidence: Some(TestPlanEvidenceStatus::Located),
        coverage,
        validator_definition,
        dependency_checks_digest: dependency_digest,
        issued_unix_ms: 1,
    };
    let connection = store.connection().unwrap();
    connection
        .execute(
            "INSERT INTO artifact_requests(
                request_id, logical_id, source_digest, contract_id, route_key,
                request_json, created_unix_ms, format_revision
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 1, 2)",
            params![
                request_id.to_string(),
                format!("{name}:test-plan"),
                source.to_string(),
                artifact.contract.id.as_str(),
                "test-plan",
                "{}",
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO artifacts(
                artifact_id, request_id, contract_id, artifact_json,
                created_unix_ms, format_revision
             ) VALUES(?1, ?2, ?3, ?4, 1, 2)",
            params![
                artifact_id.to_string(),
                request_id.to_string(),
                artifact.contract.id.as_str(),
                serde_json::to_string(&artifact).unwrap(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO semantic_worlds(world_digest, world_json, created_unix_ms)
             VALUES(?1, ?2, 1)",
            params![world.id().to_string(), serde_json::to_string(&world).unwrap()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO artifact_validation_certificates(
                certificate_id, artifact_id, validator_definition_digest,
                dependency_manifest_digest, world_digest, certificate_json, issued_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 1)",
            params![
                certificate_digest.to_string(),
                artifact_id.to_string(),
                certificate.validator_definition.to_string(),
                dependency_digest.to_string(),
                world.id().to_string(),
                serde_json::to_string(&certificate).unwrap(),
            ],
        )
        .unwrap();
    certificate_digest
}

fn seed_exploration_certificate(store: &RuntimeStore, name: &str, source: Digest) -> Digest {
    let request_id = Digest::blake3(format!("{name}:exploration-request"));
    let dependency_digest = Digest::blake3(format!("{name}:exploration-dependencies"));
    let world = SemanticWorld {
        repository_lineage: Digest::blake3(format!("{name}:repository")),
        source_selector: source.to_string(),
        platform: "test".to_owned(),
        features: "exploration".to_owned(),
        configuration: None,
        toolchain: None,
    };
    let worker_artifact = SemanticWorkerArtifact::CodeLocation {
        locations: vec![SemanticLocation {
            role: LocationRole::Primary,
            path: "fixture.txt".to_owned(),
            symbol: None,
            byte_start: Some(0),
            byte_end: Some(1),
        }],
        gaps: Vec::new(),
    };
    let contract = ArtifactContract::semantic(
        format!("{name}.exploration"),
        1,
        worker_artifact.kind(),
        CacheScope::SnapshotExact,
    );
    let artifact_id = worker_artifact.canonical_artifact_id(contract.definition_digest).unwrap();
    let artifact_digest = artifact_id.digest();
    let artifact = Artifact {
        id: artifact_digest,
        request_id,
        contract,
        payload: serde_json::to_value(&worker_artifact).unwrap(),
        dependency_manifest: DependencyManifest {
            scope: CacheScope::SnapshotExact,
            observed_files_complete: true,
            dependencies: Vec::new(),
            gaps: Vec::new(),
        },
        validations: Vec::new(),
        created_unix_ms: 1,
    };
    let coverage = CoverageManifest {
        entries: Vec::new(),
        world: world.clone(),
        dependency_manifest_digest: dependency_digest,
    };
    let validator_definition = Digest::blake3(format!("{name}:exploration-validator"));
    let certificate_id = crate::semantic_validation::validation_certificate_id(
        artifact_id,
        &[],
        &[],
        &coverage,
        validator_definition,
        None,
    );
    let certificate_digest = certificate_id.digest();
    let certificate = ArtifactValidationCertificate {
        id: certificate_id,
        artifact: artifact_id,
        input_artifacts: Vec::new(),
        evidence_ids: Vec::new(),
        test_plan_evidence: None,
        coverage,
        validator_definition,
        dependency_checks_digest: dependency_digest,
        issued_unix_ms: 1,
    };
    let connection = store.connection().unwrap();
    connection
        .execute(
            "INSERT INTO artifact_requests(
                request_id, logical_id, source_digest, contract_id, route_key,
                request_json, created_unix_ms, format_revision
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 1, 2)",
            params![
                request_id.to_string(),
                format!("{name}:exploration"),
                source.to_string(),
                artifact.contract.id.as_str(),
                "code-location",
                "{}",
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO artifacts(
                artifact_id, request_id, contract_id, artifact_json,
                created_unix_ms, format_revision
             ) VALUES(?1, ?2, ?3, ?4, 1, 2)",
            params![
                artifact_id.to_string(),
                request_id.to_string(),
                artifact.contract.id.as_str(),
                serde_json::to_string(&artifact).unwrap(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO semantic_worlds(world_digest, world_json, created_unix_ms)
             VALUES(?1, ?2, 1)",
            params![world.id().to_string(), serde_json::to_string(&world).unwrap()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO artifact_validation_certificates(
                certificate_id, artifact_id, validator_definition_digest,
                dependency_manifest_digest, world_digest, certificate_json, issued_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 1)",
            params![
                certificate_digest.to_string(),
                artifact_id.to_string(),
                certificate.validator_definition.to_string(),
                dependency_digest.to_string(),
                world.id().to_string(),
                serde_json::to_string(&certificate).unwrap(),
            ],
        )
        .unwrap();
    artifact_digest
}

fn lifecycle_fixture(name: &str) -> (PathBuf, RuntimeStore, ChangeId, LifecycleProjection) {
    let path = temporary_path(name);
    let store = RuntimeStore::new(&path);
    store.initialize().unwrap();
    let profiles = LifecycleWorkerProfiles {
        explore: active_profile(&store, &format!("{name}.explorer"), CodexRole::Explorer),
        implement: active_profile(&store, &format!("{name}.implementer"), CodexRole::Implementer),
        test: active_profile(&store, &format!("{name}.test"), CodexRole::TestRunner),
        review: active_profile(&store, &format!("{name}.review"), CodexRole::Reviewer),
        verify: active_profile(&store, &format!("{name}.verify"), CodexRole::Verifier),
    };
    let source = Digest::blake3(format!("{name}:source"));
    let plan = TestPlan {
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
    };
    let certificate_digest = seed_test_plan_certificate(&store, name, source, &plan);
    let request = ChangeRequest {
        task: "Implement the bounded lifecycle fixture.".to_owned(),
        acceptance_criteria: vec!["The lifecycle remains fail closed.".to_owned()],
        allowed_paths: vec![AllowedPath {
            path: "fixture.txt".to_owned(),
            scope: AllowedPathScope::Exact,
        }],
        artifact_ids: Vec::new(),
        claim_ids: Vec::new(),
        constraints: Vec::new(),
    };
    let change_id = ChangeId::from_digest(Digest::blake3(format!("{name}:change")));
    store
        .record_change_request_with_provenance(
            &change_id,
            Digest::blake3(format!("{name}:repository")),
            source,
            request.digest(source),
            &request,
            Some(&profiles.implement),
        )
        .unwrap();
    let projection = store
        .create_lifecycle(
            &change_id,
            LifecycleSpec {
                worker_depth_limit: 1,
                profiles,
                budget: LifecycleBudget {
                    max_worker_turns: 10,
                    max_output_tokens: 10_000,
                    max_cost_microusd: 100_000,
                    max_concurrent_workers: 1,
                },
                test_plans: vec![LifecycleTestPlanBinding { plan, certificate_digest }],
            },
        )
        .unwrap();
    (path, store, change_id, projection)
}

#[test]
fn full_parent_owned_journal_reaches_apply_only_after_current_user_approval() {
    let name = "full-journal";
    let (path, store, change_id, mut projection) = lifecycle_fixture(name);
    let source = projection.lifecycle.source_snapshot;
    let exploration_id = seed_exploration_certificate(&store, name, source);
    projection = store
        .parent_transition_lifecycle(
            &change_id,
            projection.state_digest,
            LifecycleTransition::CompleteExplore {
                worker: worker(&projection.lifecycle.spec.profiles.explore),
                artifacts: vec![LifecycleArtifactRef {
                    kind: LifecycleArtifactKind::Exploration,
                    id: exploration_id,
                    source_snapshot: source,
                }],
            },
        )
        .unwrap();

    let request = ChangeRequest {
        task: "Implement the bounded lifecycle fixture.".to_owned(),
        acceptance_criteria: vec!["The lifecycle remains fail closed.".to_owned()],
        allowed_paths: vec![AllowedPath {
            path: "fixture.txt".to_owned(),
            scope: AllowedPathScope::Exact,
        }],
        artifact_ids: Vec::new(),
        claim_ids: Vec::new(),
        constraints: Vec::new(),
    };
    let before = b"before\n".to_vec();
    let after = b"after\n".to_vec();
    let files = vec![PatchFile {
        path: "fixture.txt".to_owned(),
        operation: PatchOperation::Update,
        before_digest: Some(Digest::blake3(&before)),
        after_digest: Some(Digest::blake3(&after)),
        before_bytes: before.len() as u64,
        after_bytes: after.len() as u64,
    }];
    let patch_id = PatchArtifact::compute_id(source, &files);
    let patch = PatchArtifact {
        id: patch_id,
        change_id: change_id.clone(),
        revision: 1,
        source_snapshot: source,
        files,
        summary: "Bounded lifecycle patch".to_owned(),
        acceptance_coverage: vec![AcceptanceCoverage {
            criterion: request.acceptance_criteria[0].clone(),
            status: AcceptanceStatus::Addressed,
            evidence: "fixture patch".to_owned(),
        }],
        residual_risks: Vec::new(),
        declared_output_digest: Digest::blake3(b"declared-output"),
        discrepancies: Vec::new(),
    };
    let repository_id = Digest::blake3(format!("{name}:repository"));
    store
        .record_prepared_change_with_provenance(
            repository_id,
            request.digest(source),
            &request,
            &patch,
            &serde_json::json!({"summary": "Bounded lifecycle patch"}),
            &[PatchFileBlob {
                path: "fixture.txt".to_owned(),
                before: Some(before),
                after: Some(after),
            }],
            Some(&projection.lifecycle.spec.profiles.implement),
        )
        .unwrap();
    projection = store
        .parent_transition_lifecycle(
            &change_id,
            projection.state_digest,
            LifecycleTransition::CompleteImplement {
                worker: worker(&projection.lifecycle.spec.profiles.implement),
                patch: LifecyclePatchRef { patch_id, revision: 1 },
            },
        )
        .unwrap();

    let plan = projection.lifecycle.spec.test_plans[0].clone();
    let evidence = CommandExecutionEvidence {
        id: "lifecycle-focused-evidence".to_owned(),
        approval_id: "parent-selected-test".to_owned(),
        argv: plan.plan.argv.clone(),
        cwd: plan.plan.cwd_relative.clone(),
        source_snapshot_digest: source,
        runner: plan.plan.runner.clone(),
        runner_version: None,
        exit_status: Some(0),
        duration_ms: 1,
        output_digest: Digest::blake3(b"focused-test-output"),
        output_preview: "test focused ... ok\ntest result: ok. 1 passed".to_owned(),
        test_identifier: Some(plan.plan.test_identifier.clone()),
        tests_executed: Some(1),
        infrastructure_failure: None,
    };
    store.record_command_evidence(None, &evidence).unwrap();
    projection = store
        .parent_transition_lifecycle(
            &change_id,
            projection.state_digest,
            LifecycleTransition::CompleteTest {
                worker: worker(&projection.lifecycle.spec.profiles.test),
                results: vec![LifecycleTestResult {
                    plan_digest: plan.plan_digest(),
                    certificate_digest: plan.certificate_digest,
                    available: true,
                    executed: true,
                    passed: true,
                    evidence_id: Some(evidence.id.clone()),
                    failure_code: None,
                }],
            },
        )
        .unwrap();

    let criterion = &request.acceptance_criteria[0];
    let review = ReviewArtifact::new(
        change_id.clone(),
        patch_id,
        LifecycleReviewVerdict::Approved,
        vec![LifecycleAcceptanceReview::new(
            criterion.as_bytes(),
            AcceptanceStatus::Addressed,
            b"review evidence",
        )],
        Vec::new(),
        projection.lifecycle.spec.profiles.review.definition_digest,
        projection.lifecycle.updated_unix_ms,
    )
    .unwrap();
    projection = store
        .parent_transition_lifecycle(
            &change_id,
            projection.state_digest,
            LifecycleTransition::CompleteReview {
                worker: worker(&projection.lifecycle.spec.profiles.review),
                review,
            },
        )
        .unwrap();

    let raw_coverage = vec![AcceptanceCoverage {
        criterion: criterion.clone(),
        status: AcceptanceStatus::Addressed,
        evidence: "verified in a fresh checkout".to_owned(),
    }];
    let plan_result = VerificationPlanResult {
        plan_digest: plan.plan_digest(),
        runner: plan.plan.runner.clone(),
        argv: plan.plan.argv.clone(),
        cwd_relative: plan.plan.cwd_relative.clone(),
        test_identifier: plan.plan.test_identifier.clone(),
        expected: true,
        available: true,
        executed: true,
        passed: true,
        evidence_id: Some(evidence.id.clone()),
        failure_reason: None,
    };
    let evidence_ids = vec![evidence.id.clone()];
    let plan_results = vec![plan_result];
    let verifier_definition = projection.lifecycle.spec.profiles.verify.definition_digest;
    let verification_id = VerificationArtifact::compute_id_with_plan_results(
        &change_id,
        patch_id,
        VerificationStatus::Verified,
        &raw_coverage,
        &[],
        VerificationTestProjection {
            evidence_ids: &evidence_ids,
            plan_results: &plan_results,
            plans_over_cap: false,
        },
        verifier_definition,
    );
    let verification = VerificationArtifact {
        id: verification_id,
        change_id: change_id.clone(),
        patch_id,
        verdict: VerificationStatus::Verified,
        acceptance_coverage: raw_coverage,
        findings: Vec::new(),
        test_evidence_ids: evidence_ids,
        test_plan_results: plan_results,
        test_plans_over_cap: false,
        verifier_definition,
        created_unix_ms: projection.lifecycle.updated_unix_ms,
    };
    assert!(verification.is_canonical());
    store
        .record_verification_artifact_with_provenance(
            &verification,
            &serde_json::json!({"fresh_checkout": true}),
            &serde_json::json!({}),
            None,
            Some(&projection.lifecycle.spec.profiles.verify),
        )
        .unwrap();
    projection = store
        .parent_transition_lifecycle(
            &change_id,
            projection.state_digest,
            LifecycleTransition::CompleteVerify {
                worker: worker(&projection.lifecycle.spec.profiles.verify),
                verification: LifecycleVerificationRef {
                    verification_id,
                    patch_id,
                    verdict: VerificationStatus::Verified,
                },
            },
        )
        .unwrap();
    assert_eq!(projection.lifecycle.status, LifecycleStatus::AwaitingApproval);

    let mut apply_time =
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 + 1;
    let mut apply = ChangeApplyRecord {
        id: ChangeApplyId(Digest::blake3(b"full-journal-apply")),
        change_id: change_id.clone(),
        patch_id,
        repository_root: "C:/bounded/repository".to_owned(),
        pre_snapshot: source,
        post_snapshot: None,
        status: ChangeApplyStatus::Applying,
        created_unix_ms: apply_time,
        completed_unix_ms: None,
    };
    let change_digest = store.change_digest(&change_id).unwrap().unwrap();
    assert!(matches!(
        store.begin_change_apply(&apply, &serde_json::json!({}), change_digest),
        Err(StoreError::LifecycleConflict(_))
    ));
    projection = store
        .approve_lifecycle_apply(
            &change_id,
            projection.state_digest,
            ApprovalDecisionSource::WebUser,
        )
        .unwrap();
    assert_eq!(projection.lifecycle.status, LifecycleStatus::Approved);
    apply_time = projection.lifecycle.updated_unix_ms + 1;
    apply.created_unix_ms = apply_time;
    assert!(matches!(
        store.begin_change_apply_with_lifecycle(
            &apply,
            &serde_json::json!({}),
            change_digest,
            Some(Digest::blake3(b"stale-lifecycle")),
        ),
        Err(StoreError::LifecycleConflict(_))
    ));
    assert_eq!(
        store.lifecycle(&change_id).unwrap().unwrap().lifecycle.status,
        LifecycleStatus::Approved
    );
    let barrier = Arc::new(Barrier::new(2));
    let handles = (0..2)
        .map(|_| {
            let path = path.clone();
            let apply = apply.clone();
            let barrier = Arc::clone(&barrier);
            let lifecycle_digest = projection.state_digest;
            thread::spawn(move || {
                let store = RuntimeStore::new(path);
                store.initialize().unwrap();
                barrier.wait();
                store.begin_change_apply_with_lifecycle(
                    &apply,
                    &serde_json::json!({}),
                    change_digest,
                    Some(lifecycle_digest),
                )
            })
        })
        .collect::<Vec<_>>();
    let outcomes = handles.into_iter().map(|handle| handle.join().unwrap()).collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_err()).count(), 1);
    assert_eq!(
        store.lifecycle(&change_id).unwrap().unwrap().lifecycle.status,
        LifecycleStatus::Applying
    );
    store
        .finish_change_apply(
            apply.id,
            ChangeApplyStatus::Applied,
            Some(Digest::blake3(b"post-apply")),
            apply_time + 1,
        )
        .unwrap();
    let replayed = store.replay_lifecycle(&change_id).unwrap();
    assert_eq!(replayed.lifecycle.status, LifecycleStatus::Completed);
    assert_eq!(
        replayed.lifecycle.terminal_outcome,
        Some(needle_core::LifecycleTerminalOutcome::Applied)
    );
    assert!(matches!(
        store.record_change_failure(&change_id, "late preparation failure"),
        Err(StoreError::LifecycleConflict(_))
    ));
    assert_eq!(
        store.replay_lifecycle(&change_id).unwrap().lifecycle.status,
        LifecycleStatus::Completed
    );

    drop(store);
    fs::remove_file(path).unwrap();
}

#[test]
fn creation_rejects_stale_test_plan_source_and_inactive_profiles() {
    let name = "creation-anchors";
    let (path, store, _change_id, projection) = lifecycle_fixture(name);
    let request = ChangeRequest {
        task: "Implement the bounded lifecycle fixture.".to_owned(),
        acceptance_criteria: vec!["The lifecycle remains fail closed.".to_owned()],
        allowed_paths: vec![AllowedPath {
            path: "fixture.txt".to_owned(),
            scope: AllowedPathScope::Exact,
        }],
        artifact_ids: Vec::new(),
        claim_ids: Vec::new(),
        constraints: Vec::new(),
    };
    let stale_source = Digest::blake3(b"different-source");
    let stale_change = ChangeId::from_digest(Digest::blake3(b"stale-plan-change"));
    store
        .record_change_request_with_provenance(
            &stale_change,
            Digest::blake3(b"creation-anchors-repository"),
            stale_source,
            request.digest(stale_source),
            &request,
            Some(&projection.lifecycle.spec.profiles.implement),
        )
        .unwrap();
    assert!(matches!(
        store.create_lifecycle(&stale_change, projection.lifecycle.spec.clone()),
        Err(StoreError::LifecycleConflict(_))
    ));
    assert!(store.lifecycle(&stale_change).unwrap().is_none());

    let explore_profile = &projection.lifecycle.spec.profiles.explore;
    let profile_state = store.role_profile_state(&explore_profile.profile_id).unwrap();
    store.deactivate_role_profile(&explore_profile.profile_id, profile_state.state_digest).unwrap();
    let inactive_change = ChangeId::from_digest(Digest::blake3(b"inactive-profile-change"));
    let source = projection.lifecycle.source_snapshot;
    store
        .record_change_request_with_provenance(
            &inactive_change,
            Digest::blake3(b"creation-anchors-repository"),
            source,
            request.digest(source),
            &request,
            Some(&projection.lifecycle.spec.profiles.implement),
        )
        .unwrap();
    assert!(matches!(
        store.create_lifecycle(&inactive_change, projection.lifecycle.spec),
        Err(StoreError::LifecycleConflict(_))
    ));
    assert!(store.lifecycle(&inactive_change).unwrap().is_none());

    drop(store);
    fs::remove_file(path).unwrap();
}

#[test]
fn exploration_rejects_test_plan_and_stale_source_artifacts_without_mutation() {
    let name = "exploration-typing";
    let (path, store, change_id, projection) = lifecycle_fixture(name);
    let completion = worker(&projection.lifecycle.spec.profiles.explore);
    let connection = store.connection().unwrap();
    let test_plan_artifact: String = connection
        .query_row(
            "SELECT a.artifact_id FROM artifacts a
             JOIN artifact_requests r ON r.request_id=a.request_id
             WHERE r.logical_id=?1",
            [format!("{name}:test-plan")],
            |row| row.get(0),
        )
        .unwrap();
    let test_plan_artifact = Digest::parse(&test_plan_artifact).unwrap();
    drop(connection);
    assert!(matches!(
        store.parent_transition_lifecycle(
            &change_id,
            projection.state_digest,
            LifecycleTransition::CompleteExplore {
                worker: completion.clone(),
                artifacts: vec![LifecycleArtifactRef {
                    kind: LifecycleArtifactKind::Exploration,
                    id: test_plan_artifact,
                    source_snapshot: projection.lifecycle.source_snapshot,
                }],
            },
        ),
        Err(StoreError::LifecycleConflict(_))
    ));
    let stale_artifact = seed_exploration_certificate(
        &store,
        "exploration-typing-stale",
        Digest::blake3(b"stale-source"),
    );
    assert!(matches!(
        store.parent_transition_lifecycle(
            &change_id,
            projection.state_digest,
            LifecycleTransition::CompleteExplore {
                worker: completion,
                artifacts: vec![LifecycleArtifactRef {
                    kind: LifecycleArtifactKind::Exploration,
                    id: stale_artifact,
                    source_snapshot: projection.lifecycle.source_snapshot,
                }],
            },
        ),
        Err(StoreError::LifecycleConflict(_))
    ));
    assert_eq!(store.lifecycle(&change_id).unwrap().unwrap(), projection);
    assert_eq!(store.lifecycle_events(&change_id).unwrap().len(), 1);

    drop(store);
    fs::remove_file(path).unwrap();
}

#[test]
fn projection_and_event_payload_digests_are_verified_on_read() {
    let (path, store, change_id, projection) = lifecycle_fixture("stored-digests");
    let connection = store.connection().unwrap();
    connection.execute("DROP TRIGGER change_lifecycles_transition_shape", []).unwrap();
    connection
        .execute(
            "UPDATE change_lifecycles SET state_digest=?2 WHERE change_id=?1",
            params![change_id.to_string(), Digest::blake3(b"tampered-state").to_string()],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(store.lifecycle(&change_id), Err(StoreError::LifecycleCorruption(_))));

    let connection = store.connection().unwrap();
    connection
        .execute(
            "UPDATE change_lifecycles SET state_digest=?2 WHERE change_id=?1",
            params![change_id.to_string(), projection.state_digest.to_string()],
        )
        .unwrap();
    connection.execute("DROP TRIGGER change_events_no_update", []).unwrap();
    connection
        .execute(
            "UPDATE change_events SET payload_digest=?2
             WHERE change_id=?1 AND lifecycle_sequence=0",
            params![change_id.to_string(), Digest::blake3(b"tampered-event").to_string()],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(store.lifecycle_events(&change_id), Err(StoreError::LifecycleCorruption(_))));

    drop(store);
    fs::remove_file(path).unwrap();
}

#[test]
fn concurrent_cancellation_has_one_winner_and_replays_after_restart() {
    let (path, store, change_id, projection) = lifecycle_fixture("cancel-race");
    let barrier = Arc::new(Barrier::new(2));
    let expected_state_digest = projection.state_digest;
    let transition = LifecycleTransition::Cancel {
        reason: LifecycleReason::new("user_cancelled", b"bounded user cancellation").unwrap(),
    };
    let handles = (0..2)
        .map(|_| {
            let path = path.clone();
            let change_id = change_id.clone();
            let barrier = Arc::clone(&barrier);
            let transition = transition.clone();
            thread::spawn(move || {
                let store = RuntimeStore::new(path);
                store.initialize().unwrap();
                barrier.wait();
                store.parent_transition_lifecycle(&change_id, expected_state_digest, transition)
            })
        })
        .collect::<Vec<_>>();
    let outcomes = handles.into_iter().map(|handle| handle.join().unwrap()).collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_err()).count(), 1);
    drop(store);

    let restarted = RuntimeStore::new(&path);
    restarted.initialize().unwrap();
    let replayed = restarted.replay_lifecycle(&change_id).unwrap();
    assert_eq!(replayed.lifecycle.status, LifecycleStatus::Cancelled);
    assert_eq!(replayed.lifecycle.generation, 1);
    assert_eq!(restarted.lifecycle_events(&change_id).unwrap().len(), 2);

    let connection = rusqlite::Connection::open(&path).unwrap();
    assert!(
        connection
            .execute(
                "UPDATE change_events SET event_type='tampered' WHERE change_id=?1",
                [change_id.to_string()],
            )
            .is_err()
    );
    assert!(
        connection
            .execute("DELETE FROM change_events WHERE change_id=?1", [change_id.to_string()])
            .is_err()
    );
    assert!(
        connection
            .execute("DELETE FROM change_lifecycles WHERE change_id=?1", [change_id.to_string()],)
            .is_err()
    );
    drop(connection);
    drop(restarted);
    fs::remove_file(path).unwrap();
}

#[test]
fn unavailable_exploration_artifact_leaves_projection_unchanged() {
    let (path, store, change_id, projection) = lifecycle_fixture("missing-artifact");
    let worker = LifecycleWorkerCompletion {
        profile: projection.lifecycle.spec.profiles.explore.clone(),
        worker_depth: 1,
        logical_worker_spawns: 1,
        usage: LifecycleUsage { worker_turns: 1, output_tokens: 10, cost_microusd: 10 },
    };
    let result = store.parent_transition_lifecycle(
        &change_id,
        projection.state_digest,
        LifecycleTransition::CompleteExplore {
            worker,
            artifacts: vec![LifecycleArtifactRef {
                kind: LifecycleArtifactKind::Exploration,
                id: Digest::blake3(b"missing-artifact"),
                source_snapshot: projection.lifecycle.source_snapshot,
            }],
        },
    );
    assert!(matches!(result, Err(StoreError::LifecycleConflict(_))));
    assert_eq!(store.lifecycle(&change_id).unwrap().unwrap(), projection);
    assert_eq!(store.lifecycle_events(&change_id).unwrap().len(), 1);
    drop(store);
    fs::remove_file(path).unwrap();
}
