use crate::{
    BenchmarkRoute, CorpusMaterialClass, CorpusSplit, FinalArm, FrozenCorpusManifest,
    MAX_POWER_PLAN_PAIRS, MAX_SCHEDULE_ENTRIES, MultiTaskCampaign, POWER_PLAN_ESTIMATOR_REVISION,
    POWER_PLAN_PAIR_KEY, POWER_PLAN_SCHEMA, PowerPlan, PowerRoutePlan, campaign_commitment,
    corpus_digest, digest_json,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const POWER_PLANNING_REPORT_SCHEMA: &str = "needle.power-planning-report/1";
pub const POWER_PLAN_ALPHA_BASIS_POINTS: u16 = 500;
pub const POWER_PLAN_TARGET_POWER_BASIS_POINTS: u16 = 9_000;
pub const MAX_CALIBRATION_INPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_FAILURE_DETAIL_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationObservation {
    pub corpus_digest: String,
    pub campaign_commitment: String,
    pub task_id: String,
    pub route: BenchmarkRoute,
    pub split: CorpusSplit,
    pub arm: FinalArm,
    pub repetition: u32,
    pub pair_seed: u64,
    pub quality_passed: bool,
    pub infrastructure_failure: Option<String>,
    pub total_cost_microcredits: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerPlanningReport {
    pub schema: String,
    pub manifest_digest: String,
    pub campaign_commitment: String,
    pub calibration_input_digest: String,
    pub estimator_revision: String,
    pub alpha_basis_points: u16,
    pub target_power_basis_points: u16,
    pub plan: Option<PowerPlan>,
    pub failures: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PowerEstimate {
    pub observed_log_ratio_mean: f64,
    pub observed_log_ratio_stddev: f64,
    pub required_pairs_per_route: usize,
    pub power: f64,
    pub one_sided_alpha: f64,
}

pub fn estimate_required_pairs(calibration_ratios: &[f64]) -> Option<PowerEstimate> {
    if calibration_ratios.len() < 2
        || calibration_ratios.iter().any(|ratio| !ratio.is_finite() || *ratio <= 0.0)
    {
        return None;
    }
    let logs = calibration_ratios.iter().map(|ratio| ratio.ln()).collect::<Vec<_>>();
    let mean = average(&logs);
    if mean >= 0.0 {
        return None;
    }
    let variance =
        logs.iter().map(|value| (value - mean).powi(2)).sum::<f64>() / (logs.len() - 1) as f64;
    let stddev = variance.sqrt();
    if !stddev.is_finite() || stddev == 0.0 {
        return None;
    }
    let required = required_pairs_for_moments(mean, stddev)?;
    Some(PowerEstimate {
        observed_log_ratio_mean: mean,
        observed_log_ratio_stddev: stddev,
        required_pairs_per_route: required,
        power: 0.90,
        one_sided_alpha: 0.05,
    })
}

pub fn required_pairs_for_moments(mean: f64, stddev: f64) -> Option<usize> {
    if !mean.is_finite() || mean >= 0.0 || !stddev.is_finite() || stddev <= 0.0 {
        return None;
    }
    let required = (((1.644_853_626_951_472_2 + 1.281_551_565_544_600_4) * stddev / mean.abs())
        .powi(2))
    .ceil()
    .max(3.0);
    (required.is_finite() && required <= usize::MAX as f64).then_some(required as usize)
}

pub(crate) fn validate_power_campaign(campaign: &MultiTaskCampaign) -> Vec<String> {
    if campaign.schema != "needle.multi-task-campaign/2"
        || campaign.paid_arms != [FinalArm::FrontierDirect, FinalArm::NeedleMiss]
        || campaign.offline_cache_arms
            != [
                FinalArm::ExactHit,
                FinalArm::PartialHit,
                FinalArm::IrrelevantMutation,
                FinalArm::RelevantMutation,
            ]
        || campaign.deferred_diagnostic_arms != [FinalArm::NativeSubagent, FinalArm::Escalation]
        || !campaign.task_ids.is_empty()
        || campaign.repetitions_per_task != 0
        || campaign.automatic_retries
        || campaign.statistical_claim
        || campaign.bootstrap_resamples < 1_000
        || campaign.one_sided_alpha_basis_points != POWER_PLAN_ALPHA_BASIS_POINTS
        || campaign.target_power_basis_points != POWER_PLAN_TARGET_POWER_BASIS_POINTS
    {
        vec!["power planning campaign contract is invalid".to_owned()]
    } else {
        Vec::new()
    }
}

pub fn plan_power(
    manifest: &FrozenCorpusManifest,
    campaign: &MultiTaskCampaign,
    observations: &[CalibrationObservation],
) -> PowerPlanningReport {
    let campaign_commitment = campaign_commitment(campaign);
    let manifest_digest = corpus_digest(manifest);
    let mut canonical = observations.to_vec();
    canonical.sort_by_key(observation_key);
    let calibration_input_digest = digest_json(&canonical);
    let mut failures = Vec::new();

    if manifest.schema != "needle.frozen-corpus/4" {
        failures.push("power planning requires a frozen corpus v4 manifest".to_owned());
    }
    failures.extend(validate_power_campaign(campaign));
    if observations.len() > MAX_SCHEDULE_ENTRIES {
        failures.push("calibration observation count exceeds bounded maximum".to_owned());
    }

    let calibration_tasks = manifest
        .tasks
        .iter()
        .filter(|task| task.split == CorpusSplit::Calibration)
        .map(|task| (task.id.as_str(), task))
        .collect::<BTreeMap<_, _>>();
    let material =
        calibration_tasks.values().map(|task| task.material_class).collect::<BTreeSet<_>>();
    let synthetic = material == BTreeSet::from([CorpusMaterialClass::Synthetic]);
    if material.len() != 1 || material.contains(&CorpusMaterialClass::Legacy) {
        failures.push("calibration tasks must use one non-legacy material class".to_owned());
    }

    let mut observation_keys = BTreeSet::new();
    let mut observed_tasks = BTreeSet::new();
    let mut pairs =
        BTreeMap::<(String, BenchmarkRoute, u32, u64), (Option<u64>, Option<u64>)>::new();
    for observation in &canonical {
        if observation.corpus_digest != manifest_digest
            || observation.campaign_commitment != campaign_commitment
        {
            failures.push(format!(
                "calibration observation `{}` has stale corpus or campaign identity",
                observation.task_id
            ));
        }
        let Some(task) = calibration_tasks.get(observation.task_id.as_str()) else {
            failures.push(format!(
                "calibration observation references unknown task `{}`",
                observation.task_id
            ));
            continue;
        };
        observed_tasks.insert(observation.task_id.as_str());
        if observation.split != CorpusSplit::Calibration
            || task.route != observation.route
            || task.split != observation.split
        {
            failures.push(format!(
                "calibration observation metadata differs for task `{}`",
                observation.task_id
            ));
        }
        if !matches!(observation.arm, FinalArm::FrontierDirect | FinalArm::NeedleMiss) {
            failures.push(format!(
                "calibration observation `{}` uses a non-economic arm",
                observation.task_id
            ));
            continue;
        }
        if observation.infrastructure_failure.is_some()
            || !observation.quality_passed
            || observation.total_cost_microcredits.is_none_or(|cost| cost == 0)
            || observation
                .infrastructure_failure
                .as_ref()
                .is_some_and(|detail| detail.len() > MAX_FAILURE_DETAIL_BYTES)
        {
            failures.push(format!(
                "calibration observation `{}` is failed, low-quality, or has invalid cost",
                observation.task_id
            ));
        }
        if !observation_keys.insert(observation_key(observation)) {
            failures.push(format!(
                "duplicate calibration observation identity for task `{}`",
                observation.task_id
            ));
            continue;
        }
        let pair = pairs
            .entry((
                observation.task_id.clone(),
                observation.route,
                observation.repetition,
                observation.pair_seed,
            ))
            .or_default();
        match observation.arm {
            FinalArm::FrontierDirect => pair.0 = observation.total_cost_microcredits,
            FinalArm::NeedleMiss => pair.1 = observation.total_cost_microcredits,
            _ => unreachable!(),
        }
    }
    for task_id in calibration_tasks.keys() {
        if !observed_tasks.contains(task_id) {
            failures.push(format!("calibration task `{task_id}` has no observations"));
        }
    }

    let mut ratios = BTreeMap::<BenchmarkRoute, Vec<f64>>::new();
    for ((task_id, route, repetition, pair_seed), (baseline, treatment)) in pairs {
        match (baseline, treatment) {
            (Some(baseline), Some(treatment)) if baseline > 0 && treatment > 0 => {
                ratios.entry(route).or_default().push(treatment as f64 / baseline as f64);
            }
            _ => failures.push(format!(
                "calibration pair `{task_id}` repetition {repetition} seed {pair_seed} is incomplete"
            )),
        }
    }

    let mut routes = Vec::new();
    for route in [BenchmarkRoute::LocateImplementation, BenchmarkRoute::TraceStateFlow] {
        let route_ratios = ratios.remove(&route).unwrap_or_default();
        let Some(estimate) = estimate_required_pairs(&route_ratios) else {
            failures.push(format!(
                "route {route:?} has insufficient, non-beneficial, or zero-variance calibration"
            ));
            continue;
        };
        if estimate.required_pairs_per_route > MAX_POWER_PLAN_PAIRS as usize {
            failures.push(format!("route {route:?} exceeds the bounded holdout pair maximum"));
            continue;
        }
        routes.push(PowerRoutePlan {
            route,
            baseline_arm: FinalArm::FrontierDirect,
            treatment_arm: FinalArm::NeedleMiss,
            pair_key: POWER_PLAN_PAIR_KEY.to_owned(),
            observed_log_ratio_mean: estimate.observed_log_ratio_mean,
            observed_log_ratio_stddev: estimate.observed_log_ratio_stddev,
            required_pairs: estimate.required_pairs_per_route as u32,
        });
    }

    let plan = if failures.is_empty() {
        let plan = PowerPlan {
            schema: POWER_PLAN_SCHEMA.to_owned(),
            plan_id: format!("power-plan-{}", &calibration_input_digest[3..19]),
            manifest_digest: manifest_digest.clone(),
            campaign_commitment: campaign_commitment.clone(),
            calibration_input_digest: calibration_input_digest.clone(),
            estimator_revision: POWER_PLAN_ESTIMATOR_REVISION.to_owned(),
            alpha_basis_points: POWER_PLAN_ALPHA_BASIS_POINTS,
            target_power_basis_points: POWER_PLAN_TARGET_POWER_BASIS_POINTS,
            routes,
            validated: true,
            synthetic,
            artifact_digest: String::new(),
        }
        .seal();
        let validation = plan.validate(manifest);
        if validation.is_empty() {
            Some(plan)
        } else {
            failures.extend(validation);
            None
        }
    } else {
        None
    };

    PowerPlanningReport {
        schema: POWER_PLANNING_REPORT_SCHEMA.to_owned(),
        manifest_digest,
        campaign_commitment,
        calibration_input_digest,
        estimator_revision: POWER_PLAN_ESTIMATOR_REVISION.to_owned(),
        alpha_basis_points: POWER_PLAN_ALPHA_BASIS_POINTS,
        target_power_basis_points: POWER_PLAN_TARGET_POWER_BASIS_POINTS,
        plan,
        failures,
    }
}

fn observation_key(
    observation: &CalibrationObservation,
) -> (String, String, String, BenchmarkRoute, CorpusSplit, u32, u64, FinalArm) {
    (
        observation.corpus_digest.clone(),
        observation.campaign_commitment.clone(),
        observation.task_id.clone(),
        observation.route,
        observation.split,
        observation.repetition,
        observation.pair_seed,
        observation.arm,
    )
}

fn average(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

#[cfg(test)]
#[path = "power_plan/tests.rs"]
mod tests;
