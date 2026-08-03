use super::{
    AppError, HookConfig, absolute_run_path, canonical_child_path, option_value,
    repository_status_clean, required_value, resolve_codex,
};
use needle_bench::{ArtifactCacheReplayReport, run_artifact_cache_replay};
use needle_core::{
    CacheResolution, Digest, EvidenceFailurePolicy, WorkerConfig, WorkerFailure, WorkerOutcome,
    WorkerRequest,
};
use needle_platform_codex::{CodexMainSession, MainSessionConfig};
use needle_runtime::{
    ResolveRequest, RuntimeEngine, RuntimeStore, WorkerExecutor, capture_git_snapshot,
};
use serde::Serialize;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

const REPORT_SCHEMA: &str = "needle.artifact-cache-main-replay/1";
const REPORT_FILE: &str = "artifact-cache-main-replay-report.json";

#[derive(Clone)]
struct ForbiddenWorker {
    calls: Arc<AtomicU32>,
}

impl WorkerExecutor for ForbiddenWorker {
    fn execute(
        &self,
        _config: &WorkerConfig,
        _request: &WorkerRequest,
    ) -> Result<WorkerOutcome, Box<WorkerFailure>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Box::new(WorkerFailure {
            code: "forbidden-offline-worker".to_owned(),
            diagnostic: "artifact cache main replay must resolve from the pre-populated cache"
                .to_owned(),
            input_tokens: None,
            cached_input_tokens: None,
            output_tokens: None,
            duration_ms: 0,
            logical_worker_spawns: 1,
            worker_turns: 0,
            repair_performed: false,
            discarded_facts: 0,
            worker_session_id: None,
            session_cleanup_success: Some(true),
        }))
    }
}

#[derive(Debug, Serialize)]
struct ArtifactCacheMainReplayReport {
    schema_id: String,
    mode: String,
    provider_calls: u32,
    simulated_main_turns: u32,
    source_repository: String,
    artifact_root: String,
    cache_replay: ArtifactCacheReplayReport,
    interrupt_acknowledged: bool,
    interrupt_terminal_status: String,
    runtime_status: String,
    runtime_resolution: String,
    cache_hit: bool,
    runtime_worker_spawned: bool,
    worker_executor_calls: u32,
    continuation_rendered: bool,
    main_tool_items_started: u32,
    final_response: String,
    final_response_digest: String,
    source_clean_after: bool,
    cleanup_success: bool,
    passed: bool,
}

