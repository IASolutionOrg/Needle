use super::{
    AppError, canonical_child_path, ensure_codex_authenticated, ensure_dedicated_codex_home,
    option_value, required_value, resolve_codex,
};
use crate::mcp::schema::McpNeedContextRequest;
use needle_bench::parse_codex_jsonl;
use needle_platform_codex::CodexWorker;
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_EVENT_LINE_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_EVENT_COUNT: usize = 512;
const CALIBRATION_MCP_ENV_CONFIG: &str =
    "mcp_servers.needle.env={NEEDLE_INTERNAL_CALIBRATION_REUSE='partial-tests-live'}";
const DEVELOPER_INSTRUCTIONS_PREFIX: &str = "Needle is the required context path for this task. Before any repository command, search, file read, or explanatory message, call the configured Needle MCP need_context tool exactly once with the frozen JSON arguments below. Preserve every typed field and value. Use returned certified context and do not repeat covered discovery. If the tool is unavailable or fails, stop and report the failure. This is a cache-only transport check: repository discovery is forbidden.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Preflight,
    Offline,
    Paid,
}

pub(super) struct GuardState {
    expected_request_digests: Vec<needle_core::Digest>,
    active_request_digest: Option<needle_core::Digest>,
    pub(super) mcp_started: bool,
    pub(super) mcp_succeeded: bool,
    pub(super) discovery_events: u32,
    pub(super) final_response: Option<String>,
    pub(super) observed_request_digests: Vec<needle_core::Digest>,
}

impl GuardState {
    #[cfg(test)]
    fn new(expected_request_digest: needle_core::Digest) -> Self {
        Self::new_sequence(vec![expected_request_digest])
    }

    pub(super) fn new_sequence(expected_request_digests: Vec<needle_core::Digest>) -> Self {
        Self {
            expected_request_digests,
            active_request_digest: None,
            mcp_started: false,
            mcp_succeeded: false,
            discovery_events: 0,
            final_response: None,
            observed_request_digests: Vec::new(),
        }
    }

    fn observe(&mut self, event: &Value) -> Option<String> {
        let event_type = event.get("type").and_then(Value::as_str)?;
        let item = event.get("item");
        let item_type = item.and_then(|value| value.get("type")).and_then(Value::as_str);
        if matches!(event_type, "item.started" | "item.completed")
            && matches!(
                item_type,
                Some("command_execution" | "shell_command" | "file_read" | "search" | "tool_call")
            )
        {
            self.discovery_events = self.discovery_events.saturating_add(1);
            return Some("repository discovery was attempted during the MCP-only arm".to_owned());
        }
        if matches!(event_type, "item.started" | "item.completed")
            && item_type == Some("mcp_tool_call")
        {
            let name = item.and_then(mcp_tool_name);
            if name != Some("need_context") {
                return Some(format!(
                    "unexpected MCP tool `{}` was attempted",
                    name.unwrap_or("unknown")
                ));
            }
            let observed_arguments = item.and_then(|value| value.get("arguments")).cloned();
            let observed_digest = observed_arguments.as_ref().and_then(canonical_request_digest);
            let expected_digest =
                self.expected_request_digests.get(self.observed_request_digests.len());
            if observed_digest.as_ref() != expected_digest {
                return Some(
                    "Needle MCP call did not preserve the frozen semantic request sequence"
                        .to_owned(),
                );
            }
            self.mcp_started = true;
            if event_type == "item.started" {
                self.active_request_digest = observed_digest;
            }
            if event_type == "item.completed" {
                if self.active_request_digest != observed_digest {
                    return Some(
                        "Needle MCP completion did not match the active request".to_owned(),
                    );
                }
                let failed = item.is_some_and(|value| {
                    value.get("error").is_some_and(|error| !error.is_null())
                        || value.get("status").and_then(Value::as_str) == Some("failed")
                });
                if failed {
                    return Some("Needle MCP call failed".to_owned());
                }
                if let Some(digest) = observed_digest {
                    self.observed_request_digests.push(digest);
                }
                self.active_request_digest = None;
                self.mcp_succeeded =
                    self.observed_request_digests.len() == self.expected_request_digests.len();
            }
            return None;
        }
        if event_type == "item.error" && item_type == Some("mcp_tool_call") {
            return Some("Needle MCP call emitted item.error".to_owned());
        }
        if event_type == "item.completed" && item_type == Some("agent_message") {
            let text = item.and_then(item_text).unwrap_or_default();
            if !self.mcp_succeeded {
                return Some(
                    "main emitted a message before a successful Needle MCP call".to_owned(),
                );
            }
            self.final_response = Some(text.to_owned());
        }
        if event_type == "turn.completed" && !self.mcp_succeeded {
            return Some("turn completed without a successful Needle MCP call".to_owned());
        }
        None
    }
}

