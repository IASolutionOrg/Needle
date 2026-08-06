use needle_core::{
    ApprovalDecision, ApprovalDecisionSource, ApprovalRequest, CommandClassification,
    CommandExecutionEvidence, Digest, ReadOnlyCommandPolicy, RequestedPermissions,
    TestCommandPolicy, TestPlan, WorkerConfig, WorkerObservationTrace,
};
use needle_runtime::{
    ApprovalBroker, ApprovalContext, RuntimeStore, command_evidence_from_output, parse_direct_argv,
    parse_read_only_command_argv, parse_test_command_argv,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{BufRead, BufReader, Write};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod main_session;
mod repository_tools;

pub use main_session::{
    ActiveTurnInterruption, CodexMainSession, ContinueWorkingResult, MainContinuationDiagnostics,
    MainDirectFailure, MainFinalTurn, MainNeedDiagnostics, MainNeedRelation, MainNeedTurn,
    MainSessionConfig, MainTurnResult, MainUsage, PILOT_MAIN_REPOSITORY_INSPECTION_INSTRUCTIONS,
};

const APPROVAL_TIMEOUT_SECONDS: u64 = 120;
const MAX_PROTOCOL_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TURN_ERROR_BYTES: usize = 4 * 1024;
const MAX_APP_SERVER_STDERR_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PendingApprovalMode {
    #[default]
    WaitForWebDecision,
    FailFast,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TestExecutionAvailability {
    available: bool,
    unavailable_reason: Option<String>,
}

#[derive(Debug)]
struct WorkerProcessEnvironment {
    variables: BTreeMap<String, String>,
    test_execution: TestExecutionAvailability,
}

pub(crate) struct AppServerSession {
    child: Child,
    input: ChildStdin,
    events: Receiver<Result<Value, String>>,
    stderr_diagnostic: Arc<Mutex<String>>,
    thread_id: String,
    thread_persisted: bool,
    next_request_id: u64,
    store: RuntimeStore,
    approval_context: ApprovalContext,
    broker: ApprovalBroker,
    snapshot_digest: Digest,
    approval_by_item: BTreeMap<String, ApprovalRequest>,
    test_plan_by_item: BTreeMap<String, Digest>,
    captured_command_items: BTreeSet<String>,
    command_evidence: Vec<CommandExecutionEvidence>,
    test_evidence: Vec<CapturedTestEvidence>,
    observed_files: BTreeSet<String>,
    trace_gaps: BTreeSet<String>,
    pending_approval_mode: PendingApprovalMode,
    test_execution: TestExecutionAvailability,
    patch_mode: bool,
    file_change_approvals_remaining: u8,
    file_change_approvals_granted: u8,
    cleaned: bool,
}

pub(crate) struct AppServerTurn {
    pub(crate) response: Value,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) cached_input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
    pub(crate) approval_wait: Duration,
    pub(crate) command_evidence: Vec<CommandExecutionEvidence>,
    pub(crate) test_evidence: Vec<CapturedTestEvidence>,
    pub(crate) observation_trace: WorkerObservationTrace,
    pub(crate) file_change_approvals_granted: u8,
}

#[derive(Clone, Debug)]
pub(crate) struct CapturedTestEvidence {
    pub(crate) plan_digest: Digest,
    pub(crate) evidence: CommandExecutionEvidence,
}

pub(crate) struct AppServerTurnFailure {
    pub(crate) diagnostic: String,
    pub(crate) input_tokens: Option<u64>,
    pub(crate) cached_input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
}

impl AppServerTurnFailure {
    fn before_usage(diagnostic: impl Into<String>) -> Self {
        Self {
            diagnostic: diagnostic.into(),
            input_tokens: None,
            cached_input_tokens: None,
            output_tokens: None,
        }
    }

    fn observed(
        diagnostic: impl Into<String>,
        input_tokens: Option<u64>,
        cached_input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) -> Self {
        Self { diagnostic: diagnostic.into(), input_tokens, cached_input_tokens, output_tokens }
    }
}

impl From<String> for AppServerTurnFailure {
    fn from(diagnostic: String) -> Self {
        Self::before_usage(diagnostic)
    }
}

impl AppServerSession {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        config: &WorkerConfig,
        codex_home: Option<&Path>,
        worker_process: bool,
        instructions: &str,
        checkout_root: &Path,
        target_root: &Path,
        temp_root: &Path,
        snapshot_digest: Digest,
        repository_id: Digest,
        route: &str,
        test_plan: Option<TestPlan>,
        trusted_test_execution: bool,
        store: RuntimeStore,
    ) -> Result<Self, String> {
        Self::start_with_access(
            config,
            codex_home,
            worker_process,
            instructions,
            checkout_root,
            target_root,
            temp_root,
            snapshot_digest,
            repository_id,
            route,
            test_plan,
            None,
            trusted_test_execution,
            trusted_test_execution,
            store,
            "read-only",
            0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_patch(
        config: &WorkerConfig,
        codex_home: Option<&Path>,
        instructions: &str,
        checkout_root: &Path,
        target_root: &Path,
        temp_root: &Path,
        snapshot_digest: Digest,
        repository_id: Digest,
        store: RuntimeStore,
    ) -> Result<Self, String> {
        Self::start_with_access(
            config,
            codex_home,
            true,
            instructions,
            checkout_root,
            target_root,
            temp_root,
            snapshot_digest,
            repository_id,
            "prepare.change",
            None,
            None,
            false,
            true,
            store,
            "workspace-write",
            1,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_verifier(
        config: &WorkerConfig,
        codex_home: Option<&Path>,
        instructions: &str,
        checkout_root: &Path,
        target_root: &Path,
        temp_root: &Path,
        snapshot_digest: Digest,
        repository_id: Digest,
        test_plans: Vec<TestPlan>,
        trusted_test_execution: bool,
        store: RuntimeStore,
    ) -> Result<Self, String> {
        Self::start_with_access(
            config,
            codex_home,
            true,
            instructions,
            checkout_root,
            target_root,
            temp_root,
            snapshot_digest,
            repository_id,
            "verify.change",
            None,
            Some(test_plans),
            trusted_test_execution,
            true,
            store,
            "read-only",
            0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with_access(
        config: &WorkerConfig,
        codex_home: Option<&Path>,
        worker_process: bool,
        instructions: &str,
        checkout_root: &Path,
        target_root: &Path,
        temp_root: &Path,
        snapshot_digest: Digest,
        repository_id: Digest,
        route: &str,
        test_plan: Option<TestPlan>,
        verifier_test_plans: Option<Vec<TestPlan>>,
        trusted_test_execution: bool,
        read_only_commands_allowed: bool,
        store: RuntimeStore,
        sandbox_mode: &'static str,
        file_change_approvals_remaining: u8,
    ) -> Result<Self, String> {
        let WorkerProcessEnvironment { variables: mut process_environment, test_execution } =
            worker_process_environment(
                target_root,
                temp_root,
                verifier_test_plans
                    .as_deref()
                    .and_then(|plans| plans.first())
                    .or(test_plan.as_ref()),
                trusted_test_execution,
            );
        if let Some(codex_home) = codex_home {
            process_environment
                .insert("CODEX_HOME".to_owned(), codex_home.to_string_lossy().into_owned());
        }
        prepend_codex_package_paths(&mut process_environment, &config.executable)?;
        let configured_mcp_servers = configured_mcp_server_names(config, &process_environment)?;
        let developer_instructions = worker_developer_instructions(
            instructions,
            verifier_test_plans.as_deref().and_then(|plans| plans.first()).or(test_plan.as_ref()),
            &test_execution,
        );
        let mut command = app_server_command(
            config,
            &process_environment,
            worker_process,
            &configured_mcp_servers,
        );
        let mut child =
            command.spawn().map_err(|error| format!("cannot spawn Codex App Server: {error}"))?;
        let input = match child.stdin.take() {
            Some(input) => input,
            None => {
                reap_failed_start(&mut child);
                return Err("App Server stdin unavailable".to_owned());
            }
        };
        let output = match child.stdout.take() {
            Some(output) => output,
            None => {
                reap_failed_start(&mut child);
                return Err("App Server stdout unavailable".to_owned());
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                reap_failed_start(&mut child);
                return Err("App Server stderr unavailable".to_owned());
            }
        };
        let stderr_diagnostic = Arc::new(Mutex::new(String::new()));
        let stderr_capture = Arc::clone(&stderr_diagnostic);
        let stderr_reader = thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else {
                    break;
                };
                let Ok(mut diagnostic) = stderr_capture.lock() else {
                    break;
                };
                if diagnostic.len() >= MAX_APP_SERVER_STDERR_BYTES {
                    break;
                }
                if !diagnostic.is_empty() {
                    diagnostic.push('\n');
                }
                let remaining = MAX_APP_SERVER_STDERR_BYTES.saturating_sub(diagnostic.len());
                let mut end = line.len().min(remaining);
                while end > 0 && !line.is_char_boundary(end) {
                    end -= 1;
                }
                diagnostic.push_str(&line[..end]);
            }
        });
        let (sender, events) = mpsc::sync_channel(256);
        thread::spawn(move || {
            let reader = BufReader::new(output);
            for line in reader.lines() {
                let event = line
                    .map_err(|error| format!("cannot read App Server event: {error}"))
                    .and_then(|line| {
                        if line.len() > MAX_PROTOCOL_LINE_BYTES {
                            Err("App Server event exceeds the protocol cap".to_owned())
                        } else {
                            serde_json::from_str(&line)
                                .map_err(|error| format!("invalid App Server JSON: {error}"))
                        }
                    });
                if sender.send(event).is_err() {
                    break;
                }
            }
            let _ = stderr_reader.join();
        });
        let approval_context = ApprovalContext {
            route: route.to_owned(),
            repository_id,
            checkout_root: checkout_root.to_path_buf(),
            target_root: target_root.to_path_buf(),
            temp_root: temp_root.to_path_buf(),
            test_plan,
            verifier_test_plans: verifier_test_plans.clone(),
            test_execution_available: test_execution.available,
        };
        let mut test_policy = TestCommandPolicy::cargo_test(repository_id);
        if let Some(plans) = verifier_test_plans.as_ref() {
            test_policy.maximum_executions_per_worker = plans.len().try_into().unwrap_or(u32::MAX);
        }
        let broker = ApprovalBroker::new(
            (test_execution.available
                && verifier_test_plans.as_ref().is_none_or(|plans| !plans.is_empty()))
            .then_some(test_policy)
            .into_iter()
            .collect(),
        )
        .with_read_only_policies(
            read_only_commands_allowed
                .then(|| ReadOnlyCommandPolicy::repository_inspection(repository_id))
                .into_iter()
                .collect(),
        );
        let shell_environment =
            worker_shell_environment(&process_environment, target_root, temp_root);
        let mut session = Self {
            child,
            input,
            events,
            stderr_diagnostic,
            thread_id: String::new(),
            thread_persisted: false,
            next_request_id: 1,
            store,
            approval_context,
            broker,
            snapshot_digest,
            approval_by_item: BTreeMap::new(),
            test_plan_by_item: BTreeMap::new(),
            captured_command_items: BTreeSet::new(),
            command_evidence: Vec::new(),
            test_evidence: Vec::new(),
            observed_files: BTreeSet::new(),
            trace_gaps: BTreeSet::new(),
            pending_approval_mode: PendingApprovalMode::WaitForWebDecision,
            test_execution,
            patch_mode: file_change_approvals_remaining > 0,
            file_change_approvals_remaining,
            file_change_approvals_granted: 0,
            cleaned: false,
        };
        let start_result = (|| -> Result<String, String> {
            let initialize_id = session.send_request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "needle",
                        "title": "Needle",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {
                        "experimentalApi": true,
                        "mcpServerOpenaiFormElicitation": false,
                        "requestAttestation": false
                    }
                }),
            )?;
            session.wait_for_response(initialize_id, Duration::from_secs(15))?;
            session.send_notification("initialized", None)?;
            let mut thread_params = json!({
                "model": config.model,
                "serviceTier": config.service_tier,
                "cwd": checkout_root,
                "approvalPolicy": "on-request",
                "approvalsReviewer": "user",
                "developerInstructions": developer_instructions,
                "ephemeral": true,
                "historyMode": "paginated",
                "dynamicTools": repository_tools::specs(),
                "environments": [],
                "runtimeWorkspaceRoots": [checkout_root],
                "config": {
                    "web_search": "disabled",
                    "features": {
                        "hooks": false,
                        "plugins": false,
                        "apps": false,
                        "multi_agent": false
                    },
                    "mcp_servers": {},
                    "project_doc_max_bytes": 0,
                    "project_doc_fallback_filenames": [],
                    "model_reasoning_effort": config.reasoning,
                    "allow_login_shell": false,
                    "shell_environment_policy": {
                        "inherit": "none",
                        "set": shell_environment
                    }
                }
            });
            configure_thread_access(&mut thread_params, sandbox_mode)?;
            let thread_id = session.send_request("thread/start", thread_params)?;
            let response = session.wait_for_response(thread_id, Duration::from_secs(30))?;
            response
                .pointer("/result/thread/id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| "thread/start did not return a thread id".to_owned())
        })();
        match start_result {
            Ok(thread_id) => {
                session.thread_id = thread_id;
                Ok(session)
            }
            Err(start_error) => match session.cleanup_inner() {
                Ok(()) => Err(start_error),
                Err(cleanup_error) => Err(format!(
                    "{start_error}; App Server startup cleanup failed: {cleanup_error}"
                )),
            },
        }
    }

    pub(crate) fn run_turn_cancellable(
        &mut self,
        prompt: &str,
        output_schema: &Value,
        timeout: Duration,
        cancellation: Option<&AtomicBool>,
    ) -> Result<AppServerTurn, AppServerTurnFailure> {
        let request_id = self.send_request(
            "turn/start",
            json!({
                "threadId": self.thread_id,
                "input": [{"type": "text", "text": prompt}],
                "cwd": self.approval_context.checkout_root,
                "approvalPolicy": "on-request",
                "approvalsReviewer": "user",
                "outputSchema": output_schema
            }),
        )?;
        let response = self.wait_for_response(request_id, Duration::from_secs(30))?;
        let turn_id = response
            .pointer("/result/turn/id")
            .and_then(Value::as_str)
            .ok_or_else(|| "turn/start did not return a turn id".to_owned())?
            .to_owned();
        let started = Instant::now();
        let mut approval_wait = Duration::ZERO;
        let mut final_message = None;
        let mut input_tokens = None;
        let mut cached_input_tokens = None;
        let mut output_tokens = None;
        let mut error_notification = None;
        let mut error_will_retry = None;
        loop {
            if cancellation.is_some_and(|value| value.load(Ordering::Acquire)) {
                let _ = self.send_request(
                    "turn/interrupt",
                    json!({"threadId": self.thread_id, "turnId": turn_id}),
                );
                return Err(AppServerTurnFailure::observed(
                    "worker cancelled while resolving a need",
                    input_tokens,
                    cached_input_tokens,
                    output_tokens,
                ));
            }
            let remaining = remaining_turn_time(timeout, started.elapsed(), approval_wait)
                .ok_or_else(|| {
                    AppServerTurnFailure::observed(
                        "Codex App Server turn timed out",
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                    )
                })?;
            let event =
                self.recv_event(remaining.min(Duration::from_secs(1))).map_err(|diagnostic| {
                    AppServerTurnFailure::observed(
                        diagnostic,
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                    )
                })?;
            let Some(event) = event else {
                continue;
            };
            if event.get("method").and_then(Value::as_str) == Some("item/tool/call") {
                self.handle_repository_tool_call(&event).map_err(|diagnostic| {
                    AppServerTurnFailure::observed(
                        diagnostic,
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                    )
                })?;
                continue;
            }
            if event.get("method").and_then(Value::as_str) == Some("error")
                && event.pointer("/params/turnId").and_then(Value::as_str) == Some(turn_id.as_str())
            {
                error_notification = event.pointer("/params/error").cloned();
                error_will_retry = event.pointer("/params/willRetry").and_then(Value::as_bool);
                continue;
            }
            if event.get("method").and_then(Value::as_str)
                == Some("item/commandExecution/requestApproval")
            {
                let approval_started = Instant::now();
                self.handle_command_approval(&event).map_err(|diagnostic| {
                    AppServerTurnFailure::observed(
                        diagnostic,
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                    )
                })?;
                approval_wait = approval_wait.saturating_add(approval_started.elapsed());
                continue;
            }
            if event.get("method").and_then(Value::as_str)
                == Some("item/fileChange/requestApproval")
            {
                let decision = if self.file_change_approvals_remaining > 0 {
                    self.file_change_approvals_remaining -= 1;
                    self.file_change_approvals_granted =
                        self.file_change_approvals_granted.saturating_add(1);
                    "accept"
                } else {
                    "decline"
                };
                self.respond(&event, json!({"decision": decision})).map_err(|diagnostic| {
                    AppServerTurnFailure::observed(
                        diagnostic,
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                    )
                })?;
                if self.patch_mode && decision == "decline" {
                    return Err(AppServerTurnFailure::observed(
                        "patch worker exceeded the single file-change approval",
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                    ));
                }
                continue;
            }
            if event.get("method").and_then(Value::as_str) == Some("item/completed") {
                if let Some(item) = event.pointer("/params/item") {
                    match item.get("type").and_then(Value::as_str) {
                        Some("agentMessage") => {
                            if let Some(text) = item.get("text").and_then(Value::as_str) {
                                final_message = Some(text.to_owned());
                            }
                        }
                        Some("commandExecution") => {
                            self.capture_command_trace(item);
                            self.capture_command_evidence(item).map_err(|diagnostic| {
                                AppServerTurnFailure::observed(
                                    diagnostic,
                                    input_tokens,
                                    cached_input_tokens,
                                    output_tokens,
                                )
                            })?;
                        }
                        _ => {}
                    }
                }
                continue;
            }
            if event.get("method").and_then(Value::as_str) == Some("thread/tokenUsage/updated") {
                input_tokens =
                    event.pointer("/params/tokenUsage/total/inputTokens").and_then(Value::as_u64);
                cached_input_tokens = event
                    .pointer("/params/tokenUsage/total/cachedInputTokens")
                    .and_then(Value::as_u64);
                output_tokens =
                    event.pointer("/params/tokenUsage/total/outputTokens").and_then(Value::as_u64);
                continue;
            }
            if event.get("method").and_then(Value::as_str) == Some("turn/completed")
                && event.pointer("/params/turn/id").and_then(Value::as_str)
                    == Some(turn_id.as_str())
            {
                let status = event
                    .pointer("/params/turn/status")
                    .and_then(Value::as_str)
                    .unwrap_or("failed");
                if status != "completed" {
                    return Err(AppServerTurnFailure::observed(
                        turn_failure_diagnostic(
                            status,
                            event.pointer("/params/turn/error"),
                            error_notification.as_ref(),
                            error_will_retry,
                        ),
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                    ));
                }
                break;
            }
        }
        let text = final_message.ok_or_else(|| {
            AppServerTurnFailure::observed(
                "worker returned no final message",
                input_tokens,
                cached_input_tokens,
                output_tokens,
            )
        })?;
        let response = serde_json::from_str(&text).map_err(|error| {
            AppServerTurnFailure::observed(
                format!("worker result violates output schema: {error}"),
                input_tokens,
                cached_input_tokens,
                output_tokens,
            )
        })?;
        Ok(AppServerTurn {
            response,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            approval_wait,
            command_evidence: std::mem::take(&mut self.command_evidence),
            test_evidence: std::mem::take(&mut self.test_evidence),
            observation_trace: WorkerObservationTrace {
                observed_files: std::mem::take(&mut self.observed_files).into_iter().collect(),
                gaps: std::mem::take(&mut self.trace_gaps).into_iter().collect(),
            },
            file_change_approvals_granted: self.file_change_approvals_granted,
        })
    }

    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(crate) fn test_execution_available(&self) -> bool {
        self.test_execution.available
    }

    pub(crate) fn test_execution_unavailable_reason(&self) -> Option<&str> {
        self.test_execution.unavailable_reason.as_deref()
    }

    pub(crate) fn fail_fast_on_pending_approvals(&mut self) {
        self.pending_approval_mode = PendingApprovalMode::FailFast;
    }

    pub(crate) fn cleanup(mut self) -> Result<(), String> {
        self.cleanup_inner()
    }

    fn handle_command_approval(&mut self, event: &Value) -> Result<(), String> {
        let params =
            event.get("params").ok_or_else(|| "approval request has no params".to_owned())?;
        let command = params.get("command").and_then(Value::as_str).unwrap_or_default();
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.approval_context.checkout_root.clone());
        let permissions = requested_permissions(params.get("additionalPermissions"));
        let structured_action = single_unknown_command_action(params)
            .or_else(|| single_read_only_command_action(params));
        let (mut argv, classification) = self.broker.classify_command(
            Some(command),
            structured_action,
            &cwd,
            &permissions,
            &self.approval_context,
        );
        if argv.is_empty() {
            argv = structured_action
                .and_then(|action| {
                    self.approval_context
                        .test_plan
                        .as_ref()
                        .and_then(|plan| parse_test_command_argv(action, &plan.argv).ok())
                        .or_else(|| parse_direct_argv(action).ok())
                })
                .unwrap_or_default();
        }
        let protocol_id =
            event.get("id").cloned().ok_or_else(|| "approval request has no id".to_owned())?;
        let item_id = params
            .get("itemId")
            .and_then(Value::as_str)
            .ok_or_else(|| "approval request has no item id".to_owned())?
            .to_owned();
        let thread_id = params
            .get("threadId")
            .and_then(Value::as_str)
            .ok_or_else(|| "approval request has no thread id".to_owned())?
            .to_owned();
        let turn_id = params
            .get("turnId")
            .and_then(Value::as_str)
            .ok_or_else(|| "approval request has no turn id".to_owned())?
            .to_owned();
        let payload_digest =
            ApprovalRequest::compute_payload_digest(&argv, &cwd.to_string_lossy(), &permissions)
                .map_err(|error| error.to_string())?;
        let id =
            params.get("approvalId").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(
                || {
                    Digest::blake3(format!(
                        "needle-approval\n{thread_id}\n{turn_id}\n{item_id}\n{payload_digest}\n"
                    ))
                    .to_hex()
                },
            );
        let request = ApprovalRequest {
            id: id.clone(),
            protocol_request_id: protocol_id,
            protocol_approval_id: params
                .get("approvalId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            thread_id,
            turn_id,
            item_id: item_id.clone(),
            argv,
            command_display: (!command.is_empty()).then(|| command.to_owned()),
            cwd: cwd.to_string_lossy().into_owned(),
            reason: params.get("reason").and_then(Value::as_str).map(str::to_owned),
            requested_permissions: permissions,
            route: self.approval_context.route.clone(),
            repository_id: self.approval_context.repository_id,
            repository_root: self.approval_context.checkout_root.to_string_lossy().into_owned(),
            expires_unix_ms: now_ms().saturating_add(APPROVAL_TIMEOUT_SECONDS * 1000),
            classification,
            payload_digest,
            decision: None,
            decision_source: None,
            decided_unix_ms: None,
        };
        self.store.enqueue_approval(&request).map_err(|error| error.to_string())?;
        let automatic = ApprovalBroker::automatic_decision(&request.classification);
        let decision = match automatic {
            Some((decision, source)) => {
                self.store
                    .decide_approval(&id, decision, source, payload_digest)
                    .map_err(|error| error.to_string())?;
                decision
            }
            None if self.pending_approval_mode == PendingApprovalMode::FailFast => {
                self.store
                    .decide_approval(
                        &id,
                        ApprovalDecision::Decline,
                        ApprovalDecisionSource::Runtime,
                        payload_digest,
                    )
                    .map_err(|error| error.to_string())?;
                ApprovalDecision::Decline
            }
            None => self.wait_for_web_decision(&request)?,
        };
        let rejection = (self.pending_approval_mode == PendingApprovalMode::FailFast
            && decision != ApprovalDecision::Accept)
            .then(|| {
                format!(
                    "pilot main command was not auto-approved (classification: {})",
                    command_classification_name(&request.classification)
                )
            });
        self.respond_decision(&request, decision)?;
        let test_plan_digest = self.test_plan_digest_for_approval(&request);
        self.approval_by_item.insert(item_id.clone(), request);
        if let Some(plan_digest) = test_plan_digest {
            self.test_plan_by_item.insert(item_id, plan_digest);
        }
        if let Some(rejection) = rejection {
            return Err(rejection);
        }
        Ok(())
    }

    fn handle_repository_tool_call(&mut self, event: &Value) -> Result<(), String> {
        let params = event
            .get("params")
            .ok_or_else(|| "repository tool request has no params".to_owned())?;
        if params.get("threadId").and_then(Value::as_str) != Some(self.thread_id.as_str()) {
            return Err("repository tool request targeted another thread".to_owned());
        }
        let tool = params
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| "repository tool request has no tool name".to_owned())?;
        let arguments = params.get("arguments").unwrap_or(&Value::Null);
        let result =
            repository_tools::execute(tool, arguments, &self.approval_context.checkout_root);
        let (success, text) = match result {
            Ok(output) => {
                self.observed_files.extend(output.observed_files);
                (true, output.text)
            }
            Err(diagnostic) => (false, diagnostic),
        };
        self.respond(
            event,
            json!({
                "success": success,
                "contentItems": [{"type": "inputText", "text": text}]
            }),
        )
    }

    fn wait_for_web_decision(&self, request: &ApprovalRequest) -> Result<ApprovalDecision, String> {
        loop {
            let now = now_ms();
            if now >= request.expires_unix_ms {
                let _ = self.store.expire_approvals();
                return Ok(ApprovalDecision::Decline);
            }
            if let Some(updated) =
                self.store.approval(&request.id).map_err(|error| error.to_string())?
                && let Some(decision) = updated.decision
            {
                return Ok(decision);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn respond_decision(
        &mut self,
        request: &ApprovalRequest,
        decision: ApprovalDecision,
    ) -> Result<(), String> {
        let event = json!({"id": request.protocol_request_id});
        let decision = serde_json::to_value(decision).map_err(|error| error.to_string())?;
        self.respond(&event, json!({"decision": decision}))
    }

    fn test_plan_digest_for_approval(&self, approval: &ApprovalRequest) -> Option<Digest> {
        if !matches!(approval.classification, CommandClassification::AutoApprovedTest { .. }) {
            return None;
        }
        let matches = |plan: &TestPlan| {
            approval.argv == plan.argv
                && same_canonical_path(
                    Path::new(&approval.cwd),
                    &self.approval_context.checkout_root.join(&plan.cwd_relative),
                )
        };
        if let Some(plans) = self.approval_context.verifier_test_plans.as_ref() {
            return plans
                .iter()
                .filter(|plan| matches(plan))
                .find(|plan| {
                    let digest = plan.identity_digest();
                    !self.test_plan_by_item.values().any(|used| *used == digest)
                })
                .map(TestPlan::identity_digest);
        }
        self.approval_context
            .test_plan
            .as_ref()
            .filter(|plan| matches(plan))
            .map(TestPlan::identity_digest)
    }

    fn expected_test_plan_for_item(&self, item_id: &str) -> Option<(Digest, TestPlan)> {
        let digest = *self.test_plan_by_item.get(item_id)?;
        self.approval_context
            .verifier_test_plans
            .as_ref()
            .and_then(|plans| plans.iter().find(|plan| plan.identity_digest() == digest))
            .or_else(|| {
                self.approval_context
                    .test_plan
                    .as_ref()
                    .filter(|plan| plan.identity_digest() == digest)
            })
            .cloned()
            .map(|plan| (digest, plan))
    }

    fn capture_command_evidence(&mut self, item: &Value) -> Result<(), String> {
        let Some(item_id) = item.get("id").and_then(Value::as_str) else {
            return Ok(());
        };
        if self.captured_command_items.contains(item_id) {
            return Ok(());
        }
        let Some(approval) = self.approval_by_item.get(item_id) else {
            return Ok(());
        };
        let output = item.get("aggregatedOutput").and_then(Value::as_str).unwrap_or_default();
        let duration_ms = item.get("durationMs").and_then(Value::as_u64).unwrap_or_default();
        let exit_status =
            item.get("exitCode").and_then(Value::as_i64).and_then(|value| value.try_into().ok());
        let expected_plan = self.expected_test_plan_for_item(item_id);
        let expected_test_identifier =
            expected_plan.as_ref().map(|(_, plan)| plan.test_identifier.as_str());
        let mut evidence = command_evidence_from_output(
            approval,
            self.snapshot_digest,
            None,
            expected_test_identifier,
            exit_status,
            duration_ms,
            output.as_bytes(),
        );
        evidence.argv = validated_command_argv(item, &approval.argv);
        evidence.runner = evidence.argv.first().cloned().unwrap_or_default();
        evidence.cwd = item.get("cwd").and_then(Value::as_str).unwrap_or_default().to_owned();
        let status = item.get("status").and_then(Value::as_str).unwrap_or("unknown");
        if status != "completed" || exit_status.is_none() {
            evidence.infrastructure_failure = Some(format!("app_server_command_status:{status}"));
        } else if evidence.cwd != approval.cwd {
            evidence.infrastructure_failure =
                Some("app_server_command_payload_changed:cwd".to_owned());
        }
        self.store.record_command_evidence(None, &evidence).map_err(|error| error.to_string())?;
        self.captured_command_items.insert(item_id.to_owned());
        if let Some((plan_digest, _)) = expected_plan {
            self.test_evidence
                .push(CapturedTestEvidence { plan_digest, evidence: evidence.clone() });
            if self.approval_context.verifier_test_plans.is_some()
                && !self.broker.complete_verifier_plan(plan_digest)
            {
                return Err("verifier test completion did not match the in-flight plan".to_owned());
            }
        }
        self.command_evidence.push(evidence);
        Ok(())
    }

    fn capture_command_trace(&mut self, item: &Value) {
        let declared_test = item.get("id").and_then(Value::as_str).and_then(|item_id| {
            self.approval_by_item.get(item_id).zip(self.expected_test_plan_for_item(item_id)).map(
                |(approval, (_, plan))| {
                    validated_declared_test_command(item, &approval.argv, &plan.argv)
                },
            )
        }) == Some(true);
        if declared_test {
            // Command evidence validates the declared test separately. It is
            // not repository discovery and must not downgrade artifact scope.
            return;
        }
        let trace = command_observation_trace(&self.approval_context.checkout_root, item);
        self.observed_files.extend(trace.observed_files);
        self.trace_gaps.extend(trace.gaps);
    }

    fn wait_for_response(&mut self, id: u64, timeout: Duration) -> Result<Value, String> {
        let started = Instant::now();
        loop {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or_else(|| format!("App Server request {id} timed out"))?;
            let Some(event) = self.recv_event(remaining)? else {
                continue;
            };
            if event.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = event.get("error") {
                    return Err(format!("App Server request {id} failed: {error}"));
                }
                return Ok(event);
            }
        }
    }

    fn recv_event(&self, timeout: Duration) -> Result<Option<Value>, String> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => event.map(Some),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                let diagnostic = self
                    .stderr_diagnostic
                    .lock()
                    .ok()
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty());
                match diagnostic {
                    Some(diagnostic) => {
                        Err(format!("Codex App Server event stream closed: {diagnostic}"))
                    }
                    None => Err("Codex App Server event stream closed".to_owned()),
                }
            }
        }
    }

    fn send_request(&mut self, method: &str, params: Value) -> Result<u64, String> {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        self.write_message(&json!({"id": id, "method": method, "params": params}))?;
        Ok(id)
    }

    fn send_notification(&mut self, method: &str, params: Option<Value>) -> Result<(), String> {
        let mut value = json!({"method": method});
        if let Some(params) = params {
            value["params"] = params;
        }
        self.write_message(&value)
    }

    fn respond(&mut self, request: &Value, result: Value) -> Result<(), String> {
        let id = request.get("id").cloned().ok_or_else(|| "request id missing".to_owned())?;
        self.write_message(&json!({"id": id, "result": result}))
    }

    fn write_message(&mut self, value: &Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.input, value).map_err(|error| error.to_string())?;
        self.input.write_all(b"\n").map_err(|error| error.to_string())?;
        self.input.flush().map_err(|error| error.to_string())
    }

    fn cleanup_inner(&mut self) -> Result<(), String> {
        if self.cleaned {
            return Ok(());
        }
        // Ephemeral threads are never written to Codex's thread store. Asking
        // App Server to delete one returns InvalidRequest, so process teardown
        // is the complete cleanup operation for that mode.
        let thread_cleanup = if self.thread_id.is_empty() || !self.thread_persisted {
            Ok(())
        } else {
            self.send_request("thread/delete", json!({"threadId": self.thread_id.clone()}))
                .and_then(|id| self.wait_for_response(id, Duration::from_secs(10)).map(|_| ()))
        };
        let _ = self.child.kill();
        let process_cleanup = self
            .child
            .wait()
            .map(|_| ())
            .map_err(|error| format!("cannot reap App Server: {error}"));
        self.cleaned = true;
        thread_cleanup.and(process_cleanup)
    }
}

