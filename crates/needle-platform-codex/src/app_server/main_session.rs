use super::{AppServerSession, turn_failure_diagnostic};
use needle_core::{Digest, NeedDelivery, SemanticInterrupt, WorkerConfig};
use needle_runtime::RuntimeStore;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

const MAX_INTERRUPT_BYTES: usize = 16 * 1024;
const MAX_PENDING_NEEDS: usize = 8;
const CONTINUATION_DIAGNOSTICS_FORMAT_REVISION: u16 = 1;

pub const PILOT_MAIN_REPOSITORY_INSPECTION_INSTRUCTIONS: &str = "\
Needle pilot repository-inspection protocol:
- Inspect the repository only when the user task requires it.
- Issue one bounded read-only command per tool call.
- Prefer direct `rg -n -C <count> <pattern> <repository-relative-path>` searches.
- If necessary, use one direct `Get-Content -LiteralPath <repository-relative-file> -TotalCount <count>` or `-Tail <count>` command.
- Do not compose scripts or request arbitrary line ranges. Do not use variables, loops, pipelines, redirection, command chaining, command substitution, or environment mutation.
- Do not invoke tests, builds, network tools, or file-changing commands.
- A host shell wrapper added by Codex is acceptable only when its payload is exactly one allowed command.
- If the allowed commands cannot establish the answer, answer with the bounded evidence available instead of requesting a broader command.";

pub struct MainSessionConfig<'a> {
    pub codex: &'a WorkerConfig,
    pub codex_home: &'a Path,
    pub instructions: &'a str,
    pub checkout_root: &'a Path,
    pub target_root: &'a Path,
    pub temp_root: &'a Path,
    pub snapshot_digest: Digest,
    pub repository_id: Digest,
    pub route: &'a str,
    pub store: RuntimeStore,
}

pub struct CodexMainSession {
    inner: AppServerSession,
    provider_turns_started: u32,
    last_need_diagnostics: Option<MainNeedDiagnostics>,
    last_continuation_diagnostics: Option<MainContinuationDiagnostics>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct MainUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct MainNeedDiagnostics {
    pub raw_message: String,
    pub parse_error: Option<String>,
    pub usage: MainUsage,
    pub interrupt_requested: bool,
    pub interrupt_acknowledged: bool,
    pub terminal_status: Option<String>,
    pub turn_error: Option<String>,
    pub tool_items_started: u32,
    pub duration_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MainNeedRelation {
    IdenticalMessage,
    SameSemanticNeed,
    SameSubjectDifferentObligations,
    DifferentNeed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MainContinuationDiagnostics {
    pub format_revision: u16,
    pub raw_message: String,
    pub raw_message_digest: Option<Digest>,
    pub parse_error: Option<String>,
    pub semantic_interrupt_digest: Option<Digest>,
    pub relation_to_original: Option<MainNeedRelation>,
    pub usage: MainUsage,
    pub terminal_status: Option<String>,
    pub turn_error: Option<String>,
    pub violation: Option<String>,
    pub tool_items_started: u32,
    pub duration_ms: u64,
}

#[derive(Debug)]
pub struct MainNeedTurn {
    pub semantic_interrupt: SemanticInterrupt,
    pub raw_message: String,
    pub thread_id: String,
    pub turn_id: String,
    pub usage: MainUsage,
    pub duration: Duration,
    pub interrupt_acknowledged: bool,
    pub terminal_status: String,
    pub tool_items_started: u32,
    pub main_discovery_tainted: bool,
    pub active_turn: bool,
}

#[derive(Debug)]
pub struct MainFinalTurn {
    pub response: String,
    pub turn_id: String,
    pub usage: MainUsage,
    pub duration: Duration,
    pub tool_items_started: u32,
}

#[derive(Debug)]
pub struct MainDirectFailure {
    pub diagnostic: String,
    pub usage: MainUsage,
    pub duration: Duration,
    pub tool_items_started: u32,
}

impl std::fmt::Display for MainDirectFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.diagnostic)
    }
}

impl std::error::Error for MainDirectFailure {}

#[derive(Debug)]
pub enum MainTurnResult {
    Need(Box<MainNeedTurn>),
    Final(MainFinalTurn),
}

#[derive(Debug)]
pub struct ContinueWorkingResult<T> {
    pub resolved: T,
    pub delivery: NeedDelivery,
    pub outcome: Option<MainTurnResult>,
    pub usage: MainUsage,
    pub tool_items_started: u32,
    pub main_discovery_tainted: bool,
    pub queued_needs: Vec<Box<MainNeedTurn>>,
    pub queue_overflowed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ActiveTurnInterruption {
    pub usage: MainUsage,
    pub tool_items_started: u32,
    pub main_discovery_tainted: bool,
}

impl CodexMainSession {
    pub fn start(config: MainSessionConfig<'_>) -> Result<Self, String> {
        let inner = AppServerSession::start(
            config.codex,
            Some(config.codex_home),
            false,
            config.instructions,
            config.checkout_root,
            config.target_root,
            config.temp_root,
            config.snapshot_digest,
            config.repository_id,
            config.route,
            None,
            true,
            config.store,
        )?;
        Ok(Self {
            inner,
            provider_turns_started: 0,
            last_need_diagnostics: None,
            last_continuation_diagnostics: None,
        })
    }

    pub fn start_pilot(config: MainSessionConfig<'_>) -> Result<Self, String> {
        let mut session = Self::start(config)?;
        session.inner.fail_fast_on_pending_approvals();
        Ok(session)
    }

    pub fn thread_id(&self) -> &str {
        self.inner.thread_id()
    }

    pub fn provider_turns_started(&self) -> u32 {
        self.provider_turns_started
    }

    pub fn last_need_diagnostics(&self) -> Option<&MainNeedDiagnostics> {
        self.last_need_diagnostics.as_ref()
    }

    pub fn last_continuation_diagnostics(&self) -> Option<&MainContinuationDiagnostics> {
        self.last_continuation_diagnostics.as_ref()
    }