pub(super) fn run(arguments: &[String]) -> Result<(), AppError> {
    let source_repository =
        canonical_child_path(Path::new(&required_value(arguments, "--source-repository")?))?;
    let artifact_root =
        absolute_run_path(Path::new(&required_value(arguments, "--artifact-root")?))?;
    if artifact_root.exists() {
        return Err(AppError::Experiment(format!(
            "artifact cache main replay root already exists: {}",
            artifact_root.display()
        )));
    }
    let simulator = resolve_codex(option_value(arguments, "--codex-simulator"))?;
    let cache_replay = run_artifact_cache_replay(&source_repository, &artifact_root)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let (_, snapshot) = capture_git_snapshot(&source_repository)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let target_root = artifact_root.join("main-target");
    let temp_root = artifact_root.join("main-temp");
    let codex_home = artifact_root.join("main-codex-home");
    for directory in [&target_root, &temp_root, &codex_home] {
        fs::create_dir_all(directory)?;
    }

    let store = RuntimeStore::new(artifact_root.join("needle.sqlite3"));
    let profile =
        HookConfig::default().profile().map_err(|error| AppError::Experiment(error.to_string()))?;
    let instructions = profile.rendered_context_owned();
    let main_config = WorkerConfig {
        executable: simulator.display().to_string(),
        model: "simulated-main-r35-cache".to_owned(),
        reasoning: "medium".to_owned(),
        service_tier: Some("default".to_owned()),
        timeout_seconds: 10,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
    };
    let mut session = CodexMainSession::start(MainSessionConfig {
        codex: &main_config,
        codex_home: &codex_home,
        instructions: &instructions,
        checkout_root: &source_repository,
        target_root: &target_root,
        temp_root: &temp_root,
        snapshot_digest: snapshot.source_digest,
        repository_id: snapshot.repository_id,
        route: "locate.implementation",
        store: store.clone(),
    })
    .map_err(AppError::Experiment)?;
    let session_id = session.thread_id().to_owned();
    store
        .record_session_start(
            &session_id,
            profile.definition_digest,
            Some("simulated-main-r35-cache"),
            source_repository.to_str(),
        )
        .map_err(|error| AppError::Experiment(error.to_string()))?;

    let worker_calls = Arc::new(AtomicU32::new(0));
    let execution = (|| -> Result<_, String> {
        let prompt = "Where is --glob-case-insensitive implemented?";
        let need_turn = session.run_until_need(prompt, Duration::from_secs(10))?;
        store
            .record_user_prompt(
                &session_id,
                Some(&need_turn.turn_id),
                prompt,
                source_repository.to_str(),
            )
            .map_err(|error| error.to_string())?;
        let interrupt = need_turn.semantic_interrupt.clone();
        let engine =
            RuntimeEngine::new(store.clone(), ForbiddenWorker { calls: worker_calls.clone() });
        let outcome = engine
            .resolve(&ResolveRequest {
                session_id: session_id.clone(),
                turn_id: need_turn.turn_id.clone(),
                platform: "codex".to_owned(),
                main_model: "simulated-main-r35-cache".to_owned(),
                cwd: source_repository.clone(),
                need: interrupt.compatibility_request(),
                need_ir: interrupt.typed().cloned(),
                declared_test_plan: None,
            })
            .map_err(|error| error.to_string())?;
        let continuation_rendered = outcome.rendered.contains("[NEEDLE_CONTEXT]")
            && outcome.rendered.contains("Continue the original task.")
            && outcome.rendered.contains("crates/core/flags/hiargs.rs")
            && outcome.rendered.contains("GlobCaseInsensitive::update");
        let final_turn = session.run_continuation(&outcome.rendered, Duration::from_secs(10))?;
        Ok((need_turn, outcome, continuation_rendered, final_turn))
    })();

    let simulated_main_turns = session.provider_turns_started();
    let cleanup = session.cleanup();
    let cleanup_success = cleanup.is_ok();
    let end_session = store.end_session(&session_id);
    cleanup.map_err(AppError::Experiment)?;
    end_session.map_err(|error| AppError::Experiment(error.to_string()))?;
    let (need_turn, outcome, continuation_rendered, final_turn) =
        execution.map_err(AppError::Experiment)?;
    let worker_executor_calls = worker_calls.load(Ordering::SeqCst);
    let source_clean_after = repository_status_clean(&source_repository)?;
    let final_response_valid = final_turn.response.contains("crates/core/flags/hiargs.rs")
        && final_turn.response.contains("globs")
        && final_turn.response.contains("crates/core/flags/defs.rs")
        && final_turn.response.contains("GlobCaseInsensitive::update");
    let main_tool_items_started =
        need_turn.tool_items_started.saturating_add(final_turn.tool_items_started);
    let passed = cache_replay.passed
        && need_turn.interrupt_acknowledged
        && need_turn.terminal_status == "interrupted"
        && outcome.status == "hit"
        && matches!(outcome.cache_resolution, CacheResolution::ExactHit { .. })
        && outcome.cache_hit
        && !outcome.worker_spawned
        && worker_executor_calls == 0
        && continuation_rendered
        && main_tool_items_started == 0
        && final_response_valid
        && source_clean_after
        && cleanup_success;
    let report = ArtifactCacheMainReplayReport {
        schema_id: REPORT_SCHEMA.to_owned(),
        mode: "deterministic-offline-scripted-main".to_owned(),
        provider_calls: 0,
        simulated_main_turns,
        source_repository: source_repository.display().to_string(),
        artifact_root: artifact_root.display().to_string(),
        cache_replay,
        interrupt_acknowledged: need_turn.interrupt_acknowledged,
        interrupt_terminal_status: need_turn.terminal_status,
        runtime_status: outcome.status,
        runtime_resolution: resolution_name(&outcome.cache_resolution).to_owned(),
        cache_hit: outcome.cache_hit,
        runtime_worker_spawned: outcome.worker_spawned,
        worker_executor_calls,
        continuation_rendered,
        main_tool_items_started,
        final_response_digest: Digest::blake3(final_turn.response.as_bytes()).to_string(),
        final_response: final_turn.response,
        source_clean_after,
        cleanup_success,
        passed,
    };
    let report_path = artifact_root.join(REPORT_FILE);
    fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    if !report.passed {
        return Err(AppError::Experiment(
            "artifact cache main replay did not satisfy every gate".to_owned(),
        ));
    }
    println!("{}", report_path.display());
    Ok(())
}

fn resolution_name(resolution: &CacheResolution) -> &'static str {
    match resolution {
        CacheResolution::ExactHit { .. } => "ExactHit",
        CacheResolution::CoverageHit { .. } => "CoverageHit",
        CacheResolution::CompositeHit { .. } => "CompositeHit",
        CacheResolution::ClaimHit { .. } => "ClaimHit",
        CacheResolution::ClaimCompositeHit { .. } => "ClaimCompositeHit",
        CacheResolution::PartialHit { .. } => "PartialHit",
        CacheResolution::Miss => "Miss",
        CacheResolution::Stale { .. } => "Stale",
        CacheResolution::Rejected { .. } => "Rejected",
        CacheResolution::Bypass { .. } => "Bypass",
        CacheResolution::Ambiguous { .. } => "Ambiguous",
        CacheResolution::Contradicted { .. } => "Contradicted",
    }
}
