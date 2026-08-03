use super::{
    AppError, HookConfig, absolute_run_path, canonical_child_path, ensure_cache_pilot_hook_binary,
    ensure_codex_authenticated, ensure_dedicated_codex_home, ensure_product_pilot_hook_isolation,
    option_value, price_usage_observation_optional, repository_status_clean, required_value,
    resolve_codex, validate_model_value, validate_reasoning, validate_service_tier,
};
use crate::minimal_live_pilot::protocol::{
    DEFAULT_MANIFEST, DEFAULT_PRICING, Protocol, load_pricing, load_protocol, quality_spec,
    test_plan, validate_source, workspace_path,
};
use needle_bench::{PricingSnapshot, QualityOracleResult, TokenCost};
use needle_core::{
    Digest, EvidenceFailurePolicy, NeedIr, SemanticInterrupt, WorkerConfig, WorkerFailure,
    WorkerOutcome, WorkerRequest,
};
use needle_platform_codex::{CodexWorker, TransportPreflightReport, WorkerDiagnosticContract};
use needle_runtime::{
    ResolveRequest, RuntimeEngine, RuntimeSettings, RuntimeStore, WorkerExecutor,
    bind_evidence_digests, capture_git_snapshot, validate_need_result,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

mod evaluation;
use evaluation::{
    SemanticReuseAssessment, assess_semantic_reuse, evaluate_structured_worker_quality,
    unavailable_quality, unavailable_semantic_reuse,
};

const REPORT_SCHEMA: &str = "needle.worker-live-diagnostic-report/2";
const PREFLIGHT_SCHEMA: &str = "needle.worker-live-diagnostic-preflight/1";
const CONTRACT_FILE: &str = "worker-contract.json";
const REQUEST_FILE: &str = "worker-request.json";
const REPORT_FILE: &str = "worker-live-diagnostic-report.json";
const MAXIMUM_WORKER_TURNS: u32 = 1;
const OBSERVED_WORKER_BUDGET_MICROCREDITS: u64 = 2_034_130;
const WORKER_MARKER: &str = r#"@@need
@route locate.implementation
@subject cli-option:"--glob-case-insensitive"
@require implementation-location granularity=exact-location polarity=positive selection=primary
@prefer focused-tests selection=representative
@constraint Identify the primary implementation location and one focused test that directly demonstrates case-insensitive glob matching; return only minimal supporting evidence.
@world source=current features=default
@project detail=compact

Locate the option's implementation and the single most focused behavioral test, including exact file and symbol or line anchors.
@@end"#;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerLiveDiagnosticPreflight {
    schema: String,
    passed: bool,
    live_calls_started: u32,
    provider_turns_started: u32,
    artifact_root_available: bool,
    task_id: String,
    route: String,
    repository_sha: String,
    source_snapshot_digest: Digest,
    codex_version: String,
    worker_model: String,
    worker_reasoning: String,
    service_tier: String,
    pricing_snapshot_digest: Digest,
    hook_binary_current: bool,
    dedicated_auth_available: bool,
    transport: TransportPreflightReport,
    contract: WorkerDiagnosticContract,
    maximum_logical_workers: u32,
    maximum_worker_turns: u32,
    automatic_retries: u32,
    repair_enabled: bool,
    native_fallback: bool,
    main_observations: u32,
    estimated_budget_microcredits: u64,
    estimate_basis: String,
    estimate_is_hard_provider_ceiling: bool,
    execute_flag_required: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerLiveDiagnosticReport {
    schema: String,
    mode: String,
    task_id: String,
    route: String,
    repository_sha: String,
    source_snapshot_digest: Digest,
    codex_version: String,
    worker_model: String,
    worker_reasoning: String,
    service_tier: String,
    pricing_snapshot_digest: Digest,
    contract: WorkerDiagnosticContract,
    estimated_budget_microcredits: u64,
    approved_budget_microcredits: u64,
    estimate_basis: String,
    maximum_logical_workers: u32,
    maximum_worker_turns: u32,
    automatic_retries: u32,
    repair_enabled: bool,
    native_fallback: bool,
    main_observations: u32,
    worker_succeeded: bool,
    worker_failure: Option<WorkerFailure>,
    worker_outcome: Option<WorkerOutcome>,
    evidence_validation_error: Option<String>,
    quality: QualityOracleResult,
    semantic_reuse: SemanticReuseAssessment,
    command_evidence_before: u64,
    command_evidence_after: u64,
    command_evidence_delta: u64,
    focused_test_evidence_valid: bool,
    checkout_clean: bool,
    observed_cost: Option<TokenCost>,
    wall_time_ms: u64,
    passed: bool,
    failures: Vec<String>,
}

pub(super) fn run(arguments: &[String]) -> Result<(), AppError> {
    let preflight_only = arguments.iter().any(|argument| argument == "--preflight-only");
    let execute_paid = arguments.iter().any(|argument| argument == "--execute-paid");
    if preflight_only == execute_paid {
        return Err(AppError::Usage(
            "worker-diagnostic-live requires exactly one of --preflight-only or --execute-paid"
                .to_owned(),
        ));
    }

    let codex = resolve_codex(option_value(arguments, "--codex"))?;
    let codex_home = PathBuf::from(required_value(arguments, "--codex-home")?);
    ensure_dedicated_codex_home(&codex_home)?;
    let codex_home = canonical_child_path(&codex_home)?;
    ensure_product_pilot_hook_isolation(&codex_home)?;
    ensure_cache_pilot_hook_binary(&codex_home)?;
    ensure_codex_authenticated(&codex, &codex_home, "worker-diagnostic-live")?;
    let source_repository =
        canonical_child_path(Path::new(&required_value(arguments, "--source-repository")?))?;
    let artifact_root =
        absolute_run_path(Path::new(&required_value(arguments, "--artifact-root")?))?;
    if artifact_root.exists() {
        return Err(AppError::Experiment(format!(
            "worker diagnostic artifact root already exists: {}",
            artifact_root.display()
        )));
    }

    let worker_model = required_value(arguments, "--worker-model")?;
    let worker_reasoning = required_value(arguments, "--worker-reasoning")?;
    let service_tier = required_value(arguments, "--service-tier")?;
    validate_model_value(&worker_model, "worker model")?;
    validate_reasoning(&worker_reasoning)?;
    validate_service_tier(&service_tier)?;
    let timeout_seconds = option_value(arguments, "--timeout-seconds")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid --timeout-seconds: {error}")))?
        .unwrap_or(600);
    if timeout_seconds < 180 {
        return Err(AppError::Usage(
            "worker-diagnostic-live --timeout-seconds must be at least 180".to_owned(),
        ));
    }

    let manifest_path = option_value(arguments, "--corpus")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_path(DEFAULT_MANIFEST));
    let pricing_path = option_value(arguments, "--pricing-snapshot")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_path(DEFAULT_PRICING));
    let protocol = load_protocol(&manifest_path)?;
    validate_source(&source_repository, protocol.task().repository_sha.as_str())?;
    let pricing = load_pricing(&pricing_path, &protocol.cost_model)?;
    let pricing_digest = pricing.digest()?;
    pricing
        .price_usage(&worker_model, &service_tier, 0, 0, 0)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let isolation =
        CodexWorker::verify_isolation(&codex.display().to_string()).map_err(AppError::Runtime)?;
    if !isolation.verified() {
        return Err(AppError::Experiment(format!(
            "worker isolation is not verified for Codex {}",
            isolation.codex_version
        )));
    }

    let estimated_budget = worker_budget(&protocol)?;
    let config = WorkerConfig {
        executable: codex.display().to_string(),
        model: worker_model.clone(),
        reasoning: worker_reasoning.clone(),
        service_tier: Some(service_tier.clone()),
        timeout_seconds,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
    };
    let temporary = TemporaryRunRoot::create(&artifact_root)?;
    let request = capture_worker_request(temporary.path(), &source_repository, &protocol, &config)?;
    let contract = CodexWorker::diagnostic_contract(&request).map_err(AppError::Runtime)?;
    validate_contract(&request, &contract)?;

    if preflight_only {
        let transport_worker =
            CodexWorker::with_codex_home(temporary.path().join("transport"), &codex_home);
        let transport = transport_worker
            .preflight_transport_for_test_plan(
                &config,
                &source_repository,
                request.need_key.as_str(),
                request.declared_test_plan.clone(),
                request.trusted_test_execution,
            )
            .map_err(AppError::Runtime)?;
        let (_, snapshot) = capture_git_snapshot(&source_repository)
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        let transport_matches_request = transport.source_head_sha == protocol.task().repository_sha
            && transport.source_snapshot_digest
                == request.repository_snapshot.source_digest.to_string();
        let output = WorkerLiveDiagnosticPreflight {
            schema: PREFLIGHT_SCHEMA.to_owned(),
            passed: transport.provider_turns_started == 0
                && transport.ephemeral_thread_cleanup_completed
                && transport.sandbox_cleaned
                && transport_matches_request,
            live_calls_started: 0,
            provider_turns_started: transport.provider_turns_started,
            artifact_root_available: true,
            task_id: protocol.task().id.clone(),
            route: "locate.implementation".to_owned(),
            repository_sha: protocol.task().repository_sha.clone(),
            source_snapshot_digest: snapshot.source_digest,
            codex_version: isolation.codex_version,
            worker_model,
            worker_reasoning,
            service_tier,
            pricing_snapshot_digest: pricing_digest,
            hook_binary_current: true,
            dedicated_auth_available: true,
            transport,
            contract,
            maximum_logical_workers: 1,
            maximum_worker_turns: MAXIMUM_WORKER_TURNS,
            automatic_retries: 0,
            repair_enabled: false,
            native_fallback: false,
            main_observations: 0,
            estimated_budget_microcredits: estimated_budget,
            estimate_basis: estimate_basis(),
            estimate_is_hard_provider_ceiling: false,
            execute_flag_required: "--execute-paid".to_owned(),
        };
        let rendered = serde_json::to_vec_pretty(&output)?;
        if let Some(destination) = option_value(arguments, "--output") {
            let destination = absolute_run_path(Path::new(&destination))?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&destination, rendered)?;
            println!("worker diagnostic preflight written to {}", destination.display());
        } else {
            println!("{}", String::from_utf8_lossy(&rendered));
        }
        return Ok(());
    }

    let approved_budget = required_value(arguments, "--approved-budget-microcredits")?
        .parse::<u64>()
        .map_err(|error| AppError::Usage(format!("invalid approved budget: {error}")))?;
    if approved_budget != estimated_budget {
        return Err(AppError::Experiment(format!(
            "approved budget must equal the observed worker estimate {estimated_budget} microcredits"
        )));
    }
    execute(Execution {
        codex_home: &codex_home,
        source_repository: &source_repository,
        artifact_root: &artifact_root,
        protocol: &protocol,
        pricing: &pricing,
        pricing_digest,
        codex_version: &isolation.codex_version,
        worker_model: &worker_model,
        worker_reasoning: &worker_reasoning,
        service_tier: &service_tier,
        config: &config,
        request,
        contract,
        estimated_budget,
        approved_budget,
    })
}

