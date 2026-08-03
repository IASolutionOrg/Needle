use crate::app_server::AppServerSession;
use crate::worker::CodexWorker;
use needle_core::{
    AcceptanceCoverage, AllowedPathScope, ChangeId, ChangeRequest, Digest, MAX_ACCEPTANCE_CRITERIA,
    MAX_ALLOWED_PATHS, MAX_CHANGE_ARTIFACTS, MAX_CHANGE_CLAIMS, MAX_CHANGE_TASK_BYTES,
    MAX_PATCH_DIFF_BYTES, MAX_PATCH_FILES, MAX_PATCH_FINAL_BYTES, PatchArtifact, PatchFile,
    PatchOperation, VerificationStatus, WorkerConfig,
};
use needle_runtime::{
    IsolatedCheckout, PatchFileBlob, RuntimeStore, capture_git_snapshot, materialize_patch_artifact,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_CONTEXT_ITEMS: usize = 48;
const MAX_CONTEXT_ITEM_BYTES: usize = 8 * 1024;
const MAX_SUMMARY_BYTES: usize = 4 * 1024;
const MAX_RISK_BYTES: usize = 1024;
const MAX_RISKS: usize = 8;
const MAX_COVERAGE_TEXT_BYTES: usize = 2 * 1024;
static CHANGE_NONCE: AtomicU64 = AtomicU64::new(1);

struct RepairSeed {
    previous_patch: PatchArtifact,
    previous_blobs: Vec<PatchFileBlob>,
    findings: Vec<String>,
}

struct PatchRunContext<'a> {
    started: Instant,
    change_id: ChangeId,
    request_digest: Digest,
    repair: Option<&'a RepairSeed>,
    codex_version: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchContextItem {
    pub label: String,
    pub content: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareChangeOutcome {
    pub request_digest: Digest,
    pub change_id: ChangeId,
    pub patch_id: needle_core::PatchId,
    pub summary: String,
    pub changed_files: Vec<PatchFile>,
    pub acceptance_coverage: Vec<AcceptanceCoverage>,
    pub residual_risks: Vec<String>,
    pub verification_status: VerificationStatus,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub observed_repository_files: u32,
    pub observation_gaps: u32,
    pub duration_ms: u64,
}

#[derive(Clone, Debug)]
pub struct CodexPatchWorker {
    data_directory: PathBuf,
    codex_home: Option<PathBuf>,
    cancellation: Option<Arc<AtomicBool>>,
}

impl CodexPatchWorker {
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

    pub fn prepare(
        &self,
        config: &WorkerConfig,
        repository_root: &Path,
        request: &ChangeRequest,
        context: &[PatchContextItem],
    ) -> Result<PrepareChangeOutcome, String> {
        let started = Instant::now();
        validate_change_request(request, context)?;
        let isolation = CodexWorker::verify_isolation(&config.executable)?;
        if !isolation.verified() {
            return Err(format!(
                "patch worker isolation is not verified for Codex {}",
                isolation.codex_version
            ));
        }
        let repository_root = fs::canonicalize(repository_root)
            .map_err(|error| format!("cannot resolve change repository: {error}"))?;
        let sandbox = IsolatedCheckout::materialize(
            &repository_root,
            &self.data_directory.join("change-runs"),
        )
        .map_err(|error| error.to_string())?;
        let request_digest = request.digest(sandbox.snapshot().source_digest);
        let change_id = unique_change_id(request_digest);
        let store = RuntimeStore::new(self.data_directory.join("needle.sqlite3"));
        if let Err(error) = store.initialize().and_then(|()| {
            store.record_change_request_with_provenance(
                &change_id,
                sandbox.snapshot().repository_id,
                sandbox.snapshot().source_digest,
                request_digest,
                request,
                config.role_profile_provenance.as_ref(),
            )
        }) {
            return cleanup_sandbox_after_error(sandbox, error.to_string());
        }
        let outcome = self.prepare_in_sandbox(
            config,
            request,
            context,
            sandbox,
            PatchRunContext {
                started,
                change_id: change_id.clone(),
                request_digest,
                repair: None,
                codex_version: &isolation.codex_version,
            },
        );
        record_failed_patch_run(&store, &change_id, outcome)
    }

    pub fn repair(
        &self,
        config: &WorkerConfig,
        repository_root: &Path,
        change_id: &ChangeId,
    ) -> Result<PrepareChangeOutcome, String> {
        let started = Instant::now();
        let store = RuntimeStore::new(self.data_directory.join("needle.sqlite3"));
        store.initialize().map_err(|error| error.to_string())?;
        let prepared = store
            .prepared_change(change_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "repair change was not found".to_owned())?;
        if prepared.patch.revision != 1 || prepared.state != "repairable" {
            return Err("only the first repairable patch revision can be repaired".to_owned());
        }
        let verification = store
            .latest_verification_artifact(change_id)
            .map_err(|error| error.to_string())?
            .filter(|artifact| {
                artifact.patch_id == prepared.patch.id
                    && artifact.verdict == VerificationStatus::Repairable
                    && artifact.is_canonical()
            })
            .ok_or_else(|| "latest patch has no canonical repairable verdict".to_owned())?;
        validate_change_request(&prepared.request, &[])?;
        let isolation = CodexWorker::verify_isolation(&config.executable)?;
        if !isolation.verified() {
            return Err(format!(
                "patch worker isolation is not verified for Codex {}",
                isolation.codex_version
            ));
        }
        let repository_root = fs::canonicalize(repository_root)
            .map_err(|error| format!("cannot resolve change repository: {error}"))?;
        let (_, source_snapshot) =
            capture_git_snapshot(&repository_root).map_err(|error| error.to_string())?;
        if source_snapshot.source_digest != prepared.source_snapshot {
            return Err("active source snapshot drifted from the repair base".to_owned());
        }
        let previous_blobs =
            store.patch_file_blobs(prepared.patch.id).map_err(|error| error.to_string())?;
        let sandbox = IsolatedCheckout::materialize(
            &repository_root,
            &self.data_directory.join("change-runs"),
        )
        .map_err(|error| error.to_string())?;
        if let Err(error) = store.begin_change_repair(change_id, prepared.patch.id) {
            return cleanup_sandbox_after_error(sandbox, error.to_string());
        }
        let seed = RepairSeed {
            previous_patch: prepared.patch,
            previous_blobs,
            findings: verification.findings,
        };
        let outcome = self.prepare_in_sandbox(
            config,
            &prepared.request,
            &[],
            sandbox,
            PatchRunContext {
                started,
                change_id: change_id.clone(),
                request_digest: prepared.request_digest,
                repair: Some(&seed),
                codex_version: &isolation.codex_version,
            },
        );
        record_failed_patch_run(&store, change_id, outcome)
    }

    fn prepare_in_sandbox(
        &self,
        config: &WorkerConfig,
        request: &ChangeRequest,
        context: &[PatchContextItem],
        sandbox: IsolatedCheckout,
        run: PatchRunContext<'_>,
    ) -> Result<PrepareChangeOutcome, String> {
        let PatchRunContext { started, change_id, request_digest, repair, codex_version } = run;
        let baseline = match capture_tree(sandbox.checkout_root()) {
            Ok(baseline) => baseline,
            Err(error) => return cleanup_sandbox_after_error(sandbox, error),
        };
        let baseline_blobs = match preserve_baseline_blobs(
            sandbox.checkout_root(),
            sandbox.temp_root(),
            request,
            &baseline,
        ) {
            Ok(blobs) => blobs,
            Err(error) => return cleanup_sandbox_after_error(sandbox, error),
        };
        if let Some(repair) = repair
            && let Err(error) = materialize_patch_artifact(
                sandbox.checkout_root(),
                &repair.previous_patch,
                &repair.previous_blobs,
            )
        {
            return cleanup_sandbox_after_error(
                sandbox,
                format!("cannot materialize the repair base patch: {error}"),
            );
        }
        let store = RuntimeStore::new(self.data_directory.join("needle.sqlite3"));
        if let Err(error) = store.initialize() {
            return cleanup_sandbox_after_error(sandbox, error.to_string());
        }
        let instructions = patcher_instructions(request, repair.is_some());
        let mut session = match AppServerSession::start_patch(
            config,
            self.codex_home.as_deref(),
            &instructions,
            sandbox.checkout_root(),
            sandbox.target_root(),
            sandbox.temp_root(),
            sandbox.snapshot().source_digest,
            sandbox.snapshot().repository_id,
            store.clone(),
        ) {
            Ok(session) => session,
            Err(error) => return cleanup_sandbox_after_error(sandbox, error),
        };
        session.fail_fast_on_pending_approvals();
        let turn = match session.run_turn_cancellable(
            &patcher_prompt(request, context, repair),
            &patcher_output_schema(),
            Duration::from_secs(config.timeout_seconds),
            self.cancellation.as_deref(),
        ) {
            Ok(turn) => turn,
            Err(failure) => {
                let cleanup = session.cleanup();
                return cleanup_all_after_error(
                    sandbox,
                    cleanup,
                    format!("patch worker turn failed: {}", failure.diagnostic),
                );
            }
        };
        if let Err(error) = session.cleanup() {
            return cleanup_sandbox_after_error(
                sandbox,
                format!("patch worker session cleanup failed: {error}"),
            );
        }
        let declared = match serde_json::from_value::<DeclaredPatchOutput>(turn.response.clone()) {
            Ok(declared) => declared,
            Err(error) => {
                return cleanup_sandbox_after_error(
                    sandbox,
                    format!("patch worker output is invalid: {error}"),
                );
            }
        };
        if turn.file_change_approvals_granted != 1 {
            return cleanup_sandbox_after_error(
                sandbox,
                "patch worker did not use exactly one file-change approval".to_owned(),
            );
        }
        if let Err(error) = validate_declared_output(request, &declared) {
            return cleanup_sandbox_after_error(sandbox, error);
        }
        let observed = match capture_tree(sandbox.checkout_root()) {
            Ok(observed) => observed,
            Err(error) => return cleanup_sandbox_after_error(sandbox, error),
        };
        let (files, blobs) = match observe_patch(
            sandbox.checkout_root(),
            request,
            &baseline,
            &observed,
            &baseline_blobs,
        ) {
            Ok(patch) => patch,
            Err(error) => return cleanup_sandbox_after_error(sandbox, error),
        };
        let patch_id = PatchArtifact::compute_id(sandbox.snapshot().source_digest, &files);
        let discrepancies = declared_discrepancies(&declared.changed_files, &files);
        let patch = PatchArtifact {
            id: patch_id,
            change_id: change_id.clone(),
            revision: repair.map_or(1, |_| 2),
            source_snapshot: sandbox.snapshot().source_digest,
            files: files.clone(),
            summary: declared.summary.clone(),
            acceptance_coverage: declared.acceptance_coverage.clone(),
            residual_risks: declared.residual_risks.clone(),
            declared_output_digest: Digest::blake3(
                serde_json::to_vec(&turn.response).map_err(|error| error.to_string())?,
            ),
            discrepancies,
        };
        if let Err(error) = store.record_prepared_change_with_provenance(
            sandbox.snapshot().repository_id,
            request_digest,
            request,
            &patch,
            &turn.response,
            &blobs,
            config.role_profile_provenance.as_ref(),
        ) {
            return cleanup_sandbox_after_error(
                sandbox,
                format!("cannot persist patch before cleanup: {error}"),
            );
        }
        if let Err(error) = store.record_patch_attempt_with_provenance(
            &change_id,
            patch_id,
            &json!({
                "model": config.model,
                "reasoning": config.reasoning,
                "service_tier": config.service_tier,
                "codex_version": codex_version,
                "revision": patch.revision
            }),
            &json!({
                "input_tokens": turn.input_tokens,
                "cached_input_tokens": turn.cached_input_tokens,
                "output_tokens": turn.output_tokens,
                "duration_ms": started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
            }),
            None,
            current_unix_ms(),
            config.role_profile_provenance.as_ref(),
        ) {
            let reason = format!("cannot persist patch attempt accounting: {error}");
            return cleanup_sandbox_after_error(sandbox, reason);
        }
        if let Err(error) = sandbox.cleanup() {
            return Err(format!(
                "patch persisted but disposable checkout cleanup failed closed: {error}"
            ));
        }
        Ok(PrepareChangeOutcome {
            request_digest,
            change_id,
            patch_id,
            summary: patch.summary,
            changed_files: files,
            acceptance_coverage: patch.acceptance_coverage,
            residual_risks: patch.residual_risks,
            verification_status: VerificationStatus::NotRequested,
            input_tokens: turn.input_tokens,
            cached_input_tokens: turn.cached_input_tokens,
            output_tokens: turn.output_tokens,
            observed_repository_files: turn
                .observation_trace
                .observed_files
                .len()
                .try_into()
                .unwrap_or(u32::MAX),
            observation_gaps: turn.observation_trace.gaps.len().try_into().unwrap_or(u32::MAX),
            duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        })
    }
}

fn record_failed_patch_run(
    store: &RuntimeStore,
    change_id: &ChangeId,
    outcome: Result<PrepareChangeOutcome, String>,
) -> Result<PrepareChangeOutcome, String> {
    match outcome {
        Ok(outcome) => Ok(outcome),
        Err(error) => match store.record_change_failure(change_id, &error) {
            Ok(()) => Err(error),
            Err(persistence) => {
                Err(format!("{error}; cannot persist the failed change state: {persistence}"))
            }
        },
    }
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeclaredPatchOutput {
    summary: String,
    changed_files: Vec<DeclaredChangedFile>,
    acceptance_coverage: Vec<AcceptanceCoverage>,
    residual_risks: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeclaredChangedFile {
    path: String,
    operation: PatchOperation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TreeEntryKind {
    File,
    Symlink,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TreeEntry {
    kind: TreeEntryKind,
    digest: Digest,
    bytes: u64,
}

fn validate_change_request(
    request: &ChangeRequest,
    context: &[PatchContextItem],
) -> Result<(), String> {
    if request.task.trim().is_empty() || request.task.len() > MAX_CHANGE_TASK_BYTES {
        return Err("change task must contain 1 to 8192 bytes".to_owned());
    }
    if request.acceptance_criteria.is_empty()
        || request.acceptance_criteria.len() > MAX_ACCEPTANCE_CRITERIA
        || request.acceptance_criteria.iter().any(|item| item.trim().is_empty())
    {
        return Err("change requires 1 to 8 non-empty acceptance criteria".to_owned());
    }
    if request.allowed_paths.is_empty() || request.allowed_paths.len() > MAX_ALLOWED_PATHS {
        return Err("change requires 1 to 16 allowed paths".to_owned());
    }
    let mut unique_paths = BTreeSet::new();
    for allowed in &request.allowed_paths {
        let normalized = safe_relative_path(&allowed.path)?;
        if is_protected_path(&normalized) {
            return Err(format!("protected path is not writable: `{normalized}`"));
        }
        if !unique_paths.insert(normalized) {
            return Err("allowed paths contain a duplicate scope".to_owned());
        }
    }
    if request.artifact_ids.len() > MAX_CHANGE_ARTIFACTS
        || request.claim_ids.len() > MAX_CHANGE_CLAIMS
    {
        return Err("change context exceeds artifact or claim bounds".to_owned());
    }
    if context.len() > MAX_CONTEXT_ITEMS
        || context
            .iter()
            .any(|item| item.label.trim().is_empty() || item.content.len() > MAX_CONTEXT_ITEM_BYTES)
    {
        return Err("patcher context exceeds its bounded projection".to_owned());
    }
    Ok(())
}

fn validate_declared_output(
    request: &ChangeRequest,
    output: &DeclaredPatchOutput,
) -> Result<(), String> {
    if output.summary.trim().is_empty() || output.summary.len() > MAX_SUMMARY_BYTES {
        return Err("patch summary must contain 1 to 4096 bytes".to_owned());
    }
    if output.changed_files.len() > MAX_PATCH_FILES
        || output.acceptance_coverage.len() != request.acceptance_criteria.len()
        || output.residual_risks.len() > MAX_RISKS
        || output.residual_risks.iter().any(|risk| risk.len() > MAX_RISK_BYTES)
        || output.acceptance_coverage.iter().any(|coverage| {
            coverage.criterion.len() > MAX_COVERAGE_TEXT_BYTES
                || coverage.evidence.len() > MAX_COVERAGE_TEXT_BYTES
        })
    {
        return Err("patch worker output violates cardinality bounds".to_owned());
    }
    let declared_criteria = output
        .acceptance_coverage
        .iter()
        .map(|item| item.criterion.as_str())
        .collect::<BTreeSet<_>>();
    let requested_criteria =
        request.acceptance_criteria.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if declared_criteria != requested_criteria {
        return Err(
            "patch worker did not account for each acceptance criterion exactly once".to_owned()
        );
    }
    let declared_files = output
        .changed_files
        .iter()
        .map(|file| file.path.replace('\\', "/"))
        .collect::<BTreeSet<_>>();
    if declared_files.len() != output.changed_files.len() {
        return Err("patch worker declared a changed path more than once".to_owned());
    }
    Ok(())
}

fn patcher_instructions(request: &ChangeRequest, repair: bool) -> String {
    format!(
        "You are Needle's isolated {}patch worker. Work only inside the disposable checkout. Modify only the explicitly allowed path scopes. Never access the network, credentials, Git metadata, external tools, hooks, plugins, MCP, project instructions, or other agents. Read-only repository inspection commands are allowed when necessary. Do not run tests unless the request explicitly requires a supplied certified command; no test command is supplied in this turn. Make the smallest complete UTF-8 text change satisfying the task. The runtime, not your response, determines the authoritative filesystem patch. Return only JSON matching the output schema. Allowed path scopes: {}",
        if repair { "one-shot repair " } else { "" },
        request
            .allowed_paths
            .iter()
            .map(|path| format!("{} ({:?})", path.path, path.scope))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn patcher_prompt(
    request: &ChangeRequest,
    context: &[PatchContextItem],
    repair: Option<&RepairSeed>,
) -> String {
    let mut prompt = String::from("Task:\n");
    prompt.push_str(request.task.trim());
    prompt.push_str("\n\nAcceptance criteria:\n");
    for criterion in &request.acceptance_criteria {
        prompt.push_str("- ");
        prompt.push_str(criterion.trim());
        prompt.push('\n');
    }
    if !request.constraints.is_empty() {
        prompt.push_str("\nConstraints:\n");
        for constraint in &request.constraints {
            prompt.push_str("- ");
            prompt.push_str(constraint.trim());
            prompt.push('\n');
        }
    }
    if !context.is_empty() {
        prompt.push_str("\nValidated bounded context:\n");
        for item in context {
            prompt.push_str("[context ");
            prompt.push_str(&item.label);
            prompt.push_str("]\n");
            prompt.push_str(&item.content);
            prompt.push_str("\n[/context]\n");
        }
    }
    if let Some(repair) = repair {
        prompt.push_str(
            "\nOne-shot repair context:\nThe disposable checkout already contains the previous filesystem patch. Correct only the independently reported findings below. You do not have the prior patcher transcript and must preserve already-correct behavior.\n",
        );
        for finding in &repair.findings {
            prompt.push_str("- ");
            prompt.push_str(finding.trim());
            prompt.push('\n');
        }
    }
    prompt
}

fn patcher_output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "changed_files", "acceptance_coverage", "residual_risks"],
        "properties": {
            "summary": {"type": "string", "minLength": 1, "maxLength": MAX_SUMMARY_BYTES},
            "changed_files": {
                "type": "array", "maxItems": MAX_PATCH_FILES,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["path", "operation"],
                    "properties": {
                        "path": {"type": "string", "minLength": 1, "maxLength": 1024},
                        "operation": {"type": "string", "enum": ["create", "update", "delete"]}
                    }
                }
            },
            "acceptance_coverage": {
                "type": "array", "minItems": 1, "maxItems": MAX_ACCEPTANCE_CRITERIA,
                "items": {
                    "type": "object", "additionalProperties": false,
                    "required": ["criterion", "status", "evidence"],
                    "properties": {
                        "criterion": {"type": "string", "minLength": 1, "maxLength": 2048},
                        "status": {"type": "string", "enum": ["addressed", "partial", "unaddressed"]},
                        "evidence": {"type": "string", "maxLength": 2048}
                    }
                }
            },
            "residual_risks": {
                "type": "array", "maxItems": MAX_RISKS,
                "items": {"type": "string", "maxLength": MAX_RISK_BYTES}
            }
        }
    })
}

fn capture_tree(root: &Path) -> Result<BTreeMap<String, TreeEntry>, String> {
    let mut tree = BTreeMap::new();
    let mut directories = vec![root.to_path_buf()];
    let mut buffer = [0_u8; 64 * 1024];
    while let Some(directory) = directories.pop() {
        let mut entries = fs::read_dir(&directory)
            .map_err(|error| format!("cannot inspect {}: {error}", directory.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("cannot inspect {}: {error}", directory.display()))?;
        entries.sort_unstable_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| "checkout traversal escaped its root".to_owned())?;
            let relative = path_to_slashes(relative)?;
            if relative == ".git" || relative.starts_with(".git/") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&path).map_err(|error| {
                    format!("cannot inspect symlink {}: {error}", path.display())
                })?;
                tree.insert(
                    relative,
                    TreeEntry {
                        kind: TreeEntryKind::Symlink,
                        digest: Digest::blake3(target.to_string_lossy().as_bytes()),
                        bytes: 0,
                    },
                );
            } else if metadata.is_dir() {
                directories.push(path);
            } else if metadata.is_file() {
                let mut file = File::open(&path)
                    .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
                let mut hasher = blake3::Hasher::new();
                loop {
                    let read = file
                        .read(&mut buffer)
                        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
                    if read == 0 {
                        break;
                    }
                    hasher.update(&buffer[..read]);
                }
                tree.insert(
                    relative,
                    TreeEntry {
                        kind: TreeEntryKind::File,
                        digest: Digest(*hasher.finalize().as_bytes()),
                        bytes: metadata.len(),
                    },
                );
            } else {
                return Err(format!("unsupported filesystem entry `{relative}`"));
            }
        }
    }
    Ok(tree)
}

