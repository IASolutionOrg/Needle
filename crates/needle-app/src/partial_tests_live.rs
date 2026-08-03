use super::{
    AppError, BENCHMARK_REPOSITORY_SHA, canonical_child_path, ensure_codex_authenticated,
    ensure_dedicated_codex_home, option_value, required_value, resolve_codex,
};
use crate::mcp::schema::{MappedNeedContext, McpNeedContextRequest};
use crate::mcp_live_pilot::{GuardedInput, run_guarded};
use needle_bench::{PricingSnapshot, TokenCost, parse_codex_jsonl};
use needle_core::{
    ArtifactKind, ArtifactRequest, CapabilityMode, Digest, FlowStepRole, LocationRole, Need,
    NeedFragment, PredicateKind, ReuseUnit, SemanticFlowStep, SemanticLocation,
    SemanticWorkerArtifact, WorkerObservationTrace, WorkerRequest,
};
use needle_platform_codex::CodexWorker;
use needle_runtime::{
    NeedShadowWrite, OperatorCostObservation, RuntimeSettings, RuntimeStore, capture_git_snapshot,
    validate_semantic_artifact_with_trace, validator_definition,
};
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REPORT_SCHEMA: &str = "needle.partial-tests-live/1";
const PRICING_FIXTURE: &str = "fixtures/openai-codex-pricing-2026-07-27.json";
const TRACE_TASK: &str = "Trace how --crlf changes matching and search line terminators from CLI parsing through runtime configuration, and identify a focused test proving the default scenario.";
const TESTS_TASK: &str = "Identify the focused test for ripgrep's --crlf behavior using the certified context already produced.";
// Observed from the completed r121 main turn. The run missed the reuse gate,
// but its main usage is complete and therefore replaces the earlier synthetic
// 50k-token assumption.
const ESTIMATED_MAIN_INPUT_TOKENS: u64 = 96_190;
const ESTIMATED_MAIN_CACHED_INPUT_TOKENS: u64 = 55_040;
const ESTIMATED_MAIN_OUTPUT_TOKENS: u64 = 896;
// Conservative observed worker usage from the failed r104 calibration. The
// estimate intentionally retains both worker turns until a newer successful
// single-turn observation replaces it.
const ESTIMATED_WORKER_INPUT_TOKENS: u64 = 418_061;
const ESTIMATED_WORKER_CACHED_INPUT_TOKENS: u64 = 351_488;
const ESTIMATED_WORKER_OUTPUT_TOKENS: u64 = 3_907;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Preflight,
    Offline,
    Paid,
}

struct SeedState {
    store: RuntimeStore,
    data_directory: PathBuf,
    source_snapshot: needle_core::RepositorySnapshot,
    requests: [Value; 2],
    request_digests: [Digest; 2],
    seeded_artifacts: Vec<String>,
    worker_schema_digest: Digest,
}