fn canonical_request_digest(arguments: &Value) -> Option<needle_core::Digest> {
    let encoded_bytes = serde_json::to_vec(arguments).ok()?.len();
    let request = serde_json::from_value::<McpNeedContextRequest>(arguments.clone()).ok()?;
    let routes = needle_core::built_in_route_contracts()
        .into_iter()
        .map(|contract| contract.route.as_str().to_owned())
        .collect::<Vec<_>>();
    request.validate_and_map(&routes, encoded_bytes).ok().map(|mapped| mapped.request_digest)
}

pub(super) fn run(arguments: &[String]) -> Result<(), AppError> {
    let mode = mode(arguments)?;
    let codex = resolve_codex(option_value(arguments, "--codex"))?;
    let codex_home = PathBuf::from(required_value(arguments, "--codex-home")?);
    ensure_dedicated_codex_home(&codex_home)?;
    let codex_home = canonical_child_path(&codex_home)?;
    let needle = canonical_child_path(Path::new(&required_value(arguments, "--needle")?))?;
    let source =
        canonical_child_path(Path::new(&required_value(arguments, "--source-repository")?))?;
    let data_directory =
        canonical_child_path(Path::new(&required_value(arguments, "--data-dir")?))?;
    let prompt_path =
        canonical_child_path(Path::new(&required_value(arguments, "--prompt-file")?))?;
    let request_path =
        canonical_child_path(Path::new(&required_value(arguments, "--mcp-request-file")?))?;
    if mode != Mode::Offline {
        ensure_codex_authenticated(&codex, &codex_home, "mcp-live")?;
    }
    let artifact_root = PathBuf::from(required_value(arguments, "--artifact-root")?);
    if artifact_root.exists() {
        return Err(AppError::Experiment(format!(
            "MCP live artifact root already exists: {}",
            artifact_root.display()
        )));
    }
    fs::create_dir_all(&artifact_root)?;
    let artifact_root = canonical_child_path(&artifact_root)?;
    let model = required_value(arguments, "--main-model")?;
    let reasoning = required_value(arguments, "--main-reasoning")?;
    let service_tier = required_value(arguments, "--service-tier")?;
    let expected_result = required_value(arguments, "--expected-result-digest")?;
    let expected_request: Value = serde_json::from_slice(&fs::read(&request_path)?)?;
    let expected_request_digest = canonical_request_digest(&expected_request).ok_or_else(|| {
        AppError::Experiment(
            "frozen MCP request is not a valid need_context argument object".to_owned(),
        )
    })?;
    let timeout_seconds = option_value(arguments, "--timeout-seconds")
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| AppError::Usage(format!("invalid --timeout-seconds: {error}")))?
        .unwrap_or(90);
    if !(10..=180).contains(&timeout_seconds) {
        return Err(AppError::Usage("--timeout-seconds must be between 10 and 180".to_owned()));
    }
    let isolation =
        CodexWorker::verify_isolation(&codex.display().to_string()).map_err(AppError::Runtime)?;
    if !isolation.verified() {
        return Err(AppError::Experiment("Codex transport isolation preflight failed".to_owned()));
    }

    if mode == Mode::Preflight {
        return run_preflight(PreflightInput {
            codex: &codex,
            codex_home: &codex_home,
            needle: &needle,
            source: &source,
            data_directory: &data_directory,
            artifact_root: &artifact_root,
            model: &model,
            request_path: &request_path,
            expected_result: &expected_result,
            codex_version: &isolation.codex_version,
        });
    }

    let approved_budget_microcredits = if mode == Mode::Paid {
        let value = required_value(arguments, "--approved-budget-microcredits")?
            .parse::<u64>()
            .map_err(|error| AppError::Usage(format!("invalid approved budget: {error}")))?;
        if value == 0 {
            return Err(AppError::Usage("approved budget must be positive".to_owned()));
        }
        Some(value)
    } else {
        None
    };
    let prompt = fs::read_to_string(prompt_path)?;
    let developer_instructions = format!(
        "{DEVELOPER_INSTRUCTIONS_PREFIX}\n\n<needle_mcp_request_json>\n{}\n</needle_mcp_request_json>",
        serde_json::to_string(&expected_request)?
    );
    let observation_path = data_directory.join("mcp-observations.jsonl");
    let observations_before = line_count(&observation_path)?;
    let result = run_guarded(GuardedInput {
        codex: &codex,
        codex_home: &codex_home,
        source: &source,
        artifact_root: &artifact_root,
        data_directory: &data_directory,
        prompt: prompt.trim(),
        model: &model,
        reasoning: &reasoning,
        service_tier: &service_tier,
        timeout: Duration::from_secs(timeout_seconds),
        require_observation: mode == Mode::Paid,
        expected_request_digests: vec![expected_request_digest],
        developer_instructions: &developer_instructions,
        extra_config: &[],
        calibration_reuse: false,
    })?;
    let observations_after = line_count(&observation_path)?;
    let observation = last_jsonl_value(&observation_path)?;
    let parsed = parse_codex_jsonl(&result.stdout);
    let observation_valid = observation.as_ref().is_some_and(|value| {
        value.get("cache_hit").and_then(Value::as_bool) == Some(true)
            && value.get("worker_spawned").and_then(Value::as_bool) == Some(false)
            && value.get("result_digest").and_then(Value::as_str) == Some(expected_result.as_str())
    });
    let observation_gate = mode == Mode::Offline
        || (observations_after == observations_before.saturating_add(1) && observation_valid);
    let passed = result.abort_reason.is_none()
        && result.status_success
        && result.guard.mcp_succeeded
        && result.guard.discovery_events == 0
        && result.guard.final_response.is_some()
        && parsed.terminal_success == Some(true)
        && observation_gate;
    let report = json!({
        "schema": "needle.mcp-live-observation/2",
        "mode": if mode == Mode::Paid { "paid" } else { "offline-simulator" },
        "passed": passed,
        "codex_version": isolation.codex_version,
        "provider_observations_started": u8::from(mode == Mode::Paid),
        "approved_budget_microcredits": approved_budget_microcredits,
        "estimate_is_hard_provider_ceiling": false,
        "automatic_retries": 0,
        "mcp_required": true,
        "mcp_enabled_tools": ["need_context"],
        "developer_instructions_digest": needle_core::Digest::blake3(&developer_instructions),
        "expected_request_digest": expected_request_digest,
        "observed_request_digest": result.guard.observed_request_digests.first(),
        "process": {
            "success": result.status_success,
            "exit_code": result.exit_code,
            "timed_out": result.timed_out,
            "abort_reason": result.abort_reason,
            "duration_ms": result.duration_ms,
        },
        "mcp_call_started": result.guard.mcp_started,
        "mcp_call_succeeded": result.guard.mcp_succeeded,
        "repository_discovery_events": result.guard.discovery_events,
        "final_response_present": result.guard.final_response.is_some(),
        "usage": {
            "input_tokens": parsed.usage.input_tokens,
            "cached_input_tokens": parsed.usage.cached_input_tokens,
            "output_tokens": parsed.usage.output_tokens,
        },
        "observations_before": observations_before,
        "observations_after": observations_after,
        "observation": observation,
        "expected_result_digest": expected_result,
    });
    let report_path = artifact_root.join("report.json");
    fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !passed {
        return Err(AppError::Experiment(format!(
            "MCP-only observation failed closed; report: {}",
            report_path.display()
        )));
    }
    Ok(())
}