fn observe_patch(
    checkout_root: &Path,
    request: &ChangeRequest,
    before: &BTreeMap<String, TreeEntry>,
    after: &BTreeMap<String, TreeEntry>,
    baseline_blobs: &BTreeMap<String, PathBuf>,
) -> Result<(Vec<PatchFile>, Vec<PatchFileBlob>), String> {
    let paths = before.keys().chain(after.keys()).cloned().collect::<BTreeSet<_>>();
    let changed =
        paths.into_iter().filter(|path| before.get(path) != after.get(path)).collect::<Vec<_>>();
    if changed.is_empty() {
        return Err("patch worker produced no filesystem change".to_owned());
    }
    if changed.len() > MAX_PATCH_FILES {
        return Err(format!("patch changes more than {MAX_PATCH_FILES} files"));
    }
    let normalized_allowed = request
        .allowed_paths
        .iter()
        .map(|allowed| Ok((safe_relative_path(&allowed.path)?, allowed.scope)))
        .collect::<Result<Vec<_>, String>>()?;
    let mut files = Vec::with_capacity(changed.len());
    let mut blobs = Vec::with_capacity(changed.len());
    let mut projected_bytes = 0_u64;
    let mut final_bytes = 0_u64;
    for path in changed {
        if is_protected_path(&path) {
            return Err(format!("patch changed protected path `{path}`"));
        }
        if !normalized_allowed.iter().any(|(allowed, scope)| {
            path == *allowed
                || (*scope == AllowedPathScope::Subtree
                    && path.strip_prefix(allowed).is_some_and(|suffix| suffix.starts_with('/')))
        }) {
            return Err(format!("patch changed path outside the declared scope: `{path}`"));
        }
        if before.get(&path).is_some_and(|entry| entry.kind == TreeEntryKind::Symlink)
            || after.get(&path).is_some_and(|entry| entry.kind == TreeEntryKind::Symlink)
        {
            return Err(format!("patch changes a symlink: `{path}`"));
        }
        let operation = match (before.get(&path), after.get(&path)) {
            (None, Some(_)) => PatchOperation::Create,
            (Some(_), None) => PatchOperation::Delete,
            (Some(_), Some(_)) => PatchOperation::Update,
            (None, None) => unreachable!("changed path must exist in one tree"),
        };
        let before_bytes = before.get(&path).map_or(0, |entry| entry.bytes);
        let after_bytes = after.get(&path).map_or(0, |entry| entry.bytes);
        projected_bytes = projected_bytes.saturating_add(before_bytes).saturating_add(after_bytes);
        final_bytes = final_bytes.saturating_add(after_bytes);
        if projected_bytes > MAX_PATCH_DIFF_BYTES as u64 {
            return Err(format!("projected patch exceeds {MAX_PATCH_DIFF_BYTES} bytes"));
        }
        if final_bytes > MAX_PATCH_FINAL_BYTES as u64 {
            return Err(format!("patch final content exceeds {MAX_PATCH_FINAL_BYTES} bytes"));
        }
        let before_blob = read_baseline_text(&path, before.get(&path), baseline_blobs)?;
        let after_blob = read_after_text(checkout_root, &path, after.get(&path))?;
        files.push(PatchFile {
            path: path.clone(),
            operation,
            before_digest: before.get(&path).map(|entry| entry.digest),
            after_digest: after.get(&path).map(|entry| entry.digest),
            before_bytes,
            after_bytes,
        });
        blobs.push(PatchFileBlob { path, before: before_blob, after: after_blob });
    }
    let deleted = files
        .iter()
        .filter(|file| file.operation == PatchOperation::Delete)
        .filter_map(|file| file.before_digest)
        .collect::<BTreeSet<_>>();
    if files.iter().any(|file| {
        file.operation == PatchOperation::Create
            && file.after_digest.is_some_and(|digest| deleted.contains(&digest))
    }) {
        return Err("patch contains a rename, which is not supported".to_owned());
    }
    Ok((files, blobs))
}

