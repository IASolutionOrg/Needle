use needle_bench::{
    ArtifactStore, BootstrapConfig, CachePilotArmObservation, CachePilotResolveOutcome,
    CalibrationObservation, CorpusSchedule, ExperimentArm, ExperimentObservation, ExperimentReport,
    ExperimentSchedule, FinalGateContract, FinalObservation, FrozenCorpusManifest,
    MAX_BOOTSTRAP_RESAMPLES, MAX_CALIBRATION_INPUT_BYTES, MAX_CORPUS_MANIFEST_BYTES,
    MAX_FINAL_OBSERVATION_BYTES, MAX_SCHEDULE_BYTES, MIN_BOOTSTRAP_RESAMPLES, MultiTaskCampaign,
    PilotGateResult, PowerPlan, PricingSnapshot, ProcessExecutionStatus, ProductArm,
    ProductObservation, ProductRunManifest, ProductVerdict, QualityOracleResult, QualityOracleSpec,
    TaskFixture, TokenCost, evaluate_cache_pilot, evaluate_final_gate, evaluate_mutation_pilot,
    evaluate_pilot_pair, parse_codex_jsonl, parse_jsonl, parse_task_fixture, plan_power,
    raw_digest, read_bounded_file, redact_jsonl, validate_frozen_manifest,
};
use needle_core::{
    CodexHost, CodexRole, CommandPolicy, Digest, EvidenceFailurePolicy, FORMAT_REVISION,
    FallbackPolicy, FilesystemPolicy, NeedKey, NeedRequest, NetworkPolicy, ReasoningLevel,
    RepairPolicy, RoleProfileBudget, RoleProfileDefinition, RoleProfileDefinitionInput,
    RoleProfileId, ServiceTier, TestPlan, TestPolicy, ToolPolicy, WorkerConfig,
};
use needle_platform_codex::{
    CodexWorker, CompactInput, HookConfig, SessionEndInput, SessionStartInput, StopInput,
    StopOutput, UserPromptSubmitInput, handle_post_compact, handle_pre_compact, handle_session_end,
    handle_session_start, handle_stop, handle_stop_with_resolver, handle_user_prompt_submit,
    record_compact_telemetry,
};
use needle_runtime::{
    ResolveOutcome, RuntimeSettings, RuntimeStore, capture_git_snapshot, parse_direct_argv,
};
use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

mod artifact_cache_main_replay;
mod codex_hooks;
mod codex_skill;
mod debug;
mod explore;
mod mcp;
mod mcp_live_pilot;
mod minimal_live_pilot;
mod onboarding;
mod partial_tests_live;
mod product_resolver;
mod uninstall;
mod worker_live_diagnostic;

const VERSION: &str = "0.1.0";
const BENCHMARK_REPOSITORY_URL: &str = "https://github.com/BurntSushi/ripgrep.git";
const BENCHMARK_REPOSITORY_SHA: &str = "4649aa9700619f94cf9c66876e9549d83420e16c";
const BENCHMARK_TASK_FIXTURE: &str = "fixtures/ripgrep-14.1.1-task.json";

#[derive(Debug, Error)]
enum AppError {
    #[error("usage: {0}")]
    Usage(String),
    #[error("I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hook failed: {0}")]
    Hook(#[from] needle_platform_codex::HookError),
    #[error("plugin validation failed: {0}")]
    Plugin(String),
    #[error("experiment failed: {0}")]
    Experiment(String),
    #[error("runtime failed: {0}")]
    Runtime(String),
}

fn main() {
    if let Err(error) = run() {
        eprintln!("needle: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), AppError> {
    let mut arguments = env::args().skip(1);
    let Some(command) = arguments.next() else {
        print_usage();
        return Ok(());
    };
    match command.as_str() {
        "enable" => onboarding::run_enable(arguments.collect()),
        "disable" => onboarding::run_disable(arguments.collect()),
        "status" => onboarding::run_status(arguments.collect()),
        "debug" => debug::run(arguments.collect()),
        "explore" => explore::run(arguments.collect()),
        "ui" => onboarding::run_ui(arguments.collect()),
        "uninstall" => uninstall::run(arguments.collect()),
        "init" => run_init(arguments.collect()),
        "config" => run_config(arguments.collect()),
        "route" => run_route(arguments.collect()),
        "cache" => run_cache(arguments.collect()),
        "doctor" => run_doctor(arguments.collect()),
        "serve" => run_server(arguments.collect()),
        "worker" => run_worker_utility(arguments.collect()),
        "hook" => run_hook(arguments.collect()),
        "mcp" => run_mcp(arguments.collect()),
        "experiment" => run_experiment(arguments.collect()),
        "plugin" => run_plugin(arguments.collect()),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        "--version" | "-V" => {
            println!("needle {VERSION}");
            Ok(())
        }
        _ => Err(AppError::Usage(format!("unknown subcommand `{command}`"))),
    }
}

fn print_usage() {
    println!(
        "Needle {VERSION}\n\nUsage: needle <command> [options]\n\nCompanion commands:\n  enable      Enable Needle for this repository\n  disable     Disable Needle without deleting data\n  status      Show effective activation and compatibility\n  debug       Manage bounded local worker diagnostics\n  explore     Request bounded repository context\n  ui          Open the local control plane\n  uninstall   Remove the managed Windows installation\n\nRun `needle <command> --help` for command options."
    );
}

fn run_server(arguments: Vec<String>) -> Result<(), AppError> {
    validate_server_arguments(&arguments)?;
    let data_directory = product_data_directory(&arguments)?;
    let repository_root =
        option_value(&arguments, "--repository").map(PathBuf::from).unwrap_or(env::current_dir()?);
    server::run(data_directory, repository_root, false).map_err(AppError::Runtime)
}

fn validate_server_arguments(arguments: &[String]) -> Result<(), AppError> {
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--data-dir" | "--repository" => {
                if arguments.get(index + 1).is_none() {
                    return Err(AppError::Usage(format!("{} requires a value", arguments[index])));
                }
                index += 2;
            }
            argument => {
                return Err(AppError::Usage(format!("unknown serve argument `{argument}`")));
            }
        }
    }
    Ok(())
}

fn run_worker_utility(arguments: Vec<String>) -> Result<(), AppError> {
    if arguments.first().map(String::as_str) != Some("digest-files")
        || arguments.get(1).map(String::as_str) != Some("--")
        || arguments.len() < 3
    {
        return Err(AppError::Usage(
            "worker digest-files -- <repository-relative-path> [...]".to_owned(),
        ));
    }
    let root = fs::canonicalize(env::current_dir()?)?;
    let mut output = serde_json::Map::new();
    for value in &arguments[2..] {
        let path = Path::new(value);
        if path.is_absolute()
            || value.contains('\\')
            || value.is_empty()
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(AppError::Usage(format!("unsafe repository-relative path `{value}`")));
        }
        let canonical = fs::canonicalize(root.join(path))?;
        if !canonical.starts_with(&root) || !canonical.is_file() {
            return Err(AppError::Usage(format!("path is outside the repository: `{value}`")));
        }
        output
            .insert(value.clone(), Value::String(Digest::blake3(fs::read(canonical)?).to_string()));
    }
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn run_init(arguments: Vec<String>) -> Result<(), AppError> {
    let worker_model = required_value(&arguments, "--worker-model")?;
    let worker_reasoning = required_value(&arguments, "--worker-reasoning")?;
    validate_model_value(&worker_model, "worker model")?;
    validate_reasoning(&worker_reasoning)?;
    let codex_executable =
        option_value(&arguments, "--codex").unwrap_or_else(|| "codex".to_owned());
    let worker_timeout_seconds = option_value(&arguments, "--worker-timeout-seconds")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid worker timeout: {error}")))?
        .unwrap_or(180);
    if worker_timeout_seconds == 0 || worker_timeout_seconds > 180 {
        return Err(AppError::Usage(
            "--worker-timeout-seconds must be between 1 and 180".to_owned(),
        ));
    }
    let evidence_failure_policy = option_value(&arguments, "--evidence-failure-policy")
        .map(|value| parse_evidence_failure_policy(&value))
        .transpose()?
        .unwrap_or_default();
    let trusted_test_execution =
        arguments.iter().any(|argument| argument == "--trust-test-execution");
    let isolation = CodexWorker::verify_isolation(&codex_executable).map_err(AppError::Runtime)?;
    if !isolation.verified() {
        return Err(AppError::Runtime(format!(
            "Codex {} is not in the exact validated set or lacks required isolation flags",
            isolation.codex_version
        )));
    }
    let data_directory = product_data_directory(&arguments)?;
    let store = RuntimeStore::new(data_directory.join("needle.sqlite3"));
    store
        .initialize_defaults(&RuntimeSettings {
            codex_executable,
            worker_model,
            worker_reasoning,
            worker_timeout_seconds,
            evidence_failure_policy,
            trusted_test_execution,
            multi_need_policy: needle_core::MultiNeedPolicy::default(),
        })
        .map_err(|error| AppError::Runtime(error.to_string()))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "initialized",
            "database": store.path(),
            "codex_version": isolation.codex_version,
            "scope": "snapshot_exact",
        }))?
    );
    Ok(())
}

fn run_config(arguments: Vec<String>) -> Result<(), AppError> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err(AppError::Usage("config import <file>|export [--output <file>]".to_owned()));
    };
    let store = product_store(&arguments)?;
    match action {
        "export" => {
            let rendered =
                store.export_toml().map_err(|error| AppError::Runtime(error.to_string()))?;
            if let Some(path) = option_value(&arguments, "--output") {
                fs::write(path, rendered)?;
            } else {
                print!("{rendered}");
            }
        }
        "import" => {
            let path = arguments.get(1).ok_or_else(|| {
                AppError::Usage("config import <file> [--data-dir <directory>]".to_owned())
            })?;
            let input = fs::read_to_string(path)?;
            store.import_toml(&input).map_err(|error| AppError::Runtime(error.to_string()))?;
            println!("configuration imported");
        }
        _ => {
            return Err(AppError::Usage(
                "config import <file>|export [--output <file>]".to_owned(),
            ));
        }
    }
    Ok(())
}

fn run_route(arguments: Vec<String>) -> Result<(), AppError> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err(AppError::Usage("route list|show <id>|enable <id>|disable <id>".to_owned()));
    };
    let store = product_store(&arguments)?;
    let routes = store.routes().map_err(|error| AppError::Runtime(error.to_string()))?;
    match action {
        "list" => println!("{}", serde_json::to_string_pretty(&routes)?),
        "show" => {
            let id =
                arguments.get(1).ok_or_else(|| AppError::Usage("route show <id>".to_owned()))?;
            let route = routes
                .iter()
                .find(|route| &route.id == id)
                .ok_or_else(|| AppError::Runtime(format!("route `{id}` was not found")))?;
            println!("{}", serde_json::to_string_pretty(route)?);
        }
        "enable" | "disable" => {
            let id =
                arguments.get(1).ok_or_else(|| AppError::Usage(format!("route {action} <id>")))?;
            let changed = store
                .set_route_enabled(id, action == "enable")
                .map_err(|error| AppError::Runtime(error.to_string()))?;
            if !changed {
                return Err(AppError::Runtime(format!("route `{id}` was not found")));
            }
            println!("route `{id}` {action}d");
        }
        _ => {
            return Err(AppError::Usage(
                "route list|show <id>|enable <id>|disable <id>".to_owned(),
            ));
        }
    }
    Ok(())
}

fn run_cache(arguments: Vec<String>) -> Result<(), AppError> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err(AppError::Usage(
            "cache list|show <digest>|latest-run|invalidate <digest|--all>".to_owned(),
        ));
    };
    let store = product_store(&arguments)?;
    match action {
        "list" => {
            let records =
                store.cache_records().map_err(|error| AppError::Runtime(error.to_string()))?;
            let values = records
                .into_iter()
                .map(|record| {
                    json!({
                        "identity_digest": record.identity_digest,
                        "logical_digest": record.logical_digest,
                        "source_digest": record.source_digest,
                        "created_unix_ms": record.created_unix_ms,
                        "hit_count": record.hit_count,
                    })
                })
                .collect::<Vec<_>>();
            println!("{}", serde_json::to_string_pretty(&values)?);
        }
        "show" => {
            let digest = parse_cli_digest(arguments.get(1), "cache show <digest>")?;
            let entry = store
                .cache_entry(digest)
                .map_err(|error| AppError::Runtime(error.to_string()))?
                .ok_or_else(|| AppError::Runtime("cache entry was not found".to_owned()))?;
            println!("{}", serde_json::to_string_pretty(&entry)?);
        }
        "latest-run" => {
            let run = store
                .latest_worker_run()
                .map_err(|error| AppError::Runtime(error.to_string()))?
                .ok_or_else(|| AppError::Runtime("worker run was not found".to_owned()))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "input_tokens": run.input_tokens,
                    "cached_input_tokens": run.cached_input_tokens,
                    "output_tokens": run.output_tokens,
                    "result_digest": run.result_digest,
                    "failure_code": run.failure_code,
                    "failure_diagnostic": run.failure_diagnostic,
                    "discarded_facts": run.discarded_facts,
                    "logical_worker_spawns": run.logical_worker_spawns,
                    "worker_turns": run.worker_turns,
                    "repair_performed": run.repair_performed,
                    "worker_session_id": run.worker_session_id,
                    "session_cleanup_success": run.session_cleanup_success,
                }))?
            );
        }
        "invalidate" if arguments.iter().any(|argument| argument == "--all") => {
            let count = store
                .invalidate_all_cache()
                .map_err(|error| AppError::Runtime(error.to_string()))?;
            println!("invalidated {count} cache entries");
        }
        "invalidate" => {
            let digest = parse_cli_digest(arguments.get(1), "cache invalidate <digest>")?;
            if !store
                .invalidate_cache(digest)
                .map_err(|error| AppError::Runtime(error.to_string()))?
            {
                return Err(AppError::Runtime("cache entry was not found".to_owned()));
            }
            println!("cache entry invalidated");
        }
        _ => {
            return Err(AppError::Usage(
                "cache list|show <digest>|latest-run|invalidate <digest|--all>".to_owned(),
            ));
        }
    }
    Ok(())
}

fn run_doctor(arguments: Vec<String>) -> Result<(), AppError> {
    let data_directory = product_data_directory(&arguments)?;
    let store = RuntimeStore::new(data_directory.join("needle.sqlite3"));
    store.initialize().map_err(|error| AppError::Runtime(error.to_string()))?;
    let settings = store.settings();
    let utility_gate_passed = store.utility_gate_passed().unwrap_or(false);
    let pending_worker_sessions =
        store.pending_worker_sessions().map(|sessions| sessions.len()).unwrap_or(0);
    let (database, isolation) = match settings {
        Ok(settings) => {
            let isolation = CodexWorker::verify_isolation(&settings.codex_executable)
                .map_err(AppError::Runtime)?;
            ("ready", Some(isolation))
        }
        Err(_) => ("not_initialized", None),
    };
    let snapshot = env::current_dir()
        .ok()
        .and_then(|cwd| capture_git_snapshot(&cwd).ok().map(|(_, snapshot)| snapshot));
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "database": database,
            "database_path": store.path(),
            "codex": isolation.as_ref().map(|value| json!({
                "version": value.codex_version,
                "supported": value.supported,
                "required_flags_present": value.required_flags_present,
                "isolation_verified": value.verified(),
            })),
            "repository_snapshot": snapshot,
            "product_scope": "snapshot_exact",
            "runtime_mode": "on_demand",
            "utility_gate_passed": utility_gate_passed,
            "cache_active": utility_gate_passed,
            "pending_worker_sessions": pending_worker_sessions,
        }))?
    );
    Ok(())
}

fn product_store(arguments: &[String]) -> Result<RuntimeStore, AppError> {
    let store = RuntimeStore::new(product_data_directory(arguments)?.join("needle.sqlite3"));
    store.initialize().map_err(|error| AppError::Runtime(error.to_string()))?;
    Ok(store)
}

fn hook_runtime_store() -> Result<RuntimeStore, AppError> {
    Ok(RuntimeStore::new(product_data_directory(&[])?.join("needle.sqlite3")))
}

#[derive(Clone)]
struct ActiveHookContext {
    store: RuntimeStore,
    role_profile_id: RoleProfileId,
}

fn active_hook_context(cwd: Option<&str>) -> Option<ActiveHookContext> {
    let cwd = cwd.map(Path::new)?;
    let store = match hook_runtime_store() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("needle: activation state is unavailable ({error}); fail-open");
            return None;
        }
    };
    if !store.path().is_file() {
        return None;
    }
    let (repository_root, _) = match capture_git_snapshot(cwd) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            eprintln!("needle: repository activation cannot be resolved ({error}); fail-open");
            return None;
        }
    };
    match store.activation_status(&repository_root) {
        Ok(status) if status.enabled => match status.role_profile_id {
            Some(role_profile_id) => Some(ActiveHookContext { store, role_profile_id }),
            None => {
                eprintln!("needle: enabled activation has no role profile; fail-open");
                None
            }
        },
        Ok(_) => None,
        Err(error) => {
            eprintln!("needle: activation state cannot be read ({error}); fail-open");
            None
        }
    }
}

fn profiled_hook_session(session_id: Option<&str>) -> Option<RuntimeStore> {
    let session_id = session_id?;
    let store = match hook_runtime_store() {
        Ok(store) => store,
        Err(error) => {
            eprintln!("needle: session state is unavailable ({error}); fail-open");
            return None;
        }
    };
    if !store.path().is_file() {
        return None;
    }
    match store.session(session_id) {
        Ok(Some(session)) if session.role_profile_provenance.is_some() => Some(store),
        Ok(_) => None,
        Err(error) => {
            eprintln!("needle: session activation cannot be read ({error}); fail-open");
            None
        }
    }
}

fn product_data_directory(arguments: &[String]) -> Result<PathBuf, AppError> {
    if let Some(value) = option_value(arguments, "--data-dir") {
        return Ok(PathBuf::from(value));
    }
    if let Some(value) = env::var_os("NEEDLE_DATA_DIR") {
        return Ok(PathBuf::from(value));
    }
    if let Some(value) = env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(value).join("Needle"));
    }
    if let Some(value) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(value).join("needle"));
    }
    if let Some(value) = env::var_os("HOME") {
        return Ok(PathBuf::from(value).join(".local/share/needle"));
    }
    Err(AppError::Runtime("no product data directory is available; pass --data-dir".to_owned()))
}

fn canonical_child_path(path: &Path) -> Result<PathBuf, AppError> {
    let canonical = fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        let value = canonical.to_string_lossy();
        if let Some(value) = value.strip_prefix(r"\\?\UNC\") {
            return Ok(PathBuf::from(format!(r"\\{value}")));
        }
        if let Some(value) = value.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(value));
        }
    }
    Ok(canonical)
}

fn absolute_run_path(path: &Path) -> Result<PathBuf, AppError> {
    Ok(std::path::absolute(path)?)
}

fn parse_cli_digest(value: Option<&String>, usage: &str) -> Result<Digest, AppError> {
    let value = value.ok_or_else(|| AppError::Usage(usage.to_owned()))?;
    Digest::parse(value).map_err(|error| AppError::Usage(format!("invalid digest: {error}")))
}

fn read_stdin() -> Result<String, AppError> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    Ok(input)
}

fn write_pilot_signal(kind: &str, detail: &str) {
    let Some(path) = env::var_os("NEEDLE_PILOT_SIGNAL_FILE") else {
        return;
    };
    let mut end = detail.len().min(4096);
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    let signal = json!({"kind": kind, "detail": &detail[..end]});
    if let Ok(rendered) = serde_json::to_vec(&signal) {
        let _ = fs::write(path, rendered);
    }
}

fn write_pilot_outcome(outcome: &ResolveOutcome) {
    let Some(path) = env::var_os("NEEDLE_PILOT_OUTCOME_FILE") else {
        return;
    };
    let value = CachePilotResolveOutcome {
        status: outcome.status.clone(),
        cache_resolution: outcome.cache_resolution.clone(),
        cache_hit: outcome.cache_hit,
        worker_spawned: outcome.worker_spawned,
        result_digest: outcome.result_digest,
    };
    if let Ok(rendered) = serde_json::to_vec(&value) {
        let _ = fs::write(path, rendered);
    }
}

