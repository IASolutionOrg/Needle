use crate::{FinalArm, FrozenCorpusManifest, validate_frozen_manifest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkOracle {
    pub schema: String,
    pub task_id: String,
    pub repository_sha: String,
    pub evidence: Vec<OracleEvidence>,
    pub focused_test: OracleFocusedTest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OracleEvidence {
    pub role: String,
    pub path: String,
    pub needles: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OracleFocusedTest {
    pub identifier: String,
    pub command: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CorpusTaskPreflight {
    pub task_id: String,
    pub prompt_frozen: bool,
    pub oracle_bound: bool,
    pub source_evidence_present: bool,
    pub focused_test_executed: bool,
    pub focused_test_passed: Option<bool>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CampaignCostReadiness {
    pub next_pilot_observations: usize,
    pub next_pilot_budget_estimate_microcredits: Option<u64>,
    pub full_protocol_calibration_observations: usize,
    pub powered_holdout_observations: Option<usize>,
    pub full_protocol_calibration_budget_estimate_microcredits: Option<u64>,
    pub full_protocol_campaign_budget_estimate_microcredits: Option<u64>,
    pub paid_calibration_observations: usize,
    pub offline_cache_validation_observations: usize,
    pub deferred_diagnostic_observations: usize,
    pub paid_calibration_budget_estimate_microcredits: Option<u64>,
    pub assumptions: Vec<String>,
    pub next_pilot_blockers: Vec<String>,
    pub deferred_final_gate_blockers: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CampaignCostModel {
    pub schema: String,
    pub pricing_snapshot_digest: String,
    pub unit: String,
    pub arm_estimates: Vec<ArmCostEstimate>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmCostEstimate {
    pub arm: FinalArm,
    pub microcredits_per_observation: u64,
    pub basis: String,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinimalLivePilot {
    pub schema: String,
    pub task_ids: Vec<String>,
    pub paid_arms: Vec<FinalArm>,
    pub repetitions_per_task: u32,
    pub automatic_retries: bool,
    pub statistical_claim: bool,
    pub promotion_bootstrap: PromotionBootstrap,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionBootstrap {
    pub route: String,
    pub capability: String,
    pub reuse_cost_microcredits: u64,
    pub evidence: Vec<String>,
    pub replace_after_observed_hit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultiTaskCampaign {
    pub schema: String,
    pub task_ids: Vec<String>,
    pub paid_arms: Vec<FinalArm>,
    pub offline_cache_arms: Vec<FinalArm>,
    pub deferred_diagnostic_arms: Vec<FinalArm>,
    pub repetitions_per_task: u32,
    pub automatic_retries: bool,
    pub statistical_claim: bool,
    pub bootstrap_resamples: usize,
    pub one_sided_alpha_basis_points: u16,
    pub target_power_basis_points: u16,
    pub budget_reserve: CampaignBudgetReserve,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CampaignBudgetReserve {
    pub main_turn_microcredits: u64,
    pub extra_main_turns_per_needle_observation: u32,
    pub worker_microcredits: u64,
    pub extra_workers_per_needle_observation: u32,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CorpusPreflightReport {
    pub schema: String,
    pub corpus_digest: String,
    pub source_repository: String,
    pub source_sha: Option<String>,
    pub source_clean_before: bool,
    pub source_clean_after: bool,
    pub manifest_valid: bool,
    pub arms_fixed: bool,
    pub campaign_valid: bool,
    pub campaign_digest: Option<String>,
    pub tasks: Vec<CorpusTaskPreflight>,
    pub cost_readiness: CampaignCostReadiness,
    pub provider_run_ready: bool,
    pub errors: Vec<String>,
}

pub fn corpus_digest(manifest: &FrozenCorpusManifest) -> String {
    let bytes = serde_json::to_vec(manifest).unwrap_or_default();
    format!("b3:{}", blake3::hash(&bytes).to_hex())
}

pub fn preflight_frozen_corpus(
    manifest: &FrozenCorpusManifest,
    manifest_directory: &Path,
    source_repository: &Path,
    execute_focused_tests: bool,
) -> CorpusPreflightReport {
    let mut errors = validate_frozen_manifest(manifest);
    let manifest_valid = errors.is_empty();
    let arms_fixed = manifest.arms == FinalArm::ALL;
    let source_repository = canonical_or_original(source_repository);
    let source_sha = git_output(&source_repository, &["rev-parse", "HEAD"]).ok();
    let source_clean_before = git_output(&source_repository, &["status", "--short"])
        .is_ok_and(|output| output.trim().is_empty());
    if !source_clean_before {
        errors.push("source repository is not a clean Git checkout".to_owned());
    }
    let repository_identities = manifest
        .tasks
        .iter()
        .map(|task| (&task.repository_url, &task.repository_sha))
        .collect::<BTreeSet<_>>();
    if repository_identities.len() != 1 {
        errors
            .push("this preflight requires one source checkout per repository identity".to_owned());
    }
    if let Some(task) = manifest.tasks.first() {
        if source_sha.as_deref().map(str::trim) != Some(task.repository_sha.as_str()) {
            errors.push(format!("source HEAD does not match frozen SHA {}", task.repository_sha));
        }
        match git_output(&source_repository, &["remote", "get-url", "origin"]) {
            Ok(origin)
                if normalize_git_url(origin.trim()) == normalize_git_url(&task.repository_url) => {}
            Ok(origin) => errors.push(format!(
                "source origin `{}` does not match frozen repository `{}`",
                origin.trim(),
                task.repository_url
            )),
            Err(error) => errors.push(error),
        }
    }

    let manifest_directory = canonical_or_original(manifest_directory);
    let mut task_reports = Vec::with_capacity(manifest.tasks.len());
    for task in &manifest.tasks {
        let mut task_errors = Vec::new();
        let prompt_frozen = task.prompt.trim().len() >= 40
            && !task.prompt.contains("@@need")
            && !task.prompt.contains("crates/")
            && !task.prompt.contains("tests/")
            && !task.prompt.contains("::");
        if !prompt_frozen {
            task_errors.push("prompt is not a bounded natural task prompt".to_owned());
        }
        let oracle_path = manifest_directory.join(&task.oracle_path);
        let oracle_bytes = fs::read(&oracle_path);
        let mut oracle_bound = false;
        let mut source_evidence_present = false;
        if let Ok(bytes) = oracle_bytes {
            let actual_digest = format!("b3:{}", blake3::hash(&bytes).to_hex());
            if actual_digest != task.oracle_digest {
                task_errors.push(format!(
                    "oracle digest mismatch: expected {}, observed {actual_digest}",
                    task.oracle_digest
                ));
            } else {
                match serde_json::from_slice::<BenchmarkOracle>(&bytes) {
                    Ok(oracle) => {
                        oracle_bound = validate_oracle(task, &oracle, &mut task_errors);
                        source_evidence_present =
                            validate_oracle_evidence(&source_repository, &oracle, &mut task_errors);
                    }
                    Err(error) => task_errors.push(format!("oracle JSON is invalid: {error}")),
                }
            }
        } else if let Err(error) = oracle_bytes {
            task_errors.push(format!("oracle cannot be read: {error}"));
        }

        let (focused_test_executed, focused_test_passed) =
            if execute_focused_tests && task_errors.is_empty() && source_clean_before {
                let passed = execute_focused_test(&source_repository, task, &mut task_errors);
                (true, Some(passed))
            } else {
                (false, None)
            };
        task_reports.push(CorpusTaskPreflight {
            task_id: task.id.clone(),
            prompt_frozen,
            oracle_bound,
            source_evidence_present,
            focused_test_executed,
            focused_test_passed,
            errors: task_errors,
        });
    }
    let source_clean_after = git_output(&source_repository, &["status", "--short"])
        .is_ok_and(|output| output.trim().is_empty());
    if !source_clean_after {
        errors.push("source repository changed during preflight".to_owned());
    }
    if task_reports.iter().any(|task| !task.errors.is_empty()) {
        errors.push("one or more corpus tasks failed preflight".to_owned());
    }
    if execute_focused_tests
        && task_reports.iter().any(|task| task.focused_test_passed != Some(true))
    {
        errors.push("one or more focused tests did not execute successfully".to_owned());
    }

    let calibration_tasks =
        manifest.tasks.iter().filter(|task| task.split == crate::CorpusSplit::Calibration).count();
    let cost_readiness =
        load_cost_readiness(manifest, &manifest_directory, calibration_tasks, &mut errors);
    CorpusPreflightReport {
        schema: "needle.corpus-preflight/3".to_owned(),
        corpus_digest: corpus_digest(manifest),
        source_repository: source_repository.display().to_string(),
        source_sha: source_sha.map(|value| value.trim().to_owned()),
        source_clean_before,
        source_clean_after,
        manifest_valid,
        arms_fixed,
        campaign_valid: cost_readiness.paid_calibration_budget_estimate_microcredits.is_some(),
        campaign_digest: manifest.campaign_digest.clone(),
        tasks: task_reports,
        cost_readiness,
        provider_run_ready: false,
        errors,
    }
}

fn load_cost_readiness(
    manifest: &FrozenCorpusManifest,
    manifest_directory: &Path,
    calibration_tasks: usize,
    errors: &mut Vec<String>,
) -> CampaignCostReadiness {
    let mut readiness = CampaignCostReadiness {
        next_pilot_observations: 0,
        next_pilot_budget_estimate_microcredits: None,
        full_protocol_calibration_observations: calibration_tasks
            .saturating_mul(manifest.arms.len()),
        powered_holdout_observations: None,
        full_protocol_calibration_budget_estimate_microcredits: None,
        full_protocol_campaign_budget_estimate_microcredits: None,
        paid_calibration_observations: 0,
        offline_cache_validation_observations: 0,
        deferred_diagnostic_observations: 0,
        paid_calibration_budget_estimate_microcredits: None,
        assumptions: Vec::new(),
        next_pilot_blockers: vec![
            "a fresh native App Server transport preflight is required for the multi-task runner"
                .to_owned(),
            "the observed-component budget is not an enforceable provider token ceiling".to_owned(),
            "an explicit user approval is still required before any paid run".to_owned(),
        ],
        deferred_final_gate_blockers: vec![
            "holdout repetitions require calibration variance and the 90% power calculation"
                .to_owned(),
        ],
    };
    let path = manifest_directory.join(&manifest.cost_model_path);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            errors.push(format!("cost model cannot be read: {error}"));
            return readiness;
        }
    };
    let digest = format!("b3:{}", blake3::hash(&bytes).to_hex());
    if digest != manifest.cost_model_digest {
        errors.push(format!(
            "cost model digest mismatch: expected {}, observed {digest}",
            manifest.cost_model_digest
        ));
        return readiness;
    }
    let model: CampaignCostModel = match serde_json::from_slice(&bytes) {
        Ok(model) => model,
        Err(error) => {
            errors.push(format!("cost model JSON is invalid: {error}"));
            return readiness;
        }
    };
    let configured = model.arm_estimates.iter().map(|item| item.arm).collect::<BTreeSet<_>>();
    let required = FinalArm::ALL.into_iter().collect::<BTreeSet<_>>();
    if model.schema != "needle.campaign-cost-model/1"
        || model.unit != "microcredits"
        || model.pricing_snapshot_digest.trim().is_empty()
        || model.arm_estimates.len() != configured.len()
        || configured != required
        || model.arm_estimates.iter().any(|item| {
            item.microcredits_per_observation == 0
                || item.basis.trim().is_empty()
                || item.evidence.is_empty()
                || item.evidence.iter().any(|source| source.trim().is_empty())
        })
    {
        errors.push("cost model is incomplete or internally inconsistent".to_owned());
        return readiness;
    }
    let per_task = model.arm_estimates.iter().try_fold(0_u64, |total, estimate| {
        total.checked_add(estimate.microcredits_per_observation)
    });
    readiness.full_protocol_calibration_budget_estimate_microcredits =
        per_task.and_then(|total| total.checked_mul(calibration_tasks as u64));
    if readiness.full_protocol_calibration_budget_estimate_microcredits.is_none() {
        errors.push("calibration budget estimate overflowed".to_owned());
    }
    readiness.assumptions =
        model.arm_estimates.iter().map(|item| format!("{:?}: {}", item.arm, item.basis)).collect();
    load_minimal_pilot(manifest, manifest_directory, &model, &mut readiness, errors);
    load_multi_task_campaign(manifest, manifest_directory, &model, &mut readiness, errors);
    readiness
}

fn load_multi_task_campaign(
    manifest: &FrozenCorpusManifest,
    manifest_directory: &Path,
    cost_model: &CampaignCostModel,
    readiness: &mut CampaignCostReadiness,
    errors: &mut Vec<String>,
) {
    let (Some(campaign_path), Some(expected_digest)) =
        (&manifest.campaign_path, &manifest.campaign_digest)
    else {
        return;
    };
    let bytes = match fs::read(manifest_directory.join(campaign_path)) {
        Ok(bytes) => bytes,
        Err(error) => {
            errors.push(format!("multi-task campaign cannot be read: {error}"));
            return;
        }
    };
    let digest = format!("b3:{}", blake3::hash(&bytes).to_hex());
    if digest != *expected_digest {
        errors.push(format!(
            "multi-task campaign digest mismatch: expected {expected_digest}, observed {digest}"
        ));
        return;
    }
    let campaign: MultiTaskCampaign = match serde_json::from_slice(&bytes) {
        Ok(campaign) => campaign,
        Err(error) => {
            errors.push(format!("multi-task campaign JSON is invalid: {error}"));
            return;
        }
    };
    let paid = [FinalArm::FrontierDirect, FinalArm::NeedleMiss];
    let offline = [
        FinalArm::ExactHit,
        FinalArm::PartialHit,
        FinalArm::IrrelevantMutation,
        FinalArm::RelevantMutation,
    ];
    let deferred = [FinalArm::NativeSubagent, FinalArm::Escalation];
    let task_ids = campaign.task_ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let calibration_tasks = manifest
        .tasks
        .iter()
        .filter(|task| task.split == crate::CorpusSplit::Calibration)
        .collect::<Vec<_>>();
    let expected_task_ids =
        calibration_tasks.iter().map(|task| task.id.as_str()).collect::<BTreeSet<_>>();
    let routes = calibration_tasks.iter().map(|task| task.route).collect::<BTreeSet<_>>();
    let expected_routes =
        [crate::BenchmarkRoute::LocateImplementation, crate::BenchmarkRoute::TraceStateFlow]
            .into_iter()
            .collect::<BTreeSet<_>>();
    let configured_arms = campaign
        .paid_arms
        .iter()
        .chain(&campaign.offline_cache_arms)
        .chain(&campaign.deferred_diagnostic_arms)
        .copied()
        .collect::<BTreeSet<_>>();
    if campaign.schema != "needle.multi-task-campaign/1"
        || campaign.task_ids.len() != task_ids.len()
        || task_ids != expected_task_ids
        || routes != expected_routes
        || campaign.paid_arms != paid
        || campaign.offline_cache_arms != offline
        || campaign.deferred_diagnostic_arms != deferred
        || configured_arms != FinalArm::ALL.into_iter().collect::<BTreeSet<_>>()
        || campaign.repetitions_per_task != 1
        || campaign.automatic_retries
        || campaign.statistical_claim
        || campaign.bootstrap_resamples < 1_000
        || campaign.one_sided_alpha_basis_points != 500
        || campaign.target_power_basis_points != 9_000
        || campaign.budget_reserve.main_turn_microcredits == 0
        || campaign.budget_reserve.worker_microcredits == 0
        || campaign.budget_reserve.evidence.is_empty()
        || campaign.budget_reserve.evidence.iter().any(|item| item.trim().is_empty())
    {
        errors.push(
            "multi-task campaign must partition the calibration arms into paid economics, offline cache validation and deferred diagnostics with one no-retry repetition"
                .to_owned(),
        );
        return;
    }

    let task_count = campaign.task_ids.len();
    let repetitions = campaign.repetitions_per_task as usize;
    readiness.paid_calibration_observations = task_count * campaign.paid_arms.len() * repetitions;
    readiness.offline_cache_validation_observations =
        task_count * campaign.offline_cache_arms.len() * repetitions;
    readiness.deferred_diagnostic_observations =
        task_count * campaign.deferred_diagnostic_arms.len() * repetitions;
    let paid_per_task = campaign.paid_arms.iter().try_fold(0_u64, |total, arm| {
        let estimate = cost_model
            .arm_estimates
            .iter()
            .find(|estimate| estimate.arm == *arm)?
            .microcredits_per_observation;
        total.checked_add(estimate)
    });
    let main_reserve_per_needle = campaign
        .budget_reserve
        .main_turn_microcredits
        .checked_mul(campaign.budget_reserve.extra_main_turns_per_needle_observation as u64);
    let worker_reserve_per_needle = campaign
        .budget_reserve
        .worker_microcredits
        .checked_mul(campaign.budget_reserve.extra_workers_per_needle_observation as u64);
    readiness.paid_calibration_budget_estimate_microcredits = paid_per_task
        .and_then(|value| value.checked_mul(task_count as u64))
        .and_then(|value| {
            main_reserve_per_needle
                .and_then(|reserve| reserve.checked_mul(task_count as u64))
                .and_then(|reserve| value.checked_add(reserve))
        })
        .and_then(|value| {
            worker_reserve_per_needle
                .and_then(|reserve| reserve.checked_mul(task_count as u64))
                .and_then(|reserve| value.checked_add(reserve))
        });
    if readiness.paid_calibration_budget_estimate_microcredits.is_none() {
        errors.push("multi-task paid calibration budget estimate is unavailable".to_owned());
        return;
    }
    readiness.assumptions.push(format!(
        "multi-task campaign: {} paid economics, {} offline cache-validation and {} deferred diagnostic observations",
        readiness.paid_calibration_observations,
        readiness.offline_cache_validation_observations,
        readiness.deferred_diagnostic_observations
    ));
}

fn load_minimal_pilot(
    manifest: &FrozenCorpusManifest,
    manifest_directory: &Path,
    cost_model: &CampaignCostModel,
    readiness: &mut CampaignCostReadiness,
    errors: &mut Vec<String>,
) {
    let path = manifest_directory.join(&manifest.next_pilot_path);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            errors.push(format!("minimal pilot cannot be read: {error}"));
            return;
        }
    };
    let digest = format!("b3:{}", blake3::hash(&bytes).to_hex());
    if digest != manifest.next_pilot_digest {
        errors.push(format!(
            "minimal pilot digest mismatch: expected {}, observed {digest}",
            manifest.next_pilot_digest
        ));
        return;
    }
    let pilot: MinimalLivePilot = match serde_json::from_slice(&bytes) {
        Ok(pilot) => pilot,
        Err(error) => {
            errors.push(format!("minimal pilot JSON is invalid: {error}"));
            return;
        }
    };
    let task_ids = pilot.task_ids.iter().collect::<BTreeSet<_>>();
    let paid_arms = pilot.paid_arms.iter().copied().collect::<BTreeSet<_>>();
    let expected_arms =
        [FinalArm::NeedleMiss, FinalArm::ExactHit].into_iter().collect::<BTreeSet<_>>();
    let tasks_valid = pilot.task_ids.iter().all(|task_id| {
        manifest
            .tasks
            .iter()
            .any(|task| task.id == *task_id && task.split == crate::CorpusSplit::Calibration)
    });
    if pilot.schema != "needle.minimal-live-pilot/1"
        || pilot.task_ids.len() != 1
        || task_ids.len() != pilot.task_ids.len()
        || !tasks_valid
        || pilot.paid_arms != [FinalArm::NeedleMiss, FinalArm::ExactHit]
        || paid_arms != expected_arms
        || pilot.repetitions_per_task != 1
        || pilot.automatic_retries
        || pilot.statistical_claim
        || pilot.promotion_bootstrap.route != "locate.implementation"
        || pilot.promotion_bootstrap.capability != "implementation_location"
        || pilot.promotion_bootstrap.evidence.is_empty()
        || pilot.promotion_bootstrap.evidence.iter().any(|item| item.trim().is_empty())
        || !pilot.promotion_bootstrap.replace_after_observed_hit
    {
        errors.push(
            "minimal pilot must be one calibration task with NeedleMiss then ExactHit, one repetition, no retry and no statistical claim"
                .to_owned(),
        );
        return;
    }
    let exact_hit_estimate = cost_model
        .arm_estimates
        .iter()
        .find(|estimate| estimate.arm == FinalArm::ExactHit)
        .map(|estimate| estimate.microcredits_per_observation);
    if exact_hit_estimate != Some(pilot.promotion_bootstrap.reuse_cost_microcredits) {
        errors.push(
            "minimal pilot promotion bootstrap must match the content-addressed exact-hit cost estimate"
                .to_owned(),
        );
        return;
    }
    let per_task = pilot.paid_arms.iter().try_fold(0_u64, |total, arm| {
        let estimate = cost_model
            .arm_estimates
            .iter()
            .find(|estimate| estimate.arm == *arm)?
            .microcredits_per_observation;
        total.checked_add(estimate)
    });
    readiness.next_pilot_observations =
        pilot.task_ids.len() * pilot.paid_arms.len() * pilot.repetitions_per_task as usize;
    readiness.next_pilot_budget_estimate_microcredits = per_task.and_then(|total| {
        total
            .checked_mul(pilot.task_ids.len() as u64)?
            .checked_mul(pilot.repetitions_per_task as u64)
    });
    if readiness.next_pilot_budget_estimate_microcredits.is_none() {
        errors.push("minimal pilot budget estimate is unavailable".to_owned());
    }
}

fn validate_oracle(
    task: &crate::CorpusTask,
    oracle: &BenchmarkOracle,
    errors: &mut Vec<String>,
) -> bool {
    let valid = oracle.schema == "needle.benchmark-oracle/1"
        && oracle.task_id == task.id
        && oracle.repository_sha == task.repository_sha
        && oracle.focused_test.identifier == task.test_identifier
        && oracle.focused_test.command == task.focused_command
        && !oracle.evidence.is_empty();
    if !valid {
        errors.push("oracle metadata does not bind exactly to the frozen task".to_owned());
    }
    valid
}

fn validate_oracle_evidence(
    source_repository: &Path,
    oracle: &BenchmarkOracle,
    errors: &mut Vec<String>,
) -> bool {
    let mut valid = true;
    for evidence in &oracle.evidence {
        let path = Path::new(&evidence.path);
        if path.is_absolute()
            || path.components().any(|component| {
                !matches!(component, std::path::Component::Normal(_) | std::path::Component::CurDir)
            })
            || evidence.role.trim().is_empty()
            || evidence.needles.is_empty()
            || evidence.needles.iter().any(|needle| needle.is_empty())
        {
            errors.push(format!("invalid oracle evidence entry `{}`", evidence.path));
            valid = false;
            continue;
        }
        let contents = match fs::read_to_string(source_repository.join(path)) {
            Ok(contents) => contents,
            Err(error) => {
                errors.push(format!("cannot read oracle evidence `{}`: {error}", evidence.path));
                valid = false;
                continue;
            }
        };
        for needle in &evidence.needles {
            if !contents.contains(needle) {
                errors.push(format!(
                    "oracle evidence `{}` is missing required text `{needle}`",
                    evidence.path
                ));
                valid = false;
            }
        }
    }
    valid
}

fn execute_focused_test(
    source_repository: &Path,
    task: &crate::CorpusTask,
    errors: &mut Vec<String>,
) -> bool {
    let Some((program, arguments)) = task.focused_command.split_first() else {
        errors.push("focused command is empty".to_owned());
        return false;
    };
    let output = Command::new(program)
        .args(arguments)
        .current_dir(source_repository)
        .env("CARGO_NET_OFFLINE", "true")
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            errors.push(format!("focused test failed to start: {error}"));
            return false;
        }
    };
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let identifier_line = format!("test {} ... ok", task.test_identifier);
    let passed = output.status.success()
        && combined.contains("running 1 test")
        && combined.contains(&identifier_line)
        && combined.contains("1 passed;");
    if !passed {
        errors.push(format!(
            "focused test evidence is invalid (status={}, expected `{identifier_line}`)",
            output.status
        ));
    }
    passed
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn git_output(repository: &Path, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .output()
        .map_err(|error| format!("git {} failed to start: {error}", arguments.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("git {} returned non-UTF-8 output: {error}", arguments.join(" ")))
}

fn normalize_git_url(value: &str) -> String {
    value.trim().trim_end_matches('/').trim_end_matches(".git").to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BenchmarkRoute, CorpusSplit, CorpusTask, FrozenCorpusManifest};

    #[test]
    fn current_campaign_limits_paid_observations_to_frontier_and_needle_miss() {
        let directory =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/corpus/router-cache");
        let manifest: FrozenCorpusManifest = serde_json::from_slice(
            &fs::read(directory.join("manifest.json")).expect("current manifest"),
        )
        .expect("valid current manifest");
        let mut errors = Vec::new();
        let readiness = load_cost_readiness(&manifest, &directory, 2, &mut errors);

        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(readiness.paid_calibration_observations, 4);
        assert_eq!(readiness.offline_cache_validation_observations, 8);
        assert_eq!(readiness.deferred_diagnostic_observations, 4);
        assert_eq!(readiness.paid_calibration_budget_estimate_microcredits, Some(43_476_640));
    }

    #[test]
    fn preflight_fails_closed_when_source_or_oracle_is_missing() {
        let root = std::env::temp_dir().join(format!(
            "needle-corpus-preflight-{}-{}",
            std::process::id(),
            1
        ));
        let manifest = FrozenCorpusManifest {
            schema: "needle.frozen-corpus/2".to_owned(),
            frozen_unix_ms: 1,
            arms: FinalArm::ALL.to_vec(),
            cost_model_path: "cost-model.json".to_owned(),
            cost_model_digest:
                "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            next_pilot_path: "minimal-live-pilot.json".to_owned(),
            next_pilot_digest:
                "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            campaign_path: None,
            campaign_digest: None,
            tasks: vec![CorpusTask {
                id: "missing".to_owned(),
                route: BenchmarkRoute::LocateImplementation,
                split: CorpusSplit::Calibration,
                repository_url: "https://github.com/example/repository.git".to_owned(),
                repository_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                prompt: "Locate the implementation and identify one focused behavioral test."
                    .to_owned(),
                oracle_path: "oracles/missing.json".to_owned(),
                oracle_digest:
                    "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
                test_identifier: "misc::missing".to_owned(),
                focused_command: [
                    "cargo",
                    "test",
                    "--offline",
                    "--test",
                    "integration",
                    "misc::missing",
                    "--",
                    "--exact",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            }],
        };
        let report = preflight_frozen_corpus(&manifest, &root, &root, false);
        assert!(!report.provider_run_ready);
        assert!(!report.errors.is_empty());
        assert!(
            report.cost_readiness.full_protocol_calibration_budget_estimate_microcredits.is_none()
        );
    }
}