fn reap_failed_start(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(super) fn turn_failure_diagnostic(
    status: &str,
    turn_error: Option<&Value>,
    notification_error: Option<&Value>,
    will_retry: Option<bool>,
) -> String {
    let error = turn_error.filter(|value| !value.is_null()).or(notification_error);
    let mut diagnostic = format!("Codex App Server turn ended with status {status}");
    if let Some(error) = error {
        if let Some(message) = error.get("message").and_then(Value::as_str) {
            diagnostic.push_str("; message=");
            diagnostic.push_str(message);
        }
        if let Some(details) =
            error.get("additionalDetails").and_then(Value::as_str).filter(|value| !value.is_empty())
        {
            diagnostic.push_str("; additional_details=");
            diagnostic.push_str(details);
        }
        if let Some(info) = error.get("codexErrorInfo").filter(|value| !value.is_null()) {
            diagnostic.push_str("; codex_error_info=");
            diagnostic
                .push_str(&serde_json::to_string(info).unwrap_or_else(|_| "unavailable".into()));
        }
    } else {
        diagnostic.push_str("; error_details=missing");
    }
    if let Some(will_retry) = will_retry {
        diagnostic.push_str(if will_retry { "; will_retry=true" } else { "; will_retry=false" });
    }
    bound_protocol_text(&diagnostic, MAX_TURN_ERROR_BYTES)
}

fn bound_protocol_text(value: &str, maximum: usize) -> String {
    let mut end = value.len().min(maximum);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

impl Drop for AppServerSession {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = self.cleanup_inner();
        }
    }
}