fn run_hook(arguments: Vec<String>) -> Result<(), AppError> {
    let Some(event) = arguments.first().map(String::as_str) else {
        return Err(AppError::Usage(
            "hook session-start|user-prompt-submit|stop|session-end|pre-compact|post-compact"
                .to_owned(),
        ));
    };
    let input = read_stdin()?;
    let mut config = HookConfig::from_environment();
    if config.experiment_arm.is_none() && config.plugin_data.is_none() {
        config.plugin_data = product_data_directory(&[]).ok().map(|path| path.join("hook-state"));
    }
    let invalid_input = |error: serde_json::Error| -> Result<(), AppError> {
        eprintln!("needle: invalid hook stdin for {event}: {error}; fail-open");
        println!("{{}}");
        Ok(())
    };
    let output = match event {
        "session-start" => {
            let parsed: SessionStartInput = match serde_json::from_str(&input) {
                Ok(parsed) => parsed,
                Err(error) => return invalid_input(error),
            };
            let activation = (config.experiment_arm.is_none())
                .then(|| active_hook_context(parsed.cwd.as_deref()))
                .flatten();
            if config.experiment_arm.is_none() && activation.is_none() {
                json!({})
            } else {
                let output = handle_session_start(&parsed, &config)?;
                if let Some(session_id) = parsed.session_id.as_deref() {
                    let store = activation
                        .as_ref()
                        .map(|context| context.store.clone())
                        .unwrap_or(hook_runtime_store()?);
                    let profile_digest = config.profile()?.definition_digest;
                    let selector = activation
                        .as_ref()
                        .map(|context| context.role_profile_id.clone())
                        .or_else(|| {
                            env::var("NEEDLE_ROLE_PROFILE_ID")
                                .ok()
                                .and_then(|value| RoleProfileId::new(value).ok())
                        });
                    let result = match selector {
                        Some(profile_id) => store.initialize().and_then(|_| {
                            store.record_session_start_profiled(
                                session_id,
                                profile_digest,
                                parsed.model.as_deref(),
                                parsed.cwd.as_deref(),
                                &profile_id,
                            )
                        }),
                        None => {
                            eprintln!(
                                "needle: NEEDLE_ROLE_PROFILE_ID is missing; session provenance is unknown"
                            );
                            Ok(())
                        }
                    };
                    if let Err(error) = result {
                        eprintln!(
                            "needle: cannot record profiled product session ({error}); fail-open"
                        );
                    }
                }
                serde_json::to_value(output)?
            }
        }
        "user-prompt-submit" => {
            let parsed: UserPromptSubmitInput = match serde_json::from_str(&input) {
                Ok(parsed) => parsed,
                Err(error) => return invalid_input(error),
            };
            let session_store = (config.experiment_arm.is_none())
                .then(|| profiled_hook_session(parsed.session_id.as_deref()))
                .flatten();
            if config.experiment_arm.is_none() && session_store.is_none() {
                json!({})
            } else {
                if let (Some(session_id), Some(prompt)) =
                    (parsed.session_id.as_deref(), parsed.prompt.as_deref())
                {
                    let store = session_store.unwrap_or(hook_runtime_store()?);
                    let root_prompt = env::var_os("NEEDLE_PILOT_ROOT_TASK_FILE")
                        .and_then(|path| fs::read_to_string(path).ok())
                        .unwrap_or_else(|| prompt.to_owned());
                    if let Err(error) = store.record_user_prompt(
                        session_id,
                        parsed.turn_id.as_deref(),
                        &root_prompt,
                        parsed.cwd.as_deref(),
                    ) {
                        eprintln!("needle: cannot record root task ({error}); fail-open");
                    }
                }
                serde_json::to_value(handle_user_prompt_submit(&parsed, &config))?
            }
        }
        "stop" => {
            let parsed: StopInput = match serde_json::from_str(&input) {
                Ok(parsed) => parsed,
                Err(error) => return invalid_input(error),
            };
            let output = if config.experiment_arm.is_some() {
                handle_stop(&parsed, &config)?
            } else if profiled_hook_session(parsed.session_id.as_deref()).is_none() {
                StopOutput::noop()
            } else {
                handle_stop_with_resolver(&parsed, &config, |need| {
                    let session_id = parsed
                        .session_id
                        .as_deref()
                        .ok_or_else(|| "session id is unavailable".to_owned())?;
                    let turn_id = parsed
                        .turn_id
                        .as_deref()
                        .ok_or_else(|| "turn id is unavailable".to_owned())?;
                    let cwd = parsed
                        .cwd
                        .as_deref()
                        .map(PathBuf::from)
                        .ok_or_else(|| "repository cwd is unavailable".to_owned())?;
                    let model = parsed.model.clone().unwrap_or_else(|| "unknown".to_owned());
                    let data_directory =
                        product_data_directory(&[]).map_err(|error| error.to_string())?;
                    let explicit_test_plan = env::var_os("NEEDLE_PILOT_TEST_PLAN_FILE")
                        .map(fs::read)
                        .transpose()
                        .map_err(|error| format!("cannot read pilot test plan: {error}"))?
                        .map(|bytes| {
                            serde_json::from_slice::<TestPlan>(&bytes)
                                .map_err(|error| format!("invalid pilot test plan: {error}"))
                        })
                        .transpose()?;
                    let pilot_oracle = env::var_os("NEEDLE_PILOT_ORACLE_FILE")
                        .map(fs::read)
                        .transpose()
                        .map_err(|error| format!("cannot read pilot oracle: {error}"))?
                        .map(|bytes| {
                            serde_json::from_slice::<QualityOracleSpec>(&bytes)
                                .map_err(|error| format!("invalid pilot oracle: {error}"))
                        })
                        .transpose()?;
                    let declared_test_plan = match explicit_test_plan {
                        Some(plan) => Some(plan),
                        None => pilot_oracle.as_ref().map(pilot_test_plan).transpose()?,
                    };
                    let compatibility_need = need.compatibility_request();
                    let resolve_request = needle_runtime::ResolveRequest {
                        session_id: session_id.to_owned(),
                        turn_id: turn_id.to_owned(),
                        platform: "codex".to_owned(),
                        main_model: model,
                        cwd,
                        need: compatibility_need,
                        need_ir: need.typed().cloned(),
                        declared_test_plan,
                    };
                    let worker_policy =
                        if env::var("NEEDLE_RESOLVE_CACHE_ONLY").as_deref() == Ok("1") {
                            product_resolver::WorkerPolicy::CacheOnly
                        } else {
                            product_resolver::WorkerPolicy::Allow
                        };
                    let resolver =
                        product_resolver::ProductResolver::new(data_directory, worker_policy)?;
                    let outcome = match resolver.resolve(&resolve_request) {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            write_pilot_signal("bypass", &error);
                            return Err(error);
                        }
                    };
                    write_pilot_outcome(&outcome);
                    Ok(Some(outcome.rendered))
                })?
            };
            serde_json::to_value(output)?
        }
        "session-end" => {
            let parsed: SessionEndInput = match serde_json::from_str(&input) {
                Ok(parsed) => parsed,
                Err(error) => return invalid_input(error),
            };
            let session_store = (config.experiment_arm.is_none())
                .then(|| profiled_hook_session(parsed.session_id.as_deref()))
                .flatten();
            if config.experiment_arm.is_none() && session_store.is_none() {
                json!({})
            } else {
                if let Some(session_id) = parsed.session_id.as_deref() {
                    let store = session_store.unwrap_or(hook_runtime_store()?);
                    if let Err(error) = store.end_session(session_id) {
                        eprintln!("needle: cannot clean product session ({error})");
                    }
                }
                serde_json::to_value(handle_session_end(&parsed, &config))?
            }
        }
        "pre-compact" => {
            let parsed: CompactInput = match serde_json::from_str(&input) {
                Ok(parsed) => parsed,
                Err(error) => return invalid_input(error),
            };
            if config.experiment_arm.is_none()
                && profiled_hook_session(parsed.session_id.as_deref()).is_none()
            {
                json!({})
            } else {
                let output = handle_pre_compact(&parsed);
                record_compact_telemetry(&parsed, "PreCompact", &config);
                serde_json::to_value(output)?
            }
        }
        "post-compact" => {
            let parsed: CompactInput = match serde_json::from_str(&input) {
                Ok(parsed) => parsed,
                Err(error) => return invalid_input(error),
            };
            if config.experiment_arm.is_none()
                && profiled_hook_session(parsed.session_id.as_deref()).is_none()
            {
                json!({})
            } else {
                let output = handle_post_compact(&parsed);
                record_compact_telemetry(&parsed, "PostCompact", &config);
                serde_json::to_value(output)?
            }
        }
        _ => return Err(AppError::Usage(format!("unknown hook event `{event}`"))),
    };
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &output)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn run_mcp(arguments: Vec<String>) -> Result<(), AppError> {
    if arguments.first().map(String::as_str) == Some("serve") {
        validate_mcp_serve_arguments(&arguments)?;
        let data_directory = product_data_directory(&arguments)?;
        let repository_root = option_value(&arguments, "--repository")
            .map(PathBuf::from)
            .or_else(|| env::var_os("NEEDLE_MCP_REPOSITORY_ROOT").map(PathBuf::from))
            .unwrap_or(env::current_dir()?);
        let main_model = option_value(&arguments, "--main-model")
            .or_else(|| env::var("NEEDLE_MCP_MAIN_MODEL").ok())
            .unwrap_or_else(|| "unknown".to_owned());
        validate_model_value(&main_model, "main model")?;
        let role_profile = required_value(&arguments, "--role-profile").and_then(|value| {
            RoleProfileId::new(value)
                .map_err(|error| AppError::Usage(format!("invalid role profile: {error}")))
        })?;
        return mcp::serve(mcp::ProductMcpConfig {
            data_directory,
            repository_root,
            main_model,
            cache_only: arguments.iter().any(|argument| argument == "--cache-only"),
            calibration_reuse: env::var("NEEDLE_INTERNAL_CALIBRATION_REUSE").as_deref()
                == Ok("partial-tests-live"),
            role_profile_id: role_profile,
        })
        .map_err(AppError::Runtime);
    }
    if arguments.first().map(String::as_str) != Some("serve-benchmark") || arguments.len() != 1 {
        return Err(AppError::Usage(
            "mcp serve --role-profile <id> [--data-dir <directory>] [--repository <root>] [--main-model <model>] [--cache-only] | mcp serve-benchmark"
                .to_owned(),
        ));
    }
    mcp::serve_benchmark().map_err(AppError::Runtime)
}

fn validate_mcp_serve_arguments(arguments: &[String]) -> Result<(), AppError> {
    let mut index = 1;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--cache-only" => index += 1,
            "--data-dir" | "--repository" | "--main-model" | "--role-profile" => {
                if arguments.get(index + 1).is_none() {
                    return Err(AppError::Usage(format!("{} requires a value", arguments[index])));
                }
                index += 2;
            }
            argument => {
                return Err(AppError::Usage(format!("unknown mcp serve argument `{argument}`")));
            }
        }
    }
    Ok(())
}

fn run_experiment(arguments: Vec<String>) -> Result<(), AppError> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err(AppError::Usage("experiment run|report".to_owned()));
    };
    match action {
        "run" => experiment_run(&arguments[1..]),
        "pilot" => product_pilot_run(&arguments[1..]),
        "transport-preflight" => transport_preflight_run(&arguments[1..]),
        "mcp-live" => mcp_live_pilot::run(&arguments[1..]),
        "partial-tests-live" => partial_tests_live::run(&arguments[1..]),
        "mcp-contract-microbench" => mcp_contract_microbench_run(&arguments[1..]),
        "minimal-pilot-live" => minimal_live_pilot::run(&arguments[1..]),
        "quality-oracle-replay" => quality_oracle_replay(&arguments[1..]),
        "artifact-cache-main-replay" => artifact_cache_main_replay::run(&arguments[1..]),
        "worker-diagnostic-live" => worker_live_diagnostic::run(&arguments[1..]),
        "cache-pilot" => product_cache_pilot_run(&arguments[1..]),
        "cache-pilot-report" => cache_pilot_report(&arguments[1..]),
        "product-report" => product_report(&arguments[1..]),
        "power-plan" => power_plan_report(&arguments[1..]),
        "final-report" => final_gate_report(&arguments[1..]),
        "report" => experiment_report(&arguments[1..]),
        _ => Err(AppError::Usage(
            "experiment run|transport-preflight|mcp-live|partial-tests-live|mcp-contract-microbench|minimal-pilot-live|quality-oracle-replay|artifact-cache-main-replay|worker-diagnostic-live|pilot|cache-pilot|cache-pilot-report|report|product-report|power-plan|final-report"
                .to_owned(),
        )),
    }
}