fn preserve_baseline_blobs(
    checkout_root: &Path,
    temp_root: &Path,
    request: &ChangeRequest,
    tree: &BTreeMap<String, TreeEntry>,
) -> Result<BTreeMap<String, PathBuf>, String> {
    let normalized_allowed = request
        .allowed_paths
        .iter()
        .map(|allowed| Ok((safe_relative_path(&allowed.path)?, allowed.scope)))
        .collect::<Result<Vec<_>, String>>()?;
    let blob_root = temp_root.join("patch-baseline");
    fs::create_dir_all(&blob_root)
        .map_err(|error| format!("cannot create patch baseline store: {error}"))?;
    let mut output = BTreeMap::new();
    for (path, entry) in tree {
        let selected = normalized_allowed.iter().any(|(allowed, scope)| {
            path == allowed
                || (*scope == AllowedPathScope::Subtree
                    && path.strip_prefix(allowed).is_some_and(|suffix| suffix.starts_with('/')))
        });
        if !selected
            || entry.kind != TreeEntryKind::File
            || entry.bytes > MAX_PATCH_DIFF_BYTES as u64
        {
            continue;
        }
        let blob = blob_root.join(entry.digest.to_hex());
        if !blob.exists() {
            fs::copy(checkout_root.join(path.replace('/', std::path::MAIN_SEPARATOR_STR)), &blob)
                .map_err(|error| format!("cannot preserve baseline file `{path}`: {error}"))?;
        }
        output.insert(path.clone(), blob);
    }
    Ok(output)
}

