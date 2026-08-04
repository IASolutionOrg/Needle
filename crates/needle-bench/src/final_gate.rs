use serde::{Deserialize, Serialize};
use statrs::distribution::{ContinuousCDF, Normal};
use std::collections::{BTreeMap, BTreeSet};

use crate::{
    CorpusSchedule, MAX_SCHEDULE_ENTRIES, MultiTaskCampaign, PowerPlan, PowerRoutePlan,
    campaign_commitment, validate_power_campaign,
};

pub const MAX_CORPUS_TASKS: usize = 512;
pub const MAX_CORPUS_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_FINAL_OBSERVATION_BYTES: usize = 64 * 1024 * 1024;
pub const MIN_BOOTSTRAP_RESAMPLES: usize = 1_000;
pub const MAX_BOOTSTRAP_RESAMPLES: usize = 1_000_000;
const MAX_CORPUS_IDENTIFIER_BYTES: usize = 256;

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

/// Classification carried by a public corpus task.  Synthetic and legacy
/// material is useful for deterministic/offline fixtures only and can never
/// make a provider campaign ready.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorpusMaterialClass {
    ProductionSealed,
    Synthetic,
    Legacy,
}

impl Default for CorpusMaterialClass {
    fn default() -> Self {
        Self::Legacy
    }
}