fn app_server_command(
    config: &WorkerConfig,
    process_environment: &BTreeMap<String, String>,
    worker_process: bool,
    configured_mcp_servers: &[String],
) -> Command {
    let mut command = Command::new(&config.executable);
    command
        .arg("app-server")
        .arg("--listen")
        .arg("stdio://")
        .arg("--config")
        .arg("analytics.enabled=false")
        .arg("--config")
        .arg("notify=[]")
        .arg("--config")
        .arg("features.hooks=false")
        .arg("--config")
        .arg("features.plugins=false")
        .arg("--config")
        .arg("features.apps=false")
        .arg("--config")
        .arg("features.multi_agent=false")
        .arg("--config")
        .arg("features.code_mode_host=false")
        .arg("--config")
        .arg("mcp_servers={}");
    for server_name in configured_mcp_servers {
        command.arg("--config").arg(disable_mcp_server_override(server_name));
    }
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).env_clear();
    for (key, value) in process_environment {
        command.env(key, value);
    }
    if worker_process {
        command.env("NEEDLE_WORKER", "1");
    }
    command
}

fn configure_thread_access(params: &mut Value, sandbox_mode: &str) -> Result<(), String> {
    let object = params
        .as_object_mut()
        .ok_or_else(|| "thread/start params must be a JSON object".to_owned())?;
    #[cfg(windows)]
    {
        let permission_profile = match sandbox_mode {
            "read-only" => ":read-only",
            "workspace-write" => ":workspace",
            "danger-full-access" => ":danger-full-access",
            value => return Err(format!("unsupported Codex sandbox mode: {value}")),
        };
        object.insert("permissions".to_owned(), Value::String(permission_profile.to_owned()));
    }
    #[cfg(not(windows))]
    {
        object.insert("sandbox".to_owned(), Value::String(sandbox_mode.to_owned()));
    }
    Ok(())
}

