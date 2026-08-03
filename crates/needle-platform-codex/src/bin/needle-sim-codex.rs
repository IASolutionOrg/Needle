use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

const TEST_COMMAND: &str = "cargo test suite::focused -- --exact";
const R44_TEST_COMMAND: &str =
    "cargo test --offline --test integration misc::glob_always_case_insensitive -- --exact";
const MAIN_DIRECT_READ_COMMAND: &str = "rg -n flag_definition src/lib.rs";
const MAIN_DIRECT_R84_SCRIPT: &str = "powershell.exe -NoProfile -Command '$p = \"crates/core/flags/defs.rs\"; Get-Content -LiteralPath $p | ForEach-Object { $_ }'";
const DEFAULT_MAIN_NEED: &str = "@@need\n\
@route locate.implementation\n\
@subject symbol:\"flag_definition\"\n\
@require implementation-location selection=primary granularity=exact-location\n\
@world source=current features=default\n\
\n\
Locate the primary implementation.\n\
@@end";
const SECOND_MAIN_NEED: &str = "@@need\n\
@route tests.relevant\n\
@subject symbol:\"flag_definition\"\n\
@require focused-tests selection=representative completeness=open-world\n\
@world source=current features=default\n\
\n\
Locate the focused test still required to answer.\n\
@@end";
const R59_COVERED_MAIN_NEED: &str = "@@need\n\
@route locate.implementation\n\
@subject cli-option:\"--glob-case-insensitive\"\n\
@require implementation-location selection=primary granularity=exact-location\n\
@prefer focused-tests selection=representative completeness=open-world\n\
@world source=current features=default\n\
\n\
Use the already established implementation location and continue.\n\
@@end";

fn mcp_transport_request() -> Value {
    json!({
        "route": "trace.state-flow",
        "subject": {"kind": "cli_option", "name": "--crlf"},
        "required": [
            {"kind": "implementation_location", "polarity": "positive", "selection": "primary"},
            {"kind": "runtime_flow", "scenario": "default", "completeness": "contract_complete", "granularity": "stepwise"},
            {"kind": "focused_tests", "polarity": "positive", "selection": "representative", "completeness": "open_world"}
        ],
        "preferred": [],
        "world": {"source": "current", "platform": "current", "features": "default"},
        "task": "Trace how --crlf changes matching and search line terminators from CLI parsing through runtime configuration, and identify a focused test proving the default scenario."
    })
}

fn mcp_tests_relevant_request() -> Value {
    json!({
        "route": "tests.relevant",
        "subject": {"kind": "cli_option", "name": "--crlf"},
        "required": [
            {"kind": "focused_tests", "polarity": "positive", "selection": "representative", "completeness": "open_world"}
        ],
        "preferred": [],
        "world": {"source": "current", "platform": "current", "features": "default"},
        "task": "Identify the focused test for ripgrep's --crlf behavior using the certified context already produced."
    })
}

fn is_ripgrep_main_scenario(scenario: &str) -> bool {
    matches!(
        scenario,
        "main_interrupt_r44"
            | "main_interrupt_r59_covered_repeat"
            | "main_interrupt_r61_locate"
            | "main_interrupt_r61_trace"
    )
}

fn requested_main_scenario() -> Option<String> {
    let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from)?;
    fs::read_to_string(codex_home.join(".needle-simulation-main-scenario"))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn requested_worker_scenario() -> Option<String> {
    let codex_home = env::var_os("CODEX_HOME").map(PathBuf::from)?;
    fs::read_to_string(codex_home.join(".needle-simulation-worker-scenario"))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("needle-sim-codex: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    match arguments.first().map(String::as_str) {
        Some("--version") => {
            println!("codex-cli 0.144.0");
            Ok(())
        }
        Some("app-server") if arguments.iter().any(|argument| argument == "--help") => {
            println!("--listen <URL>\n--strict-config\n--experimental");
            Ok(())
        }
        Some("app-server")
            if arguments.get(1).map(String::as_str) == Some("generate-json-schema") =>
        {
            println!("--experimental");
            Ok(())
        }
        Some("app-server") => serve_app_server(),
        Some("exec") => serve_exec_simulator(&arguments[1..]),
        Some("delete") => Ok(()),
        _ => Err("unsupported simulator invocation".to_owned()),
    }
}