fn mcp_contract_microbench_run(arguments: &[String]) -> Result<(), AppError> {
    let request = option_value(arguments, "--mcp-request-file")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("benchmarks/fixtures/mcp-request.json"));
    let iterations = option_value(arguments, "--iterations")
        .map(|value| value.parse::<u32>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid --iterations: {error}")))?
        .unwrap_or(10_000);
    let report = mcp::contract_microbench(&request, iterations).map_err(AppError::Experiment)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn quality_oracle_replay(arguments: &[String]) -> Result<(), AppError> {
    let manifest = option_value(arguments, "--manifest").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(minimal_live_pilot::protocol::DEFAULT_LEGACY_OFFLINE_MANIFEST)
    });
    let task_id = required_value(arguments, "--task-id")?;
    let response_path = PathBuf::from(required_value(arguments, "--response")?);
    let response = fs::read_to_string(&response_path)?;
    let protocol = minimal_live_pilot::protocol::load_legacy_offline_protocol(&manifest)?;
    let (task, oracle) = protocol.campaign_task(&task_id)?;
    let spec = minimal_live_pilot::protocol::quality_spec_for_task(task, oracle)?;
    let quality = needle_bench::QualityOracleResult::evaluate(&spec, &response, None);
    let report = json!({
        "schema": "needle.quality-oracle-replay/1",
        "task_id": task_id,
        "response_path": response_path,
        "response_digest": Digest::blake3(response.as_bytes()),
        "quality": quality,
    });
    if let Some(output) = option_value(arguments, "--output") {
        let output = PathBuf::from(output);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn transport_preflight_run(arguments: &[String]) -> Result<(), AppError> {
    let codex = resolve_codex(option_value(arguments, "--codex"))?;
    let repository =
        canonical_child_path(Path::new(&required_value(arguments, "--source-repository")?))?;
    let data_root = absolute_run_path(Path::new(&required_value(arguments, "--data-root")?))?;
    if data_root.exists() {
        return Err(AppError::Experiment(format!(
            "transport preflight data root already exists: {}",
            data_root.display()
        )));
    }
    let model = required_value(arguments, "--model")?;
    let reasoning = required_value(arguments, "--reasoning")?;
    let service_tier = required_value(arguments, "--service-tier")?;
    validate_model_value(&model, "worker model")?;
    validate_reasoning(&reasoning)?;
    validate_service_tier(&service_tier)?;
    fs::create_dir_all(&data_root)?;
    let config = WorkerConfig {
        executable: codex.display().to_string(),
        model,
        reasoning,
        service_tier: Some(service_tier),
        timeout_seconds: 30,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    let report = CodexWorker::new(&data_root)
        .preflight_transport(&config, &repository)
        .map_err(AppError::Runtime)?;
    let output = option_value(arguments, "--output")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_root.join("transport-preflight-report.json"));
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    eprintln!("transport preflight report written to {}", output.display());
    Ok(())
}

fn experiment_run(arguments: &[String]) -> Result<(), AppError> {
    let dry_run =
        arguments.iter().any(|argument| argument == "--dry-run" || argument == "--offline");
    let schedule_seed = option_value(arguments, "--schedule-seed")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid --schedule-seed: {error}")))?
        .unwrap_or(0);
    let mut schedule = ExperimentSchedule::single(schedule_seed);
    if let Some(value) = option_value(arguments, "--only-arm") {
        let arm = parse_experiment_arm(&value)?;
        schedule.entries.retain(|entry| entry.arm == arm);
    }
    if dry_run {
        let output = serde_json::to_string_pretty(&json!({
            "repository": {
                "url": BENCHMARK_REPOSITORY_URL,
                "sha": BENCHMARK_REPOSITORY_SHA,
            },
            "task_fixture": BENCHMARK_TASK_FIXTURE,
            "observations_per_arm": 1,
            "schedule": schedule,
        }))?;
        if let Some(path) = option_value(arguments, "--output") {
            fs::write(path, output.as_bytes())?;
        } else {
            println!("{output}");
        }
        return Ok(());
    }
    run_live_experiment(arguments, schedule)
}

fn product_pilot_run(arguments: &[String]) -> Result<(), AppError> {
    let codex = resolve_codex(option_value(arguments, "--codex"))?;
    let codex_home = PathBuf::from(required_value(arguments, "--codex-home")?);
    ensure_dedicated_codex_home(&codex_home)?;
    let codex_home = canonical_child_path(&codex_home)?;
    ensure_product_pilot_hook_isolation(&codex_home)?;
    ensure_cache_pilot_hook_binary(&codex_home)?;
    let main_model = required_value(arguments, "--main-model")?;
    let main_reasoning = required_value(arguments, "--main-reasoning")?;
    let worker_model = required_value(arguments, "--worker-model")?;
    let worker_reasoning = required_value(arguments, "--worker-reasoning")?;
    let service_tier = required_value(arguments, "--service-tier")?;
    let pricing_snapshot_path = PathBuf::from(required_value(arguments, "--pricing-snapshot")?);
    let baseline_profile = required_value(arguments, "--baseline-profile")?;
    let product_profile = required_value(arguments, "--product-profile")?;
    let artifact_root = PathBuf::from(required_value(arguments, "--artifact-root")?);
    let preflight_only = arguments.iter().any(|argument| argument == "--preflight-only");
    let only_n1 = match option_value(arguments, "--only-arm").as_deref() {
        None => false,
        Some("N1") => true,
        Some(value) => {
            return Err(AppError::Usage(format!(
                "product pilot --only-arm accepts only N1, got {value}"
            )));
        }
    };
    let reuse_p0_root = option_value(arguments, "--reuse-p0").map(PathBuf::from);
    if only_n1 != reuse_p0_root.is_some() {
        return Err(AppError::Usage(
            "product pilot --only-arm N1 requires --reuse-p0 <prior-pilot-root>".to_owned(),
        ));
    }
    let schedule_seed = option_value(arguments, "--schedule-seed")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid --schedule-seed: {error}")))?
        .unwrap_or(42);
    let timeout_seconds = option_value(arguments, "--timeout-seconds")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid --timeout-seconds: {error}")))?
        .unwrap_or(240);
    if timeout_seconds < 180 {
        return Err(AppError::Usage(
            "pilot --timeout-seconds must be at least the 180 second worker timeout".to_owned(),
        ));
    }
    for (value, label) in [(&main_model, "main model"), (&worker_model, "worker model")] {
        validate_model_value(value, label)?;
    }
    validate_reasoning(&main_reasoning)?;
    validate_reasoning(&worker_reasoning)?;
    validate_service_tier(&service_tier)?;
    validate_slug(&baseline_profile, "baseline profile")?;
    validate_slug(&product_profile, "product profile")?;
    let pricing_snapshot: PricingSnapshot =
        serde_json::from_slice(&fs::read(&pricing_snapshot_path)?).map_err(|error| {
            AppError::Experiment(format!(
                "invalid pricing snapshot {}: {error}",
                pricing_snapshot_path.display()
            ))
        })?;
    pricing_snapshot.validate().map_err(|error| AppError::Experiment(error.to_string()))?;
    let pricing_snapshot_digest = pricing_snapshot.digest()?;
    let codex_executable = codex.display().to_string();
    let isolation = CodexWorker::verify_isolation(&codex_executable).map_err(AppError::Runtime)?;
    if !isolation.verified() {
        return Err(AppError::Experiment(format!(
            "worker isolation is not verified for Codex {}",
            isolation.codex_version
        )));
    }
    if artifact_root.exists() {
        return Err(AppError::Experiment(format!(
            "pilot artifact root already exists: {}",
            artifact_root.display()
        )));
    }
    let task_path = option_value(arguments, "--tasks")
        .map(PathBuf::from)
        .unwrap_or_else(default_task_fixture_path);
    let tasks = parse_task_fixture(&fs::read_to_string(task_path)?)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let task = tasks
        .first()
        .ok_or_else(|| AppError::Experiment("pilot task fixture is empty".to_owned()))?;
    let task_route = task_route(task)?;
    let task_prompt_digest = Digest::blake3(task.prompt.as_bytes());
    let oracle: QualityOracleSpec = task
        .extra
        .get("quality_oracle")
        .cloned()
        .ok_or_else(|| AppError::Experiment("pilot task lacks quality_oracle".to_owned()))
        .and_then(|value| {
            serde_json::from_value(value)
                .map_err(|error| AppError::Experiment(format!("invalid quality oracle: {error}")))
        })?;
    let reused_p0 = reuse_p0_root.as_deref().map(load_reusable_p0).transpose()?;
    if let Some((manifest, observation)) = &reused_p0 {
        let equivalent = manifest.task_id == task.id
            && manifest.main_model == main_model
            && manifest.main_reasoning == main_reasoning
            && manifest.service_tier == service_tier
            && manifest.repository_sha == BENCHMARK_REPOSITORY_SHA
            && manifest.task_prompt_digest == Some(task_prompt_digest)
            && manifest.schedule_seed == schedule_seed
            && manifest.codex_version == isolation.codex_version
            && manifest.arm == ProductArm::P0
            && observation.arm == ProductArm::P0
            && observation.task_id == task.id
            && observation.transport_success
            && observation.process_success
            && observation.manifest_digest == manifest.digest()?;
        if !equivalent {
            return Err(AppError::Experiment(
                "reused P0 is not a completed equivalent baseline for this N1 run".to_owned(),
            ));
        }
    }
    if preflight_only {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "passed": true,
                "live_calls_started": 0,
                "artifact_root_available": true,
                "task_id": task.id,
                "route": task_route,
                "repository_sha": BENCHMARK_REPOSITORY_SHA,
                "codex_version": isolation.codex_version,
                "main_model": main_model,
                "worker_model": worker_model,
                "service_tier": service_tier,
                "pricing_snapshot_digest": pricing_snapshot_digest,
                "hook_binary_current": true,
                "planned_arms": if only_n1 {
                    json!([
                        {
                            "arm": "needle_miss",
                            "maximum_main_calls": 1,
                            "maximum_logical_workers": 1,
                        }
                    ])
                } else {
                    json!([
                        {
                            "arm": "p0",
                            "maximum_main_calls": 1,
                            "maximum_logical_workers": 0,
                        },
                        {
                            "arm": "needle_miss",
                            "maximum_main_calls": 1,
                            "maximum_logical_workers": 1,
                        }
                    ])
                },
                "automatic_retries": 0,
            }))?
        );
        return Ok(());
    }
    fs::create_dir_all(&artifact_root)?;
    let artifact_root = canonical_child_path(&artifact_root)?;
    fs::write(
        artifact_root.join("pricing-snapshot.json"),
        serde_json::to_vec_pretty(&pricing_snapshot)?,
    )?;
    let source = prepare_pilot_source(&artifact_root)?;
    let baseline_repo = if only_n1 {
        None
    } else {
        Some(clone_local_checkout(&source, &artifact_root.join("p0/repo"))?)
    };
    let needle_repo = clone_local_checkout(&source, &artifact_root.join("n1/repo"))?;
    let evaluator_repo = clone_local_checkout(&source, &artifact_root.join("evaluator/repo"))?;
    let baseline_output = artifact_root.join("p0");
    let needle_output = artifact_root.join("n1");
    if !only_n1 {
        fs::create_dir_all(&baseline_output)?;
    }
    fs::create_dir_all(&needle_output)?;
    let baseline_plugin_data = baseline_output.join("plugin-data");
    let needle_product_data = needle_output.join("product-data");
    fs::create_dir_all(&baseline_plugin_data)?;
    fs::create_dir_all(&needle_product_data)?;
    fs::write(needle_product_data.join("pilot-root-task.txt"), &task.prompt)?;
    fs::write(needle_product_data.join("pilot-oracle.json"), serde_json::to_vec_pretty(&oracle)?)?;
    let product_store = RuntimeStore::new(needle_product_data.join("needle.sqlite3"));
    product_store
        .initialize_defaults(&RuntimeSettings {
            codex_executable: codex_executable.clone(),
            worker_model: worker_model.clone(),
            worker_reasoning: worker_reasoning.clone(),
            worker_timeout_seconds: 180,
            evidence_failure_policy: option_value(arguments, "--evidence-failure-policy")
                .map(|value| parse_evidence_failure_policy(&value))
                .transpose()?
                .unwrap_or_default(),
            trusted_test_execution: true,
            multi_need_policy: needle_core::MultiNeedPolicy::default(),
        })
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let common_constraint = "\n\nDo not run tests, builds, formatters, linters, package managers, scripts, or project executables. The evaluator runs the focused test in a separate checkout.";
    let request = product_task_request(&task.prompt, &task_route);
    let needle_prompt = format!(
        "[Needle product pilot N1.]\nEmit exactly this bounded request, then stop and await the Needle continuation:\n{request}\n\nOriginal task:\n{}{}",
        task.prompt, common_constraint
    );
    let needle_args = build_codex_initial_args(
        &main_model,
        &main_reasoning,
        &service_tier,
        &product_profile,
        &needle_prompt,
        &needle_repo,
        true,
    )?;
    let baseline_run = if let Some(baseline_repo) = baseline_repo.as_deref() {
        let baseline_prompt = format!(
            "[Needle product pilot P0: native discovery, no Needle context.]\n{}{}",
            task.prompt, common_constraint
        );
        let baseline_args = build_codex_initial_args(
            &main_model,
            &main_reasoning,
            &service_tier,
            &baseline_profile,
            &baseline_prompt,
            baseline_repo,
            true,
        )?;
        let capture = run_product_child_capture(
            &codex,
            &baseline_args,
            baseline_repo,
            &codex_home,
            &baseline_plugin_data,
            None,
            &baseline_output,
            Duration::from_secs(timeout_seconds),
        )?;
        let parsed = parse_codex_jsonl(&capture.stdout_text());
        let (_, snapshot) = capture_git_snapshot(baseline_repo)
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        Some((capture, parsed, snapshot))
    } else {
        None
    };
    let needle_capture = run_product_child_capture(
        &codex,
        &needle_args,
        &needle_repo,
        &codex_home,
        &needle_product_data,
        Some(&needle_product_data),
        &needle_output,
        Duration::from_secs(timeout_seconds),
    )?;
    let needle_parsed = parse_codex_jsonl(&needle_capture.stdout_text());
    let evaluator_test_passed = if needle_capture.abort_reason.is_none() {
        run_pilot_evaluator_test(
            &evaluator_repo,
            &oracle.focused_test_command,
            Duration::from_secs(timeout_seconds.saturating_mul(2)),
        )?
    } else {
        false
    };
    let needle_quality = QualityOracleResult::evaluate(
        &oracle,
        needle_parsed.final_response.as_deref().unwrap_or_default(),
        Some(evaluator_test_passed),
    );
    let (_, needle_snapshot) = capture_git_snapshot(&needle_repo)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let profile_digest = HookConfig::default().profile()?.definition_digest;
    let routes = product_store.routes().map_err(|error| AppError::Experiment(error.to_string()))?;
    let route = routes
        .iter()
        .find(|route| route.id == task_route.as_str())
        .ok_or_else(|| AppError::Experiment(format!("{task_route} route is missing")))?;
    let preset = product_store
        .preset(&route.preset_id)
        .map_err(|error| AppError::Experiment(error.to_string()))?
        .ok_or_else(|| AppError::Experiment(format!("{task_route} preset is missing")))?;
    let worker_config = product_store
        .settings()
        .map_err(|error| AppError::Experiment(error.to_string()))?
        .worker_config();
    let (mut baseline_manifest, mut baseline_observation) =
        if let Some((baseline_capture, baseline_parsed, baseline_snapshot)) = baseline_run {
            let baseline_quality = QualityOracleResult::evaluate(
                &oracle,
                baseline_parsed.final_response.as_deref().unwrap_or_default(),
                Some(evaluator_test_passed),
            );
            let manifest = ProductRunManifest {
                format_revision: FORMAT_REVISION,
                task_id: task.id.clone(),
                task_prompt_digest: Some(task_prompt_digest),
                arm: ProductArm::P0,
                main_model: main_model.clone(),
                main_reasoning: main_reasoning.clone(),
                worker_model: None,
                worker_reasoning: None,
                service_tier: service_tier.clone(),
                codex_version: isolation.codex_version.clone(),
                operating_system: env::consts::OS.to_owned(),
                repository_sha: BENCHMARK_REPOSITORY_SHA.to_owned(),
                repository_snapshot_digest: baseline_snapshot.source_digest,
                prompt_profile_digest: None,
                route_definition_digest: None,
                preset_definition_digest: None,
                worker_configuration_digest: None,
                output_schema_digest: None,
                schedule_seed,
                pricing_revision: Some(pricing_snapshot.revision.clone()),
                pricing_snapshot_digest: Some(pricing_snapshot_digest),
            };
            let main_cost = price_usage_observation(
                &pricing_snapshot,
                &main_model,
                &service_tier,
                baseline_parsed.usage.input_tokens,
                baseline_parsed.usage.cached_input_tokens,
                baseline_parsed.usage.output_tokens,
            )?;
            let observation = ProductObservation {
                manifest_digest: manifest.digest()?,
                arm: ProductArm::P0,
                task_id: task.id.clone(),
                transport_success: baseline_parsed.terminal_event,
                process_success: !baseline_capture.failed()
                    && baseline_parsed.terminal_success == Some(true),
                quality: baseline_quality,
                cache_lookup: None,
                cache_lookup_latency_ms: None,
                worker_spawns: 0,
                duplicate_worker_spawns: 0,
                logical_worker_spawns: 0,
                worker_turns: 0,
                repair_performed: false,
                discarded_facts: 0,
                pilot_abort_reason: None,
                process: baseline_capture.process_status(),
                main_discovery_before_brief: baseline_parsed.discovery_before_brief,
                main_discovery_after_brief: baseline_parsed.discovery_after_brief,
                main_discovery_total: baseline_parsed.discovery_total,
                wall_time_ms: baseline_capture.duration_ms,
                main_input_tokens: baseline_parsed.usage.input_tokens,
                main_cached_input_tokens: baseline_parsed.usage.cached_input_tokens,
                main_output_tokens: baseline_parsed.usage.output_tokens,
                worker_input_tokens: None,
                worker_cached_input_tokens: None,
                worker_output_tokens: None,
                worker_pricing_verified: false,
                main_cost: Some(main_cost),
                worker_cost: None,
                result_digest: None,
                stale_hit: false,
            };
            (manifest, observation)
        } else {
            reused_p0.ok_or_else(|| {
                AppError::Experiment("N1-only pilot has no reusable P0 baseline".to_owned())
            })?
        };
    if baseline_manifest.pricing_snapshot_digest != Some(pricing_snapshot_digest) {
        baseline_manifest.pricing_revision = Some(pricing_snapshot.revision.clone());
        baseline_manifest.pricing_snapshot_digest = Some(pricing_snapshot_digest);
        baseline_observation.main_cost = Some(price_usage_observation(
            &pricing_snapshot,
            &baseline_manifest.main_model,
            &baseline_manifest.service_tier,
            baseline_observation.main_input_tokens,
            baseline_observation.main_cached_input_tokens,
            baseline_observation.main_output_tokens,
        )?);
        baseline_observation.worker_cost = None;
        baseline_observation.manifest_digest = baseline_manifest.digest()?;
    }
    let needle_manifest = ProductRunManifest {
        format_revision: FORMAT_REVISION,
        task_id: task.id.clone(),
        task_prompt_digest: Some(task_prompt_digest),
        arm: ProductArm::NeedleMiss,
        main_model,
        main_reasoning,
        worker_model: Some(worker_model),
        worker_reasoning: Some(worker_reasoning),
        service_tier,
        codex_version: isolation.codex_version,
        operating_system: env::consts::OS.to_owned(),
        repository_sha: BENCHMARK_REPOSITORY_SHA.to_owned(),
        repository_snapshot_digest: needle_snapshot.source_digest,
        prompt_profile_digest: Some(profile_digest),
        route_definition_digest: Some(route.definition_digest),
        preset_definition_digest: Some(preset.definition_digest),
        worker_configuration_digest: Some(worker_config.digest()),
        output_schema_digest: Some(Digest::blake3(needle_core::ARTIFACT_RESULT_SCHEMA_ID)),
        schedule_seed,
        pricing_revision: Some(pricing_snapshot.revision.clone()),
        pricing_snapshot_digest: Some(pricing_snapshot_digest),
    };
    let needle_manifest_digest = needle_manifest.digest()?;
    let latest_worker = product_store
        .latest_worker_run()
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let main_cost = price_usage_observation_optional(
        &pricing_snapshot,
        &needle_manifest.main_model,
        &needle_manifest.service_tier,
        needle_parsed.usage.input_tokens,
        needle_parsed.usage.cached_input_tokens,
        needle_parsed.usage.output_tokens,
    )?;
    let worker_cost = price_usage_observation_optional(
        &pricing_snapshot,
        needle_manifest.worker_model.as_deref().ok_or_else(|| {
            AppError::Experiment("Needle manifest is missing worker model".to_owned())
        })?,
        &needle_manifest.service_tier,
        latest_worker.as_ref().and_then(|run| run.input_tokens),
        latest_worker.as_ref().and_then(|run| run.cached_input_tokens),
        latest_worker.as_ref().and_then(|run| run.output_tokens),
    )?;
    let logical_worker_spawns = latest_worker.as_ref().map_or(0, |run| run.logical_worker_spawns);
    let needle_observation = ProductObservation {
        manifest_digest: needle_manifest_digest,
        arm: ProductArm::NeedleMiss,
        task_id: task.id.clone(),
        transport_success: needle_parsed.terminal_event,
        process_success: !needle_capture.failed() && needle_parsed.terminal_success == Some(true),
        quality: needle_quality,
        cache_lookup: Some("miss".to_owned()),
        cache_lookup_latency_ms: None,
        worker_spawns: logical_worker_spawns,
        duplicate_worker_spawns: logical_worker_spawns.saturating_sub(1),
        logical_worker_spawns,
        worker_turns: latest_worker.as_ref().map_or(0, |run| run.worker_turns),
        repair_performed: latest_worker.as_ref().is_some_and(|run| run.repair_performed),
        discarded_facts: latest_worker.as_ref().map_or(0, |run| run.discarded_facts),
        pilot_abort_reason: needle_capture.abort_reason.clone(),
        process: needle_capture.process_status(),
        main_discovery_before_brief: needle_parsed.discovery_before_brief,
        main_discovery_after_brief: needle_parsed.discovery_after_brief,
        main_discovery_total: needle_parsed.discovery_total,
        wall_time_ms: needle_capture.duration_ms,
        main_input_tokens: needle_parsed.usage.input_tokens,
        main_cached_input_tokens: needle_parsed.usage.cached_input_tokens,
        main_output_tokens: needle_parsed.usage.output_tokens,
        worker_input_tokens: latest_worker.as_ref().and_then(|run| run.input_tokens),
        worker_cached_input_tokens: latest_worker.as_ref().and_then(|run| run.cached_input_tokens),
        worker_output_tokens: latest_worker.as_ref().and_then(|run| run.output_tokens),
        worker_pricing_verified: true,
        main_cost,
        worker_cost,
        result_digest: latest_worker.as_ref().and_then(|run| run.result_digest),
        stale_hit: false,
    };
    let gate = evaluate_pilot_pair(&baseline_observation, &needle_observation);
    if gate.passed {
        product_store
            .mark_utility_gate_passed()
            .map_err(|error| AppError::Experiment(error.to_string()))?;
    }
    fs::write(
        artifact_root.join("run-manifests.json"),
        serde_json::to_vec_pretty(&[baseline_manifest, needle_manifest])?,
    )?;
    let observations = [&baseline_observation, &needle_observation]
        .into_iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n")
        + "\n";
    fs::write(artifact_root.join("product-observations.jsonl"), observations)?;
    fs::write(artifact_root.join("pilot-report.json"), serde_json::to_vec_pretty(&gate)?)?;
    let verdict =
        ProductVerdict::evaluate(&[baseline_observation.clone(), needle_observation.clone()]);
    fs::write(artifact_root.join("product-verdict.json"), serde_json::to_vec_pretty(&verdict)?)?;
    println!(
        "product pilot report written to {}",
        artifact_root.join("pilot-report.json").display()
    );
    if gate.passed {
        Ok(())
    } else {
        Err(AppError::Experiment(format!(
            "product pilot gate failed: {}; make at most one targeted correction before repeating the pair",
            gate.failures.join(", ")
        )))
    }
}

