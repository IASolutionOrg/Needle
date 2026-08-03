use super::protocol::{pilot_main_instructions, quality_spec_for_task, test_plan};
use super::*;
use needle_bench::{BenchmarkRoute, CorpusTask};
use serde::{Deserialize, Serialize};

const REPORT_SCHEMA: &str = "needle.multi-task-live-report/1";
const LOCATE_MAIN_SCENARIO: &str = "main_interrupt_r61_locate";
const LOCATE_WORKER_SCENARIO: &str = "worker_r61_locate";
const TRACE_MAIN_SCENARIO: &str = "main_interrupt_r61_trace";
const TRACE_WORKER_SCENARIO: &str = "worker_r61_trace";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MultiTaskLiveReport {
    schema: String,
    mode: String,
    campaign_digest: Digest,
    source_sha: String,
    source_snapshot_digest: Digest,
    codex_version: String,
    main_model: String,
    worker_model: String,
    service_tier: String,
    pricing_snapshot_digest: Digest,
    estimated_budget_microcredits: u64,
    approved_budget_microcredits: u64,
    automatic_retries: u32,
    maximum_provider_observations: usize,
    provider_observations_started: usize,
    maximum_logical_workers: usize,
    logical_worker_spawns: u32,
    checkout_clean: bool,
    tasks: Vec<MultiTaskObservation>,
    total_observed_cost_microcredits: Option<u64>,
    passed: bool,
    failures: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MultiTaskObservation {
    task_id: String,
    route: String,
    main_only: MinimalLiveArm,
    needle_miss: MinimalLiveArm,
    validation_certificates: u64,
    routing_saving_basis_points: Option<i64>,
    checkout_clean: bool,
    quality_equivalent: bool,
    exactly_one_worker: bool,
    zero_main_discovery: bool,
    costs_available: bool,
    passed: bool,
    failures: Vec<String>,
}

pub(super) fn execute(context: &Execution<'_>) -> Result<(), AppError> {
    fs::create_dir_all(context.artifact_root)?;
    let artifact_root = canonical_child_path(context.artifact_root)?;
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
        artifact_root.join("multi-task-campaign.json"),
        serde_json::to_vec_pretty(context.protocol.campaign())?,
    )?;

    let repository = clone_local_checkout(context.source_repository, &artifact_root.join("repo"))?;
    let (_, initial_snapshot) = capture_git_snapshot(&repository)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let profile =
        HookConfig::default().profile().map_err(|error| AppError::Experiment(error.to_string()))?;
    let main_instructions = pilot_main_instructions(&profile.rendered_context_owned());
    let mut observations = Vec::with_capacity(context.protocol.campaign().task_ids.len());

    for (task_index, task_id) in context.protocol.campaign().task_ids.iter().enumerate() {
        let (task, oracle) = context.protocol.campaign_task(task_id)?;
        if context.offline {
            configure_simulator(context.codex_home, task.route)?;
        }
        observations.push(observe_pair(
            context,
            &artifact_root,
            &repository,
            initial_snapshot.source_digest,
            initial_snapshot.repository_id,
            profile.definition_digest,
            &main_instructions,
            task_index,
            task,
            oracle,
        )?);
    }

    let (_, final_snapshot) = capture_git_snapshot(&repository)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let checkout_clean = initial_snapshot.source_digest == final_snapshot.source_digest
        && repository_status_clean(&repository)?;
    let provider_observations_started = observations
        .iter()
        .flat_map(|observation| [&observation.main_only, &observation.needle_miss])
        .filter(|arm| arm.provider_observation_started)
        .count();
    let logical_worker_spawns = observations
        .iter()
        .map(|observation| observation.needle_miss.logical_worker_spawns)
        .fold(0_u32, u32::saturating_add);
    let total_observed_cost_microcredits = observations
        .iter()
        .flat_map(|observation| [&observation.main_only, &observation.needle_miss])
        .try_fold(0_u64, |total, arm| total.checked_add(arm.total_cost_microcredits()?));
    let mut failures = Vec::new();
    if observations.len() != context.protocol.campaign().task_ids.len() {
        failures.push("campaign_task_count".to_owned());
    }
    if observations.iter().any(|observation| !observation.passed) {
        failures.push("task_gate".to_owned());
    }
    if !checkout_clean {
        failures.push("checkout_integrity".to_owned());
    }
    if logical_worker_spawns as usize != observations.len() {
        failures.push("logical_worker_count".to_owned());
    }
    if total_observed_cost_microcredits.is_none() {
        failures.push("usage_or_pricing".to_owned());
    }
    let expected_provider_observations = if context.offline { 0 } else { observations.len() * 2 };
    if provider_observations_started != expected_provider_observations {
        failures.push("provider_observation_count".to_owned());
    }
    let report = MultiTaskLiveReport {
        schema: REPORT_SCHEMA.to_owned(),
        mode: if context.offline {
            "multi-task-offline-simulator"
        } else {
            "multi-task-provider-live"
        }
        .to_owned(),
        campaign_digest: context
            .protocol
            .manifest
            .campaign_digest
            .as_deref()
            .and_then(|value| Digest::parse(value).ok())
            .ok_or_else(|| AppError::Experiment("campaign digest is invalid".to_owned()))?,
        source_sha: context.protocol.task().repository_sha.clone(),
        source_snapshot_digest: initial_snapshot.source_digest,
        codex_version: context.codex_version.to_owned(),
        main_model: context.main_model.to_owned(),
        worker_model: context.worker_model.to_owned(),
        service_tier: context.service_tier.to_owned(),
        pricing_snapshot_digest: context.pricing_digest,
        estimated_budget_microcredits: context.estimated_budget,
        approved_budget_microcredits: context.approved_budget,
        automatic_retries: 0,
        maximum_provider_observations: observations.len() * 2,
        provider_observations_started,
        maximum_logical_workers: observations.len(),
        logical_worker_spawns,
        checkout_clean,
        total_observed_cost_microcredits,
        passed: failures.is_empty(),
        failures,
        tasks: observations,
    };
    let output = artifact_root.join("multi-task-live-report.json");
    fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
    println!("multi-task live report written to {}", output.display());
    if report.passed {
        Ok(())
    } else {
        Err(AppError::Experiment(format!(
            "multi-task live gate failed: {}",
            report.failures.join(", ")
        )))
    }
}

