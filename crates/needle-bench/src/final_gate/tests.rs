use super::*;
use crate::{
    CORPUS_SCHEDULE_SCHEMA, CalibrationObservation, CampaignBudgetReserve, CorpusMaterialClass,
    CorpusSchedule, CorpusScheduleEntry, FocusedTestPolicyRef, MultiTaskCampaign,
    POWER_PLAN_ESTIMATOR_REVISION, POWER_PLAN_PAIR_KEY, POWER_PLAN_SCHEMA, PowerPlan,
    campaign_commitment, corpus_digest, plan_power, raw_digest,
};

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
        prompt: "A bounded final-gate benchmark prompt with no embedded answer material."
            .to_owned(),
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
        schedule_digest: None,
        power_plan_path: Some("power-plan.json".to_owned()),
        power_plan_digest: None,
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
        one_sided_alpha_basis_points: 500,
        target_power_basis_points: 9_000,
        budget_reserve: CampaignBudgetReserve {
            main_turn_microcredits: 1,
            extra_main_turns_per_needle_observation: 1,
            worker_microcredits: 1,
            extra_workers_per_needle_observation: 0,
            evidence: vec!["synthetic".to_owned()],
        },
    }
}

fn calibration_observations(
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
                    pair_seed: 10 + repetition as u64,
                    quality_passed: true,
                    infrastructure_failure: None,
                    total_cost_microcredits: Some(cost),
                });
            }
        }
    }
    observations
}

struct Fixture {
    manifest: FrozenCorpusManifest,
    campaign: MultiTaskCampaign,
    campaign_digest: String,
    schedule: CorpusSchedule,
    schedule_digest: String,
    plan: PowerPlan,
    plan_digest: String,
    observations: Vec<FinalObservation>,
}

fn fixture(material_class: CorpusMaterialClass) -> Fixture {
    let mut manifest = manifest(material_class);
    let campaign = campaign();
    let campaign_digest = raw_digest(&serde_json::to_vec(&campaign).unwrap());
    manifest.campaign_digest = Some(campaign_digest.clone());
    let planning =
        plan_power(&manifest, &campaign, &calibration_observations(&manifest, &campaign));
    assert!(planning.failures.is_empty(), "{:?}", planning.failures);
    let plan = planning.plan.unwrap();
    let plan_digest = raw_digest(&serde_json::to_vec(&plan).unwrap());
    let mut entries = Vec::new();
    for task in manifest.tasks.iter().filter(|task| task.split == CorpusSplit::Calibration) {
        for arm in [FinalArm::FrontierDirect, FinalArm::NeedleMiss] {
            entries.push(CorpusScheduleEntry {
                task_id: task.id.clone(),
                route: task.route,
                split: task.split,
                arm,
                repetition: 0,
                pair_seed: 10,
            });
        }
    }
    for task in manifest.tasks.iter().filter(|task| task.split == CorpusSplit::Holdout) {
        for repetition in 0..plan.required_pairs(task.route).unwrap() {
            for arm in FinalArm::ALL {
                entries.push(CorpusScheduleEntry {
                    task_id: task.id.clone(),
                    route: task.route,
                    split: task.split,
                    arm,
                    repetition,
                    pair_seed: 100 + repetition as u64,
                });
            }
        }
    }
    let schedule = CorpusSchedule {
        schema: CORPUS_SCHEDULE_SCHEMA.to_owned(),
        manifest_digest: corpus_digest(&manifest),
        power_plan_digest: plan_digest.clone(),
        automatic_retries: false,
        entries,
    };
    let schedule_digest = raw_digest(&serde_json::to_vec(&schedule).unwrap());
    manifest.schedule_digest = Some(schedule_digest.clone());
    manifest.power_plan_digest = Some(plan_digest.clone());
    assert!(validate_frozen_manifest(&manifest).is_empty());
    assert!(plan.validate(&manifest).is_empty());
    assert!(schedule.validate(&manifest, &plan, &plan_digest).is_empty());

    let corpus = corpus_digest(&manifest);
    let observations = schedule
        .entries
        .iter()
        .filter(|entry| entry.split == CorpusSplit::Holdout)
        .map(|entry| {
            let invalidated_nodes = if entry.arm == FinalArm::PartialHit {
                vec!["behavior".to_owned()]
            } else {
                Vec::new()
            };
            FinalObservation {
                corpus_digest: corpus.clone(),
                schedule_digest: schedule_digest.clone(),
                power_plan_digest: plan_digest.clone(),
                task_id: entry.task_id.clone(),
                route: entry.route,
                split: entry.split,
                arm: entry.arm,
                repetition: entry.repetition,
                pair_seed: entry.pair_seed,
                quality_passed: true,
                stale_hit: false,
                infrastructure_failure: None,
                total_cost_microcredits: match entry.arm {
                    FinalArm::FrontierDirect => Some(1_000 + entry.repetition as u64 * 10),
                    FinalArm::NeedleMiss => Some(550 + entry.repetition as u64 * 10),
                    _ => Some(100),
                },
                worker_spawns: u32::from(entry.arm != FinalArm::ExactHit),
                main_discovery_on_covered_scope: 0,
                recomputed_nodes: invalidated_nodes.clone(),
                expected_invalidated_nodes: invalidated_nodes,
            }
        })
        .collect();
    Fixture {
        manifest,
        campaign,
        campaign_digest,
        schedule,
        schedule_digest,
        plan,
        plan_digest,
        observations,
    }
}