fn product_cache_pilot_run(arguments: &[String]) -> Result<(), AppError> {
    let codex = resolve_codex(option_value(arguments, "--codex"))?;
    let codex_home = PathBuf::from(required_value(arguments, "--codex-home")?);
    ensure_dedicated_codex_home(&codex_home)?;
    let codex_home = canonical_child_path(&codex_home)?;
    ensure_product_pilot_hook_isolation(&codex_home)?;
    ensure_cache_pilot_hook_binary(&codex_home)?;
    let source_pilot_root =
        canonical_child_path(Path::new(&required_value(arguments, "--source-pilot-root")?))?;
    let artifact_root =
        absolute_run_path(Path::new(&required_value(arguments, "--artifact-root")?))?;
    let product_profile = required_value(arguments, "--product-profile")?;
    validate_slug(&product_profile, "product profile")?;
    let preflight_only = arguments.iter().any(|argument| argument == "--preflight-only");
    let mutation_suite = arguments.iter().any(|argument| argument == "--mutation-suite");
    let pricing_snapshot_path = PathBuf::from(required_value(arguments, "--pricing-snapshot")?);
    let timeout_seconds = option_value(arguments, "--timeout-seconds")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid --timeout-seconds: {error}")))?
        .unwrap_or(240);
    if timeout_seconds < 180 {
        return Err(AppError::Usage(
            "cache-pilot --timeout-seconds must be at least 180".to_owned(),
        ));
    }
    if artifact_root.exists() {
        return Err(AppError::Experiment(format!(
            "cache-pilot artifact root already exists: {}",
            artifact_root.display()
        )));
    }
    let source_report: PilotGateResult =
        serde_json::from_slice(&fs::read(source_pilot_root.join("pilot-report.json"))?)?;
    if !source_report.passed
        || !source_report.comparable_completion
        || !source_report.economics_pass
    {
        return Err(AppError::Experiment(
            "source pilot did not pass comparable completion and economics".to_owned(),
        ));
    }
    let manifests: Vec<ProductRunManifest> =
        serde_json::from_slice(&fs::read(source_pilot_root.join("run-manifests.json"))?)?;
    let source_manifest =
        manifests.iter().find(|manifest| manifest.arm == ProductArm::NeedleMiss).ok_or_else(
            || AppError::Experiment("source pilot lacks Needle miss manifest".to_owned()),
        )?;
    let task_path = option_value(arguments, "--tasks")
        .map(PathBuf::from)
        .unwrap_or_else(default_task_fixture_path);
    let tasks = parse_task_fixture(&fs::read_to_string(task_path)?)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let task = tasks
        .first()
        .ok_or_else(|| AppError::Experiment("cache-pilot task fixture is empty".to_owned()))?;
    let task_route = task_route(task)?;
    let task_prompt_digest = Digest::blake3(task.prompt.as_bytes());
    if source_manifest.task_id != task.id
        || source_manifest.task_prompt_digest != Some(task_prompt_digest)
        || source_manifest.repository_sha != BENCHMARK_REPOSITORY_SHA
    {
        return Err(AppError::Experiment(
            "cache-pilot task, prompt, or repository differs from the promoted source pilot"
                .to_owned(),
        ));
    }
    let oracle: QualityOracleSpec = task
        .extra
        .get("quality_oracle")
        .cloned()
        .ok_or_else(|| AppError::Experiment("cache-pilot task lacks quality_oracle".to_owned()))
        .and_then(|value| {
            serde_json::from_value(value)
                .map_err(|error| AppError::Experiment(format!("invalid quality oracle: {error}")))
        })?;
    let pricing_snapshot: PricingSnapshot =
        serde_json::from_slice(&fs::read(&pricing_snapshot_path)?).map_err(|error| {
            AppError::Experiment(format!(
                "invalid pricing snapshot {}: {error}",
                pricing_snapshot_path.display()
            ))
        })?;
    pricing_snapshot.validate().map_err(|error| AppError::Experiment(error.to_string()))?;
    let pricing_snapshot_digest = pricing_snapshot.digest()?;
    if source_manifest.pricing_snapshot_digest != Some(pricing_snapshot_digest) {
        return Err(AppError::Experiment(
            "cache-pilot pricing snapshot differs from the promoted source pilot".to_owned(),
        ));
    }
    let codex_executable = codex.display().to_string();
    let isolation = CodexWorker::verify_isolation(&codex_executable).map_err(AppError::Runtime)?;
    if !isolation.verified() || source_manifest.codex_version != isolation.codex_version {
        return Err(AppError::Experiment(
            "cache-pilot Codex version differs from the promoted source pilot".to_owned(),
        ));
    }
    let main_model = source_manifest.main_model.clone();
    let main_reasoning = source_manifest.main_reasoning.clone();
    let worker_model = source_manifest
        .worker_model
        .clone()
        .ok_or_else(|| AppError::Experiment("source manifest lacks worker model".to_owned()))?;
    let service_tier = source_manifest.service_tier.clone();
    let source_data = source_pilot_root.join("n1/product-data");
    let source_store = RuntimeStore::new(source_data.join("needle.sqlite3"));
    if !source_store
        .utility_gate_passed()
        .map_err(|error| AppError::Experiment(error.to_string()))?
    {
        return Err(AppError::Experiment("source pilot runtime data is not promoted".to_owned()));
    }
    let source_config =
        source_store.export_toml().map_err(|error| AppError::Experiment(error.to_string()))?;
    let source_repository = source_pilot_root.join("source");
    if !source_repository.is_dir() {
        return Err(AppError::Experiment(
            "source pilot repository snapshot is unavailable".to_owned(),
        ));
    }
    if preflight_only {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "passed": true,
                "live_calls_started": 0,
                "source_pilot": source_pilot_root,
                "artifact_root_available": !artifact_root.exists(),
            "task_id": task.id,
            "route": task_route,
                "repository_sha": BENCHMARK_REPOSITORY_SHA,
                "codex_version": isolation.codex_version,
                "main_model": main_model,
                "worker_model": worker_model,
                "service_tier": service_tier,
                "pricing_snapshot_digest": pricing_snapshot_digest,
                "route_promoted": true,
                "hook_binary_current": true,
                "planned_arms": if mutation_suite {
                    json!([
                        {
                            "arm": "publication_miss",
                            "maximum_logical_workers": 1,
                        },
                        {
                            "arm": "irrelevant_mutation",
                            "maximum_logical_workers": 0,
                            "runs_only_after_semantic_publication": true,
                        },
                        {
                            "arm": "relevant_mutation",
                            "maximum_logical_workers": 1,
                            "runs_only_after_irrelevant_full_hit": true,
                        }
                    ])
                } else {
                    json!([
                        {
                            "arm": "publication_miss",
                            "maximum_logical_workers": 1,
                        },
                        {
                            "arm": "exact_hit",
                            "maximum_logical_workers": 0,
                            "runs_only_after_valid_publication": true,
                        }
                    ])
                },
                "automatic_retries": 0,
            }))?
        );
        return Ok(());
    }

    fs::create_dir_all(&artifact_root)?;
    let artifact_root = canonical_child_path(&artifact_root)?;
    fs::write(
        artifact_root.join("pricing-snapshot.json"),
        serde_json::to_vec_pretty(&pricing_snapshot)?,
    )?;
    fs::write(
        artifact_root.join("source-run-manifests.json"),
        serde_json::to_vec_pretty(&manifests)?,
    )?;
    let repository = clone_local_checkout(&source_repository, &artifact_root.join("repo"))?;
    let (_, initial_snapshot) = capture_git_snapshot(&repository)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let product_data = artifact_root.join("product-data");
    let publication_output = artifact_root.join("publication");
    let exact_output = artifact_root.join("exact");
    fs::create_dir_all(&product_data)?;
    fs::create_dir_all(&publication_output)?;
    fs::create_dir_all(&exact_output)?;
    fs::write(product_data.join("pilot-root-task.txt"), &task.prompt)?;
    fs::write(product_data.join("pilot-oracle.json"), serde_json::to_vec_pretty(&oracle)?)?;
    let store = RuntimeStore::new(product_data.join("needle.sqlite3"));
    store.initialize().map_err(|error| AppError::Experiment(error.to_string()))?;
    store.import_toml(&source_config).map_err(|error| AppError::Experiment(error.to_string()))?;
    let mut settings = store.settings().map_err(|error| AppError::Experiment(error.to_string()))?;
    settings.codex_executable = codex_executable;
    store
        .set_runtime_settings(&settings)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    store.mark_utility_gate_passed().map_err(|error| AppError::Experiment(error.to_string()))?;

    let common_constraint = "\n\nDo not run tests, builds, formatters, linters, package managers, scripts, or project executables. The evaluator runs the focused test in a separate checkout.";
    let request = product_task_request(&task.prompt, &task_route);
    let prompt = format!(
        "[Needle post-promotion cache pilot.]\nEmit exactly this bounded request, then stop and await the Needle continuation:\n{request}\n\nOriginal task:\n{}{}",
        task.prompt, common_constraint
    );
    let codex_arguments = build_codex_initial_args(
        &main_model,
        &main_reasoning,
        &service_tier,
        &product_profile,
        &prompt,
        &repository,
        true,
    )?;

    let publication_before =
        store.worker_run_count().map_err(|error| AppError::Experiment(error.to_string()))?;
    let publication_capture = run_product_child_capture(
        &codex,
        &codex_arguments,
        &repository,
        &codex_home,
        &product_data,
        Some(&product_data),
        &publication_output,
        Duration::from_secs(timeout_seconds),
    )?;
    let publication_parsed = parse_codex_jsonl(&publication_capture.stdout_text());
    let publication_after =
        store.worker_run_count().map_err(|error| AppError::Experiment(error.to_string()))?;
    let publication_worker =
        store.latest_worker_run().map_err(|error| AppError::Experiment(error.to_string()))?;
    let published_artifacts =
        store.artifacts().map_err(|error| AppError::Experiment(error.to_string()))?;
    let semantic_artifacts_after_publication = published_artifacts
        .iter()
        .filter(|artifact| artifact.dependency_manifest.supports_worktree_semantic())
        .count()
        .try_into()
        .unwrap_or(u64::MAX);
    let artifacts_after_publication = published_artifacts.len().try_into().unwrap_or(u64::MAX);
    let cache_entries_after_publication = store
        .cache_records()
        .map_err(|error| AppError::Experiment(error.to_string()))?
        .len()
        .try_into()
        .unwrap_or(u64::MAX);
    let publication = CachePilotArmObservation {
        arm: ProductArm::NeedleMiss,
        transport_success: publication_parsed.terminal_event,
        process_success: !publication_capture.failed()
            && publication_parsed.terminal_success == Some(true),
        process: publication_capture.process_status(),
        resolve: read_pilot_outcome(&product_data),
        worker_runs_before: publication_before,
        worker_runs_after: publication_after,
        worker_run_delta: publication_after.saturating_sub(publication_before),
        main_discovery_total: publication_parsed.discovery_total,
        wall_time_ms: publication_capture.duration_ms,
        main_input_tokens: publication_parsed.usage.input_tokens,
        main_cached_input_tokens: publication_parsed.usage.cached_input_tokens,
        main_output_tokens: publication_parsed.usage.output_tokens,
        main_cost: price_usage_observation_optional(
            &pricing_snapshot,
            &main_model,
            &service_tier,
            publication_parsed.usage.input_tokens,
            publication_parsed.usage.cached_input_tokens,
            publication_parsed.usage.output_tokens,
        )?,
        worker_cost: price_usage_observation_optional(
            &pricing_snapshot,
            &worker_model,
            &service_tier,
            publication_worker.as_ref().and_then(|run| run.input_tokens),
            publication_worker.as_ref().and_then(|run| run.cached_input_tokens),
            publication_worker.as_ref().and_then(|run| run.output_tokens),
        )?,
    };
    let publication_ready = publication.transport_success
        && publication.process_success
        && publication.worker_run_delta == 1
        && artifacts_after_publication > 0
        && cache_entries_after_publication > 0
        && publication.resolve.as_ref().is_some_and(|outcome| {
            outcome.status == "generated"
                && matches!(outcome.cache_resolution, needle_core::CacheResolution::Miss)
                && !outcome.cache_hit
                && outcome.worker_spawned
        });
    if mutation_suite {
        return finish_mutation_pilot(MutationRunContext {
            codex: &codex,
            codex_arguments: &codex_arguments,
            repository: &repository,
            codex_home: &codex_home,
            product_data: &product_data,
            artifact_root: &artifact_root,
            pricing_snapshot: &pricing_snapshot,
            main_model: &main_model,
            worker_model: &worker_model,
            service_tier: &service_tier,
            store: &store,
            timeout: Duration::from_secs(timeout_seconds),
            initial_source_digest: initial_snapshot.source_digest,
            publication,
            publication_ready,
            published_artifacts: &published_artifacts,
            semantic_artifacts_after_publication,
            artifacts_after_publication,
        });
    }

    let exact = if publication_ready {
        let exact_before =
            store.worker_run_count().map_err(|error| AppError::Experiment(error.to_string()))?;
        let exact_capture = run_product_child_capture(
            &codex,
            &codex_arguments,
            &repository,
            &codex_home,
            &product_data,
            Some(&product_data),
            &exact_output,
            Duration::from_secs(timeout_seconds),
        )?;
        let exact_parsed = parse_codex_jsonl(&exact_capture.stdout_text());
        let exact_after =
            store.worker_run_count().map_err(|error| AppError::Experiment(error.to_string()))?;
        CachePilotArmObservation {
            arm: ProductArm::NeedleHit,
            transport_success: exact_parsed.terminal_event,
            process_success: !exact_capture.failed() && exact_parsed.terminal_success == Some(true),
            process: exact_capture.process_status(),
            resolve: read_pilot_outcome(&product_data),
            worker_runs_before: exact_before,
            worker_runs_after: exact_after,
            worker_run_delta: exact_after.saturating_sub(exact_before),
            main_discovery_total: exact_parsed.discovery_total,
            wall_time_ms: exact_capture.duration_ms,
            main_input_tokens: exact_parsed.usage.input_tokens,
            main_cached_input_tokens: exact_parsed.usage.cached_input_tokens,
            main_output_tokens: exact_parsed.usage.output_tokens,
            main_cost: price_usage_observation_optional(
                &pricing_snapshot,
                &main_model,
                &service_tier,
                exact_parsed.usage.input_tokens,
                exact_parsed.usage.cached_input_tokens,
                exact_parsed.usage.output_tokens,
            )?,
            worker_cost: None,
        }
    } else {
        CachePilotArmObservation {
            arm: ProductArm::NeedleHit,
            transport_success: false,
            process_success: false,
            process: ProcessExecutionStatus {
                status: "skipped:publication_miss_failed".to_owned(),
                ..ProcessExecutionStatus::default()
            },
            resolve: None,
            worker_runs_before: publication_after,
            worker_runs_after: publication_after,
            worker_run_delta: 0,
            main_discovery_total: 0,
            wall_time_ms: 0,
            main_input_tokens: None,
            main_cached_input_tokens: None,
            main_output_tokens: None,
            main_cost: None,
            worker_cost: None,
        }
    };
    let (_, final_snapshot) = capture_git_snapshot(&repository)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let checkout_clean = initial_snapshot.source_digest == final_snapshot.source_digest
        && repository_status_clean(&repository)?;
    let report = evaluate_cache_pilot(
        publication,
        exact,
        artifacts_after_publication,
        cache_entries_after_publication,
        checkout_clean,
    );
    fs::write(artifact_root.join("cache-pilot-report.json"), serde_json::to_vec_pretty(&report)?)?;
    let observations =
        [serde_json::to_string(&report.publication)?, serde_json::to_string(&report.exact)?]
            .join("\n")
            + "\n";
    fs::write(artifact_root.join("cache-observations.jsonl"), observations)?;
    println!(
        "cache pilot report written to {}",
        artifact_root.join("cache-pilot-report.json").display()
    );
    if report.passed {
        Ok(())
    } else {
        Err(AppError::Experiment(format!(
            "cache pilot gate failed: {}",
            report.failures.join(", ")
        )))
    }
}

struct MutationRunContext<'a> {
    codex: &'a Path,
    codex_arguments: &'a [String],
    repository: &'a Path,
    codex_home: &'a Path,
    product_data: &'a Path,
    artifact_root: &'a Path,
    pricing_snapshot: &'a PricingSnapshot,
    main_model: &'a str,
    worker_model: &'a str,
    service_tier: &'a str,
    store: &'a RuntimeStore,
    timeout: Duration,
    initial_source_digest: Digest,
    publication: CachePilotArmObservation,
    publication_ready: bool,
    published_artifacts: &'a [needle_core::Artifact],
    semantic_artifacts_after_publication: u64,
    artifacts_after_publication: u64,
}