fn serve_exec_simulator(arguments: &[String]) -> Result<(), String> {
    let separator = arguments
        .iter()
        .position(|argument| argument == "--")
        .ok_or_else(|| "exec simulation requires a prompt separator".to_owned())?;
    if arguments.len() != separator + 2 {
        return Err("exec options must precede `--` and the single prompt".to_owned());
    }
    let codex_home = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "CODEX_HOME is required for exec simulation".to_owned())?;
    let scenario = fs::read_to_string(codex_home.join(".needle-simulation-exec-scenario"))
        .unwrap_or_else(|_| "mcp_success".to_owned());
    println!("{}", json!({"type":"thread.started","thread_id":"needle-sim-exec"}));
    println!("{}", json!({"type":"turn.started"}));
    match scenario.trim() {
        "mcp_success" => {
            println!(
                "{}",
                json!({"type":"item.started","item":{"id":"mcp-1","type":"mcp_tool_call","server":"needle","tool":"need_context","arguments":mcp_transport_request()}})
            );
            println!(
                "{}",
                json!({"type":"item.completed","item":{"id":"mcp-1","type":"mcp_tool_call","server":"needle","tool":"need_context","arguments":mcp_transport_request(),"result":{"content":[{"type":"text","text":"certified context"}],"structuredContent":{"context":"certified context"}},"error":null,"status":"completed"}})
            );
            println!(
                "{}",
                json!({"type":"item.completed","item":{"id":"answer-1","type":"agent_message","text":"final answer from Needle context"}})
            );
            println!(
                "{}",
                json!({"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":20,"reasoning_output_tokens":0}})
            );
        }
        "mcp_partial_tests_success" => {
            for (id, request) in
                [("mcp-1", mcp_transport_request()), ("mcp-2", mcp_tests_relevant_request())]
            {
                println!(
                    "{}",
                    json!({"type":"item.started","item":{"id":id,"type":"mcp_tool_call","server":"needle","tool":"need_context","arguments":request}})
                );
                println!(
                    "{}",
                    json!({"type":"item.completed","item":{"id":id,"type":"mcp_tool_call","server":"needle","tool":"need_context","arguments":request,"result":{"content":[{"type":"text","text":"certified context"}],"structuredContent":{"context":"certified context"}},"error":null,"status":"completed"}})
                );
            }
            println!(
                "{}",
                json!({"type":"item.completed","item":{"id":"answer-1","type":"agent_message","text":"final answer from partial and focused-test context"}})
            );
            println!(
                "{}",
                json!({"type":"turn.completed","usage":{"input_tokens":160,"cached_input_tokens":100,"output_tokens":30,"reasoning_output_tokens":0}})
            );
        }
        "discovery_before_mcp" => {
            println!(
                "{}",
                json!({"type":"item.completed","item":{"id":"message-1","type":"agent_message","text":"I will inspect the repository"}})
            );
            io::stdout().flush().map_err(|error| error.to_string())?;
            std::thread::sleep(std::time::Duration::from_secs(2));
            println!(
                "{}",
                json!({"type":"item.started","item":{"id":"command-1","type":"command_execution","command":"rg needle ."}})
            );
        }
        value => return Err(format!("unknown exec simulation scenario `{value}`")),
    }
    Ok(())
}

#[derive(Default)]
struct Simulator {
    checkout: PathBuf,
    scenario: String,
    thread_id: String,
    main_mode: bool,
    test_execution_allowed: bool,
    workspace_write: bool,
    turn: u32,
    pending_approval: Option<PendingApproval>,
    pending_main_turn: Option<String>,
}

struct PendingApproval {
    request_id: Value,
    turn_id: String,
    item_id: String,
    test_plan: bool,
    file_change: bool,
}

impl Simulator {
    fn handle(&mut self, message: Value, output: &mut impl Write) -> Result<(), String> {
        if let Some(method) = message.get("method").and_then(Value::as_str).map(str::to_owned) {
            return self.handle_method(&method, message, output);
        }
        if self
            .pending_approval
            .as_ref()
            .is_some_and(|pending| message.get("id") == Some(&pending.request_id))
        {
            if self.pending_approval.as_ref().is_some_and(|pending| pending.file_change) {
                return self.complete_approved_file_change(message, output);
            }
            return self.complete_approved_command(output);
        }
        Ok(())
    }