fn read_baseline_text(
    relative: &str,
    entry: Option<&TreeEntry>,
    baseline_blobs: &BTreeMap<String, PathBuf>,
) -> Result<Option<Vec<u8>>, String> {
    let Some(entry) = entry else {
        return Ok(None);
    };
    if entry.kind != TreeEntryKind::File {
        return Err(format!("patch entry is not a regular file: `{relative}`"));
    }
    let path = baseline_blobs
        .get(relative)
        .ok_or_else(|| format!("baseline file exceeds the supported patch bound: `{relative}`"))?;
    let bytes = fs::read(path)
        .map_err(|error| format!("cannot read baseline file `{relative}`: {error}"))?;
    validate_utf8_text(relative, &bytes)?;
    Ok(Some(bytes))
}

fn read_after_text(
    checkout_root: &Path,
    relative: &str,
    entry: Option<&TreeEntry>,
) -> Result<Option<Vec<u8>>, String> {
    let Some(entry) = entry else {
        return Ok(None);
    };
    if entry.kind != TreeEntryKind::File {
        return Err(format!("patch entry is not a regular file: `{relative}`"));
    }
    let bytes = fs::read(checkout_root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)))
        .map_err(|error| format!("cannot read patch file `{relative}`: {error}"))?;
    validate_utf8_text(relative, &bytes)?;
    Ok(Some(bytes))
}