struct PreflightInput<'a> {
    codex: &'a Path,
    codex_home: &'a Path,
    needle: &'a Path,
    source: &'a Path,
    data_directory: &'a Path,
    artifact_root: &'a Path,
    model: &'a str,
    request_path: &'a Path,
    expected_result: &'a str,
    codex_version: &'a str,
}

fn run_preflight(input: PreflightInput<'_>) -> Result<(), AppError> {
    let preflight_data = input.artifact_root.join("data");
    fs::create_dir_all(&preflight_data)?;
    fs::copy(input.data_directory.join("needle.sqlite3"), preflight_data.join("needle.sqlite3"))?;
    let request: Value = serde_json::from_slice(&fs::read(input.request_path)?)?;
    let responses =
        direct_mcp_exchange(input.needle, &preflight_data, input.source, input.model, &request)?;
    let initialize = &responses[0];
    let tools = &responses[1];
    let call = &responses[2];
    let instructions =
        initialize.pointer("/result/instructions").and_then(Value::as_str).unwrap_or_default();
    let tool = tools.pointer("/result/tools/0").unwrap_or(&Value::Null);
    let observation = last_jsonl_value(&preflight_data.join("mcp-observations.jsonl"))?;
    let direct_valid = !instructions.is_empty()
        && instructions.len() <= 512
        && !instructions.contains("@@need")
        && tool.get("name").and_then(Value::as_str) == Some("need_context")
        && tool.pointer("/annotations/readOnlyHint").and_then(Value::as_bool) == Some(true)
        && tool.pointer("/annotations/destructiveHint").and_then(Value::as_bool) == Some(false)
        && tool.pointer("/annotations/openWorldHint").and_then(Value::as_bool) == Some(false)
        && tool.pointer("/inputSchema/properties/route/type").and_then(Value::as_str)
            == Some("string")
        && tool.pointer("/inputSchema/properties/need").is_none()
        && tool.pointer("/inputSchema/properties/request").is_none()
        && tool.pointer("/inputSchema/additionalProperties").and_then(Value::as_bool)
            == Some(false)
        && tool.pointer("/outputSchema/additionalProperties").and_then(Value::as_bool)
            == Some(false)
        && call.pointer("/result/isError").and_then(Value::as_bool) == Some(false)
        && call.pointer("/result/structuredContent/context")
            == call.pointer("/result/content/0/text")
        && observation.as_ref().is_some_and(|value| {
            value.get("cache_hit").and_then(Value::as_bool) == Some(true)
                && value.get("worker_spawned").and_then(Value::as_bool) == Some(false)
                && value.get("result_digest").and_then(Value::as_str) == Some(input.expected_result)
        });
    let config = verify_codex_mcp_configuration(input.codex, input.codex_home)?;
    let config_text = String::from_utf8_lossy(&config.stdout);
    let config_valid = config.status.success()
        && config_text.contains(&input.needle.display().to_string())
        && config_text.contains(&input.data_directory.display().to_string())
        && config_text.contains(&input.source.display().to_string())
        && config_text.contains("enabled_tools: need_context")
        && config_text.contains("default_tools_approval_mode: auto");
    let passed = direct_valid && config_valid;
    let report = json!({
        "schema": "needle.mcp-live-preflight/1",
        "passed": passed,
        "provider_observations_started": 0,
        "dedicated_auth_verified": true,
        "codex_version": input.codex_version,
        "server_instructions_present": !instructions.is_empty(),
        "server_instructions_self_contained_under_512_bytes": instructions.len() <= 512,
        "tool_annotations_valid": direct_valid,
        "direct_cache_hit": observation,
        "codex_mcp_configuration_valid": config_valid,
        "codex_mcp_configuration": config_text.trim(),
        "live_guard": {
            "requires_first_action_need_context": true,
            "aborts_on_agent_message_before_tool": true,
            "aborts_on_any_repository_discovery": true,
            "maximum_event_bytes": MAX_EVENT_BYTES,
            "maximum_event_count": MAX_EVENT_COUNT,
        },
        "automatic_retries": 0,
        "estimate_is_hard_provider_ceiling": false,
        "explicit_user_approval_required": true,
    });
    fs::write(input.artifact_root.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !passed {
        return Err(AppError::Experiment("MCP live preflight failed".to_owned()));
    }
    Ok(())
}

fn direct_mcp_exchange(
    needle: &Path,
    data_directory: &Path,
    source: &Path,
    model: &str,
    request_arguments: &Value,
) -> Result<Vec<Value>, AppError> {
    let mut child = Command::new(needle)
        .args(["mcp", "serve", "--data-dir"])
        .arg(data_directory)
        .arg("--repository")
        .arg(source)
        .args(["--main-model", model, "--cache-only"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| AppError::Experiment("MCP stdin unavailable".to_owned()))?;
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"needle-preflight","version":crate::VERSION}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":{"progressToken":0}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"need_context","arguments":request_arguments,"_meta":{"progressToken":1}}}),
    ] {
        serde_json::to_writer(&mut stdin, &request)?;
        stdin.write_all(b"\n")?;
    }
    drop(stdin);
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .ok_or_else(|| AppError::Experiment("MCP stdout unavailable".to_owned()))?
        .read_to_string(&mut stdout)?;
    let status = child.wait()?;
    if !status.success() {
        return Err(AppError::Experiment("direct MCP preflight process failed".to_owned()));
    }
    let responses = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<Vec<Value>, _>>()?;
    if responses.len() != 3 {
        return Err(AppError::Experiment(format!(
            "direct MCP preflight returned {} responses instead of 3",
            responses.len()
        )));
    }
    Ok(responses)
}