    pub fn run_until_need(
        &mut self,
        prompt: &str,
        timeout: Duration,
    ) -> Result<MainNeedTurn, String> {
        self.last_need_diagnostics = None;
        let (turn_id, started) = self.start_turn(prompt)?;
        let mut usage = MainUsage::default();
        let mut message = String::new();
        let mut marker_complete = false;
        let mut active_turn = false;
        let mut parsed_active_interrupt = None;
        let mut interrupt_request_id = None;
        let mut interrupt_acknowledged = false;
        let mut tool_items_started = 0_u32;
        let mut violation = None;
        let mut terminal_status = None;
        let mut terminal_error = None;
        let mut error_notification = None;
        let mut error_will_retry = None;

        loop {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or_else(|| "main semantic interrupt turn timed out".to_owned())?;
            let Some(event) = self.inner.recv_event(remaining.min(Duration::from_secs(1)))? else {
                continue;
            };
            if interrupt_request_id.is_some()
                && event.get("id").and_then(Value::as_u64) == interrupt_request_id
            {
                if let Some(error) = event.get("error") {
                    return Err(format!("turn/interrupt failed: {error}"));
                }
                interrupt_acknowledged = true;
                if terminal_status.is_some() {
                    break;
                }
                continue;
            }
            match event.get("method").and_then(Value::as_str) {
                Some("error")
                    if event.pointer("/params/turnId").and_then(Value::as_str)
                        == Some(turn_id.as_str()) =>
                {
                    error_notification = event.pointer("/params/error").cloned();
                    error_will_retry = event.pointer("/params/willRetry").and_then(Value::as_bool);
                }
                Some("item/agentMessage/delta") => {
                    let delta = event
                        .pointer("/params/delta")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "agent message delta omitted text".to_owned())?;
                    append_bounded(&mut message, delta)?;
                    if !marker_complete && has_end_marker(&message) {
                        marker_complete = true;
                        if let Ok(parsed) = parse_interrupt(&message)
                            && parsed.coordination()
                                == needle_core::NeedCoordination::ContinueWorking
                        {
                            active_turn = true;
                            parsed_active_interrupt = Some(parsed);
                            break;
                        }
                        interrupt_request_id = Some(self.request_interrupt(&turn_id)?);
                    }
                }
                Some("item/completed") => {
                    let Some(item) = event.pointer("/params/item") else {
                        continue;
                    };
                    if item.get("type").and_then(Value::as_str) == Some("agentMessage")
                        && !marker_complete
                        && let Some(text) = item.get("text").and_then(Value::as_str)
                        && has_end_marker(text)
                    {
                        message.clear();
                        append_bounded(&mut message, text)?;
                        marker_complete = true;
                        if let Ok(parsed) = parse_interrupt(&message)
                            && parsed.coordination()
                                == needle_core::NeedCoordination::ContinueWorking
                        {
                            active_turn = true;
                            parsed_active_interrupt = Some(parsed);
                            break;
                        }
                        interrupt_request_id = Some(self.request_interrupt(&turn_id)?);
                    }
                }
                Some("item/started") => {
                    if event.pointer("/params/item").is_some_and(is_tool_item) {
                        tool_items_started = tool_items_started.saturating_add(1);
                        violation.get_or_insert_with(|| {
                            "main started a tool before semantic interruption completed".to_owned()
                        });
                        if interrupt_request_id.is_none() {
                            interrupt_request_id = Some(self.request_interrupt(&turn_id)?);
                        }
                    }
                }
                Some("item/commandExecution/requestApproval") => {
                    self.inner.respond(&event, json!({"decision": "decline"}))?;
                    tool_items_started = tool_items_started.saturating_add(1);
                    violation.get_or_insert_with(|| {
                        "main requested command approval during semantic interruption".to_owned()
                    });
                    if interrupt_request_id.is_none() {
                        interrupt_request_id = Some(self.request_interrupt(&turn_id)?);
                    }
                }
                Some("item/fileChange/requestApproval") => {
                    self.inner.respond(&event, json!({"decision": "decline"}))?;
                    tool_items_started = tool_items_started.saturating_add(1);
                    violation.get_or_insert_with(|| {
                        "main requested a file change during semantic interruption".to_owned()
                    });
                    if interrupt_request_id.is_none() {
                        interrupt_request_id = Some(self.request_interrupt(&turn_id)?);
                    }
                }
                Some("thread/tokenUsage/updated") => usage.absorb(&event),
                Some("turn/completed")
                    if event.pointer("/params/turn/id").and_then(Value::as_str)
                        == Some(turn_id.as_str()) =>
                {
                    let status = event
                        .pointer("/params/turn/status")
                        .and_then(Value::as_str)
                        .unwrap_or("failed");
                    terminal_status = Some(status.to_owned());
                    if status != "interrupted" && status != "completed" {
                        terminal_error = Some(turn_failure_diagnostic(
                            status,
                            event.pointer("/params/turn/error"),
                            error_notification.as_ref(),
                            error_will_retry,
                        ));
                    }
                    if interrupt_request_id.is_none() || interrupt_acknowledged {
                        break;
                    }
                }
                _ => {}
            }
        }

        let parsed_interrupt = parsed_active_interrupt
            .map(Ok)
            .or_else(|| marker_complete.then(|| parse_interrupt(&message)))
            .transpose();
        self.last_need_diagnostics = Some(MainNeedDiagnostics {
            raw_message: message.clone(),
            parse_error: parsed_interrupt.as_ref().err().cloned(),
            usage,
            interrupt_requested: interrupt_request_id.is_some(),
            interrupt_acknowledged,
            terminal_status: terminal_status
                .clone()
                .or_else(|| active_turn.then(|| "active".to_owned())),
            turn_error: terminal_error.clone(),
            tool_items_started,
            duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        });
        if let Some(violation) = violation {
            return Err(violation);
        }
        let semantic_interrupt = parsed_interrupt?
            .ok_or_else(|| "main completed without a semantic interrupt".to_owned())?;
        let terminal_status = terminal_status.unwrap_or_else(|| "active".to_owned());
        if terminal_status != "active"
            && terminal_status != "interrupted"
            && terminal_status != "completed"
        {
            return Err(terminal_error.unwrap_or_else(|| {
                format!("main semantic interrupt turn ended with status {terminal_status}")
            }));
        }
        if interrupt_request_id.is_some() && !interrupt_acknowledged {
            return Err("turn/interrupt was not acknowledged".to_owned());
        }
        Ok(MainNeedTurn {
            semantic_interrupt,
            raw_message: message,
            thread_id: self.inner.thread_id.clone(),
            turn_id,
            usage,
            duration: started.elapsed(),
            interrupt_acknowledged,
            terminal_status,
            tool_items_started,
            main_discovery_tainted: tool_items_started > 0,
            active_turn,
        })
    }