fn validate_utf8_text(path: &str, bytes: &[u8]) -> Result<(), String> {
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        return Err(format!("patch changes non-UTF-8 or binary file `{path}`"));
    }
    Ok(())
}

fn declared_discrepancies(declared: &[DeclaredChangedFile], observed: &[PatchFile]) -> Vec<String> {
    let declared = declared
        .iter()
        .map(|file| (file.path.replace('\\', "/"), file.operation))
        .collect::<BTreeSet<_>>();
    let observed =
        observed.iter().map(|file| (file.path.clone(), file.operation)).collect::<BTreeSet<_>>();
    declared
        .difference(&observed)
        .map(|(path, _)| format!("declared but not observed: {path}"))
        .chain(
            observed
                .difference(&declared)
                .map(|(path, _)| format!("observed but not declared: {path}")),
        )
        .collect()
}

fn safe_relative_path(value: &str) -> Result<String, String> {
    let replaced = value.replace('\\', "/");
    let path = Path::new(&replaced);
    if replaced.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        })
    {
        return Err(format!("unsafe relative path `{value}`"));
    }
    let normalized = path_to_slashes(path)?;
    if normalized == "." || normalized.ends_with('/') {
        return Err(format!("unsafe relative path `{value}`"));
    }
    Ok(normalized)
}

