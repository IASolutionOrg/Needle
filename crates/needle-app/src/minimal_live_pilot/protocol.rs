use super::super::{AppError, BENCHMARK_REPOSITORY_SHA, canonical_child_path};
use needle_bench::{
    BenchmarkOracle, BenchmarkRoute, CampaignCostModel, CorpusSplit, CorpusTask, FinalArm,
    FrozenCorpusManifest, MinimalLivePilot, MultiTaskCampaign, PricingSnapshot, QualityOracleSpec,
    validate_frozen_manifest,
};
use needle_core::{Digest, TestPlan};
use needle_platform_codex::PILOT_MAIN_REPOSITORY_INSPECTION_INSTRUCTIONS;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const DEFAULT_MANIFEST: &str = "benchmarks/corpus/router-cache/manifest.json";
pub(crate) const DEFAULT_LEGACY_OFFLINE_MANIFEST: &str =
    "benchmarks/corpus/router-cache/legacy-offline-manifest.json";
pub(crate) const DEFAULT_PRICING: &str = "fixtures/openai-codex-pricing-2026-07-27.json";
const PILOT_TASK_ID: &str = "ripgrep-glob-case-insensitive-locate-calibration";
pub(crate) const TRACE_REUSE_TASK_ID: &str = "ripgrep-crlf-trace-calibration";
const TRACE_REUSE_EVIDENCE: [&str; 2] = [
    "benchmarks/results/offline/multi-task-quality-replay.md",
    "benchmarks/results/offline/end-to-end-proof-replay.md",
];
pub(crate) const MULTI_NEED_WORKER_RESERVE_MICROCREDITS: u64 = 2_034_130;
pub(crate) const MULTI_NEED_MAIN_TURN_RESERVE_MICROCREDITS: u64 = 2_633_875;
pub(crate) const MULTI_NEED_EXTRA_WORKER_RESERVES: u64 = 0;
pub(crate) const MULTI_NEED_EXTRA_MAIN_TURN_RESERVES: u64 = 2;

pub(crate) fn pilot_main_instructions(base: &str) -> String {
    let base = base.trim_end();
    let mut instructions =
        String::with_capacity(base.len() + PILOT_MAIN_REPOSITORY_INSPECTION_INSTRUCTIONS.len() + 2);
    instructions.push_str(base);
    instructions.push_str("\n\n");
    instructions.push_str(PILOT_MAIN_REPOSITORY_INSPECTION_INSTRUCTIONS);
    instructions
}