    pub fn run_direct(
        &mut self,
        prompt: &str,
        timeout: Duration,
    ) -> Result<MainFinalTurn, MainDirectFailure> {
        let (turn_id, started) =
            self.start_turn(prompt).map_err(|diagnostic| MainDirectFailure {
                diagnostic,
                usage: MainUsage::default(),
                duration: Duration::ZERO,
                tool_items_started: 0,
            })?;
        let mut usage = MainUsage::default();
        let mut final_message = None;
        let mut tool_items = BTreeSet::new();
        let terminal_status: String;
        let mut terminal_error = None;
        let mut error_notification = None;
        let mut error_will_retry = None;

        loop {
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return Err(main_direct_failure(
                    "main-only turn timed out".to_owned(),
                    usage,
                    &started,
                    tool_items.len(),
                ));
            };
            let event = self.inner.recv_event(remaining.min(Duration::from_secs(1))).map_err(
                |diagnostic| main_direct_failure(diagnostic, usage, &started, tool_items.len()),
            )?;
            let Some(event) = event else {
                continue;
            };
            match event.get("method").and_then(Value::as_str) {
                Some("error")
                    if event.pointer("/params/turnId").and_then(Value::as_str)
                        == Some(turn_id.as_str()) =>
                {
                    error_notification = event.pointer("/params/error").cloned();
                    error_will_retry = event.pointer("/params/willRetry").and_then(Value::as_bool);
                }
                Some("item/started") => {
                    if let Some(item) = event.pointer("/params/item")
                        && is_tool_item(item)
                    {
                        tool_items.insert(
                            item.get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown-tool")
                                .to_owned(),
                        );
                    }
                }
                Some("item/commandExecution/requestApproval") => {
                    if let Some(item_id) = event.pointer("/params/itemId").and_then(Value::as_str) {
                        tool_items.insert(item_id.to_owned());
                    }
                    self.inner.handle_command_approval(&event).map_err(|diagnostic| {
                        main_direct_failure(diagnostic, usage, &started, tool_items.len())
                    })?;
                }
                Some("item/fileChange/requestApproval") => {
                    if let Some(item_id) = event.pointer("/params/itemId").and_then(Value::as_str) {
                        tool_items.insert(item_id.to_owned());
                    }
                    self.inner.respond(&event, json!({"decision": "decline"})).map_err(
                        |diagnostic| {
                            main_direct_failure(diagnostic, usage, &started, tool_items.len())
                        },
                    )?;
                    return Err(main_direct_failure(
                        "pilot main requested a file change".to_owned(),
                        usage,
                        &started,
                        tool_items.len(),
                    ));
                }
                Some("item/completed") => {
                    let Some(item) = event.pointer("/params/item") else {
                        continue;
                    };
                    match item.get("type").and_then(Value::as_str) {
                        Some("agentMessage") => {
                            if let Some(text) = item.get("text").and_then(Value::as_str) {
                                final_message = Some(text.to_owned());
                            }
                        }
                        Some("commandExecution") => {
                            self.inner.capture_command_trace(item);
                        }
                        _ => {}
                    }
                }
                Some("thread/tokenUsage/updated") => usage.absorb(&event),
                Some("turn/completed")
                    if event.pointer("/params/turn/id").and_then(Value::as_str)
                        == Some(turn_id.as_str()) =>
                {
                    let status = event
                        .pointer("/params/turn/status")
                        .and_then(Value::as_str)
                        .unwrap_or("failed")
                        .to_owned();
                    if status != "completed" {
                        terminal_error = Some(turn_failure_diagnostic(
                            &status,
                            event.pointer("/params/turn/error"),
                            error_notification.as_ref(),
                            error_will_retry,
                        ));
                    }
                    terminal_status = status;
                    break;
                }
                _ => {}
            }
        }

