use needle_core::{
    ArtifactId, ArtifactKind, ArtifactRequest, CacheResolution, CommandExecutionEvidence, Digest,
    EvidenceFailurePolicy, FlowStepRole, LocationRole, Need, NeedFragment, NeedIr, RouteContract,
    SemanticArtifactResult, SemanticFlowStep, SemanticLocation, SemanticWorkerArtifact, TestPlan,
    WorkerObservationTrace, built_in_route_contracts, compile_need, need_fragment,
};
use needle_runtime::{
    NeedShadowWrite, RuntimeSettings, RuntimeStore, SemanticCostEstimates, SemanticResolver,
    SemanticReuseDecision, validate_semantic_artifact, validate_semantic_test_plan_with_evidence,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use thiserror::Error;

pub const CALIBRATION_REPLAY_SCHEMA_ID: &str = "needle.proof-calibration-replay/1";
pub const RIPGREP_CALIBRATION_SHA: &str = "4649aa9700619f94cf9c66876e9549d83420e16c";
pub const RIPGREP_CALIBRATION_SUBJECT: &str = "--glob-case-insensitive";

const EVIDENCE_FILES: [&str; 4] = [
    "crates/core/flags/defs.rs",
    "crates/core/flags/hiargs.rs",
    "crates/ignore/src/overrides.rs",
    "tests/misc.rs",
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CalibrationReplayCase {
    pub name: String,
    pub expected_plan: String,
    pub observed_plan: String,
    pub expected_selected_artifacts: u32,
    pub selected_artifacts: u32,
    pub expected_stale_candidates: u32,
    pub stale_candidates: u32,
    pub sufficiency_certificate: Option<String>,
    pub plan_id: String,
    pub authoritative: bool,
    pub runtime_resolution: String,
    pub passed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationReplayReport {
    pub schema_id: String,
    pub mode: String,
    pub provider_calls: u32,
    pub repository_sha: String,
    pub repository_lineage: String,
    pub source_snapshot_digest: String,
    pub worker_schema: String,
    pub worker_artifacts: u32,
    pub validator_revision: u32,
    pub validation_certificates: u32,
    pub cases: Vec<CalibrationReplayCase>,
    pub selected_proofs: u32,
    pub true_positives: u32,
    pub false_positives: u32,
    pub proof_precision: f64,
    pub opportunity_rate: f64,
    pub authoritative_hits: u32,
    pub workers_avoided: u32,
    pub live_run_ready: bool,
}

#[derive(Debug, Error)]
pub enum CalibrationReplayError {
    #[error("calibration replay I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("calibration replay store failed: {0}")]
    Store(#[from] needle_runtime::StoreError),
    #[error("calibration replay validation failed: {0}")]
    Validation(#[from] needle_runtime::SemanticValidationError),
    #[error("calibration replay resolution failed: {0}")]
    Resolver(#[from] needle_runtime::SemanticResolverError),
    #[error("calibration replay input is invalid: {0}")]
    Invalid(String),
    #[error("calibration replay serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn run_positive_calibration_replay(
    source_repository: &Path,
    artifact_root: &Path,
) -> Result<(CalibrationReplayReport, SemanticArtifactResult), CalibrationReplayError> {
    let repository_root = artifact_root.join("fixture-repository");
    copy_evidence_files(source_repository, &repository_root)?;
    let snapshot_digest = evidence_snapshot_digest(&repository_root)?;
    let repository_lineage = Digest::blake3(
        format!("https://github.com/BurntSushi/ripgrep\n{RIPGREP_CALIBRATION_SHA}").as_bytes(),
    );

    let routes = built_in_route_contracts();
    let locate_route = route(&routes, "locate.implementation")?;
    let trace_route = route(&routes, "trace.state-flow")?;
    let tests_route = route(&routes, "tests.relevant")?;
    let locate_ir = parse_need(locate_marker())?;
    let trace_ir = parse_need(trace_marker())?;
    let tests_ir = parse_need(tests_marker())?;
    let locate = compile_need(&locate_ir, repository_lineage, locate_route)
        .map_err(|error| CalibrationReplayError::Invalid(error.to_string()))?;
    let trace = compile_need(&trace_ir, repository_lineage, trace_route)
        .map_err(|error| CalibrationReplayError::Invalid(error.to_string()))?;
    let tests = compile_need(&tests_ir, repository_lineage, tests_route)
        .map_err(|error| CalibrationReplayError::Invalid(error.to_string()))?;
    let locate_fragment = need_fragment(&locate, locate.required.clone(), Vec::new());
    let mut trace_obligations = trace.required.clone();
    trace_obligations.extend(trace.preferred.clone());
    let trace_fragment = need_fragment(&trace, trace_obligations, Vec::new());
    let tests_fragment = need_fragment(&tests, tests.required.clone(), Vec::new());

    let store = RuntimeStore::new(artifact_root.join("needle.sqlite3"));
    store.initialize()?;
    store.initialize_defaults(&RuntimeSettings {
        codex_executable: "codex".to_owned(),
        worker_model: "deterministic-fixture".to_owned(),
        worker_reasoning: "none".to_owned(),
        worker_timeout_seconds: 1,
        evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
        trusted_test_execution: false,
        multi_need_policy: needle_core::MultiNeedPolicy::default(),
    })?;
    record_need(&store, "locate", &locate_ir, &locate, &locate_fragment)?;
    record_need(&store, "trace", &trace_ir, &trace, &trace_fragment)?;
    record_need(&store, "tests", &tests_ir, &tests, &tests_fragment)?;

    let result = semantic_worker_result(&repository_root)?;
    let encoded = serde_json::to_vec(&result)?;
    let result: SemanticArtifactResult = serde_json::from_slice(&encoded)?;
    if result.schema_id != needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID
        || result.artifacts.len() != 3
        || !result.observation_trace.gaps.is_empty()
    {
        return Err(CalibrationReplayError::Invalid(
            "worker fixture does not satisfy artifact-result/2 bounds".to_owned(),
        ));
    }

    let mut artifact_ids = BTreeMap::<ArtifactKind, ArtifactId>::new();
    let (declared_test_plan, simulated_test_evidence) =
        deterministic_test_validation(snapshot_digest);
    store.record_command_evidence(None, &simulated_test_evidence)?;
    for worker_artifact in &result.artifacts {
        let kind = worker_artifact.kind();
        let request = semantic_request(
            &kind,
            &trace,
            &trace_fragment,
            trace_route,
            snapshot_digest,
            "Trace the declared CLI option through the runtime.",
        );
        let validated = if kind == ArtifactKind::test_plan() {
            validate_semantic_test_plan_with_evidence(
                &trace_fragment,
                worker_artifact,
                &repository_root,
                request.semantic_id().digest(),
                result.artifact_traces.get(&kind),
                &declared_test_plan,
                &simulated_test_evidence,
            )?
        } else {
            validate_semantic_artifact(
                &trace_fragment,
                worker_artifact,
                &repository_root,
                request.semantic_id().digest(),
            )?
        };
        store.publish_semantic_artifact(
            &request,
            &trace,
            &validated.artifact,
            &validated.certificate,
        )?;
        artifact_ids.insert(kind, validated.semantic_id);
    }

    let location = result
        .artifacts
        .iter()
        .find(|artifact| artifact.kind() == ArtifactKind::code_location())
        .ok_or_else(|| CalibrationReplayError::Invalid("missing code location".to_owned()))?;
    let locate_request = semantic_request(
        &ArtifactKind::code_location(),
        &locate,
        &locate_fragment,
        locate_route,
        snapshot_digest,
        "Locate where the declared CLI option is implemented.",
    );
    let validated_location = validate_semantic_artifact(
        &locate_fragment,
        location,
        &repository_root,
        locate_request.semantic_id().digest(),
    )?;
    store.publish_semantic_artifact(
        &locate_request,
        &locate,
        &validated_location.artifact,
        &validated_location.certificate,
    )?;

    let resolver = SemanticResolver::new(store);
    let costs = SemanticCostEstimates {
        fresh_microusd: Some(5_000_000),
        artifact_reuse_microusd: Some(100_000),
        claim_reuse_microusd: None,
        claim_partial_reuse_microusd: None,
    };
    let mut cases = vec![
        observe(
            "locate-exact",
            "ExactHit",
            1,
            0,
            true,
            resolver.resolve_for_route(
                &locate,
                &locate_route.route,
                &repository_root,
                snapshot_digest,
                costs,
                &[locate_request.semantic_id().digest()],
            )?,
        ),
        observe(
            "locate-reworded",
            "CoverageHit",
            1,
            0,
            true,
            resolver.resolve_for_route(
                &locate,
                &locate_route.route,
                &repository_root,
                snapshot_digest,
                costs,
                &[],
            )?,
        ),
        observe(
            "trace-composite",
            "CompositeHit",
            2,
            0,
            true,
            resolver.resolve_for_route(
                &trace,
                &trace_route.route,
                &repository_root,
                snapshot_digest,
                costs,
                &[],
            )?,
        ),
        observe(
            "tests-cross-route",
            "CoverageHit",
            1,
            0,
            true,
            resolver.resolve_for_route(
                &tests,
                &tests_route.route,
                &repository_root,
                snapshot_digest,
                costs,
                &[],
            )?,
        ),
    ];

    let irrelevant = repository_root.join("needle-irrelevant.txt");
    fs::write(&irrelevant, "unrelated\n")?;
    let irrelevant_decision = resolver.resolve_for_route(
        &trace,
        &trace_route.route,
        &repository_root,
        snapshot_digest,
        costs,
        &[],
    );
    fs::remove_file(irrelevant)?;
    cases.push(observe("irrelevant-mutation", "CompositeHit", 2, 0, true, irrelevant_decision?));

    let relevant = repository_root.join("crates/core/flags/hiargs.rs");
    let original = fs::read(&relevant)?;
    let mut mutated = original.clone();
    mutated.extend_from_slice(b"\n// deterministic relevant mutation\n");
    fs::write(&relevant, mutated)?;
    let relevant_decision = resolver.resolve_for_route(
        &trace,
        &trace_route.route,
        &repository_root,
        snapshot_digest,
        costs,
        &[],
    );
    fs::write(&relevant, original)?;
    cases.push(observe("relevant-mutation", "PartialHit", 1, 1, false, relevant_decision?));
    cases.push(observe(
        "restored-composite",
        "CompositeHit",
        2,
        0,
        true,
        resolver.resolve_for_route(
            &trace,
            &trace_route.route,
            &repository_root,
            snapshot_digest,
            costs,
            &[],
        )?,
    ));

    let selected_proofs = u32::try_from(cases.len()).unwrap_or(u32::MAX);
    let true_positives =
        u32::try_from(cases.iter().filter(|case| case.passed).count()).unwrap_or(u32::MAX);
    let false_positives = selected_proofs.saturating_sub(true_positives);
    let proof_precision = f64::from(true_positives) / f64::from(selected_proofs);
    let opportunity_rate = f64::from(selected_proofs) / f64::from(selected_proofs);
    Ok((
        CalibrationReplayReport {
            schema_id: CALIBRATION_REPLAY_SCHEMA_ID.to_owned(),
            mode: "shadow".to_owned(),
            provider_calls: 0,
            repository_sha: RIPGREP_CALIBRATION_SHA.to_owned(),
            repository_lineage: repository_lineage.to_string(),
            source_snapshot_digest: snapshot_digest.to_string(),
            worker_schema: result.schema_id.clone(),
            worker_artifacts: u32::try_from(result.artifacts.len()).unwrap_or(u32::MAX),
            validator_revision: needle_runtime::SEMANTIC_VALIDATOR_REVISION,
            validation_certificates: u32::try_from(artifact_ids.len()).unwrap_or(u32::MAX),
            cases,
            selected_proofs,
            true_positives,
            false_positives,
            proof_precision,
            opportunity_rate,
            authoritative_hits: 0,
            workers_avoided: 0,
            live_run_ready: false,
        },
        result,
    ))
}

fn deterministic_test_validation(snapshot_digest: Digest) -> (TestPlan, CommandExecutionEvidence) {
    let argv = vec![
        "cargo".to_owned(),
        "test".to_owned(),
        "--test".to_owned(),
        "integration".to_owned(),
        "misc::glob_always_case_insensitive".to_owned(),
        "--".to_owned(),
        "--exact".to_owned(),
    ];
    let plan = TestPlan {
        runner: "cargo".to_owned(),
        argv: argv.clone(),
        cwd_relative: ".".to_owned(),
        test_identifier: "misc::glob_always_case_insensitive".to_owned(),
        requires_approval: true,
        execution_evidence_id: None,
    };
    let output = "test misc::glob_always_case_insensitive ... ok\n\
        test result: ok. 1 passed; 0 failed";
    let evidence = CommandExecutionEvidence {
        id: format!(
            "command-evidence-calibration-{}",
            Digest::blake3(b"positive-control").to_hex()
        ),
        approval_id: "deterministic-positive-control".to_owned(),
        argv,
        cwd: ".".to_owned(),
        source_snapshot_digest: snapshot_digest,
        runner: "cargo".to_owned(),
        runner_version: Some("deterministic-fixture".to_owned()),
        exit_status: Some(0),
        duration_ms: 1,
        output_digest: Digest::blake3(output),
        output_preview: output.to_owned(),
        test_identifier: Some("misc::glob_always_case_insensitive".to_owned()),
        tests_executed: Some(1),
        infrastructure_failure: None,
    };
    (plan, evidence)
}

fn observe(
    name: &str,
    expected_plan: &str,
    expected_selected_artifacts: u32,
    expected_stale_candidates: u32,
    expect_certificate: bool,
    decision: SemanticReuseDecision,
) -> CalibrationReplayCase {
    let observed_plan = decision
        .plan
        .as_ref()
        .and_then(|plan| plan.decision_reason.split("::").nth(1))
        .unwrap_or("Missing")
        .to_owned();
    let selected_artifacts = u32::try_from(decision.artifacts.len()).unwrap_or(u32::MAX);
    let stale_candidates = u32::try_from(decision.stale_candidates).unwrap_or(u32::MAX);
    let passed = !decision.authoritative
        && matches!(decision.resolution, CacheResolution::Bypass { .. })
        && observed_plan == expected_plan
        && decision.plan.is_some()
        && selected_artifacts == expected_selected_artifacts
        && stale_candidates == expected_stale_candidates
        && decision.certificate.is_some() == expect_certificate;
    CalibrationReplayCase {
        name: name.to_owned(),
        expected_plan: expected_plan.to_owned(),
        observed_plan,
        expected_selected_artifacts,
        selected_artifacts,
        expected_stale_candidates,
        stale_candidates,
        sufficiency_certificate: decision
            .certificate
            .as_ref()
            .map(|certificate| certificate.id.to_string()),
        plan_id: decision.plan.as_ref().map(|plan| plan.id.to_string()).unwrap_or_default(),
        authoritative: decision.authoritative,
        runtime_resolution: resolution_name(&decision.resolution).to_owned(),
        passed,
    }
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

pub(crate) fn semantic_worker_result(
    repository_root: &Path,
) -> Result<SemanticArtifactResult, CalibrationReplayError> {
    let defs = "crates/core/flags/defs.rs";
    let hiargs = "crates/core/flags/hiargs.rs";
    let overrides = "crates/ignore/src/overrides.rs";
    let test = "tests/misc.rs";
    let (start, end) = exact_range(&repository_root.join(defs), RIPGREP_CALIBRATION_SUBJECT)?;
    let artifacts = vec![
        SemanticWorkerArtifact::CodeLocation {
            locations: vec![SemanticLocation {
                role: LocationRole::Primary,
                path: defs.to_owned(),
                symbol: Some("GlobCaseInsensitive".to_owned()),
                byte_start: Some(start),
                byte_end: Some(end),
            }],
            gaps: Vec::new(),
        },
        SemanticWorkerArtifact::BehaviorTrace {
            scenario: "Default CLI search configuration and the --crlf-enabled CRLF search path"
                .to_owned(),
            steps: vec![
                flow_step(FlowStepRole::Producer, defs, "GlobCaseInsensitive"),
                flow_step(FlowStepRole::Carrier, hiargs, "globs"),
                flow_step(
                    FlowStepRole::Transformation,
                    overrides,
                    "OverrideBuilder::case_insensitive",
                ),
                flow_step(FlowStepRole::Precedence, overrides, "Override::matched"),
                flow_step(FlowStepRole::Consumer, test, "glob_always_case_insensitive"),
            ],
            gaps: Vec::new(),
        },
        SemanticWorkerArtifact::TestPlan {
            runner: "cargo".to_owned(),
            argv: vec![
                "cargo".to_owned(),
                "test".to_owned(),
                "--test".to_owned(),
                "integration".to_owned(),
                "misc::glob_always_case_insensitive".to_owned(),
                "--".to_owned(),
                "--exact".to_owned(),
            ],
            cwd_relative: ".".to_owned(),
            identifiers: vec!["misc::glob_always_case_insensitive".to_owned()],
            selection: "representative".to_owned(),
            evidence_paths: vec![test.to_owned()],
        },
    ];
    let mut artifact_traces = BTreeMap::new();
    artifact_traces.insert(ArtifactKind::code_location(), trace(&[defs]));
    artifact_traces.insert(ArtifactKind::behavior_trace(), trace(&[defs, hiargs, overrides, test]));
    artifact_traces.insert(ArtifactKind::test_plan(), trace(&[test]));
    Ok(SemanticArtifactResult {
        schema_id: needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID.to_owned(),
        artifacts,
        observation_trace: trace(&EVIDENCE_FILES),
        artifact_traces,
    })
}

fn flow_step(role: FlowStepRole, path: &str, symbol: &str) -> SemanticFlowStep {
    SemanticFlowStep {
        role,
        location: SemanticLocation {
            role: LocationRole::Supporting,
            path: path.to_owned(),
            symbol: Some(symbol.to_owned()),
            byte_start: None,
            byte_end: None,
        },
        description: format!("{role:?} evidence"),
    }
}

fn trace(files: &[&str]) -> WorkerObservationTrace {
    WorkerObservationTrace {
        observed_files: files.iter().map(|path| (*path).to_owned()).collect(),
        gaps: Vec::new(),
    }
}

fn semantic_request(
    kind: &ArtifactKind,
    need: &Need,
    fragment: &NeedFragment,
    route: &RouteContract,
    snapshot: Digest,
    wording: &str,
) -> ArtifactRequest {
    ArtifactRequest {
        contract_id: format!("needle.semantic.{}", kind.0),
        contract_revision: 2,
        repository_id: need.world.repository_lineage,
        source_snapshot_digest: snapshot,
        route_key: route.route.clone(),
        normalized_request: wording.to_owned(),
        semantic_fragment_id: Some(fragment.id),
        input_artifact_ids: Vec::new(),
    }
}

fn record_need(
    store: &RuntimeStore,
    label: &str,
    ir: &NeedIr,
    need: &Need,
    fragment: &NeedFragment,
) -> Result<(), needle_runtime::StoreError> {
    store.record_need_shadow(NeedShadowWrite {
        session_id: label,
        turn_id: "turn",
        transport_digest: Digest::blake3(format!("{label}-transport").as_bytes()),
        parser_definition_digest: needle_core::need_grammar_definition_digest(),
        prompt_profile_digest: Digest::blake3(b"v04-calibration-profile"),
        need_ir: ir,
        need,
        fragments: std::slice::from_ref(fragment),
    })
}

fn parse_need(marker: String) -> Result<NeedIr, CalibrationReplayError> {
    NeedIr::parse(&marker)
        .map_err(|error| CalibrationReplayError::Invalid(error.to_string()))?
        .ok_or_else(|| CalibrationReplayError::Invalid("missing @@need marker".to_owned()))
}

fn route<'a>(
    routes: &'a [RouteContract],
    key: &str,
) -> Result<&'a RouteContract, CalibrationReplayError> {
    routes
        .iter()
        .find(|route| route.route.as_str() == key)
        .ok_or_else(|| CalibrationReplayError::Invalid(format!("missing route {key}")))
}

fn copy_evidence_files(
    source_repository: &Path,
    destination: &Path,
) -> Result<(), CalibrationReplayError> {
    for relative in EVIDENCE_FILES {
        let source = source_repository.join(relative);
        if !source.is_file() {
            return Err(CalibrationReplayError::Invalid(format!(
                "missing source evidence {relative}"
            )));
        }
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, target)?;
    }
    Ok(())
}

fn evidence_snapshot_digest(repository_root: &Path) -> Result<Digest, CalibrationReplayError> {
    let mut hasher = needle_core::CanonicalHasher::new(b"calibration-source-snapshot");
    for relative in EVIDENCE_FILES {
        hasher.field_str(relative);
        hasher.field_digest(Digest::blake3(fs::read(repository_root.join(relative))?));
    }
    Ok(hasher.finish())
}

fn exact_range(path: &Path, needle: &str) -> Result<(u64, u64), CalibrationReplayError> {
    let bytes = fs::read(path)?;
    let needle = needle.as_bytes();
    let start =
        bytes.windows(needle.len()).position(|window| window == needle).ok_or_else(|| {
            CalibrationReplayError::Invalid(format!(
                "{} does not contain the calibration subject",
                path.display()
            ))
        })?;
    let end = start.saturating_add(needle.len());
    Ok((u64::try_from(start).unwrap_or(u64::MAX), u64::try_from(end).unwrap_or(u64::MAX)))
}

fn locate_marker() -> String {
    format!(
        "@@need\n\
@route locate.implementation\n\
@subject cli-option:\"{RIPGREP_CALIBRATION_SUBJECT}\"\n\
@require implementation-location selection=primary granularity=exact-location polarity=positive\n\
@world source=current features=default\n\
\n\
Locate the implementation evidence required for continuation.\n\
@@end"
    )
}

fn trace_marker() -> String {
    format!(
        "@@need\n\
@route trace.state-flow\n\
@subject cli-option:\"{RIPGREP_CALIBRATION_SUBJECT}\"\n\
@require implementation-location selection=primary granularity=exact-location polarity=positive\n\
@require runtime-flow scenario=default completeness=contract-complete granularity=stepwise\n\
@prefer focused-tests selection=representative completeness=open-world polarity=positive\n\
@world source=current features=default\n\
\n\
Trace the runtime flow and return bounded evidence.\n\
@@end"
    )
}

fn tests_marker() -> String {
    format!(
        "@@need\n\
@route tests.relevant\n\
@subject cli-option:\"{RIPGREP_CALIBRATION_SUBJECT}\"\n\
@require focused-tests selection=representative completeness=open-world polarity=positive\n\
@world source=current features=default\n\
\n\
Identify the representative focused test.\n\
@@end"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_control_selects_every_expected_shadow_plan_without_authority() {
        let root = std::env::temp_dir().join(format!(
            "needle-positive-calibration-{}",
            Digest::blake3(format!("{:?}", std::time::Instant::now())).to_hex()
        ));
        let source = root.join("source");
        for relative in EVIDENCE_FILES {
            let path = source.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!(
                    "{RIPGREP_CALIBRATION_SUBJECT}\n\
                     GlobCaseInsensitive globs OverrideBuilder case_insensitive Override matched \
                     glob_always_case_insensitive\n"
                ),
            )
            .unwrap();
        }
        let (report, result) =
            run_positive_calibration_replay(&source, &root.join("artifacts")).unwrap();
        assert_eq!(result.schema_id, "needle.artifact-result/2");
        assert_eq!(report.selected_proofs, 7);
        assert_eq!(report.true_positives, 7);
        assert_eq!(report.false_positives, 0);
        assert_eq!(report.proof_precision, 1.0);
        assert_eq!(report.opportunity_rate, 1.0);
        assert_eq!(report.authoritative_hits, 0);
        assert_eq!(report.workers_avoided, 0);
        assert!(!report.live_run_ready);
        assert!(report.cases.iter().all(|case| case.passed));
        let relevant = report.cases.iter().find(|case| case.name == "relevant-mutation").unwrap();
        assert_eq!(relevant.observed_plan, "PartialHit");
        assert!(relevant.stale_candidates > 0);
        let _ = fs::remove_dir_all(root);
    }
}