pub(super) fn run(arguments: &[String]) -> Result<(), AppError> {
    let mode = mode(arguments)?;
    let codex = resolve_codex(option_value(arguments, "--codex"))?;
    let codex_home = PathBuf::from(required_value(arguments, "--codex-home")?);
    ensure_dedicated_codex_home(&codex_home)?;
    let codex_home = canonical_child_path(&codex_home)?;
    let needle = canonical_child_path(Path::new(&required_value(arguments, "--needle")?))?;
    let source =
        canonical_child_path(Path::new(&required_value(arguments, "--source-repository")?))?;
    if mode != Mode::Offline {
        ensure_codex_authenticated(&codex, &codex_home, "partial-tests-live")?;
    }
    let artifact_root = PathBuf::from(required_value(arguments, "--artifact-root")?);
    if artifact_root.exists() {
        return Err(AppError::Experiment(format!(
            "partial-tests artifact root already exists: {}",
            artifact_root.display()
        )));
    }
    fs::create_dir_all(&artifact_root)?;
    let artifact_root = canonical_child_path(&artifact_root)?;
    let main_model = required_value(arguments, "--main-model")?;
    let main_reasoning = required_value(arguments, "--main-reasoning")?;
    let worker_model = required_value(arguments, "--worker-model")?;
    let worker_reasoning = required_value(arguments, "--worker-reasoning")?;
    let service_tier = required_value(arguments, "--service-tier")?;
    let timeout_seconds = option_value(arguments, "--timeout-seconds")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid --timeout-seconds: {error}")))?
        .unwrap_or(180);
    if !(30..=600).contains(&timeout_seconds) {
        return Err(AppError::Usage("--timeout-seconds must be between 30 and 600".to_owned()));
    }
    let isolation =
        CodexWorker::verify_isolation(&codex.display().to_string()).map_err(AppError::Runtime)?;
    if !isolation.verified() || isolation.codex_version != "0.144.0" {
        return Err(AppError::Experiment(format!(
            "Codex 0.144.0 isolation preflight failed for {}",
            isolation.codex_version
        )));
    }
    verify_pinned_clean_source(&source)?;
    let pricing_path = option_value(arguments, "--pricing")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(PRICING_FIXTURE));
    let pricing: PricingSnapshot = serde_json::from_slice(&fs::read(pricing_path)?)?;
    let estimate = estimate_cost(&pricing, &main_model, &worker_model, &service_tier)?;
    let state = seed_state(&artifact_root, &source, &codex, &worker_model, &worker_reasoning)?;

    match mode {
        Mode::Preflight => run_preflight(
            &artifact_root,
            &source,
            &codex_home,
            &codex,
            &needle,
            &main_model,
            &worker_model,
            &worker_reasoning,
            &service_tier,
            &pricing,
            estimate,
            isolation.codex_version,
            state,
        ),
        Mode::Offline | Mode::Paid => run_observation(
            mode,
            &artifact_root,
            &source,
            &codex_home,
            &codex,
            &needle,
            &main_model,
            &main_reasoning,
            &worker_model,
            &worker_reasoning,
            &service_tier,
            &pricing,
            estimate,
            timeout_seconds,
            isolation.codex_version,
            state,
            arguments,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_preflight(
    artifact_root: &Path,
    source: &Path,
    codex_home: &Path,
    codex: &Path,
    needle: &Path,
    main_model: &str,
    worker_model: &str,
    worker_reasoning: &str,
    service_tier: &str,
    pricing: &PricingSnapshot,
    estimate: u64,
    codex_version: String,
    state: SeedState,
) -> Result<(), AppError> {
    let settings = state.store.settings().map_err(|error| AppError::Runtime(error.to_string()))?;
    let transport = CodexWorker::with_codex_home(&state.data_directory, codex_home)
        .preflight_transport(&settings.worker_config(), source)
        .map_err(AppError::Runtime)?;
    let responses = direct_exchange(
        needle,
        &state.data_directory,
        source,
        main_model,
        codex_home,
        true,
        std::slice::from_ref(&state.requests[0]),
    )?;
    let tool = responses[1].pointer("/result/tools/0").unwrap_or(&Value::Null);
    let call = responses[2].pointer("/result/structuredContent").unwrap_or(&Value::Null);
    let schema_valid = tool.get("name").and_then(Value::as_str) == Some("need_context")
        && tool.pointer("/inputSchema/additionalProperties").and_then(Value::as_bool)
            == Some(false)
        && tool.pointer("/outputSchema/additionalProperties").and_then(Value::as_bool)
            == Some(false)
        && tool.pointer("/outputSchema/properties/calibration/type").and_then(Value::as_str)
            == Some("boolean")
        && call.get("status").and_then(Value::as_str) == Some("bypass")
        && call.get("worker_spawned").and_then(Value::as_bool) == Some(false);
    let passed = transport.provider_turns_started == 0
        && transport.app_server_initialized
        && transport.ephemeral_thread_cleanup_completed
        && schema_valid
        && state.store.worker_run_count().map_err(|error| AppError::Runtime(error.to_string()))?
            == 0;
    let report = json!({
        "schema": REPORT_SCHEMA,
        "mode": "preflight",
        "passed": passed,
        "provider_observations_started": 0,
        "automatic_retries": 0,
        "dedicated_auth_verified": true,
        "codex_version": codex_version,
        "native_codex": codex,
        "source_sha": BENCHMARK_REPOSITORY_SHA,
        "source_snapshot_digest": state.source_snapshot.source_digest,
        "seeded_artifact_ids": state.seeded_artifacts,
        "request_digests": state.request_digests,
        "mcp_schema_valid": schema_valid,
        "transport": transport,
        "models": {"main": main_model, "worker": worker_model, "worker_reasoning": worker_reasoning, "service_tier": service_tier},
        "pricing_snapshot_digest": pricing.digest().map_err(|error| AppError::Experiment(error.to_string()))?,
        "estimated_budget_microcredits": estimate,
        "estimate_is_hard_provider_ceiling": false,
        "explicit_user_approval_required": true,
    });
    write_report(artifact_root, &report)?;
    if !passed {
        return Err(AppError::Experiment("partial-tests native preflight failed".to_owned()));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_observation(
    mode: Mode,
    artifact_root: &Path,
    source: &Path,
    codex_home: &Path,
    codex: &Path,
    needle: &Path,
    main_model: &str,
    main_reasoning: &str,
    worker_model: &str,
    worker_reasoning: &str,
    service_tier: &str,
    pricing: &PricingSnapshot,
    estimate: u64,
    timeout_seconds: u64,
    codex_version: String,
    state: SeedState,
    arguments: &[String],
) -> Result<(), AppError> {
    let approved_budget = if mode == Mode::Paid {
        let approved = required_value(arguments, "--approved-budget-microcredits")?
            .parse::<u64>()
            .map_err(|error| AppError::Usage(format!("invalid approved budget: {error}")))?;
        if approved < estimate {
            return Err(AppError::Usage(format!(
                "approved budget {approved} is below the preflight estimate {estimate}"
            )));
        }
        Some(approved)
    } else {
        None
    };
    if mode == Mode::Offline {
        fs::write(codex_home.join(".needle-simulation-worker-scenario"), "worker_partial_tests")?;
        fs::write(
            codex_home.join(".needle-simulation-exec-scenario"),
            "mcp_partial_tests_success",
        )?;
    }
    let workers_before =
        state.store.worker_run_count().map_err(|error| AppError::Runtime(error.to_string()))?;
    let snapshot_before =
        capture_git_snapshot(source).map_err(|error| AppError::Runtime(error.to_string()))?.1;
    let main = if mode == Mode::Offline {
        let responses = direct_exchange(
            needle,
            &state.data_directory,
            source,
            main_model,
            codex_home,
            false,
            &state.requests,
        )?;
        if let Err(error) = validate_product_sequence(&responses) {
            let runs = state
                .store
                .worker_runs_after(0)
                .map_err(|store_error| AppError::Runtime(store_error.to_string()))?;
            return Err(AppError::Experiment(format!("{error}; worker runs: {runs:?}")));
        }
        let instructions = developer_instructions(&state.requests)?;
        run_guarded(GuardedInput {
            codex,
            codex_home,
            source,
            artifact_root,
            data_directory: &state.data_directory,
            prompt: "Answer the --crlf task using the two Needle contexts.",
            model: main_model,
            reasoning: main_reasoning,
            service_tier,
            timeout: Duration::from_secs(timeout_seconds),
            require_observation: false,
            expected_request_digests: state.request_digests.to_vec(),
            developer_instructions: &instructions,
            extra_config: &[],
            calibration_reuse: true,
        })?
    } else {
        let instructions = developer_instructions(&state.requests)?;
        let mcp_args = [
            "mcp".to_owned(),
            "serve".to_owned(),
            "--data-dir".to_owned(),
            state.data_directory.display().to_string(),
            "--repository".to_owned(),
            source.display().to_string(),
            "--main-model".to_owned(),
            main_model.to_owned(),
        ];
        let extra_config = vec![
            format!(
                "mcp_servers.needle.command={}",
                toml::Value::String(needle.display().to_string())
            ),
            format!(
                "mcp_servers.needle.args={}",
                toml::Value::Array(mcp_args.iter().cloned().map(toml::Value::String).collect())
            ),
        ];
        run_guarded(GuardedInput {
            codex,
            codex_home,
            source,
            artifact_root,
            data_directory: &state.data_directory,
            prompt: "Trace ripgrep's --crlf behavior and identify its focused test.",
            model: main_model,
            reasoning: main_reasoning,
            service_tier,
            timeout: Duration::from_secs(timeout_seconds),
            require_observation: true,
            expected_request_digests: state.request_digests.to_vec(),
            developer_instructions: &instructions,
            extra_config: &extra_config,
            calibration_reuse: true,
        })?
    };
    let observations = read_jsonl(&state.data_directory.join("mcp-observations.jsonl"))?;
    let parsed = parse_codex_jsonl(&main.stdout);
    let provider_observations_started = u8::from(
        mode == Mode::Paid
            && main
                .stdout
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|event| event.get("type").and_then(Value::as_str) == Some("turn.started")),
    );
    let operator_cost =
        record_operator_cost(&state, pricing, worker_model, worker_reasoning, service_tier)?;
    let main_cost = price_optional(
        pricing,
        main_model,
        service_tier,
        parsed.usage.input_tokens,
        parsed.usage.cached_input_tokens,
        parsed.usage.output_tokens,
    )?;
    let workers_after =
        state.store.worker_run_count().map_err(|error| AppError::Runtime(error.to_string()))?;
    let snapshot_after =
        capture_git_snapshot(source).map_err(|error| AppError::Runtime(error.to_string()))?.1;
    let source_unchanged = snapshot_before.source_digest == snapshot_after.source_digest;
    let sequence_valid = observations.len() == 2
        && validate_observations(&observations).is_ok()
        && workers_after.saturating_sub(workers_before) == 1;
    let main_valid = main.abort_reason.is_none()
        && main.status_success
        && main.guard.mcp_succeeded
        && main.guard.discovery_events == 0
        && main.guard.final_response.is_some()
        && parsed.terminal_success == Some(true);
    let passed = sequence_valid && main_valid && source_unchanged;
    let report = json!({
        "schema": REPORT_SCHEMA,
        "mode": if mode == Mode::Paid {"paid"} else {"offline-simulator"},
        "passed": passed,
        "provider_observations_started": provider_observations_started,
        "automatic_retries": 0,
        "codex_version": codex_version,
        "source_sha": BENCHMARK_REPOSITORY_SHA,
        "source_snapshot_before": snapshot_before.source_digest,
        "source_snapshot_after": snapshot_after.source_digest,
        "source_unchanged": source_unchanged,
        "request_digests": state.request_digests,
        "observed_request_digests": main.guard.observed_request_digests,
        "observations": observations,
        "worker_run_delta": workers_after.saturating_sub(workers_before),
        "operator_cost_observation": operator_cost,
        "main": {
            "success": main.status_success,
            "exit_code": main.exit_code,
            "timed_out": main.timed_out,
            "abort_reason": main.abort_reason,
            "duration_ms": main.duration_ms,
            "discovery_events": main.guard.discovery_events,
            "final_response_present": main.guard.final_response.is_some(),
            "usage": parsed.usage,
            "cost": main_cost,
        },
        "models": {"main": main_model, "worker": worker_model, "service_tier": service_tier},
        "pricing_snapshot_digest": pricing.digest().map_err(|error| AppError::Experiment(error.to_string()))?,
        "estimated_budget_microcredits": estimate,
        "approved_budget_microcredits": approved_budget,
        "estimate_is_hard_provider_ceiling": false,
    });
    write_report(artifact_root, &report)?;
    if !passed {
        return Err(AppError::Experiment(format!(
            "partial-tests observation failed closed; report: {}",
            artifact_root.join("report.json").display()
        )));
    }
    Ok(())
}

fn seed_state(
    artifact_root: &Path,
    source: &Path,
    codex: &Path,
    worker_model: &str,
    worker_reasoning: &str,
) -> Result<SeedState, AppError> {
    let data_directory = artifact_root.join("data");
    fs::create_dir_all(&data_directory)?;
    let store = RuntimeStore::new(data_directory.join("needle.sqlite3"));
    store
        .initialize_defaults(&RuntimeSettings {
            codex_executable: codex.display().to_string(),
            worker_model: worker_model.to_owned(),
            worker_reasoning: worker_reasoning.to_owned(),
            worker_timeout_seconds: 180,
            evidence_failure_policy: needle_core::EvidenceFailurePolicy::DiscardInvalidFact,
            trusted_test_execution: false,
            multi_need_policy: needle_core::MultiNeedPolicy::default(),
        })
        .map_err(|error| AppError::Runtime(error.to_string()))?;
    store.mark_utility_gate_passed().map_err(|error| AppError::Runtime(error.to_string()))?;
    let (_, snapshot) =
        capture_git_snapshot(source).map_err(|error| AppError::Runtime(error.to_string()))?;
    let requests = [trace_request(), tests_request()];
    let mapped = map_request(&requests[0])?;
    let route = needle_core::built_in_route_contracts()
        .into_iter()
        .find(|route| route.route.as_str() == "trace.state-flow")
        .ok_or_else(|| AppError::Experiment("trace route contract is missing".to_owned()))?;
    let need = needle_core::compile_need(&mapped.need_ir, snapshot.repository_id, &route)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let fragment = needle_core::need_fragment(&need, need.required.clone(), Vec::new());
    store
        .record_need_shadow(NeedShadowWrite {
            session_id: "partial-tests-seed",
            turn_id: "seed",
            transport_digest: mapped.request_digest,
            parser_definition_digest: needle_core::need_ir_definition_digest(),
            prompt_profile_digest: Digest::blake3(b"partial-tests-live-seed"),
            need_ir: &mapped.need_ir,
            need: &need,
            fragments: std::slice::from_ref(&fragment),
        })
        .map_err(|error| AppError::Runtime(error.to_string()))?;
    let (location, flow) = seed_artifacts(source)?;
    let traces = [
        WorkerObservationTrace {
            observed_files: vec!["crates/core/flags/defs.rs".to_owned()],
            gaps: Vec::new(),
        },
        WorkerObservationTrace {
            observed_files: vec![
                "crates/core/flags/defs.rs".to_owned(),
                "crates/core/flags/hiargs.rs".to_owned(),
            ],
            gaps: Vec::new(),
        },
    ];
    let mut seeded_artifacts = Vec::new();
    for (artifact, trace) in [location, flow].iter().zip(&traces) {
        let kind = artifact.kind();
        let request = semantic_request(&kind, &need, &fragment, snapshot.source_digest, TRACE_TASK);
        let validated = validate_semantic_artifact_with_trace(
            &fragment,
            artifact,
            source,
            request.semantic_id().digest(),
            Some(trace),
        )
        .map_err(|error| AppError::Experiment(error.to_string()))?;
        store
            .publish_semantic_artifact(&request, &need, &validated.artifact, &validated.certificate)
            .map_err(|error| AppError::Runtime(error.to_string()))?;
        seeded_artifacts.push(validated.artifact.id.to_string());
    }
    for class in store
        .capability_classes()
        .map_err(|error| AppError::Runtime(error.to_string()))?
        .into_iter()
        .filter(|class| class.reuse_unit == ReuseUnit::Artifact)
    {
        store
            .set_capability_mode(
                &class.id,
                class.definition_digest,
                CapabilityMode::Authoritative,
                Some(Digest::blake3(b"partial-tests-live-calibration-promotion")),
            )
            .map_err(|error| AppError::Runtime(error.to_string()))?;
    }
    let worker_schema_digest = worker_schema_digest(&need, &fragment, &snapshot, source)?;
    let mapped_second = map_request(&requests[1])?;
    Ok(SeedState {
        store,
        data_directory,
        source_snapshot: snapshot,
        requests,
        request_digests: [mapped.request_digest, mapped_second.request_digest],
        seeded_artifacts,
        worker_schema_digest,
    })
}

fn seed_artifacts(
    source: &Path,
) -> Result<(SemanticWorkerArtifact, SemanticWorkerArtifact), AppError> {
    let definitions = "crates/core/flags/defs.rs";
    let high_args = "crates/core/flags/hiargs.rs";
    let bytes = fs::read(source.join(definitions))?;
    let start =
        bytes.windows(b"--crlf".len()).position(|window| window == b"--crlf").ok_or_else(|| {
            AppError::Experiment("pinned ripgrep source has no --crlf anchor".to_owned())
        })?;
    let end = start + b"--crlf".len();
    let location = SemanticWorkerArtifact::CodeLocation {
        locations: vec![SemanticLocation {
            role: LocationRole::Primary,
            path: definitions.to_owned(),
            symbol: Some("Crlf".to_owned()),
            byte_start: Some(start.try_into().unwrap_or(u64::MAX)),
            byte_end: Some(end.try_into().unwrap_or(u64::MAX)),
        }],
        gaps: Vec::new(),
    };
    let step = |role, path: &str, symbol: &str, description: &str| SemanticFlowStep {
        role,
        location: SemanticLocation {
            role: LocationRole::Supporting,
            path: path.to_owned(),
            symbol: Some(symbol.to_owned()),
            byte_start: None,
            byte_end: None,
        },
        description: description.to_owned(),
    };
    let flow = SemanticWorkerArtifact::BehaviorTrace {
        scenario: "Default CLI search configuration and the --crlf-enabled CRLF search path"
            .to_owned(),
        steps: vec![
            step(FlowStepRole::Producer, definitions, "Crlf", "The --crlf switch is parsed."),
            step(FlowStepRole::Carrier, high_args, "HiArgs", "HiArgs carries CRLF state."),
            step(
                FlowStepRole::Transformation,
                high_args,
                "matcher",
                "The matcher enables CRLF-aware matching.",
            ),
            step(
                FlowStepRole::Precedence,
                definitions,
                "NullData",
                "Null-data precedence clears CRLF.",
            ),
            step(
                FlowStepRole::Consumer,
                high_args,
                "searcher",
                "The searcher selects the CRLF line terminator.",
            ),
        ],
        gaps: Vec::new(),
    };
    Ok((location, flow))
}

fn semantic_request(
    kind: &ArtifactKind,
    need: &Need,
    fragment: &NeedFragment,
    snapshot: Digest,
    wording: &str,
) -> ArtifactRequest {
    ArtifactRequest {
        contract_id: format!("needle.semantic.{}", kind.0),
        contract_revision: 2,
        repository_id: need.world.repository_lineage,
        source_snapshot_digest: snapshot,
        route_key: needle_core::NeedKey::new("trace.state-flow").expect("built-in route"),
        normalized_request: wording.to_owned(),
        semantic_fragment_id: Some(fragment.id),
        input_artifact_ids: Vec::new(),
    }
}

fn worker_schema_digest(
    need: &Need,
    fragment: &NeedFragment,
    snapshot: &needle_core::RepositorySnapshot,
    source: &Path,
) -> Result<Digest, AppError> {
    let preset = needle_runtime::built_in_presets()
        .into_iter()
        .find(|preset| preset.id == "trace.state-flow")
        .ok_or_else(|| AppError::Experiment("trace preset is missing".to_owned()))?;
    let contract = CodexWorker::diagnostic_contract(&WorkerRequest {
        root_task: TRACE_TASK.to_owned(),
        need_key: needle_core::NeedKey::new("trace.state-flow").expect("built-in route"),
        need_body: TRACE_TASK.to_owned(),
        preset,
        repository_root: source.display().to_string(),
        repository_snapshot: snapshot.clone(),
        declared_test_plan: None,
        trusted_test_execution: false,
        requested_artifact_kinds: vec![ArtifactKind::test_plan()],
        semantic_fragment: Some(needle_core::need_fragment(
            need,
            need.required
                .iter()
                .filter(|obligation| obligation.predicate == PredicateKind::FocusedTests)
                .cloned()
                .collect(),
            fragment.semantic_inputs.clone(),
        )),
    })
    .map_err(AppError::Runtime)?;
    Ok(contract.output_schema_digest)
}

fn direct_exchange(
    needle: &Path,
    data_directory: &Path,
    source: &Path,
    model: &str,
    codex_home: &Path,
    cache_only: bool,
    requests: &[Value],
) -> Result<Vec<Value>, AppError> {
    let mut command = Command::new(needle);
    command
        .args(["mcp", "serve", "--data-dir"])
        .arg(data_directory)
        .arg("--repository")
        .arg(source)
        .args(["--main-model", model])
        .env("CODEX_HOME", codex_home)
        .env("NEEDLE_INTERNAL_CALIBRATION_REUSE", "partial-tests-live")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if cache_only {
        command.arg("--cache-only");
    }
    let mut child = command.spawn()?;
    let mut input = BufWriter::new(
        child
            .stdin
            .take()
            .ok_or_else(|| AppError::Experiment("MCP stdin unavailable".to_owned()))?,
    );
    let mut output = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| AppError::Experiment("MCP stdout unavailable".to_owned()))?,
    );
    write_rpc(
        &mut input,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"partial-tests-live","version":super::VERSION}}}),
    )?;
    let mut responses = vec![read_rpc(&mut output)?];
    write_rpc(&mut input, &json!({"jsonrpc":"2.0","method":"notifications/initialized"}))?;
    write_rpc(&mut input, &json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))?;
    responses.push(read_rpc(&mut output)?);
    for (index, request) in requests.iter().enumerate() {
        write_rpc(
            &mut input,
            &json!({"jsonrpc":"2.0","id":index + 3,"method":"tools/call","params":{"name":"need_context","arguments":request}}),
        )?;
        responses.push(read_rpc(&mut output)?);
    }
    drop(input);
    let status = child.wait()?;
    if !status.success() {
        let mut diagnostic = String::new();
        if let Some(stderr) = child.stderr.take() {
            for line in BufReader::new(stderr).lines().map_while(Result::ok).take(20) {
                diagnostic.push_str(&line);
                diagnostic.push('\n');
            }
        }
        return Err(AppError::Experiment(format!(
            "partial-tests MCP server failed: {}",
            diagnostic.trim()
        )));
    }
    Ok(responses)
}

