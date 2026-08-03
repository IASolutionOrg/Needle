use needle_core::{
    AllowedPath, AllowedPathScope, ChangeApplyId, ChangeApplyRecord, ChangeApplyStatus,
    ChangeRequest, Digest, EvidenceFailurePolicy, MultiNeedPolicy, VerificationStatus,
    WorkerConfig,
};
use needle_platform_codex::{CodexPatchWorker, CodexVerifier};
use needle_runtime::{
    ChangeApplyError, RuntimeSettings, RuntimeStore, apply_verified_change,
    materialize_patch_artifact, recover_pending_change_applies,
};
use std::fs;
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

fn temporary_root() -> PathBuf {
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("needle-patcher-offline-{}-{suffix}", std::process::id()))
}

fn git(repository: &Path, arguments: &[&str]) {
    assert!(
        Command::new("git").arg("-C").arg(repository).args(arguments).status().unwrap().success()
    );
}
