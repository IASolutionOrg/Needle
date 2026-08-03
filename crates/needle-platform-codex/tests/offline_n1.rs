use needle_core::{
    CodexHost, CodexRole, CommandPolicy, Digest, EvidenceFailurePolicy, FallbackPolicy,
    FilesystemPolicy, NeedRequest, NetworkPolicy, RepairPolicy, RoleProfileBudget,
    RoleProfileDefinition, RoleProfileDefinitionInput, RoleProfileId, ServiceTier, TestPlan,
    TestPolicy, ToolPolicy, WorkerConfig,
};
use needle_platform_codex::{CodexWorker, HookConfig, StopInput, handle_stop_with_resolver};
use needle_runtime::{ResolveRequest, RuntimeEngine, RuntimeSettings, RuntimeStore};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const SIMULATOR: &str = env!("CARGO_BIN_EXE_needle-sim-codex");
const EXPECTED_FAILURES: [(&str, &str); 4] = [
    ("wrong_test_identifier", "test_evidence_invalid"),
    ("no_test_executed", "test_evidence_invalid"),
    ("test_exit_failure", "test_evidence_invalid"),
    ("incomplete_after_repair", "artifact_protocol_incomplete"),
];

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[test]
fn repair_path_is_stable_across_three_offline_n1_runs() {
    for repetition in 0..3 {
        let simulation = Simulation::run("repair_success", repetition);
        assert!(
            simulation.frontier.contains("[NEEDLE_CONTEXT]"),
            "repetition {repetition}: {}",
            simulation.frontier
        );
        assert!(simulation.frontier.contains("src/lib.rs"), "{}", simulation.frontier);
        assert!(simulation.frontier.contains("apply_flag"), "{}", simulation.frontier);
        assert_eq!(simulation.worker_status, "completed");
        assert_eq!(simulation.logical_worker_spawns, 1);
        assert_eq!(simulation.worker_turns, 2);
        assert!(simulation.repair_performed);
        assert_eq!(simulation.command_evidence_count, 2);
        assert!(simulation.no_pending_approvals);
        assert!(simulation.exact_cache_hit);
        assert_eq!(simulation.worker_run_count, 2);
        assert!(simulation.checkout_clean);
        assert!(simulation.sandboxes_cleaned);
    }
}

#[test]
fn offline_n1_reports_semantic_test_and_artifact_failures_without_models() {
    for (scenario, expected_code) in EXPECTED_FAILURES {
        let simulation = Simulation::run(scenario, 0);
        assert!(
            simulation.frontier.contains("Continue using native repository discovery"),
            "{scenario}: {}",
            simulation.frontier
        );
        assert_eq!(simulation.worker_status, "failed", "{scenario}");
        assert_eq!(simulation.failure_code.as_deref(), Some(expected_code), "{scenario}");
        assert_eq!(simulation.logical_worker_spawns, 1, "{scenario}");
        assert_eq!(simulation.session_cleanup_success, Some(true), "{scenario}");
        assert!(simulation.no_pending_worker_sessions, "{scenario}");
        assert!(simulation.checkout_clean, "{scenario}");
        assert!(simulation.sandboxes_cleaned, "{scenario}");
    }
}

#[test]
fn completed_command_payload_mismatch_is_rejected_end_to_end() {
    let simulation = Simulation::run("payload_mismatch", 0);
    assert_eq!(simulation.worker_status, "failed");
    assert_eq!(simulation.failure_code.as_deref(), Some("test_evidence_invalid"));
    assert_eq!(simulation.command_evidence_count, 1);
    assert!(simulation.frontier.contains("Continue using native repository discovery"));
}

#[test]
fn declared_test_plan_does_not_require_the_worker_to_execute_it() {
    let simulation = Simulation::run("test_not_invoked", 0);
    assert!(simulation.frontier.contains("[NEEDLE_CONTEXT]"), "{}", simulation.frontier);
    assert_eq!(simulation.worker_status, "completed");
    assert_eq!(simulation.logical_worker_spawns, 1);
    assert_eq!(simulation.worker_turns, 1);
    assert!(!simulation.repair_performed);
    assert_eq!(simulation.command_evidence_count, 0);
    assert!(simulation.no_pending_approvals);
    assert_eq!(simulation.session_cleanup_success, Some(true));
    assert!(simulation.no_pending_worker_sessions);
    assert!(simulation.checkout_clean);
    assert!(simulation.sandboxes_cleaned);
}