fn verify_codex_mcp_configuration(
    codex: &Path,
    codex_home: &Path,
) -> Result<std::process::Output, AppError> {
    let output = Command::new(codex)
        .args([
            "-c",
            "mcp_servers.needle.required=true",
            "-c",
            "mcp_servers.needle.enabled_tools=['need_context']",
            "-c",
            "mcp_servers.needle.default_tools_approval_mode='auto'",
            "-c",
            "mcp_servers.needle.tools.need_context.approval_mode='auto'",
            "mcp",
            "get",
            "needle",
        ])
        .env("CODEX_HOME", codex_home)
        .output()?;
    Ok(output)
}

pub(super) struct GuardedInput<'a> {
    pub(super) codex: &'a Path,
    pub(super) codex_home: &'a Path,
    pub(super) source: &'a Path,
    pub(super) artifact_root: &'a Path,
    pub(super) data_directory: &'a Path,
    pub(super) prompt: &'a str,
    pub(super) model: &'a str,
    pub(super) reasoning: &'a str,
    pub(super) service_tier: &'a str,
    pub(super) timeout: Duration,
    pub(super) require_observation: bool,
    pub(super) expected_request_digests: Vec<needle_core::Digest>,
    pub(super) developer_instructions: &'a str,
    pub(super) extra_config: &'a [String],
    pub(super) calibration_reuse: bool,
}