    fn handle_method(
        &mut self,
        method: &str,
        message: Value,
        output: &mut impl Write,
    ) -> Result<(), String> {
        let id = message.get("id").cloned();
        match method {
            "initialize" => respond(output, id, json!({})),
            "initialized" => Ok(()),
            "thread/start" => {
                self.thread_id = format!("needle-sim-thread-{}", std::process::id());
                self.checkout = message
                    .pointer("/params/cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .ok_or_else(|| "thread/start omitted cwd".to_owned())?;
                self.scenario = if message.pointer("/params/model").and_then(Value::as_str)
                    == Some("simulated-main-r35-cache")
                {
                    "main_interrupt_r35_cache".to_owned()
                } else if message.pointer("/params/model").and_then(Value::as_str)
                    == Some("gpt-5.6-sol")
                {
                    requested_main_scenario().unwrap_or_else(|| "main_interrupt_r44".to_owned())
                } else if message.pointer("/params/model").and_then(Value::as_str)
                    == Some("gpt-5.6-luna")
                {
                    if let Some(scenario) = requested_worker_scenario() {
                        if scenario == "repair_flow" {
                            let instructions = message
                                .pointer("/params/developerInstructions")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if message.pointer("/params/sandbox").and_then(Value::as_str)
                                == Some("workspace-write")
                            {
                                if instructions.contains("one-shot repair") {
                                    "patch_repair".to_owned()
                                } else {
                                    "patch_repair_initial".to_owned()
                                }
                            } else {
                                "verifier_repair_flow".to_owned()
                            }
                        } else {
                            scenario
                        }
                    } else if message
                        .pointer("/params/developerInstructions")
                        .and_then(Value::as_str)
                        .is_some_and(|instructions| {
                            instructions.contains(
                                "declared TestPlan permits but does not require execution",
                            )
                        })
                    {
                        "worker_r47_optional_test".to_owned()
                    } else {
                        "worker_r44".to_owned()
                    }
                } else {
                    fs::read_to_string(self.checkout.join(".needle-simulation-scenario"))
                        .unwrap_or_else(|_| "repair_success".to_owned())
                        .trim()
                        .to_owned()
                };
                self.main_mode = message
                    .pointer("/params/developerInstructions")
                    .and_then(Value::as_str)
                    .is_some_and(|instructions| {
                        instructions.contains("Needle semantic interrupt protocol")
                    });
                self.test_execution_allowed = message
                    .pointer("/params/developerInstructions")
                    .and_then(Value::as_str)
                    .is_some_and(|instructions| {
                        instructions.contains(
                            "The declared TestPlan permits but does not require execution",
                        )
                    });
                self.workspace_write = message.pointer("/params/sandbox").and_then(Value::as_str)
                    == Some("workspace-write");
                let developer_instructions = message
                    .pointer("/params/developerInstructions")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if ((is_ripgrep_main_scenario(&self.scenario) && !self.main_mode)
                    || self.scenario.starts_with("main_direct_"))
                    && !developer_instructions
                        .contains("Needle pilot repository-inspection protocol")
                {
                    return Err(
                        "pilot main omitted the bounded repository-inspection protocol".to_owned()
                    );
                }
                if self.scenario.starts_with("main_interrupt")
                    && message.pointer("/params/baseInstructions").is_some()
                {
                    return Err(
                        "Needle instructions were duplicated as base instructions".to_owned()
                    );
                }
                respond(output, id, json!({"thread": {"id": self.thread_id}}))
            }
            "turn/start" => self.start_turn(id, &message, output),
            "turn/steer" => self.steer_main_turn(id, message, output),
            "turn/interrupt" => self.interrupt_main_turn(id, message, output),
            "thread/delete" => respond(output, id, json!({})),
            _ => {
                if let Some(id) = id {
                    respond(output, Some(id), json!({}))
                } else {
                    Ok(())
                }
            }
        }
    }

    fn start_turn(
        &mut self,
        id: Option<Value>,
        message: &Value,
        output: &mut impl Write,
    ) -> Result<(), String> {
        self.turn = self.turn.saturating_add(1);
        let turn_id = format!("needle-sim-turn-{}", self.turn);
        respond(output, id, json!({"turn": {"id": turn_id}}))?;
        if matches!(
            self.scenario.as_str(),
            "main_direct_read_only" | "main_direct_r84_pending_approval"
        ) {
            self.emit_usage(output)?;
            let (approval_id, command) = if self.scenario == "main_direct_read_only" {
                ("main-direct-read-only", MAIN_DIRECT_READ_COMMAND)
            } else {
                ("main-direct-r84-pending", MAIN_DIRECT_R84_SCRIPT)
            };
            let item_id = format!("{approval_id}-item");
            let request_id = json!(8800 + self.turn);
            emit(
                output,
                json!({
                    "id": request_id,
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "approvalId": approval_id,
                        "threadId": self.thread_id,
                        "turnId": turn_id,
                        "itemId": item_id,
                        "command": command,
                        "cwd": self.checkout,
                        "commandActions": [{
                            "type": "unknown",
                            "command": command
                        }],
                        "additionalPermissions": {}
                    }
                }),
            )?;
            self.pending_approval = Some(PendingApproval {
                request_id,
                turn_id,
                item_id,
                test_plan: false,
                file_change: false,
            });
            return Ok(());
        }
        if is_ripgrep_main_scenario(&self.scenario) && !self.main_mode {
            if self.scenario == "main_interrupt_r61_trace" {
                return self.complete_text_turn(
                    &turn_id,
                    "The --crlf flow starts at Crlf in crates/core/flags/defs.rs. In crates/core/flags/hiargs.rs the matcher uses crlf(true) and the searcher uses LineTerminator::crlf(); feature::f416_crlf covers the default behavior.",
                    output,
                );
            }
            return self.complete_text_turn(
                &turn_id,
                "The primary implementation flows through globs in crates/core/flags/hiargs.rs; the focused test is misc::glob_always_case_insensitive in tests/misc.rs.",
                output,
            );
        }
        if self.scenario.starts_with("main_interrupt") && self.main_mode {
            if self.turn == 1 {
                let prompt = message.to_string();
                let implementation_only = is_ripgrep_main_scenario(&self.scenario)
                    && (prompt.contains(
                        "Find the primary code location responsible for the --glob-case-insensitive",
                    ) || prompt.contains(
                        "Identify the primary implementation location that handles ripgrep's --glob-case-insensitive",
                    ));
                return self.start_main_need_turn(&turn_id, output, implementation_only);
            }
            if self.scenario == "main_interrupt_nested_same" {
                return self.complete_text_turn(&turn_id, DEFAULT_MAIN_NEED, output);
            }
            if self.scenario == "main_interrupt_two_needs" && self.turn == 2 {
                return self.complete_text_turn(&turn_id, SECOND_MAIN_NEED, output);
            }
            if self.scenario == "main_interrupt_r59_covered_repeat" && self.turn == 2 {
                return self.complete_text_turn(&turn_id, R59_COVERED_MAIN_NEED, output);
            }
            let response = if self.scenario == "main_interrupt_r35_cache" {
                "The primary implementation is globs in crates/core/flags/hiargs.rs, supported by GlobCaseInsensitive::update in crates/core/flags/defs.rs."
            } else if self.scenario == "main_interrupt_r61_trace" {
                "The --crlf flow starts at Crlf in crates/core/flags/defs.rs. In crates/core/flags/hiargs.rs the matcher uses crlf(true) and the searcher uses LineTerminator::crlf(); feature::f416_crlf covers the default behavior."
            } else if is_ripgrep_main_scenario(&self.scenario) {
                "The primary implementation flows through globs in crates/core/flags/hiargs.rs; the focused test is misc::glob_always_case_insensitive in tests/misc.rs."
            } else {
                "The implementation is in src/lib.rs and the focused test is suite::focused."
            };
            return self.complete_text_turn(&turn_id, response, output);
        }
        if self.scenario == "worker_turn_failed" {
            let error = json!({
                "message": "simulated provider rejection",
                "additionalDetails": "the request was rejected before model output",
                "codexErrorInfo": "badRequest"
            });
            self.emit_usage(output)?;
            emit(
                output,
                json!({
                    "method": "error",
                    "params": {
                        "error": error,
                        "threadId": self.thread_id,
                        "turnId": turn_id,
                        "willRetry": false
                    }
                }),
            )?;
            return emit(
                output,
                json!({
                    "method": "turn/completed",
                    "params": {
                        "turn": {
                            "id": turn_id,
                            "status": "failed",
                            "error": error
                        }
                    }
                }),
            );
        }
        if matches!(
            self.scenario.as_str(),
            "patch_worker" | "patch_repair_initial" | "patch_repair"
        ) {
            let output_schema =
                message.pointer("/params/outputSchema").map(Value::to_string).unwrap_or_default();
            if !output_schema.contains("acceptance_coverage")
                || !output_schema.contains("changed_files")
            {
                return Err("patch worker did not receive its bounded output schema".to_owned());
            }
            let request_id = json!(9101);
            let item_id = "needle-sim-patch-1".to_owned();
            emit(
                output,
                json!({
                    "id": request_id,
                    "method": "item/fileChange/requestApproval",
                    "params": {
                        "threadId": self.thread_id,
                        "turnId": turn_id,
                        "itemId": item_id,
                        "reason": "Apply the requested isolated fixture update"
                    }
                }),
            )?;
            self.pending_approval = Some(PendingApproval {
                request_id,
                turn_id,
                item_id,
                test_plan: false,
                file_change: true,
            });
            return Ok(());
        }
        if matches!(self.scenario.as_str(), "verifier_worker" | "verifier_repair_flow") {
            let prompt =
                message.pointer("/params/input/0/text").and_then(Value::as_str).unwrap_or_default();
            if prompt.contains("Updated the fixture in the disposable checkout") {
                return Err("verifier received the patcher transcript".to_owned());
            }
            let contents = fs::read_to_string(self.checkout.join("fixture.txt"))
                .map_err(|error| format!("verifier cannot inspect patched fixture: {error}"))?;
            if self.scenario == "verifier_repair_flow" && contents == "broken by isolated patcher\n"
            {
                return self.complete_turn(
                    &turn_id,
                    json!({
                        "verdict": "repairable",
                        "acceptance_coverage": [{
                            "criterion": "The fixture changes.",
                            "status": "partial",
                            "evidence": "fixture.txt changed but contains the wrong value"
                        }],
                        "findings": ["Replace the broken fixture value with the requested value."]
                    }),
                    output,
                );
            }
            if contents != "changed by isolated patcher\n" {
                return Err("verifier did not receive the materialized patch".to_owned());
            }
            return self.complete_turn(
                &turn_id,
                json!({
                    "verdict": "verified",
                    "acceptance_coverage": [{
                        "criterion": "The fixture changes.",
                        "status": "addressed",
                        "evidence": "fixture.txt contains the updated text"
                    }],
                    "findings": []
                }),
                output,
            );
        }
        let output_schema =
            message.pointer("/params/outputSchema").map(Value::to_string).unwrap_or_default();
        let test_plan_only = output_schema.contains("\"const\":\"test-plan\"")
            && !output_schema.contains("\"const\":\"code-location\"");
        if self.turn == 1 {
            if self.scenario == "test_not_invoked" {
                return self.complete_turn(&turn_id, repair_artifact("repair_success"), output);
            }
            if matches!(
                self.scenario.as_str(),
                "worker_r44"
                    | "worker_r47_optional_test"
                    | "worker_r61_locate"
                    | "worker_r61_trace"
                    | "worker_partial_tests"
            ) {
                let observed_files = if self.scenario == "worker_r61_trace" {
                    ["crates/core/flags/defs.rs", "crates/core/flags/hiargs.rs"]
                } else if self.scenario == "worker_partial_tests" {
                    ["tests/feature.rs", "tests/feature.rs"]
                } else {
                    ["crates/core/flags/defs.rs", "tests/misc.rs"]
                };
                for (index, relative) in observed_files.into_iter().enumerate() {
                    emit(
                        output,
                        json!({
                            "method": "item/completed",
                            "params": {
                                "item": {
                                    "id": format!("needle-sim-read-{index}"),
                                    "type": "commandExecution",
                                    "command": format!("read {relative}"),
                                    "commandActions": [{
                                        "type": "read",
                                        "path": self.checkout.join(relative)
                                    }],
                                    "cwd": self.checkout,
                                    "status": "completed",
                                    "exitCode": 0,
                                    "durationMs": 1,
                                    "aggregatedOutput": ""
                                }
                            }
                        }),
                    )?;
                }
            }
            if self.scenario == "worker_r47_optional_test" {
                return self.complete_turn(&turn_id, r44_location_artifact(), output);
            }
            if self.scenario == "worker_r61_locate" {
                return self.complete_turn(&turn_id, r44_location_artifact(), output);
            }
            if self.scenario == "worker_r61_trace" {
                return self.complete_turn(&turn_id, r61_trace_artifact(), output);
            }
            if self.scenario == "worker_partial_tests" {
                if !test_plan_only {
                    return Err("partial-tests worker was not restricted to test-plan".to_owned());
                }
                let prompt = message
                    .pointer("/params/input/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if prompt.contains("Trace how --crlf changes matching and search line terminators")
                {
                    return Err("partial-tests worker repeated the original broad task".to_owned());
                }
                for required in [
                    "Do not reconstruct the original broad task",
                    "subject cli-option: \"--crlf\"",
                    "missing obligation focused-tests",
                ] {
                    if !prompt.contains(required) {
                        return Err(format!(
                            "partial-tests worker prompt omitted residual constraint `{required}`"
                        ));
                    }
                }
                return self.complete_turn(&turn_id, crlf_test_plan_artifact(), output);
            }
            if !self.test_execution_allowed {
                let artifact = if output_schema.contains("needle.artifact-result/2") {
                    semantic_flag_location_artifact()
                } else {
                    initial_artifact()
                };
                return self.complete_turn(&turn_id, artifact, output);
            }
            let item_id = "needle-sim-command-1".to_owned();
            let request_id = json!(9001);
            let test_command =
                if self.scenario == "worker_r44" { R44_TEST_COMMAND } else { TEST_COMMAND };
            emit(
                output,
                json!({
                    "id": request_id,
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "threadId": self.thread_id,
                        "turnId": turn_id,
                        "itemId": item_id,
                        "command": test_command,
                        "cwd": self.checkout,
                        "commandActions": [{
                            "type": "unknown",
                            "command": test_command
                        }],
                        "additionalPermissions": {}
                    }
                }),
            )?;
            self.pending_approval = Some(PendingApproval {
                request_id,
                turn_id,
                item_id,
                test_plan: test_plan_only,
                file_change: false,
            });
            return Ok(());
        }
        if test_plan_only {
            return self.complete_turn(&turn_id, test_plan_artifact(), output);
        }
        self.complete_turn(&turn_id, repair_artifact(&self.scenario), output)
    }