fn path_to_slashes(path: &Path) -> Result<String, String> {
    let mut output = String::new();
    for component in path.components() {
        let Component::Normal(value) = component else {
            return Err(format!("unsafe path `{}`", path.display()));
        };
        let value = value.to_str().ok_or_else(|| format!("non-UTF-8 path `{}`", path.display()))?;
        if !output.is_empty() {
            output.push('/');
        }
        output.push_str(value);
    }
    Ok(output)
}

fn is_protected_path(path: &str) -> bool {
    path.split('/').any(|component| {
        matches!(
            component.to_ascii_lowercase().as_str(),
            ".git"
                | ".needle"
                | ".codegraph"
                | ".cache"
                | "target"
                | "node_modules"
                | "dist"
                | "build"
        )
    })
}

fn unique_change_id(request_digest: Digest) -> ChangeId {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let nonce = CHANGE_NONCE.fetch_add(1, Ordering::Relaxed);
    let mut hasher = needle_core::CanonicalHasher::new(b"needle-change-id");
    hasher.field_digest(request_digest);
    hasher.field_bytes(&now.to_le_bytes());
    hasher.field_bytes(&std::process::id().to_le_bytes());
    hasher.field_bytes(&nonce.to_le_bytes());
    ChangeId::from_digest(hasher.finish())
}