pub(super) struct GuardedResult {
    pub(super) stdout: String,
    pub(super) status_success: bool,
    pub(super) exit_code: Option<i32>,
    pub(super) timed_out: bool,
    pub(super) abort_reason: Option<String>,
    pub(super) duration_ms: u64,
    pub(super) guard: GuardState,
}

pub(super) fn run_guarded(input: GuardedInput<'_>) -> Result<GuardedResult, AppError> {
    let stdout_path = input.artifact_root.join("main-stdout.jsonl");
    let stderr_path = input.artifact_root.join("main-stderr.log");
    let mut command = Command::new(input.codex);
    configure_guarded_arguments(&mut command, &input);
    command
        .current_dir(input.source)
        .env("CODEX_HOME", input.codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(File::create(&stderr_path)?));
    let started = Instant::now();
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Experiment("Codex stdout unavailable".to_owned()))?;
    let (sender, receiver) = mpsc::sync_channel(32);
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sender.send(line.map_err(|error| error.to_string())).is_err() {
                break;
            }
        }
    });
    let mut output = BufWriter::new(File::create(&stdout_path)?);
    let mut captured = String::new();
    let mut guard = GuardState::new_sequence(input.expected_request_digests);
    let mut abort_reason = None;
    let mut timed_out = false;
    let mut event_count = 0_usize;
    let status = loop {
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(Ok(line)) => {
                event_count = event_count.saturating_add(1);
                if line.len() > MAX_EVENT_LINE_BYTES
                    || captured.len().saturating_add(line.len()).saturating_add(1) > MAX_EVENT_BYTES
                    || event_count > MAX_EVENT_COUNT
                {
                    abort_reason = Some("Codex JSONL exceeded the bounded output cap".to_owned());
                } else {
                    output.write_all(line.as_bytes())?;
                    output.write_all(b"\n")?;
                    captured.push_str(&line);
                    captured.push('\n');
                    match serde_json::from_str::<Value>(&line) {
                        Ok(event) => {
                            if abort_reason.is_none() {
                                abort_reason = guard.observe(&event);
                            }
                        }
                        Err(error) => {
                            abort_reason = Some(format!("invalid Codex JSONL event: {error}"));
                        }
                    }
                }
                if abort_reason.is_some() {
                    terminate(&mut child);
                    break child.wait().ok();
                }
            }
            Ok(Err(error)) => {
                abort_reason = Some(error);
                terminate(&mut child);
                break child.wait().ok();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break child.wait().ok(),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait()? {
                    break Some(status);
                }
            }
        }
        if started.elapsed() >= input.timeout {
            timed_out = true;
            abort_reason = Some("MCP-only Codex turn timed out".to_owned());
            terminate(&mut child);
            break child.wait().ok();
        }
    };
    output.flush()?;
    let status_success = status.as_ref().is_some_and(|status| status.success());
    let exit_code = status.as_ref().and_then(|status| status.code());
    if !status_success && abort_reason.is_none() {
        let diagnostic = fs::read_to_string(&stderr_path).unwrap_or_default();
        let preview = diagnostic.trim().chars().take(1024).collect::<String>();
        abort_reason = Some(if preview.is_empty() {
            format!("Codex exited unsuccessfully with code {exit_code:?}")
        } else {
            format!("Codex exited unsuccessfully with code {exit_code:?}: {preview}")
        });
    }
    let observations = line_count(&input.data_directory.join("mcp-observations.jsonl"))?;
    if input.require_observation
        && guard.mcp_succeeded
        && observations == 0
        && abort_reason.is_none()
    {
        abort_reason =
            Some("Codex reported MCP success but Needle recorded no observation".to_owned());
    }
    Ok(GuardedResult {
        stdout: captured,
        status_success,
        exit_code,
        timed_out,
        abort_reason,
        duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        guard,
    })
}