    fn start_main_need_turn(
        &mut self,
        turn_id: &str,
        output: &mut impl Write,
        reworded_hit: bool,
    ) -> Result<(), String> {
        if self.scenario == "main_interrupt_r61_trace" {
            return self.emit_main_need(
                turn_id,
                output,
                [
                    "@@need\n@route trace.state-flow\n",
                    "@subject cli-option:\"--crlf\"\n",
                    "@require implementation-location selection=primary\n",
                    "@require runtime-flow scenario=default completeness=contract-complete granularity=stepwise\n",
                    "@require focused-tests selection=representative completeness=open-world\n",
                    "@world source=current features=default\n\n",
                    "Trace the runtime flow and identify the focused test.\n@@end",
                ],
            );
        }
        let subject = match self.scenario.as_str() {
            "main_interrupt_invalid_subject" => "@subject cli_flag:\"--glob-case-insensitive\"\n",
            "main_interrupt_r35_cache"
            | "main_interrupt_r44"
            | "main_interrupt_r59_covered_repeat"
            | "main_interrupt_r61_locate" => "@subject cli-option:\"--glob-case-insensitive\"\n",
            _ => "@subject symbol:\"flag_definition\"\n",
        };
        let body = if is_ripgrep_main_scenario(&self.scenario) && reworded_hit {
            "Find the primary code location for the option.\n@@end"
        } else if matches!(
            self.scenario.as_str(),
            "main_interrupt_r35_cache"
                | "main_interrupt_r44"
                | "main_interrupt_r59_covered_repeat"
                | "main_interrupt_r61_locate"
        ) {
            "Locate the option implementation.\n@@end"
        } else {
            "Locate the primary implementation.\n@@end"
        };
        let focused = if self.scenario == "main_interrupt_r61_locate" && !reworded_hit {
            "@prefer focused-tests selection=representative completeness=open-world\n"
        } else if self.scenario == "main_interrupt_two_needs"
            || (is_ripgrep_main_scenario(&self.scenario) && !reworded_hit)
        {
            "@require focused-tests selection=representative completeness=open-world\n"
        } else {
            ""
        };
        let coordination = if self.scenario.contains("continue") {
            "@coordination continue-working\n"
        } else {
            ""
        };
        for delta in [
            "@@need\n@route locate.implementation\n",
            coordination,
            subject,
            "@require implementation-location selection=primary granularity=exact-location\n",
            focused,
            "@world source=current features=default\n\n",
            body,
        ] {
            emit(
                output,
                json!({
                    "method": "item/agentMessage/delta",
                    "params": {
                        "threadId": self.thread_id,
                        "turnId": turn_id,
                        "itemId": "needle-sim-main-need",
                        "delta": delta
                    }
                }),
            )?;
        }
        self.pending_main_turn = Some(turn_id.to_owned());
        if self.scenario == "main_interrupt_continue_queued" {
            for (item_id, text) in [
                (
                    "needle-sim-main-queued-test-need",
                    "@@need\n@route tests.relevant\n@coordination continue-working\n@subject symbol:\"flag_definition\"\n@require focused-tests selection=representative completeness=open-world polarity=positive\n@world source=current features=default\n\nFind the focused test while the first need resolves.\n@@end",
                ),
                (
                    "needle-sim-main-queued-trace-need",
                    "@@need\n@route trace.state-flow\n@coordination continue-working\n@subject symbol:\"flag_definition\"\n@require implementation-location selection=primary granularity=exact-location\n@require runtime-flow scenario=default completeness=stepwise\n@world source=current features=default\n\nTrace the runtime flow while the earlier needs resolve.\n@@end",
                ),
            ] {
                emit(
                    output,
                    json!({
                        "method": "item/completed",
                        "params": {
                            "threadId": self.thread_id,
                            "turnId": turn_id,
                            "item": {
                                "id": item_id,
                                "type": "agentMessage",
                                "text": text
                            }
                        }
                    }),
                )?;
            }
        }
        if self.scenario == "main_interrupt_continue_queue_overflow" {
            for index in 0..9 {
                emit(
                    output,
                    json!({
                        "method": "item/completed",
                        "params": {
                            "threadId": self.thread_id,
                            "turnId": turn_id,
                            "item": {
                                "id": format!("needle-sim-main-overflow-{index}"),
                                "type": "agentMessage",
                                "text": "@@need\n@route tests.relevant\n@coordination continue-working\n@subject symbol:\"flag_definition\"\n@require focused-tests selection=representative completeness=open-world polarity=positive\n@world source=current features=default\n\nFind another focused test.\n@@end"
                            }
                        }
                    }),
                )?;
            }
        }
        if self.scenario == "main_interrupt_continue_cancelled" {
            self.emit_usage(output)?;
            emit(
                output,
                json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": turn_id, "status": "cancelled"}}
                }),
            )?;
            self.pending_main_turn = None;
        }
        if self.scenario == "main_interrupt_continue_tools" {
            emit(
                output,
                json!({
                    "method": "item/started",
                    "params": {
                        "threadId": self.thread_id,
                        "turnId": turn_id,
                        "item": {"id": "sim-tool", "type": "webSearch"}
                    }
                }),
            )?;
        }
        Ok(())
    }

    fn emit_main_need<const N: usize>(
        &mut self,
        turn_id: &str,
        output: &mut impl Write,
        deltas: [&str; N],
    ) -> Result<(), String> {
        for delta in deltas {
            emit(
                output,
                json!({
                    "method": "item/agentMessage/delta",
                    "params": {
                        "threadId": self.thread_id,
                        "turnId": turn_id,
                        "itemId": "needle-sim-main-need",
                        "delta": delta
                    }
                }),
            )?;
        }
        self.pending_main_turn = Some(turn_id.to_owned());
        Ok(())
    }

    fn steer_main_turn(
        &mut self,
        id: Option<Value>,
        message: Value,
        output: &mut impl Write,
    ) -> Result<(), String> {
        let expected = message
            .pointer("/params/expectedTurnId")
            .and_then(Value::as_str)
            .ok_or_else(|| "turn/steer omitted expectedTurnId".to_owned())?;
        if self.pending_main_turn.as_deref() != Some(expected) {
            return respond_error(output, id, "activeTurnNotSteerable");
        }
        if self.scenario == "main_interrupt_continue_not_steerable" {
            let turn_id = self.pending_main_turn.take().unwrap();
            self.emit_usage(output)?;
            emit(
                output,
                json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": turn_id, "status": "completed"}}
                }),
            )?;
            return respond_error(output, id, "activeTurnNotSteerable");
        }
        let turn_id = self.pending_main_turn.take().unwrap();
        respond(output, id, json!({"turnId": turn_id}))?;
        self.complete_text_turn(
            &turn_id,
            "The steered response uses the delivered Needle context.",
            output,
        )
    }

    fn interrupt_main_turn(
        &mut self,
        id: Option<Value>,
        message: Value,
        output: &mut impl Write,
    ) -> Result<(), String> {
        let turn_id = message
            .pointer("/params/turnId")
            .and_then(Value::as_str)
            .ok_or_else(|| "turn/interrupt omitted turnId".to_owned())?;
        if self.pending_main_turn.as_deref() != Some(turn_id) {
            return Err("turn/interrupt did not target the pending semantic turn".to_owned());
        }
        self.pending_main_turn = None;
        respond(output, id, json!({}))?;
        self.emit_usage(output)?;
        emit(
            output,
            json!({
                "method": "turn/completed",
                "params": {
                    "turn": {
                        "id": turn_id,
                        "status": "interrupted"
                    }
                }
            }),
        )
    }

    fn complete_approved_command(&mut self, output: &mut impl Write) -> Result<(), String> {
        let pending =
            self.pending_approval.take().ok_or_else(|| "approval state is missing".to_owned())?;
        let expected_command = match self.scenario.as_str() {
            "worker_r44" => R44_TEST_COMMAND,
            "main_direct_read_only" => MAIN_DIRECT_READ_COMMAND,
            "main_direct_r84_pending_approval" => MAIN_DIRECT_R84_SCRIPT,
            _ => TEST_COMMAND,
        };
        let (command, action) = if self.scenario == "payload_mismatch" {
            (TEST_COMMAND, "cargo test other -- --exact")
        } else {
            (expected_command, expected_command)
        };
        let (exit_code, test_output) = match self.scenario.as_str() {
            "wrong_test_identifier" => {
                (0, "\nrunning 1 test\ntest other ... ok\n\ntest result: ok. 1 passed; 0 failed\n")
            }
            "no_test_executed" => (0, "\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed\n"),
            "test_exit_failure" => (
                1,
                "\nrunning 1 test\ntest suite::focused ... FAILED\n\ntest result: FAILED. 0 passed; 1 failed\n",
            ),
            "worker_r44" => (
                0,
                "\nrunning 1 test\ntest misc::glob_always_case_insensitive ... ok\n\ntest result: ok. 1 passed; 0 failed\n",
            ),
            _ => (
                0,
                "\nrunning 1 test\ntest suite::focused ... ok\n\ntest result: ok. 1 passed; 0 failed\n",
            ),
        };
        emit(
            output,
            json!({
                "method": "item/completed",
                "params": {
                    "item": {
                        "id": pending.item_id,
                        "type": "commandExecution",
                        "command": command,
                        "commandActions": [{
                            "type": "unknown",
                            "command": action
                        }],
                        "cwd": self.checkout,
                        "status": "completed",
                        "exitCode": exit_code,
                        "durationMs": 12,
                        "aggregatedOutput": test_output
                    }
                }
            }),
        )?;
        if self.scenario == "main_direct_read_only" {
            return self.complete_text_turn(
                &pending.turn_id,
                "The bounded read found flag_definition in src/lib.rs.",
                output,
            );
        }
        if self.scenario == "main_direct_r84_pending_approval" {
            return self.complete_text_turn(
                &pending.turn_id,
                "The unbounded script was declined.",
                output,
            );
        }
        self.complete_turn(
            &pending.turn_id,
            if self.scenario == "worker_r44" {
                r44_location_artifact()
            } else if pending.test_plan {
                test_plan_artifact()
            } else {
                initial_artifact()
            },
            output,
        )
    }

    fn complete_approved_file_change(
        &mut self,
        message: Value,
        output: &mut impl Write,
    ) -> Result<(), String> {
        let pending =
            self.pending_approval.take().ok_or_else(|| "approval state is missing".to_owned())?;
        if !self.workspace_write {
            return Err("patch worker was not started in workspace-write sandbox".to_owned());
        }
        if message.pointer("/result/decision").and_then(Value::as_str) != Some("accept") {
            return Err("patch worker file change was not accepted once".to_owned());
        }
        let contents = if self.scenario == "patch_repair_initial" {
            "broken by isolated patcher\n"
        } else {
            "changed by isolated patcher\n"
        };
        fs::write(self.checkout.join("fixture.txt"), contents)
            .map_err(|error| format!("cannot apply simulated patch: {error}"))?;
        emit(
            output,
            json!({
                "method": "item/completed",
                "params": {
                    "item": {
                        "id": pending.item_id,
                        "type": "fileChange",
                        "status": "completed"
                    }
                }
            }),
        )?;
        self.complete_turn(
            &pending.turn_id,
            json!({
                "summary": "Updated the fixture in the disposable checkout.",
                "changed_files": [{"path": "fixture.txt", "operation": "update"}],
                "acceptance_coverage": [{
                    "criterion": "The fixture changes.",
                    "status": "addressed",
                    "evidence": "fixture.txt contains the requested updated text"
                }],
                "residual_risks": []
            }),
            output,
        )
    }

    fn complete_turn(
        &self,
        turn_id: &str,
        artifact: Value,
        output: &mut impl Write,
    ) -> Result<(), String> {
        emit(
            output,
            json!({
                "method": "item/completed",
                "params": {
                    "item": {
                        "id": format!("needle-sim-message-{}", self.turn),
                        "type": "agentMessage",
                        "text": artifact.to_string()
                    }
                }
            }),
        )?;
        self.emit_usage(output)?;
        emit(
            output,
            json!({
                "method": "turn/completed",
                "params": {
                    "turn": {
                        "id": turn_id,
                        "status": "completed"
                    }
                }
            }),
        )
    }

    fn complete_text_turn(
        &self,
        turn_id: &str,
        response: &str,
        output: &mut impl Write,
    ) -> Result<(), String> {
        emit(
            output,
            json!({
                "method": "item/completed",
                "params": {
                    "item": {
                        "id": format!("needle-sim-message-{}", self.turn),
                        "type": "agentMessage",
                        "phase": "final_answer",
                        "text": response
                    }
                }
            }),
        )?;
        self.emit_usage(output)?;
        emit(
            output,
            json!({
                "method": "turn/completed",
                "params": {
                    "turn": {
                        "id": turn_id,
                        "status": "completed"
                    }
                }
            }),
        )
    }

    fn emit_usage(&self, output: &mut impl Write) -> Result<(), String> {
        let (input_tokens, cached_input_tokens, output_tokens) =
            if is_ripgrep_main_scenario(&self.scenario) && !self.main_mode {
                (500, 100, 100)
            } else {
                (100 * u64::from(self.turn), 40 * u64::from(self.turn), 20 * u64::from(self.turn))
            };
        emit(
            output,
            json!({
                "method": "thread/tokenUsage/updated",
                "params": {
                    "tokenUsage": {
                        "total": {
                            "inputTokens": input_tokens,
                            "cachedInputTokens": cached_input_tokens,
                            "outputTokens": output_tokens
                        }
                    }
                }
            }),
        )
    }
}