#[derive(Clone, Debug)]
pub(crate) struct Protocol {
    pub(crate) manifest: FrozenCorpusManifest,
    pub(crate) pilot: MinimalLivePilot,
    pub(crate) cost_model: CampaignCostModel,
    oracle: BenchmarkOracle,
    campaign: MultiTaskCampaign,
    campaign_oracles: BTreeMap<String, BenchmarkOracle>,
    task_index: usize,
    pub(crate) estimated_budget_microcredits: u64,
    pub(crate) bootstrap_evidence_digest: Digest,
    trace_bootstrap_evidence_digest: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct EconomicStageBudget {
    pub(crate) main_only_microcredits: u64,
    pub(crate) needle_miss_microcredits: u64,
    pub(crate) needle_hit_microcredits: u64,
    pub(crate) base_microcredits: u64,
    pub(crate) worker_reserve_microcredits: u64,
    pub(crate) main_turn_reserve_microcredits: u64,
    pub(crate) total_microcredits: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct MultiTaskStageBudget {
    pub(crate) task_count: usize,
    pub(crate) main_only_microcredits_per_task: u64,
    pub(crate) needle_miss_microcredits_per_task: u64,
    pub(crate) base_microcredits: u64,
    pub(crate) worker_reserve_microcredits: u64,
    pub(crate) main_turn_reserve_microcredits: u64,
    pub(crate) total_microcredits: u64,
}

impl Protocol {
    pub(crate) fn task(&self) -> &needle_bench::CorpusTask {
        &self.manifest.tasks[self.task_index]
    }

    pub(crate) fn oracle(&self) -> &BenchmarkOracle {
        &self.oracle
    }

    pub(crate) fn campaign(&self) -> &MultiTaskCampaign {
        &self.campaign
    }

    pub(crate) fn campaign_task(
        &self,
        task_id: &str,
    ) -> Result<(&CorpusTask, &BenchmarkOracle), AppError> {
        let task =
            self.manifest.tasks.iter().find(|task| task.id == task_id).ok_or_else(|| {
                AppError::Experiment(format!("campaign task `{task_id}` is missing"))
            })?;
        let oracle = self.campaign_oracles.get(task_id).ok_or_else(|| {
            AppError::Experiment(format!("campaign oracle `{task_id}` is missing"))
        })?;
        Ok((task, oracle))
    }

    pub(crate) fn select_campaign_task(&mut self, task_id: &str) -> Result<(), AppError> {
        let task_index =
            self.manifest.tasks.iter().position(|task| task.id == task_id).ok_or_else(|| {
                AppError::Experiment(format!("campaign task `{task_id}` is missing"))
            })?;
        let oracle = self.campaign_oracles.get(task_id).cloned().ok_or_else(|| {
            AppError::Experiment(format!("campaign oracle `{task_id}` is missing"))
        })?;
        self.task_index = task_index;
        self.oracle = oracle;
        Ok(())
    }

    pub(crate) fn promotion_evidence_digest(&self) -> Digest {
        match self.task().route {
            BenchmarkRoute::LocateImplementation => self.bootstrap_evidence_digest,
            BenchmarkRoute::TraceStateFlow => self.trace_bootstrap_evidence_digest,
        }
    }

    pub(crate) fn economic_stage_budget(&self) -> Result<EconomicStageBudget, AppError> {
        let main_only_microcredits = arm_estimate(&self.cost_model, FinalArm::FrontierDirect)?;
        let needle_miss_microcredits = arm_estimate(&self.cost_model, FinalArm::NeedleMiss)?;
        let needle_hit_microcredits = arm_estimate(&self.cost_model, FinalArm::ExactHit)?;
        let base_microcredits = main_only_microcredits
            .checked_add(needle_miss_microcredits)
            .and_then(|total| total.checked_add(needle_hit_microcredits))
            .ok_or_else(|| {
                AppError::Experiment("economic stage base budget overflowed".to_owned())
            })?;
        let worker_reserve_microcredits = MULTI_NEED_WORKER_RESERVE_MICROCREDITS
            .checked_mul(MULTI_NEED_EXTRA_WORKER_RESERVES)
            .ok_or_else(|| {
                AppError::Experiment("economic stage worker reserve overflowed".to_owned())
            })?;
        let main_turn_reserve_microcredits = MULTI_NEED_MAIN_TURN_RESERVE_MICROCREDITS
            .checked_mul(MULTI_NEED_EXTRA_MAIN_TURN_RESERVES)
            .ok_or_else(|| {
                AppError::Experiment("economic stage main-turn reserve overflowed".to_owned())
            })?;
        let total_microcredits = base_microcredits
            .checked_add(worker_reserve_microcredits)
            .and_then(|total| total.checked_add(main_turn_reserve_microcredits))
            .ok_or_else(|| AppError::Experiment("economic stage budget overflowed".to_owned()))?;
        Ok(EconomicStageBudget {
            main_only_microcredits,
            needle_miss_microcredits,
            needle_hit_microcredits,
            base_microcredits,
            worker_reserve_microcredits,
            main_turn_reserve_microcredits,
            total_microcredits,
        })
    }

    pub(crate) fn multi_task_stage_budget(&self) -> Result<MultiTaskStageBudget, AppError> {
        let task_count = self.campaign.task_ids.len();
        let main_only_microcredits_per_task =
            arm_estimate(&self.cost_model, FinalArm::FrontierDirect)?;
        let needle_miss_microcredits_per_task =
            arm_estimate(&self.cost_model, FinalArm::NeedleMiss)?;
        let base_per_task = main_only_microcredits_per_task
            .checked_add(needle_miss_microcredits_per_task)
            .ok_or_else(|| AppError::Experiment("campaign task budget overflowed".to_owned()))?;
        let base_microcredits = base_per_task
            .checked_mul(task_count as u64)
            .ok_or_else(|| AppError::Experiment("campaign base budget overflowed".to_owned()))?;
        let worker_reserve_microcredits = self
            .campaign
            .budget_reserve
            .worker_microcredits
            .checked_mul(u64::from(
                self.campaign.budget_reserve.extra_workers_per_needle_observation,
            ))
            .and_then(|reserve| reserve.checked_mul(task_count as u64))
            .ok_or_else(|| AppError::Experiment("campaign worker reserve overflowed".to_owned()))?;
        let main_turn_reserve_microcredits = self
            .campaign
            .budget_reserve
            .main_turn_microcredits
            .checked_mul(u64::from(
                self.campaign.budget_reserve.extra_main_turns_per_needle_observation,
            ))
            .and_then(|reserve| reserve.checked_mul(task_count as u64))
            .ok_or_else(|| {
                AppError::Experiment("campaign main-turn reserve overflowed".to_owned())
            })?;
        let total_microcredits = base_microcredits
            .checked_add(worker_reserve_microcredits)
            .and_then(|total| total.checked_add(main_turn_reserve_microcredits))
            .ok_or_else(|| AppError::Experiment("campaign total budget overflowed".to_owned()))?;
        Ok(MultiTaskStageBudget {
            task_count,
            main_only_microcredits_per_task,
            needle_miss_microcredits_per_task,
            base_microcredits,
            worker_reserve_microcredits,
            main_turn_reserve_microcredits,
            total_microcredits,
        })
    }
}

fn arm_estimate(model: &CampaignCostModel, arm: FinalArm) -> Result<u64, AppError> {
    model
        .arm_estimates
        .iter()
        .find(|estimate| estimate.arm == arm)
        .map(|estimate| estimate.microcredits_per_observation)
        .ok_or_else(|| AppError::Experiment(format!("cost estimate is missing {arm:?}")))
}

/// Provider/live entry point.  Neither the public v4 answer-free manifest nor
/// a caller-supplied legacy manifest is executable by this runner: v4 still
/// lacks the isolated evaluator boundary and v2/v3 are offline-only.
pub(crate) fn load_protocol(manifest_path: &Path) -> Result<Protocol, AppError> {
    let manifest_path = canonical_child_path(manifest_path)?;
    let manifest: FrozenCorpusManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    if manifest.schema != "needle.frozen-corpus/4" {
        return Err(AppError::Experiment(
            "legacy frozen corpus manifests are restricted to the explicit offline replay loader"
                .to_owned(),
        ));
    }
    Err(AppError::Experiment(
        "frozen corpus v4 requires an evaluator-owned production sealed bundle; provider execution is fail-closed in this runner"
            .to_owned(),
    ))
}

/// Explicit legacy offline loader used only by deterministic replay/simulator
/// paths.  Keeping this separate from `load_protocol` prevents a caller-
/// supplied v2/v3 manifest from reaching a provider/live command.
pub(crate) fn load_legacy_offline_protocol(manifest_path: &Path) -> Result<Protocol, AppError> {
    let manifest_path = canonical_child_path(manifest_path)?;
    let directory = manifest_path
        .parent()
        .ok_or_else(|| AppError::Experiment("frozen corpus has no parent directory".to_owned()))?;
    let manifest: FrozenCorpusManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    if !matches!(manifest.schema.as_str(), "needle.frozen-corpus/2" | "needle.frozen-corpus/3") {
        return Err(AppError::Experiment(
            "legacy offline loader accepts only frozen corpus v2/v3 manifests".to_owned(),
        ));
    }
    let errors = validate_frozen_manifest(&manifest);
    if !errors.is_empty() {
        return Err(AppError::Experiment(format!(
            "frozen corpus is invalid: {}",
            errors.join(", ")
        )));
    }
    let pilot_path = directory.join(&manifest.next_pilot_path);
    verify_digest(&pilot_path, &manifest.next_pilot_digest, "minimal pilot")?;
    let pilot: MinimalLivePilot = serde_json::from_slice(&fs::read(&pilot_path)?)?;
    if pilot.schema != "needle.minimal-live-pilot/1"
        || pilot.task_ids != [PILOT_TASK_ID]
        || pilot.paid_arms != [FinalArm::NeedleMiss, FinalArm::ExactHit]
        || pilot.repetitions_per_task != 1
        || pilot.automatic_retries
        || pilot.statistical_claim
        || pilot.promotion_bootstrap.route != "locate.implementation"
        || pilot.promotion_bootstrap.capability != "implementation_location"
        || !pilot.promotion_bootstrap.replace_after_observed_hit
    {
        return Err(AppError::Experiment(
            "minimal live pilot protocol differs from the frozen two-arm stage".to_owned(),
        ));
    }
    let cost_path = directory.join(&manifest.cost_model_path);
    verify_digest(&cost_path, &manifest.cost_model_digest, "cost model")?;
    let cost_model: CampaignCostModel = serde_json::from_slice(&fs::read(&cost_path)?)?;
    if cost_model.schema != "needle.campaign-cost-model/1" || cost_model.unit != "microcredits" {
        return Err(AppError::Experiment("campaign cost model is incompatible".to_owned()));
    }
    let campaign = load_campaign(directory, &manifest)?;
    let campaign_oracles = campaign
        .task_ids
        .iter()
        .map(|task_id| {
            let task = manifest.tasks.iter().find(|task| task.id == *task_id).ok_or_else(|| {
                AppError::Experiment(format!("campaign task `{task_id}` is missing"))
            })?;
            Ok((task_id.clone(), load_oracle(directory, task)?))
        })
        .collect::<Result<BTreeMap<_, _>, AppError>>()?;
    let frozen_two_arm_estimate = [FinalArm::NeedleMiss, FinalArm::ExactHit]
        .into_iter()
        .map(|arm| {
            cost_model
                .arm_estimates
                .iter()
                .find(|estimate| estimate.arm == arm)
                .map(|estimate| estimate.microcredits_per_observation)
                .ok_or_else(|| AppError::Experiment(format!("cost estimate is missing {arm:?}")))
        })
        .try_fold(0_u64, |total, cost| {
            total
                .checked_add(cost?)
                .ok_or_else(|| AppError::Experiment("pilot budget overflowed".to_owned()))
        })?;
    let estimated_budget_microcredits = frozen_two_arm_estimate
        .checked_add(
            MULTI_NEED_WORKER_RESERVE_MICROCREDITS
                .checked_mul(MULTI_NEED_EXTRA_WORKER_RESERVES)
                .ok_or_else(|| {
                    AppError::Experiment("pilot worker reserve overflowed".to_owned())
                })?,
        )
        .and_then(|total| {
            MULTI_NEED_MAIN_TURN_RESERVE_MICROCREDITS
                .checked_mul(MULTI_NEED_EXTRA_MAIN_TURN_RESERVES)
                .and_then(|reserve| total.checked_add(reserve))
        })
        .ok_or_else(|| AppError::Experiment("pilot multi-need budget overflowed".to_owned()))?;
    let task_index = manifest
        .tasks
        .iter()
        .position(|task| task.id == PILOT_TASK_ID)
        .ok_or_else(|| AppError::Experiment("minimal pilot task is missing".to_owned()))?;
    let task = &manifest.tasks[task_index];
    if task.route != BenchmarkRoute::LocateImplementation
        || task.repository_sha != BENCHMARK_REPOSITORY_SHA
    {
        return Err(AppError::Experiment(
            "minimal pilot task route or repository SHA changed".to_owned(),
        ));
    }
    let oracle = campaign_oracles
        .get(&task.id)
        .cloned()
        .ok_or_else(|| AppError::Experiment("minimal pilot oracle is missing".to_owned()))?;
    let bootstrap_evidence_digest =
        bootstrap_digest(&manifest_path, &pilot.promotion_bootstrap.evidence)?;
    let trace_bootstrap_evidence_digest =
        bootstrap_digest(&manifest_path, &TRACE_REUSE_EVIDENCE.map(str::to_owned))?;
    Ok(Protocol {
        manifest,
        pilot,
        cost_model,
        oracle,
        campaign,
        campaign_oracles,
        task_index,
        estimated_budget_microcredits,
        bootstrap_evidence_digest,
        trace_bootstrap_evidence_digest,
    })
}

fn load_campaign(
    directory: &Path,
    manifest: &FrozenCorpusManifest,
) -> Result<MultiTaskCampaign, AppError> {
    let path = manifest
        .campaign_path
        .as_ref()
        .ok_or_else(|| AppError::Experiment("multi-task campaign path is missing".to_owned()))?;
    let digest = manifest
        .campaign_digest
        .as_ref()
        .ok_or_else(|| AppError::Experiment("multi-task campaign digest is missing".to_owned()))?;
    let campaign_path = directory.join(path);
    verify_digest(&campaign_path, digest, "multi-task campaign")?;
    let campaign: MultiTaskCampaign = serde_json::from_slice(&fs::read(campaign_path)?)?;
    let calibration_task_ids = manifest
        .tasks
        .iter()
        .filter(|task| task.split == CorpusSplit::Calibration)
        .map(|task| task.id.clone())
        .collect::<Vec<_>>();
    if campaign.schema != "needle.multi-task-campaign/1"
        || campaign.task_ids != calibration_task_ids
        || campaign.paid_arms != [FinalArm::FrontierDirect, FinalArm::NeedleMiss]
        || campaign.offline_cache_arms
            != [
                FinalArm::ExactHit,
                FinalArm::PartialHit,
                FinalArm::IrrelevantMutation,
                FinalArm::RelevantMutation,
            ]
        || campaign.deferred_diagnostic_arms != [FinalArm::NativeSubagent, FinalArm::Escalation]
        || campaign.repetitions_per_task != 1
        || campaign.automatic_retries
        || campaign.statistical_claim
        || campaign.bootstrap_resamples < 1_000
        || campaign.one_sided_alpha_basis_points != 500
        || campaign.target_power_basis_points != 9_000
        || campaign.budget_reserve.main_turn_microcredits == 0
        || campaign.budget_reserve.worker_microcredits == 0
        || campaign.budget_reserve.evidence.is_empty()
    {
        return Err(AppError::Experiment(
            "multi-task campaign differs from the frozen calibration protocol".to_owned(),
        ));
    }
    let routes = campaign
        .task_ids
        .iter()
        .filter_map(|task_id| manifest.tasks.iter().find(|task| task.id == *task_id))
        .map(|task| task.route)
        .collect::<Vec<_>>();
    if routes != [BenchmarkRoute::LocateImplementation, BenchmarkRoute::TraceStateFlow] {
        return Err(AppError::Experiment(
            "multi-task campaign must contain locate then trace calibration".to_owned(),
        ));
    }
    Ok(campaign)
}

fn load_oracle(directory: &Path, task: &CorpusTask) -> Result<BenchmarkOracle, AppError> {
    let oracle_path = directory.join(&task.oracle_path);
    verify_digest(&oracle_path, &task.oracle_digest, "benchmark oracle")?;
    let oracle: BenchmarkOracle = serde_json::from_slice(&fs::read(&oracle_path)?)?;
    if oracle.schema != "needle.benchmark-oracle/1"
        || oracle.task_id != task.id
        || oracle.repository_sha != task.repository_sha
        || oracle.focused_test.identifier != task.test_identifier
        || oracle.focused_test.command != task.focused_command
        || oracle.evidence.is_empty()
    {
        return Err(AppError::Experiment(format!(
            "benchmark oracle differs from frozen task `{}`",
            task.id
        )));
    }
    Ok(oracle)
}

pub(crate) fn load_pricing(
    pricing_path: &Path,
    cost_model: &CampaignCostModel,
) -> Result<PricingSnapshot, AppError> {
    let pricing: PricingSnapshot = serde_json::from_slice(&fs::read(pricing_path)?)?;
    pricing.validate().map_err(|error| AppError::Experiment(error.to_string()))?;
    if pricing.digest()?.to_string() != cost_model.pricing_snapshot_digest {
        return Err(AppError::Experiment(
            "pricing snapshot differs from the frozen cost model".to_owned(),
        ));
    }
    Ok(pricing)
}

pub(crate) fn validate_source(repository: &Path, expected_sha: &str) -> Result<(), AppError> {
    let revision = git_output(repository, &["rev-parse", "HEAD"])?;
    if revision.trim() != expected_sha {
        return Err(AppError::Experiment(format!(
            "source repository SHA differs: expected {expected_sha}, got {}",
            revision.trim()
        )));
    }
    if !git_output(repository, &["status", "--short"])?.trim().is_empty() {
        return Err(AppError::Experiment(
            "minimal live pilot source repository must be clean".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn quality_spec(protocol: &Protocol) -> Result<QualityOracleSpec, AppError> {
    quality_spec_for_task(protocol.task(), protocol.oracle())
}

pub(crate) fn quality_spec_for_task(
    task: &CorpusTask,
    oracle: &BenchmarkOracle,
) -> Result<QualityOracleSpec, AppError> {
    let required_files =
        oracle.evidence.iter().map(|evidence| evidence.path.as_str()).collect::<Vec<_>>();
    let (files, symbols, claims, forbidden, accepted_tests) = match task.route {
        BenchmarkRoute::LocateImplementation
            if required_files
                == [
                    "crates/core/flags/defs.rs",
                    "crates/core/flags/hiargs.rs",
                    "tests/misc.rs",
                ] =>
        {
            (
                vec!["crates/core/flags/hiargs.rs".to_owned()],
                vec!["globs".to_owned()],
                Vec::new(),
                vec![
                    "cargo test --test integration misc::glob_case_insensitive -- --exact"
                        .to_owned(),
                ],
                Vec::new(),
            )
        }
        BenchmarkRoute::TraceStateFlow
            if required_files
                == [
                    "crates/core/flags/defs.rs",
                    "crates/core/flags/hiargs.rs",
                    "crates/core/flags/hiargs.rs",
                    "tests/feature.rs",
                ] =>
        {
            (
                vec![
                    "crates/core/flags/defs.rs".to_owned(),
                    "crates/core/flags/hiargs.rs".to_owned(),
                ],
                Vec::new(),
                vec!["crlf(true)".to_owned(), "LineTerminator::crlf".to_owned()],
                Vec::new(),
                vec!["line_terminator_crlf".to_owned()],
            )
        }
        _ => {
            return Err(AppError::Experiment(format!(
                "quality oracle evidence paths changed for `{}`",
                task.id
            )));
        }
    };
    Ok(QualityOracleSpec {
        required_files: files,
        required_symbols: symbols,
        required_claims: claims,
        forbidden_claims: forbidden,
        focused_test_command: task.test_identifier.clone(),
        accepted_focused_test_identifiers: accepted_tests,
        focused_test_required: true,
    })
}

pub(crate) fn coverage_hit_quality_spec(
    protocol: &Protocol,
) -> Result<QualityOracleSpec, AppError> {
    let mut spec = quality_spec(protocol)?;
    spec.required_symbols.clear();
    spec.focused_test_required = false;
    Ok(spec)
}

pub(crate) fn test_plan(task: &needle_bench::CorpusTask) -> TestPlan {
    TestPlan {
        runner: "cargo".to_owned(),
        argv: task.focused_command.clone(),
        cwd_relative: ".".to_owned(),
        test_identifier: task.test_identifier.clone(),
        requires_approval: true,
        execution_evidence_id: None,
    }
}

fn bootstrap_digest(manifest_path: &Path, evidence: &[String]) -> Result<Digest, AppError> {
    let repository_root = manifest_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| AppError::Experiment("cannot resolve repository root".to_owned()))?;
    let mut hasher = needle_core::CanonicalHasher::new(b"minimal-pilot-promotion-evidence");
    for relative in evidence {
        let path = repository_root.join(relative);
        hasher.field_str(&relative.replace('\\', "/"));
        hasher.field_digest(Digest::blake3(fs::read(path)?));
    }
    Ok(hasher.finish())
}

fn verify_digest(path: &Path, expected: &str, label: &str) -> Result<(), AppError> {
    let actual = Digest::blake3(fs::read(path)?).to_string();
    if actual != expected {
        return Err(AppError::Experiment(format!(
            "{label} digest differs: expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

fn git_output(repository: &Path, arguments: &[&str]) -> Result<String, AppError> {
    let output = Command::new("git").arg("-C").arg(repository).args(arguments).output()?;
    if !output.status.success() {
        return Err(AppError::Experiment(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| AppError::Experiment(format!("git output is not UTF-8: {error}")))
}

pub(crate) fn workspace_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(relative)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_protocol_is_exactly_one_miss_then_one_hit() {
        let protocol =
            load_legacy_offline_protocol(&workspace_path(DEFAULT_LEGACY_OFFLINE_MANIFEST)).unwrap();
        assert_eq!(protocol.pilot.paid_arms, [FinalArm::NeedleMiss, FinalArm::ExactHit]);
        assert_eq!(protocol.estimated_budget_microcredits, 10_936_895);
        assert_eq!(protocol.task().id, PILOT_TASK_ID);
    }

    #[test]
    fn current_multi_task_budget_contains_only_the_two_paid_arms_and_reserves() {
        let protocol =
            load_legacy_offline_protocol(&workspace_path(DEFAULT_LEGACY_OFFLINE_MANIFEST)).unwrap();
        let budget = protocol.multi_task_stage_budget().unwrap();
        assert_eq!(protocol.campaign.task_ids.len(), 2);
        assert_eq!(budget.task_count, 2);
        assert_eq!(budget.base_microcredits, 32_941_140);
        assert_eq!(budget.worker_reserve_microcredits, 0);
        assert_eq!(budget.main_turn_reserve_microcredits, 10_535_500);
        assert_eq!(budget.total_microcredits, 43_476_640);
    }

    #[test]
    fn natural_r43_answer_satisfies_the_public_quality_gate() {
        let protocol =
            load_legacy_offline_protocol(&workspace_path(DEFAULT_LEGACY_OFFLINE_MANIFEST)).unwrap();
        let spec = quality_spec(&protocol).unwrap();
        let response = "The primary implementation is `globs` in \
            `crates/core/flags/hiargs.rs` (lines 1209-1219). The focused test is \
            `misc::glob_always_case_insensitive`.";
        let result = needle_bench::QualityOracleResult::evaluate(&spec, response, Some(true));
        assert!(result.passed, "{:?}", result.failures);
    }

    #[test]
    fn coverage_hit_oracle_does_not_require_an_unrequested_test() {
        let protocol =
            load_legacy_offline_protocol(&workspace_path(DEFAULT_LEGACY_OFFLINE_MANIFEST)).unwrap();
        let spec = coverage_hit_quality_spec(&protocol).unwrap();
        let response = "The primary implementation is in `crates/core/flags/hiargs.rs`.";
        let result = needle_bench::QualityOracleResult::evaluate(&spec, response, None);
        assert!(result.passed, "{:?}", result.failures);
        assert!(!spec.focused_test_required);
    }

    #[test]
    fn trace_campaign_oracle_accepts_declared_semantic_evidence_and_test_alternative() {
        let protocol =
            load_legacy_offline_protocol(&workspace_path(DEFAULT_LEGACY_OFFLINE_MANIFEST)).unwrap();
        let (task, oracle) = protocol.campaign_task("ripgrep-crlf-trace-calibration").unwrap();
        let spec = quality_spec_for_task(task, oracle).unwrap();
        let response = "`--crlf` is parsed in crates/core/flags/defs.rs. In \
            crates/core/flags/hiargs.rs the matcher uses crlf(true) and the searcher uses \
            LineTerminator::crlf(). The focused test is line_terminator_crlf.";
        let result = needle_bench::QualityOracleResult::evaluate(&spec, response, None);
        assert!(result.passed, "{:?}", result.failures);
    }

    #[test]
    fn trace_campaign_oracle_rejects_an_incomplete_runtime_explanation() {
        let protocol =
            load_legacy_offline_protocol(&workspace_path(DEFAULT_LEGACY_OFFLINE_MANIFEST)).unwrap();
        let (task, oracle) = protocol.campaign_task("ripgrep-crlf-trace-calibration").unwrap();
        let spec = quality_spec_for_task(task, oracle).unwrap();
        let response = "`--crlf` appears in crates/core/flags/defs.rs and \
            crates/core/flags/hiargs.rs. Run line_terminator_crlf.";
        let result = needle_bench::QualityOracleResult::evaluate(&spec, response, None);
        assert!(!result.passed);
        assert_eq!(result.failures, vec!["required_claims"]);
    }

    #[test]
    fn provider_loader_rejects_the_public_v4_manifest() {
        let error = load_protocol(&workspace_path(DEFAULT_MANIFEST)).unwrap_err();
        assert!(error.to_string().contains("evaluator-owned production sealed bundle"));
    }
}
