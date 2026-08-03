use crate::app_server::{
    AppServerSession, AppServerTurn, AppServerTurnFailure, CapturedTestEvidence,
};
use crate::worker::CodexWorker;
use needle_core::claim::ClaimPayload;
use needle_core::{
    AcceptanceCoverage, AcceptanceStatus, ChangeId, Digest, MAX_VERIFIER_TEST_PLANS, PatchArtifact,
    SemanticWorkerArtifact, TestPlan, VerificationArtifact, VerificationPlanResult,
    VerificationStatus, WorkerConfig,
};
use needle_runtime::{
    IsolatedCheckout, RuntimeStore, artifact_and_certificate_are_fresh, capture_git_snapshot,
    claim_validation_certificate_is_fresh, materialize_patch_artifact, validate_test_evidence,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_VERIFIER_FINDINGS: usize = 16;
const MAX_VERIFIER_TEXT_BYTES: usize = 2 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyChangeOutcome {
    pub artifact: VerificationArtifact,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub duration_ms: u64,
    pub verifier_started: bool,
}

#[derive(Clone, Debug)]
pub struct CodexVerifier {
    data_directory: PathBuf,
    codex_home: Option<PathBuf>,
    cancellation: Option<Arc<AtomicBool>>,
}

impl CodexVerifier {
    pub fn new(data_directory: impl Into<PathBuf>) -> Self {
        Self { data_directory: data_directory.into(), codex_home: None, cancellation: None }
    }

    pub fn with_codex_home(
        data_directory: impl Into<PathBuf>,
        codex_home: impl Into<PathBuf>,
    ) -> Self {
        Self {
            data_directory: data_directory.into(),
            codex_home: Some(codex_home.into()),
            cancellation: None,
        }
    }

    pub fn with_cancellation(mut self, cancellation: Arc<AtomicBool>) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub fn verify(
        &self,
        config: &WorkerConfig,
        repository_root: &Path,
        change_id: &ChangeId,
    ) -> Result<VerifyChangeOutcome, String> {
        let started = Instant::now();
        let store = RuntimeStore::new(self.data_directory.join("needle.sqlite3"));
        store.initialize().map_err(|error| error.to_string())?;
        let prepared = store
            .prepared_change(change_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("change `{change_id}` does not exist"))?;
        if prepared.state == "failed" {
            return Err(format!("change `{change_id}` failed closed and cannot be verified"));
        }
        let definition = verifier_definition();
        if let Some(cached) = store
            .latest_verification_artifact(change_id)
            .map_err(|error| error.to_string())?
            .filter(|artifact| {
                artifact.patch_id == prepared.patch.id
                    && artifact.verifier_definition == definition
                    && matches!(
                        artifact.verdict,
                        VerificationStatus::Verified | VerificationStatus::Rejected
                    )
            })
        {
            return Ok(VerifyChangeOutcome {
                artifact: cached,
                input_tokens: Some(0),
                cached_input_tokens: Some(0),
                output_tokens: Some(0),
                duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                verifier_started: false,
            });
        }
        if prepared.state == "repairing" {
            return Err(format!(
                "change `{change_id}` has a repair in progress and cannot be verified"
            ));
        }
        let isolation = CodexWorker::verify_isolation(&config.executable)?;
        if !isolation.verified() {
            return Err(format!(
                "verifier isolation is not verified for Codex {}",
                isolation.codex_version
            ));
        }
        let repository_root = fs::canonicalize(repository_root)
            .map_err(|error| format!("cannot resolve verification repository: {error}"))?;
        let sandbox = IsolatedCheckout::materialize(
            &repository_root,
            &self.data_directory.join("verification-runs"),
        )
        .map_err(|error| error.to_string())?;
        if sandbox.snapshot().source_digest != prepared.source_snapshot {
            return cleanup_sandbox_after_error(
                sandbox,
                "active source snapshot changed after patch preparation".to_owned(),
            );
        }
        let blobs = match store.patch_file_blobs(prepared.patch.id) {
            Ok(blobs) => blobs,
            Err(error) => return cleanup_sandbox_after_error(sandbox, error.to_string()),
        };
        if let Err(error) =
            materialize_patch_artifact(sandbox.checkout_root(), &prepared.patch, &blobs)
        {
            return cleanup_sandbox_after_error(sandbox, error.to_string());
        }
        let (_, patched_snapshot) = match capture_git_snapshot(sandbox.checkout_root()) {
            Ok(snapshot) => snapshot,
            Err(error) => return cleanup_sandbox_after_error(sandbox, error.to_string()),
        };
        let settings = store.settings().map_err(|error| error.to_string())?;
        let associated = match associated_test_plan(
            &store,
            &prepared.request,
            &prepared.patch,
            &repository_root,
        ) {
            Ok(associated) => associated,
            Err(error) => return cleanup_sandbox_after_error(sandbox, error),
        };
        let mut associated_plans = associated.plans.clone();
        if !settings.trusted_test_execution {
            for entry in &mut associated_plans {
                entry.available = false;
                entry.unavailable_reason =
                    Some("the repository is not trusted for test execution".to_owned());
            }
        }
        let executable_plans = (!associated.over_cap && settings.trusted_test_execution)
            .then(|| {
                associated_plans
                    .iter()
                    .filter(|entry| entry.available)
                    .map(|entry| entry.plan.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let instructions = verifier_instructions(
            &associated_plans,
            associated.expected,
            associated.over_cap,
            associated.unavailable_reason.as_deref(),
        );
        let mut session = match AppServerSession::start_verifier(
            config,
            self.codex_home.as_deref(),
            &instructions,
            sandbox.checkout_root(),
            sandbox.target_root(),
            sandbox.temp_root(),
            patched_snapshot.source_digest,
            patched_snapshot.repository_id,
            executable_plans,
            settings.trusted_test_execution,
            store.clone(),
        ) {
            Ok(session) => session,
            Err(error) => return cleanup_sandbox_after_error(sandbox, error),
        };
        session.fail_fast_on_pending_approvals();
        let turn = session.run_turn_cancellable(
            &verifier_prompt(&prepared.request, &prepared.patch),
            &verifier_output_schema(),
            Duration::from_secs(config.timeout_seconds),
            self.cancellation.as_deref(),
        );
        let mut unavailable_reason = associated.unavailable_reason.clone();
        if !session.test_execution_available() && !associated_plans.is_empty() {
            let reason = session
                .test_execution_unavailable_reason()
                .unwrap_or("test execution was unavailable in the verifier session")
                .to_owned();
            for entry in &mut associated_plans {
                if entry.available {
                    entry.available = false;
                    entry.unavailable_reason = Some(reason.clone());
                }
            }
            unavailable_reason = Some(match unavailable_reason.take() {
                Some(previous) => format!("{previous}; {reason}"),
                None => reason,
            });
        }
        let (
            declared,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            evidence_ids,
            plan_results,
        ) = normalize_verifier_turn(
            turn,
            &prepared.request.acceptance_criteria,
            &associated_plans,
            associated.expected,
            associated.over_cap,
            unavailable_reason.as_deref(),
            sandbox.checkout_root(),
            patched_snapshot.source_digest,
        );
        if let Err(error) = session.cleanup() {
            return cleanup_sandbox_after_error(
                sandbox,
                format!("verifier App Server cleanup failed: {error}"),
            );
        }
        let (_, after_verifier) = match capture_git_snapshot(sandbox.checkout_root()) {
            Ok(snapshot) => snapshot,
            Err(error) => return cleanup_sandbox_after_error(sandbox, error.to_string()),
        };
        if after_verifier.source_digest != patched_snapshot.source_digest {
            return cleanup_sandbox_after_error(
                sandbox,
                "read-only verifier changed the patched checkout".to_owned(),
            );
        }
        if let Err(error) = sandbox.cleanup() {
            return Err(format!("verifier checkout cleanup failed closed: {error}"));
        }
        let created_unix_ms = now_ms();
        let artifact = VerificationArtifact {
            id: VerificationArtifact::compute_id_with_plan_results_and_cap(
                change_id,
                prepared.patch.id,
                declared.verdict,
                &declared.acceptance_coverage,
                &declared.findings,
                &evidence_ids,
                &plan_results,
                associated.over_cap,
                definition,
            ),
            change_id: change_id.clone(),
            patch_id: prepared.patch.id,
            verdict: declared.verdict,
            acceptance_coverage: declared.acceptance_coverage,
            findings: declared.findings,
            test_evidence_ids: evidence_ids,
            test_plan_results: plan_results.clone(),
            test_plans_over_cap: associated.over_cap,
            verifier_definition: definition,
            created_unix_ms,
        };
        let attempt = json!({
            "model": config.model,
            "reasoning": config.reasoning,
            "service_tier": config.service_tier,
            "codex_version": isolation.codex_version,
            "verifier_started": true,
            "test_plans_over_cap": associated.over_cap,
            "test_plan_results": plan_results,
        });
        let usage = json!({
            "input_tokens": input_tokens,
            "cached_input_tokens": cached_input_tokens,
            "output_tokens": output_tokens
        });
        store
            .record_verification_artifact(&artifact, &attempt, &usage, None)
            .map_err(|error| error.to_string())?;
        Ok(VerifyChangeOutcome {
            artifact,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            verifier_started: true,
        })
    }

    pub fn record_inconclusive(
        &self,
        change_id: &ChangeId,
        reason: &str,
    ) -> Result<VerifyChangeOutcome, String> {
        let store = RuntimeStore::new(self.data_directory.join("needle.sqlite3"));
        store.initialize().map_err(|error| error.to_string())?;
        let prepared = store
            .prepared_change(change_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("change `{change_id}` does not exist"))?;
        let finding = bounded_text(reason, MAX_VERIFIER_TEXT_BYTES);
        let acceptance_coverage = prepared
            .request
            .acceptance_criteria
            .iter()
            .map(|criterion| AcceptanceCoverage {
                criterion: criterion.clone(),
                status: AcceptanceStatus::Unaddressed,
                evidence: "verification orchestration did not complete".to_owned(),
            })
            .collect::<Vec<_>>();
        let findings = vec![finding];
        let definition = verifier_definition();
        let previous_created_unix_ms = store
            .latest_verification_artifact(change_id)
            .map_err(|error| error.to_string())?
            .filter(|artifact| artifact.patch_id == prepared.patch.id)
            .map(|artifact| artifact.created_unix_ms)
            .unwrap_or(0);
        let created_unix_ms = now_ms().max(previous_created_unix_ms.saturating_add(1));
        let artifact = VerificationArtifact {
            id: VerificationArtifact::compute_id(
                change_id,
                prepared.patch.id,
                VerificationStatus::Inconclusive,
                &acceptance_coverage,
                &findings,
                &[],
                definition,
            ),
            change_id: change_id.clone(),
            patch_id: prepared.patch.id,
            verdict: VerificationStatus::Inconclusive,
            acceptance_coverage,
            findings,
            test_evidence_ids: Vec::new(),
            test_plan_results: Vec::new(),
            test_plans_over_cap: false,
            verifier_definition: definition,
            created_unix_ms,
        };
        store
            .record_verification_artifact(
                &artifact,
                &json!({"phase": "repair_orchestration", "verifier_started": false}),
                &json!({
                    "input_tokens": 0,
                    "cached_input_tokens": 0,
                    "output_tokens": 0
                }),
                None,
            )
            .map_err(|error| error.to_string())?;
        Ok(VerifyChangeOutcome {
            artifact,
            input_tokens: Some(0),
            cached_input_tokens: Some(0),
            output_tokens: Some(0),
            duration_ms: 0,
            verifier_started: false,
        })
    }
}

#[derive(Clone, Debug)]
struct AssociatedTestPlan {
    plans: Vec<AssociatedPlan>,
    expected: bool,
    unavailable_reason: Option<String>,
    over_cap: bool,
}

#[derive(Clone, Debug)]
struct AssociatedPlan {
    plan: TestPlan,
    available: bool,
    unavailable_reason: Option<String>,
}

fn associated_test_plan(
    store: &RuntimeStore,
    request: &needle_core::ChangeRequest,
    patch: &PatchArtifact,
    repository_root: &Path,
) -> Result<AssociatedTestPlan, String> {
    let changed = patch.files.iter().map(|file| file.path.as_str()).collect::<BTreeSet<_>>();
    let mut plans = BTreeMap::<Digest, AssociatedPlan>::new();
    let mut expected = false;
    let mut unavailable = Vec::new();
    for id in &request.artifact_ids {
        let Some(artifact) =
            store.semantic_artifact(&id.to_string()).map_err(|error| error.to_string())?
        else {
            continue;
        };
        let Ok(payload) =
            serde_json::from_value::<SemanticWorkerArtifact>(artifact.payload.clone())
        else {
            continue;
        };
        let SemanticWorkerArtifact::TestPlan {
            runner,
            argv,
            cwd_relative,
            identifiers,
            evidence_paths,
            ..
        } = payload
        else {
            continue;
        };
        expected = true;
        if identifiers.len() != 1 {
            unavailable
                .push(format!("artifact test plan `{id}` must contain exactly one identifier"));
            continue;
        }
        let plan = plan_from_parts(runner, argv, cwd_relative, identifiers);
        let mut reasons = Vec::new();
        let certificate = store
            .validation_certificate_for_artifact(&id.to_string())
            .map_err(|error| error.to_string())?;
        if certificate.as_ref().is_none_or(|certificate| {
            !artifact_and_certificate_are_fresh(&artifact, certificate, repository_root)
        }) {
            reasons.push(format!("artifact test plan `{id}` has no fresh certificate"));
        }
        if artifact
            .dependency_manifest
            .dependencies
            .iter()
            .any(|dependency| changed.contains(dependency.path.as_str()))
            || evidence_paths.iter().any(|path| changed.contains(path.as_str()))
        {
            reasons.push(format!("artifact test plan `{id}` changed or became stale"));
        }
        if plan.test_command().is_err() {
            reasons.push(format!("artifact test plan `{id}` is unsupported or unsafe"));
        }
        let available = reasons.is_empty();
        unavailable.extend(reasons.iter().cloned());
        insert_associated_plan(&mut plans, plan, available, reasons);
    }
    for id in &request.claim_ids {
        let Some(claim) = store.semantic_claim(*id).map_err(|error| error.to_string())? else {
            continue;
        };
        let ClaimPayload::FocusedTest {
            runner,
            argv,
            cwd_relative,
            identifier,
            evidence_paths,
            ..
        } = claim.payload
        else {
            continue;
        };
        expected = true;
        let plan = plan_from_parts(runner, argv, cwd_relative, vec![identifier]);
        let mut reasons = Vec::new();
        let certificate =
            store.claim_validation_certificate_for_claim(*id).map_err(|error| error.to_string())?;
        if certificate.as_ref().is_none_or(|certificate| {
            !claim_validation_certificate_is_fresh(certificate, repository_root)
        }) {
            reasons.push(format!("claim test plan `{id}` has no fresh certificate"));
        }
        if certificate.as_ref().is_some_and(|certificate| {
            certificate
                .dependencies
                .iter()
                .any(|dependency| changed.contains(dependency.path.as_str()))
        }) || evidence_paths.iter().any(|path| changed.contains(path.as_str()))
        {
            reasons.push(format!("claim test plan `{id}` changed or became stale"));
        }
        if plan.test_command().is_err() {
            reasons.push(format!("claim test plan `{id}` is unsupported or unsafe"));
        }
        let available = reasons.is_empty();
        unavailable.extend(reasons.iter().cloned());
        insert_associated_plan(&mut plans, plan, available, reasons);
    }
    let over_cap = plans.len() > MAX_VERIFIER_TEST_PLANS;
    if over_cap {
        unavailable.push(format!(
            "{} distinct certified test plans exceed the verifier bound of {}",
            plans.len(),
            MAX_VERIFIER_TEST_PLANS
        ));
    }
    let unavailable_reason = (!unavailable.is_empty())
        .then(|| bounded_text(&unavailable.join("; "), MAX_VERIFIER_TEXT_BYTES));
    Ok(AssociatedTestPlan {
        plans: plans.into_values().collect(),
        expected,
        unavailable_reason,
        over_cap,
    })
}

fn plan_from_parts(
    runner: String,
    argv: Vec<String>,
    cwd_relative: String,
    identifiers: Vec<String>,
) -> TestPlan {
    TestPlan {
        runner,
        argv,
        cwd_relative,
        test_identifier: identifiers.into_iter().next().unwrap_or_default(),
        requires_approval: true,
        execution_evidence_id: None,
    }
}

fn insert_associated_plan(
    plans: &mut BTreeMap<Digest, AssociatedPlan>,
    plan: TestPlan,
    available: bool,
    reasons: Vec<String>,
) {
    let digest = plan.identity_digest();
    let reason =
        (!reasons.is_empty()).then(|| bounded_text(&reasons.join("; "), MAX_VERIFIER_TEXT_BYTES));
    if let Some(existing) = plans.get_mut(&digest) {
        existing.available &= available;
        if let Some(reason) = reason {
            let merged = match existing.unavailable_reason.take() {
                Some(previous) => format!("{previous}; {reason}"),
                None => reason,
            };
            existing.unavailable_reason = Some(bounded_text(&merged, MAX_VERIFIER_TEXT_BYTES));
        }
    } else {
        plans.insert(digest, AssociatedPlan { plan, available, unavailable_reason: reason });
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredVerification {
    verdict: VerificationStatus,
    acceptance_coverage: Vec<AcceptanceCoverage>,
    findings: Vec<String>,
}

fn normalize_verifier_turn(
    turn: Result<AppServerTurn, AppServerTurnFailure>,
    requested_criteria: &[String],
    plans: &[AssociatedPlan],
    test_expected: bool,
    over_cap: bool,
    unavailable_reason: Option<&str>,
    checkout_root: &Path,
    snapshot_digest: Digest,
) -> (
    DeclaredVerification,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Vec<String>,
    Vec<VerificationPlanResult>,
) {
    let turn = match turn {
        Ok(turn) => turn,
        Err(failure) => {
            let plan_results =
                plan_results(plans, &[], None, Some(&failure.diagnostic), over_cap, None);
            let mut declared = inconclusive_verification(&failure.diagnostic);
            if test_expected {
                append_authoritative_finding(
                    &mut declared,
                    unavailable_reason.unwrap_or(
                        "one or more associated focused tests lacked complete execution evidence",
                    ),
                );
            }
            if over_cap {
                append_authoritative_finding(
                    &mut declared,
                    "the verifier test-plan bound was exceeded; no truncated subset was authorized",
                );
            }
            return (
                declared,
                failure.input_tokens,
                failure.cached_input_tokens,
                failure.output_tokens,
                Vec::new(),
                plan_results,
            );
        }
    };
    let mut declared = serde_json::from_value::<DeclaredVerification>(turn.response)
        .unwrap_or_else(|error| {
            inconclusive_verification(&format!("invalid verifier output: {error}"))
        });
    if declared.findings.len() > MAX_VERIFIER_FINDINGS
        || declared.findings.iter().any(|finding| finding.len() > MAX_VERIFIER_TEXT_BYTES)
        || declared.acceptance_coverage.iter().any(|coverage| {
            coverage.criterion.len() > MAX_VERIFIER_TEXT_BYTES
                || coverage.evidence.len() > MAX_VERIFIER_TEXT_BYTES
        })
    {
        declared = inconclusive_verification("verifier output exceeded bounded fields");
    }
    let covered = declared
        .acceptance_coverage
        .iter()
        .map(|coverage| coverage.criterion.as_str())
        .collect::<BTreeSet<_>>();
    let requested = requested_criteria.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if covered != requested
        || declared.acceptance_coverage.len() != requested_criteria.len()
        || declared.verdict == VerificationStatus::NotRequested
    {
        declared = inconclusive_verification(
            "verifier did not account for every acceptance criterion exactly once",
        );
    }
    let plan_results = plan_results(
        plans,
        &turn.test_evidence,
        Some(checkout_root),
        None,
        over_cap,
        Some(snapshot_digest),
    );
    let evidence_ids =
        plan_results.iter().filter_map(|result| result.evidence_id.clone()).collect::<Vec<_>>();
    let expected_plans_valid = !test_expected
        || (!plans.is_empty()
            && !over_cap
            && plan_results.iter().all(|result| {
                !result.expected || (result.available && result.executed && result.passed)
            })
            && evidence_ids.len() == plan_results.iter().filter(|result| result.expected).count());
    if test_expected && !expected_plans_valid {
        let reason = unavailable_reason
            .unwrap_or("one or more associated focused tests lacked complete execution evidence");
        if matches!(declared.verdict, VerificationStatus::Verified | VerificationStatus::Repairable)
        {
            declared.verdict = VerificationStatus::Inconclusive;
        }
        append_authoritative_finding(&mut declared, reason);
    }
    if over_cap {
        declared.verdict = VerificationStatus::Inconclusive;
        append_authoritative_finding(
            &mut declared,
            "the verifier test-plan bound was exceeded; no truncated subset was authorized",
        );
    }
    if test_expected
        && turn.test_evidence.iter().any(|evidence| {
            !plans.iter().any(|plan| plan.plan.identity_digest() == evidence.plan_digest)
        })
    {
        declared.verdict = VerificationStatus::Inconclusive;
        append_authoritative_finding(
            &mut declared,
            "runtime captured test evidence for an undeclared plan",
        );
    }
    if declared.verdict == VerificationStatus::Verified
        && (declared
            .acceptance_coverage
            .iter()
            .any(|coverage| coverage.status != AcceptanceStatus::Addressed)
            || !declared.findings.is_empty()
            || !turn.observation_trace.gaps.is_empty())
    {
        declared.verdict = VerificationStatus::Inconclusive;
        declared
            .findings
            .push("verified verdict lacked complete acceptance or observation evidence".to_owned());
    }
    (
        declared,
        turn.input_tokens,
        turn.cached_input_tokens,
        turn.output_tokens,
        evidence_ids,
        plan_results,
    )
}

fn append_authoritative_finding(declared: &mut DeclaredVerification, reason: &str) {
    let reason = bounded_text(reason, MAX_VERIFIER_TEXT_BYTES);
    if declared.findings.iter().any(|finding| finding == &reason) {
        return;
    }
    if declared.findings.len() >= MAX_VERIFIER_FINDINGS {
        declared.findings.pop();
    }
    declared.findings.push(reason);
}

fn plan_results(
    plans: &[AssociatedPlan],
    captured: &[CapturedTestEvidence],
    checkout_root: Option<&Path>,
    turn_failure: Option<&str>,
    over_cap: bool,
    snapshot_digest: Option<Digest>,
) -> Vec<VerificationPlanResult> {
    if over_cap {
        // Never serialize a truncated subset: the complete over-cap set is
        // represented by the bounded top-level inconclusive finding.
        return Vec::new();
    }
    let mut used_evidence = BTreeSet::new();
    plans
        .iter()
        .map(|entry| {
            let digest = entry.plan.identity_digest();
            let matching = captured
                .iter()
                .filter(|captured| captured.plan_digest == digest)
                .collect::<Vec<_>>();
            let evidence = matching.first().map(|captured| &captured.evidence);
            let mut reason = entry.unavailable_reason.clone();
            if let Some(failure) = turn_failure {
                reason = Some(bounded_text(
                    &match reason.take() {
                        Some(previous) => format!("{previous}; {failure}"),
                        None => failure.to_owned(),
                    },
                    MAX_VERIFIER_TEXT_BYTES,
                ));
            }
            let executed = !matching.is_empty();
            let mut passed = false;
            let mut evidence_id = evidence.map(|evidence| evidence.id.clone());
            if matching.len() > 1 {
                reason =
                    Some("multiple evidence items were captured for one declared plan".to_owned());
            } else if let Some(evidence) = evidence {
                if !used_evidence.insert(evidence.id.clone()) {
                    evidence_id = None;
                    reason =
                        Some("one evidence item was associated with multiple plans".to_owned());
                } else if !entry.available || over_cap {
                    reason = reason.or_else(|| Some("plan was unavailable".to_owned()));
                } else if snapshot_digest
                    .is_some_and(|snapshot| evidence.source_snapshot_digest != snapshot)
                {
                    reason = Some(
                        "captured command evidence snapshot did not match the patched checkout"
                            .to_owned(),
                    );
                } else if validate_test_evidence(&entry.plan, evidence).is_err() {
                    reason =
                        Some("captured command evidence failed focused-test validation".to_owned());
                } else if let Some(root) = checkout_root {
                    let expected_cwd = root.join(&entry.plan.cwd_relative);
                    let cwd_matches = std::fs::canonicalize(&expected_cwd)
                        .ok()
                        .zip(std::fs::canonicalize(&evidence.cwd).ok())
                        .is_some_and(|(expected, actual)| expected == actual);
                    if !cwd_matches {
                        reason =
                            Some("captured command evidence cwd did not match the plan".to_owned());
                    } else {
                        passed = true;
                    }
                } else {
                    passed = true;
                }
            } else if reason.is_none() {
                reason = Some("missing command evidence for the declared plan".to_owned());
            }
            VerificationPlanResult {
                plan_digest: digest,
                runner: entry.plan.runner.clone(),
                argv: entry.plan.argv.clone(),
                cwd_relative: entry.plan.cwd_relative.clone(),
                test_identifier: entry.plan.test_identifier.clone(),
                expected: true,
                available: entry.available,
                executed,
                passed,
                evidence_id,
                failure_reason: reason.map(|reason| bounded_text(&reason, MAX_VERIFIER_TEXT_BYTES)),
            }
        })
        .collect()
}

fn inconclusive_verification(reason: &str) -> DeclaredVerification {
    DeclaredVerification {
        verdict: VerificationStatus::Inconclusive,
        acceptance_coverage: Vec::new(),
        findings: vec![reason.chars().take(MAX_VERIFIER_TEXT_BYTES).collect()],
    }
}

fn verifier_instructions(
    plans: &[AssociatedPlan],
    test_expected: bool,
    over_cap: bool,
    unavailable_reason: Option<&str>,
) -> String {
    let mut test = if over_cap {
        format!(
            "The complete certified plan set contains {} plans, exceeding the hard bound of {MAX_VERIFIER_TEST_PLANS}; no test command is authorized and no subset may be executed.",
            plans.len()
        )
    } else if plans.is_empty() {
        if test_expected {
            format!(
                "A related test exists but no complete certified plan is available ({}). Do not run tests and return inconclusive if test evidence is necessary.",
                bounded_text(
                    unavailable_reason.unwrap_or("unavailable certified plan"),
                    MAX_VERIFIER_TEXT_BYTES
                )
            )
        } else {
            "No certified focused test is associated with this change. Do not run tests; perform bounded static verification.".to_owned()
        }
    } else {
        "Certified focused plans are ordered below. Execute each available plan exactly once, sequentially, using the exact runner, argv, cwd, and identifier. Never execute an unavailable plan; any unavailable expected plan forces an inconclusive verdict.".to_owned()
    };
    if !over_cap {
        test.push_str("\nOrdered certified plans:\n");
        for (index, entry) in plans.iter().enumerate() {
            let reason = entry
                .unavailable_reason
                .as_deref()
                .map(|reason| format!(" reason={}", bounded_text(reason, MAX_VERIFIER_TEXT_BYTES)))
                .unwrap_or_default();
            test.push_str(&format!(
                "- {}: digest={} runner={} argv={:?} cwd_relative={} test_identifier={} available={}{}\n",
                index + 1,
                entry.plan.identity_digest(),
                entry.plan.runner,
                entry.plan.argv,
                entry.plan.cwd_relative,
                entry.plan.test_identifier,
                entry.available,
                reason
            ));
        }
    }
    format!(
        "You are Needle's independent verifier. The patch is already materialized in a read-only disposable checkout. You have no patcher transcript and must not modify files. Check the actual filesystem against every acceptance criterion. Use only bounded repository inspection, no network, credentials, hooks, plugins, MCP, project instructions, or other agents. {test} Return only JSON matching the output schema."
    )
}

fn verifier_prompt(request: &needle_core::ChangeRequest, patch: &PatchArtifact) -> String {
    let mut prompt =
        format!("Verify this task independently:\n{}\n\nAcceptance criteria:\n", request.task);
    for criterion in &request.acceptance_criteria {
        prompt.push_str("- ");
        prompt.push_str(criterion);
        prompt.push('\n');
    }
    prompt.push_str("\nAuthoritative changed-file manifest:\n");
    for file in &patch.files {
        prompt.push_str(&format!("- {} ({:?})\n", file.path, file.operation));
    }
    prompt
}

fn verifier_output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["verdict", "acceptance_coverage", "findings"],
        "properties": {
            "verdict": {"type": "string", "enum": ["verified", "rejected", "repairable", "inconclusive"]},
            "acceptance_coverage": {
                "type": "array", "maxItems": 8,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["criterion", "status", "evidence"],
                    "properties": {
                        "criterion": {"type": "string", "maxLength": MAX_VERIFIER_TEXT_BYTES},
                        "status": {"type": "string", "enum": ["addressed", "partial", "unaddressed"]},
                        "evidence": {"type": "string", "maxLength": MAX_VERIFIER_TEXT_BYTES}
                    }
                }
            },
            "findings": {
                "type": "array", "maxItems": MAX_VERIFIER_FINDINGS,
                "items": {"type": "string", "maxLength": MAX_VERIFIER_TEXT_BYTES}
            }
        }
    })
}

fn verifier_definition() -> Digest {
    let mut hasher = needle_core::CanonicalHasher::new(b"needle-verifier-definition-v2");
    hasher.field_str("independent-read-only-v2");
    hasher.field_u16(MAX_VERIFIER_TEST_PLANS as u16);
    hasher.field_str("ordered-plan-results");
    hasher.field_str("runner argv cwd_relative test_identifier expected available executed passed evidence_id failure_reason");
    hasher.field_bytes(&serde_json::to_vec(&verifier_output_schema()).unwrap_or_default());
    hasher.finish()
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn cleanup_sandbox_after_error<T>(sandbox: IsolatedCheckout, error: String) -> Result<T, String> {
    match sandbox.cleanup() {
        Ok(()) => Err(error),
        Err(cleanup) => Err(format!("{error}; verifier checkout cleanup failed: {cleanup}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(response: Value) -> AppServerTurn {
        AppServerTurn {
            response,
            input_tokens: Some(10),
            cached_input_tokens: Some(2),
            output_tokens: Some(3),
            approval_wait: Duration::ZERO,
            command_evidence: Vec::new(),
            test_evidence: Vec::new(),
            observation_trace: Default::default(),
            file_change_approvals_granted: 0,
        }
    }

    fn plan(identifier: &str) -> TestPlan {
        TestPlan {
            runner: "cargo".to_owned(),
            argv: vec![
                "cargo".to_owned(),
                "test".to_owned(),
                identifier.to_owned(),
                "--".to_owned(),
                "--exact".to_owned(),
            ],
            cwd_relative: ".".to_owned(),
            test_identifier: identifier.to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        }
    }

    fn evidence(plan: &TestPlan, id: &str, snapshot: Digest) -> CapturedTestEvidence {
        CapturedTestEvidence {
            plan_digest: plan.identity_digest(),
            evidence: needle_core::CommandExecutionEvidence {
                id: id.to_owned(),
                approval_id: format!("approval-{id}"),
                argv: plan.argv.clone(),
                cwd: ".".to_owned(),
                source_snapshot_digest: snapshot,
                runner: "cargo".to_owned(),
                runner_version: None,
                exit_status: Some(0),
                duration_ms: 1,
                output_digest: Digest::blake3("output"),
                output_preview: format!("running 1 test\ntest {} ... ok\n", plan.test_identifier),
                test_identifier: Some(plan.test_identifier.clone()),
                tests_executed: Some(1),
                infrastructure_failure: None,
            },
        }
    }

    #[test]
    fn verified_requires_exact_acceptance_coverage() {
        let (verification, ..) = normalize_verifier_turn(
            Ok(turn(json!({
                "verdict": "verified",
                "acceptance_coverage": [],
                "findings": []
            }))),
            &["criterion".to_owned()],
            &[],
            false,
            false,
            None,
            Path::new("."),
            Digest::blake3("snapshot"),
        );
        assert_eq!(verification.verdict, VerificationStatus::Inconclusive);
    }

    #[test]
    fn verified_with_certified_test_requires_command_evidence() {
        let plan = TestPlan {
            runner: "cargo".to_owned(),
            argv: vec![
                "cargo".to_owned(),
                "test".to_owned(),
                "suite::focused".to_owned(),
                "--".to_owned(),
                "--exact".to_owned(),
            ],
            cwd_relative: ".".to_owned(),
            test_identifier: "suite::focused".to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        let (verification, ..) = normalize_verifier_turn(
            Ok(turn(json!({
                "verdict": "verified",
                "acceptance_coverage": [{
                    "criterion": "criterion",
                    "status": "addressed",
                    "evidence": "static evidence"
                }],
                "findings": []
            }))),
            &["criterion".to_owned()],
            &[AssociatedPlan { plan, available: true, unavailable_reason: None }],
            true,
            false,
            None,
            Path::new("."),
            Digest::blake3("snapshot"),
        );
        assert_eq!(verification.verdict, VerificationStatus::Inconclusive);
        assert!(verification.findings.iter().any(|finding| finding.contains("focused test")));
    }

    #[test]
    fn associated_plan_dedup_is_exact_and_ordered() {
        let first = plan("suite::first");
        let second = plan("suite::second");
        let mut duplicate = first.clone();
        duplicate.execution_evidence_id = Some("old-evidence".to_owned());
        let first_digest = first.identity_digest();
        let mut plans = BTreeMap::new();
        insert_associated_plan(&mut plans, second, true, Vec::new());
        insert_associated_plan(&mut plans, first, true, Vec::new());
        insert_associated_plan(&mut plans, duplicate, false, vec!["stale certificate".to_owned()]);

        assert_eq!(plans.len(), 2);
        let digests = plans.keys().copied().collect::<Vec<_>>();
        assert!(digests.windows(2).all(|pair| pair[0] < pair[1]));
        let merged = plans.get(&first_digest).expect("duplicate plan is retained");
        assert!(!merged.available);
        assert!(
            merged
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("stale certificate"))
        );
        assert_eq!(merged.plan.identity_digest(), first_digest);
    }

    #[test]
    fn incomplete_expected_plan_adds_authoritative_finding() {
        let associated = AssociatedPlan {
            plan: plan("suite::unavailable"),
            available: false,
            unavailable_reason: Some("certificate is stale".to_owned()),
        };
        let (verification, _, _, _, _, records) = normalize_verifier_turn(
            Ok(turn(json!({
                "verdict": "rejected",
                "acceptance_coverage": [{
                    "criterion": "criterion",
                    "status": "addressed",
                    "evidence": "static"
                }],
                "findings": []
            }))),
            &["criterion".to_owned()],
            &[associated],
            true,
            false,
            Some("certificate is stale"),
            Path::new("."),
            Digest::blake3("snapshot"),
        );
        assert_eq!(verification.verdict, VerificationStatus::Rejected);
        assert!(
            verification.findings.iter().any(|finding| finding.contains("certificate is stale"))
        );
        assert_eq!(records.len(), 1);
        assert!(!records[0].passed);
    }

    #[test]
    fn invalid_or_partial_plan_evidence_cannot_verify() {
        let snapshot = Digest::blake3("snapshot");
        let certified = plan("suite::invalid");
        let mut captured = evidence(&certified, "evidence-invalid", snapshot);
        captured.evidence.exit_status = Some(1);
        captured.evidence.output_preview =
            "running 1 test\ntest suite::invalid ... FAILED\n".to_owned();
        let mut verifier_turn = turn(json!({
            "verdict": "verified",
            "acceptance_coverage": [{
                "criterion": "criterion",
                "status": "addressed",
                "evidence": "runtime"
            }],
            "findings": []
        }));
        verifier_turn.test_evidence = vec![captured];
        let (verification, _, _, _, _, records) = normalize_verifier_turn(
            Ok(verifier_turn),
            &["criterion".to_owned()],
            &[AssociatedPlan { plan: certified, available: true, unavailable_reason: None }],
            true,
            false,
            None,
            Path::new("."),
            snapshot,
        );
        assert_eq!(verification.verdict, VerificationStatus::Inconclusive);
        assert_eq!(records.len(), 1);
        assert!(records[0].executed);
        assert!(!records[0].passed);
        assert!(
            records[0]
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("focused-test validation"))
        );
    }

    #[test]
    fn snapshot_empty_execution_and_undeclared_evidence_fail_closed() {
        let snapshot = Digest::blake3("snapshot");
        let certified = plan("suite::certified");
        let associated =
            [AssociatedPlan { plan: certified.clone(), available: true, unavailable_reason: None }];

        let mut mismatched = evidence(&certified, "evidence-snapshot", Digest::blake3("other"));
        let mut verifier_turn = turn(json!({
            "verdict": "verified",
            "acceptance_coverage": [{
                "criterion": "criterion",
                "status": "addressed",
                "evidence": "runtime"
            }],
            "findings": []
        }));
        verifier_turn.test_evidence = vec![mismatched.clone()];
        let (verification, _, _, _, _, records) = normalize_verifier_turn(
            Ok(verifier_turn),
            &["criterion".to_owned()],
            &associated,
            true,
            false,
            None,
            Path::new("."),
            snapshot,
        );
        assert_eq!(verification.verdict, VerificationStatus::Inconclusive);
        assert!(
            records[0].failure_reason.as_deref().is_some_and(|reason| reason.contains("snapshot"))
        );

        mismatched = evidence(&certified, "evidence-empty", snapshot);
        mismatched.evidence.tests_executed = Some(0);
        mismatched.evidence.output_preview =
            "running 0 tests\ntest result: ok. 0 passed; 0 failed\n".to_owned();
        let mut verifier_turn = turn(json!({
            "verdict": "verified",
            "acceptance_coverage": [{
                "criterion": "criterion",
                "status": "addressed",
                "evidence": "runtime"
            }],
            "findings": []
        }));
        verifier_turn.test_evidence = vec![mismatched];
        let (verification, _, _, _, _, records) = normalize_verifier_turn(
            Ok(verifier_turn),
            &["criterion".to_owned()],
            &associated,
            true,
            false,
            None,
            Path::new("."),
            snapshot,
        );
        assert_eq!(verification.verdict, VerificationStatus::Inconclusive);
        assert!(
            records[0]
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("focused-test validation"))
        );

        let undeclared = plan("suite::undeclared");
        let mut verifier_turn = turn(json!({
            "verdict": "verified",
            "acceptance_coverage": [{
                "criterion": "criterion",
                "status": "addressed",
                "evidence": "runtime"
            }],
            "findings": []
        }));
        verifier_turn.test_evidence = vec![evidence(&undeclared, "evidence-undeclared", snapshot)];
        let (verification, _, _, _, _, records) = normalize_verifier_turn(
            Ok(verifier_turn),
            &["criterion".to_owned()],
            &associated,
            true,
            false,
            None,
            Path::new("."),
            snapshot,
        );
        assert_eq!(verification.verdict, VerificationStatus::Inconclusive);
        assert!(verification.findings.iter().any(|finding| finding.contains("undeclared plan")));
        assert!(records[0].evidence_id.is_none());
    }

    #[test]
    fn two_and_four_ordered_plans_require_distinct_runtime_evidence() {
        let snapshot = Digest::blake3("snapshot");
        for count in [2, 4] {
            let mut plans = (0..count)
                .map(|index| AssociatedPlan {
                    plan: plan(&format!("suite::case_{index}")),
                    available: true,
                    unavailable_reason: None,
                })
                .collect::<Vec<_>>();
            plans.sort_by_key(|entry| entry.plan.identity_digest());
            let captured = plans
                .iter()
                .enumerate()
                .map(|(index, entry)| evidence(&entry.plan, &format!("evidence-{index}"), snapshot))
                .collect::<Vec<_>>();
            let (verification, _, _, _, evidence_ids, records) = normalize_verifier_turn(
                Ok({
                    let mut turn = turn(json!({
                        "verdict": "verified",
                        "acceptance_coverage": [{
                            "criterion": "criterion",
                            "status": "addressed",
                            "evidence": "runtime"
                        }],
                        "findings": []
                    }));
                    turn.test_evidence = captured;
                    turn
                }),
                &["criterion".to_owned()],
                &plans,
                true,
                false,
                None,
                Path::new("."),
                snapshot,
            );
            assert_eq!(verification.verdict, VerificationStatus::Verified);
            assert_eq!(evidence_ids.len(), count);
            assert_eq!(records.len(), count);
            assert!(records.iter().all(|record| record.passed));
        }
    }

    #[test]
    fn over_cap_plan_set_authorizes_no_subset() {
        let plans = (0..5)
            .map(|index| AssociatedPlan {
                plan: plan(&format!("suite::case_{index}")),
                available: true,
                unavailable_reason: None,
            })
            .collect::<Vec<_>>();
        let (verification, _, _, _, _, records) = normalize_verifier_turn(
            Ok(turn(json!({
                "verdict": "verified",
                "acceptance_coverage": [{
                    "criterion": "criterion",
                    "status": "addressed",
                    "evidence": "static"
                }],
                "findings": []
            }))),
            &["criterion".to_owned()],
            &plans,
            true,
            true,
            Some("5 plans"),
            Path::new("."),
            Digest::blake3("snapshot"),
        );
        assert_eq!(verification.verdict, VerificationStatus::Inconclusive);
        assert!(verification.findings.iter().any(|finding| finding.contains("test-plan bound")));
        assert!(records.is_empty());
    }

    #[test]
    fn over_cap_instructions_redact_plan_details() {
        let plans = (0..5)
            .map(|index| AssociatedPlan {
                plan: plan(&format!("suite::case_{index}")),
                available: true,
                unavailable_reason: None,
            })
            .collect::<Vec<_>>();
        let instructions = verifier_instructions(&plans, true, true, Some("internal details"));
        assert!(instructions.contains("exceeding the hard bound"));
        assert!(!instructions.contains("suite::case_0"));
        assert!(!instructions.contains("argv=["));
        assert!(!instructions.contains("digest="));
    }
}
