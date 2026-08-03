use needle_core::{
    AllowedPath, AllowedPathScope, ArtifactId, ArtifactRequest, ChangeApplyId, ChangeApplyRecord,
    ChangeApplyStatus, ChangeRequest, Digest, EvidenceFailurePolicy, MultiNeedPolicy, NeedIr,
    SemanticWorkerArtifact, TestPlan, VerificationStatus, WorkerConfig, built_in_route_contracts,
    compile_need, need_fragment,
};
use needle_platform_codex::{CodexPatchWorker, CodexVerifier};
use needle_runtime::{
    ChangeApplyError, RuntimeSettings, RuntimeStore, apply_verified_change,
    materialize_patch_artifact, recover_pending_change_applies, validate_semantic_test_plan,
};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const SIMULATOR: &str = env!("CARGO_BIN_EXE_needle-sim-codex");

#[test]
fn patcher_changes_only_disposable_checkout_and_persists_filesystem_patch() {
    let root = temporary_root();
    let repository = root.join("source");
    let data = root.join("data");
    let codex_home = root.join("codex-home");
    fs::create_dir_all(&repository).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    fs::write(codex_home.join(".needle-simulation-worker-scenario"), "patch_worker\n").unwrap();
    git(&repository, &["init", "--quiet"]);
    git(&repository, &["config", "user.email", "needle@example.invalid"]);
    git(&repository, &["config", "user.name", "Needle Test"]);
    fs::write(repository.join("fixture.txt"), "original active content\n").unwrap();
    git(&repository, &["add", "fixture.txt"]);
    git(&repository, &["commit", "--quiet", "-m", "fixture"]);

    let request = ChangeRequest {
        task: "Update the fixture text.".to_owned(),
        acceptance_criteria: vec!["The fixture changes.".to_owned()],
        allowed_paths: vec![AllowedPath {
            path: "fixture.txt".to_owned(),
            scope: AllowedPathScope::Exact,
        }],
        artifact_ids: Vec::new(),
        claim_ids: Vec::new(),
        constraints: vec!["Keep the file as UTF-8 text.".to_owned()],
    };
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "gpt-5.6-luna".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
    };
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store
        .initialize_defaults(&RuntimeSettings {
            codex_executable: SIMULATOR.to_owned(),
            worker_model: config.model.clone(),
            worker_reasoning: config.reasoning.clone(),
            worker_timeout_seconds: config.timeout_seconds,
            evidence_failure_policy: config.evidence_failure_policy,
            trusted_test_execution: false,
            multi_need_policy: MultiNeedPolicy::default(),
        })
        .unwrap();
    drop(store);
    let outcome = CodexPatchWorker::with_codex_home(&data, &codex_home)
        .prepare(&config, &repository, &request, &[])
        .unwrap();

    assert_eq!(
        fs::read_to_string(repository.join("fixture.txt")).unwrap(),
        "original active content\n"
    );
    assert_eq!(outcome.changed_files.len(), 1);
    assert_eq!(outcome.changed_files[0].path, "fixture.txt");
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    let unverified_digest = store.change_digest(&outcome.change_id).unwrap().unwrap();
    assert!(matches!(
        apply_verified_change(&store, &repository, &outcome.change_id, unverified_digest),
        Err(ChangeApplyError::NotVerified)
    ));
    drop(store);
    fs::write(codex_home.join(".needle-simulation-worker-scenario"), "verifier_worker\n").unwrap();
    let verification = CodexVerifier::with_codex_home(&data, &codex_home)
        .verify(&config, &repository, &outcome.change_id)
        .unwrap();
    assert_eq!(verification.artifact.verdict, VerificationStatus::Verified);
    assert!(verification.verifier_started);
    assert_eq!(
        fs::read_to_string(repository.join("fixture.txt")).unwrap(),
        "original active content\n"
    );
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    let prepared = store.prepared_change(&outcome.change_id).unwrap().unwrap();
    assert_eq!(prepared.state, "verified");
    assert_eq!(prepared.patch.id, outcome.patch_id);
    let blobs = store.patch_file_blobs(outcome.patch_id).unwrap();
    assert_eq!(blobs.len(), 1);
    assert_eq!(blobs[0].before.as_deref(), Some(b"original active content\n".as_slice()));
    assert_eq!(blobs[0].after.as_deref(), Some(b"changed by isolated patcher\n".as_slice()));
    assert!(data.join("change-runs").read_dir().unwrap().next().is_none());

    let change_digest = store.change_digest(&outcome.change_id).unwrap().unwrap();
    fs::write(repository.join("fixture.txt"), "unrelated active drift\n").unwrap();
    assert!(matches!(
        apply_verified_change(&store, &repository, &outcome.change_id, change_digest),
        Err(ChangeApplyError::SnapshotDrift)
    ));
    fs::write(repository.join("fixture.txt"), "original active content\n").unwrap();

    let recovery_id = ChangeApplyId(Digest::blake3(b"offline-recovery"));
    store
        .begin_change_apply(
            &ChangeApplyRecord {
                id: recovery_id,
                change_id: outcome.change_id.clone(),
                patch_id: outcome.patch_id,
                repository_root: fs::canonicalize(&repository)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                pre_snapshot: prepared.source_snapshot,
                post_snapshot: None,
                status: ChangeApplyStatus::Applying,
                created_unix_ms: 1,
                completed_unix_ms: None,
            },
            &serde_json::json!({"test": "crash recovery"}),
            change_digest,
        )
        .unwrap();
    materialize_patch_artifact(&repository, &prepared.patch, &blobs).unwrap();
    recover_pending_change_applies(&store, &repository).unwrap();
    assert_eq!(
        fs::read_to_string(repository.join("fixture.txt")).unwrap(),
        "original active content\n"
    );
    assert_eq!(
        store.change_apply(recovery_id).unwrap().unwrap().status,
        ChangeApplyStatus::RolledBack
    );

    let applied =
        apply_verified_change(&store, &repository, &outcome.change_id, change_digest).unwrap();
    assert_eq!(applied.status, ChangeApplyStatus::Applied);
    assert_eq!(
        fs::read_to_string(repository.join("fixture.txt")).unwrap(),
        "changed by isolated patcher\n"
    );
    assert!(matches!(
        apply_verified_change(&store, &repository, &outcome.change_id, change_digest),
        Err(ChangeApplyError::NotVerified)
    ));

    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn repairable_patch_gets_exactly_one_revision_and_independent_reverification() {
    let root = temporary_root();
    let repository = root.join("source");
    let data = root.join("data");
    let codex_home = root.join("codex-home");
    fs::create_dir_all(&repository).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    fs::write(codex_home.join(".needle-simulation-worker-scenario"), "repair_flow\n").unwrap();
    git(&repository, &["init", "--quiet"]);
    git(&repository, &["config", "user.email", "needle@example.invalid"]);
    git(&repository, &["config", "user.name", "Needle Test"]);
    fs::write(repository.join("fixture.txt"), "original active content\n").unwrap();
    git(&repository, &["add", "fixture.txt"]);
    git(&repository, &["commit", "--quiet", "-m", "fixture"]);
    let request = ChangeRequest {
        task: "Update the fixture text.".to_owned(),
        acceptance_criteria: vec!["The fixture changes.".to_owned()],
        allowed_paths: vec![AllowedPath {
            path: "fixture.txt".to_owned(),
            scope: AllowedPathScope::Exact,
        }],
        artifact_ids: Vec::new(),
        claim_ids: Vec::new(),
        constraints: Vec::new(),
    };
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "gpt-5.6-luna".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
    };
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store
        .initialize_defaults(&RuntimeSettings {
            codex_executable: SIMULATOR.to_owned(),
            worker_model: config.model.clone(),
            worker_reasoning: config.reasoning.clone(),
            worker_timeout_seconds: config.timeout_seconds,
            evidence_failure_policy: config.evidence_failure_policy,
            trusted_test_execution: false,
            multi_need_policy: MultiNeedPolicy::default(),
        })
        .unwrap();
    let patcher = CodexPatchWorker::with_codex_home(&data, &codex_home);
    let first = patcher.prepare(&config, &repository, &request, &[]).unwrap();
    let verifier = CodexVerifier::with_codex_home(&data, &codex_home);
    let first_verification = verifier.verify(&config, &repository, &first.change_id).unwrap();
    assert_eq!(first_verification.artifact.verdict, VerificationStatus::Repairable);

    let repaired = patcher.repair(&config, &repository, &first.change_id).unwrap();
    assert_eq!(repaired.change_id, first.change_id);
    assert_ne!(repaired.patch_id, first.patch_id);
    let final_verification = verifier.verify(&config, &repository, &first.change_id).unwrap();
    assert_eq!(final_verification.artifact.verdict, VerificationStatus::Verified);
    assert!(patcher.repair(&config, &repository, &first.change_id).is_err());
    assert_eq!(
        fs::read_to_string(repository.join("fixture.txt")).unwrap(),
        "original active content\n"
    );
    let prepared = store.prepared_change(&first.change_id).unwrap().unwrap();
    assert_eq!(prepared.patch.revision, 2);
    assert_eq!(prepared.patch.id, repaired.patch_id);
    assert_eq!(prepared.state, "verified");
    assert!(prepared.repair_attempted);
    assert!(data.join("change-runs").read_dir().unwrap().next().is_none());
    assert!(data.join("verification-runs").read_dir().unwrap().next().is_none());

    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn verifier_runs_bounded_certified_plan_matrix_offline() {
    for (name, plan_count, duplicate, stale) in [
        ("one", 1, false, false),
        ("two", 2, false, false),
        ("four", 4, false, false),
        ("duplicate", 1, true, false),
        ("over-cap", 5, false, false),
        ("stale", 1, false, true),
    ] {
        run_certified_plan_scenario(name, plan_count, duplicate, stale);
    }
}