fn write_rpc(output: &mut impl Write, request: &Value) -> Result<(), AppError> {
    serde_json::to_writer(&mut *output, request)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn read_rpc(input: &mut impl BufRead) -> Result<Value, AppError> {
    let mut line = String::new();
    input.read_line(&mut line)?;
    if line.is_empty() {
        return Err(AppError::Experiment("MCP server closed before responding".to_owned()));
    }
    Ok(serde_json::from_str(&line)?)
}

fn validate_product_sequence(responses: &[Value]) -> Result<(), AppError> {
    if responses.len() != 4 {
        return Err(AppError::Experiment(format!(
            "MCP sequence returned {} responses instead of 4",
            responses.len()
        )));
    }
    let first = responses[2].pointer("/result/structuredContent").ok_or_else(|| {
        AppError::Experiment(format!(
            "first MCP response has no structured content: {}",
            responses[2]
        ))
    })?;
    let second = responses[3].pointer("/result/structuredContent").ok_or_else(|| {
        AppError::Experiment(format!(
            "second MCP response has no structured content: {}",
            responses[3]
        ))
    })?;
    let first_valid = first.pointer("/resolution/kind").and_then(Value::as_str)
        == Some("partial_hit")
        && first.pointer("/resolution/artifact_ids").and_then(Value::as_array).map(Vec::len)
            == Some(2)
        && first.get("worker_spawned").and_then(Value::as_bool) == Some(true)
        && first.get("calibration").and_then(Value::as_bool) == Some(true);
    let second_valid = second.pointer("/resolution/kind").and_then(Value::as_str)
        == Some("coverage_hit")
        && second.get("worker_spawned").and_then(Value::as_bool) == Some(false)
        && second.get("cache_hit").and_then(Value::as_bool) == Some(true);
    if !first_valid || !second_valid {
        return Err(AppError::Experiment(
            "MCP sequence did not produce PartialHit followed by zero-worker CoverageHit"
                .to_owned(),
        ));
    }
    Ok(())
}

fn validate_observations(observations: &[Value]) -> Result<(), AppError> {
    if observations.len() != 2 {
        return Err(AppError::Experiment(format!(
            "expected two MCP observations, found {}",
            observations.len()
        )));
    }
    let first_valid = observations[0]
        .pointer("/cache_resolution/partial_hit/reused")
        .and_then(Value::as_array)
        .map(Vec::len)
        == Some(2)
        && observations[0].get("worker_spawned").and_then(Value::as_bool) == Some(true)
        && observations[0].get("calibration").and_then(Value::as_bool) == Some(true);
    let second_valid = observations[1]
        .pointer("/cache_resolution/coverage_hit/artifact_id")
        .and_then(Value::as_str)
        .is_some()
        && observations[1].get("worker_spawned").and_then(Value::as_bool) == Some(false);
    if first_valid && second_valid {
        Ok(())
    } else {
        Err(AppError::Experiment(
            "recorded MCP observations failed the partial/coverage gate".to_owned(),
        ))
    }
}

fn record_operator_cost(
    state: &SeedState,
    pricing: &PricingSnapshot,
    worker_model: &str,
    worker_reasoning: &str,
    service_tier: &str,
) -> Result<Option<TokenCost>, AppError> {
    let runs =
        state.store.worker_runs_after(0).map_err(|error| AppError::Runtime(error.to_string()))?;
    let Some(run) = runs.last() else {
        return Ok(None);
    };
    let cost = price_optional(
        pricing,
        worker_model,
        service_tier,
        run.input_tokens,
        run.cached_input_tokens,
        run.output_tokens,
    )?;
    if let (Some(cost), Some(result_digest)) = (&cost, run.result_digest) {
        let pricing_digest =
            pricing.digest().map_err(|error| AppError::Experiment(error.to_string()))?;
        state
            .store
            .record_operator_cost_observation(&OperatorCostObservation {
                artifact_kind: ArtifactKind::test_plan().0,
                worker_model: worker_model.to_owned(),
                worker_reasoning: worker_reasoning.to_owned(),
                service_tier: service_tier.to_owned(),
                schema_digest: state.worker_schema_digest,
                validator_definition_digest: validator_definition(&ArtifactKind::test_plan()),
                pricing_digest,
                requested_kind_count: 1,
                cost_microusd: cost.total_microcredits,
                execution_attempt_id: None,
                evidence_digest: result_digest,
                observed_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
            })
            .map_err(|error| AppError::Runtime(error.to_string()))?;
    }
    Ok(cost)
}

fn price_optional(
    pricing: &PricingSnapshot,
    model: &str,
    tier: &str,
    input: Option<u64>,
    cached: Option<u64>,
    output: Option<u64>,
) -> Result<Option<TokenCost>, AppError> {
    let (Some(input), Some(cached), Some(output)) = (input, cached, output) else {
        return Ok(None);
    };
    pricing
        .price_usage(model, tier, input, cached, output)
        .map(Some)
        .map_err(|error| AppError::Experiment(error.to_string()))
}

fn estimate_cost(
    pricing: &PricingSnapshot,
    main_model: &str,
    worker_model: &str,
    tier: &str,
) -> Result<u64, AppError> {
    let main = pricing
        .price_usage(
            main_model,
            tier,
            ESTIMATED_MAIN_INPUT_TOKENS,
            ESTIMATED_MAIN_CACHED_INPUT_TOKENS,
            ESTIMATED_MAIN_OUTPUT_TOKENS,
        )
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let worker = pricing
        .price_usage(
            worker_model,
            tier,
            ESTIMATED_WORKER_INPUT_TOKENS,
            ESTIMATED_WORKER_CACHED_INPUT_TOKENS,
            ESTIMATED_WORKER_OUTPUT_TOKENS,
        )
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    main.total_microcredits
        .checked_add(worker.total_microcredits)
        .ok_or_else(|| AppError::Experiment("cost estimate overflowed".to_owned()))
}

fn developer_instructions(requests: &[Value; 2]) -> Result<String, AppError> {
    Ok(format!(
        "Needle is the required context path. Before repository discovery or explanatory prose, call the configured need_context tool with request 1 below. After it succeeds, call need_context with request 2. Preserve every JSON field and value exactly. Do not run repository tools or repeat covered discovery. After both calls succeed, answer the user from the returned bounded context. If either call fails, stop and report it.\n\n<request_1>\n{}\n</request_1>\n<request_2>\n{}\n</request_2>",
        serde_json::to_string(&requests[0])?,
        serde_json::to_string(&requests[1])?,
    ))
}

fn trace_request() -> Value {
    serde_json::from_str(include_str!("../../../benchmarks/fixtures/mcp-request.json"))
        .expect("checked-in MCP fixture is valid JSON")
}

fn tests_request() -> Value {
    json!({
        "route": "tests.relevant",
        "subject": {"kind": "cli_option", "name": "--crlf"},
        "required": [{
            "kind": "focused_tests",
            "polarity": "positive",
            "selection": "representative",
            "completeness": "open_world"
        }],
        "preferred": [],
        "world": {"source": "current", "platform": "current", "features": "default"},
        "task": TESTS_TASK
    })
}

fn map_request(value: &Value) -> Result<MappedNeedContext, AppError> {
    let bytes = serde_json::to_vec(value)?.len();
    let request: McpNeedContextRequest = serde_json::from_value(value.clone())?;
    let routes = needle_core::built_in_route_contracts()
        .into_iter()
        .map(|route| route.route.as_str().to_owned())
        .collect::<Vec<_>>();
    request.validate_and_map(&routes, bytes).map_err(AppError::Experiment)
}

fn verify_pinned_clean_source(source: &Path) -> Result<(), AppError> {
    let head = Command::new("git").arg("-C").arg(source).args(["rev-parse", "HEAD"]).output()?;
    let sha = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    if !head.status.success() || sha != BENCHMARK_REPOSITORY_SHA {
        return Err(AppError::Experiment(format!(
            "partial-tests requires ripgrep at {BENCHMARK_REPOSITORY_SHA}; observed {sha}"
        )));
    }
    let status =
        Command::new("git").arg("-C").arg(source).args(["status", "--porcelain"]).output()?;
    if !status.status.success() || !status.stdout.is_empty() {
        return Err(AppError::Experiment(
            "partial-tests requires a clean pinned source checkout".to_owned(),
        ));
    }
    Ok(())
}

fn read_jsonl(path: &Path) -> Result<Vec<Value>, AppError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    BufReader::new(File::open(path)?)
        .lines()
        .filter(|line| line.as_ref().is_ok_and(|value| !value.trim().is_empty()))
        .map(|line| Ok(serde_json::from_str(&line?)?))
        .collect()
}