struct Execution<'a> {
    codex_home: &'a Path,
    source_repository: &'a Path,
    artifact_root: &'a Path,
    protocol: &'a Protocol,
    pricing: &'a PricingSnapshot,
    pricing_digest: Digest,
    codex_version: &'a str,
    worker_model: &'a str,
    worker_reasoning: &'a str,
    service_tier: &'a str,
    config: &'a WorkerConfig,
    request: WorkerRequest,
    contract: WorkerDiagnosticContract,
    estimated_budget: u64,
    approved_budget: u64,
}

fn execute(context: Execution<'_>) -> Result<(), AppError> {
    fs::create_dir_all(context.artifact_root)?;
    let artifact_root = canonical_child_path(context.artifact_root)?;
    fs::write(artifact_root.join(CONTRACT_FILE), serde_json::to_vec_pretty(&context.contract)?)?;
    fs::write(artifact_root.join(REQUEST_FILE), serde_json::to_vec_pretty(&context.request)?)?;
    fs::write(
        artifact_root.join("pricing-snapshot.json"),
        serde_json::to_vec_pretty(context.pricing)?,
    )?;

    let (_, initial_snapshot) = capture_git_snapshot(context.source_repository)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let worker_data = artifact_root.join("worker-data");
    fs::create_dir_all(&worker_data)?;
    let store = RuntimeStore::new(worker_data.join("needle.sqlite3"));
    store.initialize().map_err(|error| AppError::Experiment(error.to_string()))?;
    let evidence_before =
        store.command_evidence_count().map_err(|error| AppError::Experiment(error.to_string()))?;
    let worker = CodexWorker::with_codex_home(&worker_data, context.codex_home);
    let started = Instant::now();
    let result = worker.execute(context.config, &context.request);
    let wall_time_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    let evidence_after =
        store.command_evidence_count().map_err(|error| AppError::Experiment(error.to_string()))?;
    let evidence_delta = evidence_after.saturating_sub(evidence_before);
    let focused_test_evidence_valid = evidence_delta == 1;
    let (_, final_snapshot) = capture_git_snapshot(context.source_repository)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let checkout_clean = initial_snapshot.source_digest == final_snapshot.source_digest
        && repository_status_clean(context.source_repository)?;
    let oracle = quality_spec(context.protocol)?;

    let (mut outcome, failure) = match result {
        Ok(outcome) => (Some(outcome), None),
        Err(failure) => (None, Some(*failure)),
    };
    let evidence_validation_error = outcome.as_mut().and_then(|outcome| {
        bind_evidence_digests(context.source_repository, &mut outcome.result)
            .and_then(|()| validate_need_result(context.source_repository, &outcome.result))
            .err()
            .map(|error| error.to_string())
    });
    let result_evidence_valid = outcome.is_some() && evidence_validation_error.is_none();
    let implementation_paths = context
        .protocol
        .oracle()
        .evidence
        .iter()
        .filter(|evidence| evidence.role != "test")
        .map(|evidence| evidence.path.clone())
        .collect::<BTreeSet<_>>();
    let quality = outcome.as_ref().filter(|_| result_evidence_valid).map_or_else(
        unavailable_quality,
        |outcome| {
            evaluate_structured_worker_quality(
                &oracle,
                outcome.semantic_artifact_result.as_ref(),
                context.source_repository,
                &implementation_paths,
                context
                    .request
                    .declared_test_plan
                    .as_ref()
                    .map(|plan| plan.test_identifier.as_str()),
                focused_test_evidence_valid,
            )
        },
    );
    let semantic_reuse = outcome.as_ref().map_or_else(unavailable_semantic_reuse, |outcome| {
        assess_semantic_reuse(
            context.request.semantic_fragment.as_ref(),
            outcome.semantic_artifact_result.as_ref(),
            context.source_repository,
            context.request.repository_snapshot.source_digest,
            &implementation_paths,
        )
    });
    let usage = outcome
        .as_ref()
        .map(|outcome| (outcome.input_tokens, outcome.cached_input_tokens, outcome.output_tokens))
        .or_else(|| {
            failure.as_ref().map(|failure| {
                (failure.input_tokens, failure.cached_input_tokens, failure.output_tokens)
            })
        })
        .unwrap_or((None, None, None));
    let observed_cost = price_usage_observation_optional(
        context.pricing,
        context.worker_model,
        context.service_tier,
        usage.0,
        usage.1,
        usage.2,
    )?;
    let worker_succeeded = outcome.is_some();
    let attempt = attempt_metrics(outcome.as_ref(), failure.as_ref());
    let exactly_one_turn = attempt.is_some_and(|attempt| {
        attempt.logical_worker_spawns == 1
            && attempt.worker_turns == MAXIMUM_WORKER_TURNS
            && !attempt.repair_performed
    });
    let cleanup_succeeded = attempt.and_then(|attempt| attempt.cleanup_success) == Some(true);
    let mut failures = Vec::new();
    for (passed, failure_code) in [
        (worker_succeeded, "worker_execution"),
        (result_evidence_valid, "result_evidence"),
        (exactly_one_turn, "worker_turn_count"),
        (focused_test_evidence_valid, "focused_test_evidence"),
        (quality.passed, "quality"),
        (semantic_reuse.ready, "semantic_reuse"),
        (checkout_clean, "checkout_integrity"),
        (cleanup_succeeded, "worker_cleanup"),
        (observed_cost.is_some(), "usage_or_pricing"),
    ] {
        if !passed {
            failures.push(failure_code.to_owned());
        }
    }
    let report = WorkerLiveDiagnosticReport {
        schema: REPORT_SCHEMA.to_owned(),
        mode: "provider-live-worker-only".to_owned(),
        task_id: context.protocol.task().id.clone(),
        route: "locate.implementation".to_owned(),
        repository_sha: context.protocol.task().repository_sha.clone(),
        source_snapshot_digest: initial_snapshot.source_digest,
        codex_version: context.codex_version.to_owned(),
        worker_model: context.worker_model.to_owned(),
        worker_reasoning: context.worker_reasoning.to_owned(),
        service_tier: context.service_tier.to_owned(),
        pricing_snapshot_digest: context.pricing_digest,
        contract: context.contract,
        estimated_budget_microcredits: context.estimated_budget,
        approved_budget_microcredits: context.approved_budget,
        estimate_basis: estimate_basis(),
        maximum_logical_workers: 1,
        maximum_worker_turns: MAXIMUM_WORKER_TURNS,
        automatic_retries: 0,
        repair_enabled: false,
        native_fallback: false,
        main_observations: 0,
        worker_succeeded,
        worker_failure: failure,
        worker_outcome: outcome,
        evidence_validation_error,
        quality,
        semantic_reuse,
        command_evidence_before: evidence_before,
        command_evidence_after: evidence_after,
        command_evidence_delta: evidence_delta,
        focused_test_evidence_valid,
        checkout_clean,
        observed_cost,
        wall_time_ms,
        passed: failures.is_empty(),
        failures,
    };
    let report_path = artifact_root.join(REPORT_FILE);
    fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("worker live diagnostic report written to {}", report_path.display());
    if report.passed {
        Ok(())
    } else {
        let technical = report
            .worker_failure
            .as_ref()
            .map(|failure| format!("{}: {}", failure.code, failure.diagnostic))
            .or_else(|| {
                report
                    .evidence_validation_error
                    .as_ref()
                    .map(|error| format!("result evidence validation: {error}"))
            })
            .unwrap_or_else(|| "worker completed but the diagnostic gate failed".to_owned());
        Err(AppError::Experiment(format!(
            "worker live diagnostic failed ({technical}); gates: {}",
            report.failures.join(", ")
        )))
    }
}