fn evaluate(fixture: &Fixture) -> FinalGateReport {
    evaluate_final_gate(
        FinalGateContract {
            manifest: &fixture.manifest,
            campaign: &fixture.campaign,
            schedule: &fixture.schedule,
            power_plan: &fixture.plan,
            campaign_digest: &fixture.campaign_digest,
            schedule_digest: &fixture.schedule_digest,
            power_plan_digest: &fixture.plan_digest,
        },
        &fixture.observations,
        BootstrapConfig { resamples: 2_000, seed: 42 },
    )
}

#[test]
fn complete_schedule_bound_holdout_passes_both_routes() {
    let fixture = fixture(CorpusMaterialClass::ProductionSealed);
    let report = evaluate(&fixture);
    assert!(report.passed, "{:#?}", report);
    assert!(report.contract_valid);
    assert_eq!(report.schedule_digest, fixture.schedule_digest);
    assert_eq!(report.power_plan_digest, fixture.plan_digest);
    assert_eq!(report.power_plan_artifact_digest, fixture.plan.artifact_digest);
    assert_eq!(report.estimator_revision, POWER_PLAN_ESTIMATOR_REVISION);
    assert_eq!(report.bootstrap_seed, 42);
    assert_eq!(report.bootstrap_resamples, 2_000);
    assert!(report.routes.iter().all(|route| {
        route.powered
            && route.valid_pairs == route.required_pairs.unwrap()
            && route.bca_95_ci.unwrap()[1] < 1.0
            && route.validation_failures.is_empty()
    }));
}

#[test]
fn bootstrap_work_is_bounded_before_evaluation() {
    let fixture = fixture(CorpusMaterialClass::ProductionSealed);
    let report = evaluate_final_gate(
        FinalGateContract {
            manifest: &fixture.manifest,
            campaign: &fixture.campaign,
            schedule: &fixture.schedule,
            power_plan: &fixture.plan,
            campaign_digest: &fixture.campaign_digest,
            schedule_digest: &fixture.schedule_digest,
            power_plan_digest: &fixture.plan_digest,
        },
        &fixture.observations,
        BootstrapConfig { resamples: MAX_BOOTSTRAP_RESAMPLES + 1, seed: 42 },
    );
    assert!(!report.contract_valid);
    assert!(report.validation_failures.iter().any(|failure| failure.contains("bootstrap")));
    assert!(report.routes.iter().all(|route| route.bca_95_ci.is_none()));
}