fn initial_artifact() -> Value {
    json!({
        "schema": "needle.artifact-result/1",
        "artifacts": [{
            "kind": "code-location",
            "p": "src/lib.rs",
            "s": "flag_definition",
            "f": ["The flag is declared and parsed here."]
        }],
        "test_plan": null
    })
}

fn semantic_flag_location_artifact() -> Value {
    json!({
        "schema": "needle.artifact-result/2",
        "artifacts": [{
            "kind": "code-location",
            "locations": [{
                "role": "primary",
                "path": "src/lib.rs",
                "symbol": "flag_definition",
                "byte_start": null,
                "byte_end": null
            }],
            "gaps": []
        }]
    })
}

fn repair_artifact(scenario: &str) -> Value {
    if scenario == "incomplete_after_repair" {
        return initial_artifact();
    }
    json!({
        "schema": "needle.artifact-result/1",
        "artifacts": [{
            "kind": "behavior-trace",
            "p": "src/lib.rs",
            "s": "apply_flag",
            "f": ["The parsed flag configures the file walker."]
        }],
        "test_plan": null
    })
}

fn test_plan_artifact() -> Value {
    json!({
        "schema": "needle.artifact-result/2",
        "artifacts": [{
            "kind": "test-plan",
            "runner": "cargo",
            "argv": ["cargo", "test", "suite::focused", "--", "--exact"],
            "cwd_relative": ".",
            "identifiers": ["suite::focused"],
            "selection": "representative",
            "evidence_paths": ["src/lib.rs"]
        }]
    })
}