#[test]
fn transport_preflight_reports_optional_runner_unavailable_without_a_model_turn() {
    let root = temporary_root("preflight", 0);
    let repository = root.join("repository");
    let data = root.join("data");
    create_repository(&repository, "test_not_invoked");
    fs::create_dir_all(&data).unwrap();
    let mut plan = test_plan();
    plan.runner = "unsupported-runner".to_owned();
    plan.argv = vec!["unsupported-runner".to_owned(), "focused".to_owned()];
    let report = CodexWorker::new(&data)
        .preflight_transport_for_test_plan(
            &WorkerConfig {
                executable: SIMULATOR.to_owned(),
                model: "simulated-worker".to_owned(),
                reasoning: "medium".to_owned(),
                service_tier: None,
                timeout_seconds: 10,
                evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
                role_profile_provenance: None,
            },
            &repository,
            "trace.state-flow",
            Some(plan),
            true,
        )
        .unwrap();
    assert!(report.test_plan_declared);
    assert!(!report.test_execution_available);
    assert!(
        report
            .test_execution_unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("not supported"))
    );
    assert!(report.app_server_initialized);
    assert!(report.ephemeral_thread_cleanup_completed);
    assert!(report.sandbox_cleaned);
    assert_eq!(report.provider_turns_started, 0);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_app_server_turn_preserves_error_usage_and_cleanup() {
    let simulation = Simulation::run("worker_turn_failed", 0);
    assert_eq!(simulation.worker_status, "failed");
    assert_eq!(simulation.failure_code.as_deref(), Some("worker_turn_failed"));
    let diagnostic = simulation.failure_diagnostic.as_deref().expect("failure diagnostic");
    assert!(diagnostic.contains("simulated provider rejection"));
    assert!(diagnostic.contains("additional_details=the request was rejected before model output"));
    assert!(diagnostic.contains("codex_error_info=\"badRequest\""));
    assert!(diagnostic.contains("will_retry=false"));
    assert_eq!(simulation.input_tokens, Some(100));
    assert_eq!(simulation.cached_input_tokens, Some(40));
    assert_eq!(simulation.output_tokens, Some(20));
    assert_eq!(simulation.session_cleanup_success, Some(true));
    assert!(simulation.no_pending_worker_sessions);
    assert!(simulation.checkout_clean);
    assert!(simulation.sandboxes_cleaned);
}

struct Simulation {
    frontier: String,
    worker_status: String,
    failure_code: Option<String>,
    failure_diagnostic: Option<String>,
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    logical_worker_spawns: u32,
    worker_turns: u32,
    repair_performed: bool,
    command_evidence_count: u64,
    no_pending_approvals: bool,
    exact_cache_hit: bool,
    worker_run_count: u64,
    session_cleanup_success: Option<bool>,
    no_pending_worker_sessions: bool,
    checkout_clean: bool,
    sandboxes_cleaned: bool,
}

fn active_role_profile(store: &RuntimeStore, prompt_profile_digest: Digest) -> RoleProfileId {
    let profile_id = RoleProfileId::new("offline.explorer").unwrap();
    let definition = RoleProfileDefinition::new(RoleProfileDefinitionInput {
        profile_id: profile_id.clone(),
        role: CodexRole::Explorer,
        host: CodexHost::Codex,
        model: "simulated-worker".to_owned(),
        reasoning: needle_core::ReasoningLevel::Medium,
        service_tier: ServiceTier::Default,
        timeout_seconds: 10,
        budget: RoleProfileBudget {
            max_turns: 2,
            max_output_tokens: 1200,
            max_cost_microusd: 1000,
        },
        prompt_profile_digest,
        output_contract_digest: Digest::blake3(needle_core::ARTIFACT_RESULT_SCHEMA_ID),
        tool_policy: ToolPolicy::ReadOnly,
        command_policy: CommandPolicy::ReadOnly,
        filesystem_policy: FilesystemPolicy::ReadOnlyCheckout,
        network_policy: NetworkPolicy::Denied,
        test_policy: TestPolicy::Disabled,
        repair_policy: RepairPolicy::Once,
        fallback_policy: FallbackPolicy::Native,
        concurrency: 1,
        route_assignments: Vec::new(),
    })
    .unwrap();
    store.create_role_profile(definition).unwrap();
    let state = store.role_profile_state(&profile_id).unwrap();
    store.activate_role_profile(&profile_id, 1, state.state_digest).unwrap();
    profile_id
}