fn configured_mcp_server_names(
    config: &WorkerConfig,
    process_environment: &BTreeMap<String, String>,
) -> Result<Vec<String>, String> {
    let mut command = Command::new(&config.executable);
    command
        .args([
            "--config",
            "features.hooks=false",
            "--config",
            "features.plugins=false",
            "--config",
            "features.apps=false",
            "--config",
            "features.multi_agent=false",
            "mcp",
            "list",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for (key, value) in process_environment {
        command.env(key, value);
    }
    let output = command
        .output()
        .map_err(|error| format!("cannot inspect configured Codex MCP servers: {error}"))?;
    if !output.status.success() {
        let diagnostic = bound_protocol_text(
            String::from_utf8_lossy(&output.stderr).trim(),
            MAX_TURN_ERROR_BYTES,
        );
        return Err(if diagnostic.is_empty() {
            format!("Codex MCP inspection failed with {}", output.status)
        } else {
            format!("Codex MCP inspection failed with {}: {diagnostic}", output.status)
        });
    }

    let entries = serde_json::from_slice::<Value>(&output.stdout)
        .map_err(|error| format!("Codex MCP inspection returned invalid JSON: {error}"))?;
    let entries = entries
        .as_array()
        .ok_or_else(|| "Codex MCP inspection did not return an array".to_owned())?;
    if entries.len() > 64 {
        return Err("Codex MCP inspection exceeded the 64-server safety bound".to_owned());
    }
    let mut names = BTreeSet::new();
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| is_safe_mcp_server_name(name))
            .ok_or_else(|| "Codex MCP inspection returned an invalid server name".to_owned())?;
        names.insert(name.to_owned());
    }
    Ok(names.into_iter().collect())
}

fn prepend_codex_package_paths(
    process_environment: &mut BTreeMap<String, String>,
    executable: &str,
) -> Result<(), String> {
    let executable = Path::new(executable);
    let Some(bin_directory) = executable.parent().filter(|path| path.is_dir()) else {
        return Ok(());
    };
    let Some(package_root) = bin_directory.parent() else {
        return Ok(());
    };
    if !package_root.join("codex-package.json").is_file() {
        return Ok(());
    }

    let mut paths = vec![
        bin_directory.to_path_buf(),
        package_root.join("codex-resources"),
        package_root.join("codex-path"),
    ];
    let existing_path = process_environment
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.clone());
    if let Some(existing_path) = existing_path {
        paths.extend(env::split_paths(&existing_path));
    }
    process_environment.retain(|key, _| !key.eq_ignore_ascii_case("PATH"));
    let joined = env::join_paths(paths)
        .map_err(|error| format!("cannot construct the managed Codex package PATH: {error}"))?;
    process_environment.insert("PATH".to_owned(), joined.to_string_lossy().into_owned());
    Ok(())
}