#[derive(Clone)]
struct CaptureWorker {
    request: Arc<Mutex<Option<WorkerRequest>>>,
}

impl WorkerExecutor for CaptureWorker {
    fn execute(
        &self,
        _config: &WorkerConfig,
        request: &WorkerRequest,
    ) -> Result<WorkerOutcome, Box<WorkerFailure>> {
        *self.request.lock().expect("capture mutex poisoned") = Some(request.clone());
        Err(Box::new(WorkerFailure {
            code: "diagnostic_capture".to_owned(),
            diagnostic: "worker request captured before provider execution".to_owned(),
            input_tokens: None,
            cached_input_tokens: None,
            output_tokens: None,
            duration_ms: 0,
            logical_worker_spawns: 0,
            worker_turns: 0,
            repair_performed: false,
            discarded_facts: 0,
            worker_session_id: None,
            session_cleanup_success: None,
        }))
    }
}

fn capture_worker_request(
    data_root: &Path,
    repository: &Path,
    protocol: &Protocol,
    config: &WorkerConfig,
) -> Result<WorkerRequest, AppError> {
    let store = RuntimeStore::new(data_root.join("capture.sqlite3"));
    store
        .initialize_defaults(&RuntimeSettings {
            codex_executable: config.executable.clone(),
            worker_model: config.model.clone(),
            worker_reasoning: config.reasoning.clone(),
            worker_timeout_seconds: config.timeout_seconds,
            evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
            trusted_test_execution: true,
            multi_need_policy: needle_core::MultiNeedPolicy::default(),
        })
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let profile =
        HookConfig::default().profile().map_err(|error| AppError::Experiment(error.to_string()))?;
    let session = "worker-diagnostic-capture";
    let turn = "worker-diagnostic-turn";
    store
        .record_session_start(
            session,
            profile.definition_digest,
            Some("diagnostic-main"),
            repository.to_str(),
        )
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    store
        .record_user_prompt(session, Some(turn), &protocol.task().prompt, repository.to_str())
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let need_ir = NeedIr::parse(WORKER_MARKER)
        .map_err(|error| AppError::Experiment(format!("worker marker is invalid: {error}")))?
        .ok_or_else(|| AppError::Experiment("worker marker was not recognized".to_owned()))?;
    let need = SemanticInterrupt::Typed {
        need_ir: need_ir.clone(),
        coordination: needle_core::NeedCoordination::WaitResponse,
    }
    .compatibility_request();
    let captured = Arc::new(Mutex::new(None));
    let engine = RuntimeEngine::new(store, CaptureWorker { request: Arc::clone(&captured) });
    let result = engine.resolve(&ResolveRequest {
        session_id: session.to_owned(),
        turn_id: turn.to_owned(),
        platform: "codex".to_owned(),
        main_model: "diagnostic-main".to_owned(),
        cwd: repository.to_owned(),
        need,
        need_ir: Some(need_ir),
        declared_test_plan: Some(test_plan(protocol.task())),
    });
    if !result.is_err_and(|error| error.to_string().contains("diagnostic_capture")) {
        return Err(AppError::Experiment(
            "worker request capture did not stop at the provider boundary".to_owned(),
        ));
    }
    captured
        .lock()
        .map_err(|_| AppError::Experiment("worker request capture mutex was poisoned".to_owned()))?
        .clone()
        .ok_or_else(|| AppError::Experiment("worker request was not captured".to_owned()))
}

