use serde::{Deserialize, Serialize};
use statrs::distribution::{ContinuousCDF, Normal};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkRoute {
    LocateImplementation,
    TraceStateFlow,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorpusSplit {
    Calibration,
    Holdout,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalArm {
    FrontierDirect,
    NativeSubagent,
    NeedleMiss,
    ExactHit,
    PartialHit,
    Escalation,
    IrrelevantMutation,
    RelevantMutation,
}

impl FinalArm {
    pub const ALL: [Self; 8] = [
        Self::FrontierDirect,
        Self::NativeSubagent,
        Self::NeedleMiss,
        Self::ExactHit,
        Self::PartialHit,
        Self::Escalation,
        Self::IrrelevantMutation,
        Self::RelevantMutation,
    ];
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusTask {
    pub id: String,
    pub route: BenchmarkRoute,
    pub split: CorpusSplit,
    pub repository_url: String,
    pub repository_sha: String,
    pub prompt: String,
    pub oracle_path: String,
    pub oracle_digest: String,
    pub test_identifier: String,
    pub focused_command: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenCorpusManifest {
    pub schema: String,
    pub frozen_unix_ms: u64,
    pub arms: Vec<FinalArm>,
    pub cost_model_path: String,
    pub cost_model_digest: String,
    pub next_pilot_path: String,
    pub next_pilot_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_digest: Option<String>,
    pub tasks: Vec<CorpusTask>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalObservation {
    pub task_id: String,
    pub route: BenchmarkRoute,
    pub split: CorpusSplit,
    pub arm: FinalArm,
    pub repetition: u32,
    pub pair_seed: u64,
    pub quality_passed: bool,
    pub stale_hit: bool,
    pub infrastructure_failure: Option<String>,
    pub total_cost_microcredits: Option<u64>,
    pub worker_spawns: u32,
    pub main_discovery_on_covered_scope: u32,
    pub recomputed_nodes: Vec<String>,
    pub expected_invalidated_nodes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteGate {
    pub route: BenchmarkRoute,
    pub quality_passed: bool,
    pub stale_safe: bool,
    pub exact_zero_worker: bool,
    pub partial_exact_recomputation: bool,
    pub zero_main_discovery: bool,
    pub paired_cost_ratio: Option<f64>,
    pub bca_95_ci: Option<[f64; 2]>,
    pub valid_pairs: usize,
    pub required_pairs: Option<usize>,
    pub powered: bool,
    pub infrastructure_failures: usize,
    pub passed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FinalGateReport {
    pub schema: String,
    pub corpus_digest: String,
    pub economic_baseline: FinalArm,
    pub economic_treatment: FinalArm,
    pub manifest_valid: bool,
    pub manifest_errors: Vec<String>,
    pub routes: Vec<RouteGate>,
    pub passed: bool,
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
    let required = (((1.644_853_626_951_472_2 + 1.281_551_565_544_600_4) * stddev / mean.abs())
        .powi(2))
    .ceil()
    .max(2.0) as usize;
    Some(PowerEstimate {
        observed_log_ratio_mean: mean,
        observed_log_ratio_stddev: stddev,
        required_pairs_per_route: required,
        power: 0.90,
        one_sided_alpha: 0.05,
    })
}

pub fn evaluate_final_gate(
    manifest: &FrozenCorpusManifest,
    observations: &[FinalObservation],
    bootstrap_resamples: usize,
    bootstrap_seed: u64,
) -> FinalGateReport {
    let mut manifest_errors = validate_frozen_manifest(manifest);
    manifest_errors.extend(validate_observations(manifest, observations));
    let manifest_valid = manifest_errors.is_empty();
    let routes = [BenchmarkRoute::LocateImplementation, BenchmarkRoute::TraceStateFlow]
        .into_iter()
        .map(|route| evaluate_route(observations, route, bootstrap_resamples, bootstrap_seed))
        .collect::<Vec<_>>();
    FinalGateReport {
        schema: "needle.final-gate/2".to_owned(),
        corpus_digest: blake3::hash(&serde_json::to_vec(manifest).unwrap_or_default())
            .to_hex()
            .to_string(),
        economic_baseline: FinalArm::FrontierDirect,
        economic_treatment: FinalArm::NeedleMiss,
        manifest_valid,
        manifest_errors,
        passed: manifest_valid && routes.iter().all(|route| route.passed),
        routes,
    }
}

pub fn validate_frozen_manifest(manifest: &FrozenCorpusManifest) -> Vec<String> {
    let mut errors = Vec::new();
    if !matches!(manifest.schema.as_str(), "needle.frozen-corpus/2" | "needle.frozen-corpus/3") {
        errors.push("unsupported frozen corpus schema".to_owned());
    }
    if manifest.frozen_unix_ms == 0 {
        errors.push("frozen corpus timestamp is missing".to_owned());
    }
    let configured_arms = manifest.arms.iter().copied().collect::<BTreeSet<_>>();
    let required_arms = FinalArm::ALL.into_iter().collect::<BTreeSet<_>>();
    if manifest.arms.len() != configured_arms.len() || configured_arms != required_arms {
        errors.push("frozen corpus must configure every final arm exactly once".to_owned());
    }
    if !safe_relative_json_path(&manifest.cost_model_path)
        || !valid_blake3_digest(&manifest.cost_model_digest)
        || !safe_relative_json_path(&manifest.next_pilot_path)
        || !valid_blake3_digest(&manifest.next_pilot_digest)
    {
        errors.push("frozen corpus campaign reference is invalid".to_owned());
    }
    match (
        manifest.schema.as_str(),
        manifest.campaign_path.as_deref(),
        manifest.campaign_digest.as_deref(),
    ) {
        ("needle.frozen-corpus/2", None, None) => {}
        ("needle.frozen-corpus/3", Some(path), Some(digest))
            if safe_relative_json_path(path) && valid_blake3_digest(digest) => {}
        ("needle.frozen-corpus/2", _, _) => {
            errors.push("frozen corpus v2 cannot reference a multi-task campaign".to_owned());
        }
        ("needle.frozen-corpus/3", _, _) => {
            errors
                .push("frozen corpus v3 requires a valid multi-task campaign reference".to_owned());
        }
        _ => {}
    }
    let mut tasks = BTreeMap::new();
    for task in &manifest.tasks {
        if tasks.insert(task.id.as_str(), task).is_some() {
            errors.push(format!("duplicate corpus task `{}`", task.id));
        }
        if task.repository_url.is_empty()
            || task.repository_sha.len() != 40
            || !task.repository_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            || task.prompt.trim().len() < 40
            || task.prompt.len() > 4_000
            || task.prompt.contains("@@need")
            || !safe_relative_json_path(&task.oracle_path)
            || !valid_blake3_digest(&task.oracle_digest)
            || task.test_identifier.trim().is_empty()
            || task.focused_command.is_empty()
        {
            errors.push(format!("corpus task `{}` is incomplete", task.id));
        }
        let expected_command = [
            "cargo",
            "test",
            "--offline",
            "--test",
            "integration",
            task.test_identifier.as_str(),
            "--",
            "--exact",
        ];
        if task.focused_command.iter().map(String::as_str).ne(expected_command) {
            errors.push(format!(
                "corpus task `{}` does not use the exact direct Cargo test policy",
                task.id
            ));
        }
    }
    for route in [BenchmarkRoute::LocateImplementation, BenchmarkRoute::TraceStateFlow] {
        for split in [CorpusSplit::Calibration, CorpusSplit::Holdout] {
            if !manifest.tasks.iter().any(|task| task.route == route && task.split == split) {
                errors.push(format!("corpus is missing {route:?}/{split:?} tasks"));
            }
        }
    }
    errors
}

fn validate_observations(
    manifest: &FrozenCorpusManifest,
    observations: &[FinalObservation],
) -> Vec<String> {
    let mut errors = Vec::new();
    let tasks =
        manifest.tasks.iter().map(|task| (task.id.as_str(), task)).collect::<BTreeMap<_, _>>();
    let mut observation_keys = BTreeSet::new();
    for observation in observations {
        let Some(task) = tasks.get(observation.task_id.as_str()) else {
            errors.push(format!("observation references unknown task `{}`", observation.task_id));
            continue;
        };
        if task.route != observation.route || task.split != observation.split {
            errors
                .push(format!("observation metadata differs from task `{}`", observation.task_id));
        }
        let key = (
            observation.task_id.as_str(),
            observation.arm,
            observation.repetition,
            observation.pair_seed,
        );
        if !observation_keys.insert(key) {
            errors
                .push(format!("duplicate observation identity for task `{}`", observation.task_id));
        }
    }
    for task in manifest.tasks.iter().filter(|task| task.split == CorpusSplit::Holdout) {
        for arm in FinalArm::ALL {
            if !observations.iter().any(|item| item.task_id == task.id && item.arm == arm) {
                errors.push(format!("holdout task `{}` is missing arm {arm:?}", task.id));
            }
        }
    }
    errors
}

fn valid_blake3_digest(value: &str) -> bool {
    value.len() == 67
        && value.starts_with("b3:")
        && value[3..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn safe_relative_json_path(value: &str) -> bool {
    let path = std::path::Path::new(value);
    !value.is_empty()
        && path.is_relative()
        && path.extension().and_then(|extension| extension.to_str()) == Some("json")
        && path.components().all(|component| {
            matches!(component, std::path::Component::Normal(_) | std::path::Component::CurDir)
        })
}

fn evaluate_route(
    observations: &[FinalObservation],
    route: BenchmarkRoute,
    bootstrap_resamples: usize,
    bootstrap_seed: u64,
) -> RouteGate {
    let relevant = observations
        .iter()
        .filter(|item| item.route == route && item.split == CorpusSplit::Holdout)
        .collect::<Vec<_>>();
    let infrastructure_failures =
        relevant.iter().filter(|item| item.infrastructure_failure.is_some()).count();
    let valid = relevant
        .iter()
        .filter(|item| item.infrastructure_failure.is_none())
        .copied()
        .collect::<Vec<_>>();
    let quality_passed = !valid.is_empty() && valid.iter().all(|item| item.quality_passed);
    let stale_safe = !valid.is_empty() && valid.iter().all(|item| !item.stale_hit);
    let exact = valid.iter().filter(|item| item.arm == FinalArm::ExactHit).collect::<Vec<_>>();
    let exact_zero_worker = !exact.is_empty() && exact.iter().all(|item| item.worker_spawns == 0);
    let partial = valid.iter().filter(|item| item.arm == FinalArm::PartialHit).collect::<Vec<_>>();
    let partial_exact_recomputation = !partial.is_empty()
        && partial.iter().all(|item| {
            let actual = item.recomputed_nodes.iter().collect::<BTreeSet<_>>();
            let expected = item.expected_invalidated_nodes.iter().collect::<BTreeSet<_>>();
            actual == expected
        });
    let zero_main_discovery =
        !valid.is_empty() && valid.iter().all(|item| item.main_discovery_on_covered_scope == 0);
    let ratios = paired_ratios(&valid);
    let calibration = observations
        .iter()
        .filter(|item| {
            item.route == route
                && item.split == CorpusSplit::Calibration
                && item.infrastructure_failure.is_none()
        })
        .collect::<Vec<_>>();
    let power = estimate_required_pairs(&paired_ratios(&calibration));
    let required_pairs = power.as_ref().map(|estimate| estimate.required_pairs_per_route);
    let powered = required_pairs.is_some_and(|required| ratios.len() >= required);
    let paired_cost_ratio = (!ratios.is_empty()).then(|| average(&ratios));
    let bca_95_ci = bca_interval(&ratios, bootstrap_resamples, bootstrap_seed);
    let economics = bca_95_ci.is_some_and(|interval| interval[1] < 1.0);
    RouteGate {
        route,
        quality_passed,
        stale_safe,
        exact_zero_worker,
        partial_exact_recomputation,
        zero_main_discovery,
        paired_cost_ratio,
        bca_95_ci,
        valid_pairs: ratios.len(),
        required_pairs,
        powered,
        infrastructure_failures,
        passed: quality_passed
            && stale_safe
            && exact_zero_worker
            && partial_exact_recomputation
            && zero_main_discovery
            && powered
            && economics,
    }
}

fn paired_ratios(observations: &[&FinalObservation]) -> Vec<f64> {
    let mut pairs = BTreeMap::<(String, u32, u64), (Option<u64>, Option<u64>)>::new();
    for observation in observations {
        let key = (observation.task_id.clone(), observation.repetition, observation.pair_seed);
        let pair = pairs.entry(key).or_default();
        match observation.arm {
            FinalArm::FrontierDirect => pair.0 = observation.total_cost_microcredits,
            FinalArm::NeedleMiss => pair.1 = observation.total_cost_microcredits,
            _ => {}
        }
    }
    pairs
        .into_values()
        .filter_map(|(native, needle)| {
            let native = native?;
            let needle = needle?;
            (native > 0).then_some(needle as f64 / native as f64)
        })
        .collect()
}

fn bca_interval(values: &[f64], resamples: usize, seed: u64) -> Option<[f64; 2]> {
    if values.len() < 3
        || resamples < 1_000
        || values.iter().any(|value| !value.is_finite() || *value < 0.0)
    {
        return None;
    }
    let observed = average(values);
    let mut random = Lcg::new(seed);
    let mut bootstrap = Vec::with_capacity(resamples);
    for _ in 0..resamples {
        let sample_mean =
            (0..values.len()).map(|_| values[random.index(values.len())]).sum::<f64>()
                / values.len() as f64;
        bootstrap.push(sample_mean);
    }
    bootstrap.sort_by(f64::total_cmp);
    let below = bootstrap.iter().filter(|value| **value < observed).count() as f64;
    let normal = Normal::new(0.0, 1.0).ok()?;
    let proportion = ((below + 0.5) / (resamples as f64 + 1.0)).clamp(1e-9, 1.0 - 1e-9);
    let bias = normal.inverse_cdf(proportion);

    let jackknife = (0..values.len())
        .map(|omitted| {
            values
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != omitted)
                .map(|(_, value)| *value)
                .sum::<f64>()
                / (values.len() - 1) as f64
        })
        .collect::<Vec<_>>();
    let jackknife_mean = average(&jackknife);
    let numerator = jackknife.iter().map(|value| (jackknife_mean - value).powi(3)).sum::<f64>();
    let denominator =
        6.0 * jackknife.iter().map(|value| (jackknife_mean - value).powi(2)).sum::<f64>().powf(1.5);
    let acceleration = if denominator == 0.0 { 0.0 } else { numerator / denominator };
    let adjusted = |alpha: f64| {
        let z = normal.inverse_cdf(alpha);
        normal.cdf(bias + (bias + z) / (1.0 - acceleration * (bias + z)))
    };
    Some([quantile(&bootstrap, adjusted(0.025)), quantile(&bootstrap, adjusted(0.975))])
}

fn average(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

fn quantile(sorted: &[f64], probability: f64) -> f64 {
    let position = probability.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        sorted[lower]
    } else {
        sorted[lower] + (sorted[upper] - sorted[lower]) * (position - lower as f64)
    }
}

struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn index(&mut self, upper: usize) -> usize {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        ((self.0 >> 32) as usize) % upper
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> FrozenCorpusManifest {
        let mut tasks = Vec::new();
        for route in [BenchmarkRoute::LocateImplementation, BenchmarkRoute::TraceStateFlow] {
            for split in [CorpusSplit::Calibration, CorpusSplit::Holdout] {
                tasks.push(CorpusTask {
                    id: if split == CorpusSplit::Calibration {
                        format!("{route:?}-cal")
                    } else {
                        format!("{route:?}")
                    },
                    route,
                    split,
                    repository_url: "https://example.invalid/repository.git".to_owned(),
                    repository_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                    prompt:
                        "Locate the implementation and provide a focused test for this behavior."
                            .to_owned(),
                    oracle_path: format!("oracles/{route:?}-{split:?}.json"),
                    oracle_digest:
                        "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                            .to_owned(),
                    test_identifier: "misc::example".to_owned(),
                    focused_command: [
                        "cargo",
                        "test",
                        "--offline",
                        "--test",
                        "integration",
                        "misc::example",
                        "--",
                        "--exact",
                    ]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                });
            }
        }
        FrozenCorpusManifest {
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
            tasks,
        }
    }

    fn observation(
        route: BenchmarkRoute,
        arm: FinalArm,
        repetition: u32,
        cost: Option<u64>,
    ) -> FinalObservation {
        FinalObservation {
            task_id: format!("{route:?}"),
            route,
            split: CorpusSplit::Holdout,
            arm,
            repetition,
            pair_seed: repetition as u64,
            quality_passed: true,
            stale_hit: false,
            infrastructure_failure: None,
            total_cost_microcredits: cost,
            worker_spawns: u32::from(arm != FinalArm::ExactHit),
            main_discovery_on_covered_scope: 0,
            recomputed_nodes: if arm == FinalArm::PartialHit {
                vec!["behavior".to_owned()]
            } else {
                Vec::new()
            },
            expected_invalidated_nodes: if arm == FinalArm::PartialHit {
                vec!["behavior".to_owned()]
            } else {
                Vec::new()
            },
        }
    }

    #[test]
    fn bca_gate_passes_only_when_both_routes_have_upper_bound_below_one() {
        let mut observations = Vec::new();
        for route in [BenchmarkRoute::LocateImplementation, BenchmarkRoute::TraceStateFlow] {
            for arm in [
                FinalArm::NativeSubagent,
                FinalArm::ExactHit,
                FinalArm::PartialHit,
                FinalArm::Escalation,
                FinalArm::IrrelevantMutation,
                FinalArm::RelevantMutation,
            ] {
                observations.push(observation(route, arm, 0, None));
            }
            for repetition in 1..=6 {
                let mut frontier =
                    observation(route, FinalArm::FrontierDirect, repetition, Some(1_000));
                frontier.task_id = format!("{route:?}-cal");
                frontier.split = CorpusSplit::Calibration;
                observations.push(frontier);
                let mut needle = observation(
                    route,
                    FinalArm::NeedleMiss,
                    repetition,
                    Some(600 + repetition as u64 * 10),
                );
                needle.task_id = format!("{route:?}-cal");
                needle.split = CorpusSplit::Calibration;
                observations.push(needle);
            }
            for repetition in 1..=12 {
                observations.push(observation(
                    route,
                    FinalArm::FrontierDirect,
                    repetition,
                    Some(1_000 + repetition as u64),
                ));
                observations.push(observation(
                    route,
                    FinalArm::NeedleMiss,
                    repetition,
                    Some(600 + repetition as u64),
                ));
            }
        }
        let report = evaluate_final_gate(&manifest(), &observations, 2_000, 7);
        assert!(report.passed);
        assert!(report.manifest_valid);
        assert_eq!(report.economic_baseline, FinalArm::FrontierDirect);
        assert_eq!(report.economic_treatment, FinalArm::NeedleMiss);
        assert!(report.routes.iter().all(|route| route.powered));
        assert!(report.routes.iter().all(|route| route.bca_95_ci.unwrap()[1] < 1.0));
    }

    #[test]
    fn any_stale_or_quality_failure_blocks_the_route() {
        let mut observations = vec![
            observation(BenchmarkRoute::LocateImplementation, FinalArm::ExactHit, 0, None),
            observation(BenchmarkRoute::LocateImplementation, FinalArm::PartialHit, 0, None),
        ];
        observations[0].stale_hit = true;
        let report = evaluate_final_gate(&manifest(), &observations, 2_000, 1);
        assert!(!report.routes[0].passed);
        assert!(!report.routes[0].stale_safe);
    }

    #[test]
    fn incomplete_manifest_or_missing_arms_cannot_pass() {
        let observations = vec![
            observation(BenchmarkRoute::LocateImplementation, FinalArm::ExactHit, 0, None),
            observation(BenchmarkRoute::LocateImplementation, FinalArm::PartialHit, 0, None),
        ];
        let report = evaluate_final_gate(&manifest(), &observations, 2_000, 9);
        assert!(!report.manifest_valid);
        assert!(!report.passed);
        assert!(report.manifest_errors.iter().any(|error| error.contains("missing arm")));
    }

    #[test]
    fn frozen_corpus_v3_requires_a_content_addressed_campaign() {
        let mut manifest = manifest();
        manifest.schema = "needle.frozen-corpus/3".to_owned();
        assert!(validate_frozen_manifest(&manifest).iter().any(|error| {
            error == "frozen corpus v3 requires a valid multi-task campaign reference"
        }));

        manifest.campaign_path = Some("campaign.json".to_owned());
        manifest.campaign_digest =
            Some("b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned());
        assert!(validate_frozen_manifest(&manifest).is_empty());
    }

    #[test]
    fn power_estimate_uses_only_observed_calibration_variance() {
        let estimate = estimate_required_pairs(&[0.70, 0.76, 0.82, 0.73]).unwrap();
        assert_eq!(estimate.power, 0.90);
        assert_eq!(estimate.one_sided_alpha, 0.05);
        assert!(estimate.required_pairs_per_route >= 2);
    }
}
