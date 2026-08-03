use needle_core::{
    ApprovalDecision, ApprovalDecisionSource, CodexHost, CodexRole, CommandClassification,
    CommandPolicy, Digest, EvidenceFailurePolicy, FallbackPolicy, FilesystemPolicy,
    NeedCoordination, NeedDelivery, NeedStepRelation, NetworkPolicy, PredicateKind, RepairPolicy,
    RoleProfileBudget, RoleProfileDefinition, RoleProfileDefinitionInput, RoleProfileId,
    ServiceTier, TestPlan, TestPolicy, ToolPolicy, WorkerConfig, built_in_route_contracts,
    classify_need_step, compile_need,
};
use needle_platform_codex::{
    CodexMainSession, CodexWorker, HookConfig, MainNeedRelation, MainSessionConfig, MainTurnResult,
    PILOT_MAIN_REPOSITORY_INSPECTION_INSTRUCTIONS,
};
use needle_runtime::{ResolveRequest, RuntimeEngine, RuntimeSettings, RuntimeStore};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SIMULATOR: &str = env!("CARGO_BIN_EXE_needle-sim-codex");
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn active_role_profile(store: &RuntimeStore, prompt_profile_digest: Digest) -> RoleProfileId {
    let profile_id = RoleProfileId::new("main-interrupt.explorer").unwrap();
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
        repair_policy: RepairPolicy::None,
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

#[test]
fn direct_main_completes_without_semantic_interrupt() {
    let root = temporary_root();
    let repository = root.join("repository");
    let target = root.join("target");
    let temp = root.join("temp");
    let codex_home = root.join("codex-home");
    let data = root.join("data");
    create_repository(&repository);
    for directory in [&target, &temp, &codex_home, &data] {
        fs::create_dir_all(directory).unwrap();
    }
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store.initialize().unwrap();
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "gpt-5.6-sol".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    let mut session = CodexMainSession::start_pilot(MainSessionConfig {
        codex: &config,
        codex_home: &codex_home,
        instructions: PILOT_MAIN_REPOSITORY_INSPECTION_INSTRUCTIONS,
        checkout_root: &repository,
        target_root: &target,
        temp_root: &temp,
        snapshot_digest: Digest::blake3(b"direct-main-source"),
        repository_id: Digest::blake3(b"direct-main-repository"),
        route: "benchmark.main-only",
        store,
    })
    .unwrap();

    let result = session
        .run_direct(
            "Where is --glob-case-insensitive implemented, and which focused test proves it?",
            Duration::from_secs(5),
        )
        .unwrap();
    assert!(result.response.contains("crates/core/flags/hiargs.rs"));
    assert!(result.response.contains("misc::glob_always_case_insensitive"));
    assert_eq!(result.tool_items_started, 0);
    assert_eq!(result.usage.input_tokens, Some(500));
    session.cleanup().unwrap();
    let _ = fs::remove_dir_all(root);
}

#[test]
fn pilot_main_auto_approves_one_bounded_repository_read() {
    let root = temporary_root();
    let repository = root.join("repository");
    let target = root.join("target");
    let temp = root.join("temp");
    let codex_home = root.join("codex-home");
    let data = root.join("data");
    create_repository_with_scenario(&repository, "main_direct_read_only");
    for directory in [&target, &temp, &codex_home, &data] {
        fs::create_dir_all(directory).unwrap();
    }
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store.initialize().unwrap();
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "simulated-main-direct".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    let mut session = CodexMainSession::start_pilot(MainSessionConfig {
        codex: &config,
        codex_home: &codex_home,
        instructions: PILOT_MAIN_REPOSITORY_INSPECTION_INSTRUCTIONS,
        checkout_root: &repository,
        target_root: &target,
        temp_root: &temp,
        snapshot_digest: Digest::blake3(b"direct-main-read-source"),
        repository_id: Digest::blake3(b"direct-main-read-repository"),
        route: "benchmark.main-only",
        store: store.clone(),
    })
    .unwrap();

    let result = session.run_direct("Locate flag_definition.", Duration::from_secs(5)).unwrap();
    assert!(result.response.contains("src/lib.rs"));
    assert_eq!(result.tool_items_started, 1);
    assert_eq!(result.usage.input_tokens, Some(100));
    let approval = store.approval("main-direct-read-only").unwrap().unwrap();
    assert!(matches!(approval.classification, CommandClassification::AutoApprovedReadOnly { .. }));
    assert_eq!(approval.decision, Some(ApprovalDecision::Accept));
    assert_eq!(approval.decision_source, Some(ApprovalDecisionSource::AutoPolicy));
    session.cleanup().unwrap();
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pilot_main_fails_fast_on_r84_style_script_and_preserves_usage() {
    let root = temporary_root();
    let repository = root.join("repository");
    let target = root.join("target");
    let temp = root.join("temp");
    let codex_home = root.join("codex-home");
    let data = root.join("data");
    create_repository_with_scenario(&repository, "main_direct_r84_pending_approval");
    for directory in [&target, &temp, &codex_home, &data] {
        fs::create_dir_all(directory).unwrap();
    }
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store.initialize().unwrap();
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "simulated-main-direct".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    let mut session = CodexMainSession::start_pilot(MainSessionConfig {
        codex: &config,
        codex_home: &codex_home,
        instructions: PILOT_MAIN_REPOSITORY_INSPECTION_INSTRUCTIONS,
        checkout_root: &repository,
        target_root: &target,
        temp_root: &temp,
        snapshot_digest: Digest::blake3(b"direct-main-r84-source"),
        repository_id: Digest::blake3(b"direct-main-r84-repository"),
        route: "benchmark.main-only",
        store: store.clone(),
    })
    .unwrap();

    let started = Instant::now();
    let error =
        session.run_direct("Trace the implementation.", Duration::from_secs(5)).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(error.diagnostic.contains("classification: pending_user"));
    assert_eq!(error.tool_items_started, 1);
    assert_eq!(error.usage.input_tokens, Some(100));
    let approval = store.approval("main-direct-r84-pending").unwrap().unwrap();
    assert_eq!(approval.classification, CommandClassification::PendingUser);
    assert_eq!(approval.decision, Some(ApprovalDecision::Decline));
    assert_eq!(approval.decision_source, Some(ApprovalDecisionSource::Runtime));
    assert!(store.pending_approvals().unwrap().is_empty());
    session.cleanup().unwrap();
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn semantic_message_interrupts_before_tools_and_continues_same_thread() {
    let root = temporary_root();
    let repository = root.join("repository");
    let target = root.join("target");
    let temp = root.join("temp");
    let codex_home = root.join("codex-home");
    let data = root.join("data");
    create_repository(&repository);
    for directory in [&target, &temp, &codex_home, &data] {
        fs::create_dir_all(directory).unwrap();
    }
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store.initialize().unwrap();
    let profile = HookConfig::default().profile().unwrap();
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "simulated-main".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    let source_digest = Digest::blake3(b"main-interrupt-source");
    let mut session = CodexMainSession::start(MainSessionConfig {
        codex: &config,
        codex_home: &codex_home,
        instructions: &profile.rendered_context_owned(),
        checkout_root: &repository,
        target_root: &target,
        temp_root: &temp,
        snapshot_digest: source_digest,
        repository_id: Digest::blake3(b"main-interrupt-repository"),
        route: "locate.implementation",
        store,
    })
    .unwrap();

    let thread_id = session.thread_id().to_owned();
    let need = session
        .run_until_need("Where is flag_definition implemented?", Duration::from_secs(5))
        .unwrap();
    assert_eq!(need.thread_id, thread_id);
    assert_eq!(need.semantic_interrupt.key().as_str(), "locate.implementation");
    assert!(need.interrupt_acknowledged);
    assert_eq!(need.terminal_status, "interrupted");
    assert_eq!(need.tool_items_started, 0);
    assert_eq!(need.usage.input_tokens, Some(100));

    let final_turn = session
        .run_continuation(
            "[NEEDLE_CONTEXT]\nvalidated evidence\n[/NEEDLE_CONTEXT]\n\nContinue.",
            Duration::from_secs(5),
        )
        .unwrap();
    assert!(final_turn.response.contains("src/lib.rs"));
    assert_eq!(final_turn.tool_items_started, 0);
    assert_eq!(final_turn.usage.input_tokens, Some(200));
    session.cleanup().unwrap();

    assert!(git_output(&repository, &["status", "--porcelain"]).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn nested_continuation_need_is_preserved_and_classified_before_cleanup() {
    let root = temporary_root();
    let repository = root.join("repository");
    let target = root.join("target");
    let temp = root.join("temp");
    let codex_home = root.join("codex-home");
    let data = root.join("data");
    create_repository_with_scenario(&repository, "main_interrupt_nested_same");
    for directory in [&target, &temp, &codex_home, &data] {
        fs::create_dir_all(directory).unwrap();
    }
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store.initialize().unwrap();
    let profile = HookConfig::default().profile().unwrap();
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "simulated-main".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    let mut session = CodexMainSession::start(MainSessionConfig {
        codex: &config,
        codex_home: &codex_home,
        instructions: &profile.rendered_context_owned(),
        checkout_root: &repository,
        target_root: &target,
        temp_root: &temp,
        snapshot_digest: Digest::blake3(b"main-nested-source"),
        repository_id: Digest::blake3(b"main-nested-repository"),
        route: "locate.implementation",
        store,
    })
    .unwrap();

    let need = session
        .run_until_need("Where is flag_definition implemented?", Duration::from_secs(5))
        .unwrap();
    let original_message = session.last_need_diagnostics().unwrap().raw_message.clone();
    let original_digest = need.semantic_interrupt.digest();
    let error = session
        .run_continuation(
            "[NEEDLE_CONTEXT]\nvalidated evidence\n[/NEEDLE_CONTEXT]\n\nContinue.",
            Duration::from_secs(5),
        )
        .unwrap_err();
    assert_eq!(error, "main emitted a nested semantic interrupt");
    let diagnostics = session.last_continuation_diagnostics().unwrap();
    assert_eq!(diagnostics.format_revision, 1);
    assert_eq!(diagnostics.raw_message, original_message);
    assert_eq!(diagnostics.raw_message_digest, Some(Digest::blake3(&original_message)));
    assert_eq!(diagnostics.parse_error, None);
    assert_eq!(diagnostics.semantic_interrupt_digest, Some(original_digest));
    assert_eq!(diagnostics.relation_to_original, Some(MainNeedRelation::IdenticalMessage));
    assert_eq!(diagnostics.terminal_status.as_deref(), Some("completed"));
    assert_eq!(diagnostics.violation.as_deref(), Some(error.as_str()));
    assert_eq!(diagnostics.tool_items_started, 0);
    assert_eq!(session.provider_turns_started(), 2);
    session.cleanup().unwrap();

    assert!(git_output(&repository, &["status", "--porcelain"]).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sequential_turns_accept_two_needs_then_return_a_final_response() {
    let root = temporary_root();
    let repository = root.join("repository");
    let target = root.join("target");
    let temp = root.join("temp");
    let codex_home = root.join("codex-home");
    let data = root.join("data");
    create_repository_with_scenario(&repository, "main_interrupt_two_needs");
    for directory in [&target, &temp, &codex_home, &data] {
        fs::create_dir_all(directory).unwrap();
    }
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store.initialize().unwrap();
    let profile = HookConfig::default().profile().unwrap();
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "simulated-main".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    let mut session = CodexMainSession::start(MainSessionConfig {
        codex: &config,
        codex_home: &codex_home,
        instructions: &profile.rendered_context_owned(),
        checkout_root: &repository,
        target_root: &target,
        temp_root: &temp,
        snapshot_digest: Digest::blake3(b"multi-need-source"),
        repository_id: Digest::blake3(b"multi-need-repository"),
        route: "locate.implementation",
        store,
    })
    .unwrap();

    let first = session.run_until_need("Locate it.", Duration::from_secs(5)).unwrap();
    assert_eq!(first.semantic_interrupt.key().as_str(), "locate.implementation");
    let second = session
        .run_next("[NEEDLE_CONTEXT]\nlocation\n[/NEEDLE_CONTEXT]", Duration::from_secs(5), false)
        .unwrap();
    let MainTurnResult::Need(second) = second else {
        panic!("second turn must request the residual test")
    };
    assert_eq!(second.semantic_interrupt.key().as_str(), "tests.relevant");
    let repository_id = Digest::blake3(b"multi-need-repository");
    let contracts = built_in_route_contracts();
    let first_need = compile_need(
        first.semantic_interrupt.typed().unwrap(),
        repository_id,
        contracts
            .iter()
            .find(|contract| contract.route.as_str() == "locate.implementation")
            .unwrap(),
    )
    .unwrap();
    let second_need = compile_need(
        second.semantic_interrupt.typed().unwrap(),
        repository_id,
        contracts.iter().find(|contract| contract.route.as_str() == "tests.relevant").unwrap(),
    )
    .unwrap();
    assert!(first_need.required.iter().any(|item| item.predicate == PredicateKind::FocusedTests));
    let satisfied = first_need
        .required
        .iter()
        .filter(|item| item.predicate == PredicateKind::ImplementationLocation)
        .map(|item| item.id)
        .collect::<Vec<_>>();
    assert_eq!(
        classify_need_step(&first_need, &second_need, &satisfied),
        NeedStepRelation::Residual
    );
    let final_turn = session
        .run_next("[NEEDLE_CONTEXT]\ntest\n[/NEEDLE_CONTEXT]", Duration::from_secs(5), false)
        .unwrap();
    let MainTurnResult::Final(final_turn) = final_turn else { panic!("third turn must answer") };
    assert!(final_turn.response.contains("src/lib.rs"));
    assert_eq!(final_turn.usage.input_tokens, Some(300));
    assert_eq!(session.provider_turns_started(), 3);
    session.cleanup().unwrap();
    assert!(git_output(&repository, &["status", "--porcelain"]).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn continue_working_steers_the_active_turn_and_taints_pending_tools() {
    let (root, repository, mut session) = main_session("main_interrupt_continue_tools");
    let need = session.run_until_need("Locate it.", Duration::from_secs(5)).unwrap();
    assert!(need.active_turn);
    assert_eq!(need.semantic_interrupt.coordination(), NeedCoordination::ContinueWorking);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    sender.send(Ok::<_, String>("validated context".to_owned())).unwrap();
    let continued = session
        .await_resolution_and_steer(
            &need.turn_id,
            &receiver,
            |context, _| format!("[NEEDLE_CONTEXT]\n{context}\n[/NEEDLE_CONTEXT]"),
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(continued.delivery, NeedDelivery::TurnSteer);
    assert!(continued.main_discovery_tainted);
    assert_eq!(continued.tool_items_started, 1);
    let Some(MainTurnResult::Final(final_turn)) = continued.outcome else {
        panic!("steered turn must finish")
    };
    assert!(final_turn.response.contains("steered response"));
    session.cleanup().unwrap();
    assert!(git_output(&repository, &["status", "--porcelain"]).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn continue_working_queues_multiple_needs_fifo_while_resolution_is_active() {
    let (root, repository, mut session) = main_session("main_interrupt_continue_queued");
    let need = session.run_until_need("Locate it.", Duration::from_secs(5)).unwrap();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let sender_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        sender.send(Ok::<_, String>("validated context".to_owned())).unwrap();
    });
    let rendered_pending = AtomicU64::new(0);
    let continued = session
        .await_resolution_and_steer(
            &need.turn_id,
            &receiver,
            |context, pending| {
                rendered_pending.store(pending as u64, Ordering::Release);
                format!("[NEEDLE_CONTEXT]\n{context}\n[/NEEDLE_CONTEXT]")
            },
            Duration::from_secs(5),
        )
        .unwrap();
    sender_thread.join().unwrap();
    assert_eq!(continued.delivery, NeedDelivery::TurnSteer);
    assert_eq!(rendered_pending.load(Ordering::Acquire), 2);
    assert_eq!(continued.queued_needs.len(), 2);
    assert_eq!(continued.queued_needs[0].semantic_interrupt.key().as_str(), "tests.relevant");
    assert_eq!(continued.queued_needs[1].semantic_interrupt.key().as_str(), "trace.state-flow");
    assert_eq!(
        continued.queued_needs[0].semantic_interrupt.coordination(),
        NeedCoordination::ContinueWorking
    );
    assert!(!continued.queued_needs[0].active_turn);
    assert!(!continued.queued_needs[1].active_turn);
    assert!(matches!(continued.outcome, Some(MainTurnResult::Final(_))));
    session.cleanup().unwrap();
    assert!(git_output(&repository, &["status", "--porcelain"]).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn continue_working_marks_pending_queue_overflow_without_failing_the_turn() {
    let (root, repository, mut session) = main_session("main_interrupt_continue_queue_overflow");
    let need = session.run_until_need("Locate it.", Duration::from_secs(5)).unwrap();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let sender_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        sender.send(Ok::<_, String>("validated context".to_owned())).unwrap();
    });
    let continued = session
        .await_resolution_and_steer(
            &need.turn_id,
            &receiver,
            |context, _| context.clone(),
            Duration::from_secs(5),
        )
        .unwrap();
    sender_thread.join().unwrap();
    assert_eq!(continued.queued_needs.len(), 8);
    assert!(continued.queue_overflowed);
    assert!(matches!(continued.outcome, Some(MainTurnResult::Final(_))));
    session.cleanup().unwrap();
    assert!(git_output(&repository, &["status", "--porcelain"]).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn continue_working_degrades_when_the_active_turn_is_not_steerable() {
    let (root, repository, mut session) = main_session("main_interrupt_continue_not_steerable");
    let need = session.run_until_need("Locate it.", Duration::from_secs(5)).unwrap();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    sender.send(Ok::<_, String>("validated context".to_owned())).unwrap();
    let continued = session
        .await_resolution_and_steer(
            &need.turn_id,
            &receiver,
            |context, _| context.clone(),
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(continued.delivery, NeedDelivery::TurnStart);
    assert!(continued.outcome.is_none());
    session.cleanup().unwrap();
    assert!(git_output(&repository, &["status", "--porcelain"]).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn continue_working_reports_task_cancellation_with_usage() {
    let (root, repository, mut session) = main_session("main_interrupt_continue_cancelled");
    let need = session.run_until_need("Locate it.", Duration::from_secs(5)).unwrap();
    let (_sender, receiver) = std::sync::mpsc::sync_channel::<Result<String, String>>(1);
    let cancelled = AtomicBool::new(false);
    let observed_usage = std::sync::Mutex::new(None);
    let error = session
        .await_resolution_and_steer_cancellable(
            &need.turn_id,
            &receiver,
            |context, _| context.clone(),
            |usage, _, _| {
                cancelled.store(true, Ordering::Release);
                *observed_usage.lock().unwrap() = Some(usage);
            },
            Duration::from_secs(5),
        )
        .unwrap_err();
    assert!(error.contains("main task cancelled"));
    assert!(cancelled.load(Ordering::Acquire));
    assert_eq!(observed_usage.lock().unwrap().unwrap().input_tokens, Some(100));
    session.cleanup().unwrap();
    assert!(git_output(&repository, &["status", "--porcelain"]).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn r35_cache_main_simulation_returns_the_frontier_answer_without_tools() {
    let root = temporary_root();
    let repository = root.join("repository");
    let target = root.join("target");
    let temp = root.join("temp");
    let codex_home = root.join("codex-home");
    let data = root.join("data");
    create_repository(&repository);
    for directory in [&target, &temp, &codex_home, &data] {
        fs::create_dir_all(directory).unwrap();
    }
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store.initialize().unwrap();
    let profile = HookConfig::default().profile().unwrap();
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "simulated-main-r35-cache".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    let mut session = CodexMainSession::start(MainSessionConfig {
        codex: &config,
        codex_home: &codex_home,
        instructions: &profile.rendered_context_owned(),
        checkout_root: &repository,
        target_root: &target,
        temp_root: &temp,
        snapshot_digest: Digest::blake3(b"r35-main-source"),
        repository_id: Digest::blake3(b"r35-main-repository"),
        route: "locate.implementation",
        store,
    })
    .unwrap();

    let need = session
        .run_until_need("Where is --glob-case-insensitive implemented?", Duration::from_secs(5))
        .unwrap();
    assert_eq!(need.semantic_interrupt.key().as_str(), "locate.implementation");
    assert_eq!(
        need.semantic_interrupt.typed().unwrap().subjects[0].canonical_name,
        "--glob-case-insensitive"
    );
    assert_eq!(
        need.semantic_interrupt.compatibility_request().body,
        "Locate the option implementation."
    );
    assert!(need.interrupt_acknowledged);
    assert_eq!(need.tool_items_started, 0);

    let final_turn = session
        .run_continuation(
            "[NEEDLE_CONTEXT]\nvalidated r35 cache frontier\n[/NEEDLE_CONTEXT]\n\nContinue.",
            Duration::from_secs(5),
        )
        .unwrap();
    assert!(final_turn.response.contains("crates/core/flags/hiargs.rs"));
    assert!(final_turn.response.contains("GlobCaseInsensitive::update"));
    assert_eq!(final_turn.tool_items_started, 0);
    assert_eq!(session.provider_turns_started(), 2);
    session.cleanup().unwrap();
    assert!(git_output(&repository, &["status", "--porcelain"]).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_subject_is_interrupted_and_preserved_before_validation() {
    let root = temporary_root();
    let repository = root.join("repository");
    let target = root.join("target");
    let temp = root.join("temp");
    let codex_home = root.join("codex-home");
    let data = root.join("data");
    create_repository_with_scenario(&repository, "main_interrupt_invalid_subject");
    for directory in [&target, &temp, &codex_home, &data] {
        fs::create_dir_all(directory).unwrap();
    }
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store.initialize().unwrap();
    let profile = HookConfig::default().profile().unwrap();
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "simulated-main".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    let mut session = CodexMainSession::start(MainSessionConfig {
        codex: &config,
        codex_home: &codex_home,
        instructions: &profile.rendered_context_owned(),
        checkout_root: &repository,
        target_root: &target,
        temp_root: &temp,
        snapshot_digest: Digest::blake3(b"main-invalid-source"),
        repository_id: Digest::blake3(b"main-invalid-repository"),
        route: "locate.implementation",
        store,
    })
    .unwrap();

    let error = session
        .run_until_need("Where is the option implemented?", Duration::from_secs(5))
        .unwrap_err();
    assert!(error.contains("NeedIR header `@subject` is unknown or malformed"));
    let diagnostics = session.last_need_diagnostics().unwrap();
    assert!(diagnostics.raw_message.contains("@subject cli_flag:\"--glob-case-insensitive\""));
    assert_eq!(diagnostics.parse_error.as_deref(), Some(error.as_str()));
    assert!(diagnostics.interrupt_requested);
    assert!(diagnostics.interrupt_acknowledged);
    assert_eq!(diagnostics.terminal_status.as_deref(), Some("interrupted"));
    assert_eq!(diagnostics.tool_items_started, 0);
    assert_eq!(diagnostics.usage.input_tokens, Some(100));
    assert_eq!(session.provider_turns_started(), 1);
    session.cleanup().unwrap();

    assert!(git_output(&repository, &["status", "--porcelain"]).is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn supervised_main_resolves_with_worker_then_continues_without_discovery() {
    let root = temporary_root();
    let repository = root.join("repository");
    let target = root.join("target");
    let temp = root.join("temp");
    let codex_home = root.join("codex-home");
    let data = root.join("data");
    create_repository(&repository);
    for directory in [&target, &temp, &codex_home, &data] {
        fs::create_dir_all(directory).unwrap();
    }
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store
        .initialize_defaults(&RuntimeSettings {
            codex_executable: SIMULATOR.to_owned(),
            worker_model: "simulated-worker".to_owned(),
            worker_reasoning: "medium".to_owned(),
            worker_timeout_seconds: 10,
            evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
            trusted_test_execution: true,
            multi_need_policy: needle_core::MultiNeedPolicy::default(),
        })
        .unwrap();
    let profile = HookConfig::default().profile().unwrap();
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "simulated-main".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    let source_digest = Digest::blake3(b"main-resolve-source");
    let repository_id = Digest::blake3(b"main-resolve-repository");
    let mut session = CodexMainSession::start(MainSessionConfig {
        codex: &config,
        codex_home: &codex_home,
        instructions: &profile.rendered_context_owned(),
        checkout_root: &repository,
        target_root: &target,
        temp_root: &temp,
        snapshot_digest: source_digest,
        repository_id,
        route: "locate.implementation",
        store: RuntimeStore::new(data.join("needle.sqlite3")),
    })
    .unwrap();
    let session_id = session.thread_id().to_owned();
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

    let need = session
        .run_until_need("Where is flag_definition implemented?", Duration::from_secs(5))
        .unwrap();
    store
        .record_user_prompt(
            &session_id,
            Some(&need.turn_id),
            "Where is flag_definition implemented?",
            repository.to_str(),
        )
        .unwrap();
    let interrupt = need.semantic_interrupt;
    let engine = RuntimeEngine::new(
        RuntimeStore::new(data.join("needle.sqlite3")),
        CodexWorker::with_codex_home(&data, &codex_home),
    );
    let outcome = engine
        .resolve(&ResolveRequest {
            session_id: session_id.clone(),
            turn_id: need.turn_id,
            platform: "codex".to_owned(),
            main_model: "simulated-main".to_owned(),
            cwd: repository.clone(),
            need: interrupt.compatibility_request(),
            need_ir: interrupt.typed().cloned(),
            declared_test_plan: Some(test_plan()),
        })
        .unwrap();
    assert!(outcome.worker_spawned);
    assert!(outcome.rendered.contains("[NEEDLE_CONTEXT]"));
    let final_turn = session.run_continuation(&outcome.rendered, Duration::from_secs(5)).unwrap();
    assert!(final_turn.response.contains("src/lib.rs"));
    assert_eq!(need.tool_items_started + final_turn.tool_items_started, 0);
    assert_eq!(store.worker_run_count().unwrap(), 1);
    // The semantic need requests only ImplementationLocation. A declared
    // TestPlan that is not part of the missing obligations must not cause a
    // worker test execution.
    assert_eq!(store.command_evidence_count().unwrap(), 0);
    session.cleanup().unwrap();
    store.end_session(&session_id).unwrap();
    assert!(git_output(&repository, &["status", "--porcelain"]).is_empty());
    drop(engine);
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

fn main_session(scenario: &str) -> (PathBuf, PathBuf, CodexMainSession) {
    let root = temporary_root();
    let repository = root.join("repository");
    let target = root.join("target");
    let temp = root.join("temp");
    let codex_home = root.join("codex-home");
    let data = root.join("data");
    create_repository_with_scenario(&repository, scenario);
    for directory in [&target, &temp, &codex_home, &data] {
        fs::create_dir_all(directory).unwrap();
    }
    let store = RuntimeStore::new(data.join("needle.sqlite3"));
    store.initialize().unwrap();
    let profile = HookConfig::default().profile().unwrap();
    let config = WorkerConfig {
        executable: SIMULATOR.to_owned(),
        model: "simulated-main".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    let session = CodexMainSession::start(MainSessionConfig {
        codex: &config,
        codex_home: &codex_home,
        instructions: &profile.rendered_context_owned(),
        checkout_root: &repository,
        target_root: &target,
        temp_root: &temp,
        snapshot_digest: Digest::blake3(b"continue-source"),
        repository_id: Digest::blake3(b"continue-repository"),
        route: "locate.implementation",
        store,
    })
    .unwrap();
    (root, repository, session)
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

fn create_repository(repository: &Path) {
    create_repository_with_scenario(repository, "main_interrupt");
}

fn create_repository_with_scenario(repository: &Path, scenario: &str) {
    fs::create_dir_all(repository.join("src")).unwrap();
    run_git(repository, &["init", "--quiet"]);
    run_git(repository, &["config", "core.autocrlf", "false"]);
    run_git(repository, &["config", "user.email", "needle@example.invalid"]);
    run_git(repository, &["config", "user.name", "Needle Simulation"]);
    fs::write(repository.join("src/lib.rs"), "pub fn flag_definition() {}\n").unwrap();
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

fn temporary_root() -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("needle-main-interrupt-{}-{id}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}