fn is_safe_mcp_server_name(server_name: &str) -> bool {
    !server_name.is_empty()
        && server_name.len() <= 256
        && server_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn disable_mcp_server_override(server_name: &str) -> String {
    format!("mcp_servers.{server_name}.enabled=false")
}

fn remaining_turn_time(
    timeout: Duration,
    elapsed: Duration,
    approval_wait: Duration,
) -> Option<Duration> {
    timeout.saturating_add(approval_wait).checked_sub(elapsed)
}

fn worker_process_environment(
    target_root: &Path,
    temp_root: &Path,
    test_plan: Option<&TestPlan>,
    trusted_test_execution: bool,
) -> WorkerProcessEnvironment {
    let mut environment = crate::worker::sanitized_environment();
    let test_execution = initial_test_execution_availability(test_plan, trusted_test_execution);
    #[cfg(all(windows, target_env = "msvc"))]
    let mut test_execution = test_execution;
    #[cfg(all(windows, target_env = "msvc"))]
    {
        if test_execution.available {
            let discovered = discover_windows_msvc_environment(&environment);
            apply_windows_msvc_environment(
                &mut environment,
                target_root,
                &mut test_execution,
                discovered,
            );
        }
    }
    let _ = (test_plan, trusted_test_execution);
    environment.insert("CARGO_TARGET_DIR".to_owned(), target_root.to_string_lossy().into_owned());
    for key in ["TEMP", "TMP", "TMPDIR"] {
        insert_environment_case_insensitive(
            &mut environment,
            key.to_owned(),
            temp_root.to_string_lossy().into_owned(),
        );
    }
    WorkerProcessEnvironment { variables: environment, test_execution }
}

fn initial_test_execution_availability(
    test_plan: Option<&TestPlan>,
    trusted_test_execution: bool,
) -> TestExecutionAvailability {
    let Some(plan) = test_plan else {
        return TestExecutionAvailability::default();
    };
    if !trusted_test_execution {
        return TestExecutionAvailability {
            available: false,
            unavailable_reason: Some("the repository is not trusted for test execution".to_owned()),
        };
    }
    if plan.test_command().is_err() {
        return TestExecutionAvailability {
            available: false,
            unavailable_reason: Some(
                "the declared test runner is not supported by the approval broker".to_owned(),
            ),
        };
    }
    TestExecutionAvailability { available: true, unavailable_reason: None }
}

#[cfg(all(windows, target_env = "msvc"))]
fn apply_windows_msvc_environment(
    environment: &mut BTreeMap<String, String>,
    target_root: &Path,
    test_execution: &mut TestExecutionAvailability,
    discovered: Result<Option<BTreeMap<String, String>>, String>,
) {
    let mut toolchain = match discovered {
        Ok(Some(toolchain)) => toolchain,
        Ok(None) => {
            test_execution.available = false;
            test_execution.unavailable_reason = Some(
                "no supported Windows MSVC toolchain was found for the optional Cargo test"
                    .to_owned(),
            );
            return;
        }
        Err(error) => {
            test_execution.available = false;
            test_execution.unavailable_reason = Some(format!(
                "Windows MSVC discovery failed for the optional Cargo test: {}",
                bound_protocol_text(&error, 512)
            ));
            return;
        }
    };
    let wrapper = match materialize_windows_msvc_linker_wrapper(&toolchain, target_root) {
        Ok(wrapper) => wrapper,
        Err(error) => {
            test_execution.available = false;
            test_execution.unavailable_reason = Some(format!(
                "Windows MSVC setup failed for the optional Cargo test: {}",
                bound_protocol_text(&error, 512)
            ));
            return;
        }
    };
    toolchain.insert(
        "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER".to_owned(),
        windows_cmd_path(&wrapper),
    );
    for (key, value) in toolchain {
        insert_environment_case_insensitive(environment, key, value);
    }
}

fn worker_developer_instructions(
    instructions: &str,
    test_plan: Option<&TestPlan>,
    availability: &TestExecutionAvailability,
) -> String {
    if test_plan.is_none() || availability.available {
        return instructions.to_owned();
    }
    let reason = availability
        .unavailable_reason
        .as_deref()
        .unwrap_or("the configured test runner is unavailable");
    format!(
        "{instructions}\n\nNeedle runtime constraint: the declared TestPlan remains optional, but test execution is unavailable in this session ({reason}). Do not request the test command. Inspect its definition structurally and return the requested artifacts without command evidence."
    )
}

fn worker_shell_environment(
    process_environment: &BTreeMap<String, String>,
    target_root: &Path,
    temp_root: &Path,
) -> BTreeMap<String, String> {
    const SAFE_PROCESS_KEYS: &[&str] = &[
        "PATH",
        "PATHEXT",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER",
        "INCLUDE",
        "LIB",
        "LIBPATH",
        "UCRTVERSION",
        "UNIVERSALCRTSDKDIR",
        "VCINSTALLDIR",
        "VCTOOLSINSTALLDIR",
        "VSINSTALLDIR",
        "WINDOWSSDKDIR",
        "WINDOWSSDKLIBVERSION",
        "WINDOWSSDKVERSION",
    ];
    let mut environment = BTreeMap::new();
    for (key, value) in process_environment {
        if SAFE_PROCESS_KEYS.iter().any(|allowed| key.eq_ignore_ascii_case(allowed)) {
            insert_environment_case_insensitive(&mut environment, key.clone(), value.clone());
        }
    }
    environment.insert("CARGO_TARGET_DIR".to_owned(), target_root.to_string_lossy().into_owned());
    for key in ["TEMP", "TMP", "TMPDIR"] {
        environment.insert(key.to_owned(), temp_root.to_string_lossy().into_owned());
    }
    environment
}

fn insert_environment_case_insensitive(
    environment: &mut BTreeMap<String, String>,
    key: String,
    value: String,
) {
    if let Some(existing) =
        environment.keys().find(|existing| existing.eq_ignore_ascii_case(&key)).cloned()
    {
        environment.remove(&existing);
    }
    environment.insert(key, value);
}

#[cfg(windows)]
fn discover_windows_msvc_environment(
    base_environment: &BTreeMap<String, String>,
) -> Result<Option<BTreeMap<String, String>>, String> {
    const SAFE_BOOTSTRAP_KEYS: &[&str] = &[
        "NUMBER_OF_PROCESSORS",
        "OS",
        "PROCESSOR_ARCHITECTURE",
        "PROCESSOR_IDENTIFIER",
        "PROCESSOR_LEVEL",
        "PROCESSOR_REVISION",
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "SystemDrive",
    ];
    let mut bootstrap_environment = base_environment.clone();
    for key in SAFE_BOOTSTRAP_KEYS {
        if let Ok(value) = std::env::var(key) {
            insert_environment_case_insensitive(
                &mut bootstrap_environment,
                (*key).to_owned(),
                value,
            );
        }
    }
    bootstrap_environment.insert("VSCMD_SKIP_SENDTELEMETRY".to_owned(), "1".to_owned());
    let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") else {
        return Ok(None);
    };
    let vswhere = PathBuf::from(program_files_x86)
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    if !vswhere.is_file() {
        return Ok(None);
    }
    let output = Command::new(&vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .env_clear()
        .envs(&bootstrap_environment)
        .output()
        .map_err(|error| format!("cannot inspect the Windows MSVC installation: {error}"))?;
    if !output.status.success() {
        return Err(format!("vswhere failed with {}", output.status));
    }
    let installation = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from);
    let Some(installation) = installation else {
        return Ok(None);
    };
    let installation = std::fs::canonicalize(&installation)
        .map_err(|error| format!("cannot resolve Visual Studio installation: {error}"))?;
    let vsdev = std::fs::canonicalize(installation.join("Common7/Tools/VsDevCmd.bat"))
        .map_err(|error| format!("cannot resolve VsDevCmd.bat: {error}"))?;
    if !vsdev.starts_with(&installation) {
        return Err("VsDevCmd.bat resolved outside the Visual Studio installation".to_owned());
    }
    let comspec = base_environment
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("COMSPEC"))
        .map(|(_, value)| PathBuf::from(value))
        .ok_or_else(|| "sanitized environment has no COMSPEC".to_owned())?;
    let command_line = format!(
        "call \"{}\" -no_logo -arch=x64 -host_arch=x64 >nul && set",
        windows_cmd_path(&vsdev)
    );
    let mut command = Command::new(comspec);
    command.args(["/d", "/c"]).raw_arg(command_line).env_clear().envs(&bootstrap_environment);
    let output = command
        .output()
        .map_err(|error| format!("cannot initialize the Windows MSVC environment: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "VsDevCmd.bat failed with {}: {}",
            output.status,
            stderr.trim().chars().take(512).collect::<String>()
        ));
    }
    let mut environment =
        filter_windows_toolchain_environment(&String::from_utf8_lossy(&output.stdout));
    let linker = environment
        .get("PATH")
        .and_then(|path| {
            std::env::split_paths(path)
                .map(|directory| directory.join("link.exe"))
                .find(|candidate| candidate.is_file())
        })
        .ok_or_else(|| "VsDevCmd.bat did not expose link.exe on PATH".to_owned())?;
    let linker = std::fs::canonicalize(linker)
        .map_err(|error| format!("cannot resolve the Windows MSVC linker: {error}"))?;
    environment
        .insert("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER".to_owned(), windows_cmd_path(&linker));
    Ok(Some(environment))
}