        if terminal_status != "completed" {
            return Err(main_direct_failure(
                terminal_error.unwrap_or_else(|| {
                    format!("main-only turn ended with status {terminal_status}")
                }),
                usage,
                &started,
                tool_items.len(),
            ));
        }
        let response = final_message.ok_or_else(|| {
            main_direct_failure(
                "main-only turn returned no final response".to_owned(),
                usage,
                &started,
                tool_items.len(),
            )
        })?;
        Ok(MainFinalTurn {
            response,
            turn_id,
            usage,
            duration: started.elapsed(),
            tool_items_started: tool_items.len().try_into().unwrap_or(u32::MAX),
        })
    }

    pub fn run_continuation(
        &mut self,
        continuation: &str,
        timeout: Duration,
    ) -> Result<MainFinalTurn, String> {
        self.last_continuation_diagnostics = None;
        let original_message =
            self.last_need_diagnostics.as_ref().map(|diagnostics| diagnostics.raw_message.clone());
        let (turn_id, started) = self.start_turn(continuation)?;
        let mut usage = MainUsage::default();
        let mut final_message = None;
        let mut parse_error = None;
        let mut semantic_interrupt_digest = None;
        let mut relation_to_original = None;
        let mut interrupt_request_id = None;
        let mut tool_items_started = 0_u32;
        let mut violation = None;
        let terminal_status;
        let mut terminal_error = None;
        let mut error_notification = None;
        let mut error_will_retry = None;

        loop {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or_else(|| "main continuation turn timed out".to_owned())?;
            let Some(event) = self.inner.recv_event(remaining.min(Duration::from_secs(1)))? else {
                continue;
            };
            if interrupt_request_id.is_some()
                && event.get("id").and_then(Value::as_u64) == interrupt_request_id
            {
                if let Some(error) = event.get("error") {
                    return Err(format!("turn/interrupt failed: {error}"));
                }
                continue;
            }
            match event.get("method").and_then(Value::as_str) {
                Some("error")
                    if event.pointer("/params/turnId").and_then(Value::as_str)
                        == Some(turn_id.as_str()) =>
                {
                    error_notification = event.pointer("/params/error").cloned();
                    error_will_retry = event.pointer("/params/willRetry").and_then(Value::as_bool);
                }
                Some("item/started") => {
                    if event.pointer("/params/item").is_some_and(is_tool_item) {
                        tool_items_started = tool_items_started.saturating_add(1);
                        violation.get_or_insert_with(|| {
                            "main repeated discovery after Needle continuation".to_owned()
                        });
                        if interrupt_request_id.is_none() {
                            interrupt_request_id = Some(self.request_interrupt(&turn_id)?);
                        }
                    }
                }
                Some("item/commandExecution/requestApproval") => {
                    self.inner.respond(&event, json!({"decision": "decline"}))?;
                    tool_items_started = tool_items_started.saturating_add(1);
                    violation.get_or_insert_with(|| {
                        "main requested command approval after Needle continuation".to_owned()
                    });
                    if interrupt_request_id.is_none() {
                        interrupt_request_id = Some(self.request_interrupt(&turn_id)?);
                    }
                }
                Some("item/fileChange/requestApproval") => {
                    self.inner.respond(&event, json!({"decision": "decline"}))?;
                    tool_items_started = tool_items_started.saturating_add(1);
                    violation.get_or_insert_with(|| {
                        "main requested a file change after Needle continuation".to_owned()
                    });
                    if interrupt_request_id.is_none() {
                        interrupt_request_id = Some(self.request_interrupt(&turn_id)?);
                    }
                }
                Some("item/completed") => {
                    if let Some(item) = event.pointer("/params/item")
                        && item.get("type").and_then(Value::as_str) == Some("agentMessage")
                        && let Some(text) = item.get("text").and_then(Value::as_str)
                    {
                        if has_end_marker(text) {
                            match SemanticInterrupt::parse(text) {
                                Ok(Some(nested)) => {
                                    semantic_interrupt_digest = Some(nested.digest());
                                    relation_to_original =
                                        original_message.as_deref().and_then(|original_message| {
                                            parse_interrupt(original_message).ok().map(|original| {
                                                classify_need_relation(
                                                    original,
                                                    original_message,
                                                    &nested,
                                                    text,
                                                )
                                            })
                                        });
                                    violation.get_or_insert_with(|| {
                                        "main emitted a nested semantic interrupt".to_owned()
                                    });
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    parse_error = Some(error.to_string());
                                    violation.get_or_insert_with(|| {
                                        "main emitted a malformed nested semantic interrupt"
                                            .to_owned()
                                    });
                                }
                            }
                        }
                        final_message = Some(text.to_owned());
                    }
                }
                Some("thread/tokenUsage/updated") => usage.absorb(&event),
                Some("turn/completed")
                    if event.pointer("/params/turn/id").and_then(Value::as_str)
                        == Some(turn_id.as_str()) =>
                {
                    terminal_status = event
                        .pointer("/params/turn/status")
                        .and_then(Value::as_str)
                        .unwrap_or("failed")
                        .to_owned();
                    if terminal_status != "completed" {
                        terminal_error = Some(turn_failure_diagnostic(
                            &terminal_status,
                            event.pointer("/params/turn/error"),
                            error_notification.as_ref(),
                            error_will_retry,
                        ));
                    }
                    break;
                }
                _ => {}
            }
        }

        if violation.is_some() || terminal_status != "completed" {
            self.last_continuation_diagnostics = Some(MainContinuationDiagnostics {
                format_revision: CONTINUATION_DIAGNOSTICS_FORMAT_REVISION,
                raw_message: final_message
                    .as_deref()
                    .map(bound_diagnostic_text)
                    .unwrap_or_default(),
                raw_message_digest: final_message.as_deref().map(Digest::blake3),
                parse_error,
                semantic_interrupt_digest,
                relation_to_original,
                usage,
                terminal_status: Some(terminal_status.clone()),
                turn_error: terminal_error.clone(),
                violation: violation.clone(),
                tool_items_started,
                duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            });
        }
        if let Some(violation) = violation {
            return Err(violation);
        }
        if terminal_status != "completed" {
            return Err(terminal_error.unwrap_or_else(|| {
                format!("main continuation turn ended with status {terminal_status}")
            }));
        }
        Ok(MainFinalTurn {
            response: final_message
                .ok_or_else(|| "main continuation returned no final message".to_owned())?,
            turn_id,
            usage,
            duration: started.elapsed(),
            tool_items_started,
        })
    }

    pub fn run_next(
        &mut self,
        input: &str,
        timeout: Duration,
        allow_main_tools: bool,
    ) -> Result<MainTurnResult, String> {
        let (turn_id, started) = self.start_turn(input)?;
        let mut usage = MainUsage::default();
        let mut message = String::new();
        let mut final_message = None;
        let mut marker_complete = false;
        let mut active_turn = false;
        let mut parsed_active_interrupt = None;
        let mut interrupt_request_id = None;
        let mut interrupt_acknowledged = false;
        let mut tool_items_started = 0_u32;
        let mut violation = None;
        let mut terminal_status = None;
        let mut terminal_error = None;
        let mut error_notification = None;
        let mut error_will_retry = None;

        loop {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or_else(|| "main turn timed out".to_owned())?;
            let Some(event) = self.inner.recv_event(remaining.min(Duration::from_secs(1)))? else {
                continue;
            };
            if interrupt_request_id.is_some()
                && event.get("id").and_then(Value::as_u64) == interrupt_request_id
            {
                if let Some(error) = event.get("error") {
                    return Err(format!("turn/interrupt failed: {error}"));
                }
                interrupt_acknowledged = true;
                if terminal_status.is_some() {
                    break;
                }
                continue;
            }
            match event.get("method").and_then(Value::as_str) {
                Some("error")
                    if event.pointer("/params/turnId").and_then(Value::as_str)
                        == Some(turn_id.as_str()) =>
                {
                    error_notification = event.pointer("/params/error").cloned();
                    error_will_retry = event.pointer("/params/willRetry").and_then(Value::as_bool);
                }
                Some("item/agentMessage/delta") => {
                    let delta = event
                        .pointer("/params/delta")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "agent message delta omitted text".to_owned())?;
                    append_bounded(&mut message, delta)?;
                    if !marker_complete && has_end_marker(&message) {
                        marker_complete = true;
                        if let Ok(parsed) = parse_interrupt(&message)
                            && parsed.coordination()
                                == needle_core::NeedCoordination::ContinueWorking
                        {
                            active_turn = true;
                            parsed_active_interrupt = Some(parsed);
                            break;
                        }
                        interrupt_request_id = Some(self.request_interrupt(&turn_id)?);
                    }
                }
                Some("item/completed") => {
                    let Some(item) = event.pointer("/params/item") else {
                        continue;
                    };
                    if item.get("type").and_then(Value::as_str) == Some("agentMessage")
                        && let Some(text) = item.get("text").and_then(Value::as_str)
                    {
                        if has_end_marker(text) {
                            if message.is_empty() {
                                append_bounded(&mut message, text)?;
                            }
                            marker_complete = true;
                            if let Ok(parsed) = parse_interrupt(&message)
                                && parsed.coordination()
                                    == needle_core::NeedCoordination::ContinueWorking
                            {
                                active_turn = true;
                                parsed_active_interrupt = Some(parsed);
                                break;
                            }
                        } else {
                            final_message = Some(text.to_owned());
                        }
                    }
                }
                Some("item/started") if event.pointer("/params/item").is_some_and(is_tool_item) => {
                    tool_items_started = tool_items_started.saturating_add(1);
                    if !allow_main_tools {
                        violation.get_or_insert_with(|| {
                            "main repeated discovery while waiting for Needle context".to_owned()
                        });
                    }
                    if !allow_main_tools && interrupt_request_id.is_none() {
                        interrupt_request_id = Some(self.request_interrupt(&turn_id)?);
                    }
                }
                Some("item/commandExecution/requestApproval") => {
                    self.inner.respond(&event, json!({"decision": "decline"}))?;
                    tool_items_started = tool_items_started.saturating_add(1);
                    if !allow_main_tools {
                        violation.get_or_insert_with(|| {
                            "main requested command approval while waiting for Needle context"
                                .to_owned()
                        });
                    }
                    if !allow_main_tools && interrupt_request_id.is_none() {
                        interrupt_request_id = Some(self.request_interrupt(&turn_id)?);
                    }
                }
                Some("item/fileChange/requestApproval") => {
                    self.inner.respond(&event, json!({"decision": "decline"}))?;
                    tool_items_started = tool_items_started.saturating_add(1);
                    if !allow_main_tools {
                        violation.get_or_insert_with(|| {
                            "main requested a file change while waiting for Needle context"
                                .to_owned()
                        });
                    }
                    if !allow_main_tools && interrupt_request_id.is_none() {
                        interrupt_request_id = Some(self.request_interrupt(&turn_id)?);
                    }
                }
                Some("thread/tokenUsage/updated") => usage.absorb(&event),
                Some("turn/completed")
                    if event.pointer("/params/turn/id").and_then(Value::as_str)
                        == Some(turn_id.as_str()) =>
                {
                    let status = event
                        .pointer("/params/turn/status")
                        .and_then(Value::as_str)
                        .unwrap_or("failed");
                    terminal_status = Some(status.to_owned());
                    if status != "interrupted" && status != "completed" {
                        terminal_error = Some(turn_failure_diagnostic(
                            status,
                            event.pointer("/params/turn/error"),
                            error_notification.as_ref(),
                            error_will_retry,
                        ));
                    }
                    if interrupt_request_id.is_none() || interrupt_acknowledged {
                        break;
                    }
                }
                _ => {}
            }
        }

        if let Some(violation) = violation {
            return Err(violation);
        }
        let terminal_status = terminal_status.unwrap_or_else(|| "active".to_owned());
        if terminal_status != "active"
            && terminal_status != "interrupted"
            && terminal_status != "completed"
        {
            return Err(terminal_error
                .unwrap_or_else(|| format!("main turn ended with status {terminal_status}")));
        }
        if marker_complete {
            if interrupt_request_id.is_some() && !interrupt_acknowledged {
                return Err("turn/interrupt was not acknowledged".to_owned());
            }
            return Ok(MainTurnResult::Need(Box::new(MainNeedTurn {
                semantic_interrupt: parsed_active_interrupt.unwrap_or(parse_interrupt(&message)?),
                raw_message: message,
                thread_id: self.inner.thread_id.clone(),
                turn_id,
                usage,
                duration: started.elapsed(),
                interrupt_acknowledged,
                terminal_status,
                tool_items_started,
                main_discovery_tainted: tool_items_started > 0,
                active_turn,
            })));
        }
        Ok(MainTurnResult::Final(MainFinalTurn {
            response: final_message
                .ok_or_else(|| "main turn returned no final message".to_owned())?,
            turn_id,
            usage,
            duration: started.elapsed(),
            tool_items_started,
        }))
    }

    pub fn await_resolution_and_steer<T: Send>(
        &mut self,
        turn_id: &str,
        resolution: &Receiver<Result<T, String>>,
        render: impl Fn(&T, usize) -> String,
        timeout: Duration,
    ) -> Result<ContinueWorkingResult<T>, String> {
        self.await_resolution_and_steer_cancellable(
            turn_id,
            resolution,
            render,
            |_, _, _| {},
            timeout,
        )
    }

    pub fn await_resolution_and_steer_cancellable<T: Send>(
        &mut self,
        turn_id: &str,
        resolution: &Receiver<Result<T, String>>,
        render: impl Fn(&T, usize) -> String,
        on_task_cancel: impl Fn(MainUsage, u32, bool),
        timeout: Duration,
    ) -> Result<ContinueWorkingResult<T>, String> {
        let started = Instant::now();
        let mut usage = MainUsage::default();
        let mut tool_items_started = 0_u32;
        let mut main_discovery_tainted = false;
        let mut turn_completed = false;
        let mut queued_needs = Vec::new();
        let mut queue_overflowed = false;

        let resolved = loop {
            match resolution.try_recv() {
                Ok(Ok(value)) => break value,
                Ok(Err(error)) => {
                    if !turn_completed {
                        let interrupted = self.interrupt_active_turn(
                            turn_id,
                            timeout.saturating_sub(started.elapsed()).max(Duration::from_secs(1)),
                        )?;
                        usage.merge_snapshot(interrupted.usage);
                        tool_items_started =
                            tool_items_started.saturating_add(interrupted.tool_items_started);
                        main_discovery_tainted |= interrupted.main_discovery_tainted;
                    }
                    on_task_cancel(usage, tool_items_started, main_discovery_tainted);
                    return Err(format!("continue-working resolver failed: {error}"));
                }
                Err(TryRecvError::Disconnected) => {
                    return Err("continue-working resolver channel closed".to_owned());
                }
                Err(TryRecvError::Empty) => {}
            }
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or_else(|| "continue-working resolution timed out".to_owned())?;
            let Some(event) = self.inner.recv_event(remaining.min(Duration::from_millis(50)))?
            else {
                continue;
            };
            if let Some(queued) = observe_pending_event(
                &mut self.inner,
                &event,
                turn_id,
                &mut usage,
                &mut tool_items_started,
                &mut main_discovery_tainted,
                &mut turn_completed,
            )? {
                if queued_needs.len() >= MAX_PENDING_NEEDS {
                    queue_overflowed = true;
                } else {
                    queued_needs.push(queued);
                }
            }
            if turn_was_cancelled(&event, turn_id) {
                on_task_cancel(usage, tool_items_started, main_discovery_tainted);
                return Err("main task cancelled while a need was resolving".to_owned());
            }
        };

        if turn_completed {
            finish_queued_needs(&mut queued_needs, false);
            return Ok(ContinueWorkingResult {
                resolved,
                delivery: NeedDelivery::TurnStart,
                outcome: None,
                usage,
                tool_items_started,
                main_discovery_tainted,
                queued_needs,
                queue_overflowed,
            });
        }

        let pending_count = queued_needs.len().saturating_add(usize::from(queue_overflowed));
        let context = render(&resolved, pending_count);
        let request_id = self.inner.send_request(
            "turn/steer",
            json!({
                "threadId": self.inner.thread_id,
                "input": [{"type": "text", "text": context}],
                "expectedTurnId": turn_id
            }),
        )?;
        loop {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or_else(|| "turn/steer timed out".to_owned())?;
            let Some(event) = self.inner.recv_event(remaining.min(Duration::from_millis(50)))?
            else {
                continue;
            };
            if event.get("id").and_then(Value::as_u64) == Some(request_id) {
                if let Some(error) = event.get("error") {
                    let diagnostic = error.to_string();
                    if diagnostic.contains("activeTurnNotSteerable") {
                        finish_queued_needs(&mut queued_needs, false);
                        return Ok(ContinueWorkingResult {
                            resolved,
                            delivery: NeedDelivery::TurnStart,
                            outcome: None,
                            usage,
                            tool_items_started,
                            main_discovery_tainted,
                            queued_needs,
                            queue_overflowed,
                        });
                    }
                    return Err(format!("turn/steer failed: {error}"));
                }
                break;
            }
            if let Some(queued) = observe_pending_event(
                &mut self.inner,
                &event,
                turn_id,
                &mut usage,
                &mut tool_items_started,
                &mut main_discovery_tainted,
                &mut turn_completed,
            )? {
                if queued_needs.len() >= MAX_PENDING_NEEDS {
                    queue_overflowed = true;
                } else {
                    queued_needs.push(queued);
                }
            }
            if turn_was_cancelled(&event, turn_id) {
                on_task_cancel(usage, tool_items_started, main_discovery_tainted);
                return Err("main task cancelled before need delivery".to_owned());
            }
            if turn_completed {
                finish_queued_needs(&mut queued_needs, false);
                return Ok(ContinueWorkingResult {
                    resolved,
                    delivery: NeedDelivery::TurnStart,
                    outcome: None,
                    usage,
                    tool_items_started,
                    main_discovery_tainted,
                    queued_needs,
                    queue_overflowed,
                });
            }
        }

        let outcome = self.consume_active_turn(
            turn_id,
            timeout.saturating_sub(started.elapsed()).max(Duration::from_secs(1)),
            &mut usage,
            &mut tool_items_started,
            &mut main_discovery_tainted,
        )?;
        let turn_active = matches!(&outcome, MainTurnResult::Need(need) if need.active_turn);
        finish_queued_needs(&mut queued_needs, turn_active);
        Ok(ContinueWorkingResult {
            resolved,
            delivery: NeedDelivery::TurnSteer,
            outcome: Some(outcome),
            usage,
            tool_items_started,
            main_discovery_tainted,
            queued_needs,
            queue_overflowed,
        })
    }

    pub fn interrupt_active_turn(
        &mut self,
        turn_id: &str,
        timeout: Duration,
    ) -> Result<ActiveTurnInterruption, String> {
        let request_id = self.request_interrupt(turn_id)?;
        let started = Instant::now();
        let mut acknowledged = false;
        let mut completed = false;
        let mut usage = MainUsage::default();
        let mut tool_items_started = 0_u32;
        let mut main_discovery_tainted = false;
        while !acknowledged || !completed {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or_else(|| "turn/interrupt active turn timed out".to_owned())?;
            let Some(event) = self.inner.recv_event(remaining.min(Duration::from_secs(1)))? else {
                continue;
            };
            if event.get("id").and_then(Value::as_u64) == Some(request_id) {
                if let Some(error) = event.get("error") {
                    return Err(format!("turn/interrupt failed: {error}"));
                }
                acknowledged = true;
                continue;
            }
            let _ = observe_pending_event(
                &mut self.inner,
                &event,
                turn_id,
                &mut usage,
                &mut tool_items_started,
                &mut main_discovery_tainted,
                &mut completed,
            )?;
        }
        Ok(ActiveTurnInterruption { usage, tool_items_started, main_discovery_tainted })
    }

    fn consume_active_turn(
        &mut self,
        turn_id: &str,
        timeout: Duration,
        usage: &mut MainUsage,
        tool_items_started: &mut u32,
        main_discovery_tainted: &mut bool,
    ) -> Result<MainTurnResult, String> {
        let started = Instant::now();
        let mut message = String::new();
        let mut final_message = None;
        let mut marker_complete = false;
        let mut parsed_interrupt = None;
        let mut interrupt_request_id = None;
        let mut interrupt_acknowledged = false;
        let mut terminal_status = None;

        loop {
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or_else(|| "steered main turn timed out".to_owned())?;
            let Some(event) = self.inner.recv_event(remaining.min(Duration::from_secs(1)))? else {
                continue;
            };
            if interrupt_request_id.is_some()
                && event.get("id").and_then(Value::as_u64) == interrupt_request_id
            {
                if let Some(error) = event.get("error") {
                    return Err(format!("turn/interrupt failed after steer: {error}"));
                }
                interrupt_acknowledged = true;
                if terminal_status.is_some() {
                    break;
                }
                continue;
            }
            match event.get("method").and_then(Value::as_str) {
                Some("item/agentMessage/delta") => {
                    let delta = event
                        .pointer("/params/delta")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "steered agent message delta omitted text".to_owned())?;
                    append_bounded(&mut message, delta)?;
                    if !marker_complete && has_end_marker(&message) {
                        marker_complete = true;
                        let interrupt = parse_interrupt(&message)?;
                        if interrupt.coordination()
                            == needle_core::NeedCoordination::ContinueWorking
                        {
                            return Ok(MainTurnResult::Need(Box::new(MainNeedTurn {
                                semantic_interrupt: interrupt,
                                raw_message: message.clone(),
                                thread_id: self.inner.thread_id.clone(),
                                turn_id: turn_id.to_owned(),
                                usage: *usage,
                                duration: started.elapsed(),
                                interrupt_acknowledged: false,
                                terminal_status: "active".to_owned(),
                                tool_items_started: *tool_items_started,
                                main_discovery_tainted: *tool_items_started > 0,
                                active_turn: true,
                            })));
                        }
                        parsed_interrupt = Some(interrupt);
                        interrupt_request_id = Some(self.request_interrupt(turn_id)?);
                    }
                }
                Some("item/completed") => {
                    if let Some(item) = event.pointer("/params/item")
                        && item.get("type").and_then(Value::as_str) == Some("agentMessage")
                        && let Some(text) = item.get("text").and_then(Value::as_str)
                    {
                        if has_end_marker(text) {
                            message.clear();
                            append_bounded(&mut message, text)?;
                            marker_complete = true;
                            let interrupt = parse_interrupt(&message)?;
                            if interrupt.coordination()
                                == needle_core::NeedCoordination::ContinueWorking
                            {
                                return Ok(MainTurnResult::Need(Box::new(MainNeedTurn {
                                    semantic_interrupt: interrupt,
                                    raw_message: message.clone(),
                                    thread_id: self.inner.thread_id.clone(),
                                    turn_id: turn_id.to_owned(),
                                    usage: *usage,
                                    duration: started.elapsed(),
                                    interrupt_acknowledged: false,
                                    terminal_status: "active".to_owned(),
                                    tool_items_started: *tool_items_started,
                                    main_discovery_tainted: *tool_items_started > 0,
                                    active_turn: true,
                                })));
                            }
                            parsed_interrupt = Some(interrupt);
                        } else {
                            final_message = Some(text.to_owned());
                        }
                    }
                }
                Some("item/started") if event.pointer("/params/item").is_some_and(is_tool_item) => {
                    *tool_items_started = tool_items_started.saturating_add(1);
                    *main_discovery_tainted = true;
                }
                Some("item/commandExecution/requestApproval") => {
                    self.inner.respond(&event, json!({"decision": "decline"}))?;
                    *tool_items_started = tool_items_started.saturating_add(1);
                    *main_discovery_tainted = true;
                }
                Some("item/fileChange/requestApproval") => {
                    self.inner.respond(&event, json!({"decision": "decline"}))?;
                    *tool_items_started = tool_items_started.saturating_add(1);
                    *main_discovery_tainted = true;
                }
                Some("thread/tokenUsage/updated") => usage.absorb(&event),
                Some("turn/completed")
                    if event.pointer("/params/turn/id").and_then(Value::as_str)
                        == Some(turn_id) =>
                {
                    terminal_status = Some(
                        event
                            .pointer("/params/turn/status")
                            .and_then(Value::as_str)
                            .unwrap_or("failed")
                            .to_owned(),
                    );
                    if interrupt_request_id.is_none() || interrupt_acknowledged {
                        break;
                    }
                }
                _ => {}
            }
        }
        let terminal_status =
            terminal_status.ok_or_else(|| "steered turn has no terminal status".to_owned())?;
        if terminal_status != "completed" && terminal_status != "interrupted" {
            return Err(format!("steered turn ended with status {terminal_status}"));
        }
        if marker_complete {
            return Ok(MainTurnResult::Need(Box::new(MainNeedTurn {
                semantic_interrupt: parsed_interrupt
                    .ok_or_else(|| "steered marker was not parsed".to_owned())?,
                raw_message: message,
                thread_id: self.inner.thread_id.clone(),
                turn_id: turn_id.to_owned(),
                usage: *usage,
                duration: started.elapsed(),
                interrupt_acknowledged,
                terminal_status,
                tool_items_started: *tool_items_started,
                main_discovery_tainted: *main_discovery_tainted,
                active_turn: false,
            })));
        }
        Ok(MainTurnResult::Final(MainFinalTurn {
            response: final_message
                .ok_or_else(|| "steered turn returned no final message".to_owned())?,
            turn_id: turn_id.to_owned(),
            usage: *usage,
            duration: started.elapsed(),
            tool_items_started: *tool_items_started,
        }))
    }

    pub fn cleanup(self) -> Result<(), String> {
        self.inner.cleanup()
    }

    fn start_turn(&mut self, input: &str) -> Result<(String, Instant), String> {
        let request_id = self.inner.send_request(
            "turn/start",
            json!({
                "threadId": self.inner.thread_id,
                "input": [{"type": "text", "text": input}],
                "cwd": self.inner.approval_context.checkout_root,
                "approvalPolicy": "on-request",
                "approvalsReviewer": "user"
            }),
        )?;
        let response = self.inner.wait_for_response(request_id, Duration::from_secs(30))?;
        let turn_id = response
            .pointer("/result/turn/id")
            .and_then(Value::as_str)
            .ok_or_else(|| "turn/start did not return a turn id".to_owned())?
            .to_owned();
        self.provider_turns_started = self.provider_turns_started.saturating_add(1);
        Ok((turn_id, Instant::now()))
    }

    fn request_interrupt(&mut self, turn_id: &str) -> Result<u64, String> {
        self.inner.send_request(
            "turn/interrupt",
            json!({
                "threadId": self.inner.thread_id,
                "turnId": turn_id
            }),
        )
    }
}