#[allow(clippy::too_many_arguments)]
fn observe_pair(
    context: &Execution<'_>,
    artifact_root: &Path,
    repository: &Path,
    source_snapshot_digest: Digest,
    repository_id: Digest,
    prompt_profile_digest: Digest,
    main_instructions: &str,
    task_index: usize,
    task: &CorpusTask,
    oracle: &needle_bench::BenchmarkOracle,
) -> Result<MultiTaskObservation, AppError> {
    // Worker sandboxes add several nested path components. Keep the on-disk
    // campaign key short for pinned repositories with deep paths on Windows;
    // the immutable task ID remains in the report.
    let task_root = artifact_root.join(format!("t{:02}", task_index + 1));
    let product_data = task_root.join("product-data");
    let main_only_output = task_root.join("main-only");
    let miss_output = task_root.join("needle-miss");
    fs::create_dir_all(&product_data)?;
    fs::write(product_data.join("root-task.txt"), &task.prompt)?;
    let declared_test_plan = test_plan(task);
    fs::write(
        product_data.join("test-plan.json"),
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

    let quality_spec = quality_spec_for_task(task, oracle)?;
    let main_only = observe_direct_main(DirectObservation {
        context,
        repository,
        output: &main_only_output,
        source_snapshot_digest,
        repository_id,
        prompt: &task.prompt,
        quality_spec: &quality_spec,
    })?;
    let route = route_key(task.route);
    let needle_miss = observe_arm(Observe {
        arm: FinalArm::NeedleMiss,
        route,
        codex: context.codex,
        main_reasoning: context.main_reasoning,
        repository,
        codex_home: context.codex_home,
        product_data: &product_data,
        output: &miss_output,
        source_snapshot_digest,
        repository_id,
        prompt_profile_digest,
        main_instructions,
        prompt: &task.prompt,
        declared_test_plan: &declared_test_plan,
        store: &store,
        pricing: context.pricing,
        main_model: context.main_model,
        worker_model: context.worker_model,
        service_tier: context.service_tier,
        timeout: context.timeout,
        quality_spec: &quality_spec,
        inherited_test_evidence: false,
        offline: context.offline,
        cache_only: false,
    })?;
    let validation_certificates = store
        .artifacts()
        .map_err(|error| AppError::Experiment(error.to_string()))?
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
    let (_, final_snapshot) = capture_git_snapshot(repository)
        .map_err(|error| AppError::Experiment(error.to_string()))?;
    let checkout_clean = final_snapshot.source_digest == source_snapshot_digest
        && repository_status_clean(repository)?;
    let quality_equivalent = main_only.quality.passed && needle_miss.quality.passed;
    let exactly_one_worker = valid_miss(&needle_miss)
        && needle_miss.logical_worker_spawns == 1
        && needle_miss.worker_turns == 1
        && validation_certificates > 0;
    let zero_main_discovery = needle_miss.main_discovery_total == 0;
    let costs_available = main_only.total_cost_microcredits().is_some()
        && needle_miss.total_cost_microcredits().is_some();
    let mut failures = Vec::new();
    for (passed, name) in [
        (main_only.transport_success && main_only.process_success, "frontier_direct_execution"),
        (quality_equivalent, "quality"),
        (exactly_one_worker, "needle_miss_worker"),
        (zero_main_discovery, "needle_main_discovery"),
        (costs_available, "usage_or_pricing"),
        (checkout_clean, "checkout_integrity"),
    ] {
        if !passed {
            failures.push(name.to_owned());
        }
    }
    let routing_saving_basis_points = savings_basis_points(
        main_only.total_cost_microcredits(),
        needle_miss.total_cost_microcredits(),
    );
    Ok(MultiTaskObservation {
        task_id: task.id.clone(),
        route: route.to_owned(),
        main_only,
        needle_miss,
        validation_certificates,
        routing_saving_basis_points,
        checkout_clean,
        quality_equivalent,
        exactly_one_worker,
        zero_main_discovery,
        costs_available,
        passed: failures.is_empty(),
        failures,
    })
}

fn configure_simulator(codex_home: &Path, route: BenchmarkRoute) -> Result<(), AppError> {
    let (main, worker) = match route {
        BenchmarkRoute::LocateImplementation => (LOCATE_MAIN_SCENARIO, LOCATE_WORKER_SCENARIO),
        BenchmarkRoute::TraceStateFlow => (TRACE_MAIN_SCENARIO, TRACE_WORKER_SCENARIO),
    };
    fs::write(codex_home.join(".needle-simulation-main-scenario"), main)?;
    fs::write(codex_home.join(".needle-simulation-worker-scenario"), worker)?;
    Ok(())
}

const fn route_key(route: BenchmarkRoute) -> &'static str {
    match route {
        BenchmarkRoute::LocateImplementation => "locate.implementation",
        BenchmarkRoute::TraceStateFlow => "trace.state-flow",
    }
}
