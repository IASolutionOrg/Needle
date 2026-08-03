use crate::{
    CalibrationReplayError, CampaignCostModel, FinalArm, FrozenCorpusManifest, MinimalLivePilot,
    preflight_frozen_corpus,
};
use needle_core::{
    CacheResolution, CapabilityMode, Claim, CodexHost, CodexRole, CommandExecutionEvidence,
    CommandPolicy, Digest, EvidenceFailurePolicy, EvidenceReference, FallbackPolicy,
    FilesystemPolicy, NeedIr, NeedResult, NetworkPolicy, PredicateKind, RepairPolicy, ReuseUnit,
    RoleProfileBudget, RoleProfileDefinition, RoleProfileDefinitionInput, RoleProfileId,
    SemanticArtifactResult, SemanticInterrupt, SemanticWorkerArtifact, ServiceTier, TestPlan,
    TestPolicy, ToolPolicy, Uncertainty, WorkerConfig, WorkerFailure, WorkerOutcome, WorkerRequest,
};
use needle_runtime::{
    ResolveOutcome, ResolveRequest, RouteCostObservation, RuntimeEngine, RuntimeError,
    RuntimeSettings, RuntimeStore, SnapshotError, StoreError, WorkerExecutor, capture_git_snapshot,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const MINIMAL_PILOT_DRY_RUN_SCHEMA_ID: &str = "needle.minimal-pilot-dry-run/3";
pub const REWORDED_COVERAGE_HIT_PROMPT: &str = "Find the primary code location responsible for the --glob-case-insensitive command-line option.";
pub const ECONOMIC_EQUIVALENT_HIT_PROMPT: &str = "Identify the primary implementation location that handles ripgrep's --glob-case-insensitive option.";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DryRunArm {
    pub arm: FinalArm,
    pub status: String,
    pub resolution: String,
    pub worker_spawned: bool,
    pub cache_hit: bool,
    pub result_digest: String,
    pub continuation_rendered: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MinimalPilotDryRunReport {
    pub schema: String,
    pub mode: String,
    pub provider_calls: u32,
    pub automatic_retries: bool,
    pub task_id: String,
    pub route: String,
    pub source_repository: String,
    pub source_sha: String,
    pub snapshot_identity_revision: u16,
    pub repository_id: String,
    pub source_snapshot_digest: String,
    pub source_clean_before: bool,
    pub source_clean_after: bool,
    pub artifact_root: String,
    pub worker_spawns: u32,
    pub miss: DryRunArm,
    pub hit: DryRunArm,
    pub expected_hit_resolution: String,
    pub hit_prompt_reworded: bool,
    pub semantic_interrupt_digest_matches: bool,
    pub same_result_digest: bool,
    pub reused_location_artifact: bool,
    pub semantic_artifact_count: u64,
    pub validation_certificate_count: u64,
    pub command_evidence_count: u64,
    pub validation_rejection_count: u64,
    pub focused_test_projected_on_miss: bool,
    pub bootstrap_source: String,
    pub bootstrap_reuse_cost_microcredits: u64,
    pub simulated_fresh_cost_microcredits: u64,
    pub observed_reuse_cost_present: bool,
    pub observed_reuse_supersedes_bootstrap: bool,
    pub capability_mode: CapabilityMode,
    pub passed: bool,
    pub provider_run_ready: bool,
    pub remaining_blockers: Vec<String>,
}

#[derive(Debug, Error)]
pub enum MinimalPilotDryRunError {
    #[error("minimal pilot dry-run I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("minimal pilot dry-run JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("minimal pilot dry-run store failed: {0}")]
    Store(#[from] StoreError),
    #[error("minimal pilot dry-run runtime failed: {0}")]
    Runtime(#[from] RuntimeError),
    #[error("minimal pilot dry-run snapshot failed: {0}")]
    Snapshot(#[from] SnapshotError),
    #[error("minimal pilot dry-run fixture failed: {0}")]
    Fixture(#[from] CalibrationReplayError),
    #[error("minimal pilot dry-run input is invalid: {0}")]
    Invalid(String),
}

#[derive(Clone)]
struct DeterministicPilotWorker {
    spawns: Arc<AtomicU32>,
    store: RuntimeStore,
}

impl WorkerExecutor for DeterministicPilotWorker {
    fn execute(
        &self,
        config: &WorkerConfig,
        request: &WorkerRequest,
    ) -> Result<WorkerOutcome, Box<WorkerFailure>> {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        let repository_root = Path::new(&request.repository_root);
        if let Some(plan) = request.declared_test_plan.as_ref() {
            let output = format!(
                "running 1 test\ntest {} ... ok\n\ntest result: ok. 1 passed; 0 failed",
                plan.test_identifier
            );
            self.store
                .record_command_evidence(
                    None,
                    &CommandExecutionEvidence {
                        id: format!(
                            "command-evidence-r44-{}",
                            Digest::blake3(output.as_bytes()).to_hex()
                        ),
                        approval_id: "deterministic-r44-auto-approval".to_owned(),
                        argv: plan.argv.clone(),
                        cwd: request.repository_root.clone(),
                        source_snapshot_digest: request.repository_snapshot.source_digest,
                        runner: plan.runner.clone(),
                        runner_version: Some("deterministic-offline-fixture".to_owned()),
                        exit_status: Some(0),
                        duration_ms: 1,
                        output_digest: Digest::blake3(output.as_bytes()),
                        output_preview: output,
                        test_identifier: Some(plan.test_identifier.clone()),
                        tests_executed: Some(1),
                        infrastructure_failure: None,
                    },
                )
                .map_err(|error| {
                    Box::new(WorkerFailure {
                        code: "fixture-command-evidence".to_owned(),
                        diagnostic: error.to_string(),
                        input_tokens: None,
                        cached_input_tokens: None,
                        output_tokens: None,
                        duration_ms: 0,
                        logical_worker_spawns: 1,
                        worker_turns: 1,
                        repair_performed: false,
                        discarded_facts: 0,
                        worker_session_id: None,
                        session_cleanup_success: Some(true),
                        role_profile_provenance: None,
                    })
                })?;
        }
        let mut semantic_result = crate::calibration_replay::semantic_worker_result(
            repository_root,
        )
        .map_err(|error| {
            Box::new(WorkerFailure {
                code: "fixture".to_owned(),
                diagnostic: error.to_string(),
                input_tokens: None,
                cached_input_tokens: None,
                output_tokens: None,
                duration_ms: 0,
                logical_worker_spawns: 1,
                worker_turns: 1,
                repair_performed: false,
                discarded_facts: 0,
                worker_session_id: None,
                session_cleanup_success: Some(true),
                role_profile_provenance: None,
            })
        })?;
        retain_location_artifact(&mut semantic_result);
        if let Some(SemanticWorkerArtifact::CodeLocation { locations, .. }) =
            semantic_result.artifacts.first_mut()
        {
            for location in locations {
                location.byte_start = None;
                location.byte_end = None;
            }
        }
        let location = semantic_result
            .artifacts
            .iter()
            .find_map(|artifact| match artifact {
                SemanticWorkerArtifact::CodeLocation { locations, .. } => locations.first(),
                _ => None,
            })
            .ok_or_else(|| {
                Box::new(WorkerFailure {
                    code: "fixture".to_owned(),
                    diagnostic: "fixture produced no implementation location".to_owned(),
                    input_tokens: None,
                    cached_input_tokens: None,
                    output_tokens: None,
                    duration_ms: 0,
                    logical_worker_spawns: 1,
                    worker_turns: 1,
                    repair_performed: false,
                    discarded_facts: 0,
                    worker_session_id: None,
                    session_cleanup_success: Some(true),
                    role_profile_provenance: None,
                })
            })?;
        let evidence_id = "implementation-location".to_owned();
        let test_evidence = request.declared_test_plan.as_ref().map(|plan| EvidenceReference {
            id: "focused-test".to_owned(),
            path: "tests/misc.rs".to_owned(),
            symbol: Some(plan.test_identifier.clone()),
            content_digest: Digest::blake3(b"bound-by-runtime"),
            byte_start: None,
            byte_end: None,
        });
        let mut evidence = vec![EvidenceReference {
            id: evidence_id.clone(),
            path: location.path.clone(),
            symbol: location.symbol.clone(),
            content_digest: Digest::blake3(b"bound-by-runtime"),
            byte_start: location.byte_start,
            byte_end: location.byte_end,
        }];
        if let Some(test_evidence) = test_evidence {
            evidence.push(test_evidence);
        }
        Ok(WorkerOutcome {
            result: NeedResult {
                complete: true,
                summary: "Located the primary implementation for --glob-case-insensitive."
                    .to_owned(),
                claims: vec![Claim {
                    id: "implementation".to_owned(),
                    kind: "implementation".to_owned(),
                    subject: "--glob-case-insensitive".to_owned(),
                    statement: "The primary option implementation is GlobCaseInsensitive."
                        .to_owned(),
                    evidence_ids: vec![evidence_id.clone()],
                }],
                evidence,
                suggested_reads: Vec::new(),
                suggested_commands: Vec::new(),
                uncertainty: Vec::<Uncertainty>::new(),
            },
            artifact_result: None,
            semantic_artifact_result: Some(semantic_result),
            worker_model: config.model.clone(),
            worker_reasoning: config.reasoning.clone(),
            codex_version: "deterministic-offline-fixture".to_owned(),
            input_tokens: None,
            cached_input_tokens: None,
            output_tokens: None,
            duration_ms: 0,
            process_status: "success".to_owned(),
            logical_worker_spawns: 1,
            worker_turns: 1,
            repair_performed: false,
            discarded_facts: 0,
            worker_session_id: None,
            session_cleanup_success: Some(true),
            role_profile_provenance: config.role_profile_provenance.clone(),
        })
    }
}

pub fn run_minimal_pilot_dry_run(
    manifest_path: &Path,
    source_repository: &Path,
    artifact_root: &Path,
) -> Result<MinimalPilotDryRunReport, MinimalPilotDryRunError> {
    run_minimal_pilot_dry_run_with_hit_prompt(manifest_path, source_repository, artifact_root, None)
}

pub fn run_minimal_pilot_reworded_coverage_dry_run(
    manifest_path: &Path,
    source_repository: &Path,
    artifact_root: &Path,
) -> Result<MinimalPilotDryRunReport, MinimalPilotDryRunError> {
    run_minimal_pilot_dry_run_with_hit_prompt(
        manifest_path,
        source_repository,
        artifact_root,
        Some(REWORDED_COVERAGE_HIT_PROMPT),
    )
}

fn run_minimal_pilot_dry_run_with_hit_prompt(
    manifest_path: &Path,
    source_repository: &Path,
    artifact_root: &Path,
    reworded_hit_prompt: Option<&str>,
) -> Result<MinimalPilotDryRunReport, MinimalPilotDryRunError> {
    let manifest_bytes = fs::read(manifest_path)?;
    let manifest: FrozenCorpusManifest = serde_json::from_slice(&manifest_bytes)?;
    let manifest_directory = manifest_path.parent().ok_or_else(|| {
        MinimalPilotDryRunError::Invalid("manifest path has no parent directory".to_owned())
    })?;
    let preflight =
        preflight_frozen_corpus(&manifest, manifest_directory, source_repository, false);
    if !preflight.errors.is_empty() {
        return Err(MinimalPilotDryRunError::Invalid(preflight.errors.join("; ")));
    }
    let pilot_path = manifest_directory.join(&manifest.next_pilot_path);
    let pilot: MinimalLivePilot = serde_json::from_slice(&fs::read(&pilot_path)?)?;
    let cost_model: CampaignCostModel =
        serde_json::from_slice(&fs::read(manifest_directory.join(&manifest.cost_model_path))?)?;
    let task_id = pilot
        .task_ids
        .first()
        .ok_or_else(|| MinimalPilotDryRunError::Invalid("pilot has no task".to_owned()))?;
    let task = manifest.tasks.iter().find(|task| &task.id == task_id).ok_or_else(|| {
        MinimalPilotDryRunError::Invalid("pilot task is not in corpus".to_owned())
    })?;
    let fresh_cost = arm_cost(&cost_model, FinalArm::NeedleMiss)?;
    if artifact_root.join("needle.sqlite3").exists() {
        return Err(MinimalPilotDryRunError::Invalid(format!(
            "artifact root is not fresh: {}",
            artifact_root.display()
        )));
    }
    fs::create_dir_all(artifact_root)?;
    let source_repository = source_repository.canonicalize()?;
    let (_, snapshot) = capture_git_snapshot(&source_repository)?;
    let source_clean_before = git_clean(&source_repository)?;
    let source_sha = git_output(&source_repository, &["rev-parse", "HEAD"])?;
    if !source_clean_before || source_sha.trim() != task.repository_sha {
        return Err(MinimalPilotDryRunError::Invalid(
            "source checkout changed after corpus preflight".to_owned(),
        ));
    }

    let store = RuntimeStore::new(artifact_root.join("needle.sqlite3"));
    store.initialize_defaults(&RuntimeSettings {
        codex_executable: "deterministic-offline-fixture".to_owned(),
        worker_model: "deterministic-offline-fixture".to_owned(),
        worker_reasoning: "none".to_owned(),
        worker_timeout_seconds: 1,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        trusted_test_execution: false,
        multi_need_policy: needle_core::MultiNeedPolicy::default(),
    })?;
    let role_profile_id = deterministic_role_profile(&store)?;
    store.mark_utility_gate_passed()?;
    let spawns = Arc::new(AtomicU32::new(0));
    let engine = RuntimeEngine::new(
        store.clone(),
        DeterministicPilotWorker { spawns: spawns.clone(), store: store.clone() },
    );
    let declared_test_plan = TestPlan {
        runner: "cargo".to_owned(),
        argv: task.focused_command.clone(),
        cwd_relative: ".".to_owned(),
        test_identifier: task.test_identifier.clone(),
        requires_approval: true,
        execution_evidence_id: None,
    };
    let miss_request = resolve_request(
        &store,
        task_id,
        "miss",
        &task.prompt,
        &source_repository,
        Some(declared_test_plan.clone()),
        true,
        &role_profile_id,
    )?;
    let miss_interrupt_digest = semantic_interrupt_digest(&miss_request)?;
    let miss = engine.resolve(&miss_request)?;
    if !miss.worker_spawned || miss.cache_hit || miss.status != "generated" {
        return Err(MinimalPilotDryRunError::Invalid(
            "first arm did not execute as a clean Needle miss".to_owned(),
        ));
    }

    store.record_route_cost_observation(&RouteCostObservation {
        route_key: pilot.promotion_bootstrap.route.clone(),
        cost_microusd: fresh_cost,
        source: "fresh".to_owned(),
        evidence_digest: Digest::blake3(b"minimal-pilot-dry-run-fresh-estimate"),
        observed_unix_ms: now_ms(),
    })?;
    store.record_route_cost_observation(&RouteCostObservation {
        route_key: pilot.promotion_bootstrap.route.clone(),
        cost_microusd: pilot.promotion_bootstrap.reuse_cost_microcredits,
        source: "reuse_bootstrap".to_owned(),
        evidence_digest: bootstrap_evidence_digest(manifest_directory, &pilot)?,
        observed_unix_ms: now_ms().saturating_add(1),
    })?;
    let class = store
        .capability_classes()?
        .into_iter()
        .find(|class| {
            class.reuse_unit == ReuseUnit::Artifact
                && class.predicate == PredicateKind::ImplementationLocation
        })
        .ok_or_else(|| {
            MinimalPilotDryRunError::Invalid(
                "ImplementationLocation capability is missing".to_owned(),
            )
        })?;
    let promoted = store
        .set_capability_mode(
            &class.id,
            class.definition_digest,
            CapabilityMode::Authoritative,
            Some(bootstrap_evidence_digest(manifest_directory, &pilot)?),
        )?
        .ok_or_else(|| {
            MinimalPilotDryRunError::Invalid("capability promotion was not persisted".to_owned())
        })?;

    let hit_prompt = reworded_hit_prompt.unwrap_or(&task.prompt);
    let expected_hit_resolution =
        if reworded_hit_prompt.is_some() { "CoverageHit" } else { "ExactHit" };
    let reworded_hit = reworded_hit_prompt.is_some();
    let hit_request = resolve_request(
        &store,
        task_id,
        "hit",
        hit_prompt,
        &source_repository,
        (!reworded_hit).then_some(declared_test_plan),
        !reworded_hit,
        &role_profile_id,
    )?;
    let hit_interrupt_digest = semantic_interrupt_digest(&hit_request)?;
    let semantic_interrupt_digest_matches = miss_interrupt_digest == hit_interrupt_digest;
    let hit = engine.resolve(&hit_request)?;
    let same_result_digest = miss.result_digest == hit.result_digest;
    let artifacts = store.artifacts()?;
    let semantic_artifacts = artifacts
        .iter()
        .filter(|artifact| {
            serde_json::from_value::<SemanticWorkerArtifact>(artifact.payload.clone()).is_ok()
        })
        .collect::<Vec<_>>();
    let validation_certificate_count = semantic_artifacts
        .iter()
        .filter(|artifact| {
            store
                .validation_certificate_for_artifact(&artifact.id.to_string())
                .ok()
                .flatten()
                .is_some()
        })
        .count()
        .try_into()
        .unwrap_or(u64::MAX);
    let location_artifact = semantic_artifacts.iter().find(|artifact| {
        serde_json::from_value::<SemanticWorkerArtifact>(artifact.payload.clone())
            .is_ok_and(|payload| matches!(payload, SemanticWorkerArtifact::CodeLocation { .. }))
    });
    let reused_location_artifact =
        location_artifact.is_some_and(|artifact| match &hit.cache_resolution {
            CacheResolution::CoverageHit { artifact_id, .. } => *artifact_id == artifact.id,
            _ => false,
        });
    let command_evidence_count = store.command_evidence_count()?;
    let validation_rejection_count = store.semantic_validation_rejection_count()?;
    let focused_test_projected_on_miss = miss.rendered.contains(&task.test_identifier)
        && miss.rendered.contains(&task.focused_command.join(" "));
    let semantic_identity_gate = if reworded_hit {
        !semantic_interrupt_digest_matches && reused_location_artifact
    } else {
        semantic_interrupt_digest_matches && same_result_digest
    };
    let source_clean_after = git_clean(&source_repository)?;
    let worker_spawns = spawns.load(Ordering::SeqCst);
    let passed = hit.cache_hit
        && !hit.worker_spawned
        && hit.status == "hit"
        && resolution_name(&hit.cache_resolution) == expected_hit_resolution
        && semantic_identity_gate
        && semantic_artifacts.len() == 2
        && validation_certificate_count == 2
        && command_evidence_count == 1
        && validation_rejection_count == 0
        && focused_test_projected_on_miss
        && worker_spawns == 1
        && source_clean_after
        && continuation_rendered(&miss)
        && continuation_rendered(&hit);
    if !passed {
        return Err(MinimalPilotDryRunError::Invalid(format!(
            "r44 miss-to-authoritative-hit assertions failed: resolution={}, expected={expected_hit_resolution}, cache_hit={}, worker_spawned={}, semantic_digest_match={semantic_interrupt_digest_matches}, result_digest_match={same_result_digest}, reused_location={reused_location_artifact}, semantic_artifacts={}, validation_certificates={validation_certificate_count}, command_evidence={command_evidence_count}, validation_rejections={validation_rejection_count}, focused_test_projected={focused_test_projected_on_miss}, worker_spawns={worker_spawns}, source_clean={source_clean_after}, miss_continuation={}, hit_continuation={}",
            resolution_name(&hit.cache_resolution),
            hit.cache_hit,
            hit.worker_spawned,
            semantic_artifacts.len(),
            continuation_rendered(&miss),
            continuation_rendered(&hit),
        )));
    }
    let observed_reuse_cost_present =
        store.observed_route_cost_by_source(&pilot.promotion_bootstrap.route, "reuse")?.is_some();
    Ok(MinimalPilotDryRunReport {
        schema: MINIMAL_PILOT_DRY_RUN_SCHEMA_ID.to_owned(),
        mode: "deterministic-offline".to_owned(),
        provider_calls: 0,
        automatic_retries: false,
        task_id: task.id.clone(),
        route: pilot.promotion_bootstrap.route,
        source_repository: source_repository.display().to_string(),
        source_sha: source_sha.trim().to_owned(),
        snapshot_identity_revision: snapshot.identity_revision,
        repository_id: snapshot.repository_id.to_string(),
        source_snapshot_digest: snapshot.source_digest.to_string(),
        source_clean_before,
        source_clean_after,
        artifact_root: artifact_root.display().to_string(),
        worker_spawns,
        miss: arm_report(FinalArm::NeedleMiss, &miss),
        hit: arm_report(FinalArm::ExactHit, &hit),
        expected_hit_resolution: expected_hit_resolution.to_owned(),
        hit_prompt_reworded: reworded_hit_prompt.is_some(),
        semantic_interrupt_digest_matches,
        same_result_digest,
        reused_location_artifact,
        semantic_artifact_count: semantic_artifacts.len().try_into().unwrap_or(u64::MAX),
        validation_certificate_count,
        command_evidence_count,
        validation_rejection_count,
        focused_test_projected_on_miss,
        bootstrap_source: "reuse_bootstrap".to_owned(),
        bootstrap_reuse_cost_microcredits: pilot.promotion_bootstrap.reuse_cost_microcredits,
        simulated_fresh_cost_microcredits: fresh_cost,
        observed_reuse_cost_present,
        observed_reuse_supersedes_bootstrap: observed_reuse_cost_present,
        capability_mode: promoted.mode,
        passed,
        provider_run_ready: false,
        remaining_blockers: vec![
            "the dry-run used a deterministic worker and did not exercise live Codex transport"
                .to_owned(),
            "the estimate is not an enforceable provider token ceiling".to_owned(),
            "the authoritative offline hit used promotion bootstrap economics; no provider-observed reuse cost is present"
                .to_owned(),
            "a new explicit user approval is required before the two paid observations".to_owned(),
        ],
    })
}

fn semantic_interrupt_digest(request: &ResolveRequest) -> Result<Digest, MinimalPilotDryRunError> {
    let need_ir = request.need_ir.clone().ok_or_else(|| {
        MinimalPilotDryRunError::Invalid(
            "deterministic pilot request has no typed semantic need".to_owned(),
        )
    })?;
    Ok(SemanticInterrupt::Typed {
        need_ir,
        coordination: needle_core::NeedCoordination::WaitResponse,
    }
    .digest())
}

fn deterministic_role_profile(
    store: &RuntimeStore,
) -> Result<RoleProfileId, MinimalPilotDryRunError> {
    let profile_id = RoleProfileId::new("benchmark.explorer")
        .map_err(|error| MinimalPilotDryRunError::Invalid(error.to_string()))?;
    let definition = RoleProfileDefinition::new(RoleProfileDefinitionInput {
        profile_id: profile_id.clone(),
        role: CodexRole::Explorer,
        host: CodexHost::Codex,
        model: "deterministic-offline-fixture".to_owned(),
        reasoning: needle_core::ReasoningLevel::Low,
        service_tier: ServiceTier::Default,
        timeout_seconds: 1,
        budget: RoleProfileBudget {
            max_turns: 2,
            max_output_tokens: 1200,
            max_cost_microusd: 1000,
        },
        prompt_profile_digest: Digest::blake3(b"minimal-pilot-v04-locate-profile"),
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
    .map_err(|error| MinimalPilotDryRunError::Invalid(error.to_string()))?;
    store.create_role_profile(definition)?;
    let state = store.role_profile_state(&profile_id)?;
    store.activate_role_profile(&profile_id, 1, state.state_digest)?;
    Ok(profile_id)
}

#[allow(clippy::too_many_arguments)]
fn resolve_request(
    store: &RuntimeStore,
    task_id: &str,
    arm: &str,
    prompt: &str,
    source_repository: &Path,
    declared_test_plan: Option<TestPlan>,
    require_focused_tests: bool,
    role_profile_id: &RoleProfileId,
) -> Result<ResolveRequest, MinimalPilotDryRunError> {
    let session = format!("minimal-pilot-{task_id}-{arm}");
    let turn = "turn";
    let profile_digest = Digest::blake3(b"minimal-pilot-v04-locate-profile");
    store.record_session_start_profiled(
        &session,
        profile_digest,
        Some("frontier"),
        source_repository.to_str(),
        role_profile_id,
    )?;
    store.record_user_prompt(&session, Some(turn), prompt, source_repository.to_str())?;
    let focused_tests = if require_focused_tests {
        "@require focused-tests selection=representative completeness=open-world polarity=positive\n"
    } else {
        ""
    };
    let marker = format!(
        "@@need\n\
         @route locate.implementation\n\
         @subject cli-option:\"--glob-case-insensitive\"\n\
         @require implementation-location selection=primary granularity=exact-location polarity=positive\n\
         {focused_tests}\
         @world source=current features=default\n\
         \n\
         {prompt}\n\
         @@end"
    );
    let need_ir = NeedIr::parse(&marker)
        .map_err(|error| MinimalPilotDryRunError::Invalid(error.to_string()))?
        .ok_or_else(|| {
            MinimalPilotDryRunError::Invalid("semantic marker was not parsed".to_owned())
        })?;
    Ok(ResolveRequest {
        session_id: session,
        turn_id: turn.to_owned(),
        platform: "codex".to_owned(),
        main_model: "frontier".to_owned(),
        cwd: source_repository.to_path_buf(),
        need: SemanticInterrupt::Typed {
            need_ir: need_ir.clone(),
            coordination: needle_core::NeedCoordination::WaitResponse,
        }
        .compatibility_request(),
        need_ir: Some(need_ir),
        declared_test_plan,
    })
}

fn retain_location_artifact(result: &mut SemanticArtifactResult) {
    result
        .artifacts
        .retain(|artifact| matches!(artifact, SemanticWorkerArtifact::CodeLocation { .. }));
    result.artifact_traces.retain(|kind, _| kind == &needle_core::ArtifactKind::code_location());
    if let Some(trace) =
        result.artifact_traces.get(&needle_core::ArtifactKind::code_location()).cloned()
    {
        result.observation_trace = trace;
    }
}

fn arm_cost(model: &CampaignCostModel, arm: FinalArm) -> Result<u64, MinimalPilotDryRunError> {
    model
        .arm_estimates
        .iter()
        .find(|estimate| estimate.arm == arm)
        .map(|estimate| estimate.microcredits_per_observation)
        .ok_or_else(|| {
            MinimalPilotDryRunError::Invalid(format!("missing cost estimate for {arm:?}"))
        })
}

fn bootstrap_evidence_digest(
    manifest_directory: &Path,
    pilot: &MinimalLivePilot,
) -> Result<Digest, MinimalPilotDryRunError> {
    let mut hasher = needle_core::CanonicalHasher::new(b"minimal-pilot-promotion-evidence");
    for relative in &pilot.promotion_bootstrap.evidence {
        let path = manifest_directory
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .ok_or_else(|| {
                MinimalPilotDryRunError::Invalid(
                    "cannot resolve promotion evidence from manifest directory".to_owned(),
                )
            })?
            .join(relative);
        hasher.field_str(&relative.replace('\\', "/"));
        hasher.field_digest(Digest::blake3(fs::read(path)?));
    }
    Ok(hasher.finish())
}

fn arm_report(arm: FinalArm, outcome: &ResolveOutcome) -> DryRunArm {
    DryRunArm {
        arm,
        status: outcome.status.clone(),
        resolution: resolution_name(&outcome.cache_resolution).to_owned(),
        worker_spawned: outcome.worker_spawned,
        cache_hit: outcome.cache_hit,
        result_digest: outcome.result_digest.to_string(),
        continuation_rendered: continuation_rendered(outcome),
    }
}

fn continuation_rendered(outcome: &ResolveOutcome) -> bool {
    outcome.rendered.contains("[NEEDLE_CONTEXT]")
        && outcome.rendered.contains("Continue the original task.")
        && outcome.rendered.contains("crates/core/flags/defs.rs")
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
        CacheResolution::Ambiguous { .. } => "Ambiguous",
        CacheResolution::Contradicted { .. } => "Contradicted",
        CacheResolution::Bypass { .. } => "Bypass",
    }
}

fn git_clean(repository: &Path) -> Result<bool, MinimalPilotDryRunError> {
    Ok(git_output(repository, &["status", "--short"])?.trim().is_empty())
}

fn git_output(repository: &Path, arguments: &[&str]) -> Result<String, MinimalPilotDryRunError> {
    let output = Command::new("git").arg("-C").arg(repository).args(arguments).output()?;
    if !output.status.success() {
        return Err(MinimalPilotDryRunError::Invalid(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| MinimalPilotDryRunError::Invalid(error.to_string()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