impl MainUsage {
    pub fn merge_snapshot(&mut self, snapshot: Self) {
        self.input_tokens = maximum(self.input_tokens, snapshot.input_tokens);
        self.cached_input_tokens = maximum(self.cached_input_tokens, snapshot.cached_input_tokens);
        self.output_tokens = maximum(self.output_tokens, snapshot.output_tokens);
    }

    fn absorb(&mut self, event: &Value) {
        self.input_tokens = maximum(
            self.input_tokens,
            event.pointer("/params/tokenUsage/total/inputTokens").and_then(Value::as_u64),
        );
        self.cached_input_tokens = maximum(
            self.cached_input_tokens,
            event.pointer("/params/tokenUsage/total/cachedInputTokens").and_then(Value::as_u64),
        );
        self.output_tokens = maximum(
            self.output_tokens,
            event.pointer("/params/tokenUsage/total/outputTokens").and_then(Value::as_u64),
        );
    }
}

fn main_direct_failure(
    diagnostic: String,
    usage: MainUsage,
    started: &Instant,
    tool_items_started: usize,
) -> MainDirectFailure {
    MainDirectFailure {
        diagnostic,
        usage,
        duration: started.elapsed(),
        tool_items_started: tool_items_started.try_into().unwrap_or(u32::MAX),
    }
}