fn write_report(root: &Path, report: &Value) -> Result<(), AppError> {
    fs::write(root.join("report.json"), serde_json::to_vec_pretty(report)?)?;
    println!("{}", serde_json::to_string_pretty(report)?);
    Ok(())
}

fn mode(arguments: &[String]) -> Result<Mode, AppError> {
    let selected = [
        ("--preflight-only", Mode::Preflight),
        ("--execute-offline-simulator", Mode::Offline),
        ("--execute-paid", Mode::Paid),
    ]
    .into_iter()
    .filter(|(flag, _)| arguments.iter().any(|argument| argument == flag))
    .map(|(_, mode)| mode)
    .collect::<Vec<_>>();
    match selected.as_slice() {
        [mode] => Ok(*mode),
        _ => Err(AppError::Usage(
            "partial-tests-live requires exactly one of --preflight-only, --execute-offline-simulator or --execute-paid"
                .to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_partial_requests_are_distinct_and_semantically_valid() {
        let first = map_request(&trace_request()).unwrap();
        let second = map_request(&tests_request()).unwrap();
        assert_ne!(first.request_digest, second.request_digest);
        assert_eq!(first.request.route, "trace.state-flow");
        assert_eq!(second.request.route, "tests.relevant");
        assert_eq!(second.request.task, TESTS_TASK);
    }

    #[test]
    fn paid_estimate_uses_observed_main_and_conservative_worker_usage() {
        let pricing: PricingSnapshot = serde_json::from_slice(include_bytes!(
            "../../../fixtures/openai-codex-pricing-2026-07-27.json"
        ))
        .unwrap();

        assert_eq!(
            estimate_cost(&pricing, "gpt-5.6-sol", "gpt-5.6-luna", "default").unwrap(),
            9_632_845
        );
    }
}