fn run_certified_plan_scenario(name: &str, plan_count: usize, duplicate: bool, stale: bool) {
    let root = temporary_root();
    let repository = root.join("source");
    let data = root.join("data");
    let codex_home = root.join("codex-home");
    fs::create_dir_all(repository.join("tests")).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    fs::write(codex_home.join(".needle-simulation-worker-scenario"), "patch_worker\n").unwrap();
    git(&repository, &["init", "--quiet"]);
    git(&repository, &["config", "user.email", "needle@example.invalid"]);
    git(&repository, &["config", "user.name", "Needle Test"]);
    fs::write(repository.join("fixture.txt"), "original active content\n").unwrap();
    let identifiers =
        (0..plan_count).map(|index| format!("answer_case_{index}")).collect::<Vec<_>>();
    let evidence = identifiers
        .iter()
        .map(|identifier| format!("#[test] fn {identifier}() {{}}\n"))
        .collect::<String>();
    fs::write(repository.join("tests/fixture.rs"), format!("fn answer() {{}}\n{evidence}"))
        .unwrap();
    fs::write(repository.join("tests/duplicate.rs"), format!("fn answer() {{}}\n{evidence}"))
        .unwrap();
    git(&repository, &["add", "fixture.txt", "tests/fixture.rs", "tests/duplicate.rs"]);
    git(&repository, &["commit", "--quiet", "-m", "fixture"]);

    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "gpt-5.6-luna".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
    };
    store
        .initialize_defaults(&RuntimeSettings {
            codex_executable: SIMULATOR.to_owned(),
            worker_model: config.model.clone(),
            worker_reasoning: config.reasoning.clone(),
            worker_timeout_seconds: config.timeout_seconds,
            evidence_failure_policy: config.evidence_failure_policy,
            trusted_test_execution: true,
            multi_need_policy: MultiNeedPolicy::default(),
        })
        .unwrap();

    let ir = NeedIr::parse(
        "@@need\n\
         @route tests.relevant\n\
         @subject symbol:\"answer\"\n\
         @require focused-tests selection=representative completeness=open-world polarity=positive\n\
         @world source=current features=default\n\
         \n\
         Find the relevant focused test.\n\
         @@end",
    )
    .unwrap()
    .unwrap();
    let route = built_in_route_contracts()
        .into_iter()
        .find(|route| route.route.as_str() == "tests.relevant")
        .unwrap();
    let need = compile_need(&ir, Digest::blake3(format!("certified-plan-{name}")), &route).unwrap();
    let fragment = need_fragment(&need, need.required.clone(), Vec::new());

    let mut artifact_ids = Vec::with_capacity(plan_count);
    let mut simulator_plans = Vec::with_capacity(plan_count);
    for (index, identifier) in identifiers.iter().enumerate() {
        let argv = vec![
            "cargo".to_owned(),
            "test".to_owned(),
            identifier.clone(),
            "--".to_owned(),
            "--exact".to_owned(),
        ];
        let evidence_path = "tests/fixture.rs".to_owned();
        let worker_artifact = SemanticWorkerArtifact::TestPlan {
            runner: "cargo".to_owned(),
            argv: argv.clone(),
            cwd_relative: ".".to_owned(),
            identifiers: vec![identifier.clone()],
            selection: "representative".to_owned(),
            evidence_paths: vec![evidence_path.clone()],
        };
        let declared_plan = TestPlan {
            runner: "cargo".to_owned(),
            argv: argv.clone(),
            cwd_relative: ".".to_owned(),
            test_identifier: identifier.clone(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        let request = ArtifactRequest {
            contract_id: "needle.semantic.test-plan".to_owned(),
            contract_revision: 2,
            repository_id: need.world.repository_lineage,
            source_snapshot_digest: Digest::blake3(format!("certified-plan-source-{name}")),
            route_key: route.route.clone(),
            normalized_request: format!("focused certified plan {name} {index}"),
            semantic_fragment_id: Some(fragment.id),
            input_artifact_ids: Vec::new(),
        };
        let validated = validate_semantic_test_plan(
            &fragment,
            &worker_artifact,
            &repository,
            request.semantic_id().digest(),
            None,
            &declared_plan,
        )
        .unwrap();
        store
            .publish_semantic_artifact(&request, &need, &validated.artifact, &validated.certificate)
            .unwrap();
        artifact_ids.push(ArtifactId(validated.artifact.id));
        simulator_plans.push(serde_json::json!({
            "runner": "cargo",
            "argv": argv,
            "cwd_relative": ".",
            "test_identifier": identifier,
            "available": !stale,
        }));
    }
    if duplicate {
        artifact_ids.push(*artifact_ids.first().expect("duplicate scenario has one plan"));
    }
    fs::write(
        codex_home.join(".needle-simulation-verifier-plans"),
        serde_json::to_vec(&simulator_plans).unwrap(),
    )
    .unwrap();
    if stale {
        fs::OpenOptions::new()
            .append(true)
            .open(repository.join("tests/fixture.rs"))
            .unwrap()
            .write_all(b"// dependency mutation after certification\n")
            .unwrap();
    }
    drop(store);

    let request = ChangeRequest {
        task: "Update the fixture text.".to_owned(),
        acceptance_criteria: vec!["The fixture changes.".to_owned()],
        allowed_paths: vec![AllowedPath {
            path: "fixture.txt".to_owned(),
            scope: AllowedPathScope::Exact,
        }],
        artifact_ids,
        claim_ids: Vec::new(),
        constraints: Vec::new(),
    };
    let outcome = CodexPatchWorker::with_codex_home(&data, &codex_home)
        .prepare(&config, &repository, &request, &[])
        .unwrap();
    assert_eq!(
        fs::read_to_string(repository.join("fixture.txt")).unwrap(),
        "original active content\n"
    );
    assert!(data.join("change-runs").read_dir().unwrap().next().is_none());

    fs::write(codex_home.join(".needle-simulation-worker-scenario"), "verifier_worker\n").unwrap();
    let verification = CodexVerifier::with_codex_home(&data, &codex_home)
        .verify(&config, &repository, &outcome.change_id)
        .unwrap();
    let artifact = &verification.artifact;
    if name == "over-cap" {
        assert_eq!(artifact.verdict, VerificationStatus::Inconclusive);
        assert!(artifact.test_plans_over_cap);
        assert!(artifact.test_plan_results.is_empty());
        assert!(artifact.test_evidence_ids.is_empty());
        assert!(artifact.findings.iter().any(|finding| finding.contains("test-plan bound")));
    } else if name == "stale" {
        assert_ne!(artifact.verdict, VerificationStatus::Verified);
        assert_eq!(artifact.test_plan_results.len(), 1);
        let result = &artifact.test_plan_results[0];
        assert!(result.expected);
        assert!(!result.available);
        assert!(!result.executed);
        assert!(!result.passed);
        assert!(result.evidence_id.is_none());
        assert!(artifact.test_evidence_ids.is_empty());
        assert!(
            artifact.findings.iter().any(|finding| {
                finding.contains("stale") || finding.contains("fresh certificate")
            })
        );
    } else {
        let expected = if name == "duplicate" { 1 } else { plan_count };
        assert_eq!(artifact.verdict, VerificationStatus::Verified);
        assert!(!artifact.test_plans_over_cap);
        assert_eq!(artifact.test_plan_results.len(), expected);
        assert_eq!(artifact.test_evidence_ids.len(), expected);
        assert!(artifact.test_plan_results.iter().all(|result| {
            result.expected
                && result.available
                && result.executed
                && result.passed
                && result.evidence_id.is_some()
                && result.failure_reason.is_none()
        }));
        assert!(
            artifact
                .test_plan_results
                .windows(2)
                .all(|pair| { pair[0].plan_digest < pair[1].plan_digest })
        );
        let evidence_ids = artifact.test_evidence_ids.iter().collect::<BTreeSet<_>>();
        assert_eq!(evidence_ids.len(), expected);
    }
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    assert!(store.pending_approvals().unwrap().is_empty());
    assert!(store.pending_worker_sessions().unwrap().is_empty());
    assert!(data.join("verification-runs").read_dir().unwrap().next().is_none());
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

fn temporary_root() -> PathBuf {
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("needle-patcher-offline-{}-{suffix}", std::process::id()))
}

fn git(repository: &Path, arguments: &[&str]) {
    assert!(
        Command::new("git").arg("-C").arg(repository).args(arguments).status().unwrap().success()
    );
}