fn maximum(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).max(right.unwrap_or(0))),
    }
}

fn append_bounded(message: &mut String, value: &str) -> Result<(), String> {
    if message.len().saturating_add(value.len()) > MAX_INTERRUPT_BYTES {
        return Err("main semantic interrupt exceeds 16 KiB".to_owned());
    }
    message.push_str(value);
    Ok(())
}

fn bound_diagnostic_text(value: &str) -> String {
    let mut end = value.len().min(MAX_INTERRUPT_BYTES);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn classify_need_relation(
    original: SemanticInterrupt,
    original_message: &str,
    nested: &SemanticInterrupt,
    nested_message: &str,
) -> MainNeedRelation {
    if original_message == nested_message {
        return MainNeedRelation::IdenticalMessage;
    }
    if original.digest() == nested.digest() {
        return MainNeedRelation::SameSemanticNeed;
    }
    if let (Some(original), Some(nested)) = (original.typed(), nested.typed())
        && original.route_hint == nested.route_hint
        && original.subjects == nested.subjects
        && original.world == nested.world
    {
        return MainNeedRelation::SameSubjectDifferentObligations;
    }
    MainNeedRelation::DifferentNeed
}

fn observe_pending_event(
    session: &mut AppServerSession,
    event: &Value,
    turn_id: &str,
    usage: &mut MainUsage,
    tool_items_started: &mut u32,
    main_discovery_tainted: &mut bool,
    turn_completed: &mut bool,
) -> Result<Option<Box<MainNeedTurn>>, String> {
    let mut queued_need = None;
    match event.get("method").and_then(Value::as_str) {
        Some("thread/tokenUsage/updated") => usage.absorb(event),
        Some("item/started") if event.pointer("/params/item").is_some_and(is_tool_item) => {
            *tool_items_started = tool_items_started.saturating_add(1);
            *main_discovery_tainted = true;
        }
        Some("item/commandExecution/requestApproval") => {
            session.respond(event, json!({"decision": "decline"}))?;
            *tool_items_started = tool_items_started.saturating_add(1);
            *main_discovery_tainted = true;
        }
        Some("item/fileChange/requestApproval") => {
            session.respond(event, json!({"decision": "decline"}))?;
            *tool_items_started = tool_items_started.saturating_add(1);
            *main_discovery_tainted = true;
        }
        Some("item/completed") => {
            let Some(item) = event.pointer("/params/item") else {
                return Ok(None);
            };
            if item.get("type").and_then(Value::as_str) == Some("agentMessage")
                && let Some(text) = item.get("text").and_then(Value::as_str)
                && has_end_marker(text)
            {
                let semantic_interrupt = parse_interrupt(text)?;
                queued_need = Some(Box::new(MainNeedTurn {
                    semantic_interrupt,
                    raw_message: text.to_owned(),
                    thread_id: session.thread_id.clone(),
                    turn_id: turn_id.to_owned(),
                    usage: *usage,
                    duration: Duration::ZERO,
                    interrupt_acknowledged: false,
                    terminal_status: "queued".to_owned(),
                    tool_items_started: 0,
                    main_discovery_tainted: *main_discovery_tainted,
                    active_turn: true,
                }));
            }
        }
        Some("turn/completed")
            if event.pointer("/params/turn/id").and_then(Value::as_str) == Some(turn_id) =>
        {
            *turn_completed = true;
        }
        _ => {}
    }
    Ok(queued_need)
}

fn finish_queued_needs(queued_needs: &mut [Box<MainNeedTurn>], active_turn: bool) {
    for need in queued_needs {
        need.active_turn = active_turn;
        need.terminal_status = if active_turn { "queued" } else { "completed" }.to_owned();
    }
}

fn turn_was_cancelled(event: &Value, turn_id: &str) -> bool {
    event.get("method").and_then(Value::as_str) == Some("turn/completed")
        && event.pointer("/params/turn/id").and_then(Value::as_str) == Some(turn_id)
        && matches!(
            event.pointer("/params/turn/status").and_then(Value::as_str),
            Some("cancelled" | "canceled")
        )
}

fn has_end_marker(value: &str) -> bool {
    value.lines().any(|line| line.trim_end_matches('\r') == "@@end")
}

fn parse_interrupt(value: &str) -> Result<SemanticInterrupt, String> {
    SemanticInterrupt::parse(value)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "agent message is not a semantic interrupt".to_owned())
}