fn validate_contract(
    request: &WorkerRequest,
    contract: &WorkerDiagnosticContract,
) -> Result<(), AppError> {
    let plan = request
        .declared_test_plan
        .as_ref()
        .ok_or_else(|| AppError::Experiment("diagnostic worker has no TestPlan".to_owned()))?;
    if request.semantic_fragment.is_none()
        || contract.output_schema_id != needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID
        || contract.requested_artifact_kinds != [needle_core::ArtifactKind::code_location()]
        || !contract.prompt.contains(&plan.argv.join(" "))
        || !contract.prompt.contains("--glob-case-insensitive")
        || contains_json_key(&contract.output_schema, "oneOf")
    {
        return Err(AppError::Experiment(
            "diagnostic worker contract differs from the frozen one-turn semantic request"
                .to_owned(),
        ));
    }
    Ok(())
}

fn contains_json_key(value: &serde_json::Value, key: &str) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            object.contains_key(key) || object.values().any(|value| contains_json_key(value, key))
        }
        serde_json::Value::Array(values) => {
            values.iter().any(|value| contains_json_key(value, key))
        }
        _ => false,
    }
}

fn worker_budget(protocol: &Protocol) -> Result<u64, AppError> {
    let frontier = arm_cost(protocol, needle_bench::FinalArm::FrontierDirect)?;
    let proxy = arm_cost(protocol, needle_bench::FinalArm::NativeSubagent)?;
    let budget = proxy.checked_sub(frontier).ok_or_else(|| {
        AppError::Experiment("native-subagent proxy is below frontier-direct cost".to_owned())
    })?;
    if budget != OBSERVED_WORKER_BUDGET_MICROCREDITS {
        return Err(AppError::Experiment(format!(
            "worker-only diagnostic estimate changed: expected {OBSERVED_WORKER_BUDGET_MICROCREDITS}, got {budget}"
        )));
    }
    Ok(budget)
}