#[cfg(windows)]
fn materialize_windows_msvc_linker_wrapper(
    environment: &BTreeMap<String, String>,
    target_root: &Path,
) -> Result<PathBuf, String> {
    let linker = environment
        .get("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER")
        .map(PathBuf::from)
        .ok_or_else(|| "MSVC environment has no absolute Cargo linker".to_owned())?;
    let linker = std::fs::canonicalize(linker)
        .map_err(|error| format!("cannot resolve the configured MSVC linker: {error}"))?;
    let manifest_tool = environment
        .get("PATH")
        .and_then(|path| {
            std::env::split_paths(path)
                .map(|directory| directory.join("mt.exe"))
                .find(|candidate| candidate.is_file())
        })
        .ok_or_else(|| "VsDevCmd.bat did not expose mt.exe on PATH".to_owned())?;
    let manifest_tool = std::fs::canonicalize(manifest_tool)
        .map_err(|error| format!("cannot resolve the Windows manifest tool: {error}"))?;
    let linker_directory =
        linker.parent().ok_or_else(|| "MSVC linker has no parent directory".to_owned())?;
    let manifest_tool_directory = manifest_tool
        .parent()
        .ok_or_else(|| "Windows manifest tool has no parent directory".to_owned())?;
    let linker = safe_windows_batch_path(&linker)?;
    let linker_directory = safe_windows_batch_path(linker_directory)?;
    let manifest_tool_directory = safe_windows_batch_path(manifest_tool_directory)?;
    std::fs::create_dir_all(target_root)
        .map_err(|error| format!("cannot create the worker target root: {error}"))?;
    let wrapper = target_root.join(".needle-msvc-linker.cmd");
    let contents = format!(
        "@echo off\r\nsetlocal\r\nset \"PATH={linker_directory};{manifest_tool_directory};%PATH%\"\r\n\"{linker}\" %*\r\nexit /b %ERRORLEVEL%\r\n"
    );
    std::fs::write(&wrapper, contents)
        .map_err(|error| format!("cannot materialize the MSVC linker wrapper: {error}"))?;
    std::fs::canonicalize(&wrapper)
        .map_err(|error| format!("cannot resolve the MSVC linker wrapper: {error}"))
}

#[cfg(windows)]
fn safe_windows_batch_path(path: &Path) -> Result<String, String> {
    let path = windows_cmd_path(path);
    if path.contains(['"', '%', '\r', '\n']) {
        return Err("MSVC tool path cannot be represented safely in a batch wrapper".to_owned());
    }
    Ok(path)
}

#[cfg(windows)]
fn windows_cmd_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn filter_windows_toolchain_environment(output: &str) -> BTreeMap<String, String> {
    const SAFE_KEYS: &[&str] = &[
        "PATH",
        "INCLUDE",
        "LIB",
        "LIBPATH",
        "UCRTVERSION",
        "UNIVERSALCRTSDKDIR",
        "VCINSTALLDIR",
        "VCTOOLSINSTALLDIR",
        "VSINSTALLDIR",
        "WINDOWSSDKDIR",
        "WINDOWSSDKLIBVERSION",
        "WINDOWSSDKVERSION",
    ];
    let mut environment = BTreeMap::new();
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(canonical_key) =
            SAFE_KEYS.iter().find(|allowed| key.eq_ignore_ascii_case(allowed))
        else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        insert_environment_case_insensitive(
            &mut environment,
            (*canonical_key).to_owned(),
            value.trim().to_owned(),
        );
    }
    environment
}

