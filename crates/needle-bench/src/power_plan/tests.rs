use super::*;
use crate::{CampaignBudgetReserve, CorpusTask, FocusedTestPolicyRef};

fn digest(byte: char) -> String {
    format!("b3:{}", byte.to_string().repeat(64))
}

fn task(
    id: &str,
    route: BenchmarkRoute,
    split: CorpusSplit,
    material_class: CorpusMaterialClass,
) -> CorpusTask {
    CorpusTask {
        id: id.to_owned(),
        route,
        split,
        repository_url: "https://example.invalid/repository.git".to_owned(),
        repository_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        prompt: "A bounded benchmark task prompt with no embedded answer material.".to_owned(),
        material_class,
        focused_test_policy: FocusedTestPolicyRef {
            identity: format!("policy-{id}"),
            commitment: digest('1'),
        },
        oracle_schema: "needle.sealed-oracle/1".to_owned(),
        oracle_digest: digest('2'),
        oracle_path: String::new(),
        test_identifier: String::new(),
        focused_command: Vec::new(),
    }
}

fn manifest(material_class: CorpusMaterialClass) -> FrozenCorpusManifest {
    FrozenCorpusManifest {
        schema: "needle.frozen-corpus/4".to_owned(),
        frozen_unix_ms: 1,
        arms: FinalArm::ALL.to_vec(),
        cost_model_path: "cost-model.json".to_owned(),
        cost_model_digest: digest('3'),
        next_pilot_path: "pilot.json".to_owned(),
        next_pilot_digest: digest('4'),
        campaign_path: Some("campaign.json".to_owned()),
        campaign_digest: Some(digest('5')),
        schedule_path: Some("schedule.json".to_owned()),
        schedule_digest: Some(digest('6')),
        power_plan_path: Some("power-plan.json".to_owned()),
        power_plan_digest: Some(digest('7')),
        sealed_bundle_schema: Some("needle.sealed-oracle-index/1".to_owned()),
        sealed_bundle_digest: Some(digest('8')),
        tasks: vec![
            task(
                "cal-locate",
                BenchmarkRoute::LocateImplementation,
                CorpusSplit::Calibration,
                material_class,
            ),
            task(
                "cal-trace",
                BenchmarkRoute::TraceStateFlow,
                CorpusSplit::Calibration,
                material_class,
            ),
            task(
                "hold-locate",
                BenchmarkRoute::LocateImplementation,
                CorpusSplit::Holdout,
                material_class,
            ),
            task(
                "hold-trace",
                BenchmarkRoute::TraceStateFlow,
                CorpusSplit::Holdout,
                material_class,
            ),
        ],
    }
}

fn campaign() -> MultiTaskCampaign {
    MultiTaskCampaign {
        schema: "needle.multi-task-campaign/2".to_owned(),
        schedule_digest: digest('6'),
        task_ids: Vec::new(),
        paid_arms: vec![FinalArm::FrontierDirect, FinalArm::NeedleMiss],
        offline_cache_arms: vec![
            FinalArm::ExactHit,
            FinalArm::PartialHit,
            FinalArm::IrrelevantMutation,
            FinalArm::RelevantMutation,
        ],
        deferred_diagnostic_arms: vec![FinalArm::NativeSubagent, FinalArm::Escalation],
        repetitions_per_task: 0,
        automatic_retries: false,
        statistical_claim: false,
        bootstrap_resamples: 10_000,
        one_sided_alpha_basis_points: POWER_PLAN_ALPHA_BASIS_POINTS,
        target_power_basis_points: POWER_PLAN_TARGET_POWER_BASIS_POINTS,
        budget_reserve: CampaignBudgetReserve {
            main_turn_microcredits: 1,
            extra_main_turns_per_needle_observation: 1,
            worker_microcredits: 1,
            extra_workers_per_needle_observation: 0,
            evidence: vec!["synthetic".to_owned()],
        },
    }
}

fn observations(
    manifest: &FrozenCorpusManifest,
    campaign: &MultiTaskCampaign,
) -> Vec<CalibrationObservation> {
    let corpus = corpus_digest(manifest);
    let campaign = campaign_commitment(campaign);
    let mut observations = Vec::new();
    for (task_id, route) in [
        ("cal-locate", BenchmarkRoute::LocateImplementation),
        ("cal-trace", BenchmarkRoute::TraceStateFlow),
    ] {
        for (repetition, treatment) in [(0, 600), (1, 700)] {
            for (arm, cost) in
                [(FinalArm::FrontierDirect, 1_000), (FinalArm::NeedleMiss, treatment)]
            {
                observations.push(CalibrationObservation {
                    corpus_digest: corpus.clone(),
                    campaign_commitment: campaign.clone(),
                    task_id: task_id.to_owned(),
                    route,
                    split: CorpusSplit::Calibration,
                    arm,
                    repetition,
                    pair_seed: 100 + repetition as u64,
                    quality_passed: true,
                    infrastructure_failure: None,
                    total_cost_microcredits: Some(cost),
                });
            }
        }
    }
    observations
}