#[test]
fn synthetic_plan_and_calibration_leakage_are_ineligible() {
    let synthetic = fixture(CorpusMaterialClass::Synthetic);
    let report = evaluate(&synthetic);
    assert!(!report.passed);
    assert!(report.validation_failures.iter().any(|failure| failure.contains("synthetic")));

    let mut production = fixture(CorpusMaterialClass::ProductionSealed);
    let entry = production.schedule.entries[0].clone();
    production.observations.push(FinalObservation {
        corpus_digest: corpus_digest(&production.manifest),
        schedule_digest: production.schedule_digest.clone(),
        power_plan_digest: production.plan_digest.clone(),
        task_id: entry.task_id,
        route: entry.route,
        split: entry.split,
        arm: entry.arm,
        repetition: entry.repetition,
        pair_seed: entry.pair_seed,
        quality_passed: true,
        stale_hit: false,
        infrastructure_failure: None,
        total_cost_microcredits: Some(1_000),
        worker_spawns: 1,
        main_discovery_on_covered_scope: 0,
        recomputed_nodes: Vec::new(),
        expected_invalidated_nodes: Vec::new(),
    });
    let report = evaluate(&production);
    assert!(!report.passed);
    assert!(report.validation_failures.iter().any(|failure| failure.contains("cannot enter")));
}

#[test]
fn final_gate_rejects_missing_duplicate_extra_and_stale_identities() {
    let base = fixture(CorpusMaterialClass::ProductionSealed);
    let mut cases = Vec::<(&str, Fixture)>::new();

    let mut missing = fixture(CorpusMaterialClass::ProductionSealed);
    missing.observations.pop();
    cases.push(("missing", missing));

    let mut duplicate = fixture(CorpusMaterialClass::ProductionSealed);
    duplicate.observations.push(duplicate.observations[0].clone());
    cases.push(("duplicate", duplicate));

    let mut seed = fixture(CorpusMaterialClass::ProductionSealed);
    seed.observations[0].pair_seed += 1;
    cases.push(("mismatched seed", seed));

    let mut split = fixture(CorpusMaterialClass::ProductionSealed);
    split.observations[0].split = CorpusSplit::Calibration;
    cases.push(("cross split", split));

    let mut route = fixture(CorpusMaterialClass::ProductionSealed);
    route.observations[0].route = BenchmarkRoute::TraceStateFlow;
    cases.push(("cross route", route));

    let mut stale = fixture(CorpusMaterialClass::ProductionSealed);
    stale.observations[0].schedule_digest = digest('9');
    cases.push(("stale digest", stale));

    let mut extra = fixture(CorpusMaterialClass::ProductionSealed);
    let mut observation = extra.observations[0].clone();
    observation.repetition = 999;
    extra.observations.push(observation);
    cases.push(("extra repetition", extra));

    for (name, fixture) in cases {
        let report = evaluate(&fixture);
        assert!(!report.passed, "case `{name}` unexpectedly passed");
        assert!(
            !report.validation_failures.is_empty()
                || report.routes.iter().any(|route| !route.validation_failures.is_empty()),
            "case `{name}` did not record its failure"
        );
    }
    assert!(evaluate(&base).passed);
}