fn configure_guarded_arguments(command: &mut Command, input: &GuardedInput<'_>) {
    command
        .args(["exec", "--json", "--ephemeral", "--strict-config"])
        .arg("--model")
        .arg(input.model)
        .args(["-c", &format!("model_reasoning_effort='{}'", input.reasoning)])
        .args(["-c", &format!("service_tier='{}'", input.service_tier)])
        .args([
            "-c",
            &format!(
                "developer_instructions={}",
                toml::Value::String(input.developer_instructions.to_owned())
            ),
        ])
        .args(["-c", "mcp_servers.needle.required=true"])
        .args(["-c", "mcp_servers.needle.enabled_tools=['need_context']"])
        .args(["-c", "mcp_servers.needle.default_tools_approval_mode='auto'"])
        .args(["-c", "mcp_servers.needle.tools.need_context.approval_mode='auto'"])
        .args(["-c", "web_search='disabled'"])
        .args(["-c", "project_doc_max_bytes=0"])
        .args(["--sandbox", "read-only", "--cd"])
        .arg(input.source);
    if input.calibration_reuse {
        command.args(["-c", CALIBRATION_MCP_ENV_CONFIG]);
    }
    for config in input.extra_config {
        command.args(["-c", config]);
    }
    command.arg("--").arg(input.prompt);
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
}