#[test]
fn planning_is_deterministic_and_digest_bound() {
    let manifest = manifest(CorpusMaterialClass::ProductionSealed);
    let campaign = campaign();
    let observations = observations(&manifest, &campaign);
    let first = plan_power(&manifest, &campaign, &observations);
    let mut reordered = observations.clone();
    reordered.reverse();
    let second = plan_power(&manifest, &campaign, &reordered);
    assert!(first.failures.is_empty(), "{:?}", first.failures);
    assert_eq!(first, second);
    let plan = first.plan.unwrap();
    assert!(!plan.synthetic);
    assert_eq!(plan.artifact_digest, plan.canonical_digest());
    assert!(plan.routes.iter().all(|route| route.required_pairs >= 3));

    let mut changed = observations;
    changed[0].total_cost_microcredits = Some(1_001);
    let changed = plan_power(&manifest, &campaign, &changed);
    assert_ne!(changed.calibration_input_digest, second.calibration_input_digest);
    assert_ne!(changed.plan.unwrap().artifact_digest, plan.artifact_digest);
}

#[test]
fn synthetic_material_is_derived_and_cannot_be_presented_as_real() {
    let manifest = manifest(CorpusMaterialClass::Synthetic);
    let campaign = campaign();
    let report = plan_power(&manifest, &campaign, &observations(&manifest, &campaign));
    let plan = report.plan.unwrap();
    assert!(plan.synthetic);
    let mut forged = plan;
    forged.synthetic = false;
    forged = forged.seal();
    assert!(
        forged
            .validate(&manifest)
            .iter()
            .any(|failure| failure.contains("material classification"))
    );
}

#[test]
fn planning_rejects_changed_campaign_parameters() {
    let manifest = manifest(CorpusMaterialClass::ProductionSealed);
    let mut campaign = campaign();
    let mut observations = observations(&manifest, &campaign);
    campaign.one_sided_alpha_basis_points = 499;
    let changed_commitment = campaign_commitment(&campaign);
    observations
        .iter_mut()
        .for_each(|observation| observation.campaign_commitment = changed_commitment.clone());
    let report = plan_power(&manifest, &campaign, &observations);
    assert!(report.plan.is_none());
    assert!(report.failures.iter().any(|failure| failure.contains("campaign contract")));
}

#[test]
fn planning_fails_closed_for_incomplete_or_contaminated_calibration() {
    let manifest = manifest(CorpusMaterialClass::ProductionSealed);
    let campaign = campaign();
    let baseline = observations(&manifest, &campaign);
    let mut cases = Vec::<(&str, Vec<CalibrationObservation>)>::new();

    let mut missing = baseline.clone();
    missing.pop();
    cases.push(("missing counterpart", missing));

    let mut duplicate = baseline.clone();
    duplicate.push(duplicate[0].clone());
    cases.push(("duplicate arm", duplicate));

    let mut mismatched_seed = baseline.clone();
    mismatched_seed[1].pair_seed += 1;
    cases.push(("mismatched seed", mismatched_seed));

    let mut non_positive = baseline.clone();
    non_positive[0].total_cost_microcredits = Some(0);
    cases.push(("non-positive cost", non_positive));

    let mut quality = baseline.clone();
    quality[0].quality_passed = false;
    cases.push(("quality failure", quality));

    let mut infrastructure = baseline.clone();
    infrastructure[0].infrastructure_failure = Some("transport".to_owned());
    cases.push(("infrastructure failure", infrastructure));

    let mut split = baseline.clone();
    split[0].split = CorpusSplit::Holdout;
    cases.push(("split contamination", split));

    let mut route = baseline.clone();
    route[0].route = BenchmarkRoute::TraceStateFlow;
    cases.push(("route contamination", route));

    let mut stale = baseline.clone();
    stale[0].corpus_digest = digest('9');
    cases.push(("stale corpus", stale));

    let mut non_beneficial = baseline.clone();
    non_beneficial
        .iter_mut()
        .filter(|item| item.arm == FinalArm::NeedleMiss)
        .for_each(|item| item.total_cost_microcredits = Some(1_100 + item.repetition as u64 * 100));
    cases.push(("non-beneficial effect", non_beneficial));

    let mut zero_variance = baseline;
    zero_variance
        .iter_mut()
        .filter(|item| item.arm == FinalArm::NeedleMiss)
        .for_each(|item| item.total_cost_microcredits = Some(600));
    cases.push(("zero variance", zero_variance));

    for (name, observations) in cases {
        let report = plan_power(&manifest, &campaign, &observations);
        assert!(report.plan.is_none(), "case `{name}` unexpectedly produced a plan");
        assert!(!report.failures.is_empty(), "case `{name}` did not explain the failure");
    }
}