#[test]
fn failed_evidence_is_never_filtered_into_a_favorable_subset() {
    let mut cases = Vec::<(&str, Fixture)>::new();

    let mut infrastructure = fixture(CorpusMaterialClass::ProductionSealed);
    infrastructure.observations[0].infrastructure_failure = Some("transport".to_owned());
    cases.push(("infrastructure", infrastructure));

    let mut quality = fixture(CorpusMaterialClass::ProductionSealed);
    quality.observations[0].quality_passed = false;
    cases.push(("quality", quality));

    let mut stale = fixture(CorpusMaterialClass::ProductionSealed);
    stale.observations[0].stale_hit = true;
    cases.push(("stale", stale));

    let mut cost = fixture(CorpusMaterialClass::ProductionSealed);
    cost.observations[0].total_cost_microcredits = Some(0);
    cases.push(("non-positive cost", cost));

    let mut worker = fixture(CorpusMaterialClass::ProductionSealed);
    let exact = worker.observations.iter_mut().find(|item| item.arm == FinalArm::ExactHit).unwrap();
    exact.worker_spawns = 1;
    cases.push(("worker count", worker));

    let mut invalidation = fixture(CorpusMaterialClass::ProductionSealed);
    let partial =
        invalidation.observations.iter_mut().find(|item| item.arm == FinalArm::PartialHit).unwrap();
    partial.recomputed_nodes = vec!["unrelated".to_owned()];
    cases.push(("invalidated nodes", invalidation));

    let mut discovery = fixture(CorpusMaterialClass::ProductionSealed);
    discovery.observations[0].main_discovery_on_covered_scope = 1;
    cases.push(("main discovery", discovery));

    for (name, fixture) in cases {
        let report = evaluate(&fixture);
        assert!(!report.passed, "case `{name}` unexpectedly passed");
        assert!(
            report.routes.iter().any(|route| {
                !route.validation_failures.is_empty()
                    && route.paired_cost_ratio.is_none()
                    && route.bca_95_ci.is_none()
            }),
            "case `{name}` retained favorable statistics"
        );
    }
}

#[test]
fn holdout_values_cannot_change_the_frozen_power_plan() {
    let mut fixture = fixture(CorpusMaterialClass::ProductionSealed);
    let required = fixture.plan.routes.iter().map(|route| route.required_pairs).collect::<Vec<_>>();
    fixture
        .observations
        .iter_mut()
        .filter(|item| item.arm == FinalArm::NeedleMiss)
        .for_each(|item| item.total_cost_microcredits = Some(1_100));
    let report = evaluate(&fixture);
    assert_eq!(
        report.routes.iter().map(|route| route.required_pairs.unwrap() as u32).collect::<Vec<_>>(),
        required
    );
    assert!(!report.passed);
    assert!(report.routes.iter().all(|route| !route.powered || route.bca_95_ci.unwrap()[1] >= 1.0));
}

#[test]
fn forged_power_plan_content_or_schedule_count_fails_before_statistics() {
    let mut mismatched_campaign = fixture(CorpusMaterialClass::ProductionSealed);
    mismatched_campaign.campaign.budget_reserve.main_turn_microcredits += 1;
    let report = evaluate(&mismatched_campaign);
    assert!(!report.contract_valid);
    assert!(
        report.validation_failures.iter().any(|failure| failure.contains("campaign commitment"))
    );

    let mut forged = fixture(CorpusMaterialClass::ProductionSealed);
    forged.plan.routes[0].required_pairs += 1;
    let report = evaluate(&forged);
    assert!(!report.contract_valid);
    assert!(
        report.validation_failures.iter().any(|failure| {
            failure.contains("artifact digest") || failure.contains("pair count")
        })
    );

    let mut stale_schedule = fixture(CorpusMaterialClass::ProductionSealed);
    stale_schedule.schedule.entries.pop();
    let report = evaluate(&stale_schedule);
    assert!(!report.contract_valid);
    assert!(report.validation_failures.iter().any(|failure| {
        failure.contains("pair count")
            || failure.contains("missing")
            || failure.contains("complete arm set")
    }));
}

#[test]
fn power_plan_shape_rejects_changed_estimator_contract() {
    let mut fixture = fixture(CorpusMaterialClass::ProductionSealed);
    fixture.plan.estimator_revision = "changed".to_owned();
    fixture.plan.alpha_basis_points = 499;
    fixture.plan.routes[0].pair_key = "list-order".to_owned();
    fixture.plan = fixture.plan.seal();
    let failures = fixture.plan.validate(&fixture.manifest);
    assert!(failures.iter().any(|failure| failure.contains("estimator")));
    assert!(failures.iter().any(|failure| failure.contains("alpha")));
    assert!(failures.iter().any(|failure| failure.contains("bounded values")));
    assert_eq!(fixture.plan.schema, POWER_PLAN_SCHEMA);
    assert_eq!(
        POWER_PLAN_PAIR_KEY,
        "corpus_digest:campaign_commitment:task_id:route:split:repetition:pair_seed"
    );
}
