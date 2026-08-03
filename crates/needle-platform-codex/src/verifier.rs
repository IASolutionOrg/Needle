use crate::app_server::{AppServerSession, AppServerTurn, AppServerTurnFailure};
use crate::worker::CodexWorker;
use needle_core::claim::ClaimPayload;
use needle_core::{
    AcceptanceCoverage, AcceptanceStatus, ChangeId, Digest, PatchArtifact, SemanticWorkerArtifact,
    TestPlan, VerificationArtifact, VerificationStatus, WorkerConfig,
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
        let trusted_plan =
            settings.trusted_test_execution.then_some(associated.plan.clone()).flatten();
        let instructions = verifier_instructions(
            trusted_plan.as_ref(),
            associated.expected,
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
            trusted_plan.clone(),
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
        let (declared, input_tokens, cached_input_tokens, output_tokens, evidence_ids) =
            normalize_verifier_turn(
                turn,
                &prepared.request.acceptance_criteria,
                trusted_plan.as_ref(),
                associated.expected,
                associated.unavailable_reason.as_deref(),
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
            id: VerificationArtifact::compute_id(
                change_id,
                prepared.patch.id,
                declared.verdict,
                &declared.acceptance_coverage,
                &declared.findings,
                &evidence_ids,
                definition,
            ),
            change_id: change_id.clone(),
            patch_id: prepared.patch.id,
            verdict: declared.verdict,
            acceptance_coverage: declared.acceptance_coverage,
            findings: declared.findings,
            test_evidence_ids: evidence_ids,
            verifier_definition: definition,
            created_unix_ms,
        };
        let attempt = json!({
            "model": config.model,
            "reasoning": config.reasoning,
            "service_tier": config.service_tier,
            "codex_version": isolation.codex_version,
            "verifier_started": true
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
    plan: Option<TestPlan>,
    expected: bool,
    unavailable_reason: Option<String>,
}

fn associated_test_plan(
    store: &RuntimeStore,
    request: &needle_core::ChangeRequest,
    patch: &PatchArtifact,
    repository_root: &Path,
) -> Result<AssociatedTestPlan, String> {
    let changed = patch.files.iter().map(|file| file.path.as_str()).collect::<BTreeSet<_>>();
    let mut plans = BTreeMap::<String, TestPlan>::new();
    let mut expected = false;
    let mut unavailable = Vec::new();
    for id in &request.artifact_ids {
        let Some(artifact) =
            store.semantic_artifact(&id.to_string()).map_err(|error| error.to_string())?
        else {
            continue;
        };
        let Some(certificate) = store
            .validation_certificate_for_artifact(&id.to_string())
            .map_err(|error| error.to_string())?
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
        if !artifact_and_certificate_are_fresh(&artifact, &certificate, repository_root)
            || artifact
                .dependency_manifest
                .dependencies
                .iter()
                .any(|dependency| changed.contains(dependency.path.as_str()))
            || evidence_paths.iter().any(|path| changed.contains(path.as_str()))
        {
            unavailable.push(format!("artifact test plan `{id}` changed or became stale"));
            continue;
        }
        insert_plan(&mut plans, runner, argv, cwd_relative, identifiers)?;
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
        let Some(certificate) =
            store.claim_validation_certificate_for_claim(*id).map_err(|error| error.to_string())?
        else {
            unavailable.push(format!("claim test plan `{id}` has no certificate"));
            continue;
        };
        if !claim_validation_certificate_is_fresh(&certificate, repository_root)
            || certificate
                .dependencies
                .iter()
                .any(|dependency| changed.contains(dependency.path.as_str()))
            || evidence_paths.iter().any(|path| changed.contains(path.as_str()))
        {
            unavailable.push(format!("claim test plan `{id}` changed or became stale"));
            continue;
        }
        insert_plan(&mut plans, runner, argv, cwd_relative, vec![identifier])?;
    }
    if plans.len() > 1 {
        unavailable.push(
            "multiple distinct certified test plans require a future verifier adapter".to_owned(),
        );
        plans.clear();
    }
    Ok(AssociatedTestPlan {
        plan: plans.into_values().next(),
        expected,
        unavailable_reason: (!unavailable.is_empty()).then(|| unavailable.join("; ")),
    })
}

fn insert_plan(
    plans: &mut BTreeMap<String, TestPlan>,
    runner: String,
    argv: Vec<String>,
    cwd_relative: String,
    identifiers: Vec<String>,
) -> Result<(), String> {
    if identifiers.len() != 1 {
        return Err("certified test plan must contain exactly one identifier".to_owned());
    }
    let identifier = identifiers.into_iter().next().expect("identifier length was checked");
    let plan = TestPlan {
        runner,
        argv,
        cwd_relative,
        test_identifier: identifier,
        requires_approval: true,
        execution_evidence_id: None,
    };
    plan.test_command().map_err(|_| "certified test plan is no longer safe".to_owned())?;
    let key = serde_json::to_string(&plan).map_err(|error| error.to_string())?;
    plans.insert(key, plan);
    Ok(())
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
    test_plan: Option<&TestPlan>,
    test_expected: bool,
    unavailable_reason: Option<&str>,
) -> (DeclaredVerification, Option<u64>, Option<u64>, Option<u64>, Vec<String>) {
    let turn = match turn {
        Ok(turn) => turn,
        Err(failure) => {
            return (
                inconclusive_verification(&failure.diagnostic),
                failure.input_tokens,
                failure.cached_input_tokens,
                failure.output_tokens,
                Vec::new(),
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
    let mut evidence_ids = Vec::new();
    if let Some(plan) = test_plan {
        if let Some(evidence) = turn
            .command_evidence
            .iter()
            .find(|evidence| validate_test_evidence(plan, evidence).is_ok())
        {
            evidence_ids.push(evidence.id.clone());
        } else if declared.verdict == VerificationStatus::Verified {
            declared.verdict = VerificationStatus::Inconclusive;
            declared.findings.push(
                "the certified focused test did not produce valid execution evidence".to_owned(),
            );
        }
    } else if test_expected && declared.verdict == VerificationStatus::Verified {
        declared.verdict = VerificationStatus::Inconclusive;
        declared.findings.push(
            unavailable_reason.unwrap_or("the associated test plan was unavailable").to_owned(),
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
    (declared, turn.input_tokens, turn.cached_input_tokens, turn.output_tokens, evidence_ids)
}

fn inconclusive_verification(reason: &str) -> DeclaredVerification {
    DeclaredVerification {
        verdict: VerificationStatus::Inconclusive,
        acceptance_coverage: Vec::new(),
        findings: vec![reason.chars().take(MAX_VERIFIER_TEXT_BYTES).collect()],
    }
}

fn verifier_instructions(
    plan: Option<&TestPlan>,
    test_expected: bool,
    unavailable_reason: Option<&str>,
) -> String {
    let test = if let Some(plan) = plan {
        format!(
            "Execute exactly this certified focused test once and no other test command: {:?}. A verified verdict requires observing it pass.",
            plan.argv
        )
    } else if test_expected {
        format!(
            "A related test exists but cannot be executed safely in this verification ({}) . Do not run tests and return inconclusive if test evidence is necessary.",
            unavailable_reason.unwrap_or("unavailable certified plan")
        )
    } else {
        "No certified focused test is associated with this change. Do not run tests; perform bounded static verification.".to_owned()
    };
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
    let mut hasher = needle_core::CanonicalHasher::new(b"needle-verifier-definition");
    hasher.field_str("independent-read-only-v1");
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
            observation_trace: Default::default(),
            file_change_approvals_granted: 0,
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
            None,
            false,
            None,
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
            Some(&plan),
            true,
            None,
        );
        assert_eq!(verification.verdict, VerificationStatus::Inconclusive);
        assert!(verification.findings.iter().any(|finding| finding.contains("focused test")));
    }
}
