use super::{
    AppError, HookConfig, absolute_run_path, canonical_child_path, clone_local_checkout,
    ensure_cache_pilot_hook_binary, ensure_codex_authenticated, ensure_dedicated_codex_home,
    ensure_product_pilot_hook_isolation, option_value, price_usage_observation_optional,
    provision_experiment_role_profile, repository_status_clean, required_value, resolve_codex,
    validate_model_value, validate_reasoning, validate_service_tier, validate_slug,
};
use needle_bench::{
    BenchmarkRoute, CachePilotResolveOutcome, ECONOMIC_EQUIVALENT_HIT_PROMPT, FinalArm,
    PricingSnapshot, ProcessExecutionStatus, QualityOracleResult, QualityOracleSpec,
    REWORDED_COVERAGE_HIT_PROMPT, TokenCost,
};
use needle_core::{
    CacheResolution, CapabilityMode, Digest, EvidenceFailurePolicy, ModelPolicy, MultiNeedPolicy,
    NeedStep, PredicateKind, ReuseUnit, RoleProfileId, SelectedPlan, SemanticWorkerArtifact,
    WorkerConfig, WorkerProfile,
};
use needle_platform_codex::{CodexWorker, TransportPreflightReport};
use needle_runtime::{
    NeedStepRequestRecord, PROOF_RESOLUTION_FORMAT_REVISION, RouteCostObservation, RuntimeSettings,
    RuntimeStore, capture_git_snapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod direct_main;
mod multi_task_campaign;
pub(crate) mod protocol;
mod supervised_main;

use direct_main::{DirectObservation, observe as observe_direct_main};
use protocol::{
    DEFAULT_LEGACY_OFFLINE_MANIFEST, DEFAULT_MANIFEST, DEFAULT_PRICING,
    MULTI_NEED_EXTRA_MAIN_TURN_RESERVES, MULTI_NEED_EXTRA_WORKER_RESERVES,
    MULTI_NEED_MAIN_TURN_RESERVE_MICROCREDITS, MULTI_NEED_WORKER_RESERVE_MICROCREDITS, Protocol,
    TRACE_REUSE_TASK_ID, coverage_hit_quality_spec, load_legacy_offline_protocol, load_pricing,
    load_protocol, quality_spec, test_plan, validate_source, workspace_path,
};
use supervised_main::SupervisedMain;

const REPORT_SCHEMA: &str = "needle.minimal-live-pilot-report/8";
const HIT_RESOLUTION_TARGET: &str = "CoverageHit";
const MAXIMUM_MISS_NEEDS: u8 = 2;
const MAXIMUM_MISS_LOGICAL_WORKERS: u8 = 1;
const MAXIMUM_HIT_NEEDS: u8 = 2;
const MAXIMUM_HIT_LOGICAL_WORKERS: u8 = 1;
const MAXIMUM_MAIN_TURNS_PER_NEEDLE_ARM: u32 = 3;
const MAXIMUM_LEGACY_STAGE_MAIN_TURNS: u32 = 6;
const MAXIMUM_ECONOMIC_STAGE_MAIN_TURNS: u32 = 7;
const OFFLINE_MAIN_SCENARIO: &str = "main_interrupt_r59_covered_repeat";
const TRACE_OFFLINE_MAIN_SCENARIO: &str = "main_interrupt_r61_trace";
const TRACE_OFFLINE_WORKER_SCENARIO: &str = "worker_r61_trace";
const TRACE_REUSE_PUBLICATION_PROMPT: &str = "Trace how ripgrep's --crlf option changes matching and search line terminators in the default scenario, and identify a focused test for that behavior.";
const TRACE_REUSE_EQUIVALENT_HIT_PROMPT: &str = "For ripgrep's --crlf option, explain the default runtime flow from option handling to matcher and searcher line terminators, including a representative focused test.";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MinimalLiveArm {
    arm: FinalArm,
    observed_arm: FinalArm,
    provider_observation_started: bool,
    transport_success: bool,
    process_success: bool,
    process: ProcessExecutionStatus,
    quality: QualityOracleResult,
    resolve: Option<CachePilotResolveOutcome>,
    #[serde(default)]
    resolve_sequence: Vec<CachePilotResolveOutcome>,
    #[serde(default)]
    need_steps: Vec<NeedStep>,
    #[serde(default)]
    need_requests: Vec<NeedStepRequestRecord>,
    worker_runs_before: u64,
    worker_runs_after: u64,
    worker_run_delta: u64,
    logical_worker_spawns: u32,
    worker_turns: u32,
    repair_performed: bool,
    command_evidence_before: u64,
    command_evidence_after: u64,
    command_evidence_delta: u64,
    main_discovery_total: u32,
    wall_time_ms: u64,
    main_input_tokens: Option<u64>,
    main_cached_input_tokens: Option<u64>,
    main_output_tokens: Option<u64>,
    worker_input_tokens: Option<u64>,
    worker_cached_input_tokens: Option<u64>,
    worker_output_tokens: Option<u64>,
    main_cost: Option<TokenCost>,
    worker_cost: Option<TokenCost>,
    #[serde(default)]
    selected_plan: Option<SelectedPlan>,
}

impl MinimalLiveArm {
    fn total_cost_microcredits(&self) -> Option<u64> {
        let main = self.main_cost.as_ref()?.total_microcredits;
        if !self.worker_execution_observed() {
            return Some(main);
        }
        main.checked_add(self.worker_cost.as_ref()?.total_microcredits)
    }

    fn worker_execution_observed(&self) -> bool {
        self.worker_run_delta > 0 || self.logical_worker_spawns > 0 || self.worker_turns > 0
    }

    fn skipped(arm: FinalArm, reason: &str, workers: u64, evidence: u64) -> Self {
        Self {
            arm,
            observed_arm: arm,
            provider_observation_started: false,
            transport_success: false,
            process_success: false,
            process: ProcessExecutionStatus {
                status: format!("skipped:{reason}"),
                ..ProcessExecutionStatus::default()
            },
            quality: unavailable_quality(),
            resolve: None,
            resolve_sequence: Vec::new(),
            need_steps: Vec::new(),
            need_requests: Vec::new(),
            worker_runs_before: workers,
            worker_runs_after: workers,
            worker_run_delta: 0,
            logical_worker_spawns: 0,
            worker_turns: 0,
            repair_performed: false,
            command_evidence_before: evidence,
            command_evidence_after: evidence,
            command_evidence_delta: 0,
            main_discovery_total: 0,
            wall_time_ms: 0,
            main_input_tokens: None,
            main_cached_input_tokens: None,
            main_output_tokens: None,
            worker_input_tokens: None,
            worker_cached_input_tokens: None,
            worker_output_tokens: None,
            main_cost: None,
            worker_cost: None,
            selected_plan: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MinimalLivePilotReport {
    schema: String,
    mode: String,
    task_id: String,
    route: String,
    source_sha: String,
    source_snapshot_digest: Digest,
    codex_version: String,
    main_model: String,
    worker_model: String,
    service_tier: String,
    pricing_snapshot_digest: Digest,
    estimated_budget_microcredits: u64,
    approved_budget_microcredits: u64,
    automatic_retries: bool,
    maximum_main_observations: u32,
    maximum_main_turns: u32,
    maximum_logical_workers: u32,
    maximum_worker_turns: u32,
    provider_observations_started: u32,
    capability_mode: Option<CapabilityMode>,
    bootstrap_evidence_digest: Digest,
    validation_certificates: u64,
    sufficiency_certificates: u64,
    checkout_clean: bool,
    hit_prompt_reworded: bool,
    hit_resolution_target: String,
    #[serde(default)]
    main_only: Option<MinimalLiveArm>,
    miss: MinimalLiveArm,
    hit: MinimalLiveArm,
    #[serde(default)]
    economic_contract_digest: Option<Digest>,
    #[serde(default)]
    economic_semantics_equivalent: Option<bool>,
    #[serde(default)]
    routing_saving_basis_points: Option<i64>,
    #[serde(default)]
    cache_saving_basis_points: Option<i64>,
    #[serde(default)]
    total_saving_basis_points: Option<i64>,
    cache_behavior_suite: String,
    quality_passed: bool,
    cache_candidate_valid: bool,
    proof_gated_hit: bool,
    production_policy_reason: Option<String>,
    hit_execution: String,
    coverage_zero_worker: bool,
    exactly_one_worker: bool,
    worker_count_bounded: bool,
    zero_repair: bool,
    zero_main_discovery: bool,
    focused_test_execution_required: bool,
    focused_test_execution_observed: bool,
    focused_test_evidence_valid: bool,
    same_artifact: bool,
    costs_available: bool,
    hit_cost_lower: bool,
    observed_miss_cost_microcredits: Option<u64>,
    observed_hit_cost_microcredits: Option<u64>,
    passed: bool,
    failures: Vec<String>,
}

pub(super) fn run(arguments: &[String]) -> Result<(), AppError> {
    let preflight_only = arguments.iter().any(|argument| argument == "--preflight-only");
    let execute_paid = arguments.iter().any(|argument| argument == "--execute-paid");
    let economic_preflight_only =
        arguments.iter().any(|argument| argument == "--economic-preflight-only");
    let execute_economic_paid =
        arguments.iter().any(|argument| argument == "--execute-economic-paid");
    let execute_legacy_offline =
        arguments.iter().any(|argument| argument == "--execute-offline-simulator");
    let execute_economic_offline =
        arguments.iter().any(|argument| argument == "--execute-economic-offline-simulator");
    let multi_task_preflight_only =
        arguments.iter().any(|argument| argument == "--multi-task-preflight-only");
    let execute_multi_task_paid =
        arguments.iter().any(|argument| argument == "--execute-multi-task-paid");
    let execute_multi_task_offline =
        arguments.iter().any(|argument| argument == "--execute-multi-task-offline-simulator");
    let trace_reuse_preflight_only =
        arguments.iter().any(|argument| argument == "--trace-reuse-preflight-only");
    let execute_trace_reuse_paid =
        arguments.iter().any(|argument| argument == "--execute-trace-reuse-paid");
    let execute_trace_reuse_offline =
        arguments.iter().any(|argument| argument == "--execute-trace-reuse-offline-simulator");
    let execute_offline = execute_legacy_offline
        || execute_economic_offline
        || execute_multi_task_offline
        || execute_trace_reuse_offline;
    let trace_reuse =
        trace_reuse_preflight_only || execute_trace_reuse_paid || execute_trace_reuse_offline;
    let economic =
        economic_preflight_only || execute_economic_paid || execute_economic_offline || trace_reuse;
    let multi_task =
        multi_task_preflight_only || execute_multi_task_paid || execute_multi_task_offline;
    if u8::from(preflight_only)
        + u8::from(execute_paid)
        + u8::from(economic_preflight_only)
        + u8::from(execute_economic_paid)
        + u8::from(execute_legacy_offline)
        + u8::from(execute_economic_offline)
        + u8::from(multi_task_preflight_only)
        + u8::from(execute_multi_task_paid)
        + u8::from(execute_multi_task_offline)
        + u8::from(trace_reuse_preflight_only)
        + u8::from(execute_trace_reuse_paid)
        + u8::from(execute_trace_reuse_offline)
        != 1
    {
        return Err(AppError::Usage(
            "minimal-pilot-live requires exactly one supported execution mode, including the trace reuse modes --trace-reuse-preflight-only, --execute-trace-reuse-paid or --execute-trace-reuse-offline-simulator".to_owned(),
        ));
    }

    let codex = resolve_codex(option_value(arguments, "--codex"))?;
    let codex_home = PathBuf::from(required_value(arguments, "--codex-home")?);
    if execute_offline {
        fs::create_dir_all(&codex_home)?;
    } else {
        ensure_dedicated_codex_home(&codex_home)?;
    }
    let codex_home = canonical_child_path(&codex_home)?;
    if execute_offline {
        let executable_name =
            codex.file_stem().and_then(|value| value.to_str()).unwrap_or_default();
        if executable_name != "needle-sim-codex" {
            return Err(AppError::Usage(
                "offline simulator modes accept only the native needle-sim-codex executable"
                    .to_owned(),
            ));
        }
        if codex_home.join("auth.json").exists() {
            return Err(AppError::Experiment(
                "offline simulator Codex home must not contain auth.json".to_owned(),
            ));
        }
    } else {
        ensure_product_pilot_hook_isolation(&codex_home)?;
        ensure_cache_pilot_hook_binary(&codex_home)?;
        ensure_codex_authenticated(&codex, &codex_home, "minimal-pilot-live")?;
    }
    let source_repository =
        canonical_child_path(Path::new(&required_value(arguments, "--source-repository")?))?;
    let artifact_root =
        absolute_run_path(Path::new(&required_value(arguments, "--artifact-root")?))?;
    if artifact_root.exists() {
        return Err(AppError::Experiment(format!(
            "minimal live pilot artifact root already exists: {}",
            artifact_root.display()
        )));
    }

    let main_model = required_value(arguments, "--main-model")?;
    let main_reasoning = required_value(arguments, "--main-reasoning")?;
    let worker_model = required_value(arguments, "--worker-model")?;
    let worker_reasoning = required_value(arguments, "--worker-reasoning")?;
    let service_tier = required_value(arguments, "--service-tier")?;
    let product_profile = required_value(arguments, "--product-profile")?;
    for (model, label) in [(&main_model, "main model"), (&worker_model, "worker model")] {
        validate_model_value(model, label)?;
    }
    validate_reasoning(&main_reasoning)?;
    validate_reasoning(&worker_reasoning)?;
    validate_service_tier(&service_tier)?;
    validate_slug(&product_profile, "product profile")?;
    if execute_offline && (main_model != "gpt-5.6-sol" || worker_model != "gpt-5.6-luna") {
        return Err(AppError::Usage(
            "offline simulator modes require --main-model gpt-5.6-sol and --worker-model gpt-5.6-luna"
                .to_owned(),
        ));
    }
    let timeout_seconds = option_value(arguments, "--timeout-seconds")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid --timeout-seconds: {error}")))?
        .unwrap_or(600);
    if !execute_offline && timeout_seconds < 180 {
        return Err(AppError::Usage(
            "minimal-pilot-live --timeout-seconds must be at least 180".to_owned(),
        ));
    }
    if product_profile != "marker" {
        return Err(AppError::Usage(
            "minimal-pilot-live requires the frozen product profile `marker`".to_owned(),
        ));
    }

    let manifest_path =
        option_value(arguments, "--corpus").map(PathBuf::from).unwrap_or_else(|| {
            workspace_path(if execute_offline {
                DEFAULT_LEGACY_OFFLINE_MANIFEST
            } else {
                DEFAULT_MANIFEST
            })
        });
    let pricing_path = option_value(arguments, "--pricing-snapshot")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_path(DEFAULT_PRICING));
    let mut protocol = if execute_offline {
        load_legacy_offline_protocol(&manifest_path)?
    } else {
        load_protocol(&manifest_path)?
    };
    if trace_reuse {
        protocol.select_campaign_task(TRACE_REUSE_TASK_ID)?;
    }
    let economic_stage_budget = protocol.economic_stage_budget()?;
    let multi_task_stage_budget = protocol.multi_task_stage_budget()?;
    let estimated_budget = if multi_task {
        multi_task_stage_budget.total_microcredits
    } else if economic {
        economic_stage_budget.total_microcredits
    } else {
        protocol.estimated_budget_microcredits
    };
    validate_source(&source_repository, protocol.task().repository_sha.as_str())?;
    let pricing = load_pricing(&pricing_path, &protocol.cost_model)?;
    let pricing_digest = pricing.digest()?;
    for model in [&main_model, &worker_model] {
        pricing
            .price_usage(model, &service_tier, 0, 0, 0)
            .map_err(|error| AppError::Experiment(error.to_string()))?;
    }
    let isolation =
        CodexWorker::verify_isolation(&codex.display().to_string()).map_err(AppError::Runtime)?;
    if !isolation.verified() {
        return Err(AppError::Experiment(format!(
            "worker isolation is not verified for Codex {}",
            isolation.codex_version
        )));
    }
    let worker_transport_preflight = if preflight_only
        || economic_preflight_only
        || multi_task_preflight_only
        || trace_reuse_preflight_only
    {
        fs::create_dir_all(&artifact_root)?;
        let report = run_declared_test_transport_preflight(
            &codex,
            &codex_home,
            &source_repository,
            &artifact_root.join("worker-transport-preflight"),
            &protocol,
            &worker_model,
            &worker_reasoning,
            &service_tier,
        )?;
        fs::write(
            artifact_root.join("worker-transport-preflight-report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        Some(report)
    } else {
        None
    };

    if multi_task_preflight_only {
        let tasks = protocol
            .campaign()
            .task_ids
            .iter()
            .map(|task_id| {
                let (task, _) = protocol.campaign_task(task_id)?;
                Ok(json!({
                    "task_id": task.id,
                    "route": task.route,
                    "prompt_digest": Digest::blake3(task.prompt.as_bytes()),
                    "oracle_digest": task.oracle_digest,
                    "planned_arms": ["frontier_direct", "needle_miss"],
                    "maximum_main_turns": 1 + MAXIMUM_MAIN_TURNS_PER_NEEDLE_ARM,
                    "maximum_logical_workers": 1,
                }))
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let task_count = tasks.len();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": "needle.multi-task-live-preflight/1",
                "passed": true,
                "provider_run_ready": false,
                "live_calls_started": 0,
                "artifact_root_available": true,
                "repository_sha": protocol.task().repository_sha,
                "codex_version": isolation.codex_version,
                "worker_transport_preflight": worker_transport_preflight,
                "main_model": main_model,
                "worker_model": worker_model,
                "service_tier": service_tier,
                "pricing_snapshot_digest": pricing_digest,
                "campaign_digest": protocol.manifest.campaign_digest,
                "tasks": tasks,
                "paid_arms": protocol.campaign().paid_arms,
                "repetitions_per_task": protocol.campaign().repetitions_per_task,
                "automatic_retries": 0,
                "maximum_provider_observations": task_count * 2,
                "maximum_logical_workers": task_count,
                "budget_components": multi_task_stage_budget,
                "estimated_budget_microcredits": estimated_budget,
                "estimate_is_hard_provider_ceiling": false,
                "deterministic_replay_required": true,
                "execute_flag_required": "--execute-multi-task-paid",
                "explicit_user_approval_required": true,
            }))?
        );
        return Ok(());
    }

    if preflight_only || economic_preflight_only || trace_reuse_preflight_only {
        let planned_arms = if economic {
            json!([
                {
                    "arm": "main_only",
                    "maximum_main_observations": 1,
                    "maximum_main_turns": 1,
                    "maximum_needs": 0,
                    "maximum_logical_workers": 0,
                    "read_only_inspection_policy": "repository-read-only-v1",
                },
                {
                    "arm": "needle_miss",
                    "maximum_main_observations": 1,
                    "maximum_main_turns": MAXIMUM_MAIN_TURNS_PER_NEEDLE_ARM,
                    "maximum_needs": MAXIMUM_MISS_NEEDS,
                    "maximum_logical_workers": MAXIMUM_MISS_LOGICAL_WORKERS,
                    "maximum_worker_turns": MAXIMUM_MISS_LOGICAL_WORKERS,
                },
                {
                    "arm": "needle_hit",
                    "maximum_main_observations": 1,
                    "maximum_main_turns": MAXIMUM_MAIN_TURNS_PER_NEEDLE_ARM,
                    "maximum_needs": MAXIMUM_HIT_NEEDS,
                    "maximum_logical_workers": 0,
                    "configured_worker_safety_bound": MAXIMUM_HIT_LOGICAL_WORKERS,
                    "prompt_reworded": true,
                    "expected_resolution": if trace_reuse {
                        "CompositeHit"
                    } else {
                        "ProofGatedFullHit"
                    },
                    "runs_only_after_valid_publication": true,
                }
            ])
        } else {
            json!([
                {
                    "arm": "needle_miss",
                    "maximum_main_observations": 1,
                    "maximum_main_turns": MAXIMUM_MAIN_TURNS_PER_NEEDLE_ARM,
                    "maximum_needs": MAXIMUM_MISS_NEEDS,
                    "maximum_logical_workers": MAXIMUM_MISS_LOGICAL_WORKERS,
                    "maximum_worker_turns": MAXIMUM_MISS_LOGICAL_WORKERS,
                },
                {
                    "arm": "coverage_hit",
                    "maximum_main_observations": 1,
                    "maximum_main_turns": MAXIMUM_MAIN_TURNS_PER_NEEDLE_ARM,
                    "maximum_needs": MAXIMUM_HIT_NEEDS,
                    "maximum_logical_workers": 0,
                    "configured_worker_safety_bound": MAXIMUM_HIT_LOGICAL_WORKERS,
                    "prompt_reworded": true,
                    "expected_resolution": HIT_RESOLUTION_TARGET,
                    "runs_only_after_valid_publication": true,
                }
            ])
        };
        let route = benchmark_route_key(protocol.task().route);
        let output = json!({
            "schema": if trace_reuse_preflight_only {
                "needle.trace-reuse-live-preflight/1"
            } else if economic_preflight_only {
                "needle.minimal-live-economic-preflight/1"
            } else {
                "needle.minimal-live-pilot-preflight/1"
            },
            "passed": true,
            "live_calls_started": 0,
            "artifact_root_available": true,
            "task_id": protocol.task().id,
            "route": route,
            "repository_sha": protocol.task().repository_sha,
            "codex_version": isolation.codex_version,
            "worker_transport_preflight": worker_transport_preflight,
            "main_model": main_model,
            "worker_model": worker_model,
            "service_tier": service_tier,
            "pricing_snapshot_digest": pricing_digest,
            "hook_binary_current": true,
            "dedicated_auth_available": true,
            "prompt_is_frozen_natural_task": true,
            "hidden_oracle_not_exported_to_model": true,
            "planned_arms": planned_arms,
            "economic_contract": economic.then(|| json!({
                "comparison": "MainOnly -> NeedleMiss -> NeedleHit",
                "same_semantic_need_required": true,
                "same_required_obligations_required": true,
                "same_quality_oracle_required": true,
                "actual_execution_accounting": true,
                "cache_behavior_suite": "separate",
                "expected_hit_resolution": if trace_reuse_preflight_only {
                    "CompositeHit"
                } else {
                    "ProofGatedFullHit"
                },
            })),
            "automatic_retries": 0,
            "bounded_bypass_completes_natively": true,
            "maximum_main_turns": if economic {
                MAXIMUM_ECONOMIC_STAGE_MAIN_TURNS
            } else {
                MAXIMUM_LEGACY_STAGE_MAIN_TURNS
            },
            "budget_reserves": {
                "extra_worker_reserves": MULTI_NEED_EXTRA_WORKER_RESERVES,
                "microcredits_per_worker_reserve": MULTI_NEED_WORKER_RESERVE_MICROCREDITS,
                "extra_main_turn_reserves": MULTI_NEED_EXTRA_MAIN_TURN_RESERVES,
                "microcredits_per_main_turn_reserve": MULTI_NEED_MAIN_TURN_RESERVE_MICROCREDITS,
            },
            "budget_components": economic.then_some(&economic_stage_budget),
            "estimated_budget_microcredits": estimated_budget,
            "estimate_is_hard_provider_ceiling": false,
            "execute_flag_required": if trace_reuse_preflight_only {
                "--execute-trace-reuse-paid"
            } else if economic_preflight_only {
                "--execute-economic-paid"
            } else {
                "--execute-paid"
            },
            "explicit_user_approval_required": true,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    let approved_budget = if execute_offline {
        0
    } else {
        required_value(arguments, "--approved-budget-microcredits")?
            .parse::<u64>()
            .map_err(|error| AppError::Usage(format!("invalid approved budget: {error}")))?
    };
    if !execute_offline && approved_budget != estimated_budget {
        return Err(AppError::Experiment(format!(
            "approved budget must equal the stage estimate {estimated_budget} microcredits"
        )));
    }

    let execution = Execution {
        codex: &codex,
        codex_home: &codex_home,
        source_repository: &source_repository,
        artifact_root: &artifact_root,
        protocol: &protocol,
        pricing: &pricing,
        pricing_digest,
        codex_version: &isolation.codex_version,
        main_model: &main_model,
        main_reasoning: &main_reasoning,
        worker_model: &worker_model,
        worker_reasoning: &worker_reasoning,
        service_tier: &service_tier,
        timeout: Duration::from_secs(timeout_seconds),
        estimated_budget,
        approved_budget,
        offline: execute_offline,
        economic,
        trace_reuse,
    };
    if multi_task { multi_task_campaign::execute(&execution) } else { execute(execution) }
}

struct Execution<'a> {
    codex: &'a Path,
    codex_home: &'a Path,
    source_repository: &'a Path,
    artifact_root: &'a Path,
    protocol: &'a Protocol,
    pricing: &'a PricingSnapshot,
    pricing_digest: Digest,
    codex_version: &'a str,
    main_model: &'a str,
    main_reasoning: &'a str,
    worker_model: &'a str,
    worker_reasoning: &'a str,
    service_tier: &'a str,
    timeout: Duration,
    estimated_budget: u64,
    approved_budget: u64,
    offline: bool,
    economic: bool,
    trace_reuse: bool,
}

fn execute(context: Execution<'_>) -> Result<(), AppError> {
    fs::create_dir_all(context.artifact_root)?;
    let artifact_root = canonical_child_path(context.artifact_root)?;
    if context.offline {
        fs::create_dir_all(context.codex_home)?;
        fs::write(
            context.codex_home.join(".needle-simulation-main-scenario"),
            if context.trace_reuse { TRACE_OFFLINE_MAIN_SCENARIO } else { OFFLINE_MAIN_SCENARIO },
        )?;
        if context.trace_reuse {
            fs::write(
                context.codex_home.join(".needle-simulation-worker-scenario"),
                TRACE_OFFLINE_WORKER_SCENARIO,
            )?;
        }
    }
    fs::write(
        artifact_root.join("pricing-snapshot.json"),
        serde_json::to_vec_pretty(context.pricing)?,
    )?;
    if !context.offline {
        let report = run_declared_test_transport_preflight(
            context.codex,
            context.codex_home,
            context.source_repository,
            &artifact_root.join("worker-transport-preflight"),
            context.protocol,
            context.worker_model,
            context.worker_reasoning,
            context.service_tier,
        )?;
        fs::write(
            artifact_root.join("worker-transport-preflight-report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
    }
    fs::write(
        artifact_root.join("frozen-corpus.json"),
        serde_json::to_vec_pretty(&context.protocol.manifest)?,
    )?;
    fs::write(
        artifact_root.join("minimal-live-pilot.json"),
        serde_json::to_vec_pretty(&context.protocol.pilot)?,
    )?;

    let repository = clone_local_checkout(context.source_repository, &artifact_root.join("repo"))?;
    let (_, initial_snapshot) = capture_git_snapshot(&repository)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let product_data = artifact_root.join("product-data");
    let main_only_output = artifact_root.join("main-only");
    let miss_output = artifact_root.join("miss");
    let hit_output = artifact_root.join("hit");
    fs::create_dir_all(&product_data)?;
    if context.economic {
        fs::create_dir_all(&main_only_output)?;
    }
    fs::create_dir_all(&miss_output)?;
    fs::create_dir_all(&hit_output)?;
    fs::write(product_data.join("pilot-root-task.txt"), &context.protocol.task().prompt)?;
    let declared_test_plan = test_plan(context.protocol.task());
    fs::write(
        product_data.join("pilot-test-plan.json"),
        serde_json::to_vec_pretty(&declared_test_plan)?,
    )?;

    let store = RuntimeStore::new(product_data.join("needle.sqlite3"));
    store
        .initialize_defaults(&RuntimeSettings {
            codex_executable: context.codex.display().to_string(),
            worker_model: context.worker_model.to_owned(),
            worker_reasoning: context.worker_reasoning.to_owned(),
            worker_timeout_seconds: context.timeout.as_secs().min(600),
            evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
            trusted_test_execution: true,
            multi_need_policy: MultiNeedPolicy {
                max_needs_per_task: MAXIMUM_MISS_NEEDS,
                max_workers_per_task: MAXIMUM_MISS_LOGICAL_WORKERS,
                ..MultiNeedPolicy::default()
            },
        })
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    store.mark_utility_gate_passed().map_err(|error| AppError::Experiment(error.to_string()))?;
    store
        .set_model_policy(&ModelPolicy::FixedOrder {
            profiles: vec![WorkerProfile::new(
                "codex",
                context.worker_model,
                context.worker_reasoning,
                Some(context.service_tier.to_owned()),
            )],
            repair_once: false,
            native_fallback: false,
        })
        .map_err(|error| AppError::Experiment(error.to_string()))?;

    let profile =
        HookConfig::default().profile().map_err(|error| AppError::Experiment(error.to_string()))?;
    let role_profile_id = provision_experiment_role_profile(
        &store,
        "minimal-live-pilot.explorer",
        profile.definition_digest,
        context.worker_model,
        context.worker_reasoning,
        context.service_tier,
        context.timeout.as_secs().min(600),
        false,
    )?;
    let main_instructions = protocol::pilot_main_instructions(&profile.rendered_context_owned());
    let miss_quality_spec = quality_spec(context.protocol)?;
    let hit_quality_spec = coverage_hit_quality_spec(context.protocol)?;
    let route = benchmark_route_key(context.protocol.task().route);
    let publication_prompt = if context.economic {
        if context.trace_reuse {
            TRACE_REUSE_PUBLICATION_PROMPT
        } else {
            REWORDED_COVERAGE_HIT_PROMPT
        }
    } else {
        &context.protocol.task().prompt
    };
    let publication_quality_spec =
        if context.economic { &hit_quality_spec } else { &miss_quality_spec };
    let main_only = context
        .economic
        .then(|| {
            observe_direct_main(DirectObservation {
                context: &context,
                repository: &repository,
                output: &main_only_output,
                source_snapshot_digest: initial_snapshot.source_digest,
                repository_id: initial_snapshot.repository_id,
                prompt: publication_prompt,
                quality_spec: publication_quality_spec,
            })
        })
        .transpose()?;
    if main_only.as_ref().is_some_and(|arm| !valid_main_only_arm(arm)) {
        let workers =
            store.worker_run_count().map_err(|error| AppError::Experiment(error.to_string()))?;
        let evidence = store
            .command_evidence_count()
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        let miss = MinimalLiveArm::skipped(
            FinalArm::NeedleMiss,
            "main_only_failed_fail_fast",
            workers,
            evidence,
        );
        let hit = MinimalLiveArm::skipped(
            FinalArm::ExactHit,
            "main_only_failed_fail_fast",
            workers,
            evidence,
        );
        let (_, final_snapshot) = capture_git_snapshot(&repository)
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        let report = evaluate_report(
            ReportInput {
                context: &context,
                source_snapshot_digest: initial_snapshot.source_digest,
                task_prompt_digest: Digest::blake3(publication_prompt.as_bytes()),
                quality_oracle_digest: Digest::blake3(serde_json::to_vec(
                    publication_quality_spec,
                )?),
                capability_mode: None,
                validation_certificates: 0,
                sufficiency_certificates: 0,
                checkout_clean: initial_snapshot.source_digest == final_snapshot.source_digest
                    && repository_status_clean(&repository)?,
                reused_validated_artifact: false,
            },
            main_only,
            miss,
            hit,
        );
        let output = artifact_root.join("minimal-live-pilot-report.json");
        fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
        return Err(AppError::Experiment(
            "main-only arm failed; Needle miss and hit arms were skipped before further provider calls"
                .to_owned(),
        ));
    }
    let miss = observe_arm(Observe {
        arm: FinalArm::NeedleMiss,
        route,
        codex: context.codex,
        main_reasoning: context.main_reasoning,
        repository: &repository,
        codex_home: context.codex_home,
        product_data: &product_data,
        output: &miss_output,
        source_snapshot_digest: initial_snapshot.source_digest,
        repository_id: initial_snapshot.repository_id,
        prompt_profile_digest: profile.definition_digest,
        role_profile_id: &role_profile_id,
        main_instructions: &main_instructions,
        prompt: publication_prompt,
        declared_test_plan: &declared_test_plan,
        store: &store,
        pricing: context.pricing,
        main_model: context.main_model,
        worker_model: context.worker_model,
        service_tier: context.service_tier,
        timeout: context.timeout,
        quality_spec: publication_quality_spec,
        inherited_test_evidence: false,
        offline: context.offline,
        cache_only: false,
    })?;
    let semantic_artifacts =
        store.artifacts().map_err(|error| AppError::Experiment(error.to_string()))?;
    let validation_certificates = semantic_artifacts
        .iter()
        .filter(|artifact| {
            serde_json::from_value::<SemanticWorkerArtifact>(artifact.payload.clone()).is_ok()
                && store
                    .validation_certificate_for_artifact(&artifact.id.to_string())
                    .ok()
                    .flatten()
                    .is_some()
        })
        .count()
        .try_into()
        .unwrap_or(u64::MAX);
    let miss_ready = valid_miss(&miss) && validation_certificates > 0;

    let capability_mode = if miss_ready {
        let fresh_cost = if context.offline {
            context
                .protocol
                .cost_model
                .arm_estimates
                .iter()
                .find(|estimate| estimate.arm == FinalArm::NeedleMiss)
                .map(|estimate| estimate.microcredits_per_observation)
                .ok_or_else(|| {
                    AppError::Experiment(
                        "offline replay has no frozen Needle miss cost estimate".to_owned(),
                    )
                })?
        } else {
            miss.total_cost_microcredits().ok_or_else(|| {
                AppError::Experiment("minimal live miss usage cannot be priced".to_owned())
            })?
        };
        store
            .record_route_cost_observation(&RouteCostObservation {
                route_key: route.to_owned(),
                cost_microusd: fresh_cost,
                source: "fresh".to_owned(),
                evidence_digest: Digest::blake3(serde_json::to_vec(&miss)?),
                observed_unix_ms: now_ms(),
            })
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        let reuse_bootstrap = if context.trace_reuse {
            context
                .protocol
                .cost_model
                .arm_estimates
                .iter()
                .find(|estimate| estimate.arm == FinalArm::ExactHit)
                .map(|estimate| estimate.microcredits_per_observation)
                .ok_or_else(|| {
                    AppError::Experiment(
                        "trace reuse has no frozen full-hit cost estimate".to_owned(),
                    )
                })?
        } else {
            context.protocol.pilot.promotion_bootstrap.reuse_cost_microcredits
        };
        let promotion_evidence_digest = context.protocol.promotion_evidence_digest();
        store
            .record_route_cost_observation(&RouteCostObservation {
                route_key: route.to_owned(),
                cost_microusd: reuse_bootstrap,
                source: "reuse_bootstrap".to_owned(),
                evidence_digest: promotion_evidence_digest,
                observed_unix_ms: now_ms().saturating_add(1),
            })
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        let required_predicates: &[PredicateKind] = match context.protocol.task().route {
            BenchmarkRoute::LocateImplementation => &[PredicateKind::ImplementationLocation],
            BenchmarkRoute::TraceStateFlow => &[
                PredicateKind::ImplementationLocation,
                PredicateKind::RuntimeFlow,
                PredicateKind::FocusedTests,
            ],
        };
        let classes =
            store.capability_classes().map_err(|error| AppError::Experiment(error.to_string()))?;
        let mut promoted = true;
        for predicate in required_predicates {
            let class = classes
                .iter()
                .find(|class| {
                    class.reuse_unit == ReuseUnit::Artifact && class.predicate == *predicate
                })
                .ok_or_else(|| {
                    AppError::Experiment(format!("{predicate:?} capability is missing"))
                })?;
            let mode = store
                .set_capability_mode(
                    &class.id,
                    class.definition_digest,
                    CapabilityMode::Authoritative,
                    Some(promotion_evidence_digest),
                )
                .map_err(|error| AppError::Experiment(error.to_string()))?
                .map(|class| class.mode);
            promoted &= mode == Some(CapabilityMode::Authoritative);
        }
        promoted.then_some(CapabilityMode::Authoritative)
    } else {
        None
    };

    let hit = if miss_ready && capability_mode == Some(CapabilityMode::Authoritative) {
        let mut hit_settings =
            store.settings().map_err(|error| AppError::Experiment(error.to_string()))?;
        hit_settings.multi_need_policy.max_needs_per_task = MAXIMUM_HIT_NEEDS;
        hit_settings.multi_need_policy.max_workers_per_task = MAXIMUM_HIT_LOGICAL_WORKERS;
        store
            .set_runtime_settings(&hit_settings)
            .map_err(|error| AppError::Experiment(error.to_string()))?;
        observe_arm(Observe {
            arm: FinalArm::ExactHit,
            route,
            codex: context.codex,
            main_reasoning: context.main_reasoning,
            repository: &repository,
            codex_home: context.codex_home,
            product_data: &product_data,
            output: &hit_output,
            source_snapshot_digest: initial_snapshot.source_digest,
            repository_id: initial_snapshot.repository_id,
            prompt_profile_digest: profile.definition_digest,
            role_profile_id: &role_profile_id,
            main_instructions: &main_instructions,
            prompt: if context.economic {
                if context.trace_reuse {
                    TRACE_REUSE_EQUIVALENT_HIT_PROMPT
                } else {
                    ECONOMIC_EQUIVALENT_HIT_PROMPT
                }
            } else {
                REWORDED_COVERAGE_HIT_PROMPT
            },
            declared_test_plan: &declared_test_plan,
            store: &store,
            pricing: context.pricing,
            main_model: context.main_model,
            worker_model: context.worker_model,
            service_tier: context.service_tier,
            timeout: context.timeout,
            quality_spec: if context.economic {
                publication_quality_spec
            } else {
                &hit_quality_spec
            },
            inherited_test_evidence: false,
            offline: context.offline,
            cache_only: true,
        })?
    } else {
        MinimalLiveArm::skipped(
            FinalArm::ExactHit,
            "publication_miss_failed",
            store.worker_run_count().map_err(|error| AppError::Experiment(error.to_string()))?,
            store
                .command_evidence_count()
                .map_err(|error| AppError::Experiment(error.to_string()))?,
        )
    };

    if (valid_proof_hit(&hit) || (context.economic && valid_economic_hit(&hit)))
        && let Some(reuse_cost) = hit.total_cost_microcredits()
    {
        store
            .record_route_cost_observation(&RouteCostObservation {
                route_key: route.to_owned(),
                cost_microusd: reuse_cost,
                source: "reuse".to_owned(),
                evidence_digest: Digest::blake3(serde_json::to_vec(&hit)?),
                observed_unix_ms: now_ms().saturating_add(2),
            })
            .map_err(|error| AppError::Experiment(error.to_string()))?;
    }

    let (_, final_snapshot) = capture_git_snapshot(&repository)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let checkout_clean = initial_snapshot.source_digest == final_snapshot.source_digest
        && repository_status_clean(&repository)?;
    let sufficiency_certificates = store
        .proof_certificates(100)
        .map_err(|error| AppError::Experiment(error.to_string()))?
        .len()
        .try_into()
        .unwrap_or(u64::MAX);
    let reused_artifact_ids = hit
        .resolve
        .as_ref()
        .into_iter()
        .flat_map(|outcome| resolution_artifact_ids(&outcome.cache_resolution))
        .chain(
            hit.selected_plan
                .iter()
                .flat_map(|plan| plan.artifact_ids.iter().map(ToString::to_string)),
        )
        .collect::<BTreeSet<_>>();
    let expected_reused_artifacts = if context.trace_reuse { 2 } else { 1 };
    let reused_validated_artifact = reused_artifact_ids.len() >= expected_reused_artifacts
        && reused_artifact_ids.iter().all(|artifact_id| {
            store.validation_certificate_for_artifact(artifact_id).ok().flatten().is_some()
        });
    let report = evaluate_report(
        ReportInput {
            context: &context,
            source_snapshot_digest: initial_snapshot.source_digest,
            task_prompt_digest: Digest::blake3(publication_prompt.as_bytes()),
            quality_oracle_digest: Digest::blake3(serde_json::to_vec(publication_quality_spec)?),
            capability_mode,
            validation_certificates,
            sufficiency_certificates,
            checkout_clean,
            reused_validated_artifact,
        },
        main_only,
        miss,
        hit,
    );
    let output = artifact_root.join("minimal-live-pilot-report.json");
    fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
    println!("minimal live pilot report written to {}", output.display());
    if report.passed {
        Ok(())
    } else {
        Err(AppError::Experiment(format!(
            "minimal live pilot gate failed: {}",
            report.failures.join(", ")
        )))
    }
}

struct Observe<'a> {
    arm: FinalArm,
    route: &'a str,
    codex: &'a Path,
    main_reasoning: &'a str,
    repository: &'a Path,
    codex_home: &'a Path,
    product_data: &'a Path,
    output: &'a Path,
    source_snapshot_digest: Digest,
    repository_id: Digest,
    prompt_profile_digest: Digest,
    role_profile_id: &'a RoleProfileId,
    main_instructions: &'a str,
    prompt: &'a str,
    declared_test_plan: &'a needle_core::TestPlan,
    store: &'a RuntimeStore,
    pricing: &'a PricingSnapshot,
    main_model: &'a str,
    worker_model: &'a str,
    service_tier: &'a str,
    timeout: Duration,
    quality_spec: &'a QualityOracleSpec,
    inherited_test_evidence: bool,
    offline: bool,
    cache_only: bool,
}

fn observe_arm(context: Observe<'_>) -> Result<MinimalLiveArm, AppError> {
    let selected_plan_ids_before = context
        .store
        .selected_plans(500)
        .map_err(|error| AppError::Experiment(error.to_string()))?
        .into_iter()
        .map(|plan| plan.id.to_string())
        .collect::<std::collections::BTreeSet<_>>();
    let worker_runs_before = context
        .store
        .worker_run_count()
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let command_evidence_before = context
        .store
        .command_evidence_count()
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    fs::create_dir_all(context.output)?;
    let started = Instant::now();
    let mut main = SupervisedMain::default();
    let execution_error = run_supervised_main(&context, &mut main).err();
    let process_success = execution_error.is_none();
    let process = process_status(execution_error.as_deref());
    if let Some(error) = execution_error {
        fs::write(context.output.join("main-error.txt"), &error)?;
    }
    if !main.final_response.is_empty() {
        fs::write(context.output.join("main-final-response.txt"), &main.final_response)?;
    }
    let worker_runs_after = context
        .store
        .worker_run_count()
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let command_evidence_after = context
        .store
        .command_evidence_count()
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let workers = context
        .store
        .worker_runs_after(worker_runs_before)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let selected_plan = context
        .store
        .selected_plans(500)
        .map_err(|error| AppError::Experiment(error.to_string()))?
        .into_iter()
        .find(|plan| !selected_plan_ids_before.contains(&plan.id.to_string()));
    let worker_input_tokens = sum_worker_usage(&workers, |run| run.input_tokens);
    let worker_cached_input_tokens = sum_worker_usage(&workers, |run| run.cached_input_tokens);
    let worker_output_tokens = sum_worker_usage(&workers, |run| run.output_tokens);
    let logical_worker_spawns =
        workers.iter().map(|run| run.logical_worker_spawns).fold(0, u32::saturating_add);
    let worker_turns = workers.iter().map(|run| run.worker_turns).fold(0, u32::saturating_add);
    let observed_arm = if context.arm == FinalArm::ExactHit
        && (!workers.is_empty() || logical_worker_spawns > 0)
    {
        FinalArm::NeedleMiss
    } else {
        context.arm
    };
    let command_evidence_delta = command_evidence_after.saturating_sub(command_evidence_before);
    let test_evidence_observed = context.inherited_test_evidence
        || (1..=u64::from(MAXIMUM_MISS_LOGICAL_WORKERS)).contains(&command_evidence_delta);
    let quality = QualityOracleResult::evaluate(
        context.quality_spec,
        &main.final_response,
        test_evidence_observed.then_some(true),
    );
    let need_requests = main
        .need_steps
        .iter()
        .map(|step| {
            context
                .store
                .need_step_request(step.id)
                .map_err(|error| AppError::Experiment(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    Ok(MinimalLiveArm {
        arm: context.arm,
        observed_arm,
        provider_observation_started: main.provider_observation_started && !context.offline,
        transport_success: main.transport_started,
        process_success,
        process,
        quality,
        resolve: main.resolve,
        resolve_sequence: main.resolves,
        need_steps: main.need_steps,
        need_requests,
        worker_runs_before,
        worker_runs_after,
        worker_run_delta: worker_runs_after.saturating_sub(worker_runs_before),
        logical_worker_spawns,
        worker_turns,
        repair_performed: workers.iter().any(|run| run.repair_performed),
        command_evidence_before,
        command_evidence_after,
        command_evidence_delta,
        main_discovery_total: main.tool_items_started,
        wall_time_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        main_input_tokens: main.usage.input_tokens,
        main_cached_input_tokens: main.usage.cached_input_tokens,
        main_output_tokens: main.usage.output_tokens,
        worker_input_tokens,
        worker_cached_input_tokens,
        worker_output_tokens,
        main_cost: price_usage_observation_optional(
            context.pricing,
            context.main_model,
            context.service_tier,
            main.usage.input_tokens,
            main.usage.cached_input_tokens,
            main.usage.output_tokens,
        )?,
        worker_cost: if workers.is_empty() {
            None
        } else {
            price_usage_observation_optional(
                context.pricing,
                context.worker_model,
                context.service_tier,
                worker_input_tokens,
                worker_cached_input_tokens,
                worker_output_tokens,
            )?
        },
        selected_plan,
    })
}

fn sum_worker_usage(
    workers: &[needle_runtime::WorkerRunRecord],
    select: impl Fn(&needle_runtime::WorkerRunRecord) -> Option<u64>,
) -> Option<u64> {
    if workers.is_empty() {
        return None;
    }
    workers.iter().try_fold(0_u64, |total, worker| total.checked_add(select(worker)?))
}

fn run_supervised_main(
    context: &Observe<'_>,
    observation: &mut SupervisedMain,
) -> Result<(), String> {
    supervised_main::run_supervised_main(context, observation)
}

fn process_status(error: Option<&str>) -> ProcessExecutionStatus {
    supervised_main::process_status(error)
}

struct ReportInput<'a> {
    context: &'a Execution<'a>,
    source_snapshot_digest: Digest,
    task_prompt_digest: Digest,
    quality_oracle_digest: Digest,
    capability_mode: Option<CapabilityMode>,
    validation_certificates: u64,
    sufficiency_certificates: u64,
    checkout_clean: bool,
    reused_validated_artifact: bool,
}

fn evaluate_report(
    input: ReportInput<'_>,
    main_only: Option<MinimalLiveArm>,
    miss: MinimalLiveArm,
    hit: MinimalLiveArm,
) -> MinimalLivePilotReport {
    let quality_passed = miss.quality.passed
        && hit.quality.passed
        && main_only.as_ref().is_none_or(|arm| arm.quality.passed);
    let proof_gated_hit = (valid_proof_hit(&hit)
        || (input.context.economic && valid_economic_hit(&hit)))
        && input.sufficiency_certificates > 0;
    let cache_candidate_valid =
        proof_gated_hit || (valid_cache_candidate(&hit) && input.sufficiency_certificates > 0);
    let production_policy_reason =
        hit.selected_plan.as_ref().map(|plan| plan.decision_reason.clone());
    let hit_execution = if proof_gated_hit {
        hit.resolve.as_ref().map_or("proof_gated_hit", |outcome| match &outcome.cache_resolution {
            CacheResolution::ExactHit { .. } => "exact_hit",
            CacheResolution::CoverageHit { .. } => "coverage_hit",
            CacheResolution::CompositeHit { .. } => "composite_hit",
            _ => "proof_gated_hit",
        })
    } else if hit.worker_execution_observed() {
        "fresh_worker"
    } else if cache_candidate_valid {
        "advisory_candidate"
    } else {
        "bypass_or_failure"
    }
    .to_owned();
    let coverage_zero_worker = hit.worker_run_delta == 0
        && hit.resolve.as_ref().is_some_and(|outcome| !outcome.worker_spawned);
    let exactly_one_worker =
        miss.worker_run_delta == 1 && miss.logical_worker_spawns == 1 && hit.worker_run_delta == 0;
    let worker_count_bounded =
        valid_worker_count(miss.worker_run_delta, miss.logical_worker_spawns)
            && hit.worker_run_delta == 0;
    let zero_repair = miss.worker_turns == miss.logical_worker_spawns && !miss.repair_performed;
    let zero_main_discovery = miss.main_discovery_total == 0 && hit.main_discovery_total == 0;
    let focused_test_execution_observed = miss.command_evidence_delta > 0;
    let focused_test_evidence_valid = focused_test_execution_observed
        && (1..=u64::from(MAXIMUM_MISS_LOGICAL_WORKERS)).contains(&miss.command_evidence_delta);
    let same_artifact = input.reused_validated_artifact;
    let observed_miss_cost_microcredits = miss.total_cost_microcredits();
    let observed_hit_cost_microcredits = hit.total_cost_microcredits();
    let observed_main_only_cost_microcredits =
        main_only.as_ref().and_then(MinimalLiveArm::total_cost_microcredits);
    let costs_available = observed_miss_cost_microcredits.is_some()
        && observed_hit_cost_microcredits.is_some()
        && (!input.context.economic || observed_main_only_cost_microcredits.is_some());
    let hit_cost_lower = observed_miss_cost_microcredits
        .zip(observed_hit_cost_microcredits)
        .is_some_and(|(miss, hit)| hit < miss);
    let economic_semantics_equivalent = input.context.economic.then(|| {
        let miss_step = miss.need_steps.first();
        let hit_step = hit.need_steps.first();
        miss_step.zip(hit_step).is_some_and(|(miss_step, hit_step)| {
            miss_step.need_id == hit_step.need_id && miss_step.required == hit_step.required
        })
    });
    let economic_contract_digest = input.context.economic.then(|| {
        let mut hasher = needle_core::CanonicalHasher::new(b"economic-benchmark-contract");
        hasher.field_str(&input.context.protocol.task().id);
        hasher.field_str(benchmark_route_key(input.context.protocol.task().route));
        hasher.field_str(&input.context.protocol.task().repository_sha);
        hasher.field_digest(input.task_prompt_digest);
        hasher.field_digest(input.quality_oracle_digest);
        if let Some(step) = miss.need_steps.first() {
            hasher.field_digest(step.need_id.digest());
            for obligation in &step.required {
                hasher.field_digest(obligation.digest());
            }
        }
        hasher.finish()
    });
    let routing_saving_basis_points =
        savings_basis_points(observed_main_only_cost_microcredits, observed_miss_cost_microcredits);
    let cache_saving_basis_points =
        savings_basis_points(observed_miss_cost_microcredits, observed_hit_cost_microcredits);
    let total_saving_basis_points =
        savings_basis_points(observed_main_only_cost_microcredits, observed_hit_cost_microcredits);
    let routing_saving = routing_saving_basis_points.is_some_and(|value| value > 0);
    let cache_saving = cache_saving_basis_points.is_some_and(|value| value > 0);
    let total_saving = total_saving_basis_points.is_some_and(|value| value > 0);
    let main_only_valid = main_only.as_ref().is_some_and(valid_main_only_arm);
    let mut failures = Vec::new();
    let checks: Vec<(bool, &str)> = if input.context.economic {
        vec![
            (main_only_valid, "main_only"),
            (valid_miss(&miss), "needle_miss"),
            (quality_passed, "quality"),
            (economic_semantics_equivalent == Some(true), "semantic_equivalence"),
            (
                proof_gated_hit,
                if cache_candidate_valid {
                    "production_policy_rejected_reuse"
                } else {
                    "proof_gated_hit"
                },
            ),
            (
                coverage_zero_worker,
                if hit.worker_execution_observed() {
                    "expected_hit_executed_worker"
                } else {
                    "coverage_zero_worker"
                },
            ),
            (worker_count_bounded, "worker_count"),
            (zero_repair, "worker_repair"),
            (zero_main_discovery, "needle_main_discovery"),
            (same_artifact, "artifact_identity"),
            (costs_available, "costs"),
            (routing_saving, "routing_economics"),
            (cache_saving, "cache_economics"),
            (total_saving, "total_economics"),
            (input.checkout_clean, "checkout_integrity"),
            (input.capability_mode == Some(CapabilityMode::Authoritative), "capability_promotion"),
            (input.validation_certificates > 0, "validation_certificate"),
        ]
    } else {
        vec![
            (valid_miss(&miss), "publication_miss"),
            (quality_passed, "quality"),
            (
                proof_gated_hit,
                if cache_candidate_valid {
                    "production_policy_rejected_reuse"
                } else {
                    "proof_gated_hit"
                },
            ),
            (
                coverage_zero_worker,
                if hit.worker_execution_observed() {
                    "expected_hit_executed_worker"
                } else {
                    "coverage_zero_worker"
                },
            ),
            (worker_count_bounded, "worker_count"),
            (zero_repair, "worker_repair"),
            (zero_main_discovery, "main_discovery"),
            (same_artifact, "artifact_identity"),
            (costs_available, "costs"),
            (hit_cost_lower, "economics"),
            (input.checkout_clean, "checkout_integrity"),
            (input.capability_mode == Some(CapabilityMode::Authoritative), "capability_promotion"),
            (input.validation_certificates > 0, "validation_certificate"),
        ]
    };
    for (passed, failure) in checks {
        if !passed {
            failures.push(failure.to_owned());
        }
    }
    if input.context.offline {
        let expected_validation_certificates =
            if input.context.protocol.task().route == BenchmarkRoute::TraceStateFlow {
                3
            } else if input.context.economic {
                1
            } else {
                2
            };
        for (passed, failure) in [
            (exactly_one_worker, "offline_exactly_one_worker"),
            (
                input.validation_certificates == expected_validation_certificates,
                "offline_expected_validation_certificates",
            ),
            (
                miss.command_evidence_delta <= u64::from(MAXIMUM_MISS_LOGICAL_WORKERS),
                "offline_command_evidence_bounded",
            ),
        ] {
            if !passed {
                failures.push(failure.to_owned());
            }
        }
    }
    MinimalLivePilotReport {
        schema: REPORT_SCHEMA.to_owned(),
        mode: if input.context.economic && input.context.offline {
            "economic-deterministic-offline-app-server".to_owned()
        } else if input.context.economic {
            "economic-provider-live".to_owned()
        } else if input.context.offline {
            "deterministic-offline-app-server".to_owned()
        } else {
            "provider-live".to_owned()
        },
        task_id: input.context.protocol.task().id.clone(),
        route: benchmark_route_key(input.context.protocol.task().route).to_owned(),
        source_sha: input.context.protocol.task().repository_sha.clone(),
        source_snapshot_digest: input.source_snapshot_digest,
        codex_version: input.context.codex_version.to_owned(),
        main_model: input.context.main_model.to_owned(),
        worker_model: input.context.worker_model.to_owned(),
        service_tier: input.context.service_tier.to_owned(),
        pricing_snapshot_digest: input.context.pricing_digest,
        estimated_budget_microcredits: input.context.estimated_budget,
        approved_budget_microcredits: input.context.approved_budget,
        automatic_retries: false,
        maximum_main_observations: if input.context.economic { 3 } else { 2 },
        maximum_main_turns: if input.context.economic {
            MAXIMUM_ECONOMIC_STAGE_MAIN_TURNS
        } else {
            MAXIMUM_LEGACY_STAGE_MAIN_TURNS
        },
        maximum_logical_workers: u32::from(MAXIMUM_MISS_LOGICAL_WORKERS),
        maximum_worker_turns: u32::from(MAXIMUM_MISS_LOGICAL_WORKERS),
        provider_observations_started: u32::from(miss.provider_observation_started)
            + u32::from(hit.provider_observation_started)
            + main_only.as_ref().map_or(0, |arm| u32::from(arm.provider_observation_started)),
        capability_mode: input.capability_mode,
        bootstrap_evidence_digest: input.context.protocol.promotion_evidence_digest(),
        validation_certificates: input.validation_certificates,
        sufficiency_certificates: input.sufficiency_certificates,
        checkout_clean: input.checkout_clean,
        hit_prompt_reworded: true,
        hit_resolution_target: if input.context.economic {
            "ProofGatedFullHit".to_owned()
        } else {
            HIT_RESOLUTION_TARGET.to_owned()
        },
        main_only,
        miss,
        hit,
        economic_contract_digest,
        economic_semantics_equivalent,
        routing_saving_basis_points,
        cache_saving_basis_points,
        total_saving_basis_points,
        cache_behavior_suite: "separate:not_run".to_owned(),
        quality_passed,
        cache_candidate_valid,
        proof_gated_hit,
        production_policy_reason,
        hit_execution,
        coverage_zero_worker,
        exactly_one_worker,
        worker_count_bounded,
        zero_repair,
        zero_main_discovery,
        focused_test_execution_required: false,
        focused_test_execution_observed,
        focused_test_evidence_valid,
        same_artifact,
        costs_available,
        hit_cost_lower,
        observed_miss_cost_microcredits,
        observed_hit_cost_microcredits,
        passed: failures.is_empty(),
        failures,
    }
}

fn valid_main_only_arm(arm: &MinimalLiveArm) -> bool {
    arm.transport_success
        && arm.process_success
        && !arm.worker_execution_observed()
        && arm.resolve.is_none()
}

fn valid_miss(arm: &MinimalLiveArm) -> bool {
    arm.transport_success
        && arm.process_success
        && valid_worker_count(arm.worker_run_delta, arm.logical_worker_spawns)
        && arm.worker_turns == arm.logical_worker_spawns
        && !arm.repair_performed
        && arm.resolve.as_ref().is_some_and(|outcome| {
            outcome.status == "generated"
                && matches!(outcome.cache_resolution, CacheResolution::Miss)
                && !outcome.cache_hit
                && outcome.worker_spawned
        })
}

#[allow(clippy::too_many_arguments)]
fn run_declared_test_transport_preflight(
    codex: &Path,
    codex_home: &Path,
    source_repository: &Path,
    data_root: &Path,
    protocol: &Protocol,
    worker_model: &str,
    worker_reasoning: &str,
    service_tier: &str,
) -> Result<TransportPreflightReport, AppError> {
    let config = WorkerConfig {
        executable: codex.display().to_string(),
        model: worker_model.to_owned(),
        reasoning: worker_reasoning.to_owned(),
        service_tier: Some(service_tier.to_owned()),
        timeout_seconds: 30,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        role_profile_provenance: None,
    };
    CodexWorker::with_codex_home(data_root, codex_home)
        .preflight_transport_for_test_plan(
            &config,
            source_repository,
            benchmark_route_key(protocol.task().route),
            Some(test_plan(protocol.task())),
            true,
        )
        .map_err(AppError::Runtime)
}

const fn benchmark_route_key(route: BenchmarkRoute) -> &'static str {
    match route {
        BenchmarkRoute::LocateImplementation => "locate.implementation",
        BenchmarkRoute::TraceStateFlow => "trace.state-flow",
    }
}

fn valid_worker_count(worker_run_delta: u64, logical_worker_spawns: u32) -> bool {
    worker_run_delta == u64::from(logical_worker_spawns)
        && (1..=u64::from(MAXIMUM_MISS_LOGICAL_WORKERS)).contains(&worker_run_delta)
        && (1..=u32::from(MAXIMUM_MISS_LOGICAL_WORKERS)).contains(&logical_worker_spawns)
}

fn valid_proof_hit(arm: &MinimalLiveArm) -> bool {
    arm.transport_success
        && arm.process_success
        && arm.worker_run_delta == 0
        && arm.resolve.as_ref().is_some_and(|outcome| {
            outcome.status == "hit"
                && outcome.cache_hit
                && !outcome.worker_spawned
                && matches!(
                    &outcome.cache_resolution,
                    CacheResolution::CoverageHit {
                        sufficiency_certificate_id: _,
                        selected_plan_id: _,
                        resolution_format_revision: PROOF_RESOLUTION_FORMAT_REVISION,
                        ..
                    }
                )
        })
}

fn valid_economic_hit(arm: &MinimalLiveArm) -> bool {
    arm.transport_success
        && arm.process_success
        && !arm.worker_execution_observed()
        && arm.resolve.as_ref().is_some_and(|outcome| {
            outcome.status == "hit"
                && outcome.cache_hit
                && !outcome.worker_spawned
                && match &outcome.cache_resolution {
                    CacheResolution::CoverageHit { resolution_format_revision, .. } => {
                        *resolution_format_revision == PROOF_RESOLUTION_FORMAT_REVISION
                    }
                    CacheResolution::ExactHit {
                        sufficiency_certificate_id,
                        selected_plan_id,
                        resolution_format_revision,
                        ..
                    }
                    | CacheResolution::CompositeHit {
                        sufficiency_certificate_id,
                        selected_plan_id,
                        resolution_format_revision,
                        ..
                    } => {
                        sufficiency_certificate_id.is_some()
                            && selected_plan_id.is_some()
                            && *resolution_format_revision == Some(PROOF_RESOLUTION_FORMAT_REVISION)
                    }
                    _ => false,
                }
        })
}

fn resolution_artifact_ids(resolution: &CacheResolution) -> Vec<String> {
    match resolution {
        CacheResolution::ExactHit { artifact_id, .. }
        | CacheResolution::CoverageHit { artifact_id, .. } => vec![artifact_id.to_string()],
        CacheResolution::CompositeHit { artifact_ids, .. } => {
            artifact_ids.iter().map(ToString::to_string).collect()
        }
        CacheResolution::PartialHit { reused, .. } => {
            reused.iter().map(ToString::to_string).collect()
        }
        _ => Vec::new(),
    }
}

fn valid_cache_candidate(arm: &MinimalLiveArm) -> bool {
    arm.selected_plan.as_ref().is_some_and(|plan| {
        plan.missing_mask == 0
            && !plan.artifact_ids.is_empty()
            && matches!(
                plan.decision_reason.as_str(),
                "Advisory::CoverageHit"
                    | "Authoritative::CoverageHit"
                    | "Advisory::ExactHit"
                    | "Authoritative::ExactHit"
                    | "Advisory::CompositeHit"
                    | "Authoritative::CompositeHit"
            )
    })
}

fn savings_basis_points(baseline: Option<u64>, candidate: Option<u64>) -> Option<i64> {
    let baseline = baseline?;
    let candidate = candidate?;
    if baseline == 0 {
        return None;
    }
    let delta = i128::from(baseline) - i128::from(candidate);
    delta.checked_mul(10_000)?.checked_div(i128::from(baseline))?.try_into().ok()
}

fn unavailable_quality() -> QualityOracleResult {
    QualityOracleResult {
        passed: false,
        required_files_present: false,
        required_symbols_present: false,
        required_claims_present: false,
        forbidden_claims_absent: false,
        focused_test_suggested: false,
        evaluator_test_passed: None,
        failures: vec!["not_executed".to_owned()],
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::protocol::DEFAULT_LEGACY_OFFLINE_MANIFEST;
    use super::*;
    use needle_bench::ProcessExecutionStatus;

    #[test]
    fn live_hit_requires_coverage_resolution_with_v04_proof() {
        let arm = MinimalLiveArm {
            arm: FinalArm::ExactHit,
            observed_arm: FinalArm::ExactHit,
            provider_observation_started: true,
            transport_success: true,
            process_success: true,
            process: ProcessExecutionStatus {
                status: "exit:0".to_owned(),
                exit_code: Some(0),
                ..ProcessExecutionStatus::default()
            },
            quality: unavailable_quality(),
            resolve: Some(CachePilotResolveOutcome {
                status: "hit".to_owned(),
                cache_resolution: CacheResolution::CoverageHit {
                    artifact_id: Digest::blake3(b"artifact"),
                    sufficiency_certificate_id: needle_core::ReuseSufficiencyCertificateId(
                        Digest::blake3(b"proof"),
                    ),
                    selected_plan_id: needle_core::SelectedPlanId(Digest::blake3(b"plan")),
                    resolution_format_revision: PROOF_RESOLUTION_FORMAT_REVISION,
                },
                cache_hit: true,
                worker_spawned: false,
                result_digest: Digest::blake3(b"result"),
            }),
            resolve_sequence: Vec::new(),
            need_steps: Vec::new(),
            need_requests: Vec::new(),
            worker_runs_before: 1,
            worker_runs_after: 1,
            worker_run_delta: 0,
            logical_worker_spawns: 0,
            worker_turns: 0,
            repair_performed: false,
            command_evidence_before: 1,
            command_evidence_after: 1,
            command_evidence_delta: 0,
            main_discovery_total: 0,
            wall_time_ms: 1,
            main_input_tokens: Some(1),
            main_cached_input_tokens: Some(0),
            main_output_tokens: Some(1),
            worker_input_tokens: None,
            worker_cached_input_tokens: None,
            worker_output_tokens: None,
            main_cost: None,
            worker_cost: None,
            selected_plan: None,
        };
        assert!(valid_proof_hit(&arm));
        let mut exact = arm;
        if let Some(resolve) = exact.resolve.as_mut() {
            resolve.cache_resolution = CacheResolution::ExactHit {
                artifact_id: Digest::blake3(b"artifact"),
                sufficiency_certificate_id: Some(needle_core::ReuseSufficiencyCertificateId(
                    Digest::blake3(b"proof"),
                )),
                selected_plan_id: Some(needle_core::SelectedPlanId(Digest::blake3(b"plan"))),
                resolution_format_revision: Some(PROOF_RESOLUTION_FORMAT_REVISION),
            };
        }
        assert!(!valid_proof_hit(&exact));
        assert!(valid_economic_hit(&exact));
        if let Some(resolve) = exact.resolve.as_mut() {
            resolve.cache_resolution = CacheResolution::CompositeHit {
                artifact_ids: vec![Digest::blake3(b"location"), Digest::blake3(b"flow")],
                sufficiency_certificate_id: Some(needle_core::ReuseSufficiencyCertificateId(
                    Digest::blake3(b"composite-proof"),
                )),
                selected_plan_id: Some(needle_core::SelectedPlanId(Digest::blake3(
                    b"composite-plan",
                ))),
                resolution_format_revision: Some(PROOF_RESOLUTION_FORMAT_REVISION),
            };
        }
        assert!(valid_economic_hit(&exact));
    }

    #[test]
    fn supervised_process_status_preserves_spawn_timeout_and_abort_causes() {
        let spawned = process_status(Some("cannot spawn Codex App Server: missing"));
        assert!(spawned.spawn_error.as_deref().unwrap().contains("missing"));
        assert!(!spawned.timed_out);

        let timed_out = process_status(Some("main continuation turn timed out"));
        assert!(timed_out.timed_out);
        assert!(timed_out.abort_reason.as_deref().unwrap().contains("continuation"));

        let aborted = process_status(Some("main repeated discovery after Needle continuation"));
        assert!(aborted.abort_reason.as_deref().unwrap().contains("repeated discovery"));
    }

    #[test]
    fn live_miss_accepts_exactly_one_consistent_logical_worker() {
        assert!(valid_worker_count(1, 1));
        assert!(!valid_worker_count(0, 0));
        assert!(!valid_worker_count(1, 2));
        assert!(!valid_worker_count(2, 2));
        assert!(!valid_worker_count(3, 3));
    }

    #[test]
    fn arm_cost_follows_the_worker_that_actually_executed() {
        let main = test_cost("gpt-5.6-sol", 1_000);
        let worker = test_cost("gpt-5.6-luna", 200);
        let mut arm = MinimalLiveArm::skipped(FinalArm::ExactHit, "test", 0, 0);
        arm.main_cost = Some(main);
        assert_eq!(arm.total_cost_microcredits(), Some(1_000));

        arm.worker_run_delta = 1;
        arm.logical_worker_spawns = 1;
        arm.worker_turns = 1;
        assert_eq!(arm.total_cost_microcredits(), None);

        arm.worker_cost = Some(worker);
        assert_eq!(arm.total_cost_microcredits(), Some(1_200));
    }

    #[test]
    fn advisory_coverage_plan_is_a_valid_candidate_not_an_authoritative_hit() {
        let mut arm = MinimalLiveArm::skipped(FinalArm::ExactHit, "test", 0, 0);
        arm.selected_plan = Some(SelectedPlan {
            id: needle_core::SelectedPlanId(Digest::blake3(b"plan")),
            need: needle_core::NeedId(Digest::blake3(b"need")),
            artifact_ids: vec![needle_core::ArtifactId(Digest::blake3(b"artifact"))],
            claim_ids: Vec::new(),
            claim_validation_certificate_ids: Vec::new(),
            claim_set_certificate_ids: Vec::new(),
            covered_mask: 1,
            missing_mask: 0,
            economics: needle_core::PlanEconomics {
                expected_fresh_microusd: Some(1_000),
                expected_selected_microusd: Some(1_200),
                proof_overhead_micros: 10,
                expected_net_microusd: Some(-200),
            },
            proof_budget: needle_core::ProofBudget::default(),
            decision_reason: "Advisory::CoverageHit".to_owned(),
        });
        assert!(valid_cache_candidate(&arm));
        assert!(!valid_proof_hit(&arm));
    }

    #[test]
    fn savings_basis_points_preserve_positive_zero_and_negative_results() {
        assert_eq!(savings_basis_points(Some(100), Some(40)), Some(6_000));
        assert_eq!(savings_basis_points(Some(100), Some(100)), Some(0));
        assert_eq!(savings_basis_points(Some(100), Some(125)), Some(-2_500));
        assert_eq!(savings_basis_points(Some(0), Some(0)), None);
        assert_eq!(savings_basis_points(None, Some(1)), None);
    }

    #[test]
    fn economic_stage_budget_adds_main_only_and_keeps_reserves_explicit() {
        let protocol =
            load_legacy_offline_protocol(&workspace_path(DEFAULT_LEGACY_OFFLINE_MANIFEST)).unwrap();
        let budget = protocol.economic_stage_budget().unwrap();
        assert_eq!(budget.main_only_microcredits, 11_158_850);
        assert_eq!(budget.needle_miss_microcredits, 5_311_720);
        assert_eq!(budget.needle_hit_microcredits, 357_425);
        assert_eq!(budget.base_microcredits, 16_827_995);
        assert_eq!(budget.worker_reserve_microcredits, 0);
        assert_eq!(budget.main_turn_reserve_microcredits, 5_267_750);
        assert_eq!(budget.total_microcredits, 22_095_745);
    }

    fn test_cost(model: &str, total_microcredits: u64) -> TokenCost {
        TokenCost {
            pricing_snapshot_digest: Digest::blake3(b"pricing"),
            pricing_revision: "test".to_owned(),
            model: model.to_owned(),
            service_tier: "default".to_owned(),
            unit: "credits".to_owned(),
            uncached_input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            uncached_input_microcredits_per_million: 1,
            cached_input_microcredits_per_million: 1,
            output_microcredits_per_million: 1,
            uncached_input_microcredits: 0,
            cached_input_microcredits: 0,
            output_microcredits: total_microcredits,
            total_microcredits,
            rounding: "test".to_owned(),
        }
    }
}