fn arm_cost(protocol: &Protocol, arm: needle_bench::FinalArm) -> Result<u64, AppError> {
    protocol
        .cost_model
        .arm_estimates
        .iter()
        .find(|estimate| estimate.arm == arm)
        .map(|estimate| estimate.microcredits_per_observation)
        .ok_or_else(|| AppError::Experiment(format!("cost estimate is missing {arm:?}")))
}

fn estimate_basis() -> String {
    "Maximum observed single-worker cost: 2.034130 credits in an accepted historical calibration; not a provider-enforced ceiling."
        .to_owned()
}

#[derive(Clone, Copy)]
struct AttemptMetrics {
    logical_worker_spawns: u32,
    worker_turns: u32,
    repair_performed: bool,
    cleanup_success: Option<bool>,
}

fn attempt_metrics(
    outcome: Option<&WorkerOutcome>,
    failure: Option<&WorkerFailure>,
) -> Option<AttemptMetrics> {
    outcome
        .map(|outcome| AttemptMetrics {
            logical_worker_spawns: outcome.logical_worker_spawns,
            worker_turns: outcome.worker_turns,
            repair_performed: outcome.repair_performed,
            cleanup_success: outcome.session_cleanup_success,
        })
        .or_else(|| {
            failure.map(|failure| AttemptMetrics {
                logical_worker_spawns: failure.logical_worker_spawns,
                worker_turns: failure.worker_turns,
                repair_performed: failure.repair_performed,
                cleanup_success: failure.session_cleanup_success,
            })
        })
}

