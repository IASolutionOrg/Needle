use crate::{
    BenchmarkRoute, CorpusMaterialClass, CorpusSplit, CorpusTask, FinalArm, FrozenCorpusManifest,
    corpus_digest,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const CORPUS_SCHEDULE_SCHEMA: &str = "needle.corpus-schedule/1";
pub const POWER_PLAN_SCHEMA: &str = "needle.power-plan/1";
pub const ARM_LAUNCH_SCHEMA: &str = "needle.arm-launch/1";
pub const MAX_POWER_PLAN_PAIRS: u32 = 10_000;
pub const MAX_SCHEDULE_ENTRIES: usize = 100_000;
pub const MAX_SCHEDULE_BYTES: usize = 4 * 1024 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_PROMPT_BYTES: usize = 4_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleEntry {
    pub task_id: String,
    pub route: BenchmarkRoute,
    pub split: CorpusSplit,
    pub arm: FinalArm,
    pub repetition: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRepetitions {
    pub route: BenchmarkRoute,
    pub repetitions: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerRoutePlan {
    pub route: BenchmarkRoute,
    pub baseline_arm: FinalArm,
    pub treatment_arm: FinalArm,
    pub pair_key: String,
    pub observed_log_ratio_mean: f64,
    pub observed_log_ratio_stddev: f64,
    pub required_pairs: u32,
}

/// Immutable, evaluator-independent count contract.  Issue #7 owns the
/// statistical production of a real plan; this type only binds the resulting
/// counts and provenance to a manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerPlan {
    pub schema: String,
    pub plan_id: String,
    pub manifest_digest: String,
    pub campaign_digest: String,
    pub calibration_input_digest: String,
    pub estimator_revision: String,
    pub alpha_basis_points: u16,
    pub target_power_basis_points: u16,
    pub routes: Vec<PowerRoutePlan>,
    pub validated: bool,
    pub synthetic: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerPlanReference {
    pub schema: String,
    pub digest: String,
}

impl PowerPlanReference {
    pub fn from_plan_bytes(bytes: &[u8]) -> Self {
        Self { schema: POWER_PLAN_SCHEMA.to_owned(), digest: raw_digest(bytes) }
    }
}

impl PowerPlan {
    pub fn validate(&self, manifest: &FrozenCorpusManifest) -> Vec<String> {
        let mut errors = Vec::new();
        if self.schema != POWER_PLAN_SCHEMA {
            errors.push("power plan schema is unsupported".to_owned());
        }
        if self.plan_id.trim().is_empty() || self.plan_id.len() > MAX_IDENTIFIER_BYTES {
            errors.push("power plan id is missing".to_owned());
        }
        if self.manifest_digest != corpus_digest(manifest) {
            errors.push("power plan manifest digest differs".to_owned());
        }
        if !valid_digest(&self.campaign_digest) {
            errors.push("power plan campaign digest is invalid".to_owned());
        }
        if !valid_digest(&self.calibration_input_digest) {
            errors.push("power plan calibration-input digest is invalid".to_owned());
        }
        if self.estimator_revision.trim().is_empty() || self.estimator_revision.len() > 128 {
            errors.push("power plan estimator revision is invalid".to_owned());
        }
        if self.alpha_basis_points != 500 || self.target_power_basis_points != 9_000 {
            errors.push("power plan alpha/target-power contract differs".to_owned());
        }
        let expected_routes =
            [BenchmarkRoute::LocateImplementation, BenchmarkRoute::TraceStateFlow];
        if self.routes.len() != expected_routes.len()
            || self.routes.iter().map(|item| item.route).ne(expected_routes)
        {
            errors.push("power plan routes must be canonical and complete".to_owned());
        }
        let mut seen_routes = BTreeSet::new();
        for route in &self.routes {
            if !seen_routes.insert(route.route) {
                errors.push("power plan contains duplicate routes".to_owned());
            }
            if route.baseline_arm != FinalArm::FrontierDirect
                || route.treatment_arm != FinalArm::NeedleMiss
                || route.pair_key != "task_id:repetition"
                || !route.observed_log_ratio_mean.is_finite()
                || route.observed_log_ratio_mean >= 0.0
                || route.observed_log_ratio_mean < -100.0
                || !route.observed_log_ratio_stddev.is_finite()
                || route.observed_log_ratio_stddev <= 0.0
                || route.observed_log_ratio_stddev > 100.0
                || route.required_pairs == 0
                || route.required_pairs > MAX_POWER_PLAN_PAIRS
            {
                errors
                    .push(format!("power plan route {:?} has invalid bounded values", route.route));
            }
        }
        if !self.validated {
            errors.push("power plan has not been validated".to_owned());
        }
        errors
    }

    pub fn required_pairs(&self, route: BenchmarkRoute) -> Option<u32> {
        self.routes.iter().find(|item| item.route == route).map(|item| item.required_pairs)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorpusSchedule {
    pub schema: String,
    pub manifest_digest: String,
    pub power_plan_digest: String,
    pub automatic_retries: bool,
    pub entries: Vec<ScheduleEntry>,
}

impl CorpusSchedule {
    pub fn validate(
        &self,
        manifest: &FrozenCorpusManifest,
        power_plan: &PowerPlan,
        raw_power_plan_digest: &str,
    ) -> Vec<String> {
        let mut errors = Vec::new();
        if self.schema != CORPUS_SCHEDULE_SCHEMA {
            errors.push("corpus schedule schema is unsupported".to_owned());
        }
        if self.manifest_digest != corpus_digest(manifest) {
            errors.push("schedule manifest digest differs".to_owned());
        }
        if self.power_plan_digest != raw_power_plan_digest || !valid_digest(raw_power_plan_digest) {
            errors.push("schedule power plan digest differs from raw power-plan bytes".to_owned());
        }
        if self.automatic_retries {
            errors.push("automatic retries must be disabled".to_owned());
        }

        if self.entries.len() > MAX_SCHEDULE_ENTRIES {
            errors.push("schedule entry count exceeds bounded maximum".to_owned());
            return errors;
        }
        if self.entries.iter().any(|entry| {
            entry.task_id.len() > MAX_IDENTIFIER_BYTES || entry.task_id.trim().is_empty()
        }) {
            errors.push("schedule task identifier exceeds bounded length".to_owned());
            return errors;
        }
        validate_entries_in_manifest_order(self, manifest, power_plan, &mut errors);
        errors
    }
}

/// Bounded projection handed to a model/worker runner.  No answer, oracle
/// bytes, focused argv, or external bundle location can be represented here.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmLaunch {
    pub schema: String,
    pub manifest_digest: String,
    pub schedule_digest: String,
    pub power_plan_digest: String,
    pub task_id: String,
    pub route: BenchmarkRoute,
    pub split: CorpusSplit,
    pub repository_sha: String,
    pub prompt: String,
    pub arm: FinalArm,
    pub repetition: u32,
    pub focused_test_policy_identity: String,
    pub focused_test_policy_commitment: String,
}

impl ArmLaunch {
    fn from_task_with_digests(
        task: &CorpusTask,
        arm: FinalArm,
        repetition: u32,
        manifest_digest: &str,
        schedule_digest: &str,
        power_plan_digest: &str,
    ) -> Result<Self, String> {
        if !valid_digest(manifest_digest)
            || !valid_digest(schedule_digest)
            || !valid_digest(power_plan_digest)
        {
            return Err("launch artifact digests are missing or invalid".to_owned());
        }
        if task.material_class == CorpusMaterialClass::Legacy {
            return Err("legacy task cannot be projected for provider execution".to_owned());
        }
        if task.id.trim().is_empty()
            || task.repository_sha.len() != 40
            || !task.repository_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            || task.prompt.trim().is_empty()
            || task.id.len() > MAX_IDENTIFIER_BYTES
            || task.prompt.len() > MAX_PROMPT_BYTES
            || task.focused_test_policy.identity.trim().is_empty()
            || !valid_digest(&task.focused_test_policy.commitment)
        {
            return Err("task has incomplete bounded launch metadata".to_owned());
        }
        Ok(Self {
            schema: ARM_LAUNCH_SCHEMA.to_owned(),
            manifest_digest: manifest_digest.to_owned(),
            schedule_digest: schedule_digest.to_owned(),
            power_plan_digest: power_plan_digest.to_owned(),
            task_id: task.id.clone(),
            route: task.route,
            split: task.split,
            repository_sha: task.repository_sha.clone(),
            prompt: task.prompt.clone(),
            arm,
            repetition,
            focused_test_policy_identity: task.focused_test_policy.identity.clone(),
            focused_test_policy_commitment: task.focused_test_policy.commitment.clone(),
        })
    }

    pub fn serialization_is_bounded(&self) -> bool {
        let encoded = serde_json::to_string(self).unwrap_or_default().to_ascii_lowercase();
        [
            "oracle_path",
            "oracle_bytes",
            "needles",
            "expected_file",
            "expected_symbol",
            "focused_command",
            "argv",
            "bundle_path",
            "bundle_location",
            "evaluator_answer",
        ]
        .iter()
        .all(|forbidden| !encoded.contains(forbidden))
    }
}

pub fn build_launch_plan(
    manifest: &FrozenCorpusManifest,
    schedule: &CorpusSchedule,
    power_plan: &PowerPlan,
    schedule_digest: &str,
    power_plan_digest: &str,
) -> Result<Vec<ArmLaunch>, Vec<String>> {
    let errors = schedule.validate(manifest, power_plan, power_plan_digest);
    if !errors.is_empty() {
        return Err(errors);
    }
    let manifest_digest = corpus_digest(manifest);
    schedule
        .entries
        .iter()
        .map(|entry| {
            let task = manifest
                .tasks
                .iter()
                .find(|task| task.id == entry.task_id)
                .ok_or_else(|| "schedule references an unknown task".to_owned())?;
            ArmLaunch::from_task_with_digests(
                task,
                entry.arm,
                entry.repetition,
                &manifest_digest,
                schedule_digest,
                power_plan_digest,
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| vec![error])
}

fn validate_entries_in_manifest_order(
    schedule: &CorpusSchedule,
    manifest: &FrozenCorpusManifest,
    power_plan: &PowerPlan,
    errors: &mut Vec<String>,
) {
    let mut offset = 0usize;
    let mut holdout_pairs = BTreeMap::<BenchmarkRoute, BTreeSet<(String, u32)>>::new();
    let calibration_arms = [FinalArm::FrontierDirect, FinalArm::NeedleMiss];
    for task in manifest.tasks.iter().filter(|task| task.split == CorpusSplit::Calibration) {
        for arm in calibration_arms {
            let Some(entry) = schedule.entries.get(offset) else {
                errors.push("schedule is missing a calibration entry".to_owned());
                return;
            };
            if entry.task_id != task.id
                || entry.route != task.route
                || entry.split != task.split
                || entry.arm != arm
                || entry.repetition != 0
            {
                errors.push(
                    "schedule calibration entries differ from canonical task order".to_owned(),
                );
            }
            offset = offset.saturating_add(1);
        }
    }
    for task in manifest.tasks.iter().filter(|task| task.split == CorpusSplit::Holdout) {
        let mut task_pairs = BTreeSet::new();
        let mut pair_counts = BTreeMap::<(String, u32), usize>::new();
        let task_start = offset;
        while let Some(entry) = schedule.entries.get(offset) {
            if entry.task_id != task.id {
                break;
            }
            if entry.route != task.route || entry.split != task.split {
                errors.push("schedule holdout metadata differs from manifest task".to_owned());
            }
            let pair = (entry.task_id.clone(), entry.repetition);
            task_pairs.insert(pair.clone());
            *pair_counts.entry(pair.clone()).or_default() += 1;
            holdout_pairs.entry(task.route).or_default().insert(pair);
            let within_pair = (offset - task_start) % FinalArm::ALL.len();
            if entry.arm != FinalArm::ALL[within_pair] {
                errors.push("schedule holdout arms are not in canonical FinalArm order".to_owned());
            }
            let canonical_repetition = ((offset - task_start) / FinalArm::ALL.len()) as u32;
            if entry.repetition != canonical_repetition {
                errors.push(
                    "schedule holdout repetitions are not in canonical block order".to_owned(),
                );
            }
            offset = offset.saturating_add(1);
        }
        if task_pairs.is_empty() {
            errors.push("holdout task is missing from schedule".to_owned());
            continue;
        }
        let repetitions =
            task_pairs.iter().map(|(_, repetition)| *repetition).collect::<BTreeSet<_>>();
        if repetitions
            .iter()
            .copied()
            .enumerate()
            .any(|(index, repetition)| repetition != index as u32)
        {
            errors.push("holdout task repetitions are not contiguous".to_owned());
        }
        if pair_counts.values().any(|count| *count != FinalArm::ALL.len()) {
            errors.push("holdout task must contain one complete arm set per pair".to_owned());
        }
    }
    if offset != schedule.entries.len() {
        errors.push("schedule contains duplicate or extra entries".to_owned());
    }
    for route in [BenchmarkRoute::LocateImplementation, BenchmarkRoute::TraceStateFlow] {
        let observed = holdout_pairs.get(&route).map_or(0, BTreeSet::len) as u32;
        if power_plan.required_pairs(route) != Some(observed) {
            errors.push(format!("schedule holdout pair count differs for route {route:?}"));
        }
    }
}

pub fn digest_json<T: Serialize>(value: &T) -> String {
    format!("b3:{}", blake3::hash(&serde_json::to_vec(value).unwrap_or_default()).to_hex())
}

pub fn raw_digest(bytes: &[u8]) -> String {
    format!("b3:{}", blake3::hash(bytes).to_hex())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 67
        && value.starts_with("b3:")
        && value[3..].bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FocusedTestPolicyRef;

    fn task(split: CorpusSplit, route: BenchmarkRoute, id: &str) -> CorpusTask {
        CorpusTask {
            id: id.to_owned(),
            route,
            split,
            repository_url: "https://example.invalid/repo.git".to_owned(),
            repository_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            prompt: "A bounded synthetic benchmark prompt with no answer material.".to_owned(),
            material_class: CorpusMaterialClass::Synthetic,
            focused_test_policy: FocusedTestPolicyRef {
                identity: "synthetic-policy".to_owned(),
                commitment: "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_owned(),
            },
            oracle_schema: "needle.sealed-oracle/1".to_owned(),
            oracle_digest: "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_owned(),
            oracle_path: String::new(),
            test_identifier: String::new(),
            focused_command: Vec::new(),
        }
    }

    fn manifest() -> FrozenCorpusManifest {
        let tasks = vec![
            task(CorpusSplit::Calibration, BenchmarkRoute::LocateImplementation, "cal-locate"),
            task(CorpusSplit::Calibration, BenchmarkRoute::TraceStateFlow, "cal-trace"),
            task(CorpusSplit::Holdout, BenchmarkRoute::LocateImplementation, "hold-locate"),
            task(CorpusSplit::Holdout, BenchmarkRoute::TraceStateFlow, "hold-trace"),
        ];
        FrozenCorpusManifest {
            schema: "needle.frozen-corpus/4".to_owned(),
            frozen_unix_ms: 1,
            arms: FinalArm::ALL.to_vec(),
            cost_model_path: "cost-model.json".to_owned(),
            cost_model_digest:
                "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            next_pilot_path: "minimal-live-pilot.json".to_owned(),
            next_pilot_digest:
                "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            campaign_path: Some("campaign.json".to_owned()),
            campaign_digest: Some(
                "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            ),
            schedule_path: None,
            schedule_digest: None,
            power_plan_path: None,
            power_plan_digest: None,
            sealed_bundle_schema: None,
            sealed_bundle_digest: None,
            tasks,
        }
    }

    fn plan(manifest: &FrozenCorpusManifest) -> PowerPlan {
        PowerPlan {
            schema: POWER_PLAN_SCHEMA.to_owned(),
            plan_id: "synthetic".to_owned(),
            manifest_digest: corpus_digest(manifest),
            campaign_digest: manifest.campaign_digest.clone().unwrap(),
            calibration_input_digest:
                "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            estimator_revision: "issue-7-structural-v1".to_owned(),
            alpha_basis_points: 500,
            target_power_basis_points: 9_000,
            routes: vec![
                PowerRoutePlan {
                    route: BenchmarkRoute::LocateImplementation,
                    baseline_arm: FinalArm::FrontierDirect,
                    treatment_arm: FinalArm::NeedleMiss,
                    pair_key: "task_id:repetition".to_owned(),
                    observed_log_ratio_mean: -0.4,
                    observed_log_ratio_stddev: 0.2,
                    required_pairs: 1,
                },
                PowerRoutePlan {
                    route: BenchmarkRoute::TraceStateFlow,
                    baseline_arm: FinalArm::FrontierDirect,
                    treatment_arm: FinalArm::NeedleMiss,
                    pair_key: "task_id:repetition".to_owned(),
                    observed_log_ratio_mean: -0.4,
                    observed_log_ratio_stddev: 0.2,
                    required_pairs: 1,
                },
            ],
            validated: true,
            synthetic: true,
        }
    }

    fn schedule(manifest: &FrozenCorpusManifest, plan: &PowerPlan) -> CorpusSchedule {
        let mut entries = Vec::new();
        for task in manifest.tasks.iter().filter(|task| task.split == CorpusSplit::Calibration) {
            for arm in [FinalArm::FrontierDirect, FinalArm::NeedleMiss] {
                entries.push(ScheduleEntry {
                    task_id: task.id.clone(),
                    route: task.route,
                    split: task.split,
                    arm,
                    repetition: 0,
                });
            }
        }
        for task in manifest.tasks.iter().filter(|task| task.split == CorpusSplit::Holdout) {
            for arm in FinalArm::ALL {
                entries.push(ScheduleEntry {
                    task_id: task.id.clone(),
                    route: task.route,
                    split: task.split,
                    arm,
                    repetition: 0,
                });
            }
        }
        let bytes = serde_json::to_vec(&plan).unwrap();
        CorpusSchedule {
            schema: CORPUS_SCHEDULE_SCHEMA.to_owned(),
            manifest_digest: corpus_digest(manifest),
            power_plan_digest: raw_digest(&bytes),
            automatic_retries: false,
            entries,
        }
    }

    #[test]
    fn schedule_is_canonical_and_launch_projection_binds_artifact_digests() {
        let manifest = manifest();
        let plan = plan(&manifest);
        let plan_bytes = serde_json::to_vec(&plan).unwrap();
        let plan_digest = raw_digest(&plan_bytes);
        let schedule = schedule(&manifest, &plan);
        let schedule_bytes = serde_json::to_vec(&schedule).unwrap();
        let schedule_digest = raw_digest(&schedule_bytes);
        assert!(plan.validate(&manifest).is_empty());
        assert!(schedule.validate(&manifest, &plan, &plan_digest).is_empty());
        let launch =
            build_launch_plan(&manifest, &schedule, &plan, &schedule_digest, &plan_digest).unwrap();
        assert_eq!(launch.len(), schedule.entries.len());
        assert_eq!(launch[0].manifest_digest, corpus_digest(&manifest));
        assert!(launch.iter().all(ArmLaunch::serialization_is_bounded));
    }

    #[test]
    fn schedule_rejects_duplicate_reordered_and_syntactically_valid_stale_plan_digest() {
        let manifest = manifest();
        let plan = plan(&manifest);
        let plan_digest = raw_digest(&serde_json::to_vec(&plan).unwrap());
        let mut schedule = schedule(&manifest, &plan);
        schedule.entries.reverse();
        schedule.entries.push(schedule.entries[0].clone());
        let errors = schedule.validate(&manifest, &plan, &plan_digest);
        assert!(errors.iter().any(|error| error.contains("canonical")
            || error.contains("duplicate")
            || error.contains("extra")));
        let stale = "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(
            schedule
                .validate(&manifest, &plan, stale)
                .iter()
                .any(|error| error.contains("raw power-plan"))
        );
    }

    #[test]
    fn schedule_rejects_complete_holdout_blocks_in_reversed_repetition_order() {
        let manifest = manifest();
        let mut plan = plan(&manifest);
        for route in &mut plan.routes {
            route.required_pairs = 2;
        }
        let canonical = schedule(&manifest, &plan);
        let mut entries = canonical.entries[..4].to_vec();
        for task_id in ["hold-locate", "hold-trace"] {
            let block = canonical
                .entries
                .iter()
                .filter(|entry| entry.task_id == task_id)
                .cloned()
                .collect::<Vec<_>>();
            let mut repetition_one = block.clone();
            repetition_one.iter_mut().for_each(|entry| entry.repetition = 1);
            let mut repetition_zero = block;
            repetition_zero.iter_mut().for_each(|entry| entry.repetition = 0);
            entries.extend(repetition_one);
            entries.extend(repetition_zero);
        }
        let mut reversed = canonical;
        reversed.entries = entries;
        let plan_digest = raw_digest(&serde_json::to_vec(&plan).unwrap());
        let errors = reversed.validate(&manifest, &plan, &plan_digest);
        assert!(errors.iter().any(|error| error.contains("repetition") && error.contains("block")));
    }

    #[test]
    fn public_manifest_and_launch_shapes_are_answer_free() {
        let encoded = serde_json::to_string(&manifest()).unwrap();
        for prohibited in [
            "oracle_path",
            "focused_command",
            "test_identifier",
            "needles",
            "expected_symbol",
            "bundle_location",
        ] {
            assert!(!encoded.contains(prohibited), "manifest leaked `{prohibited}`");
        }
        let manifest = manifest();
        let plan = plan(&manifest);
        let plan_bytes = serde_json::to_vec(&plan).unwrap();
        let plan_digest = raw_digest(&plan_bytes);
        let schedule = schedule(&manifest, &plan);
        let schedule_digest = raw_digest(&serde_json::to_vec(&schedule).unwrap());
        let launch = build_launch_plan(&manifest, &schedule, &plan, &schedule_digest, &plan_digest)
            .unwrap()
            .remove(0);
        assert!(launch.serialization_is_bounded());
    }

    #[test]
    fn unknown_schedule_fields_and_synthetic_plan_are_fail_closed() {
        let malformed = r#"{"schema":"needle.corpus-schedule/1","manifest_digest":"b3:0000000000000000000000000000000000000000000000000000000000000000","power_plan_digest":"b3:0000000000000000000000000000000000000000000000000000000000000000","automatic_retries":false,"entries":[],"answer":"leak"}"#;
        assert!(serde_json::from_str::<CorpusSchedule>(malformed).is_err());
        let manifest = manifest();
        let plan = plan(&manifest);
        assert!(plan.synthetic);
    }
}