fn is_tool_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some(
            "commandExecution"
                | "fileChange"
                | "mcpToolCall"
                | "dynamicToolCall"
                | "collabToolCall"
                | "webSearch"
                | "imageView"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_marker_requires_its_own_line() {
        assert!(has_end_marker("@@need\n@route x\n\nbody\n@@end"));
        assert!(!has_end_marker("@@need\nbody @@end"));
    }

    #[test]
    fn tool_items_are_fail_closed() {
        assert!(is_tool_item(&json!({"type": "commandExecution"})));
        assert!(is_tool_item(&json!({"type": "webSearch"})));
        assert!(!is_tool_item(&json!({"type": "agentMessage"})));
    }

    #[test]
    fn need_relation_distinguishes_wording_obligations_and_subjects() {
        let original = marker("answer", "", "Locate the implementation.");
        let reworded = marker("answer", "", "Find the primary code location.");
        let broader = marker(
            "answer",
            "@prefer focused-tests selection=representative\n",
            "Locate the implementation and a focused test.",
        );
        let different = marker("other", "", "Locate the implementation.");
        let parsed_original = parse_interrupt(&original).unwrap();

        assert_eq!(
            classify_need_relation(
                parsed_original.clone(),
                &original,
                &parse_interrupt(&original).unwrap(),
                &original,
            ),
            MainNeedRelation::IdenticalMessage
        );
        assert_eq!(
            classify_need_relation(
                parsed_original.clone(),
                &original,
                &parse_interrupt(&reworded).unwrap(),
                &reworded,
            ),
            MainNeedRelation::SameSemanticNeed
        );
        assert_eq!(
            classify_need_relation(
                parsed_original.clone(),
                &original,
                &parse_interrupt(&broader).unwrap(),
                &broader,
            ),
            MainNeedRelation::SameSubjectDifferentObligations
        );
        assert_eq!(
            classify_need_relation(
                parsed_original,
                &original,
                &parse_interrupt(&different).unwrap(),
                &different,
            ),
            MainNeedRelation::DifferentNeed
        );
    }

    fn marker(subject: &str, preferred: &str, body: &str) -> String {
        format!(
            "@@need\n\
             @route locate.implementation\n\
             @subject symbol:\"{subject}\"\n\
             @require implementation-location selection=primary granularity=exact-location\n\
             {preferred}\
             @world source=current features=default\n\
             \n\
             {body}\n\
             @@end"
        )
    }
}