impl Simulation {
    fn run(scenario: &str, repetition: u32) -> Self {
        let root = temporary_root(scenario, repetition);
        let repository = root.join("repository");
        let data = root.join("data");
        create_repository(&repository, scenario);
        fs::create_dir_all(&data).unwrap();

        let store = RuntimeStore::new(data.join("needle.sqlite3"));
        store
            .initialize_defaults(&RuntimeSettings {
                codex_executable: SIMULATOR.to_owned(),
                worker_model: "simulated-worker".to_owned(),
                worker_reasoning: "medium".to_owned(),
                worker_timeout_seconds: 10,
                evidence_failure_policy: EvidenceFailurePolicy::RepairOnce,
                trusted_test_execution: true,
                multi_need_policy: needle_core::MultiNeedPolicy::default(),
            })
            .unwrap();
        let session_id = format!("offline-session-{repetition}");
        let turn_id = format!("offline-turn-{repetition}");
        let profile = HookConfig::default().profile().unwrap();
        let role_profile_id = active_role_profile(&store, profile.definition_digest);
        store
            .record_session_start_profiled(
                &session_id,
                profile.definition_digest,
                Some("simulated-main"),
                repository.to_str(),
                &role_profile_id,
            )
            .unwrap();
        store
            .record_user_prompt(
                &session_id,
                Some(&turn_id),
                "Trace the simulated flag from parsing to application.",
                repository.to_str(),
            )
            .unwrap();

        let engine = RuntimeEngine::new(
            RuntimeStore::new(data.join("needle.sqlite3")),
            CodexWorker::new(&data),
        );
        let stop = StopInput {
            session_id: Some(session_id.clone()),
            turn_id: Some(turn_id.clone()),
            last_assistant_message: Some(
                "@@need:trace.state-flow\nTrace the simulated flag.\n@@end".to_owned(),
            ),
            cwd: repository.to_str().map(str::to_owned),
            model: Some("simulated-main".to_owned()),
            ..StopInput::default()
        };
        let hook = HookConfig { plugin_data: Some(data.clone()), ..HookConfig::default() };
        let output = handle_stop_with_resolver(&stop, &hook, |need| {
            engine
                .resolve(&ResolveRequest {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    platform: "codex".to_owned(),
                    main_model: "simulated-main".to_owned(),
                    cwd: repository.clone(),
                    need: need.compatibility_request(),
                    need_ir: need.typed().cloned(),
                    declared_test_plan: Some(test_plan()),
                })
                .map(|outcome| Some(outcome.rendered))
                .map_err(|error| error.to_string())
        })
        .unwrap();
        let frontier = output.reason.unwrap_or_default();
        let exact_cache_hit = if scenario == "repair_success" {
            store.mark_utility_gate_passed().unwrap();
            let cache_session = format!("offline-cache-session-{repetition}");
            let cache_turn = format!("offline-cache-turn-{repetition}");
            store
                .record_session_start_profiled(
                    &cache_session,
                    profile.definition_digest,
                    Some("simulated-main"),
                    repository.to_str(),
                    &role_profile_id,
                )
                .unwrap();
            store
                .record_user_prompt(
                    &cache_session,
                    Some(&cache_turn),
                    "Trace the simulated flag from parsing to application.",
                    repository.to_str(),
                )
                .unwrap();
            let cache_seed = engine
                .resolve(&ResolveRequest {
                    session_id: cache_session,
                    turn_id: cache_turn,
                    platform: "codex".to_owned(),
                    main_model: "simulated-main".to_owned(),
                    cwd: repository.clone(),
                    need: simulated_need(),
                    need_ir: None,
                    declared_test_plan: Some(test_plan()),
                })
                .unwrap();
            assert!(!cache_seed.cache_hit);
            assert!(cache_seed.worker_spawned);

            let hit_session = format!("offline-hit-session-{repetition}");
            let hit_turn = format!("offline-hit-turn-{repetition}");
            store
                .record_session_start_profiled(
                    &hit_session,
                    profile.definition_digest,
                    Some("simulated-main"),
                    repository.to_str(),
                    &role_profile_id,
                )
                .unwrap();
            store
                .record_user_prompt(
                    &hit_session,
                    Some(&hit_turn),
                    "Trace the simulated flag from parsing to application.",
                    repository.to_str(),
                )
                .unwrap();
            let hit = engine
                .resolve(&ResolveRequest {
                    session_id: hit_session,
                    turn_id: hit_turn,
                    platform: "codex".to_owned(),
                    main_model: "simulated-main".to_owned(),
                    cwd: repository.clone(),
                    need: simulated_need(),
                    need_ir: None,
                    declared_test_plan: Some(test_plan()),
                })
                .unwrap();
            assert!(!hit.worker_spawned);
            hit.cache_hit
        } else {
            false
        };
        let worker = store.latest_worker_run().unwrap().expect("worker run");
        let worker_provenance = worker
            .role_profile_provenance
            .as_ref()
            .expect("profiled sessions must retain worker provenance");
        assert_eq!(&worker_provenance.profile_id, &role_profile_id);
        assert_eq!(worker_provenance.revision, 1);
        let worker_run_count = store.worker_run_count().unwrap();
        let command_evidence_count = store.command_evidence_count().unwrap();
        let no_pending_approvals = store.pending_approvals().unwrap().is_empty();
        let no_pending_worker_sessions = store.pending_worker_sessions().unwrap().is_empty();
        let checkout_clean = git_output(&repository, &["status", "--porcelain"]).is_empty();
        let worker_root = data.join("worker-runs");
        let sandboxes_cleaned =
            !worker_root.exists() || fs::read_dir(&worker_root).unwrap().next().is_none();

        let result = Self {
            frontier,
            worker_status: if worker.failure_code.is_some() {
                "failed".to_owned()
            } else {
                "completed".to_owned()
            },
            failure_code: worker.failure_code,
            failure_diagnostic: worker.failure_diagnostic,
            input_tokens: worker.input_tokens,
            cached_input_tokens: worker.cached_input_tokens,
            output_tokens: worker.output_tokens,
            logical_worker_spawns: worker.logical_worker_spawns,
            worker_turns: worker.worker_turns,
            repair_performed: worker.repair_performed,
            command_evidence_count,
            no_pending_approvals,
            exact_cache_hit,
            worker_run_count,
            session_cleanup_success: worker.session_cleanup_success,
            no_pending_worker_sessions,
            checkout_clean,
            sandboxes_cleaned,
        };
        drop(engine);
        drop(store);
        fs::remove_dir_all(root).unwrap();
        result
    }
}