fn finish_mutation_pilot(context: MutationRunContext<'_>) -> Result<(), AppError> {
    let worker_count = context
        .store
        .worker_run_count()
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let semantic_ready = context.publication_ready
        && context.artifacts_after_publication > 0
        && context.semantic_artifacts_after_publication == context.artifacts_after_publication;
    let irrelevant_relative = "needle-mutation-irrelevant.txt";
    let irrelevant_path = context.repository.join(irrelevant_relative);
    let irrelevant = if semantic_ready {
        if irrelevant_path.exists() {
            return Err(AppError::Experiment(format!(
                "irrelevant mutation path already exists: {}",
                irrelevant_path.display()
            )));
        }
        fs::write(&irrelevant_path, b"Needle irrelevant mutation pilot.\n")?;
        let result = observe_mutation_arm(
            &context,
            ProductArm::NeedleHit,
            &context.artifact_root.join("irrelevant"),
        );
        let cleanup = fs::remove_file(&irrelevant_path);
        cleanup?;
        result?
    } else {
        skipped_cache_observation(
            ProductArm::NeedleHit,
            "semantic_publication_failed",
            worker_count,
        )
    };
    let irrelevant_ready = irrelevant.transport_success
        && irrelevant.process_success
        && irrelevant.worker_run_delta == 0
        && irrelevant.resolve.as_ref().is_some_and(|outcome| {
            outcome.status == "hit"
                && matches!(
                    outcome.cache_resolution,
                    needle_core::CacheResolution::ExactHit { .. }
                        | needle_core::CacheResolution::CompositeHit { .. }
                )
                && outcome.cache_hit
                && !outcome.worker_spawned
                && context
                    .publication
                    .resolve
                    .as_ref()
                    .is_some_and(|seed| seed.result_digest == outcome.result_digest)
        });
    let relevant_relative = if irrelevant_ready {
        select_relevant_mutation_path(context.published_artifacts)?
    } else {
        String::new()
    };
    let relevant = if irrelevant_ready {
        let relevant_path = context.repository.join(&relevant_relative);
        let original = fs::read(&relevant_path)?;
        let mut mutated = original.clone();
        mutated.extend_from_slice(b"\n// Needle relevant mutation pilot.\n");
        fs::write(&relevant_path, mutated)?;
        let result = observe_mutation_arm(
            &context,
            ProductArm::StaleMutation,
            &context.artifact_root.join("relevant"),
        );
        let restore = fs::write(&relevant_path, original);
        restore?;
        result?
    } else {
        let count = context
            .store
            .worker_run_count()
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        skipped_cache_observation(ProductArm::StaleMutation, "irrelevant_full_hit_failed", count)
    };
    let (_, restored_snapshot) = capture_git_snapshot(context.repository)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let checkout_restored = restored_snapshot.source_digest == context.initial_source_digest
        && repository_status_clean(context.repository)?;
    let report = evaluate_mutation_pilot(
        context.publication,
        irrelevant,
        relevant,
        context.semantic_artifacts_after_publication,
        context.artifacts_after_publication,
        checkout_restored,
        irrelevant_relative.to_owned(),
        relevant_relative,
    );
    fs::write(
        context.artifact_root.join("mutation-pilot-report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    let observations = [
        serde_json::to_string(&report.publication)?,
        serde_json::to_string(&report.irrelevant)?,
        serde_json::to_string(&report.relevant)?,
    ]
    .join("\n")
        + "\n";
    fs::write(context.artifact_root.join("mutation-observations.jsonl"), observations)?;
    println!(
        "mutation pilot report written to {}",
        context.artifact_root.join("mutation-pilot-report.json").display()
    );
    if report.passed {
        Ok(())
    } else {
        Err(AppError::Experiment(format!(
            "mutation pilot gate failed: {}",
            report.failures.join(", ")
        )))
    }
}

fn observe_mutation_arm(
    context: &MutationRunContext<'_>,
    arm: ProductArm,
    output: &Path,
) -> Result<CachePilotArmObservation, AppError> {
    fs::create_dir_all(output)?;
    let before = context
        .store
        .worker_run_count()
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let capture = run_product_child_capture(
        context.codex,
        context.codex_arguments,
        context.repository,
        context.codex_home,
        context.product_data,
        Some(context.product_data),
        output,
        context.timeout,
    )?;
    let parsed = parse_codex_jsonl(&capture.stdout_text());
    let after = context
        .store
        .worker_run_count()
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let worker = if after > before {
        context
            .store
            .latest_worker_run()
            .map_err(|error| AppError::Experiment(error.to_string()))?
    } else {
        None
    };
    Ok(CachePilotArmObservation {
        arm,
        transport_success: parsed.terminal_event,
        process_success: !capture.failed() && parsed.terminal_success == Some(true),
        process: capture.process_status(),
        resolve: read_pilot_outcome(context.product_data),
        worker_runs_before: before,
        worker_runs_after: after,
        worker_run_delta: after.saturating_sub(before),
        main_discovery_total: parsed.discovery_total,
        wall_time_ms: capture.duration_ms,
        main_input_tokens: parsed.usage.input_tokens,
        main_cached_input_tokens: parsed.usage.cached_input_tokens,
        main_output_tokens: parsed.usage.output_tokens,
        main_cost: price_usage_observation_optional(
            context.pricing_snapshot,
            context.main_model,
            context.service_tier,
            parsed.usage.input_tokens,
            parsed.usage.cached_input_tokens,
            parsed.usage.output_tokens,
        )?,
        worker_cost: price_usage_observation_optional(
            context.pricing_snapshot,
            context.worker_model,
            context.service_tier,
            worker.as_ref().and_then(|run| run.input_tokens),
            worker.as_ref().and_then(|run| run.cached_input_tokens),
            worker.as_ref().and_then(|run| run.output_tokens),
        )?,
    })
}

fn skipped_cache_observation(
    arm: ProductArm,
    reason: &str,
    worker_count: u64,
) -> CachePilotArmObservation {
    CachePilotArmObservation {
        arm,
        transport_success: false,
        process_success: false,
        process: ProcessExecutionStatus {
            status: format!("skipped:{reason}"),
            ..ProcessExecutionStatus::default()
        },
        resolve: None,
        worker_runs_before: worker_count,
        worker_runs_after: worker_count,
        worker_run_delta: 0,
        main_discovery_total: 0,
        wall_time_ms: 0,
        main_input_tokens: None,
        main_cached_input_tokens: None,
        main_output_tokens: None,
        main_cost: None,
        worker_cost: None,
    }
}

fn select_relevant_mutation_path(artifacts: &[needle_core::Artifact]) -> Result<String, AppError> {
    let location_paths = artifacts
        .iter()
        .filter(|artifact| artifact.contract.kind == needle_core::ArtifactKind::code_location())
        .flat_map(|artifact| artifact.dependency_manifest.dependencies.iter())
        .map(|dependency| dependency.path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    artifacts
        .iter()
        .filter(|artifact| artifact.contract.kind == needle_core::ArtifactKind::behavior_trace())
        .flat_map(|artifact| artifact.dependency_manifest.dependencies.iter())
        .map(|dependency| dependency.path.as_str())
        .filter(|path| !location_paths.contains(path))
        .filter(|path| Path::new(path).extension().and_then(|value| value.to_str()) == Some("rs"))
        .min()
        .map(str::to_owned)
        .ok_or_else(|| {
            AppError::Experiment(
                "mutation pilot found no behavior dependency independent of CodeLocation"
                    .to_owned(),
            )
        })
}

fn cache_pilot_report(arguments: &[String]) -> Result<(), AppError> {
    let artifact_root = PathBuf::from(required_value(arguments, "--artifact-root")?);
    if !artifact_root.is_dir() {
        return Err(AppError::Usage(format!(
            "cache pilot artifact root is unavailable: {}",
            artifact_root.display()
        )));
    }
    let observations = fs::read_to_string(artifact_root.join("cache-observations.jsonl"))?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<CachePilotArmObservation>)
        .collect::<Result<Vec<_>, _>>()?;
    let publication = observations
        .iter()
        .find(|observation| observation.arm == ProductArm::NeedleMiss)
        .cloned()
        .ok_or_else(|| {
            AppError::Experiment("cache pilot lacks publication observation".to_owned())
        })?;
    let exact = observations
        .iter()
        .find(|observation| observation.arm == ProductArm::NeedleHit)
        .cloned()
        .ok_or_else(|| AppError::Experiment("cache pilot lacks repeat observation".to_owned()))?;
    let previous: Value =
        serde_json::from_slice(&fs::read(artifact_root.join("cache-pilot-report.json"))?)?;
    let artifacts_after_publication = previous
        .get("artifacts_after_publication")
        .and_then(Value::as_u64)
        .ok_or_else(|| AppError::Experiment("cache pilot lacks artifact count".to_owned()))?;
    let cache_entries_after_publication = previous
        .get("cache_entries_after_publication")
        .and_then(Value::as_u64)
        .ok_or_else(|| AppError::Experiment("cache pilot lacks cache count".to_owned()))?;
    let checkout_clean = previous
        .get("checkout_clean")
        .and_then(Value::as_bool)
        .ok_or_else(|| AppError::Experiment("cache pilot lacks checkout verdict".to_owned()))?;
    let report = evaluate_cache_pilot(
        publication,
        exact,
        artifacts_after_publication,
        cache_entries_after_publication,
        checkout_clean,
    );
    let output = option_value(arguments, "--output")
        .map(PathBuf::from)
        .unwrap_or_else(|| artifact_root.join("cache-pilot-report-v2.json"));
    if output.exists() {
        return Err(AppError::Experiment(format!(
            "cache pilot report output already exists: {}",
            output.display()
        )));
    }
    fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
    println!("cache pilot report written to {}", output.display());
    if report.passed {
        Ok(())
    } else {
        Err(AppError::Experiment(format!(
            "cache pilot gate failed: {}",
            report.failures.join(", ")
        )))
    }
}

fn read_pilot_outcome(product_data: &Path) -> Option<CachePilotResolveOutcome> {
    fs::read(product_data.join("pilot-outcome.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

fn repository_status_clean(repository: &Path) -> Result<bool, AppError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|error| AppError::Experiment(format!("inspect pilot checkout: {error}")))?;
    if !output.status.success() {
        return Err(AppError::Experiment(format!(
            "inspect pilot checkout failed with status {}",
            output.status
        )));
    }
    Ok(output.stdout.is_empty())
}

fn price_usage_observation(
    pricing: &PricingSnapshot,
    model: &str,
    service_tier: &str,
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
) -> Result<TokenCost, AppError> {
    let input_tokens = input_tokens.ok_or_else(|| {
        AppError::Experiment(format!("missing input-token usage for priced model `{model}`"))
    })?;
    let cached_input_tokens = cached_input_tokens.ok_or_else(|| {
        AppError::Experiment(format!("missing cached-input-token usage for priced model `{model}`"))
    })?;
    let output_tokens = output_tokens.ok_or_else(|| {
        AppError::Experiment(format!("missing output-token usage for priced model `{model}`"))
    })?;
    pricing
        .price_usage(model, service_tier, input_tokens, cached_input_tokens, output_tokens)
        .map_err(|error| AppError::Experiment(error.to_string()))
}

fn price_usage_observation_optional(
    pricing: &PricingSnapshot,
    model: &str,
    service_tier: &str,
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
) -> Result<Option<TokenCost>, AppError> {
    let (Some(input_tokens), Some(cached_input_tokens), Some(output_tokens)) =
        (input_tokens, cached_input_tokens, output_tokens)
    else {
        return Ok(None);
    };
    pricing
        .price_usage(model, service_tier, input_tokens, cached_input_tokens, output_tokens)
        .map(Some)
        .map_err(|error| AppError::Experiment(error.to_string()))
}

fn run_live_experiment(arguments: &[String], schedule: ExperimentSchedule) -> Result<(), AppError> {
    let codex = resolve_codex(option_value(arguments, "--codex"))?;
    let codex_home = PathBuf::from(required_value(arguments, "--codex-home")?);
    ensure_dedicated_codex_home(&codex_home)?;
    let model = required_value(arguments, "--model")?;
    let reasoning = required_value(arguments, "--reasoning")?;
    let service_tier = required_value(arguments, "--service-tier")?;
    let artifact_root = required_path(arguments, "--artifact-root", "--artifact-root")?;
    let task_path = option_value(arguments, "--tasks")
        .map(PathBuf::from)
        .unwrap_or_else(default_task_fixture_path);
    if !task_path.is_file() {
        return Err(AppError::Usage(format!(
            "task fixture JSON must be a file: {}",
            task_path.display()
        )));
    }
    let tasks = parse_task_fixture(&fs::read_to_string(task_path)?)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let baseline_profile = profile_value(arguments, &["--baseline-profile", "--profile-baseline"])?;
    let marker_profile = profile_value(arguments, &["--marker-profile", "--profile-marker"])?;
    let tool_profile = profile_value(arguments, &["--tool-profile", "--profile-tool"])?;
    validate_model_value(&model, "model")?;
    validate_reasoning(&reasoning)?;
    validate_service_tier(&service_tier)?;
    let timeout_seconds = option_value(arguments, "--timeout-seconds")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid --timeout-seconds: {error}")))?
        .unwrap_or(180);
    if timeout_seconds == 0 {
        return Err(AppError::Usage("--timeout-seconds must be positive".to_owned()));
    }
    let fixture_source = prepare_benchmark_repository(&artifact_root)?;
    let fixture_digest = digest_tree(&fixture_source)?;
    let store = ArtifactStore::new(&artifact_root)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let mut observations_jsonl = String::new();
    let mut had_failure = false;
    for (observation_index, entry) in schedule.entries.iter().enumerate() {
        let task = tasks
            .get((entry.task_seed as usize) % tasks.len())
            .ok_or_else(|| AppError::Experiment("task fixture is empty".to_owned()))?;
        let profile = match entry.arm {
            ExperimentArm::P0 => &baseline_profile,
            ExperimentArm::P1 | ExperimentArm::P2 | ExperimentArm::P4 => &marker_profile,
            ExperimentArm::P3 => &tool_profile,
        };
        let task_request = deterministic_task_request(task.id.as_str(), entry.task_seed);
        let deterministic_request = NeedRequest::parse(&task_request)
            .map_err(|error| {
                AppError::Experiment(format!("deterministic fixture marker invalid: {error}"))
            })?
            .ok_or_else(|| {
                AppError::Experiment("deterministic fixture marker missing".to_owned())
            })?;
        let deterministic_payload = needle_bench::p2_payload(&deterministic_request);
        let prompt = arm_prompt(entry.arm, &task.prompt, &task_request);
        let observation_dir =
            artifact_root.join("observations").join(format!("{observation_index:03}"));
        fs::create_dir_all(&observation_dir)?;
        let fixture_repo = observation_dir.join("repo");
        if fixture_repo.exists() {
            return Err(AppError::Experiment(format!(
                "observation fixture already exists: {}",
                fixture_repo.display()
            )));
        }
        copy_tree(&fixture_source, &fixture_repo)?;
        let plugin_data = observation_dir.join("plugin-data");
        fs::create_dir_all(&plugin_data)?;
        fs::write(plugin_data.join("benchmark-payload.txt"), deterministic_payload.as_bytes())?;
        let initial_args = build_codex_initial_args(
            &model,
            &reasoning,
            &service_tier,
            profile,
            &prompt,
            &fixture_repo,
            entry.arm != ExperimentArm::P0,
        )?;
        let initial_capture = run_child_capture(
            &codex,
            &initial_args,
            &fixture_repo,
            &codex_home,
            &plugin_data,
            &observation_dir,
            "initial",
            entry.arm,
            Duration::from_secs(timeout_seconds),
        )?;
        let initial_stdout_text = initial_capture.stdout_text();
        let initial_stderr_text = initial_capture.stderr_text();
        let initial_parsed = parse_codex_jsonl(&initial_stdout_text);
        let (capture, parsed, initial_usage, duration_ms, mut process_failure) = if entry.arm
            == ExperimentArm::P0
        {
            match initial_parsed.thread_id.clone() {
                None => (
                    initial_capture.clone(),
                    initial_parsed.clone(),
                    None,
                    initial_capture.duration_ms,
                    true,
                ),
                Some(thread_id) => {
                    let resume_args = build_codex_resume_args(&thread_id, &deterministic_payload)?;
                    let resume_capture = run_child_capture(
                        &codex,
                        &resume_args,
                        &fixture_repo,
                        &codex_home,
                        &plugin_data,
                        &observation_dir,
                        "resume",
                        entry.arm,
                        Duration::from_secs(timeout_seconds),
                    )?;
                    let resume_text = resume_capture.stdout_text();
                    let total_duration_ms = observation_duration_ms(
                        initial_capture.duration_ms,
                        Some(resume_capture.duration_ms),
                    );
                    (
                        resume_capture,
                        parse_codex_jsonl(&resume_text),
                        Some(initial_parsed.usage.clone()),
                        total_duration_ms,
                        initial_capture.failed()
                            || !initial_parsed.errors.is_empty()
                            || initial_parsed.terminal_success == Some(false),
                    )
                }
            }
        } else {
            (
                initial_capture.clone(),
                initial_parsed.clone(),
                None,
                observation_duration_ms(initial_capture.duration_ms, None),
                initial_capture.failed(),
            )
        };
        process_failure |= !parsed.errors.is_empty()
            || !parsed.terminal_event
            || parsed.terminal_success != Some(true)
            || (entry.arm == ExperimentArm::P3 && parsed.tool_call_success != Some(true));
        if entry.arm == ExperimentArm::P0 && initial_parsed.thread_id.is_none() {
            process_failure = true;
        }
        had_failure |= process_failure;
        let stdout_text = capture.stdout_text();
        let stderr_text = capture.stderr_text();
        let stdout_digest = store
            .put(redact_jsonl(&stdout_text).as_bytes())
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        let stderr_digest = store
            .put(redact_jsonl(&stderr_text).as_bytes())
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        let initial_stdout_digest = store
            .put(redact_jsonl(&initial_stdout_text).as_bytes())
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        let initial_stderr_digest = store
            .put(redact_jsonl(&initial_stderr_text).as_bytes())
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        let telemetry_bytes = fs::read(plugin_data.join("telemetry.jsonl")).ok();
        let telemetry =
            telemetry_bytes.as_deref().map(extract_telemetry_profile).unwrap_or_default();
        let telemetry_digest = telemetry_bytes
            .as_deref()
            .map(|bytes| store.put(bytes))
            .transpose()
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        let mut usage = parsed.usage.clone();
        if usage.latency_ms.is_none() {
            usage.latency_ms = Some(duration_ms);
            usage.latency_precision = needle_bench::MetricPrecision::Partial;
        }
        let (compaction_events, compaction_precision) = merge_compaction_evidence(
            parsed.compaction_events,
            parsed.compaction_precision,
            &telemetry,
        );
        let mut extra = serde_json::Map::new();
        extra.insert("task_id".to_owned(), Value::String(task.id.clone()));
        extra.insert("profile".to_owned(), Value::String(profile.clone()));
        extra.insert("transport".to_owned(), Value::String(transport_name(entry.arm).to_owned()));
        extra.insert(
            "request_digest".to_owned(),
            Value::String(needle_core::Digest::blake3(&task_request).to_string()),
        );
        extra.insert("fixture_digest".to_owned(), Value::String(fixture_digest.to_string()));
        extra.insert("fixture_repo".to_owned(), Value::String(fixture_repo.display().to_string()));
        extra.insert(
            "repository_url".to_owned(),
            Value::String(BENCHMARK_REPOSITORY_URL.to_owned()),
        );
        extra.insert(
            "repository_sha".to_owned(),
            Value::String(BENCHMARK_REPOSITORY_SHA.to_owned()),
        );
        extra.insert("status".to_owned(), Value::String(capture.status_text()));
        extra.insert("duration_ms".to_owned(), Value::from(duration_ms));
        extra.insert("stdout_artifact".to_owned(), Value::String(stdout_digest.to_string()));
        extra.insert("stderr_artifact".to_owned(), Value::String(stderr_digest.to_string()));
        extra.insert(
            "initial_stdout_artifact".to_owned(),
            Value::String(initial_stdout_digest.to_string()),
        );
        extra.insert(
            "initial_stderr_artifact".to_owned(),
            Value::String(initial_stderr_digest.to_string()),
        );
        extra.insert("initial_status".to_owned(), Value::String(initial_capture.status_text()));
        extra.insert("process_failure".to_owned(), Value::Bool(process_failure));
        extra.insert("parser_errors".to_owned(), json!(parsed.errors));
        extra.insert("telemetry_observed".to_owned(), Value::Bool(telemetry.observed));
        extra
            .insert("telemetry_stream_complete".to_owned(), Value::Bool(telemetry.stream_complete));
        extra.insert("telemetry_stop_block".to_owned(), Value::Bool(telemetry.stop_block));
        extra.insert("pre_compact_events".to_owned(), Value::from(telemetry.pre_compact_events));
        extra.insert("post_compact_events".to_owned(), Value::from(telemetry.post_compact_events));
        if let Some(initial_usage) = initial_usage {
            extra.insert("initial_usage".to_owned(), serde_json::to_value(initial_usage)?);
        }
        if let Some(telemetry_digest) = telemetry_digest {
            extra.insert(
                "telemetry_artifact".to_owned(),
                Value::String(telemetry_digest.to_string()),
            );
        }
        if let Some(thread_id) = initial_parsed.thread_id {
            extra.insert("thread_id".to_owned(), Value::String(thread_id));
        }
        let observation = ExperimentObservation {
            arm: entry.arm,
            repetition: entry.repetition,
            task_seed: entry.task_seed,
            usage,
            prompt_profile_digest: parsed.prompt_profile_digest.or(telemetry.profile_digest),
            profile_payload_bytes: parsed.profile_payload_bytes.or(telemetry.profile_payload_bytes),
            mcp_startup_ms: parsed.mcp_startup_ms,
            compaction_events,
            compaction_precision,
            mcp_startup_precision: parsed.mcp_startup_precision,
            continuation_success: if process_failure {
                Some(false)
            } else {
                continuation_for_observation(entry.arm, &capture, &parsed, telemetry.stop_block)
            },
            artifact_digest: Some(stdout_digest),
            extra,
        };
        observations_jsonl.push_str(&serde_json::to_string(&observation)?);
        observations_jsonl.push('\n');
    }
    fs::write(artifact_root.join("observations.jsonl"), observations_jsonl)?;
    println!("experiment artifacts written to {}", artifact_root.display());
    if had_failure {
        Err(AppError::Experiment(
            "one or more observations failed process or structured-evidence checks; artifacts retained"
                .to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn deterministic_task_request(task_id: &str, task_seed: u64) -> String {
    format!(
        "@@need:trace.state-flow\nTrace the ripgrep `--glob-case-insensitive` execution path for benchmark task `{task_id}` seed `{task_seed}`.\n@@end"
    )
}

fn task_route(task: &TaskFixture) -> Result<NeedKey, AppError> {
    let value =
        task.extra.get("route").and_then(Value::as_str).unwrap_or("trace.state-flow").trim();
    NeedKey::new(value)
        .map_err(|error| AppError::Experiment(format!("invalid task route `{value}`: {error}")))
}

fn product_task_request(task_prompt: &str, route: &NeedKey) -> String {
    format!("@@need:{route}\n{}\n@@end", task_prompt.trim())
}

#[derive(Clone, Debug)]
struct ChildCapture {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    status: Option<ExitStatus>,
    timed_out: bool,
    spawn_error: Option<String>,
    abort_reason: Option<String>,
    duration_ms: u64,
}

impl ChildCapture {
    fn stdout_text(&self) -> String {
        fs::read(&self.stdout_path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }

    fn stderr_text(&self) -> String {
        fs::read(&self.stderr_path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }

    fn failed(&self) -> bool {
        self.timed_out
            || self.spawn_error.is_some()
            || self.abort_reason.is_some()
            || self.status.is_none_or(|status| !status.success())
    }

    fn status_text(&self) -> String {
        if let Some(error) = &self.spawn_error {
            return format!("spawn-error:{error}");
        }
        if self.timed_out {
            return "timeout".to_owned();
        }
        if let Some(reason) = &self.abort_reason {
            return format!("aborted:{reason}");
        }
        self.status
            .and_then(|status| status.code())
            .map(|code| format!("exit:{code}"))
            .unwrap_or_else(|| "signaled".to_owned())
    }

    fn process_status(&self) -> ProcessExecutionStatus {
        ProcessExecutionStatus {
            status: self.status_text(),
            spawn_error: self.spawn_error.clone(),
            exit_code: self.status.and_then(|status| status.code()),
            timed_out: self.timed_out,
            abort_reason: self.abort_reason.clone(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_child_capture(
    codex: &Path,
    arguments: &[String],
    current_dir: &Path,
    codex_home: &Path,
    plugin_data: &Path,
    output_dir: &Path,
    phase: &str,
    arm: ExperimentArm,
    timeout: Duration,
) -> Result<ChildCapture, AppError> {
    let stdout_path = output_dir.join(format!("{phase}-stdout.jsonl"));
    let stderr_path = output_dir.join(format!("{phase}-stderr.log"));
    let stdout = fs::File::create(&stdout_path)?;
    let stderr = fs::File::create(&stderr_path)?;
    let started = Instant::now();
    let mut command = Command::new(codex);
    command
        .args(arguments)
        .current_dir(current_dir)
        .env("CODEX_HOME", codex_home)
        .env("PLUGIN_DATA", plugin_data)
        .env("NEEDLE_LIVE_HARNESS", "1")
        .env("NEEDLE_EXPERIMENT_ARM", arm.as_str())
        .env("NEEDLE_BENCHMARK_PAYLOAD_FILE", plugin_data.join("benchmark-payload.txt"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ok(ChildCapture {
                stdout_path,
                stderr_path,
                status: None,
                timed_out: false,
                spawn_error: Some(error.to_string()),
                abort_reason: None,
                duration_ms: started.elapsed().as_millis() as u64,
            });
        }
    };
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok();
        }
        thread::sleep(Duration::from_millis(25));
    };
    Ok(ChildCapture {
        stdout_path,
        stderr_path,
        status,
        timed_out,
        spawn_error: None,
        abort_reason: None,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_product_child_capture(
    codex: &Path,
    arguments: &[String],
    current_dir: &Path,
    codex_home: &Path,
    plugin_data: &Path,
    product_data: Option<&Path>,
    output_dir: &Path,
    timeout: Duration,
) -> Result<ChildCapture, AppError> {
    let stdout_path = output_dir.join("main-stdout.jsonl");
    let stderr_path = output_dir.join("main-stderr.log");
    let stdout = fs::File::create(&stdout_path)?;
    let stderr = fs::File::create(&stderr_path)?;
    let started = Instant::now();
    let mut command = Command::new(codex);
    command
        .args(arguments)
        .current_dir(current_dir)
        .env("CODEX_HOME", codex_home)
        .env("PLUGIN_DATA", plugin_data)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(product_data) = product_data {
        let signal_path = product_data.join("pilot-signal.json");
        let outcome_path = product_data.join("pilot-outcome.json");
        let _ = fs::remove_file(&signal_path);
        let _ = fs::remove_file(&outcome_path);
        command
            .env("NEEDLE_DATA_DIR", product_data)
            .env("NEEDLE_PILOT_ROOT_TASK_FILE", product_data.join("pilot-root-task.txt"))
            .env("NEEDLE_PILOT_SIGNAL_FILE", &signal_path)
            .env("NEEDLE_PILOT_OUTCOME_FILE", &outcome_path);
        let oracle_path = product_data.join("pilot-oracle.json");
        if oracle_path.is_file() {
            command.env("NEEDLE_PILOT_ORACLE_FILE", oracle_path);
        }
        let test_plan_path = product_data.join("pilot-test-plan.json");
        if test_plan_path.is_file() {
            command.env("NEEDLE_PILOT_TEST_PLAN_FILE", test_plan_path);
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ok(ChildCapture {
                stdout_path,
                stderr_path,
                status: None,
                timed_out: false,
                spawn_error: Some(error.to_string()),
                abort_reason: None,
                duration_ms: started.elapsed().as_millis() as u64,
            });
        }
    };
    let mut timed_out = false;
    let mut abort_reason = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if let Some(product_data) = product_data {
            let signal_path = product_data.join("pilot-signal.json");
            if signal_path.is_file() {
                abort_reason = Some(
                    fs::read_to_string(&signal_path).unwrap_or_else(|_| "pilot_signal".to_owned()),
                );
                let _ = child.kill();
                break child.wait().ok();
            }
        }
        if started.elapsed() >= timeout {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok();
        }
        thread::sleep(Duration::from_millis(25));
    };
    Ok(ChildCapture {
        stdout_path,
        stderr_path,
        status,
        timed_out,
        spawn_error: None,
        abort_reason,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

fn prepare_pilot_source(artifact_root: &Path) -> Result<PathBuf, AppError> {
    let source = artifact_root.join("source");
    let mut clone = Command::new("git");
    clone
        .args(["clone", "--quiet", "--no-checkout", "--filter=blob:none", "--"])
        .arg(BENCHMARK_REPOSITORY_URL)
        .arg(&source);
    run_repository_command(&mut clone, "clone pilot repository")?;
    let mut checkout = Command::new("git");
    checkout.arg("-C").arg(&source).args([
        "checkout",
        "--quiet",
        "--detach",
        BENCHMARK_REPOSITORY_SHA,
    ]);
    run_repository_command(&mut checkout, "checkout pilot repository")?;
    Ok(source)
}

fn clone_local_checkout(source: &Path, destination: &Path) -> Result<PathBuf, AppError> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut clone = Command::new("git");
    clone.args(["clone", "--quiet", "--no-hardlinks", "--"]).arg(source).arg(destination);
    run_repository_command(&mut clone, "clone isolated pilot checkout")?;
    let mut checkout = Command::new("git");
    checkout.arg("-C").arg(destination).args([
        "checkout",
        "--quiet",
        "--detach",
        BENCHMARK_REPOSITORY_SHA,
    ]);
    run_repository_command(&mut checkout, "pin isolated pilot checkout")?;
    Ok(destination.to_path_buf())
}

fn run_pilot_evaluator_test(
    repository: &Path,
    focused_command: &str,
    timeout: Duration,
) -> Result<bool, AppError> {
    const EXPECTED: &str =
        "cargo test --test integration misc::glob_always_case_insensitive -- --exact";
    if focused_command != EXPECTED {
        return Err(AppError::Experiment(
            "pilot evaluator only accepts the pinned focused test command".to_owned(),
        ));
    }
    let stdout = fs::File::create(repository.parent().unwrap().join("test-stdout.log"))?;
    let stderr = fs::File::create(repository.parent().unwrap().join("test-stderr.log"))?;
    let mut child = Command::new("cargo")
        .args([
            "test",
            "--test",
            "integration",
            "misc::glob_always_case_insensitive",
            "--",
            "--exact",
        ])
        .current_dir(repository)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| AppError::Experiment(format!("spawn evaluator test: {error}")))?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn continuation_for_observation(
    arm: ExperimentArm,
    capture: &ChildCapture,
    parsed: &needle_bench::CodexParseResult,
    stop_block_telemetry: bool,
) -> Option<bool> {
    if capture.failed() {
        return Some(false);
    }
    terminal_continuation_for_arm(arm, parsed, stop_block_telemetry)
}

fn terminal_continuation_for_arm(
    arm: ExperimentArm,
    parsed: &needle_bench::CodexParseResult,
    stop_block_telemetry: bool,
) -> Option<bool> {
    if !parsed.terminal_event {
        return None;
    }
    if matches!(arm, ExperimentArm::P1 | ExperimentArm::P2 | ExperimentArm::P4)
        && !stop_block_telemetry
    {
        return Some(false);
    }
    if arm == ExperimentArm::P3 && parsed.tool_call_success != Some(true) {
        return Some(false);
    }
    parsed.continuation_success
}

fn observation_duration_ms(initial_ms: u64, follow_up_ms: Option<u64>) -> u64 {
    initial_ms.saturating_add(follow_up_ms.unwrap_or_default())
}

fn default_task_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../").join(BENCHMARK_TASK_FIXTURE)
}

fn prepare_benchmark_repository(artifact_root: &Path) -> Result<PathBuf, AppError> {
    let checkout = artifact_root.join("repository-checkout");
    let snapshot = artifact_root.join("repository-snapshot");
    if checkout.exists() || snapshot.exists() {
        return Err(AppError::Experiment(format!(
            "benchmark repository preparation path already exists under {}",
            artifact_root.display()
        )));
    }

    let mut clone = Command::new("git");
    clone
        .arg("clone")
        .arg("--quiet")
        .arg("--no-checkout")
        .arg("--filter=blob:none")
        .arg("--")
        .arg(BENCHMARK_REPOSITORY_URL)
        .arg(&checkout);
    run_repository_command(&mut clone, "clone benchmark repository")?;

    let mut checkout_command = Command::new("git");
    checkout_command
        .arg("-C")
        .arg(&checkout)
        .arg("checkout")
        .arg("--quiet")
        .arg("--detach")
        .arg(BENCHMARK_REPOSITORY_SHA);
    run_repository_command(&mut checkout_command, "checkout pinned benchmark commit")?;

    let mut revision = Command::new("git");
    revision.arg("-C").arg(&checkout).arg("rev-parse").arg("HEAD");
    let output = revision
        .output()
        .map_err(|error| AppError::Experiment(format!("verify benchmark commit: {error}")))?;
    if !output.status.success() {
        return Err(AppError::Experiment(format!(
            "verify benchmark commit failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let actual = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if actual != BENCHMARK_REPOSITORY_SHA {
        return Err(AppError::Experiment(format!(
            "benchmark commit mismatch: expected {BENCHMARK_REPOSITORY_SHA}, got {actual}"
        )));
    }

    let git_metadata = checkout.join(".git");
    if !git_metadata.is_dir() {
        return Err(AppError::Experiment(
            "cloned benchmark repository has no .git directory".to_owned(),
        ));
    }
    copy_tree_without_git(&checkout, &snapshot)?;
    if snapshot.join(".git").exists() {
        return Err(AppError::Experiment(
            "benchmark repository snapshot contains forbidden .git metadata".to_owned(),
        ));
    }
    fs::remove_dir_all(&checkout)?;
    if checkout.exists() {
        return Err(AppError::Experiment(
            "temporary benchmark checkout was not removed".to_owned(),
        ));
    }
    Ok(snapshot)
}

fn run_repository_command(command: &mut Command, description: &str) -> Result<(), AppError> {
    let output = command
        .output()
        .map_err(|error| AppError::Experiment(format!("{description}: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(AppError::Experiment(format!("{description} failed: {}", stderr.trim())))
}

fn digest_tree(root: &Path) -> Result<needle_core::Digest, AppError> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut bytes = Vec::new();
    for (relative, content) in files {
        bytes.extend_from_slice(relative.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&content);
        bytes.push(0);
    }
    Ok(needle_core::Digest::blake3(bytes))
}

fn collect_files(
    root: &Path,
    current: &Path,
    output: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), AppError> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, output)?;
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| AppError::Experiment(error.to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            output.push((relative, fs::read(path)?));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TelemetryEvidence {
    profile_digest: Option<needle_core::Digest>,
    profile_payload_bytes: Option<u64>,
    observed: bool,
    stream_complete: bool,
    stop_block: bool,
    pre_compact_events: u32,
    post_compact_events: u32,
}

fn extract_telemetry_profile(bytes: &[u8]) -> TelemetryEvidence {
    let mut evidence = TelemetryEvidence::default();
    let mut saw_known_event = false;
    let mut malformed_line = false;
    for line in String::from_utf8_lossy(bytes).lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            malformed_line = true;
            continue;
        };
        let event = &value["event"];
        match event["event"].as_str() {
            Some("SessionStart") => {
                saw_known_event = true;
                evidence.profile_digest = evidence.profile_digest.or_else(|| {
                    event["profile_digest"]
                        .as_str()
                        .and_then(|text| needle_core::Digest::parse(text).ok())
                });
                evidence.profile_payload_bytes = evidence
                    .profile_payload_bytes
                    .or_else(|| event["profile_payload_bytes"].as_u64());
            }
            Some("Stop") => {
                saw_known_event = true;
                evidence.stop_block |= event["decision"].as_str() == Some("block");
            }
            Some("PreCompact") => {
                saw_known_event = true;
                evidence.pre_compact_events = evidence.pre_compact_events.saturating_add(1);
            }
            Some("PostCompact") => {
                saw_known_event = true;
                evidence.post_compact_events = evidence.post_compact_events.saturating_add(1);
            }
            _ => {}
        }
    }
    evidence.observed = saw_known_event;
    evidence.stream_complete = saw_known_event && !malformed_line;
    evidence
}

fn merge_compaction_evidence(
    parsed_events: u32,
    parsed_precision: needle_bench::MetricPrecision,
    telemetry: &TelemetryEvidence,
) -> (u32, needle_bench::MetricPrecision) {
    let telemetry_cycles = telemetry.pre_compact_events.max(telemetry.post_compact_events);
    let precision = if (telemetry.observed && telemetry.stream_complete)
        || parsed_precision == needle_bench::MetricPrecision::Exact
    {
        needle_bench::MetricPrecision::Exact
    } else if telemetry_cycles > 0 {
        needle_bench::MetricPrecision::Partial
    } else {
        parsed_precision
    };
    (parsed_events.max(telemetry_cycles), precision)
}

fn arm_prompt(arm: ExperimentArm, task_prompt: &str, request: &str) -> String {
    match arm {
        ExperimentArm::P0 => format!(
            "[Needle P0 baseline normal follow-up control]\nContinue natively without Needle hooks, markers, or MCP tools.\n\nTask:\n{task_prompt}"
        ),
        ExperimentArm::P1 => format!(
            "[Needle P1 Stop static control]\nEmit exactly this @@need block, then stop and await the continuation:\n{request}\n\nTask:\n{task_prompt}"
        ),
        ExperimentArm::P2 => format!(
            "[Needle P2 Stop generated control]\nEmit exactly this @@need block, then stop and await generated context:\n{request}\n\nTask:\n{task_prompt}"
        ),
        ExperimentArm::P3 => format!(
            "[Needle P3 static MCP tool control]\nCall exactly the need_context tool with request string exactly equal to:\n{request}\nDo not emit @@need.\n\nTask:\n{task_prompt}"
        ),
        ExperimentArm::P4 => format!(
            "[Needle P4 repeated-payload control]\nEmit exactly this @@need block, then stop:\n{request}\n\nTask:\n{task_prompt}"
        ),
    }
}

fn transport_name(arm: ExperimentArm) -> &'static str {
    match arm {
        ExperimentArm::P0 => "baseline-normal-follow-up",
        ExperimentArm::P1 => "stop-static",
        ExperimentArm::P2 => "stop-generated",
        ExperimentArm::P3 => "mcp-tool",
        ExperimentArm::P4 => "stop-repeated-payload",
    }
}

#[allow(dead_code)]
fn build_codex_args(
    model: &str,
    reasoning: &str,
    service_tier: &str,
    profile: &str,
    prompt: &str,
) -> Result<Vec<String>, AppError> {
    build_codex_initial_args(model, reasoning, service_tier, profile, prompt, Path::new("."), true)
}

fn build_codex_initial_args(
    model: &str,
    reasoning: &str,
    service_tier: &str,
    profile: &str,
    prompt: &str,
    fixture_repo: &Path,
    include_ephemeral: bool,
) -> Result<Vec<String>, AppError> {
    validate_model_value(model, "model")?;
    validate_reasoning(reasoning)?;
    validate_service_tier(service_tier)?;
    validate_slug(profile, "profile")?;
    let mut args = vec!["exec".to_owned(), "--json".to_owned()];
    if include_ephemeral {
        args.push("--ephemeral".to_owned());
    }
    args.extend([
        "--dangerously-bypass-hook-trust".to_owned(),
        "--sandbox".to_owned(),
        "read-only".to_owned(),
        "--cd".to_owned(),
        fixture_repo.display().to_string(),
        "--model".to_owned(),
        model.to_owned(),
        "-c".to_owned(),
        format!("model_reasoning_effort=\"{reasoning}\""),
        "-c".to_owned(),
        format!("service_tier=\"{service_tier}\""),
        "--profile".to_owned(),
        profile.to_owned(),
        "--".to_owned(),
        prompt.to_owned(),
    ]);
    Ok(args)
}

fn build_codex_resume_args(thread_id: &str, payload: &str) -> Result<Vec<String>, AppError> {
    validate_slug(thread_id, "thread id")?;
    Ok(vec![
        "exec".to_owned(),
        "resume".to_owned(),
        "--json".to_owned(),
        "--disable".to_owned(),
        "hooks".to_owned(),
        thread_id.to_owned(),
        "--".to_owned(),
        payload.to_owned(),
    ])
}

fn validate_model_value(value: &str, label: &str) -> Result<(), AppError> {
    validate_slug(value, label)
}

#[allow(clippy::too_many_arguments)]
fn provision_experiment_role_profile(
    store: &RuntimeStore,
    profile_id: &str,
    prompt_profile_digest: Digest,
    model: &str,
    reasoning: &str,
    service_tier: &str,
    timeout_seconds: u64,
    repair_once: bool,
) -> Result<RoleProfileId, AppError> {
    let reasoning = match reasoning {
        "low" => ReasoningLevel::Low,
        "medium" => ReasoningLevel::Medium,
        "high" => ReasoningLevel::High,
        "xhigh" => ReasoningLevel::Xhigh,
        value => {
            return Err(AppError::Experiment(format!(
                "experiment role profile cannot represent reasoning `{value}`"
            )));
        }
    };
    let service_tier = match service_tier {
        "default" => ServiceTier::Default,
        "priority" => ServiceTier::Priority,
        value => {
            return Err(AppError::Experiment(format!(
                "experiment role profile cannot represent service tier `{value}`"
            )));
        }
    };
    let profile_id =
        RoleProfileId::new(profile_id).map_err(|error| AppError::Experiment(error.to_string()))?;
    let definition = RoleProfileDefinition::new(RoleProfileDefinitionInput {
        profile_id: profile_id.clone(),
        role: CodexRole::Explorer,
        host: CodexHost::Codex,
        model: model.to_owned(),
        reasoning,
        service_tier,
        timeout_seconds,
        budget: RoleProfileBudget {
            max_turns: 8,
            max_output_tokens: 2000,
            max_cost_microusd: 1_000_000_000,
        },
        prompt_profile_digest,
        output_contract_digest: Digest::blake3(needle_core::ARTIFACT_RESULT_SCHEMA_ID),
        tool_policy: ToolPolicy::ReadOnly,
        command_policy: CommandPolicy::ReadOnly,
        filesystem_policy: FilesystemPolicy::ReadOnlyCheckout,
        network_policy: NetworkPolicy::Denied,
        test_policy: TestPolicy::Disabled,
        repair_policy: if repair_once { RepairPolicy::Once } else { RepairPolicy::None },
        fallback_policy: FallbackPolicy::Disabled,
        concurrency: 1,
        route_assignments: Vec::new(),
    })
    .map_err(|error| AppError::Experiment(error.to_string()))?;
    let revision = store
        .create_role_profile(definition)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let state = store
        .role_profile_state(&profile_id)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    store
        .activate_role_profile(&profile_id, revision.revision, state.state_digest)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    Ok(profile_id)
}

fn parse_experiment_arm(value: &str) -> Result<ExperimentArm, AppError> {
    match value {
        "P0" => Ok(ExperimentArm::P0),
        "P1" => Ok(ExperimentArm::P1),
        "P2" => Ok(ExperimentArm::P2),
        "P3" => Ok(ExperimentArm::P3),
        "P4" => Ok(ExperimentArm::P4),
        _ => Err(AppError::Usage("--only-arm must be P0|P1|P2|P3|P4".to_owned())),
    }
}

fn validate_reasoning(value: &str) -> Result<(), AppError> {
    if ["minimal", "low", "medium", "high", "xhigh"].contains(&value) {
        Ok(())
    } else {
        Err(AppError::Usage("reasoning must be minimal|low|medium|high|xhigh".to_owned()))
    }
}

fn parse_evidence_failure_policy(value: &str) -> Result<EvidenceFailurePolicy, AppError> {
    match value {
        "discard_invalid_fact" => Ok(EvidenceFailurePolicy::DiscardInvalidFact),
        "repair_once" => Ok(EvidenceFailurePolicy::RepairOnce),
        _ => Err(AppError::Usage(
            "--evidence-failure-policy must be discard_invalid_fact or repair_once".to_owned(),
        )),
    }
}

fn pilot_test_plan(oracle: &QualityOracleSpec) -> Result<TestPlan, String> {
    let argv = parse_direct_argv(&oracle.focused_test_command)
        .map_err(|error| format!("pilot focused test is not a direct argv: {error}"))?;
    if argv.first().map(String::as_str) != Some("cargo")
        || argv.get(1).map(String::as_str) != Some("test")
        || !argv.windows(2).any(|pair| pair == ["--", "--exact"])
    {
        return Err("pilot focused test must be a direct exact cargo test".to_owned());
    }
    let identifiers =
        argv.iter().filter(|argument| argument.contains("::")).cloned().collect::<Vec<_>>();
    if identifiers.len() != 1 {
        return Err(
            "pilot focused test must contain exactly one qualified test identifier".to_owned()
        );
    }
    Ok(TestPlan {
        runner: "cargo".to_owned(),
        argv,
        cwd_relative: ".".to_owned(),
        test_identifier: identifiers[0].clone(),
        requires_approval: true,
        execution_evidence_id: None,
    })
}

fn validate_service_tier(value: &str) -> Result<(), AppError> {
    if ["auto", "default", "flex", "priority"].contains(&value) {
        Ok(())
    } else {
        Err(AppError::Usage("service tier must be auto|default|flex|priority".to_owned()))
    }
}

fn validate_slug(value: &str, label: &str) -> Result<(), AppError> {
    if !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(AppError::Usage(format!("{label} must be a conservative ASCII slug")))
    }
}

fn required_value(arguments: &[String], name: &str) -> Result<String, AppError> {
    option_value(arguments, name).ok_or_else(|| AppError::Usage(format!("missing {name}")))
}

fn required_path(arguments: &[String], name: &str, description: &str) -> Result<PathBuf, AppError> {
    let value = required_value(arguments, name)?;
    let path = PathBuf::from(value);
    if !path.is_dir() && name != "--tasks" {
        fs::create_dir_all(&path)?;
    }
    if name == "--tasks" && !path.is_file() {
        return Err(AppError::Usage(format!("{description} must be a file")));
    }
    Ok(path)
}

fn profile_value(arguments: &[String], names: &[&str]) -> Result<String, AppError> {
    names
        .iter()
        .find_map(|name| option_value(arguments, name))
        .ok_or_else(|| AppError::Usage(format!("missing profile name ({})", names.join(" or "))))
}

fn resolve_codex(value: Option<String>) -> Result<PathBuf, AppError> {
    if let Some(path) = value {
        let path = PathBuf::from(path);
        reject_shell_launcher(&path)?;
        if path.is_file()
            && Command::new(&path)
                .arg("--version")
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
        {
            return Ok(path);
        }
        return Err(AppError::Usage(format!(
            "--codex is not an executable file: {}",
            path.display()
        )));
    }
    if let Some(path) = managed_codex_executable() {
        return Ok(path);
    }
    let probe = Command::new("codex").arg("--version").output();
    match probe {
        Ok(output) if output.status.success() => Ok(PathBuf::from("codex")),
        _ => Err(AppError::Runtime(
            "Needle's managed Codex runtime is missing; reinstall Needle or repair the installation"
                .to_owned(),
        )),
    }
}

fn managed_codex_executable() -> Option<PathBuf> {
    let current_executable = env::current_exe().ok()?;
    let candidate = managed_codex_candidate(&current_executable)?;
    if !managed_codex_package_is_complete(&candidate) {
        return None;
    }
    let output = Command::new(&candidate).arg("--version").output().ok()?;
    output.status.success().then_some(candidate)
}

fn managed_codex_candidate(needle_executable: &Path) -> Option<PathBuf> {
    let file_name = if cfg!(windows) { "codex.exe" } else { "codex" };
    Some(needle_executable.parent()?.join("runtime").join("bin").join(file_name))
}

fn managed_codex_package_is_complete(codex_executable: &Path) -> bool {
    if !codex_executable.is_file() {
        return false;
    }
    let Some(runtime_root) = codex_executable.parent().and_then(Path::parent) else {
        return false;
    };
    if !runtime_root.join("codex-package.json").is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        for relative_path in [
            "bin/codex-code-mode-host.exe",
            "codex-path/rg.exe",
            "codex-resources/codex-command-runner.exe",
            "codex-resources/codex-windows-sandbox-setup.exe",
        ] {
            if !runtime_root.join(relative_path).is_file() {
                return false;
            }
        }
    }
    true
}

fn reject_shell_launcher(path: &Path) -> Result<(), AppError> {
    let extension = path.extension().and_then(|value| value.to_str()).unwrap_or_default();
    let file_stem = path.file_stem().and_then(|value| value.to_str()).unwrap_or_default();
    let script_extension = matches!(
        extension.to_ascii_lowercase().as_str(),
        "bat" | "cmd" | "ps1" | "sh" | "bash" | "zsh" | "fish" | "command"
    );
    let shell_executable = matches!(
        file_stem.to_ascii_lowercase().as_str(),
        "cmd" | "powershell" | "pwsh" | "sh" | "bash" | "zsh" | "fish"
    );
    if script_extension || shell_executable {
        return Err(AppError::Usage(format!(
            "Codex launcher must be a native executable, not a shell or script: {}; \
             provide the platform Codex binary directly",
            path.display()
        )));
    }
    Ok(())
}

fn ensure_dedicated_codex_home(path: &Path) -> Result<(), AppError> {
    if !path.is_dir() {
        return Err(AppError::Usage(format!(
            "--codex-home must be an existing directory: {}",
            path.display()
        )));
    }
    let candidate = fs::canonicalize(path)?;
    let mut personal = Vec::new();
    if let Some(value) = env::var_os("CODEX_HOME") {
        personal.push(PathBuf::from(value));
    }
    if let Some(value) = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")) {
        personal.push(PathBuf::from(value).join(".codex"));
    }
    if personal.iter().filter_map(|path| fs::canonicalize(path).ok()).any(|path| path == candidate)
    {
        return Err(AppError::Usage(
            "--codex-home resolves to personal CODEX_HOME; use a dedicated directory".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_codex_authenticated(
    codex: &Path,
    codex_home: &Path,
    experiment: &str,
) -> Result<(), AppError> {
    let auth = codex_home.join("auth.json");
    if !auth.is_file() {
        return Err(AppError::Experiment(format!(
            "{experiment} Codex home has no local authentication file: {}",
            auth.display()
        )));
    }
    let status = Command::new(codex)
        .args(["login", "status"])
        .env("CODEX_HOME", codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(AppError::Experiment(format!(
            "{experiment} Codex home is not authenticated according to `codex login status`: {}",
            codex_home.display()
        )));
    }
    Ok(())
}

fn ensure_product_pilot_hook_isolation(path: &Path) -> Result<(), AppError> {
    let config_path = path.join("config.toml");
    if config_path.is_file() {
        let config_text = fs::read_to_string(&config_path)?;
        let config: toml::Value = toml::from_str(&config_text).map_err(|error| {
            AppError::Experiment(format!(
                "cannot parse dedicated Codex config {}: {error}",
                config_path.display()
            ))
        })?;
        if let Some(plugins) = config.get("plugins").and_then(toml::Value::as_table) {
            let configured = plugins
                .iter()
                .filter_map(|(id, value)| {
                    let explicitly_disabled =
                        value.get("enabled").and_then(toml::Value::as_bool) == Some(false);
                    (!explicitly_disabled).then_some(id.as_str())
                })
                .collect::<Vec<_>>();
            if !configured.is_empty() {
                return Err(AppError::Experiment(format!(
                    "product pilot Codex home has enabled or implicitly enabled plugins: {}; use a clean dedicated home so bundled hooks cannot race the pilot hook",
                    configured.join(", ")
                )));
            }
        }
    }

    let hooks_path = path.join("hooks.json");
    if !hooks_path.is_file() {
        return Err(AppError::Experiment(format!(
            "product pilot requires one explicit global hook set: {} is missing",
            hooks_path.display()
        )));
    }
    let hooks: Value = serde_json::from_slice(&fs::read(&hooks_path)?)?;
    for event in ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"] {
        let count = command_hook_count(&hooks, event);
        if count != 1 {
            return Err(AppError::Experiment(format!(
                "product pilot requires exactly one global {event} command hook, found {count}"
            )));
        }
    }
    Ok(())
}

fn ensure_cache_pilot_hook_binary(path: &Path) -> Result<(), AppError> {
    let hooks_path = path.join("hooks.json");
    let hooks: Value = serde_json::from_slice(&fs::read(&hooks_path)?)?;
    let current_executable = fs::canonicalize(env::current_exe()?)?;
    let current_digest = Digest::blake3(fs::read(&current_executable)?);
    for (event, hook_subcommand) in [
        ("SessionStart", "session-start"),
        ("UserPromptSubmit", "user-prompt-submit"),
        ("Stop", "stop"),
        ("SessionEnd", "session-end"),
    ] {
        let hook = hooks
            .get("hooks")
            .and_then(|value| value.get(event))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|group| group.get("hooks").and_then(Value::as_array))
            .flatten()
            .find(|hook| hook.get("type").and_then(Value::as_str) == Some("command"))
            .ok_or_else(|| {
                AppError::Experiment(format!("cache pilot has no {event} command hook"))
            })?;
        let command_key = if cfg!(windows) { "commandWindows" } else { "command" };
        let command = hook
            .get(command_key)
            .or_else(|| hook.get("command"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AppError::Experiment(format!("cache pilot {event} hook has no command"))
            })?;
        let executable =
            cache_pilot_hook_executable(command, hook_subcommand).ok_or_else(|| {
                AppError::Experiment(format!(
                    "cache pilot {event} hook must be exactly `needle hook {hook_subcommand}`"
                ))
            })?;
        reject_shell_launcher(&executable)?;
        let executable = fs::canonicalize(&executable).map_err(|error| {
            AppError::Experiment(format!(
                "cache pilot {event} hook executable {} is unavailable: {error}",
                executable.display()
            ))
        })?;
        let hook_digest = Digest::blake3(fs::read(&executable)?);
        if hook_digest != current_digest {
            return Err(AppError::Experiment(format!(
                "cache pilot {event} hook uses an outdated Needle binary {}; package the current binary before running",
                executable.display()
            )));
        }
    }
    Ok(())
}

fn cache_pilot_hook_executable(command: &str, hook_subcommand: &str) -> Option<PathBuf> {
    let executable = command.trim().strip_suffix(&format!(" hook {hook_subcommand}"))?.trim();
    let executable = if executable.starts_with('"') && executable.ends_with('"') {
        &executable[1..executable.len().checked_sub(1)?]
    } else {
        if executable.chars().any(char::is_whitespace) {
            return None;
        }
        executable
    };
    if executable.is_empty()
        || executable.chars().any(|character| {
            character.is_control() || matches!(character, '"' | '\'' | '|' | '&' | ';' | '<' | '>')
        })
    {
        return None;
    }
    Some(PathBuf::from(executable))
}

fn command_hook_count(hooks: &Value, event: &str) -> usize {
    hooks
        .get("hooks")
        .and_then(|value| value.get(event))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter(|hook| hook.get("type").and_then(Value::as_str) == Some("command"))
        .count()
}

fn load_reusable_p0(root: &Path) -> Result<(ProductRunManifest, ProductObservation), AppError> {
    let manifests_path = root.join("run-manifests.json");
    let observations_path = root.join("product-observations.jsonl");
    let manifests: Vec<ProductRunManifest> =
        serde_json::from_slice(&fs::read(&manifests_path).map_err(|error| {
            AppError::Experiment(format!(
                "cannot read reusable P0 manifests {}: {error}",
                manifests_path.display()
            ))
        })?)?;
    let manifest =
        manifests.into_iter().find(|manifest| manifest.arm == ProductArm::P0).ok_or_else(|| {
            AppError::Experiment(format!(
                "reusable pilot has no P0 manifest: {}",
                manifests_path.display()
            ))
        })?;
    let observations = fs::read_to_string(&observations_path).map_err(|error| {
        AppError::Experiment(format!(
            "cannot read reusable P0 observation {}: {error}",
            observations_path.display()
        ))
    })?;
    let observation = observations
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<ProductObservation>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|observation| observation.arm == ProductArm::P0)
        .ok_or_else(|| {
            AppError::Experiment(format!(
                "reusable pilot has no P0 observation: {}",
                observations_path.display()
            ))
        })?;
    Ok((manifest, observation))
}

fn experiment_report(arguments: &[String]) -> Result<(), AppError> {
    let Some(path) = arguments.first() else {
        return Err(AppError::Usage("experiment report <jsonl> [--output path]".to_owned()));
    };
    let input = fs::read_to_string(path)?;
    let parsed = parse_jsonl(&input);
    let report = ExperimentReport::from_observations(&parsed.observations, parsed.errors.len());
    let output = report.to_json()?;
    if let Some(destination) = option_value(arguments, "--output") {
        fs::write(destination, output.as_bytes())?;
    } else {
        println!("{output}");
    }
    Ok(())
}

fn product_report(arguments: &[String]) -> Result<(), AppError> {
    let Some(path) = arguments.first() else {
        return Err(AppError::Usage(
            "experiment product-report <product-observations.jsonl> [--output path]".to_owned(),
        ));
    };
    let input = fs::read_to_string(path)?;
    let mut observations = Vec::new();
    for (index, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        observations.push(serde_json::from_str::<ProductObservation>(line).map_err(|error| {
            AppError::Experiment(format!("product observation line {}: {error}", index + 1))
        })?);
    }
    let verdict = ProductVerdict::evaluate(&observations);
    let output = serde_json::to_string_pretty(&verdict)?;
    if let Some(destination) = option_value(arguments, "--output") {
        fs::write(destination, output)?;
    } else {
        println!("{output}");
    }
    Ok(())
}

fn final_gate_report(arguments: &[String]) -> Result<(), AppError> {
    let Some(input) = arguments.first() else {
        return Err(AppError::Usage(
            "experiment final-report <final-observations.jsonl> --corpus <frozen-corpus.json> [--bootstrap-resamples N] [--seed N] [--output path]"
                .to_owned(),
        ));
    };
    let corpus_path = PathBuf::from(required_value(arguments, "--corpus")?);
    let manifest_bytes = read_bounded_file(&corpus_path, MAX_CORPUS_MANIFEST_BYTES)?;
    let manifest =
        serde_json::from_slice::<FrozenCorpusManifest>(&manifest_bytes).map_err(|error| {
            AppError::Experiment(format!("invalid frozen corpus manifest: {error}"))
        })?;
    let manifest_errors = validate_frozen_manifest(&manifest);
    if !manifest_errors.is_empty() {
        return Err(AppError::Experiment(format!(
            "frozen corpus manifest is invalid: {}",
            manifest_errors.join("; ")
        )));
    }
    let corpus_root = corpus_path.parent().unwrap_or_else(|| Path::new("."));
    let campaign_path = manifest
        .campaign_path
        .as_deref()
        .ok_or_else(|| AppError::Experiment("frozen corpus has no campaign path".to_owned()))?;
    let schedule_path = manifest
        .schedule_path
        .as_deref()
        .ok_or_else(|| AppError::Experiment("frozen corpus has no schedule path".to_owned()))?;
    let power_plan_path = manifest
        .power_plan_path
        .as_deref()
        .ok_or_else(|| AppError::Experiment("frozen corpus has no power-plan path".to_owned()))?;
    let campaign_bytes =
        read_bounded_file(&corpus_root.join(campaign_path), MAX_CORPUS_MANIFEST_BYTES)?;
    let schedule_bytes = read_bounded_file(&corpus_root.join(schedule_path), MAX_SCHEDULE_BYTES)?;
    let power_plan_bytes =
        read_bounded_file(&corpus_root.join(power_plan_path), MAX_SCHEDULE_BYTES)?;
    let campaign = serde_json::from_slice::<MultiTaskCampaign>(&campaign_bytes)
        .map_err(|error| AppError::Experiment(format!("invalid campaign: {error}")))?;
    let schedule = serde_json::from_slice::<CorpusSchedule>(&schedule_bytes)
        .map_err(|error| AppError::Experiment(format!("invalid corpus schedule: {error}")))?;
    let power_plan = serde_json::from_slice::<PowerPlan>(&power_plan_bytes)
        .map_err(|error| AppError::Experiment(format!("invalid power plan: {error}")))?;
    let campaign_digest = raw_digest(&campaign_bytes);
    let schedule_digest = raw_digest(&schedule_bytes);
    let power_plan_digest = raw_digest(&power_plan_bytes);
    let resamples = option_value(arguments, "--bootstrap-resamples")
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid bootstrap resample count: {error}")))?
        .unwrap_or(10_000);
    if !(MIN_BOOTSTRAP_RESAMPLES..=MAX_BOOTSTRAP_RESAMPLES).contains(&resamples) {
        return Err(AppError::Usage(
            "--bootstrap-resamples must be between 1000 and 1000000".to_owned(),
        ));
    }
    let seed = option_value(arguments, "--seed")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid bootstrap seed: {error}")))?
        .unwrap_or(42);
    let observation_bytes = read_bounded_file(Path::new(input), MAX_FINAL_OBSERVATION_BYTES)?;
    let observation_text = String::from_utf8(observation_bytes).map_err(|error| {
        AppError::Experiment(format!("final observations are not UTF-8: {error}"))
    })?;
    let observations = observation_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<FinalObservation>)
        .collect::<Result<Vec<_>, _>>()?;
    let rendered = serde_json::to_string_pretty(&evaluate_final_gate(
        FinalGateContract {
            manifest: &manifest,
            campaign: &campaign,
            schedule: &schedule,
            power_plan: &power_plan,
            campaign_digest: &campaign_digest,
            schedule_digest: &schedule_digest,
            power_plan_digest: &power_plan_digest,
        },
        &observations,
        BootstrapConfig { resamples, seed },
    ))?;
    if let Some(path) = option_value(arguments, "--output") {
        fs::write(path, rendered)?;
    } else {
        println!("{rendered}");
    }
    Ok(())
}

fn power_plan_report(arguments: &[String]) -> Result<(), AppError> {
    let Some(input) = arguments.first() else {
        return Err(AppError::Usage(
            "experiment power-plan <calibration-observations.jsonl> --corpus <frozen-corpus.json> --campaign <campaign.json> [--output path]"
                .to_owned(),
        ));
    };
    let corpus_path = PathBuf::from(required_value(arguments, "--corpus")?);
    let campaign_path = PathBuf::from(required_value(arguments, "--campaign")?);
    let manifest_bytes = read_bounded_file(&corpus_path, MAX_CORPUS_MANIFEST_BYTES)?;
    let manifest =
        serde_json::from_slice::<FrozenCorpusManifest>(&manifest_bytes).map_err(|error| {
            AppError::Experiment(format!("invalid frozen corpus manifest: {error}"))
        })?;
    let campaign_bytes = read_bounded_file(&campaign_path, MAX_CORPUS_MANIFEST_BYTES)?;
    if manifest.campaign_digest.as_deref() != Some(raw_digest(&campaign_bytes).as_str()) {
        return Err(AppError::Experiment(
            "campaign bytes differ from the frozen manifest".to_owned(),
        ));
    }
    let campaign = serde_json::from_slice::<MultiTaskCampaign>(&campaign_bytes)
        .map_err(|error| AppError::Experiment(format!("invalid campaign: {error}")))?;
    let observation_bytes = read_bounded_file(Path::new(input), MAX_CALIBRATION_INPUT_BYTES)?;
    let observation_text = String::from_utf8(observation_bytes).map_err(|error| {
        AppError::Experiment(format!("calibration observations are not UTF-8: {error}"))
    })?;
    let observations = observation_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<CalibrationObservation>)
        .collect::<Result<Vec<_>, _>>()?;
    let report = plan_power(&manifest, &campaign, &observations);
    let rendered = serde_json::to_string_pretty(&report)?;
    if let Some(path) = option_value(arguments, "--output") {
        fs::write(path, rendered)?;
    } else {
        println!("{rendered}");
    }
    Ok(())
}

fn option_value(arguments: &[String], name: &str) -> Option<String> {
    arguments.windows(2).find(|pair| pair[0] == name).map(|pair| pair[1].clone())
}

fn run_plugin(arguments: Vec<String>) -> Result<(), AppError> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err(AppError::Usage("plugin validate|package".to_owned()));
    };
    match action {
        "validate" => {
            if arguments.iter().any(|argument| argument == "--benchmark") {
                validate_benchmark_plugin(Path::new("benchmark-plugin"))?;
                println!("benchmark plugin validation passed");
            } else {
                validate_plugin(Path::new("plugin"))?;
                println!("product plugin validation passed");
            }
            Ok(())
        }
        "package" => {
            let output = option_value(&arguments[1..], "--output")
                .map(PathBuf::from)
                .ok_or_else(|| AppError::Usage("plugin package --output <directory>".to_owned()))?;
            if arguments.iter().any(|argument| argument == "--benchmark") {
                package_benchmark_plugin(Path::new("benchmark-plugin"), &output)
            } else {
                package_plugin(Path::new("plugin"), &output)
            }
        }
        _ => Err(AppError::Usage("plugin validate|package".to_owned())),
    }
}

fn validate_plugin(root: &Path) -> Result<(), AppError> {
    let manifest_path = root.join(".codex-plugin/plugin.json");
    let hooks_path = root.join("hooks/hooks.json");
    for path in [&manifest_path, &hooks_path] {
        if !path.is_file() {
            return Err(AppError::Plugin(format!("missing {}", path.display())));
        }
    }
    let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let object = manifest
        .as_object()
        .ok_or_else(|| AppError::Plugin("plugin.json must contain an object".to_owned()))?;
    let allowed = ["name", "version", "description", "author", "interface"];
    if let Some(field) = object.keys().find(|field| !allowed.contains(&field.as_str())) {
        return Err(AppError::Plugin(format!("manifest field `{field}` is not accepted")));
    }
    for field in ["name", "version", "description", "author", "interface"] {
        if manifest.get(field).is_none() {
            return Err(AppError::Plugin(format!("manifest missing `{field}`")));
        }
    }
    let name = manifest["name"]
        .as_str()
        .ok_or_else(|| AppError::Plugin("manifest name must be a string".to_owned()))?;
    if name.is_empty()
        || name.chars().any(|character| {
            !(character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
        })
    {
        return Err(AppError::Plugin("manifest name must be lowercase hyphen-case".to_owned()));
    }
    let version = manifest["version"]
        .as_str()
        .ok_or_else(|| AppError::Plugin("manifest version must be a string".to_owned()))?;
    if !is_semver(version) {
        return Err(AppError::Plugin("manifest version must be strict semver".to_owned()));
    }
    if manifest["description"].as_str().is_none()
        || manifest["description"].as_str().unwrap().trim().is_empty()
    {
        return Err(AppError::Plugin("manifest description must be non-empty".to_owned()));
    }
    let author = manifest["author"]
        .as_object()
        .ok_or_else(|| AppError::Plugin("manifest author must be an object".to_owned()))?;
    if author.keys().any(|field| !["name", "email", "url"].contains(&field.as_str()))
        || author.get("name").and_then(Value::as_str).is_none_or(str::is_empty)
        || author.get("email").is_some_and(|value| value.as_str().is_none())
        || author.get("url").is_some_and(|value| value.as_str().is_none())
    {
        return Err(AppError::Plugin(
            "manifest author requires a non-empty name and string metadata".to_owned(),
        ));
    }
    validate_interface(&manifest["interface"])?;
    if manifest.get("mcpServers").is_some() || root.join(".mcp.json").exists() {
        return Err(AppError::Plugin(
            "product plugin must not contain MCP configuration".to_owned(),
        ));
    }
    if manifest.get("hooks").is_some() {
        return Err(AppError::Plugin("hooks must be discovered from hooks/hooks.json".to_owned()));
    }
    let hooks: Value = serde_json::from_slice(&fs::read(&hooks_path)?)?;
    let hook_map = hooks
        .get("hooks")
        .and_then(Value::as_object)
        .ok_or_else(|| AppError::Plugin("hooks.json requires an object named hooks".to_owned()))?;
    let expected_events =
        ["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd", "PreCompact", "PostCompact"];
    if hook_map.keys().any(|event| !expected_events.contains(&event.as_str())) {
        return Err(AppError::Plugin("hooks.json contains an unsupported event".to_owned()));
    }
    for event in expected_events {
        if !hook_map.contains_key(event) {
            return Err(AppError::Plugin(format!("hooks.json missing {event}")));
        }
        let mut commands = Vec::new();
        collect_commands(&hook_map[event], &mut commands);
        let expected_hook = match event {
            "SessionStart" => "session-start",
            "UserPromptSubmit" => "user-prompt-submit",
            "Stop" => "stop",
            "SessionEnd" => "session-end",
            "PreCompact" => "pre-compact",
            "PostCompact" => "post-compact",
            _ => unreachable!(),
        };
        let expected_command = format!("hook {expected_hook}");
        if commands.is_empty()
            || !commands.iter().any(|command| command.contains(&expected_command))
        {
            return Err(AppError::Plugin(format!("hooks.json has wrong command for {event}")));
        }
    }
    Ok(())
}

fn validate_benchmark_plugin(root: &Path) -> Result<(), AppError> {
    let manifest_path = root.join(".codex-plugin/plugin.json");
    let mcp_path = root.join(".mcp.json");
    for path in [&manifest_path, &mcp_path] {
        if !path.is_file() {
            return Err(AppError::Plugin(format!("missing {}", path.display())));
        }
    }
    let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    if manifest.get("name").and_then(Value::as_str) != Some("needle-benchmark")
        || manifest.get("mcpServers").and_then(Value::as_str) != Some("./.mcp.json")
    {
        return Err(AppError::Plugin(
            "benchmark manifest must be needle-benchmark with ./.mcp.json".to_owned(),
        ));
    }
    let mcp: Value = serde_json::from_slice(&fs::read(&mcp_path)?)?;
    if mcp.as_object().is_none_or(|object| object.keys().any(|field| field != "mcpServers")) {
        return Err(AppError::Plugin(".mcp.json must contain only mcpServers".to_owned()));
    }
    let servers = mcp.get("mcpServers").and_then(Value::as_object).ok_or_else(|| {
        AppError::Plugin(".mcp.json requires mcpServers (local Codex schema)".to_owned())
    })?;
    if servers.len() != 1 {
        return Err(AppError::Plugin("exactly one benchmark MCP server is required".to_owned()));
    }
    let (server_name, server) = servers.iter().next().expect("one server");
    if server_name != "needle-benchmark" {
        return Err(AppError::Plugin("MCP server must be named needle-benchmark".to_owned()));
    }
    if server.get("enabled") != Some(&Value::Bool(false)) {
        return Err(AppError::Plugin(
            "benchmark MCP server must be disabled by default".to_owned(),
        ));
    }
    if let Some(server_object) = server.as_object() {
        if server_object
            .keys()
            .any(|field| !["enabled", "command", "args"].contains(&field.as_str()))
        {
            return Err(AppError::Plugin("MCP server contains an unsupported field".to_owned()));
        }
    } else {
        return Err(AppError::Plugin("MCP server must be an object".to_owned()));
    }
    let command = server
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Plugin("MCP server command must be a string".to_owned()))?;
    if !is_relative_plugin_command(command) {
        return Err(AppError::Plugin(
            "MCP command must be relative or ${PLUGIN_ROOT}-resolved".to_owned(),
        ));
    }
    let args = server
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| AppError::Plugin("MCP server args must be an array".to_owned()))?;
    if args != &[Value::String("mcp".to_owned()), Value::String("serve-benchmark".to_owned())] {
        return Err(AppError::Plugin("MCP args must be mcp serve-benchmark".to_owned()));
    }
    Ok(())
}

fn validate_interface(interface: &Value) -> Result<(), AppError> {
    let object = interface
        .as_object()
        .ok_or_else(|| AppError::Plugin("manifest interface must be an object".to_owned()))?;
    let required = [
        "displayName",
        "shortDescription",
        "longDescription",
        "developerName",
        "category",
        "capabilities",
        "defaultPrompt",
    ];
    if let Some(field) = object.keys().find(|field| !required.contains(&field.as_str())) {
        return Err(AppError::Plugin(format!("interface field `{field}` is not accepted")));
    }
    for field in ["displayName", "shortDescription", "longDescription", "developerName", "category"]
    {
        if object.get(field).and_then(Value::as_str).is_none() {
            return Err(AppError::Plugin(format!("interface {field} must be a string")));
        }
    }
    if object.get("capabilities").and_then(Value::as_array).is_none_or(|values| {
        values.is_empty() || values.iter().any(|value| value.as_str().is_none())
    }) {
        return Err(AppError::Plugin(
            "interface capabilities must be an array of strings".to_owned(),
        ));
    }
    if object.get("defaultPrompt").and_then(Value::as_array).is_none_or(|values| {
        values.is_empty() || values.iter().any(|value| value.as_str().is_none())
    }) {
        return Err(AppError::Plugin(
            "interface defaultPrompt must be a non-empty array of strings".to_owned(),
        ));
    }
    Ok(())
}

fn collect_commands(value: &Value, commands: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            if let Some(command) = object.get("command").and_then(Value::as_str) {
                commands.push(command.to_owned());
            }
            for child in object.values() {
                collect_commands(child, commands);
            }
        }
        Value::Array(values) => values.iter().for_each(|child| collect_commands(child, commands)),
        _ => {}
    }
}

fn is_relative_plugin_command(command: &str) -> bool {
    command.starts_with("${PLUGIN_ROOT}/")
        || command.starts_with("${PLUGIN_ROOT}\\")
        || (command.starts_with("bin/") && !command.contains("://"))
        || (command.starts_with("bin\\") && !command.contains(":\\"))
}

fn is_semver(value: &str) -> bool {
    let mut parts = value.splitn(4, '.');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    let Some(patch) = parts.next() else {
        return false;
    };
    parts.next().is_none() && [major, minor, patch].iter().all(|part| valid_semver_number(part))
}

fn valid_semver_number(part: &str) -> bool {
    !part.is_empty()
        && part.chars().all(|character| character.is_ascii_digit())
        && (part == "0" || !part.starts_with('0'))
}

fn package_plugin(source: &Path, destination: &Path) -> Result<(), AppError> {
    validate_plugin(source)?;
    if destination.exists() {
        return Err(AppError::Plugin(format!(
            "package destination already exists: {}",
            destination.display()
        )));
    }
    fs::create_dir_all(destination)?;
    copy_tree(&source.join(".codex-plugin"), &destination.join(".codex-plugin"))?;
    copy_tree(&source.join("hooks"), &destination.join("hooks"))?;
    copy_current_binary(destination)?;
    println!("packaged product plugin at {}", destination.display());
    Ok(())
}

fn package_benchmark_plugin(source: &Path, destination: &Path) -> Result<(), AppError> {
    validate_benchmark_plugin(source)?;
    if destination.exists() {
        return Err(AppError::Plugin(format!(
            "package destination already exists: {}",
            destination.display()
        )));
    }
    fs::create_dir_all(destination)?;
    copy_tree(&source.join(".codex-plugin"), &destination.join(".codex-plugin"))?;
    fs::copy(source.join(".mcp.json"), destination.join(".mcp.json"))?;
    copy_current_binary(destination)?;
    println!("packaged benchmark plugin at {}", destination.display());
    Ok(())
}

fn copy_current_binary(destination: &Path) -> Result<(), AppError> {
    let binary_name = if cfg!(windows) { "needle.exe" } else { "needle" };
    let binary = env::current_exe()?;
    let bin_directory = destination.join("bin");
    fs::create_dir_all(&bin_directory)?;
    fs::copy(binary, bin_directory.join(binary_name))?;
    let license = "LICENSE.md";
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../").join(license);
    fs::copy(source, destination.join(license))?;
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), AppError> {
    if source.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
    }
    Ok(())
}

fn copy_tree_without_git(source: &Path, destination: &Path) -> Result<(), AppError> {
    if source.file_name().and_then(|name| name.to_str()) == Some(".git") {
        return Ok(());
    }
    if source.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree_without_git(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
    }
    Ok(())
}

#[allow(dead_code)]
fn _format_revision_boundary() -> u32 {
    FORMAT_REVISION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_mcp_rejects_unknown_or_incomplete_server_arguments() {
        assert!(validate_mcp_serve_arguments(&["serve".to_owned()]).is_ok());
        assert!(
            validate_mcp_serve_arguments(&[
                "serve".to_owned(),
                "--cache-only".to_owned(),
                "--main-model".to_owned(),
                "gpt-5.6-sol".to_owned(),
            ])
            .is_ok()
        );
        assert!(
            validate_mcp_serve_arguments(&["serve".to_owned(), "--main-model".to_owned()]).is_err()
        );
        assert!(
            validate_mcp_serve_arguments(&["serve".to_owned(), "--network".to_owned()]).is_err()
        );
    }

    #[test]
    fn resident_server_accepts_only_data_and_repository_roots() {
        assert!(validate_server_arguments(&[]).is_ok());
        assert!(
            validate_server_arguments(&[
                "--data-dir".to_owned(),
                "data".to_owned(),
                "--repository".to_owned(),
                "repo".to_owned(),
            ])
            .is_ok()
        );
        assert!(validate_server_arguments(&["--repository".to_owned()]).is_err());
        assert!(validate_server_arguments(&["--network".to_owned()]).is_err());
    }

    #[test]
    fn plugin_validator_rejects_bad_manifest_contract() {
        let root =
            std::env::temp_dir().join(format!("needle-plugin-invalid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".codex-plugin")).unwrap();
        fs::create_dir_all(root.join("hooks")).unwrap();
        fs::write(
            root.join(".codex-plugin/plugin.json"),
            r#"{"name":"Needle","version":"latest"}"#,
        )
        .unwrap();
        fs::write(root.join("hooks/hooks.json"), "{}\n").unwrap();
        assert!(validate_plugin(&root).is_err());
        let _ = fs::remove_dir_all(root);
        assert!(!is_relative_plugin_command("C:\\needle\\bin\\needle.exe"));
        assert!(validate_interface(&json!("codex")).is_err());
    }

    #[test]
    fn codex_builder_uses_supported_overrides_not_unsupported_flags() {
        let args = build_codex_args("gpt-5-codex", "high", "priority", "marker", "prompt")
            .expect("valid command values");
        assert!(args.windows(2).any(|pair| pair == ["-c", "model_reasoning_effort=\"high\""]));
        assert!(args.windows(2).any(|pair| pair == ["-c", "service_tier=\"priority\""]));
        assert!(!args.iter().any(|arg| arg == "--reasoning" || arg == "--service-tier"));
        assert!(build_codex_args("gpt/5", "high", "priority", "marker", "prompt").is_err());
        let p0 = build_codex_initial_args(
            "gpt-5-codex",
            "high",
            "priority",
            "baseline",
            "prompt",
            Path::new("fixture"),
            false,
        )
        .unwrap();
        assert!(!p0.iter().any(|arg| arg == "--ephemeral"));
        assert!(p0.windows(2).any(|pair| pair == ["--sandbox", "read-only"]));
        assert!(p0.windows(2).any(|pair| pair == ["--cd", "fixture"]));
        let resume =
            build_codex_resume_args("01234567-89ab-cdef-0123-456789abcdef", "payload").unwrap();
        assert_eq!(resume[0..2], ["exec", "resume"]);
        assert!(resume.windows(2).any(|pair| { pair == ["--disable", "hooks"] }));
        assert!(resume.iter().any(|arg| arg == "01234567-89ab-cdef-0123-456789abcdef"));
        assert!(
            !resume.iter().any(|arg| arg == "--cd" || arg == "--ephemeral" || arg == "--sandbox")
        );
    }

    #[test]
    fn codex_launcher_rejects_shells_but_accepts_native_binaries() {
        for path in [
            "codex.cmd",
            "codex.bat",
            "codex.ps1",
            "codex.sh",
            "powershell.exe",
            "pwsh.exe",
            "cmd.exe",
        ] {
            assert!(reject_shell_launcher(Path::new(path)).is_err(), "{path}");
        }
        assert!(reject_shell_launcher(Path::new("codex.exe")).is_ok());
        assert!(reject_shell_launcher(Path::new("codex")).is_ok());
    }

    #[test]
    fn managed_codex_runtime_is_resolved_relative_to_needle() {
        let root = Path::new("install");
        let needle = root.join(if cfg!(windows) { "needle.exe" } else { "needle" });
        let expected_name = if cfg!(windows) { "codex.exe" } else { "codex" };
        assert_eq!(
            managed_codex_candidate(&needle),
            Some(root.join("runtime").join("bin").join(expected_name))
        );
    }

    #[cfg(windows)]
    #[test]
    fn managed_codex_runtime_requires_the_complete_windows_package() {
        let root = env::temp_dir().join(format!("needle-managed-codex-{}", std::process::id()));
        let runtime = root.join("runtime");
        let codex = runtime.join("bin/codex.exe");
        let required = [
            codex.clone(),
            runtime.join("bin/codex-code-mode-host.exe"),
            runtime.join("codex-path/rg.exe"),
            runtime.join("codex-resources/codex-command-runner.exe"),
            runtime.join("codex-resources/codex-windows-sandbox-setup.exe"),
            runtime.join("codex-package.json"),
        ];
        let _ = fs::remove_dir_all(&root);
        for path in &required {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, []).unwrap();
        }
        assert!(managed_codex_package_is_complete(&codex));

        fs::remove_file(runtime.join("codex-resources/codex-command-runner.exe")).unwrap();
        assert!(!managed_codex_package_is_complete(&codex));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_auth_gate_rejects_missing_or_unusable_authentication() {
        let root = env::temp_dir().join(format!("needle-auth-gate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let missing =
            ensure_codex_authenticated(Path::new("missing-codex"), &root, "auth-gate-test")
                .expect_err("missing auth must fail before spawning Codex");
        assert!(missing.to_string().contains("no local authentication file"));

        fs::write(root.join("auth.json"), b"{}").unwrap();
        #[cfg(windows)]
        let failing_command = PathBuf::from("where.exe");
        #[cfg(unix)]
        let failing_command = {
            use std::os::unix::fs::PermissionsExt;

            let path = root.join("failing-codex");
            fs::write(&path, b"#!/bin/sh\nexit 1\n").unwrap();
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).unwrap();
            path
        };
        let unusable = ensure_codex_authenticated(&failing_command, &root, "auth-gate-test")
            .expect_err("failed login status must reject the provider stage");
        assert!(unusable.to_string().contains("not authenticated"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn product_child_capture_preserves_spawn_failure_status() {
        let root =
            env::temp_dir().join(format!("needle-product-spawn-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let capture = run_product_child_capture(
            &root.join("missing-codex-executable"),
            &[],
            &root,
            &root,
            &root,
            None,
            &root,
            Duration::from_secs(1),
        )
        .unwrap();
        let status = capture.process_status();

        assert_eq!(status.status, format!("spawn-error:{}", status.spawn_error.as_ref().unwrap()));
        assert!(status.exit_code.is_none());
        assert!(!status.timed_out);
        assert!(status.abort_reason.is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn arm_prompts_share_the_same_fixture_request() {
        let request = deterministic_task_request("task", 7);
        let marker = arm_prompt(ExperimentArm::P2, "task prompt", &request);
        let tool = arm_prompt(ExperimentArm::P3, "task prompt", &request);
        assert!(marker.contains(&request));
        assert!(tool.contains(&request));
        assert!(marker.contains("P2 Stop generated"));
        assert!(tool.contains("P3 static MCP tool"));
    }

    #[test]
    fn product_need_body_is_the_complete_task_prompt() {
        let task = "Trace declaration, precedence, and the most focused test command.";
        let marker = product_task_request(task, &NeedKey::new("locate.implementation").unwrap());
        let request = NeedRequest::parse(&marker).unwrap().unwrap();
        assert_eq!(request.body, task);
        assert_eq!(request.key.as_str(), "locate.implementation");
    }

    #[test]
    fn task_route_defaults_to_trace_and_accepts_locate() {
        let trace = TaskFixture {
            id: "trace".to_owned(),
            prompt: "trace".to_owned(),
            extra: serde_json::Map::new(),
        };
        assert_eq!(task_route(&trace).unwrap().as_str(), "trace.state-flow");

        let locate = TaskFixture {
            id: "locate".to_owned(),
            prompt: "locate".to_owned(),
            extra: serde_json::Map::from_iter([(
                "route".to_owned(),
                Value::String("locate.implementation".to_owned()),
            )]),
        };
        assert_eq!(task_route(&locate).unwrap().as_str(), "locate.implementation");
    }

    #[test]
    fn benchmark_snapshot_excludes_git_metadata() {
        let root = env::temp_dir().join(format!("needle-snapshot-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("checkout");
        let snapshot = root.join("snapshot");
        fs::create_dir_all(source.join(".git")).unwrap();
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(source.join(".git/config"), b"history").unwrap();
        fs::write(source.join("src/lib.rs"), b"pub fn fixture() {}\n").unwrap();

        copy_tree_without_git(&source, &snapshot).unwrap();

        assert!(!snapshot.join(".git").exists());
        assert_eq!(fs::read(snapshot.join("src/lib.rs")).unwrap(), b"pub fn fixture() {}\n");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn p0_resume_uses_the_same_deterministic_payload_as_p2() {
        let marker = deterministic_task_request("task", 7);
        let request = NeedRequest::parse(&marker).unwrap().unwrap();
        let expected = needle_bench::p2_payload(&request);
        let resume =
            build_codex_resume_args("01234567-89ab-cdef-0123-456789abcdef", &expected).unwrap();
        assert_eq!(resume.last(), Some(&expected));
        assert!(!resume.last().unwrap().contains("@@need:"));
        assert_eq!(expected, needle_bench::p2_payload(&request));
    }

    #[test]
    fn p0_latency_includes_initial_and_follow_up_captures() {
        assert_eq!(observation_duration_ms(149_000, Some(11_748)), 160_748);
        assert_eq!(observation_duration_ms(149_000, None), 149_000);
        assert_eq!(observation_duration_ms(u64::MAX, Some(1)), u64::MAX);
    }

    #[test]
    fn focused_live_runs_accept_only_known_arm_names() {
        assert_eq!(parse_experiment_arm("P3").unwrap(), ExperimentArm::P3);
        assert!(parse_experiment_arm("p3").is_err());
        assert!(parse_experiment_arm("P5").is_err());
    }

    #[test]
    fn marker_continuation_requires_isolated_stop_block_telemetry() {
        let telemetry = format!(
            "{}\n{}\n{}\n{}\n",
            serde_json::to_string(&json!({
                "digest": "b3:0000000000000000000000000000000000000000000000000000000000000000",
                "event": {"event":"SessionStart", "profile_digest":"b3:0000000000000000000000000000000000000000000000000000000000000000", "profile_payload_bytes": 12}
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "digest": "b3:1",
                "event": {"event":"Stop", "decision":"noop"}
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "digest": "b3:2",
                "event": {"event":"PreCompact"}
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "digest": "b3:3",
                "event": {"event":"PostCompact"}
            }))
            .unwrap()
        );
        let evidence = extract_telemetry_profile(telemetry.as_bytes());
        assert!(evidence.observed);
        assert!(evidence.stream_complete);
        assert!(!evidence.stop_block);
        assert_eq!(evidence.pre_compact_events, 1);
        assert_eq!(evidence.post_compact_events, 1);
        assert!(evidence.profile_digest.is_some());
        assert_eq!(evidence.profile_payload_bytes, Some(12));
        let (compaction_events, compaction_precision) =
            merge_compaction_evidence(1, needle_bench::MetricPrecision::Unavailable, &evidence);
        assert_eq!(compaction_events, 1);
        assert_eq!(compaction_precision, needle_bench::MetricPrecision::Exact);
        let imbalanced = TelemetryEvidence {
            pre_compact_events: 2,
            post_compact_events: 1,
            observed: true,
            stream_complete: true,
            ..TelemetryEvidence::default()
        };
        let (compaction_events, compaction_precision) =
            merge_compaction_evidence(0, needle_bench::MetricPrecision::Unavailable, &imbalanced);
        assert_eq!(compaction_events, 2);
        assert_eq!(compaction_precision, needle_bench::MetricPrecision::Exact);
        assert_eq!(imbalanced.pre_compact_events, 2);
        assert_eq!(imbalanced.post_compact_events, 1);
        let (compaction_events, compaction_precision) = merge_compaction_evidence(
            0,
            needle_bench::MetricPrecision::Unavailable,
            &TelemetryEvidence::default(),
        );
        assert_eq!(compaction_events, 0);
        assert_eq!(compaction_precision, needle_bench::MetricPrecision::Unavailable);
        let parsed = needle_bench::CodexParseResult {
            terminal_event: true,
            terminal_success: Some(true),
            continuation_success: Some(true),
            ..needle_bench::CodexParseResult::default()
        };
        assert_eq!(terminal_continuation_for_arm(ExperimentArm::P2, &parsed, false), Some(false));
        assert_eq!(terminal_continuation_for_arm(ExperimentArm::P2, &parsed, true), Some(true));
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn child_capture_timeout_kills_and_waits_without_pipe_contamination() {
        let root = env::temp_dir().join(format!("needle-timeout-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("plugin-data")).unwrap();
        #[cfg(windows)]
        let (codex, arguments) = (
            PathBuf::from("powershell"),
            vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "Start-Sleep -Seconds 2".to_owned(),
            ],
        );
        #[cfg(unix)]
        let (codex, arguments) = (PathBuf::from("sh"), vec!["-c".to_owned(), "sleep 2".to_owned()]);
        let capture = run_child_capture(
            &codex,
            &arguments,
            &root,
            &root,
            &root.join("plugin-data"),
            &root,
            "timeout",
            ExperimentArm::P0,
            Duration::from_millis(100),
        )
        .unwrap();
        assert!(capture.timed_out);
        assert!(capture.failed());
        assert!(capture.stdout_text().is_empty());
        assert!(capture.stderr_text().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_refuses_existing_destination_without_deleting_contents() {
        let destination =
            env::temp_dir().join(format!("needle-package-existing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&destination);
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("sentinel.txt"), b"keep").unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugin");
        let error = package_plugin(&source, &destination).expect_err("existing destination");
        assert!(error.to_string().contains("already exists"));
        assert_eq!(fs::read(destination.join("sentinel.txt")).unwrap(), b"keep");
        assert!(package_plugin(&source, Path::new(".")).is_err());
        let _ = fs::remove_dir_all(destination);
    }

    #[test]
    fn product_pilot_rejects_plugin_hook_contamination() {
        let root = env::temp_dir().join(format!("needle-pilot-isolation-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let command = json!({
            "type": "command",
            "command": "needle hook",
        });
        fs::write(
            root.join("hooks.json"),
            serde_json::to_vec(&json!({
                "hooks": {
                    "SessionStart": [{"hooks": [command.clone()]}],
                    "UserPromptSubmit": [{"hooks": [command.clone()]}],
                    "Stop": [{"hooks": [command.clone()]}],
                    "SessionEnd": [{"hooks": [command]}],
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(root.join("config.toml"), "").unwrap();
        ensure_product_pilot_hook_isolation(&root).expect("clean hook home");

        fs::write(root.join("config.toml"), "[plugins.\"needle@personal\"]\nenabled = true\n")
            .unwrap();
        let error = ensure_product_pilot_hook_isolation(&root).expect_err("active plugin");
        assert!(error.to_string().contains("needle@personal"));
        assert!(error.to_string().contains("bundled hooks"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cache_pilot_requires_hooks_from_the_current_binary() {
        let root = env::temp_dir().join(format!("needle-cache-pilot-hooks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let current = env::current_exe().unwrap();
        let hook_for = |executable: &Path, subcommand: &str| {
            let command = format!("\"{}\" hook {subcommand}", executable.display());
            json!({
                "type": "command",
                "command": command,
                "commandWindows": command,
            })
        };
        let hooks = |stop: Value| {
            json!({
                "hooks": {
                    "SessionStart": [{"hooks": [hook_for(&current, "session-start")]}],
                    "UserPromptSubmit": [{"hooks": [hook_for(&current, "user-prompt-submit")]}],
                    "Stop": [{"hooks": [stop]}],
                    "SessionEnd": [{"hooks": [hook_for(&current, "session-end")]}],
                }
            })
        };
        fs::write(
            root.join("hooks.json"),
            serde_json::to_vec(&hooks(hook_for(&current, "stop"))).unwrap(),
        )
        .unwrap();
        ensure_cache_pilot_hook_binary(&root).expect("current hook binary");

        let outdated = root.join(if cfg!(windows) { "outdated.exe" } else { "outdated" });
        fs::write(&outdated, b"outdated").unwrap();
        fs::write(
            root.join("hooks.json"),
            serde_json::to_vec(&hooks(hook_for(&outdated, "stop"))).unwrap(),
        )
        .unwrap();
        let error = ensure_cache_pilot_hook_binary(&root).expect_err("outdated hook binary");
        assert!(error.to_string().contains("outdated Needle binary"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cache_pilot_runtime_paths_are_made_absolute_before_spawn() {
        let relative = Path::new("target/cache-pilot-relative");
        let absolute = absolute_run_path(relative).unwrap();
        assert!(absolute.is_absolute());
        assert!(absolute.ends_with(relative));
        #[cfg(windows)]
        assert!(
            !absolute.to_string_lossy().contains('/'),
            "Windows run paths must use native separators: {}",
            absolute.display()
        );

        let already_absolute = env::current_dir().unwrap().join("target/cache-pilot-absolute");
        assert_eq!(absolute_run_path(&already_absolute).unwrap(), already_absolute);
    }
}
mod runtime_instance;
mod server;