fn r44_location_artifact() -> Value {
    json!({
        "schema": "needle.artifact-result/2",
        "artifacts": [{
            "kind": "code-location",
            "locations": [{
                "role": "primary",
                "path": "crates/core/flags/defs.rs",
                "symbol": "GlobCaseInsensitive",
                "byte_start": null,
                "byte_end": null
            }],
            "gaps": []
        }]
    })
}

fn r61_trace_artifact() -> Value {
    json!({
        "schema": "needle.artifact-result/2",
        "artifacts": [
            {
                "kind": "code-location",
                "locations": [{
                    "role": "primary",
                    "path": "crates/core/flags/defs.rs",
                    "symbol": "Crlf",
                    "byte_start": null,
                    "byte_end": null
                }],
                "gaps": []
            },
            {
                "kind": "behavior-trace",
                "scenario": "Default CLI search configuration and the --crlf-enabled CRLF search path",
                "steps": [
                    {
                        "role": "producer",
                        "location": {
                            "role": "primary",
                            "path": "crates/core/flags/defs.rs",
                            "symbol": "Crlf",
                            "byte_start": null,
                            "byte_end": null
                        },
                        "description": "The --crlf switch is parsed into the low-level arguments."
                    },
                    {
                        "role": "carrier",
                        "location": {
                            "role": "supporting",
                            "path": "crates/core/flags/hiargs.rs",
                            "symbol": "HiArgs",
                            "byte_start": null,
                            "byte_end": null
                        },
                        "description": "The parsed CRLF state is carried by the high-level arguments."
                    },
                    {
                        "role": "transformation",
                        "location": {
                            "role": "supporting",
                            "path": "crates/core/flags/hiargs.rs",
                            "symbol": "matcher",
                            "byte_start": null,
                            "byte_end": null
                        },
                        "description": "The matcher enables CRLF-aware matching."
                    },
                    {
                        "role": "precedence",
                        "location": {
                            "role": "supporting",
                            "path": "crates/core/flags/defs.rs",
                            "symbol": "NullData",
                            "byte_start": null,
                            "byte_end": null
                        },
                        "description": "Null-data precedence can clear the CRLF setting."
                    },
                    {
                        "role": "consumer",
                        "location": {
                            "role": "supporting",
                            "path": "crates/core/flags/hiargs.rs",
                            "symbol": "searcher",
                            "byte_start": null,
                            "byte_end": null
                        },
                        "description": "The searcher selects the CRLF line terminator."
                    }
                ],
                "gaps": []
            },
            {
                "kind": "test-plan",
                "runner": "cargo",
                "argv": [
                    "cargo",
                    "test",
                    "--offline",
                    "--test",
                    "integration",
                    "feature::f416_crlf",
                    "--",
                    "--exact"
                ],
                "cwd_relative": ".",
                "identifiers": ["feature::f416_crlf"],
                "selection": "representative",
                "evidence_paths": ["Cargo.toml", "tests/tests.rs", "tests/feature.rs"]
            }
        ]
    })
}