fn repository_relative_path(root: &Path, candidate: &Path) -> Option<String> {
    let root = std::fs::canonicalize(root).ok()?;
    let candidate = std::fs::canonicalize(candidate).ok()?;
    if !candidate.starts_with(&root) || !candidate.is_file() {
        return None;
    }
    let relative = candidate.strip_prefix(root).ok()?;
    Some(
        relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn command_observation_trace(root: &Path, item: &Value) -> WorkerObservationTrace {
    let status = item.get("status").and_then(Value::as_str);
    let declined_without_execution = matches!(status, Some("declined" | "cancelled" | "canceled"))
        && item.get("exitCode").and_then(Value::as_i64).is_none();
    if declined_without_execution {
        // A command rejected before execution observed no repository state.
        // Persist its approval/evidence record, but do not contaminate the
        // dependency closure of artifacts produced by later valid reads.
        return WorkerObservationTrace::default();
    }
    let mut observed_files = BTreeSet::new();
    let mut gaps = BTreeSet::new();
    let Some(actions) = item.get("commandActions").and_then(Value::as_array) else {
        gaps.insert("command_actions_missing".to_owned());
        return WorkerObservationTrace {
            observed_files: Vec::new(),
            gaps: gaps.into_iter().collect(),
        };
    };
    if actions.is_empty() {
        gaps.insert("command_actions_empty".to_owned());
    }
    for action in actions {
        match action.get("type").and_then(Value::as_str) {
            Some("read") => {
                let Some(path) = action.get("path").and_then(Value::as_str) else {
                    gaps.insert("read_path_missing".to_owned());
                    continue;
                };
                match repository_relative_path(root, Path::new(path)) {
                    Some(path) => {
                        observed_files.insert(path);
                    }
                    None => {
                        gaps.insert("read_path_outside_snapshot".to_owned());
                    }
                }
            }
            Some("search") => {
                gaps.insert("search_result_closure_unproven".to_owned());
            }
            Some("listFiles") => {
                gaps.insert("listing_result_closure_unproven".to_owned());
            }
            Some("unknown") => match validated_read_only_command_argv(item).as_deref() {
                Some(argv) if read_only_executable(argv) == Some("get-content") => {
                    let cwd =
                        item.get("cwd").and_then(Value::as_str).map(Path::new).unwrap_or(root);
                    let Some(path) = get_content_path(argv) else {
                        gaps.insert("unknown_command_action".to_owned());
                        continue;
                    };
                    let candidate = if Path::new(path).is_absolute() {
                        PathBuf::from(path)
                    } else {
                        cwd.join(path)
                    };
                    match repository_relative_path(root, &candidate) {
                        Some(path) => {
                            observed_files.insert(path);
                        }
                        None => {
                            gaps.insert("read_path_outside_snapshot".to_owned());
                        }
                    }
                }
                Some(argv) if read_only_executable(argv) == Some("rg") => {
                    gaps.insert("search_result_closure_unproven".to_owned());
                }
                _ => {
                    gaps.insert("unknown_command_action".to_owned());
                }
            },
            Some(other) => {
                gaps.insert(format!("unsupported_command_action:{other}"));
            }
            None => {
                gaps.insert("command_action_type_missing".to_owned());
            }
        }
    }
    WorkerObservationTrace {
        observed_files: observed_files.into_iter().collect(),
        gaps: gaps.into_iter().collect(),
    }
}

fn validated_read_only_command_argv(value: &Value) -> Option<Vec<String>> {
    let display = value
        .get("command")
        .and_then(Value::as_str)
        .and_then(|command| parse_read_only_command_argv(command).ok())?;
    let action = single_unknown_command_action(value)
        .or_else(|| single_read_only_command_action(value))
        .and_then(|command| parse_read_only_command_argv(command).ok())?;
    (display == action).then_some(display)
}

fn read_only_executable(argv: &[String]) -> Option<&str> {
    let executable = argv.first()?.replace('\\', "/");
    let executable = executable.rsplit('/').next()?.to_ascii_lowercase();
    match executable.as_str() {
        "rg" | "rg.exe" => Some("rg"),
        "get-content" => Some("get-content"),
        _ => None,
    }
}

fn get_content_path(argv: &[String]) -> Option<&str> {
    let mut index = 1;
    let mut path = None;
    while index < argv.len() {
        match argv[index].to_ascii_lowercase().as_str() {
            "-literalpath" => {
                index += 1;
                path = argv.get(index).map(String::as_str);
            }
            "-totalcount" | "-tail" | "-readcount" | "-encoding" => index += 1,
            "-raw" => {}
            _ if !argv[index].starts_with('-') && path.is_none() => {
                path = Some(argv[index].as_str());
            }
            _ => {}
        }
        index += 1;
    }
    path
}

fn command_classification_name(classification: &CommandClassification) -> &'static str {
    match classification {
        CommandClassification::AutoApprovedTest { .. } => "auto_approved_test",
        CommandClassification::AutoApprovedReadOnly { .. } => "auto_approved_read_only",
        CommandClassification::PendingUser => "pending_user",
        CommandClassification::RejectedFileChange => "rejected_file_change",
        CommandClassification::RejectedNetwork => "rejected_network",
        CommandClassification::RejectedUnparseable => "rejected_unparseable",
        CommandClassification::RejectedPolicyMismatch => "rejected_policy_mismatch",
        CommandClassification::Expired => "expired",
    }
}

fn requested_permissions(value: Option<&Value>) -> RequestedPermissions {
    let raw = value.cloned().unwrap_or(Value::Null);
    let mut output = RequestedPermissions { raw, ..RequestedPermissions::default() };
    let Some(value) = value else {
        return output;
    };
    if let Some(paths) = value.pointer("/fileSystem/write").and_then(Value::as_array) {
        output.write_paths.extend(paths.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    if let Some(paths) = value.pointer("/fileSystem/read").and_then(Value::as_array) {
        output.read_paths.extend(paths.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    if let Some(entries) = value.pointer("/fileSystem/entries").and_then(Value::as_array) {
        for entry in entries {
            let Some(path) = entry.pointer("/path/path").and_then(Value::as_str) else {
                continue;
            };
            match entry.get("access").and_then(Value::as_str) {
                Some("write") => output.write_paths.push(path.to_owned()),
                Some("read") => output.read_paths.push(path.to_owned()),
                _ => {}
            }
        }
    }
    output.network = value.pointer("/network/enabled").and_then(Value::as_bool).unwrap_or(false);
    output
}

fn single_unknown_command_action(params: &Value) -> Option<&str> {
    let actions = params.get("commandActions")?.as_array()?;
    let [action] = actions.as_slice() else {
        return None;
    };
    if action.get("type").and_then(Value::as_str) != Some("unknown") {
        return None;
    }
    action.get("command").and_then(Value::as_str).filter(|command| !command.is_empty())
}

fn single_read_only_command_action(params: &Value) -> Option<&str> {
    let actions = params.get("commandActions")?.as_array()?;
    let [action] = actions.as_slice() else {
        return None;
    };
    if !matches!(action.get("type").and_then(Value::as_str), Some("read" | "search")) {
        return None;
    }
    action
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| parse_read_only_command_argv(command).is_ok())
}

fn validated_command_argv(value: &Value, expected_argv: &[String]) -> Vec<String> {
    let display = value
        .get("command")
        .and_then(Value::as_str)
        .and_then(|command| parse_test_command_argv(command, expected_argv).ok());
    let action = single_unknown_command_action(value)
        .and_then(|command| parse_test_command_argv(command, expected_argv).ok());
    if display.as_deref() == Some(expected_argv) && action.as_deref() == Some(expected_argv) {
        expected_argv.to_vec()
    } else {
        Vec::new()
    }
}

fn validated_declared_test_command(
    item: &Value,
    approval_argv: &[String],
    declared_argv: &[String],
) -> bool {
    approval_argv == declared_argv && validated_command_argv(item, declared_argv) == declared_argv
}

fn same_canonical_path(left: &Path, right: &Path) -> bool {
    matches!(
        (std::fs::canonicalize(left), std::fs::canonicalize(right)),
        (Ok(left), Ok(right)) if left == right
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(crate) fn validate_compatibility_fixture(
    codex_version: &str,
    fixture: &Value,
) -> Result<(), String> {
    if fixture.get("codex_version").and_then(Value::as_str) != Some(codex_version) {
        return Err(format!("missing App Server compatibility fixture for Codex {codex_version}"));
    }
    let methods = fixture
        .get("required_methods")
        .and_then(Value::as_array)
        .ok_or_else(|| "compatibility fixture has no required methods".to_owned())?;
    for required in [
        "thread/start",
        "turn/start",
        "turn/interrupt",
        "thread/delete",
        "item/agentMessage/delta",
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
        "turn/completed",
    ] {
        if !methods.iter().any(|method| method.as_str() == Some(required)) {
            return Err(format!("compatibility fixture is missing {required}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_server_launch_is_forward_compatible_and_disables_user_extensions() {
        let config = WorkerConfig {
            executable: "codex".to_owned(),
            model: "gpt-test".to_owned(),
            reasoning: "medium".to_owned(),
            service_tier: None,
            timeout_seconds: 60,
            evidence_failure_policy: needle_core::EvidenceFailurePolicy::DiscardInvalidFact,
            role_profile_provenance: None,
        };
        let command = app_server_command(
            &config,
            &BTreeMap::new(),
            true,
            &["node_repl".to_owned(), "server-name_2".to_owned()],
        );
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(!arguments.iter().any(|argument| argument == "--strict-config"));
        for override_value in [
            "notify=[]",
            "features.hooks=false",
            "features.plugins=false",
            "features.apps=false",
            "features.multi_agent=false",
            "features.code_mode_host=false",
            "mcp_servers={}",
        ] {
            assert!(arguments.iter().any(|argument| argument == override_value));
        }
        for override_value in
            ["mcp_servers.node_repl.enabled=false", "mcp_servers.server-name_2.enabled=false"]
        {
            assert!(arguments.iter().any(|argument| argument == override_value));
        }
    }

    #[test]
    fn mcp_server_overrides_accept_only_safe_dotted_path_segments() {
        for name in ["codegraph", "node_repl", "server-name_2"] {
            assert!(is_safe_mcp_server_name(name));
        }
        for name in ["", "server.with.dots", "server name", r#"server"quoted"#] {
            assert!(!is_safe_mcp_server_name(name));
        }
    }

    #[test]
    fn managed_codex_package_directories_are_prepended_to_process_path() {
        let root = env::temp_dir().join(format!("needle-codex-path-{}", std::process::id()));
        let bin = root.join("bin");
        let resources = root.join("codex-resources");
        let codex_path = root.join("codex-path");
        let _ = std::fs::remove_dir_all(&root);
        for directory in [&bin, &resources, &codex_path] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(root.join("codex-package.json"), b"{}").unwrap();
        let executable = bin.join(if cfg!(windows) { "codex.exe" } else { "codex" });
        std::fs::write(&executable, []).unwrap();
        let existing = env::temp_dir().join("needle-existing-path");
        let mut environment = BTreeMap::from([(
            "Path".to_owned(),
            env::join_paths([&existing]).unwrap().to_string_lossy().into_owned(),
        )]);

        prepend_codex_package_paths(&mut environment, &executable.to_string_lossy()).unwrap();

        let paths = env::split_paths(environment.get("PATH").unwrap()).collect::<Vec<_>>();
        assert_eq!(&paths[..3], &[bin, resources, codex_path]);
        assert_eq!(paths.get(3), Some(&existing));
        assert_eq!(environment.keys().filter(|key| key.eq_ignore_ascii_case("PATH")).count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fixture_is_bound_to_supported_version_and_required_approval_methods() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../fixtures/codex-app-server/0.144.0/compatibility.json"
        ))
        .unwrap();
        validate_compatibility_fixture("0.144.0", &fixture).unwrap();
        assert!(validate_compatibility_fixture("0.145.0", &fixture).is_err());
    }

    #[test]
    fn approval_wait_does_not_consume_worker_turn_timeout() {
        assert_eq!(
            remaining_turn_time(
                Duration::from_secs(60),
                Duration::from_secs(75),
                Duration::from_secs(20),
            ),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            remaining_turn_time(
                Duration::from_secs(60),
                Duration::from_secs(81),
                Duration::from_secs(20),
            ),
            None
        );
    }

    #[test]
    fn permission_parser_preserves_write_and_network_requests() {
        let parsed = requested_permissions(Some(&json!({
            "fileSystem": {
                "write": ["C:/target"],
                "entries": [{
                    "access": "read",
                    "path": {"type": "path", "path": "C:/repo"}
                }]
            },
            "network": {"enabled": true}
        })));
        assert_eq!(parsed.write_paths, vec!["C:/target"]);
        assert_eq!(parsed.read_paths, vec!["C:/repo"]);
        assert!(parsed.network);
    }

    #[test]
    fn structured_command_action_requires_exactly_one_unknown_action() {
        let params = json!({
            "command": "powershell.exe -Command 'cargo test focused -- --exact'",
            "commandActions": [{
                "type": "unknown",
                "command": "cargo test focused -- --exact"
            }]
        });
        assert_eq!(single_unknown_command_action(&params), Some("cargo test focused -- --exact"));
        assert!(
            single_unknown_command_action(&json!({"commandActions": [
                {"type": "unknown", "command": "cargo test focused -- --exact"},
                {"type": "unknown", "command": "whoami"}
            ]}))
            .is_none()
        );
        assert!(
            single_unknown_command_action(&json!({"commandActions": [{
                "type": "search",
                "command": "cargo test focused -- --exact"
            }]}))
            .is_none()
        );
    }

    #[test]
    fn structured_read_and_search_actions_require_a_read_only_argv() {
        for kind in ["read", "search"] {
            let params = json!({
                "commandActions": [{
                    "type": kind,
                    "command": "rg -n needle src"
                }]
            });
            assert_eq!(single_read_only_command_action(&params), Some("rg -n needle src"));
        }
        assert!(
            single_read_only_command_action(&json!({"commandActions": [{
                "type": "search",
                "command": "cargo test"
            }]}))
            .is_none()
        );
        assert!(
            single_read_only_command_action(&json!({"commandActions": [{
                "type": "read",
                "command": "Get-Content src/lib.rs | Select-Object -First 1"
            }]}))
            .is_none()
        );
    }

    #[test]
    fn completed_wrapper_records_argv_only_when_display_and_action_match() {
        let expected = ["cargo", "test", "focused", "--", "--exact"].map(str::to_owned);
        let item = json!({
            "command": "\"powershell.exe\" -Command 'cargo test focused -- --exact'",
            "commandActions": [{
                "type": "unknown",
                "command": "cargo test focused -- --exact"
            }]
        });
        assert_eq!(validated_command_argv(&item, &expected), expected);
        assert!(parse_direct_argv(item["command"].as_str().unwrap()).is_err());
        let mismatch = json!({
            "command": "cmd.exe /c \"cargo test focused -- --exact\"",
            "commandActions": [{
                "type": "unknown",
                "command": "cargo test other -- --exact"
            }]
        });
        assert!(validated_command_argv(&mismatch, &expected).is_empty());
    }

    #[test]
    fn command_trace_records_reads_and_downgrades_unclosed_searches() {
        let root = std::env::temp_dir().join(format!(
            "needle-command-trace-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let source = root.join("src/lib.rs");
        std::fs::write(&source, b"fn answer() {}").unwrap();
        let trace = command_observation_trace(
            &root,
            &json!({
                "commandActions": [
                    {"type": "read", "command": "Get-Content src/lib.rs", "name": "lib.rs", "path": source},
                    {"type": "search", "command": "rg answer", "path": null, "query": "answer"}
                ]
            }),
        );
        assert_eq!(trace.observed_files, vec!["src/lib.rs"]);
        assert_eq!(trace.gaps, vec!["search_result_closure_unproven"]);
        assert!(!trace.is_complete());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn command_trace_closes_unknown_read_only_actions_only_after_exact_reads() {
        let root = std::env::temp_dir().join(format!(
            "needle-unknown-command-trace-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"fn answer() {}").unwrap();

        let read = command_observation_trace(
            &root,
            &json!({
                "command": "powershell.exe -NoProfile -Command 'Get-Content -LiteralPath src/lib.rs -TotalCount 64'",
                "cwd": root,
                "commandActions": [{
                    "type": "unknown",
                    "command": "Get-Content -LiteralPath src/lib.rs -TotalCount 64"
                }]
            }),
        );
        assert_eq!(read.observed_files, vec!["src/lib.rs"]);
        assert!(read.gaps.is_empty());

        let search = command_observation_trace(
            &root,
            &json!({
                "command": "powershell.exe -NoProfile -Command 'rg -n answer src'",
                "cwd": root,
                "commandActions": [{"type": "unknown", "command": "rg -n answer src"}]
            }),
        );
        assert!(search.observed_files.is_empty());
        assert_eq!(search.gaps, vec!["search_result_closure_unproven"]);

        let mismatch = command_observation_trace(
            &root,
            &json!({
                "command": "powershell.exe -NoProfile -Command 'Get-Content -LiteralPath src/lib.rs -TotalCount 64'",
                "cwd": root,
                "commandActions": [{
                    "type": "unknown",
                    "command": "Get-Content -LiteralPath src/other.rs -TotalCount 64"
                }]
            }),
        );
        assert!(mismatch.observed_files.is_empty());
        assert_eq!(mismatch.gaps, vec!["unknown_command_action"]);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validated_declared_test_is_not_a_discovery_gap() {
        let expected = ["cargo", "test", "focused", "--", "--exact"].map(str::to_owned).to_vec();
        let item = json!({
            "command": "powershell.exe -Command 'cargo test focused -- --exact'",
            "commandActions": [{
                "type": "unknown",
                "command": "cargo test focused -- --exact"
            }]
        });
        assert!(validated_declared_test_command(&item, &expected, &expected));
        let other = ["cargo", "test", "other", "--", "--exact"].map(str::to_owned).to_vec();
        assert!(!validated_declared_test_command(&item, &expected, &other));
        assert_eq!(
            command_observation_trace(Path::new("."), &item).gaps,
            vec!["unknown_command_action"]
        );
    }

    #[test]
    fn declined_unexecuted_command_is_not_an_observation_gap() {
        let item = json!({
            "status": "declined",
            "exitCode": null,
            "commandActions": [{
                "type": "unknown",
                "command": "powershell.exe -Command 'rg needle src 2>$null'"
            }]
        });

        assert_eq!(
            command_observation_trace(Path::new("."), &item),
            WorkerObservationTrace::default()
        );
    }

    #[test]
    fn worker_shell_environment_contains_no_home_or_credential_pointer() {
        let process_environment = BTreeMap::from([
            ("PATH".to_owned(), "C:/tools".to_owned()),
            ("USERPROFILE".to_owned(), "C:/Users/private".to_owned()),
            ("OPENAI_API_KEY".to_owned(), "secret".to_owned()),
            ("LIB".to_owned(), "C:/msvc/lib".to_owned()),
            (
                "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER".to_owned(),
                "C:/msvc/bin/link.exe".to_owned(),
            ),
        ]);
        let environment = worker_shell_environment(
            &process_environment,
            Path::new("C:/run/target"),
            Path::new("C:/run/tmp"),
        );
        for forbidden in
            ["CODEX_HOME", "HOME", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "OPENAI_API_KEY"]
        {
            assert!(!environment.contains_key(forbidden));
        }
        assert_eq!(environment.get("LIB").map(String::as_str), Some("C:/msvc/lib"));
        assert_eq!(
            environment.get("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER").map(String::as_str),
            Some("C:/msvc/bin/link.exe")
        );
        assert_eq!(environment.get("CARGO_TARGET_DIR").map(String::as_str), Some("C:/run/target"));
        assert_eq!(environment.get("TEMP").map(String::as_str), Some("C:/run/tmp"));
    }

    #[test]
    fn unavailable_optional_test_adds_a_bounded_no_execution_constraint() {
        let plan = TestPlan {
            runner: "cargo".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned(), "focused".to_owned()],
            cwd_relative: ".".to_owned(),
            test_identifier: "focused".to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        let availability = TestExecutionAvailability {
            available: false,
            unavailable_reason: Some("toolchain unavailable".to_owned()),
        };
        let instructions =
            worker_developer_instructions("base instructions", Some(&plan), &availability);
        assert!(instructions.starts_with("base instructions"));
        assert!(instructions.contains("TestPlan remains optional"));
        assert!(instructions.contains("Do not request the test command"));
        assert!(instructions.contains("toolchain unavailable"));
    }

    #[test]
    fn unsupported_or_untrusted_test_plan_is_unavailable_without_affecting_worker_startup() {
        let cargo = TestPlan {
            runner: "cargo".to_owned(),
            argv: vec!["cargo".to_owned(), "test".to_owned(), "focused".to_owned()],
            cwd_relative: ".".to_owned(),
            test_identifier: "focused".to_owned(),
            requires_approval: true,
            execution_evidence_id: None,
        };
        let untrusted = initial_test_execution_availability(Some(&cargo), false);
        assert!(!untrusted.available);
        assert!(untrusted.unavailable_reason.is_some());

        let mut unsupported = cargo;
        unsupported.runner = "pytest".to_owned();
        unsupported.argv = vec!["pytest".to_owned(), "focused".to_owned()];
        let unsupported = initial_test_execution_availability(Some(&unsupported), true);
        assert!(!unsupported.available);
        assert!(unsupported.unavailable_reason.is_some());
    }

    #[cfg(all(windows, target_env = "msvc"))]
    #[test]
    fn missing_msvc_disables_optional_cargo_test_without_rejecting_worker_environment() {
        let root = std::env::temp_dir().join(format!(
            "needle-optional-msvc-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let target = root.join("target");
        let temp = root.join("temp");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        let mut environment = crate::worker::sanitized_environment();
        let mut availability =
            TestExecutionAvailability { available: true, unavailable_reason: None };

        apply_windows_msvc_environment(&mut environment, &target, &mut availability, Ok(None));

        assert!(!availability.available);
        assert!(
            availability
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("optional Cargo test"))
        );
        assert!(!environment.contains_key("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn windows_toolchain_filter_keeps_only_required_non_secret_values() {
        let environment = filter_windows_toolchain_environment(
            "Path=C:\\MSVC\\bin;C:\\cargo\\bin\r\n\
             LIB=C:\\MSVC\\lib\r\n\
             INCLUDE=C:\\MSVC\\include\r\n\
             OPENAI_API_KEY=secret\r\n\
             HTTPS_PROXY=http://proxy\r\n\
             VSCMD_SKIP_SENDTELEMETRY=1\r\n",
        );
        assert_eq!(
            environment.get("PATH").map(String::as_str),
            Some("C:\\MSVC\\bin;C:\\cargo\\bin")
        );
        assert!(!environment.contains_key("Path"));
        assert_eq!(environment.get("LIB").map(String::as_str), Some("C:\\MSVC\\lib"));
        assert_eq!(environment.get("INCLUDE").map(String::as_str), Some("C:\\MSVC\\include"));
        assert_eq!(environment.len(), 3);
    }

    #[cfg(windows)]
    #[test]
    fn msvc_linker_wrapper_prepends_linker_and_manifest_tool_directories() {
        let root = std::env::temp_dir().join(format!(
            "needle-msvc-wrapper-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let linker_directory = root.join("msvc bin");
        let manifest_directory = root.join("sdk bin");
        let target = root.join("target");
        std::fs::create_dir_all(&linker_directory).unwrap();
        std::fs::create_dir_all(&manifest_directory).unwrap();
        let linker = linker_directory.join("link.exe");
        let manifest_tool = manifest_directory.join("mt.exe");
        std::fs::write(&linker, b"link").unwrap();
        std::fs::write(&manifest_tool, b"mt").unwrap();
        let environment = BTreeMap::from([
            (
                "PATH".to_owned(),
                std::env::join_paths([&linker_directory, &manifest_directory])
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER".to_owned(),
                windows_cmd_path(&std::fs::canonicalize(&linker).unwrap()),
            ),
        ]);
        let wrapper = materialize_windows_msvc_linker_wrapper(&environment, &target).unwrap();
        let contents = std::fs::read_to_string(wrapper).unwrap();
        assert!(
            contents.contains(&windows_cmd_path(&std::fs::canonicalize(linker_directory).unwrap()))
        );
        assert!(
            contents
                .contains(&windows_cmd_path(&std::fs::canonicalize(manifest_directory).unwrap()))
        );
        assert!(contents.contains(&windows_cmd_path(&std::fs::canonicalize(linker).unwrap())));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(all(windows, target_env = "msvc"))]
    #[test]
    #[ignore = "requires an installed Visual Studio C++ toolchain"]
    fn installed_windows_msvc_environment_exposes_linker_and_libraries() {
        let environment =
            discover_windows_msvc_environment(&crate::worker::sanitized_environment())
                .unwrap()
                .expect("Visual Studio C++ toolchain");
        let path = environment.get("PATH").expect("canonical MSVC PATH");
        assert!(std::env::split_paths(path).any(|entry| entry.join("link.exe").is_file()));
        let linker = Path::new(
            environment
                .get("CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER")
                .expect("absolute Cargo MSVC linker"),
        );
        assert!(linker.is_absolute());
        assert!(linker.is_file());
        assert!(linker.file_name().is_some_and(|name| name.eq_ignore_ascii_case("link.exe")));
        assert!(environment.get("LIB").is_some_and(|value| !value.is_empty()));
        assert!(environment.get("INCLUDE").is_some_and(|value| !value.is_empty()));
    }

    #[test]
    fn only_three_transient_command_decisions_are_serializable() {
        for decision in
            [ApprovalDecision::Accept, ApprovalDecision::Decline, ApprovalDecision::Cancel]
        {
            assert!(matches!(
                serde_json::to_value(decision).unwrap().as_str(),
                Some("accept" | "decline" | "cancel")
            ));
        }
    }
}