fn test_plan() -> TestPlan {
    TestPlan {
        runner: "cargo".to_owned(),
        argv: ["cargo", "test", "suite::focused", "--", "--exact"].map(str::to_owned).to_vec(),
        cwd_relative: ".".to_owned(),
        test_identifier: "suite::focused".to_owned(),
        requires_approval: true,
        execution_evidence_id: None,
    }
}

fn simulated_need() -> NeedRequest {
    NeedRequest::parse("@@need:trace.state-flow\nTrace the simulated flag.\n@@end")
        .unwrap()
        .unwrap()
}

fn create_repository(repository: &Path, scenario: &str) {
    fs::create_dir_all(repository.join("src")).unwrap();
    run_git(repository, &["init", "--quiet"]);
    run_git(repository, &["config", "core.autocrlf", "false"]);
    run_git(repository, &["config", "user.email", "needle@example.invalid"]);
    run_git(repository, &["config", "user.name", "Needle Simulation"]);
    fs::write(
        repository.join("src/lib.rs"),
        "pub fn flag_definition() -> bool { true }\npub fn apply_flag() { let _ = flag_definition(); }\n",
    )
    .unwrap();
    fs::write(repository.join(".needle-simulation-scenario"), scenario).unwrap();
    run_git(repository, &["add", "."]);
    run_git(repository, &["commit", "--quiet", "-m", "offline fixture"]);
}

fn run_git(repository: &Path, arguments: &[&str]) {
    let status = Command::new("git").args(arguments).current_dir(repository).status().unwrap();
    assert!(status.success(), "git {}", arguments.join(" "));
}

fn git_output(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git").args(arguments).current_dir(repository).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

fn temporary_root(scenario: &str, repetition: u32) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "needle-offline-n1-{}-{}-{scenario}-{repetition}-{id}",
        std::process::id(),
        Digest::blake3(format!("{scenario}-{repetition}-{id}")).to_hex()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}