/// A bounded, digest-bound focused-test policy.  The argv is intentionally
/// not part of a public manifest or launch projection; it is resolved by the
/// evaluator-owned sealed contract.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusedTestPolicyRef {
    pub identity: String,
    pub commitment: String,
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
    #[serde(default)]
    pub material_class: CorpusMaterialClass,
    #[serde(default)]
    pub focused_test_policy: FocusedTestPolicyRef,
    #[serde(default)]
    pub oracle_schema: String,
    pub oracle_digest: String,

    // These fields are retained only so archived v2/v3 readers and existing
    // offline synthetic demos can be decoded.  They are never serialized in
    // the v4 public manifest and are rejected for provider execution.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub oracle_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub test_identifier: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_plan_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_plan_digest: Option<String>,
    /// Commitment to the evaluator contract/index.  It is deliberately a
    /// digest only; no external bundle path or location is public.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_bundle_schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_bundle_digest: Option<String>,
    pub tasks: Vec<CorpusTask>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalObservation {
    pub corpus_digest: String,
    pub schedule_digest: String,
    pub power_plan_digest: String,
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
    pub calibration_log_ratio_mean: Option<f64>,
    pub calibration_log_ratio_stddev: Option<f64>,
    pub infrastructure_failures: usize,
    pub validation_failures: Vec<String>,
    pub passed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FinalGateReport {
    pub schema: String,
    pub corpus_digest: String,
    pub campaign_digest: String,
    pub campaign_commitment: String,
    pub schedule_digest: String,
    pub power_plan_digest: String,
    pub power_plan_artifact_digest: String,
    pub estimator_revision: String,
    pub alpha_basis_points: u16,
    pub target_power_basis_points: u16,
    pub bootstrap_seed: u64,
    pub bootstrap_resamples: usize,
    pub economic_baseline: FinalArm,
    pub economic_treatment: FinalArm,
    pub manifest_valid: bool,
    pub manifest_errors: Vec<String>,
    pub contract_valid: bool,
    pub validation_failures: Vec<String>,
    pub routes: Vec<RouteGate>,
    pub passed: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct FinalGateContract<'a> {
    pub manifest: &'a FrozenCorpusManifest,
    pub campaign: &'a MultiTaskCampaign,
    pub schedule: &'a CorpusSchedule,
    pub power_plan: &'a PowerPlan,
    pub campaign_digest: &'a str,
    pub schedule_digest: &'a str,
    pub power_plan_digest: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootstrapConfig {
    pub resamples: usize,
    pub seed: u64,
}

pub fn evaluate_final_gate(
    contract: FinalGateContract<'_>,
    observations: &[FinalObservation],
    bootstrap: BootstrapConfig,
) -> FinalGateReport {
    let FinalGateContract {
        manifest,
        campaign,
        schedule,
        power_plan,
        campaign_digest,
        schedule_digest,
        power_plan_digest,
    } = contract;
    let BootstrapConfig { resamples: bootstrap_resamples, seed: bootstrap_seed } = bootstrap;
    let manifest_errors = validate_frozen_manifest(manifest);
    let manifest_valid = manifest_errors.is_empty();
    let mut validation_failures = Vec::new();
    if manifest.schema != "needle.frozen-corpus/4" {
        validation_failures.push("final gate requires frozen corpus v4".to_owned());
    }
    if !(MIN_BOOTSTRAP_RESAMPLES..=MAX_BOOTSTRAP_RESAMPLES).contains(&bootstrap_resamples) {
        validation_failures.push("bootstrap resample count is outside bounded limits".to_owned());
    }
    validation_failures.extend(validate_power_campaign(campaign));
    let expected_campaign_commitment = campaign_commitment(campaign);
    if manifest.campaign_digest.as_deref() != Some(campaign_digest)
        || !valid_blake3_digest(campaign_digest)
    {
        validation_failures.push("campaign digest differs from the frozen manifest".to_owned());
    }
    if power_plan.campaign_commitment != expected_campaign_commitment {
        validation_failures.push("power plan campaign commitment differs".to_owned());
    }
    if power_plan.synthetic {
        validation_failures
            .push("synthetic power plan is ineligible for the final claim".to_owned());
    }
    if manifest.schedule_digest.as_deref() != Some(schedule_digest)
        || !valid_blake3_digest(schedule_digest)
    {
        validation_failures.push("schedule digest differs from the frozen manifest".to_owned());
    }
    if manifest.power_plan_digest.as_deref() != Some(power_plan_digest)
        || !valid_blake3_digest(power_plan_digest)
    {
        validation_failures.push("power plan digest differs from the frozen manifest".to_owned());
    }
    validation_failures.extend(schedule.validate(manifest, power_plan, power_plan_digest));
    validation_failures.extend(validate_observations(
        manifest,
        schedule,
        schedule_digest,
        power_plan_digest,
        observations,
    ));
    let contract_valid = manifest_valid && validation_failures.is_empty();
    let routes = [BenchmarkRoute::LocateImplementation, BenchmarkRoute::TraceStateFlow]
        .into_iter()
        .map(|route| {
            evaluate_route(
                observations,
                power_plan.routes.iter().find(|plan| plan.route == route),
                route,
                contract_valid,
                bootstrap_resamples,
                bootstrap_seed,
            )
        })
        .collect::<Vec<_>>();
    FinalGateReport {
        schema: "needle.final-gate/3".to_owned(),
        corpus_digest: crate::corpus_digest(manifest),
        campaign_digest: campaign_digest.to_owned(),
        campaign_commitment: expected_campaign_commitment,
        schedule_digest: schedule_digest.to_owned(),
        power_plan_digest: power_plan_digest.to_owned(),
        power_plan_artifact_digest: power_plan.artifact_digest.clone(),
        estimator_revision: power_plan.estimator_revision.clone(),
        alpha_basis_points: power_plan.alpha_basis_points,
        target_power_basis_points: power_plan.target_power_basis_points,
        bootstrap_seed,
        bootstrap_resamples,
        economic_baseline: FinalArm::FrontierDirect,
        economic_treatment: FinalArm::NeedleMiss,
        manifest_valid,
        manifest_errors,
        contract_valid,
        validation_failures,
        passed: contract_valid && routes.iter().all(|route| route.passed),
        routes,
    }
}

pub fn validate_frozen_manifest(manifest: &FrozenCorpusManifest) -> Vec<String> {
    let mut errors = Vec::new();
    if manifest.schema == "needle.frozen-corpus/4" {
        validate_v4_manifest(manifest, &mut errors);
        return errors;
    }
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
            errors.push("duplicate corpus task identifier".to_owned());
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
            errors.push("corpus task metadata is incomplete or exceeds bounds".to_owned());
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

fn validate_v4_manifest(manifest: &FrozenCorpusManifest, errors: &mut Vec<String>) {
    if manifest.frozen_unix_ms == 0 {
        errors.push("frozen corpus timestamp is missing".to_owned());
    }
    let configured_arms = manifest.arms.iter().copied().collect::<BTreeSet<_>>();
    let required_arms = FinalArm::ALL.into_iter().collect::<BTreeSet<_>>();
    if manifest.arms.len() != configured_arms.len() || configured_arms != required_arms {
        errors.push("frozen corpus must configure every final arm exactly once".to_owned());
    }
    for (label, path, digest) in [
        (
            "cost model",
            Some(manifest.cost_model_path.as_str()),
            Some(manifest.cost_model_digest.as_str()),
        ),
        (
            "minimal pilot",
            Some(manifest.next_pilot_path.as_str()),
            Some(manifest.next_pilot_digest.as_str()),
        ),
    ] {
        if path.is_none_or(|value| !safe_relative_json_path(value))
            || digest.is_none_or(|value| !valid_blake3_digest(value))
        {
            errors.push(format!("frozen corpus {label} reference is invalid"));
        }
    }
    match (manifest.campaign_path.as_deref(), manifest.campaign_digest.as_deref()) {
        (Some(path), Some(digest))
            if safe_relative_json_path(path) && valid_blake3_digest(digest) => {}
        _ => errors.push("frozen corpus v4 requires a valid campaign reference".to_owned()),
    }
    match (manifest.schedule_path.as_deref(), manifest.schedule_digest.as_deref()) {
        (Some(path), Some(digest))
            if safe_relative_json_path(path) && valid_blake3_digest(digest) => {}
        _ => errors.push("frozen corpus v4 requires a valid schedule reference".to_owned()),
    }
    match (manifest.power_plan_path.as_deref(), manifest.power_plan_digest.as_deref()) {
        (Some(path), Some(digest))
            if safe_relative_json_path(path) && valid_blake3_digest(digest) => {}
        _ => errors.push("frozen corpus v4 requires a valid power-plan reference".to_owned()),
    }
    match (manifest.sealed_bundle_schema.as_deref(), manifest.sealed_bundle_digest.as_deref()) {
        (Some(schema), Some(digest))
            if schema == "needle.sealed-oracle-index/1" && valid_blake3_digest(digest) => {}
        _ => errors.push("frozen corpus v4 requires a sealed evaluator commitment".to_owned()),
    }
    if manifest.tasks.is_empty() || manifest.tasks.len() > MAX_CORPUS_TASKS {
        errors.push("frozen corpus task count is out of bounds".to_owned());
    }
    let mut tasks = BTreeSet::new();
    for task in &manifest.tasks {
        if !tasks.insert(task.id.as_str()) {
            errors.push("duplicate corpus task identifier".to_owned());
        }
        if task.id.len() > MAX_CORPUS_IDENTIFIER_BYTES
            || task.repository_url.len() > 2_048
            || task.prompt.len() > 4_000
            || task.focused_test_policy.identity.len() > MAX_CORPUS_IDENTIFIER_BYTES
            || task.oracle_schema.len() > MAX_CORPUS_IDENTIFIER_BYTES
            || task.id.trim().is_empty()
            || task.repository_url.trim().is_empty()
            || task.repository_sha.len() != 40
            || !task.repository_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            || task.prompt.trim().len() < 40
            || task.prompt.len() > 4_000
            || task.prompt.contains("@@need")
            || task.focused_test_policy.identity.trim().is_empty()
            || !valid_blake3_digest(&task.focused_test_policy.commitment)
            || task.oracle_schema != "needle.sealed-oracle/1"
            || !valid_blake3_digest(&task.oracle_digest)
        {
            errors.push("corpus task metadata is incomplete or exceeds bounds".to_owned());
        }
        if !task.oracle_path.is_empty()
            || !task.test_identifier.is_empty()
            || !task.focused_command.is_empty()
        {
            errors.push("v4 corpus task contains legacy answer-bearing fields".to_owned());
        }
        if task.material_class == CorpusMaterialClass::Legacy {
            errors.push("corpus task is legacy and provider-ineligible".to_owned());
        }
    }
    for route in [BenchmarkRoute::LocateImplementation, BenchmarkRoute::TraceStateFlow] {
        for split in [CorpusSplit::Calibration, CorpusSplit::Holdout] {
            if !manifest.tasks.iter().any(|task| task.route == route && task.split == split) {
                errors.push(format!("corpus is missing {route:?}/{split:?} tasks"));
            }
        }
    }
}

fn validate_observations(
    manifest: &FrozenCorpusManifest,
    schedule: &CorpusSchedule,
    schedule_digest: &str,
    power_plan_digest: &str,
    observations: &[FinalObservation],
) -> Vec<String> {
    let mut errors = Vec::new();
    if observations.len() > MAX_SCHEDULE_ENTRIES {
        return vec!["final observation count exceeds bounded maximum".to_owned()];
    }
    let manifest_digest = crate::corpus_digest(manifest);
    let expected = schedule
        .entries
        .iter()
        .filter(|entry| entry.split == CorpusSplit::Holdout)
        .map(|entry| {
            (
                entry.task_id.clone(),
                entry.route,
                entry.split,
                entry.arm,
                entry.repetition,
                entry.pair_seed,
            )
        })
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    for observation in observations {
        if observation.corpus_digest != manifest_digest
            || observation.schedule_digest != schedule_digest
            || observation.power_plan_digest != power_plan_digest
        {
            errors.push(format!(
                "observation `{}` has stale corpus, schedule, or power-plan identity",
                observation.task_id
            ));
        }
        if observation.split != CorpusSplit::Holdout {
            errors.push(format!(
                "calibration observation `{}` cannot enter the final gate",
                observation.task_id
            ));
        }
        let key = (
            observation.task_id.clone(),
            observation.route,
            observation.split,
            observation.arm,
            observation.repetition,
            observation.pair_seed,
        );
        if !observed.insert(key.clone()) {
            errors
                .push(format!("duplicate observation identity for task `{}`", observation.task_id));
        }
        if !expected.contains(&key) {
            errors.push(format!(
                "observation identity for task `{}` is not present in the frozen schedule",
                observation.task_id
            ));
        }
        if observation.infrastructure_failure.as_ref().is_some_and(|detail| detail.len() > 512)
            || observation.recomputed_nodes.len() > 512
            || observation.expected_invalidated_nodes.len() > 512
            || observation
                .recomputed_nodes
                .iter()
                .chain(&observation.expected_invalidated_nodes)
                .any(|node| node.is_empty() || node.len() > 512)
        {
            errors.push(format!(
                "observation `{}` exceeds bounded evidence limits",
                observation.task_id
            ));
        }
    }
    for missing in expected.difference(&observed) {
        errors.push(format!(
            "scheduled holdout observation is missing for task `{}` arm {:?} repetition {} seed {}",
            missing.0, missing.3, missing.4, missing.5
        ));
    }
    if expected.len()
        != schedule.entries.iter().filter(|entry| entry.split == CorpusSplit::Holdout).count()
    {
        errors.push("frozen schedule contains duplicate holdout identities".to_owned());
    }
    let task_metadata = manifest
        .tasks
        .iter()
        .map(|task| (task.id.as_str(), (task.route, task.split)))
        .collect::<BTreeMap<_, _>>();
    for observation in observations {
        if task_metadata.get(observation.task_id.as_str())
            != Some(&(observation.route, observation.split))
        {
            errors
                .push(format!("observation metadata differs from task `{}`", observation.task_id));
        }
    }
    errors
}

fn valid_blake3_digest(value: &str) -> bool {
    value.len() == 67
        && value.starts_with("b3:")
        && value[3..].bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
    power_plan: Option<&PowerRoutePlan>,
    route: BenchmarkRoute,
    contract_valid: bool,
    bootstrap_resamples: usize,
    bootstrap_seed: u64,
) -> RouteGate {
    let relevant = observations
        .iter()
        .filter(|item| item.route == route && item.split == CorpusSplit::Holdout)
        .collect::<Vec<_>>();
    let mut validation_failures = Vec::new();
    if !contract_valid {
        validation_failures
            .push("global manifest, plan, schedule, or observation contract failed".to_owned());
    }
    let infrastructure_failures =
        relevant.iter().filter(|item| item.infrastructure_failure.is_some()).count();
    for item in &relevant {
        if let Some(detail) = item.infrastructure_failure.as_deref() {
            let detail = if detail.len() <= 512 { detail } else { "<oversized failure detail>" };
            validation_failures.push(format!(
                "task `{}` arm {:?} has an infrastructure failure: {detail}",
                item.task_id, item.arm,
            ));
        }
        if !item.quality_passed {
            validation_failures
                .push(format!("task `{}` arm {:?} failed quality", item.task_id, item.arm));
        }
        if item.stale_hit {
            validation_failures
                .push(format!("task `{}` arm {:?} produced a stale hit", item.task_id, item.arm));
        }
    }
    let quality_passed = !relevant.is_empty() && relevant.iter().all(|item| item.quality_passed);
    let stale_safe = !relevant.is_empty() && relevant.iter().all(|item| !item.stale_hit);
    let exact = relevant.iter().filter(|item| item.arm == FinalArm::ExactHit).collect::<Vec<_>>();
    let exact_zero_worker = !exact.is_empty() && exact.iter().all(|item| item.worker_spawns == 0);
    if !exact_zero_worker {
        validation_failures.push("exact-hit observations must spawn zero workers".to_owned());
    }
    let partial =
        relevant.iter().filter(|item| item.arm == FinalArm::PartialHit).collect::<Vec<_>>();
    let partial_exact_recomputation = !partial.is_empty()
        && partial.iter().all(|item| {
            let actual = item.recomputed_nodes.iter().collect::<BTreeSet<_>>();
            let expected = item.expected_invalidated_nodes.iter().collect::<BTreeSet<_>>();
            actual.len() == item.recomputed_nodes.len()
                && expected.len() == item.expected_invalidated_nodes.len()
                && actual == expected
        });
    if !partial_exact_recomputation {
        validation_failures
            .push("partial-hit invalidated nodes differ from recomputed nodes".to_owned());
    }
    let zero_main_discovery = !relevant.is_empty()
        && relevant.iter().all(|item| item.main_discovery_on_covered_scope == 0);
    if !zero_main_discovery {
        validation_failures.push("covered-scope observation performed main discovery".to_owned());
    }
    let (ratios, pair_failures) = paired_ratios(&relevant);
    validation_failures.extend(pair_failures);
    let required_pairs = power_plan.map(|plan| plan.required_pairs as usize);
    if power_plan.is_none() {
        validation_failures.push(format!("power plan is missing route {route:?}"));
    }
    let powered = required_pairs == Some(ratios.len());
    if !powered {
        validation_failures.push(format!(
            "route {route:?} observed {} valid pairs but requires {:?}",
            ratios.len(),
            required_pairs
        ));
    }
    let statistical_inputs_valid = validation_failures.is_empty() && powered;
    let paired_cost_ratio = statistical_inputs_valid.then(|| average(&ratios));
    let bca_95_ci = statistical_inputs_valid
        .then(|| bca_interval(&ratios, bootstrap_resamples, bootstrap_seed))
        .flatten();
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
        calibration_log_ratio_mean: power_plan.map(|plan| plan.observed_log_ratio_mean),
        calibration_log_ratio_stddev: power_plan.map(|plan| plan.observed_log_ratio_stddev),
        infrastructure_failures,
        validation_failures,
        passed: quality_passed
            && stale_safe
            && exact_zero_worker
            && partial_exact_recomputation
            && zero_main_discovery
            && powered
            && economics
            && statistical_inputs_valid,
    }
}

fn paired_ratios(observations: &[&FinalObservation]) -> (Vec<f64>, Vec<String>) {
    let mut pairs = BTreeMap::<(String, u32, u64), (Option<u64>, Option<u64>)>::new();
    let mut errors = Vec::new();
    for observation in observations {
        let key = (observation.task_id.clone(), observation.repetition, observation.pair_seed);
        let pair = pairs.entry(key).or_default();
        match observation.arm {
            FinalArm::FrontierDirect => {
                if pair.0.is_some() {
                    errors.push(format!("duplicate baseline for task `{}`", observation.task_id));
                }
                pair.0 = observation.total_cost_microcredits;
            }
            FinalArm::NeedleMiss => {
                if pair.1.is_some() {
                    errors.push(format!("duplicate treatment for task `{}`", observation.task_id));
                }
                pair.1 = observation.total_cost_microcredits;
            }
            _ => {}
        }
    }
    let mut ratios = Vec::new();
    for ((task_id, repetition, pair_seed), (baseline, treatment)) in pairs {
        match (baseline, treatment) {
            (Some(baseline), Some(treatment)) if baseline > 0 && treatment > 0 => {
                ratios.push(treatment as f64 / baseline as f64);
            }
            _ => errors.push(format!(
                "economic pair `{task_id}` repetition {repetition} seed {pair_seed} is incomplete or non-positive"
            )),
        }
    }
    (ratios, errors)
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
#[path = "final_gate/tests.rs"]
mod current_tests;