struct TemporaryRunRoot {
    path: PathBuf,
}

impl TemporaryRunRoot {
    fn create(artifact_root: &Path) -> Result<Self, AppError> {
        let parent = artifact_root.parent().ok_or_else(|| {
            AppError::Experiment("worker diagnostic artifact root has no parent".to_owned())
        })?;
        let parent = canonical_child_path(parent)?;
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let path = parent.join(format!(".ndw-{}-{suffix:x}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self { path: canonical_child_path(&path)? })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryRunRoot {
    fn drop(&mut self) {
        if self
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.starts_with(".ndw-"))
        {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_budget_matches_the_accepted_historical_component() {
        let protocol = load_protocol(&workspace_path(DEFAULT_MANIFEST)).unwrap();
        assert_eq!(worker_budget(&protocol).unwrap(), OBSERVED_WORKER_BUDGET_MICROCREDITS);
    }

    #[test]
    fn frozen_marker_is_unversioned_and_semantic() {
        let need = NeedIr::parse(WORKER_MARKER).unwrap().unwrap();
        assert_eq!(
            need.route_hint.as_ref().map(needle_core::NeedKey::as_str),
            Some("locate.implementation")
        );
        assert!(WORKER_MARKER.starts_with("@@need\n"));
        assert!(!WORKER_MARKER.contains("@@need:"));
    }

    #[test]
    fn failed_worker_attempt_retains_turn_and_cleanup_accounting() {
        let failure = WorkerFailure {
            code: "worker_turn_failed".to_owned(),
            diagnostic: "invalid schema".to_owned(),
            input_tokens: None,
            cached_input_tokens: None,
            output_tokens: None,
            duration_ms: 1,
            logical_worker_spawns: 1,
            worker_turns: 1,
            repair_performed: false,
            discarded_facts: 0,
            worker_session_id: Some("thread".to_owned()),
            session_cleanup_success: Some(true),
        };
        let metrics = attempt_metrics(None, Some(&failure)).unwrap();
        assert_eq!(metrics.logical_worker_spawns, 1);
        assert_eq!(metrics.worker_turns, 1);
        assert!(!metrics.repair_performed);
        assert_eq!(metrics.cleanup_success, Some(true));
    }
}