fn cleanup_sandbox_after_error<T>(sandbox: IsolatedCheckout, error: String) -> Result<T, String> {
    match sandbox.cleanup() {
        Ok(()) => Err(error),
        Err(cleanup) => Err(format!("{error}; disposable checkout cleanup failed: {cleanup}")),
    }
}

fn cleanup_all_after_error<T>(
    sandbox: IsolatedCheckout,
    session_cleanup: Result<(), String>,
    error: String,
) -> Result<T, String> {
    let sandbox_cleanup = sandbox.cleanup();
    match (session_cleanup, sandbox_cleanup) {
        (Ok(()), Ok(())) => Err(error),
        (session, checkout) => Err(format!(
            "{error}; App Server cleanup={}; checkout cleanup={}",
            session.err().unwrap_or_else(|| "ok".to_owned()),
            checkout.err().map(|value| value.to_string()).unwrap_or_else(|| "ok".to_owned())
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::AllowedPath;

    fn request(path: &str) -> ChangeRequest {
        ChangeRequest {
            task: "Update the message.".to_owned(),
            acceptance_criteria: vec!["Message is updated.".to_owned()],
            allowed_paths: vec![AllowedPath {
                path: path.to_owned(),
                scope: AllowedPathScope::Exact,
            }],
            artifact_ids: Vec::new(),
            claim_ids: Vec::new(),
            constraints: Vec::new(),
        }
    }

    fn temporary_directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "needle-patcher-{name}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn request_rejects_parent_and_protected_paths() {
        assert!(validate_change_request(&request("../outside"), &[]).is_err());
        assert!(validate_change_request(&request(".git/config"), &[]).is_err());
        assert!(validate_change_request(&request("src/lib.rs"), &[]).is_ok());
    }

    #[test]
    fn observed_scope_escape_is_rejected() {
        let root = temporary_directory("escape");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "before\n").unwrap();
        fs::write(root.join("README.md"), "before\n").unwrap();
        let before = capture_tree(&root).unwrap();
        let temp = temporary_directory("escape-base");
        let baseline =
            preserve_baseline_blobs(&root, &temp, &request("src/lib.rs"), &before).unwrap();
        fs::write(root.join("README.md"), "after\n").unwrap();
        let after = capture_tree(&root).unwrap();
        let result = observe_patch(&root, &request("src/lib.rs"), &before, &after, &baseline);
        assert!(result.unwrap_err().contains("outside the declared scope"));
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn binary_patch_is_rejected_before_persistence() {
        let root = temporary_directory("binary");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "before\n").unwrap();
        let before = capture_tree(&root).unwrap();
        let temp = temporary_directory("binary-base");
        let baseline =
            preserve_baseline_blobs(&root, &temp, &request("src/lib.rs"), &before).unwrap();
        fs::write(root.join("src/lib.rs"), [0, 159, 146, 150]).unwrap();
        let after = capture_tree(&root).unwrap();
        let result = observe_patch(&root, &request("src/lib.rs"), &before, &after, &baseline);
        assert!(result.unwrap_err().contains("non-UTF-8 or binary"));
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn symlink_and_oversized_changes_are_rejected_before_blob_reads() {
        let root = temporary_directory("entry-bounds");
        let baseline = BTreeMap::new();

        let mut symlink = BTreeMap::new();
        symlink.insert(
            "src/lib.rs".to_owned(),
            TreeEntry { kind: TreeEntryKind::Symlink, digest: Digest::blake3(b"target"), bytes: 0 },
        );
        let error =
            observe_patch(&root, &request("src/lib.rs"), &BTreeMap::new(), &symlink, &baseline)
                .unwrap_err();
        assert!(error.contains("changes a symlink"));

        let mut oversized = BTreeMap::new();
        oversized.insert(
            "src/lib.rs".to_owned(),
            TreeEntry {
                kind: TreeEntryKind::File,
                digest: Digest::blake3(b"oversized"),
                bytes: (MAX_PATCH_DIFF_BYTES as u64).saturating_add(1),
            },
        );
        let error =
            observe_patch(&root, &request("src/lib.rs"), &BTreeMap::new(), &oversized, &baseline)
                .unwrap_err();
        assert!(error.contains("projected patch exceeds"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn filesystem_rename_is_rejected() {
        let root = temporary_directory("rename");
        let temp = temporary_directory("rename-base");
        fs::write(root.join("before.txt"), "same content\n").unwrap();
        let request = ChangeRequest {
            allowed_paths: vec![
                AllowedPath { path: "before.txt".to_owned(), scope: AllowedPathScope::Exact },
                AllowedPath { path: "after.txt".to_owned(), scope: AllowedPathScope::Exact },
            ],
            ..request("before.txt")
        };
        let before = capture_tree(&root).unwrap();
        let baseline = preserve_baseline_blobs(&root, &temp, &request, &before).unwrap();
        fs::rename(root.join("before.txt"), root.join("after.txt")).unwrap();
        let after = capture_tree(&root).unwrap();
        let error = observe_patch(&root, &request, &before, &after, &baseline).unwrap_err();
        assert!(error.contains("contains a rename"));
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn observed_text_patch_preserves_exact_before_and_after_blobs() {
        let root = temporary_directory("text");
        let temp = temporary_directory("text-base");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "before\n").unwrap();
        let request = request("src/lib.rs");
        let before = capture_tree(&root).unwrap();
        let baseline = preserve_baseline_blobs(&root, &temp, &request, &before).unwrap();
        fs::write(root.join("src/lib.rs"), "after\n").unwrap();
        let after = capture_tree(&root).unwrap();
        let (files, blobs) = observe_patch(&root, &request, &before, &after, &baseline).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].operation, PatchOperation::Update);
        assert_eq!(blobs[0].before.as_deref(), Some(b"before\n".as_slice()));
        assert_eq!(blobs[0].after.as_deref(), Some(b"after\n".as_slice()));
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(temp).unwrap();
    }
}