fn crlf_test_plan_artifact() -> Value {
    json!({
        "schema": "needle.artifact-result/2",
        "artifacts": [{
            "kind": "test-plan",
            "runner": "cargo",
            "argv": [
                "test",
                "--offline",
                "--test",
                "integration",
                "feature::f416_crlf",
                "--",
                "--exact"
            ],
            "cwd_relative": ".",
            "identifiers": ["feature::f416_crlf"],
            "selection": "representative",
            "evidence_paths": ["Cargo.toml", "tests/feature.rs"]
        }]
    })
}

fn serve_app_server() -> Result<(), String> {
    let stdin = io::stdin();
    let mut output = io::stdout().lock();
    let mut simulator = Simulator::default();
    for line in stdin.lock().lines() {
        let line = line.map_err(|error| error.to_string())?;
        let message: Value = serde_json::from_str(&line).map_err(|error| error.to_string())?;
        simulator.handle(message, &mut output)?;
    }
    Ok(())
}

fn respond(output: &mut impl Write, id: Option<Value>, result: Value) -> Result<(), String> {
    let Some(id) = id else {
        return Ok(());
    };
    emit(output, json!({"id": id, "result": result}))
}

fn respond_error(output: &mut impl Write, id: Option<Value>, code: &str) -> Result<(), String> {
    let Some(id) = id else {
        return Ok(());
    };
    emit(output, json!({"id": id, "error": {"code": code, "message": code}}))
}

fn emit(output: &mut impl Write, value: Value) -> Result<(), String> {
    serde_json::to_writer(&mut *output, &value).map_err(|error| error.to_string())?;
    output.write_all(b"\n").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())
}
