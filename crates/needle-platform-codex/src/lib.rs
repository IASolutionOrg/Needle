//! Codex hook transport adapter.  Inputs deliberately accept unknown fields,
//! while outputs are small, strict protocol objects.

mod app_server;
mod patcher;
mod verifier;
mod worker;

pub use app_server::{
    ActiveTurnInterruption, CodexMainSession, ContinueWorkingResult, MainContinuationDiagnostics,
    MainDirectFailure, MainFinalTurn, MainNeedDiagnostics, MainNeedRelation, MainNeedTurn,
    MainSessionConfig, MainTurnResult, MainUsage, PILOT_MAIN_REPOSITORY_INSPECTION_INSTRUCTIONS,
};
pub use patcher::{CodexPatchWorker, PatchContextItem, PrepareChangeOutcome};
pub use verifier::{CodexVerifier, VerifyChangeOutcome};
pub use worker::{
    CodexWorker, IsolationReport, TransportPreflightReport, WorkerDiagnosticContract,
};

use needle_core::{
    ContinuationEnvelope, Digest, FallbackEnvelope, NeedKey, PromptProfile, SemanticInterrupt,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

const STATE_FILE_NAME: &str = "needle-loop-state.json";
const STATE_LOCK_FILE_NAME: &str = ".needle-loop-state.lock";
const TELEMETRY_FILE_NAME: &str = "telemetry.jsonl";
const TELEMETRY_DIRECTORY_NAME: &str = "telemetry";
const MAX_STATE_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExperimentArm {
    P0,
    P1,
    P2,
    P3,
    P4,
}

impl ExperimentArm {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "P0" => Some(Self::P0),
            "P1" => Some(Self::P1),
            "P2" => Some(Self::P2),
            "P3" => Some(Self::P3),
            "P4" => Some(Self::P4),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::P0 => "P0",
            Self::P1 => "P1",
            Self::P2 => "P2",
            Self::P3 => "P3",
            Self::P4 => "P4",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SessionStartInput {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub source: Option<String>,
    pub reason: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct UserPromptSubmitInput {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub prompt: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SessionEndInput {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub reason: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct StopInput {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    #[serde(default)]
    pub stop_hook_active: bool,
    pub last_assistant_message: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct CompactInput {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub cwd: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionStartSpecificOutput {
    #[serde(rename = "hookEventName")]
    pub hook_event_name: &'static str,
    #[serde(rename = "additionalContext")]
    pub additional_context: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionStartOutput {
    #[serde(rename = "hookSpecificOutput")]
    pub hook_specific_output: SessionStartSpecificOutput,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StopOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl StopOutput {
    pub fn noop() -> Self {
        Self::default()
    }

    pub fn block(reason: String) -> Self {
        Self { decision: Some("block"), reason: Some(reason) }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CompactOutput {}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EmptyHookOutput {}

#[derive(Clone, Debug)]
pub struct HookConfig {
    pub route_keys: Vec<String>,
    pub protocol_text: Option<String>,
    pub plugin_data: Option<PathBuf>,
    pub experiment_arm: Option<ExperimentArm>,
    pub benchmark_payload: Option<String>,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            route_keys: vec![
                "locate.implementation".to_owned(),
                "trace.state-flow".to_owned(),
                "tests.relevant".to_owned(),
            ],
            protocol_text: None,
            plugin_data: env::var_os("PLUGIN_DATA").map(PathBuf::from),
            experiment_arm: None,
            benchmark_payload: None,
        }
    }
}

impl HookConfig {
    pub fn from_environment() -> Self {
        let mut config = Self::default();
        if let Some(keys) = env::var_os("NEEDLE_ROUTE_KEYS") {
            config.route_keys = keys
                .to_string_lossy()
                .split(',')
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(ToOwned::to_owned)
                .collect();
        }
        if let Some(protocol) = env::var_os("NEEDLE_PROTOCOL_TEXT") {
            config.protocol_text = Some(protocol.to_string_lossy().into_owned());
        }
        if env::var("NEEDLE_LIVE_HARNESS").as_deref() == Ok("1")
            && let Some(arm) = env::var_os("NEEDLE_EXPERIMENT_ARM")
        {
            config.experiment_arm = ExperimentArm::parse(&arm.to_string_lossy());
        }
        if let Some(path) = env::var_os("NEEDLE_BENCHMARK_PAYLOAD_FILE") {
            config.benchmark_payload = fs::read_to_string(path).ok();
        }
        config
    }

    pub fn profile(&self) -> Result<PromptProfile, HookError> {
        if let Some(protocol) = self.protocol_text.as_deref() {
            return PromptProfile::from_strings(self.route_keys.iter().cloned(), protocol)
                .map_err(HookError::InvalidConfiguration);
        }
        let route_keys =
            self.route_keys.iter().cloned().map(NeedKey::new).collect::<Result<Vec<_>, _>>()?;
        Ok(PromptProfile::default_profile(route_keys))
    }
}

#[derive(Debug, Error)]
pub enum HookError {
    #[error("invalid hook configuration: {0}")]
    InvalidConfiguration(#[from] needle_core::NeedKeyError),
    #[error("state I/O failed: {0}")]
    StateIo(#[from] io::Error),
    #[error("state JSON is invalid: {0}")]
    StateJson(#[from] serde_json::Error),
    #[error("plugin data lock timed out")]
    LockTimeout,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct LoopState {
    #[serde(default)]
    handled: BTreeSet<String>,
    #[serde(default)]
    blocked_turns: BTreeSet<String>,
}

pub fn handle_session_start(
    input: &SessionStartInput,
    config: &HookConfig,
) -> Result<SessionStartOutput, HookError> {
    let profile = config.profile()?;
    let output = SessionStartOutput {
        hook_specific_output: SessionStartSpecificOutput {
            hook_event_name: "SessionStart",
            additional_context: profile.rendered_context_owned(),
        },
    };
    let fields = json!({
        "profile_digest": profile.definition_digest.to_string(),
        "profile_payload_bytes": profile.canonical_bytes().len(),
        "source": input.source.as_deref(),
        "reason": input.reason.as_deref(),
    });
    record_hook_telemetry(config, "SessionStart", input.session_id.as_deref(), None, fields);
    Ok(output)
}

pub fn handle_stop(input: &StopInput, config: &HookConfig) -> Result<StopOutput, HookError> {
    let output = handle_stop_inner(input, config)?;
    record_hook_telemetry(
        config,
        "Stop",
        input.session_id.as_deref(),
        input.turn_id.as_deref(),
        json!({
            "decision": output.decision,
            "arm": config.experiment_arm.map(ExperimentArm::as_str),
        }),
    );
    Ok(output)
}

pub fn handle_stop_with_resolver<F>(
    input: &StopInput,
    config: &HookConfig,
    resolver: F,
) -> Result<StopOutput, HookError>
where
    F: FnOnce(&SemanticInterrupt) -> Result<Option<String>, String>,
{
    let output = handle_product_stop_inner(input, config, resolver)?;
    record_hook_telemetry(
        config,
        "Stop",
        input.session_id.as_deref(),
        input.turn_id.as_deref(),
        json!({
            "decision": output.decision,
            "mode": "product",
        }),
    );
    Ok(output)
}

fn handle_product_stop_inner<F>(
    input: &StopInput,
    config: &HookConfig,
    resolver: F,
) -> Result<StopOutput, HookError>
where
    F: FnOnce(&SemanticInterrupt) -> Result<Option<String>, String>,
{
    if input.stop_hook_active {
        return Ok(StopOutput::noop());
    }
    let Some(message) = input.last_assistant_message.as_deref() else {
        return Ok(StopOutput::noop());
    };
    let request = match SemanticInterrupt::parse(message) {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(StopOutput::noop()),
        Err(error) => {
            eprintln!("needle: malformed @@need marker: {error}");
            return Ok(StopOutput::noop());
        }
    };
    if !config.route_keys.iter().any(|key| key == request.key().as_str()) {
        return Ok(StopOutput::noop());
    }
    let (Some(session_id), Some(turn_id), Some(plugin_data)) =
        (input.session_id.as_deref(), input.turn_id.as_deref(), config.plugin_data.as_deref())
    else {
        eprintln!("needle: product request lacks persistent session state; fail-open");
        return Ok(StopOutput::noop());
    };
    let claimed = match claim_stop(
        plugin_data,
        state_key(session_id, turn_id, &request),
        turn_key(session_id, turn_id),
    ) {
        Ok(claimed) => claimed,
        Err(error) => {
            eprintln!("needle: cannot persist loop state ({error}); fail-open");
            return Ok(StopOutput::noop());
        }
    };
    if !claimed {
        return Ok(StopOutput::noop());
    }
    match resolver(&request) {
        Ok(Some(payload)) => Ok(StopOutput::block(payload)),
        Ok(None) => Ok(StopOutput::block(FallbackEnvelope::default().render())),
        Err(error) => {
            eprintln!("needle: runtime BYPASS ({error})");
            Ok(StopOutput::block(FallbackEnvelope::default().render()))
        }
    }
}

pub fn handle_user_prompt_submit(
    input: &UserPromptSubmitInput,
    config: &HookConfig,
) -> EmptyHookOutput {
    record_hook_telemetry(
        config,
        "UserPromptSubmit",
        input.session_id.as_deref(),
        input.turn_id.as_deref(),
        json!({"prompt_bytes": input.prompt.as_deref().map(str::len)}),
    );
    EmptyHookOutput::default()
}

pub fn handle_session_end(input: &SessionEndInput, config: &HookConfig) -> EmptyHookOutput {
    record_hook_telemetry(
        config,
        "SessionEnd",
        input.session_id.as_deref(),
        None,
        json!({"reason": input.reason}),
    );
    EmptyHookOutput::default()
}

fn handle_stop_inner(input: &StopInput, config: &HookConfig) -> Result<StopOutput, HookError> {
    if input.stop_hook_active {
        eprintln!("needle: stop_hook_active=true; allowing native continuation");
        return Ok(StopOutput::noop());
    }
    let Some(message) = input.last_assistant_message.as_deref() else {
        return Ok(StopOutput::noop());
    };
    let parsed = match SemanticInterrupt::parse(message) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("needle: malformed @@need marker: {error}");
            return Ok(StopOutput::noop());
        }
    };
    let Some(request) = parsed else {
        return Ok(StopOutput::noop());
    };
    if !config.route_keys.iter().any(|key| key == request.key().as_str()) {
        eprintln!("needle: request key is not configured; allowing native discovery");
        return Ok(StopOutput::noop());
    }
    let payload = match config.experiment_arm {
        Some(ExperimentArm::P0) => {
            eprintln!("needle: P0 baseline arm; plugin continuation disabled");
            return Ok(StopOutput::noop());
        }
        Some(ExperimentArm::P1) => ContinuationEnvelope::new(
            request.key().clone(),
            "Static benchmark continuation payload.",
        )
        .render(),
        Some(ExperimentArm::P3) => {
            eprintln!("needle: P3 tool arm; Stop continuation disabled");
            return Ok(StopOutput::noop());
        }
        Some(ExperimentArm::P2) | Some(ExperimentArm::P4) => {
            config.benchmark_payload.clone().unwrap_or_else(|| {
                eprintln!("needle: benchmark payload unavailable; using fallback");
                FallbackEnvelope::default().render()
            })
        }
        None => FallbackEnvelope::default().render(),
    };
    let (Some(session_id), Some(turn_id)) = (input.session_id.as_deref(), input.turn_id.as_deref())
    else {
        eprintln!("needle: valid request has no session/turn identity; fail-open");
        return Ok(StopOutput::noop());
    };
    let state_key = state_key(session_id, turn_id, &request);
    let Some(plugin_data) = config.plugin_data.as_deref() else {
        eprintln!("needle: PLUGIN_DATA unavailable; fail-open");
        return Ok(StopOutput::noop());
    };
    let turn_key = turn_key(session_id, turn_id);
    let claimed = match claim_stop(plugin_data, state_key, turn_key) {
        Ok(claimed) => claimed,
        Err(error) => {
            eprintln!("needle: cannot persist loop state ({error}); using deterministic fallback");
            return Ok(StopOutput::block(FallbackEnvelope::default().render()));
        }
    };
    if !claimed {
        eprintln!("needle: duplicate request in logical turn; allowing native continuation");
        return Ok(StopOutput::noop());
    }
    Ok(StopOutput::block(payload))
}

pub fn handle_pre_compact(_input: &CompactInput) -> CompactOutput {
    eprintln!("needle: PreCompact observed");
    CompactOutput::default()
}

pub fn handle_post_compact(_input: &CompactInput) -> CompactOutput {
    eprintln!("needle: PostCompact observed");
    CompactOutput::default()
}

pub fn record_compact_telemetry(input: &CompactInput, event: &str, config: &HookConfig) {
    record_hook_telemetry(
        config,
        event,
        input.session_id.as_deref(),
        input.turn_id.as_deref(),
        Value::Null,
    );
}

fn state_key(session_id: &str, turn_id: &str, request: &SemanticInterrupt) -> String {
    let digest = Digest::blake3(format!("{session_id}\n{turn_id}\n{}", request.digest()));
    digest.to_string()
}

fn turn_key(session_id: &str, turn_id: &str) -> String {
    Digest::blake3(format!("{session_id}\n{turn_id}")).to_string()
}

fn state_path(plugin_data: &Path) -> PathBuf {
    plugin_data.join(STATE_FILE_NAME)
}

fn read_state(plugin_data: &Path) -> Result<LoopState, HookError> {
    fs::create_dir_all(plugin_data)?;
    let path = state_path(plugin_data);
    if !path.exists() {
        return Ok(LoopState::default());
    }
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_state(plugin_data: &Path, state: &LoopState) -> Result<(), HookError> {
    fs::create_dir_all(plugin_data)?;
    let path = state_path(plugin_data);
    let temporary = plugin_data.join(format!(".{STATE_FILE_NAME}.{}.tmp", std::process::id()));
    let backup = plugin_data.join(format!(".{STATE_FILE_NAME}.bak"));
    let bytes = serde_json::to_vec(state)?;
    fs::write(&temporary, bytes)?;
    // Windows' `rename` does not replace an existing destination.  Move the
    // old file aside, install the complete temporary file, and restore the
    // backup if installation fails.  Every path remains below PLUGIN_DATA.
    if backup.exists() {
        fs::remove_file(&backup)?;
    }
    if path.exists() {
        fs::rename(&path, &backup)?;
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        if backup.exists() && !path.exists() {
            let _ = fs::rename(&backup, &path);
        }
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    if backup.exists() {
        fs::remove_file(backup)?;
    }
    Ok(())
}

fn claim_stop(plugin_data: &Path, state_key: String, turn_key: String) -> Result<bool, HookError> {
    with_plugin_data_lock(plugin_data, || {
        let mut state = read_state(plugin_data)?;
        prune_state(&mut state);
        if state.handled.contains(&state_key) || state.blocked_turns.contains(&turn_key) {
            return Ok(false);
        }
        state.handled.insert(state_key);
        state.blocked_turns.insert(turn_key);
        prune_state(&mut state);
        write_state(plugin_data, &state)?;
        Ok(true)
    })
}

fn prune_state(state: &mut LoopState) {
    while state.handled.len() > MAX_STATE_ENTRIES {
        state.handled.pop_first();
    }
    while state.blocked_turns.len() > MAX_STATE_ENTRIES {
        state.blocked_turns.pop_first();
    }
}

fn with_plugin_data_lock<T>(
    plugin_data: &Path,
    operation: impl FnOnce() -> Result<T, HookError>,
) -> Result<T, HookError> {
    fs::create_dir_all(plugin_data)?;
    let lock_path = plugin_data.join(STATE_LOCK_FILE_NAME);
    let deadline = Instant::now() + Duration::from_secs(2);
    let lock = loop {
        match OpenOptions::new().write(true).create_new(true).open(&lock_path) {
            Ok(file) => break file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if Instant::now() >= deadline {
                    return Err(HookError::LockTimeout);
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    };
    drop(lock);
    let result = operation();
    let _ = fs::remove_file(&lock_path);
    result
}

pub fn record_hook_telemetry(
    config: &HookConfig,
    event_name: &str,
    session_id: Option<&str>,
    turn_id: Option<&str>,
    fields: Value,
) {
    let Some(plugin_data) = config.plugin_data.as_deref() else {
        return;
    };
    let mut event = Map::new();
    event.insert("format_revision".to_owned(), json!(1));
    event.insert("event".to_owned(), Value::String(event_name.to_owned()));
    event.insert("session_id".to_owned(), session_id.map_or(Value::Null, |value| json!(value)));
    event.insert("turn_id".to_owned(), turn_id.map_or(Value::Null, |value| json!(value)));
    if let Value::Object(extra) = fields {
        for (key, value) in extra {
            event.insert(key, value);
        }
    }
    let result = with_plugin_data_lock(plugin_data, || {
        let bytes = serde_json::to_vec(&Value::Object(event.clone()))?;
        let digest = Digest::blake3(&bytes);
        let directory = plugin_data.join(TELEMETRY_DIRECTORY_NAME);
        fs::create_dir_all(&directory)?;
        let content_path = directory.join(format!("{}.json", digest.to_hex()));
        if !content_path.exists() {
            fs::write(&content_path, &bytes)?;
        }
        let line = json!({"digest": digest.to_string(), "event": event});
        let mut stream = OpenOptions::new()
            .create(true)
            .append(true)
            .open(plugin_data.join(TELEMETRY_FILE_NAME))?;
        serde_json::to_writer(&mut stream, &line)?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        Ok(())
    });
    if let Err(error) = result {
        eprintln!("needle: telemetry write failed ({error})");
    }
}

pub fn state_file_name() -> &'static str {
    STATE_FILE_NAME
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_data() -> PathBuf {
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
        std::env::temp_dir().join(format!("needle-platform-test-{suffix}"))
    }

    #[test]
    fn tolerant_inputs_and_strict_outputs() {
        let start: SessionStartInput =
            serde_json::from_str(r#"{"session_id":"s","unknown":{"x":1},"model":"gpt"}"#).unwrap();
        assert_eq!(start.session_id.as_deref(), Some("s"));
        let output = handle_session_start(&start, &HookConfig::default()).unwrap();
        let value = serde_json::to_value(output).unwrap();
        assert!(value.get("hookSpecificOutput").is_some());
        assert_eq!(value["hookSpecificOutput"]["hookEventName"], "SessionStart");
    }

    #[test]
    fn stop_is_once_per_session_turn_request_and_confined_to_data() {
        let directory = temporary_data();
        let config = HookConfig { plugin_data: Some(directory.clone()), ..HookConfig::default() };
        let input = StopInput {
            session_id: Some("session".to_owned()),
            turn_id: Some("turn".to_owned()),
            last_assistant_message: Some(
                "@@need:trace.state-flow\nFind callers.\n@@end".to_owned(),
            ),
            ..StopInput::default()
        };
        assert_eq!(handle_stop(&input, &config).unwrap().decision, Some("block"));
        assert_eq!(handle_stop(&input, &config).unwrap().decision, None);
        let second = StopInput {
            turn_id: Some("turn-2".to_owned()),
            last_assistant_message: Some(
                "@@need:trace.state-flow\nFind callees.\n@@end".to_owned(),
            ),
            ..input.clone()
        };
        assert_eq!(handle_stop(&second, &config).unwrap().decision, Some("block"));
        assert!(directory.join(state_file_name()).exists());
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.join(state_file_name())).unwrap()).unwrap();
        assert_eq!(state["handled"].as_array().map(Vec::len), Some(2));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn product_stop_delivers_the_resolved_frontier_to_main_once() {
        let directory = temporary_data();
        let config = HookConfig { plugin_data: Some(directory.clone()), ..HookConfig::default() };
        let input = StopInput {
            session_id: Some("session".to_owned()),
            turn_id: Some("turn".to_owned()),
            last_assistant_message: Some(
                "@@need:trace.state-flow\nTrace the answer.\n@@end".to_owned(),
            ),
            ..StopInput::default()
        };
        let frontier =
            "[NEEDLE_CONTEXT]\nvalidated\n[/NEEDLE_CONTEXT]\n\nContinue the original task."
                .to_owned();
        let output = handle_stop_with_resolver(&input, &config, |request| {
            assert_eq!(request.key().as_str(), "trace.state-flow");
            Ok(Some(frontier.clone()))
        })
        .unwrap();
        assert_eq!(output.decision, Some("block"));
        assert_eq!(output.reason.as_deref(), Some(frontier.as_str()));

        let duplicate = handle_stop_with_resolver(&input, &config, |_| {
            panic!("duplicate logical turn must not invoke the resolver")
        })
        .unwrap();
        assert_eq!(duplicate.decision, None);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn active_stop_and_malformed_request_fail_open() {
        let active = StopInput {
            stop_hook_active: true,
            last_assistant_message: Some("@@need:key\nx\n@@end".to_owned()),
            ..StopInput::default()
        };
        assert_eq!(handle_stop(&active, &HookConfig::default()).unwrap().decision, None);
        let malformed = StopInput {
            last_assistant_message: Some("@@need:key\n@@end\nextra".to_owned()),
            ..StopInput::default()
        };
        assert_eq!(handle_stop(&malformed, &HookConfig::default()).unwrap().decision, None);
    }

    #[test]
    fn experiment_arms_have_explicit_transport_payloads() {
        let request = "@@need:trace.state-flow\nFind callers.\n@@end".to_owned();
        let base = StopInput {
            session_id: Some("arm-session".to_owned()),
            turn_id: Some("arm-turn".to_owned()),
            last_assistant_message: Some(request),
            ..StopInput::default()
        };
        let data = temporary_data();
        let p0 = HookConfig {
            plugin_data: Some(data.join("p0")),
            experiment_arm: Some(ExperimentArm::P0),
            ..HookConfig::default()
        };
        assert_eq!(handle_stop(&base, &p0).unwrap().decision, None);
        let p1 = HookConfig {
            plugin_data: Some(data.join("p1")),
            experiment_arm: Some(ExperimentArm::P1),
            ..HookConfig::default()
        };
        let p2 = HookConfig {
            plugin_data: Some(data.join("p2")),
            experiment_arm: Some(ExperimentArm::P2),
            benchmark_payload: Some("benchmark payload".to_owned()),
            ..HookConfig::default()
        };
        let p4 = HookConfig {
            plugin_data: Some(data.join("p4")),
            experiment_arm: Some(ExperimentArm::P4),
            benchmark_payload: Some("benchmark payload".to_owned()),
            ..HookConfig::default()
        };
        let static_reason = handle_stop(&base, &p1).unwrap().reason.unwrap();
        let generated_reason = handle_stop(&base, &p2).unwrap().reason.unwrap();
        let repeated_reason = handle_stop(&base, &p4).unwrap().reason.unwrap();
        assert!(static_reason.contains("Static benchmark continuation payload."));
        assert_eq!(generated_reason, repeated_reason);
        let _ = fs::remove_dir_all(data);
    }

    #[test]
    fn normal_mode_uses_fallback_and_records_content_addressed_telemetry() {
        let directory = temporary_data();
        let config = HookConfig { plugin_data: Some(directory.clone()), ..HookConfig::default() };
        let input = StopInput {
            session_id: Some("session".to_owned()),
            turn_id: Some("turn".to_owned()),
            last_assistant_message: Some(
                "@@need:trace.state-flow\nFind callers.\n@@end".to_owned(),
            ),
            ..StopInput::default()
        };
        let output = handle_stop(&input, &config).unwrap();
        assert_eq!(output.decision, Some("block"));
        assert!(output.reason.unwrap().contains("Continue using native repository discovery"));
        let telemetry = fs::read_to_string(directory.join(TELEMETRY_FILE_NAME)).unwrap();
        let line: Value = serde_json::from_str(telemetry.lines().next().unwrap()).unwrap();
        let digest = line["digest"].as_str().unwrap();
        let digest = Digest::parse(digest).unwrap();
        assert!(
            directory
                .join(TELEMETRY_DIRECTORY_NAME)
                .join(format!("{}.json", digest.to_hex()))
                .is_file()
        );
        let _ = fs::remove_dir_all(directory);
    }
}