fn mode(arguments: &[String]) -> Result<Mode, AppError> {
    let modes = [
        ("--preflight-only", Mode::Preflight),
        ("--execute-offline-simulator", Mode::Offline),
        ("--execute-paid", Mode::Paid),
    ];
    let selected = modes
        .into_iter()
        .filter(|(flag, _)| arguments.iter().any(|argument| argument == flag))
        .map(|(_, mode)| mode)
        .collect::<Vec<_>>();
    match selected.as_slice() {
        [mode] => Ok(*mode),
        _ => Err(AppError::Usage(
            "mcp-live requires exactly one of --preflight-only, --execute-offline-simulator or --execute-paid"
                .to_owned(),
        )),
    }
}

fn mcp_tool_name(item: &Value) -> Option<&str> {
    item.get("name")
        .or_else(|| item.get("tool_name"))
        .or_else(|| item.get("tool"))
        .and_then(Value::as_str)
}

fn item_text(item: &Value) -> Option<&str> {
    item.get("text")
        .or_else(|| item.get("message"))
        .or_else(|| item.get("content"))
        .and_then(Value::as_str)
}

fn line_count(path: &Path) -> Result<u64, AppError> {
    if !path.exists() {
        return Ok(0);
    }
    Ok(BufReader::new(File::open(path)?).lines().count().try_into().unwrap_or(u64::MAX))
}

