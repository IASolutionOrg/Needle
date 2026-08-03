use super::{
    AppError, Execution, FinalArm, MinimalLiveArm, QualityOracleResult, QualityOracleSpec,
    price_usage_observation_optional, process_status,
};
use needle_core::{EvidenceFailurePolicy, WorkerConfig};
use needle_platform_codex::{
    CodexMainSession, MainSessionConfig, MainUsage, PILOT_MAIN_REPOSITORY_INSPECTION_INSTRUCTIONS,
};
use needle_runtime::RuntimeStore;
use std::fs;
use std::path::Path;
use std::time::Instant;

pub(super) struct DirectObservation<'a> {
    pub(super) context: &'a Execution<'a>,
    pub(super) repository: &'a Path,
    pub(super) output: &'a Path,
    pub(super) source_snapshot_digest: needle_core::Digest,
    pub(super) repository_id: needle_core::Digest,
    pub(super) prompt: &'a str,
    pub(super) quality_spec: &'a QualityOracleSpec,
}

pub(super) fn observe(context: DirectObservation<'_>) -> Result<MinimalLiveArm, AppError> {
    fs::create_dir_all(context.output)?;
    let target_root = context.output.join("main-target");
    let temp_root = context.output.join("main-temp");
    fs::create_dir_all(&target_root)?;
    fs::create_dir_all(&temp_root)?;
    let config = WorkerConfig {
        executable: context.context.codex.display().to_string(),
        model: context.context.main_model.to_owned(),
        reasoning: context.context.main_reasoning.to_owned(),
        service_tier: Some(context.context.service_tier.to_owned()),
        timeout_seconds: context.context.timeout.as_secs(),
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
    };
    let store = RuntimeStore::new(context.output.join("main-only.sqlite3"));
    store.initialize().map_err(|error| AppError::Experiment(error.to_string()))?;

    let started = Instant::now();
    let mut transport_success = false;
    let mut final_response = String::new();
    let mut usage = MainUsage::default();
    let mut tool_items_started = 0;
    let execution_error = match CodexMainSession::start_pilot(MainSessionConfig {
        codex: &config,
        codex_home: context.context.codex_home,
        instructions: PILOT_MAIN_REPOSITORY_INSPECTION_INSTRUCTIONS,
        checkout_root: context.repository,
        target_root: &target_root,
        temp_root: &temp_root,
        snapshot_digest: context.source_snapshot_digest,
        repository_id: context.repository_id,
        route: "benchmark.main-only",
        store,
    }) {
        Ok(mut session) => {
            transport_success = true;
            let turn = session.run_direct(context.prompt, context.context.timeout);
            let turn_error = match turn {
                Ok(turn) => {
                    final_response = turn.response;
                    usage = turn.usage;
                    tool_items_started = turn.tool_items_started;
                    None
                }
                Err(error) => {
                    usage = error.usage;
                    tool_items_started = error.tool_items_started;
                    Some(error.diagnostic)
                }
            };
            let cleanup_error = session.cleanup().err();
            turn_error.or(cleanup_error)
        }
        Err(error) => Some(error),
    };
    if !final_response.is_empty() {
        fs::write(context.output.join("main-final-response.txt"), &final_response)?;
    }
    if let Some(error) = execution_error.as_ref() {
        fs::write(context.output.join("main-error.txt"), error)?;
    }
    let quality = QualityOracleResult::evaluate(context.quality_spec, &final_response, None);
    let main_cost = price_usage_observation_optional(
        context.context.pricing,
        context.context.main_model,
        context.context.service_tier,
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.output_tokens,
    )?;

    Ok(MinimalLiveArm {
        arm: FinalArm::FrontierDirect,
        observed_arm: FinalArm::FrontierDirect,
        provider_observation_started: transport_success && !context.context.offline,
        transport_success,
        process_success: execution_error.is_none(),
        process: process_status(execution_error.as_deref()),
        quality,
        resolve: None,
        resolve_sequence: Vec::new(),
        need_steps: Vec::new(),
        need_requests: Vec::new(),
        worker_runs_before: 0,
        worker_runs_after: 0,
        worker_run_delta: 0,
        logical_worker_spawns: 0,
        worker_turns: 0,
        repair_performed: false,
        command_evidence_before: 0,
        command_evidence_after: 0,
        command_evidence_delta: 0,
        main_discovery_total: tool_items_started,
        wall_time_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        main_input_tokens: usage.input_tokens,
        main_cached_input_tokens: usage.cached_input_tokens,
        main_output_tokens: usage.output_tokens,
        worker_input_tokens: None,
        worker_cached_input_tokens: None,
        worker_output_tokens: None,
        main_cost,
        worker_cost: None,
        selected_plan: None,
    })
}
