mod protocol;

use crate::app_server::{AppServerSession, validate_compatibility_fixture};
use needle_core::{
    ArtifactKind, Digest, NeedResult, SemanticArtifactResult, TestPlan, WorkerArtifactResult,
    WorkerConfig, WorkerFailure, WorkerOutcome, WorkerRequest, built_in_route_plans,
};
use needle_runtime::{IsolatedCheckout, RuntimeStore, WorkerExecutor, validate_test_evidence};
use protocol::{
    CompactWorkerResponse, GroupDiagnostic, LegacyCompactWorkerResponse, NormalizedResponse,
    SemanticCompactWorkerResponse, normalize_legacy_response, normalize_response,
    normalize_semantic_response, repair_prompt, semantic_worker_output_schema_for_scenario,
    should_repair, worker_output_schema,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const SUPPORTED_CODEX_VERSIONS: &[&str] = &["0.144.0"];
const MAX_DIAGNOSTIC_BYTES: usize = 4096;
type WorkerProtocolOutput =
    (NeedResult, WorkerArtifactResult, Option<SemanticArtifactResult>, String);
type WorkerProtocolFailure = (String, String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IsolationReport {
    pub codex_version: String,
    pub supported: bool,
    pub required_flags_present: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportPreflightReport {
    pub codex_version: String,
    pub native_launcher: String,
    pub compatibility_fixture_valid: bool,
    pub sandbox_materialized: bool,
    pub app_server_initialized: bool,
    pub ephemeral_thread_started: bool,
    pub ephemeral_thread_cleanup_completed: bool,
    pub sandbox_cleaned: bool,
    pub provider_turns_started: u32,
    #[serde(default)]
    pub test_plan_declared: bool,
    #[serde(default)]
    pub test_execution_available: bool,
    #[serde(default)]
    pub test_execution_unavailable_reason: Option<String>,
    pub source_head_sha: String,
    pub source_snapshot_digest: String,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerDiagnosticContract {
    pub schema: String,
    pub output_schema_id: String,
    pub requested_artifact_kinds: Vec<ArtifactKind>,
    pub system_instructions: String,
    pub system_instructions_digest: Digest,
    pub prompt: String,
    pub prompt_digest: Digest,
    pub output_schema: Value,
    pub output_schema_digest: Digest,
}

impl IsolationReport {
    pub fn verified(&self) -> bool {
        self.supported && self.required_flags_present
    }
}

#[derive(Clone, Debug)]
pub struct CodexWorker {
    data_directory: PathBuf,
    codex_home: Option<PathBuf>,
    cancellation: Option<Arc<AtomicBool>>,
}

impl CodexWorker {
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

    pub fn verify_isolation(executable: &str) -> Result<IsolationReport, String> {
        let version_output = Command::new(executable)
            .arg("--version")
            .output()
            .map_err(|error| format!("cannot execute Codex: {error}"))?;
        if !version_output.status.success() {
            return Err("Codex --version failed".to_owned());
        }
        let raw_version = String::from_utf8_lossy(&version_output.stdout);
        let version = raw_version
            .split_whitespace()
            .find(|part| part.bytes().next().is_some_and(|byte| byte.is_ascii_digit()))
            .unwrap_or_default()
            .to_owned();
        let app_server_help = command_text(executable, &["app-server", "--help"])?;
        let schema_help =
            command_text(executable, &["app-server", "generate-json-schema", "--help"])?;
        let required_flags_present =
            ["--listen", "--strict-config"].iter().all(|flag| app_server_help.contains(flag))
                && schema_help.contains("--experimental");
        let supported = SUPPORTED_CODEX_VERSIONS.contains(&version.as_str());
        if supported {
            let fixture: Value = serde_json::from_str(include_str!(
                "../../../fixtures/codex-app-server/0.144.0/compatibility.json"
            ))
            .map_err(|error| format!("invalid App Server compatibility fixture: {error}"))?;
            validate_compatibility_fixture(&version, &fixture)?;
        }
        Ok(IsolationReport { supported, codex_version: version, required_flags_present })
    }

    pub fn preflight_transport(
        &self,
        config: &WorkerConfig,
        repository_root: &Path,
    ) -> Result<TransportPreflightReport, String> {
        self.preflight_transport_for_test_plan(
            config,
            repository_root,
            "locate.implementation",
            None,
            false,
        )
    }

    pub fn preflight_transport_for_test_plan(
        &self,
        config: &WorkerConfig,
        repository_root: &Path,
        route: &str,
        test_plan: Option<TestPlan>,
        trusted_test_execution: bool,
    ) -> Result<TransportPreflightReport, String> {
        let started = Instant::now();
        let isolation = Self::verify_isolation(&config.executable)?;
        if !isolation.verified() {
            return Err(format!(
                "worker isolation is not verified for Codex {}",
                isolation.codex_version
            ));
        }
        let sandbox = IsolatedCheckout::materialize(
            repository_root,
            &self.data_directory.join("transport-preflight-runs"),
        )
        .map_err(|error| error.to_string())?;
        let source_head_sha = sandbox.snapshot().head_sha.clone();
        let source_snapshot_digest = sandbox.snapshot().source_digest.to_string();
        let test_plan_declared = test_plan.is_some();
        let store = RuntimeStore::new(self.data_directory.join("transport-preflight.sqlite3"));
        if let Err(error) = store.initialize() {
            return Err(cleanup_preflight_sandbox(
                sandbox,
                format!("cannot initialize transport preflight store: {error}"),
            ));
        }
        let session = match crate::app_server::AppServerSession::start(
            config,
            self.codex_home.as_deref(),
            true,
            "Needle transport preflight. Do not start a model turn.",
            sandbox.checkout_root(),
            sandbox.target_root(),
            sandbox.temp_root(),
            sandbox.snapshot().source_digest,
            sandbox.snapshot().repository_id,
            route,
            test_plan,
            trusted_test_execution,
            store,
        ) {
            Ok(session) => session,
            Err(error) => return Err(cleanup_preflight_sandbox(sandbox, error)),
        };
        if session.thread_id().is_empty() {
            return cleanup_worker_resources(session, sandbox)
                .and(Err("App Server preflight returned an empty thread id".to_owned()));
        }
        let test_execution_available = session.test_execution_available();
        let test_execution_unavailable_reason =
            session.test_execution_unavailable_reason().map(str::to_owned);
        cleanup_worker_resources(session, sandbox)?;
        Ok(TransportPreflightReport {
            codex_version: isolation.codex_version,
            native_launcher: config.executable.clone(),
            compatibility_fixture_valid: true,
            sandbox_materialized: true,
            app_server_initialized: true,
            ephemeral_thread_started: true,
            ephemeral_thread_cleanup_completed: true,
            sandbox_cleaned: true,
            provider_turns_started: 0,
            test_plan_declared,
            test_execution_available,
            test_execution_unavailable_reason,
            source_head_sha,
            source_snapshot_digest,
            duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        })
    }

    pub fn diagnostic_contract(
        request: &WorkerRequest,
    ) -> Result<WorkerDiagnosticContract, String> {
        let requested_artifact_kinds = requested_model_artifact_kinds(request);
        let system_instructions = worker_system_instructions(request);
        let prompt = worker_prompt(request);
        let output_schema = output_schema(request, &requested_artifact_kinds);
        let output_schema_digest = serde_json::to_vec(&output_schema)
            .map(Digest::blake3)
            .map_err(|error| format!("cannot digest worker output schema: {error}"))?;
        Ok(WorkerDiagnosticContract {
            schema: "needle.worker-diagnostic-contract/1".to_owned(),
            output_schema_id: if request.semantic_fragment.is_some() {
                needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID.to_owned()
            } else {
                needle_core::ARTIFACT_RESULT_SCHEMA_ID.to_owned()
            },
            requested_artifact_kinds,
            system_instructions_digest: Digest::blake3(system_instructions.as_bytes()),
            prompt_digest: Digest::blake3(prompt.as_bytes()),
            output_schema_digest,
            system_instructions,
            prompt,
            output_schema,
        })
    }

    fn execute_inner(
        &self,
        config: &WorkerConfig,
        request: &WorkerRequest,
    ) -> Result<WorkerOutcome, Box<WorkerFailure>> {
        self.recover_stale_sessions(&config.executable);
        let started = Instant::now();
        let mut usage = Usage::default();
        let mut worker_turns = 0;
        let mut repair_performed = false;
        let mut discarded_facts = 0_u32;
        let mut session_id = None;
        let mut cleanup_success = None;
        let requested_model_kinds = requested_model_artifact_kinds(request);

        let result = (|| -> Result<WorkerProtocolOutput, WorkerProtocolFailure> {
            let isolation = Self::verify_isolation(&config.executable)
                .map_err(|error| ("isolation_check_failed".to_owned(), error))?;
            if !isolation.verified() {
                return Err((
                    "isolation_unverified".to_owned(),
                    format!(
                        "worker isolation is not verified for Codex {}",
                        isolation.codex_version
                    ),
                ));
            }
            let repository_root = fs::canonicalize(&request.repository_root).map_err(|error| {
                ("repository_unavailable".to_owned(), format!("cannot resolve worker cwd: {error}"))
            })?;
            let sandbox = IsolatedCheckout::materialize(
                &repository_root,
                &self.data_directory.join("worker-runs"),
            )
            .map_err(|error| ("sandbox_bypass".to_owned(), error.to_string()))?;
            if sandbox.snapshot().source_digest != request.repository_snapshot.source_digest {
                return Err(failure_after_sandbox_cleanup(
                    sandbox,
                    &mut cleanup_success,
                    "sandbox_bypass",
                    "requested source snapshot changed before materialization".to_owned(),
                ));
            }
            let store = RuntimeStore::new(self.data_directory.join("needle.sqlite3"));
            if let Err(error) = store.initialize() {
                return Err(failure_after_sandbox_cleanup(
                    sandbox,
                    &mut cleanup_success,
                    "worker_store",
                    error.to_string(),
                ));
            }
            let effective_test_plan = effective_test_plan(request).cloned();
            let instructions = worker_system_instructions(request);
            let mut app_server = match AppServerSession::start(
                config,
                self.codex_home.as_deref(),
                true,
                &instructions,
                sandbox.checkout_root(),
                sandbox.target_root(),
                sandbox.temp_root(),
                sandbox.snapshot().source_digest,
                sandbox.snapshot().repository_id,
                request.need_key.as_str(),
                effective_test_plan.clone(),
                request.trusted_test_execution,
                store,
            ) {
                Ok(app_server) => app_server,
                Err(error) => {
                    return Err(failure_after_sandbox_cleanup(
                        sandbox,
                        &mut cleanup_success,
                        "app_server_start_failed",
                        error,
                    ));
                }
            };
            session_id = Some(app_server.thread_id().to_owned());

            let protocol_result = (|| {
                worker_turns = 1;
                let first = app_server
                    .run_turn_cancellable(
                        &worker_prompt(request),
                        &output_schema(request, &requested_model_kinds),
                        Duration::from_secs(config.timeout_seconds),
                        self.cancellation.as_deref(),
                    )
                    .map_err(|failure| {
                        usage.absorb_failure(&failure);
                        ("worker_turn_failed".to_owned(), failure.diagnostic)
                    })?;
                usage.absorb_total(&first);
                let mut command_evidence = first.command_evidence;
                let mut observation_trace = first.observation_trace;
                let first_test_error =
                    test_evidence_error(effective_test_plan.as_ref(), &command_evidence);
                let actionable_artifact_repair =
                    artifact_repair_is_actionable(&requested_model_kinds, &first.response);
                let mut normalized = deserialize_and_normalize(
                    first.response,
                    sandbox.checkout_root(),
                    &requested_model_kinds,
                );
                normalized.discard_unrequested_kinds(&requested_model_kinds);
                normalized.record_missing_requested_kinds(&requested_model_kinds);
                discarded_facts = discarded_facts.saturating_add(normalized.discarded_facts);

                let repair_required = repair_required(
                    config.evidence_failure_policy,
                    first_test_error.is_some(),
                    should_repair(config.evidence_failure_policy, &normalized)
                        && actionable_artifact_repair,
                );
                let mut final_test_error = first_test_error;
                if repair_required {
                    repair_performed = true;
                    let prompt = repair_with_test_diagnostic(
                        &normalized.diagnostics,
                        final_test_error.as_deref(),
                        effective_test_plan.as_ref(),
                        &requested_model_kinds,
                    );
                    let remaining = remaining_worker_budget(
                        config.timeout_seconds,
                        started.elapsed(),
                        first.approval_wait,
                    );
                    worker_turns = 2;
                    let second = app_server
                        .run_turn_cancellable(
                            &prompt,
                            &output_schema(request, &requested_model_kinds),
                            remaining,
                            self.cancellation.as_deref(),
                        )
                        .map_err(|failure| {
                            usage.absorb_failure(&failure);
                            (
                                "repair_turn_failed".to_owned(),
                                format!(
                                    "{}; trigger={}",
                                    failure.diagnostic,
                                    bound_text(&prompt, 512)
                                ),
                            )
                        })?;
                    usage.absorb_total(&second);
                    observation_trace.merge(second.observation_trace);
                    accumulate_command_evidence(&mut command_evidence, second.command_evidence);
                    final_test_error =
                        test_evidence_error(effective_test_plan.as_ref(), &command_evidence);
                    let mut repaired = deserialize_and_normalize(
                        second.response,
                        sandbox.checkout_root(),
                        &requested_model_kinds,
                    );
                    repaired.discard_unrequested_kinds(&requested_model_kinds);
                    repaired.record_missing_requested_kinds(&requested_model_kinds);
                    discarded_facts = discarded_facts.saturating_add(repaired.discarded_facts);
                    normalized.merge(repaired);
                }
                if let Some(error) = final_test_error {
                    return Err(("test_evidence_invalid".to_owned(), error));
                }
                let missing_kinds = normalized.missing_requested_kinds(&requested_model_kinds);
                if !missing_kinds.is_empty() {
                    return Err((
                        "artifact_protocol_incomplete".to_owned(),
                        missing_artifact_diagnostic(&missing_kinds, &normalized.diagnostics),
                    ));
                }
                if !normalized.has_facts() {
                    return Err((
                        "no_valid_evidence".to_owned(),
                        diagnostic_summary(&normalized.diagnostics),
                    ));
                }
                let (schema_id, artifacts, test_plan) =
                    normalized.artifact_result().ok_or_else(|| {
                        (
                            "artifact_protocol_ambiguous".to_owned(),
                            "worker turns used incompatible result schemas".to_owned(),
                        )
                    })?;
                let semantic_artifacts = normalized.semantic_artifacts().to_vec();
                let result = normalized
                    .into_need_result(sandbox.checkout_root())
                    .map_err(|error| ("evidence_binding_failed".to_owned(), error))?;
                let artifact_traces = request
                    .requested_artifact_kinds
                    .as_slice()
                    .first()
                    .filter(|_| request.requested_artifact_kinds.len() == 1)
                    .map(|kind| BTreeMap::from([(kind.clone(), observation_trace.clone())]))
                    .unwrap_or_default();
                let semantic_artifact_result =
                    (!semantic_artifacts.is_empty()).then(|| SemanticArtifactResult {
                        schema_id: needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID.to_owned(),
                        artifacts: semantic_artifacts,
                        observation_trace: observation_trace.clone(),
                        artifact_traces: artifact_traces.clone(),
                    });
                let artifact_result = WorkerArtifactResult {
                    schema_id,
                    artifacts,
                    test_plan,
                    observation_trace,
                    artifact_traces,
                };
                Ok((result, artifact_result, semantic_artifact_result, isolation.codex_version))
            })();
            let cleanup = cleanup_worker_resources(app_server, sandbox);
            cleanup_success = Some(cleanup.is_ok());
            match (protocol_result, cleanup) {
                (result, Ok(())) => result,
                (Ok(_), Err(cleanup_error)) => {
                    Err(("worker_cleanup_failed".to_owned(), cleanup_error))
                }
                (Err((code, diagnostic)), Err(cleanup_error)) => Err((
                    "worker_cleanup_failed".to_owned(),
                    format!(
                        "{cleanup_error}; preceding_failure={code}: {}",
                        bound_text(&diagnostic, 1024)
                    ),
                )),
            }
        })();

        let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        match result {
            Ok((result, artifact_result, semantic_artifact_result, codex_version)) => {
                Ok(WorkerOutcome {
                    result,
                    artifact_result: Some(artifact_result),
                    semantic_artifact_result,
                    worker_model: config.model.clone(),
                    worker_reasoning: config.reasoning.clone(),
                    codex_version,
                    input_tokens: usage.input_tokens,
                    cached_input_tokens: usage.cached_input_tokens,
                    output_tokens: usage.output_tokens,
                    duration_ms,
                    process_status: "success".to_owned(),
                    logical_worker_spawns: 1,
                    worker_turns,
                    repair_performed,
                    discarded_facts,
                    worker_session_id: session_id,
                    session_cleanup_success: cleanup_success,
                })
            }
            Err((code, diagnostic)) => Err(Box::new(WorkerFailure {
                code,
                diagnostic: bound_text(&diagnostic, MAX_DIAGNOSTIC_BYTES),
                input_tokens: usage.input_tokens,
                cached_input_tokens: usage.cached_input_tokens,
                output_tokens: usage.output_tokens,
                duration_ms,
                logical_worker_spawns: u32::from(worker_turns > 0),
                worker_turns,
                repair_performed,
                discarded_facts,
                worker_session_id: session_id,
                session_cleanup_success: cleanup_success,
            })),
        }
    }

    fn recover_stale_sessions(&self, executable: &str) {
        let store = RuntimeStore::new(self.data_directory.join("needle.sqlite3"));
        let Ok(()) = store.initialize() else {
            return;
        };
        let Ok(sessions) = store.pending_worker_sessions() else {
            return;
        };
        for session_id in sessions {
            if cleanup_session(executable, self.codex_home.as_deref(), &session_id).is_ok() {
                let _ = store.mark_worker_session_cleaned(&session_id);
            }
        }
    }
}

impl WorkerExecutor for CodexWorker {
    fn execute(
        &self,
        config: &WorkerConfig,
        request: &WorkerRequest,
    ) -> Result<WorkerOutcome, Box<WorkerFailure>> {
        self.execute_inner(config, request)
    }
}

fn deserialize_and_normalize(
    mut response: Value,
    root: &Path,
    requested: &[ArtifactKind],
) -> NormalizedResponse {
    let unrequested = discard_unrequested_semantic_values(&mut response, requested);
    let mut normalized =
        match serde_json::from_value::<SemanticCompactWorkerResponse>(response.clone()) {
            Ok(response) => normalize_semantic_response(response, root),
            Err(semantic_error) => {
                match serde_json::from_value::<CompactWorkerResponse>(response.clone()) {
                    Ok(response) => normalize_response(response, root),
                    Err(_) => match serde_json::from_value::<LegacyCompactWorkerResponse>(response)
                    {
                        Ok(response) => normalize_legacy_response(response, root),
                        Err(_) => NormalizedResponse::schema_failure(semantic_error.to_string()),
                    },
                }
            }
        };
    for kind in unrequested {
        normalized.record_unrequested_kind(&kind);
    }
    normalized
}

fn discard_unrequested_semantic_values(
    response: &mut Value,
    requested: &[ArtifactKind],
) -> BTreeSet<String> {
    if requested.is_empty()
        || response.get("schema").and_then(Value::as_str)
            != Some(needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID)
    {
        return BTreeSet::new();
    }
    let Some(artifacts) = response.get_mut("artifacts").and_then(Value::as_array_mut) else {
        return BTreeSet::new();
    };
    let mut discarded = BTreeSet::new();
    artifacts.retain(|artifact| {
        let Some(kind) = artifact.get("kind").and_then(Value::as_str) else {
            return true;
        };
        if requested.iter().any(|requested| requested.0 == kind) {
            true
        } else {
            discarded.insert(kind.to_owned());
            false
        }
    });
    discarded
}

fn repair_required(
    policy: needle_core::EvidenceFailurePolicy,
    test_evidence_invalid: bool,
    artifact_repair_required: bool,
) -> bool {
    policy == needle_core::EvidenceFailurePolicy::RepairOnce
        && (test_evidence_invalid || artifact_repair_required)
}

fn artifact_repair_is_actionable(requested: &[ArtifactKind], response: &Value) -> bool {
    let test_plan_only = requested == [ArtifactKind::test_plan()];
    if !test_plan_only {
        return true;
    }

    response.get("artifacts").and_then(Value::as_array).is_some_and(|artifacts| {
        artifacts
            .iter()
            .any(|artifact| artifact.get("kind").and_then(Value::as_str) == Some("test-plan"))
    }) || response.get("test_plan").is_some_and(|plan| !plan.is_null())
}

fn output_schema(request: &WorkerRequest, requested: &[ArtifactKind]) -> Value {
    if request.semantic_fragment.is_some() {
        semantic_worker_output_schema_for_scenario(requested, requested_runtime_scenario(request))
    } else {
        worker_output_schema(requested)
    }
}

fn requested_runtime_scenario(request: &WorkerRequest) -> Option<&str> {
    request
        .semantic_fragment
        .as_ref()?
        .obligations
        .iter()
        .filter(|obligation| obligation.predicate == needle_core::PredicateKind::RuntimeFlow)
        .flat_map(|obligation| obligation.facets.iter())
        .find(|facet| facet.key == "scenario")
        .map(|facet| facet.value.as_str())
}

fn worker_prompt(request: &WorkerRequest) -> String {
    let mut prompt = request.need_body.clone();
    if let Some(fragment) = &request.semantic_fragment {
        prompt.push_str("\n\nParent-owned semantic demand (do not repeat these fields in JSON):");
        for subject in &fragment.subject_definitions {
            prompt.push_str("\n- subject ");
            prompt.push_str(match subject.kind {
                needle_core::SubjectKind::Symbol => "symbol",
                needle_core::SubjectKind::CliOption => "cli-option",
                needle_core::SubjectKind::ConfigurationKey => "configuration-key",
                needle_core::SubjectKind::Test => "test",
                needle_core::SubjectKind::File => "file",
                needle_core::SubjectKind::Module => "module",
                needle_core::SubjectKind::Behavior => "behavior",
            });
            prompt.push_str(": \"");
            prompt.push_str(&subject.canonical_name);
            prompt.push('"');
        }
        for obligation in &fragment.obligations {
            if obligation.predicate == needle_core::PredicateKind::FocusedTests
                && effective_test_plan(request).is_some()
            {
                continue;
            }
            prompt.push_str("\n- missing obligation ");
            prompt.push_str(match obligation.predicate {
                needle_core::PredicateKind::ImplementationLocation => "implementation-location",
                needle_core::PredicateKind::RuntimeFlow => "runtime-flow",
                needle_core::PredicateKind::FocusedTests => "focused-tests",
            });
            for facet in &obligation.facets {
                prompt.push(' ');
                prompt.push_str(&facet.key);
                prompt.push('=');
                prompt.push_str(&facet.value);
            }
        }
        if !fragment.semantic_inputs.is_empty() {
            prompt.push_str("\n- certified input artifacts:");
            for artifact in &fragment.semantic_inputs {
                prompt.push(' ');
                prompt.push_str(&artifact.to_string());
            }
            prompt.push_str("\nDo not repeat discovery already covered by those input artifacts.");
        }
    }
    if let Some(plan) = effective_test_plan(request) {
        prompt.push_str("\n\nThe declared focused test is exact argv: ");
        prompt.push_str(&plan.argv.join(" "));
        prompt.push_str(
            ". Needle may permit it through the approval bridge only when the runtime toolchain is available. Do not execute any other command.",
        );
    }
    prompt
}

fn worker_system_instructions(request: &WorkerRequest) -> String {
    let preset = bound_text(request.preset.system_prompt.trim(), 420);
    let requested_model_kinds = requested_model_artifact_kinds(request);
    let test_policy = if effective_test_plan(request).is_some() {
        "The declared TestPlan permits but does not require execution. Run it only when Needle has not marked test execution unavailable, and then run only that direct cargo test in the isolated checkout; Needle decides its approval. The TestPlan and its test locations are parent-owned; do not return them as code-location. "
    } else if requested_model_kinds.contains(&ArtifactKind::test_plan()) {
        "No TestPlan was declared. Locate and return one from an observed test definition and Cargo target, but do not execute tests, builds, or scripts. "
    } else {
        "Do not run tests, builds, or scripts because no TestPlan was declared. "
    };
    let requested =
        requested_model_kinds.iter().map(|kind| kind.0.as_str()).collect::<Vec<_>>().join(", ");
    let schema_id = if request.semantic_fragment.is_some() {
        needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID
    } else {
        needle_core::ARTIFACT_RESULT_SCHEMA_ID
    };
    format!(
        "{preset}\n\nContract: discover only these requested artifact kinds: {requested}. Use only bounded read-only inspection in the isolated checkout. Searches may locate candidates, but return only the minimal repository-relative evidence files that support the selected result; Needle reads and validates those declarations independently. Read-only source checkout. {test_policy}For semantic locations, identify the exact symbol and always return null byte_start and byte_end; line numbers are not byte offsets. Never modify files. No network, hooks, plugins, MCP, memory, project instructions, or subagents. Return only JSON matching the provided {schema_id} output schema. Return no unrequested artifact kind. Max 8 artifacts. Derive test argv only from an observed Cargo target and test identifier. In a test-plan, argv is the complete process vector and must begin exactly with [\"cargo\",\"test\"]; do not omit cargo even though runner is also \"cargo\"."
    )
}

fn missing_artifact_diagnostic(
    missing_kinds: &[String],
    diagnostics: &[GroupDiagnostic],
) -> String {
    format!(
        "worker omitted requested artifact kinds: {}; normalization: {}",
        missing_kinds.join(", "),
        bound_text(&diagnostic_summary(diagnostics), 1024)
    )
}

fn requested_model_artifact_kinds(request: &WorkerRequest) -> Vec<ArtifactKind> {
    let mut requested = if request.requested_artifact_kinds.is_empty() {
        built_in_route_plans()
            .into_iter()
            .find(|plan| plan.route_key == request.need_key)
            .map(|plan| {
                plan.nodes
                    .into_iter()
                    .filter(|node| node.operator_id != "evidence-brief")
                    .map(|node| ArtifactKind(node.operator_id))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        request.requested_artifact_kinds.clone()
    };
    if request.declared_test_plan.is_some() {
        requested.retain(|kind| kind != &ArtifactKind::test_plan());
    }
    // A validated BehaviorTrace already carries repository-relative paths and
    // symbols for every step. The runtime deterministically projects those
    // locations into the CodeLocation node, so asking the model to repeat the
    // same evidence under a second kind only makes the protocol more brittle.
    if request.semantic_fragment.is_none() && requested.contains(&ArtifactKind::behavior_trace()) {
        requested.retain(|kind| kind != &ArtifactKind::code_location());
    }
    requested.sort();
    requested.dedup();
    requested
}

fn effective_test_plan(request: &WorkerRequest) -> Option<&TestPlan> {
    let test_requested = request.requested_artifact_kinds.is_empty()
        || request.requested_artifact_kinds.contains(&needle_core::ArtifactKind::test_plan());
    test_requested.then_some(request.declared_test_plan.as_ref()).flatten()
}

fn test_evidence_error(
    plan: Option<&TestPlan>,
    evidence: &[needle_core::CommandExecutionEvidence],
) -> Option<String> {
    let plan = plan?;
    if evidence.is_empty() {
        return None;
    }
    if evidence.iter().any(|item| validate_test_evidence(plan, item).is_ok()) {
        return None;
    }
    evidence
        .last()
        .and_then(|item| validate_test_evidence(plan, item).err())
        .map(|error| error.to_string())
}

fn accumulate_command_evidence(
    evidence: &mut Vec<needle_core::CommandExecutionEvidence>,
    next: Vec<needle_core::CommandExecutionEvidence>,
) {
    evidence.extend(next);
}

fn repair_with_test_diagnostic(
    diagnostics: &[GroupDiagnostic],
    test_error: Option<&str>,
    plan: Option<&TestPlan>,
    requested: &[ArtifactKind],
) -> String {
    let mut prompt = repair_prompt(diagnostics);
    if requested.len() == 1 {
        prompt.push_str("; required_artifact=");
        prompt.push_str(&requested[0].0);
    }
    if requested == [ArtifactKind::test_plan()] {
        prompt
            .push_str("; test-plan argv is the complete process vector and must begin cargo,test");
    }
    if let Some(error) = test_error {
        prompt.push_str("; test_invalid=");
        prompt.push_str(&bound_text(error, 240));
    }
    if let Some(plan) = plan {
        prompt.push_str("; execute only exact argv: ");
        prompt.push_str(&plan.argv.join(" "));
    }
    prompt
}

fn cleanup_session(
    executable: &str,
    codex_home: Option<&Path>,
    session_id: &str,
) -> Result<(), String> {
    let mut command = Command::new(executable);
    command
        .arg("delete")
        .arg("--force")
        .arg(session_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear();
    let mut environment = sanitized_environment();
    if let Some(codex_home) = codex_home {
        environment.insert("CODEX_HOME".to_owned(), codex_home.to_string_lossy().into_owned());
    }
    for (key, value) in environment {
        command.env(key, value);
    }
    let status =
        command.status().map_err(|error| format!("cannot delete worker session: {error}"))?;
    if status.success() { Ok(()) } else { Err(format!("Codex delete failed with {status}")) }
}

fn command_text(executable: &str, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new(executable)
        .args(arguments)
        .output()
        .map_err(|error| format!("cannot inspect Codex CLI: {error}"))?;
    if !output.status.success() {
        return Err(format!("Codex {} failed", arguments.join(" ")));
    }
    Ok(format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

pub(crate) fn sanitized_environment() -> BTreeMap<String, String> {
    const ALLOWED: &[&str] = &[
        "PATH",
        "Path",
        "PATHEXT",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "USERPROFILE",
        "HOME",
        "LOCALAPPDATA",
        "APPDATA",
        "TEMP",
        "TMP",
        "TMPDIR",
        "CODEX_HOME",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ];
    let mut output = BTreeMap::new();
    for key in ALLOWED {
        if let Ok(value) = env::var(key) {
            output.insert((*key).to_owned(), value);
        }
    }
    output
}

#[derive(Default)]
struct Usage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl Usage {
    fn absorb_total(&mut self, turn: &crate::app_server::AppServerTurn) {
        self.input_tokens = maximum(self.input_tokens, turn.input_tokens);
        self.cached_input_tokens = maximum(self.cached_input_tokens, turn.cached_input_tokens);
        self.output_tokens = maximum(self.output_tokens, turn.output_tokens);
    }

    fn absorb_failure(&mut self, failure: &crate::app_server::AppServerTurnFailure) {
        self.input_tokens = maximum(self.input_tokens, failure.input_tokens);
        self.cached_input_tokens = maximum(self.cached_input_tokens, failure.cached_input_tokens);
        self.output_tokens = maximum(self.output_tokens, failure.output_tokens);
    }
}

fn maximum(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).max(right.unwrap_or(0))),
    }
}

fn remaining_worker_budget(
    timeout_seconds: u64,
    elapsed: Duration,
    approval_wait: Duration,
) -> Duration {
    Duration::from_secs(timeout_seconds)
        .saturating_add(approval_wait)
        .saturating_sub(elapsed)
        .max(Duration::from_secs(1))
}

fn diagnostic_summary(diagnostics: &[GroupDiagnostic]) -> String {
    if diagnostics.is_empty() {
        return "worker returned no evidence groups".to_owned();
    }
    diagnostics
        .iter()
        .map(|item| match item.index {
            Some(index) => format!("group {} {}", index + 1, item.code),
            None => item.code.clone(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn bound_text(value: &str, maximum: usize) -> String {
    let mut end = value.len().min(maximum);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn failure_after_sandbox_cleanup(
    sandbox: IsolatedCheckout,
    cleanup_success: &mut Option<bool>,
    code: &str,
    diagnostic: String,
) -> WorkerProtocolFailure {
    match sandbox.cleanup() {
        Ok(()) => {
            *cleanup_success = Some(true);
            (code.to_owned(), diagnostic)
        }
        Err(error) => {
            *cleanup_success = Some(false);
            (
                "worker_cleanup_failed".to_owned(),
                format!("sandbox cleanup failed: {error}; preceding_failure={code}: {diagnostic}"),
            )
        }
    }
}

fn cleanup_worker_resources(
    app_server: AppServerSession,
    sandbox: IsolatedCheckout,
) -> Result<(), String> {
    let app_server_error = app_server.cleanup().err();
    let sandbox_error = sandbox.cleanup().err();
    match (app_server_error, sandbox_error) {
        (None, None) => Ok(()),
        (Some(app_server), None) => Err(format!("App Server cleanup failed: {app_server}")),
        (None, Some(sandbox)) => Err(format!("sandbox cleanup failed: {sandbox}")),
        (Some(app_server), Some(sandbox)) => Err(format!(
            "App Server cleanup failed: {app_server}; sandbox cleanup failed: {sandbox}"
        )),
    }
}

fn cleanup_preflight_sandbox(sandbox: IsolatedCheckout, preceding_error: String) -> String {
    match sandbox.cleanup() {
        Ok(()) => preceding_error,
        Err(cleanup_error) => {
            format!("{preceding_error}; sandbox cleanup failed: {cleanup_error}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{Digest, EvidenceFailurePolicy};

    fn request(test_plan: Option<TestPlan>) -> WorkerRequest {
        WorkerRequest {
            root_task: "root task must not be copied".to_owned(),
            need_key: needle_core::NeedKey::new("trace.state-flow").unwrap(),
            need_body: "trace the requested state flow".to_owned(),
            preset: needle_core::Preset::new(
                "trace.state-flow",
                "Trace state flow",
                "configured preset text",
            ),
            repository_root: "repository".to_owned(),
            repository_snapshot: needle_core::RepositorySnapshot {
                identity_revision: needle_core::REPOSITORY_SNAPSHOT_IDENTITY_REVISION,
                repository_id: Digest::blake3("repository"),
                head_sha: "0".repeat(40),
                tracked_changes_digest: Digest::blake3("tracked"),
                untracked_content_digest: Digest::blake3("untracked"),
                source_digest: Digest::blake3("source"),
            },
            declared_test_plan: test_plan,
            trusted_test_execution: true,
            requested_artifact_kinds: Vec::new(),
            semantic_fragment: None,
        }
    }

    #[test]
    fn prompt_uses_preset_need_split_and_is_compact() {
        let request = request(None);
        assert_eq!(worker_prompt(&request), "trace the requested state flow");
        let system = worker_system_instructions(&request);
        assert!(system.contains("configured preset text"));
        assert!(system.contains("bounded read-only inspection"));
        assert!(system.contains("Needle reads and validates those declarations independently"));
        assert!(!system.contains("Get-Content"));
        assert!(system.contains("No TestPlan was declared"));
        assert!(system.contains("always return null byte_start and byte_end"));
        assert!(system.contains("line numbers are not byte offsets"));
        assert!(!system.contains("root task must not be copied"));
        assert!(system.len() <= 1400, "{} bytes", system.len());
    }

    #[test]
    fn focused_test_only_prompt_requires_location_without_execution() {
        let mut request = request(None);
        request.requested_artifact_kinds = vec![ArtifactKind::test_plan()];
        let system = worker_system_instructions(&request);
        assert!(system.contains("Locate and return one from an observed test definition"));
        assert!(system.contains("do not execute tests, builds, or scripts"));
        assert!(system.contains("Derive test argv only from an observed Cargo target"));
    }

    #[test]
    fn missing_artifact_failure_preserves_normalization_diagnostics() {
        let diagnostic = missing_artifact_diagnostic(
            &["test-plan".to_owned()],
            &[GroupDiagnostic { index: None, code: "schema_invalid:missing_field".to_owned() }],
        );
        assert!(diagnostic.contains("test-plan"));
        assert!(diagnostic.contains("schema_invalid:missing_field"));
    }

    #[test]
    fn omitted_test_plan_does_not_consume_a_repair_turn() {
        let response = serde_json::json!({
            "schema": needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID,
            "artifacts": []
        });

        assert!(!artifact_repair_is_actionable(&[ArtifactKind::test_plan()], &response));
    }

    #[test]
    fn attempted_near_valid_test_plan_may_use_the_single_repair_turn() {
        let response = serde_json::json!({
            "schema": needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID,
            "artifacts": [{
                "kind": "test-plan",
                "runner": "cargo",
                "argv": "cargo test focused"
            }]
        });

        assert!(artifact_repair_is_actionable(&[ArtifactKind::test_plan()], &response));
    }

    #[test]
    fn malformed_unrequested_artifact_is_removed_before_deserialization() {
        let mut response = serde_json::json!({
            "schema": needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID,
            "artifacts": [
                {"kind": "code-location", "locations": "malformed"},
                {"kind": "test-plan", "runner": "cargo"}
            ]
        });

        let discarded =
            discard_unrequested_semantic_values(&mut response, &[ArtifactKind::test_plan()]);

        assert_eq!(discarded, BTreeSet::from(["code-location".to_owned()]));
        assert_eq!(response["artifacts"].as_array().map(Vec::len), Some(1));
        assert_eq!(response["artifacts"][0]["kind"], "test-plan");
    }

    #[test]
    fn repair_prompt_repeats_the_single_required_artifact_kind() {
        let prompt = repair_with_test_diagnostic(
            &[GroupDiagnostic { index: None, code: "schema_invalid".to_owned() }],
            None,
            None,
            &[ArtifactKind::test_plan()],
        );

        assert!(prompt.contains("required_artifact=test-plan"));
    }

    #[test]
    fn semantic_prompt_contains_parent_owned_subject_obligations_and_inputs() {
        let ir = needle_core::NeedIr::parse(
            "@@need\n\
             @route locate.implementation\n\
             @subject symbol:\"answer\"\n\
             @require implementation-location selection=primary granularity=exact-location polarity=positive\n\
             @world source=current features=default\n\
             \n\
             Locate the implementation.\n\
             @@end",
        )
        .unwrap()
        .unwrap();
        let route = needle_core::built_in_route_contracts()
            .into_iter()
            .find(|route| route.route.as_str() == "locate.implementation")
            .unwrap();
        let need = needle_core::compile_need(&ir, Digest::blake3(b"repository"), &route).unwrap();
        let mut request = request(None);
        request.need_key = route.route;
        request.semantic_fragment = Some(needle_core::need_fragment(
            &need,
            need.required.clone(),
            vec![needle_core::ArtifactId(Digest::blake3(b"covered"))],
        ));

        let prompt = worker_prompt(&request);
        assert!(prompt.contains("subject symbol: \"answer\""));
        assert!(prompt.contains(
            "missing obligation implementation-location granularity=exact-location polarity=positive selection=primary"
        ));
        assert!(prompt.contains("certified input artifacts:"));
        assert!(prompt.contains("Do not repeat discovery already covered"));
    }

    #[test]
    fn semantic_trace_output_schema_binds_the_compiled_scenario() {
        let ir = needle_core::NeedIr::parse(
            "@@need\n\
             @route trace.state-flow\n\
             @subject cli-option:\"--crlf\"\n\
             @require implementation-location selection=primary granularity=exact-location\n\
             @require runtime-flow scenario=default completeness=contract-complete granularity=stepwise\n\
             @world source=current features=default\n\
             \n\
             Trace the runtime flow.\n\
             @@end",
        )
        .unwrap()
        .unwrap();
        let route = needle_core::built_in_route_contracts()
            .into_iter()
            .find(|route| route.route.as_str() == "trace.state-flow")
            .unwrap();
        let need = needle_core::compile_need(&ir, Digest::blake3(b"repository"), &route).unwrap();
        let mut request = request(None);
        request.semantic_fragment =
            Some(needle_core::need_fragment(&need, need.required.clone(), Vec::new()));
        let schema = output_schema(&request, &[ArtifactKind::behavior_trace()]);
        assert_eq!(
            schema["properties"]["artifacts"]["items"]["properties"]["scenario"]["const"],
            serde_json::json!("default")
        );
    }

    #[test]
    fn diagnostic_contract_uses_the_same_prompt_and_schema_builders() {
        let ir = needle_core::NeedIr::parse(
            "@@need\n\
             @route locate.implementation\n\
             @subject cli-option:\"--glob-case-insensitive\"\n\
             @require implementation-location selection=primary granularity=exact-location polarity=positive\n\
             @prefer focused-tests selection=representative\n\
             @world source=current features=default\n\
             \n\
             Locate the implementation.\n\
             @@end",
        )
        .unwrap()
        .unwrap();
        let route = needle_core::built_in_route_contracts()
            .into_iter()
            .find(|route| route.route.as_str() == "locate.implementation")
            .unwrap();
        let need = needle_core::compile_need(&ir, Digest::blake3(b"repository"), &route).unwrap();
        let mut request = request(Some(TestPlan {
            runner: "cargo".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned(), "focused".to_owned()],
            cwd_relative: ".".to_owned(),
            test_identifier: "focused".to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        }));
        request.need_key = route.route;
        request.semantic_fragment =
            Some(needle_core::need_fragment(&need, need.required.clone(), Vec::new()));

        let contract = CodexWorker::diagnostic_contract(&request).unwrap();
        assert_eq!(contract.output_schema_id, needle_core::SEMANTIC_ARTIFACT_RESULT_SCHEMA_ID);
        assert_eq!(contract.requested_artifact_kinds, vec![ArtifactKind::code_location()]);
        assert!(!contract.prompt.contains("missing obligation focused-tests"));
        assert!(
            contract
                .system_instructions
                .contains("test locations are parent-owned; do not return them as code-location")
        );
        assert_eq!(contract.prompt, worker_prompt(&request));
        assert_eq!(contract.system_instructions, worker_system_instructions(&request));
        assert_eq!(
            contract.output_schema,
            output_schema(&request, &requested_model_artifact_kinds(&request))
        );
        assert_eq!(contract.prompt_digest, Digest::blake3(contract.prompt.as_bytes()));
    }

    #[test]
    fn declared_test_is_the_only_executable_command_in_prompt() {
        let plan = TestPlan {
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
            test_identifier: "misc::glob_always_case_insensitive".to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        let request = request(Some(plan));
        let prompt = worker_prompt(&request);
        assert!(prompt.contains(
            "cargo test --test integration misc::glob_always_case_insensitive -- --exact"
        ));
        assert!(worker_system_instructions(&request).contains("isolated checkout"));
    }

    #[test]
    fn declared_test_plan_is_input_not_repeated_worker_output() {
        let plan = TestPlan {
            runner: "cargo".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned(), "focused".to_owned()],
            cwd_relative: ".".to_owned(),
            test_identifier: "focused".to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        let request = request(Some(plan));
        assert_eq!(requested_model_artifact_kinds(&request), vec![ArtifactKind::behavior_trace()]);
        let schema = worker_output_schema(&requested_model_artifact_kinds(&request));
        assert!(schema["properties"].get("test_plan").is_none());
        assert!(!worker_system_instructions(&request).contains(r#""test_plan""#));
    }

    #[test]
    fn locate_route_still_requires_an_explicit_code_location() {
        let mut request = request(None);
        request.need_key = needle_core::NeedKey::new("locate.implementation").unwrap();
        request.preset = needle_core::Preset::new(
            "locate.implementation",
            "Locate implementation",
            "configured locate preset",
        );

        assert_eq!(
            requested_model_artifact_kinds(&request),
            vec![ArtifactKind::code_location(), ArtifactKind::test_plan()]
        );
    }

    #[test]
    fn approval_wait_does_not_consume_the_repair_budget() {
        assert_eq!(
            remaining_worker_budget(180, Duration::from_secs(150), Duration::from_secs(120),),
            Duration::from_secs(150)
        );
        assert_eq!(
            remaining_worker_budget(180, Duration::from_secs(200), Duration::ZERO),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn discard_policy_disables_test_and_artifact_repair_turns() {
        assert!(!repair_required(EvidenceFailurePolicy::DiscardInvalidFact, true, false));
        assert!(!repair_required(EvidenceFailurePolicy::DiscardInvalidFact, false, true));
        assert!(repair_required(EvidenceFailurePolicy::RepairOnce, true, false));
        assert!(repair_required(EvidenceFailurePolicy::RepairOnce, false, true));
    }

    #[test]
    fn partial_worker_does_not_reexecute_a_reused_test_node() {
        let plan = TestPlan {
            runner: "cargo".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned(), "focused".to_owned()],
            cwd_relative: ".".to_owned(),
            test_identifier: "focused".to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        let mut request = request(Some(plan));
        request.requested_artifact_kinds = vec![needle_core::ArtifactKind::behavior_trace()];
        assert_eq!(worker_prompt(&request), request.need_body);
        assert!(effective_test_plan(&request).is_none());
        assert!(worker_system_instructions(&request).contains("Do not run tests"));
    }

    #[test]
    fn cumulative_usage_is_not_double_counted_across_repair() {
        let mut usage = Usage::default();
        let first = crate::app_server::AppServerTurn {
            response: Value::Null,
            input_tokens: Some(10),
            cached_input_tokens: Some(4),
            output_tokens: Some(2),
            approval_wait: Duration::from_secs(120),
            command_evidence: Vec::new(),
            observation_trace: Default::default(),
            file_change_approvals_granted: 0,
        };
        let second = crate::app_server::AppServerTurn {
            response: Value::Null,
            input_tokens: Some(20),
            cached_input_tokens: Some(8),
            output_tokens: Some(3),
            approval_wait: Duration::ZERO,
            command_evidence: Vec::new(),
            observation_trace: Default::default(),
            file_change_approvals_granted: 0,
        };
        usage.absorb_total(&first);
        usage.absorb_total(&second);
        assert_eq!(usage.input_tokens, Some(20));
        assert_eq!(usage.cached_input_tokens, Some(8));
        assert_eq!(usage.output_tokens, Some(3));
    }

    #[test]
    fn valid_test_evidence_survives_an_artifact_repair_turn() {
        let plan = TestPlan {
            runner: "cargo".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned(), "focused".to_owned()],
            cwd_relative: ".".to_owned(),
            test_identifier: "focused".to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        let mut evidence = vec![needle_core::CommandExecutionEvidence {
            id: "evidence".to_owned(),
            approval_id: "approval".to_owned(),
            argv: plan.argv.clone(),
            cwd: "checkout".to_owned(),
            source_snapshot_digest: Digest::blake3("snapshot"),
            runner: "cargo".to_owned(),
            runner_version: None,
            exit_status: Some(0),
            duration_ms: 1,
            output_digest: Digest::blake3("output"),
            output_preview: "running 1 test\ntest focused ... ok".to_owned(),
            test_identifier: Some("focused".to_owned()),
            tests_executed: Some(1),
            infrastructure_failure: None,
        }];

        accumulate_command_evidence(&mut evidence, Vec::new());

        assert!(test_evidence_error(Some(&plan), &evidence).is_none());
    }

    #[test]
    fn sanitized_worker_environment_excludes_credentials_and_proxy_overrides() {
        let environment = sanitized_environment();
        assert!(environment.keys().all(|key| {
            let upper = key.to_ascii_uppercase();
            !upper.contains("TOKEN")
                && !upper.contains("SECRET")
                && !upper.contains("PASSWORD")
                && !upper.contains("API_KEY")
                && !upper.contains("PROXY")
        }));
    }
}