fn last_jsonl_value(path: &Path) -> Result<Option<Value>, AppError> {
    if !path.exists() {
        return Ok(None);
    }
    let mut last = None;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if !line.trim().is_empty() {
            last = Some(serde_json::from_str(&line)?);
        }
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frozen_request() -> Value {
        json!({
            "route": "locate.implementation",
            "subject": {"kind": "symbol", "name": "answer"},
            "required": [],
            "preferred": [],
            "world": {"source": "current", "platform": "current", "features": "default"},
            "task": "Locate answer."
        })
    }

    #[test]
    fn guard_requires_mcp_before_the_first_main_message() {
        let mut guard = GuardState::new(canonical_request_digest(&frozen_request()).unwrap());
        assert!(
            guard
                .observe(&json!({"type":"item.completed","item":{"type":"agent_message","text":"I will inspect"}}))
                .unwrap()
                .contains("before a successful Needle MCP call")
        );
    }

    #[test]
    fn guard_accepts_mcp_then_final_and_rejects_discovery() {
        let request = frozen_request();
        let mut guard = GuardState::new(canonical_request_digest(&request).unwrap());
        assert_eq!(
            guard.observe(&json!({"type":"item.started","item":{"type":"mcp_tool_call","tool":"need_context","arguments":request}})),
            None
        );
        assert_eq!(
            guard.observe(&json!({"type":"item.completed","item":{"type":"mcp_tool_call","tool":"need_context","arguments":frozen_request(),"result":{}}})),
            None
        );
        assert_eq!(
            guard.observe(
                &json!({"type":"item.completed","item":{"type":"agent_message","text":"answer"}})
            ),
            None
        );
        assert!(guard.mcp_succeeded);
        assert_eq!(guard.final_response.as_deref(), Some("answer"));
        assert!(
            guard
                .observe(&json!({"type":"item.started","item":{"type":"command_execution"}}))
                .unwrap()
                .contains("repository discovery")
        );
    }

    #[test]
    fn guard_reports_failed_mcp_completion_directly() {
        let request = frozen_request();
        let mut guard = GuardState::new(canonical_request_digest(&request).unwrap());
        assert_eq!(
            guard.observe(&json!({"type":"item.started","item":{"type":"mcp_tool_call","tool":"need_context","arguments":request}})),
            None
        );
        assert_eq!(
            guard.observe(&json!({"type":"item.completed","item":{"type":"mcp_tool_call","tool":"need_context","arguments":frozen_request(),"status":"failed","error":null}})),
            Some("Needle MCP call failed".to_owned())
        );
        assert!(!guard.mcp_succeeded);
    }

    #[test]
    fn guarded_extra_config_precedes_prompt_separator() {
        let extra = "mcp_servers.needle.command='needle.exe'".to_owned();
        let input = GuardedInput {
            codex: Path::new("codex.exe"),
            codex_home: Path::new("codex-home"),
            source: Path::new("source"),
            artifact_root: Path::new("artifacts"),
            data_directory: Path::new("data"),
            prompt: "prompt",
            model: "main",
            reasoning: "medium",
            service_tier: "default",
            timeout: Duration::from_secs(30),
            require_observation: true,
            expected_request_digests: Vec::new(),
            developer_instructions: "instructions",
            extra_config: std::slice::from_ref(&extra),
            calibration_reuse: true,
        };
        let mut command = Command::new("codex.exe");
        configure_guarded_arguments(&mut command, &input);
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let separator = arguments.iter().position(|argument| argument == "--").unwrap();
        let extra_position = arguments.iter().position(|argument| argument == &extra).unwrap();
        let calibration_position =
            arguments.iter().position(|argument| argument == CALIBRATION_MCP_ENV_CONFIG).unwrap();
        assert!(calibration_position < separator);
        assert!(extra_position < separator);
        assert_eq!(&arguments[separator + 1..], &["prompt"]);
    }

    #[test]
    fn guarded_non_calibration_run_does_not_configure_mcp_override() {
        let input = GuardedInput {
            codex: Path::new("codex.exe"),
            codex_home: Path::new("codex-home"),
            source: Path::new("source"),
            artifact_root: Path::new("artifacts"),
            data_directory: Path::new("data"),
            prompt: "prompt",
            model: "main",
            reasoning: "medium",
            service_tier: "default",
            timeout: Duration::from_secs(30),
            require_observation: true,
            expected_request_digests: Vec::new(),
            developer_instructions: "instructions",
            extra_config: &[],
            calibration_reuse: false,
        };
        let mut command = Command::new("codex.exe");
        configure_guarded_arguments(&mut command, &input);
        assert!(command.get_args().all(|argument| argument != CALIBRATION_MCP_ENV_CONFIG));
    }

    #[test]
    fn guard_rejects_a_rewritten_semantic_request() {
        let mut guard = GuardState::new(canonical_request_digest(&frozen_request()).unwrap());
        let failure = guard
            .observe(&json!({
                "type":"item.started",
                "item":{
                    "type":"mcp_tool_call",
                    "tool":"need_context",
                    "arguments":{"route":"locate.implementation","subject":{"kind":"symbol","name":"other"},"task":"Locate answer."}
                }
            }))
            .unwrap();
        assert!(failure.contains("did not preserve the frozen semantic request"));
        assert!(!guard.mcp_started);
    }
}
