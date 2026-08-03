pub(crate) mod change_schema;
pub(crate) mod schema;

use crate::product_resolver::{ProductResolver, WorkerPolicy};
use change_schema::{
    McpPrepareChangeRequest, McpPrepareChangeResponse, McpVerifyChangeRequest,
    McpVerifyChangeResponse,
};
use needle_core::{
    CacheResolution, CanonicalHasher, Digest, MultiNeedPolicy, Need, NeedCoordination,
    NeedDelivery, NeedStep, NeedStepRelation, NeedStepState, PredicateKind,
    ReuseSufficiencyCertificateId, VerificationStatus, classify_need_step,
};
use needle_platform_codex::{CodexPatchWorker, CodexVerifier, PatchContextItem};
use needle_runtime::{
    ResolveOutcome, ResolveRequest, artifact_and_certificate_are_fresh,
    claim_validation_certificate_is_fresh,
};
use schema::{
    MAX_MCP_REQUEST_BYTES, McpNeedContextRequest, McpNeedContextResponse, McpResolution, McpStep,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

const MODERN_PROTOCOL_VERSION: &str = "2025-06-18";
const LEGACY_PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_PROTOCOL_LINE_BYTES: usize = 64 * 1024;
const MAX_CONTEXT_CHARS: usize = 64 * 1024;
const OBSERVATION_FILE: &str = "mcp-observations.jsonl";
const MCP_SERVER_INSTRUCTIONS: &str = "Call need_context before repository discovery when typed evidence could answer the task. Reuse certified context. Use prepare_change only for an explicitly requested change with bounded path scopes; it edits a disposable checkout and returns an unverified patch. Call verify_change only when independent verification is needed. These tools never modify the active worktree.";
const NEED_CONTEXT_DESCRIPTION: &str = "Resolve typed repository evidence for one explicit semantic route and subject. Declare required and preferred capabilities; omitted facets are completed only by the selected route contract. Returns bounded certified context or a normal bypass.";
const PREPARE_CHANGE_DESCRIPTION: &str = "Prepare a bounded UTF-8 source patch in an isolated disposable checkout. The caller supplies acceptance criteria, writable path scopes, and optional certified artifact or claim context. Returns patch metadata only; verification and application are separate explicit operations.";
const VERIFY_CHANGE_DESCRIPTION: &str = "Independently verify one prepared change in a fresh read-only disposable checkout. Loads task, patch, acceptance criteria, and certified tests from SQLite by change_id; never receives the patcher transcript. Returns a verified, rejected, repairable, or inconclusive verdict without applying the patch.";

pub(crate) fn contract_microbench(request_path: &Path, iterations: u32) -> Result<Value, String> {
    if !(100..=100_000).contains(&iterations) {
        return Err("iterations must be between 100 and 100000".to_owned());
    }
    let bytes = fs::read(request_path).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_MCP_REQUEST_BYTES {
        return Err("benchmark request exceeds the 16 KiB bound".to_owned());
    }
    let routes = needle_core::built_in_route_contracts()
        .into_iter()
        .map(|contract| contract.route.as_str().to_owned())
        .collect::<Vec<_>>();
    let mut samples = Vec::with_capacity(iterations as usize);
    let mut last_digest = None;
    for _ in 0..iterations {
        let started = Instant::now();
        let request = serde_json::from_slice::<McpNeedContextRequest>(&bytes)
            .map_err(|error| error.to_string())?;
        let mapped = request.validate_and_map(&routes, bytes.len())?;
        last_digest = Some(mapped.request_digest);
        samples.push(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
    }
    samples.sort_unstable();
    let percentile = |percent: usize| {
        let index = (samples.len().saturating_sub(1) * percent) / 100;
        samples[index]
    };
    Ok(json!({
        "schema": "needle.mcp-contract-microbench/1",
        "iterations": iterations,
        "request_bytes": bytes.len(),
        "parse_map_hash_ns": {
            "p50": percentile(50),
            "p95": percentile(95),
            "max": samples.last().copied().unwrap_or_default()
        },
        "request_digest": last_digest,
        "provider_observations_started": 0
    }))
}

pub(crate) struct ProductMcpConfig {
    pub(crate) data_directory: PathBuf,
    pub(crate) repository_root: PathBuf,
    pub(crate) main_model: String,
    pub(crate) cache_only: bool,
    pub(crate) calibration_reuse: bool,
}

pub(crate) fn serve(config: ProductMcpConfig) -> Result<(), String> {
    let server = Arc::new(Mutex::new(ProductMcpServer::new(config)?));
    let cancellation =
        server.lock().map_err(|_| "MCP server lock was poisoned".to_owned())?.cancellation.clone();
    let (input_sender, input_receiver) = mpsc::sync_channel::<Result<String, String>>(16);
    thread::spawn(move || {
        let input = io::stdin();
        let mut input = BufReader::new(input.lock());
        let mut line = String::new();
        loop {
            line.clear();
            match input.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if input_sender.send(Ok(line.clone())).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = input_sender.send(Err(error.to_string()));
                    break;
                }
            }
        }
    });
    let mut output = io::BufWriter::new(io::stdout().lock());
    let mut queued = VecDeque::new();
    loop {
        let line = if let Some(line) = queued.pop_front() {
            line
        } else {
            match input_receiver.recv() {
                Ok(Ok(line)) => line,
                Ok(Err(error)) => return Err(error),
                Err(_) => break,
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let response = if line.len() > MAX_PROTOCOL_LINE_BYTES {
            rpc_error(Value::Null, -32700, "request exceeds the 64 KiB protocol cap")
        } else {
            match serde_json::from_str::<Value>(&line) {
                Ok(request)
                    if request.get("method").and_then(Value::as_str) == Some("tools/call") =>
                {
                    resolve_while_accepting_cancellation(
                        Arc::clone(&server),
                        &cancellation,
                        request,
                        &input_receiver,
                        &mut queued,
                        &mut output,
                    )?
                }
                Ok(request) => server
                    .lock()
                    .map_err(|_| "MCP server lock was poisoned".to_owned())?
                    .response(&request),
                Err(error) => rpc_error(Value::Null, -32700, &format!("parse error: {error}")),
            }
        };
        if !response.is_null() {
            write_response(&mut output, &response)?;
        }
    }
    output.flush().map_err(|error| error.to_string())
}

fn resolve_while_accepting_cancellation(
    server: Arc<Mutex<ProductMcpServer>>,
    cancellation: &AtomicBool,
    request: Value,
    input_receiver: &mpsc::Receiver<Result<String, String>>,
    queued: &mut VecDeque<String>,
    output: &mut impl Write,
) -> Result<Value, String> {
    let active_id = request.get("id").cloned().unwrap_or(Value::Null);
    cancellation.store(false, Ordering::Release);
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let response = server
            .lock()
            .map(|mut server| server.response(&request))
            .unwrap_or_else(|_| rpc_error(Value::Null, -32603, "MCP server lock was poisoned"));
        let _ = sender.send(response);
    });
    loop {
        match receiver.recv_timeout(std::time::Duration::from_millis(10)) {
            Ok(response) => return Ok(response),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Ok(rpc_error(Value::Null, -32603, "MCP resolver thread stopped"));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        loop {
            match input_receiver.try_recv() {
                Ok(Ok(line)) if cancellation_targets(&line, &active_id) => {
                    cancellation.store(true, Ordering::Release);
                }
                Ok(Ok(line)) if queued.len() < 8 => queued.push_back(line),
                Ok(Ok(line)) => {
                    if let Some(id) = json_rpc_id(&line) {
                        write_response(
                            output,
                            &rpc_error(id, -32000, "MCP request queue is full"),
                        )?;
                    }
                }
                Ok(Err(error)) => return Err(error),
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
            }
        }
    }
}

fn write_response(output: &mut impl Write, response: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *output, response).map_err(|error| error.to_string())?;
    output.write_all(b"\n").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())
}

fn cancellation_targets(line: &str, active_id: &Value) -> bool {
    serde_json::from_str::<Value>(line).ok().is_some_and(|request| {
        request.get("method").and_then(Value::as_str) == Some("notifications/cancelled")
            && request.pointer("/params/requestId") == Some(active_id)
    })
}

fn json_rpc_id(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line).ok()?.get("id").cloned()
}

fn tool_call_params(params: Option<&Value>) -> Result<&Map<String, Value>, String> {
    let params =
        params.and_then(Value::as_object).ok_or_else(|| "params must be an object".to_owned())?;
    if let Some(key) = params.keys().find(|key| {
        key.as_str() != "name" && key.as_str() != "arguments" && key.as_str() != "_meta"
    }) {
        let bounded = key.chars().take(64).collect::<String>();
        return Err(format!("unknown tools/call parameter `{bounded}`"));
    }
    if params.get("_meta").is_some_and(|value| !value.is_object()) {
        return Err("tools/call _meta must be an object".to_owned());
    }
    Ok(params)
}

/// Deterministic transport fixture used only by plugin validation. It shares
/// the public JSON contract with the product server but never invokes a model.
pub(crate) fn serve_benchmark() -> Result<(), String> {
    let routes = needle_core::built_in_route_contracts()
        .into_iter()
        .map(|contract| contract.route.as_str().to_owned())
        .collect::<Vec<_>>();
    let mut server = BenchmarkMcpServer {
        input_schema: schema::input_schema(&routes),
        output_schema: schema::output_schema(),
        routes,
        protocol: None,
        initialized: false,
        seen_request_ids: BTreeSet::new(),
        ordinal: 0,
    };
    let input = io::stdin();
    let mut input = BufReader::new(input.lock());
    let mut output = io::BufWriter::new(io::stdout().lock());
    let mut line = String::new();
    while input.read_line(&mut line).map_err(|error| error.to_string())? != 0 {
        if !line.trim().is_empty() {
            let response = if line.len() > MAX_PROTOCOL_LINE_BYTES {
                rpc_error(Value::Null, -32700, "request exceeds the 64 KiB protocol cap")
            } else {
                serde_json::from_str::<Value>(&line)
                    .map(|request| server.response(&request))
                    .unwrap_or_else(|error| {
                        rpc_error(Value::Null, -32700, &format!("parse error: {error}"))
                    })
            };
            if !response.is_null() {
                serde_json::to_writer(&mut output, &response).map_err(|error| error.to_string())?;
                output.write_all(b"\n").map_err(|error| error.to_string())?;
                output.flush().map_err(|error| error.to_string())?;
            }
        }
        line.clear();
    }
    Ok(())
}

struct BenchmarkMcpServer {
    routes: Vec<String>,
    input_schema: Value,
    output_schema: Value,
    protocol: Option<NegotiatedProtocol>,
    initialized: bool,
    seen_request_ids: BTreeSet<String>,
    ordinal: u8,
}

impl BenchmarkMcpServer {
    fn response(&mut self, request: &Value) -> Value {
        if !request.is_object() || request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return rpc_error(
                request.get("id").cloned().unwrap_or(Value::Null),
                -32600,
                "invalid request",
            );
        }
        let method = request.get("method").and_then(Value::as_str).unwrap_or_default();
        if request.get("id").is_none() {
            if method == "notifications/initialized" && self.protocol.is_some() {
                self.initialized = true;
            }
            return Value::Null;
        }
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        if !self.seen_request_ids.insert(id.to_string()) {
            return rpc_error(id, -32600, "duplicate request id");
        }
        if method == "initialize" {
            let Some(version) = request.pointer("/params/protocolVersion").and_then(Value::as_str)
            else {
                return rpc_error(id, -32602, "initialize requires protocolVersion");
            };
            self.protocol = match version {
                MODERN_PROTOCOL_VERSION => Some(NegotiatedProtocol::Modern),
                LEGACY_PROTOCOL_VERSION => Some(NegotiatedProtocol::Legacy),
                _ => return rpc_error(id, -32602, "unsupported MCP protocol version"),
            };
            return rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "needle-benchmark", "version": crate::VERSION},
                    "instructions": MCP_SERVER_INSTRUCTIONS
                }),
            );
        }
        if !self.initialized {
            return rpc_error(id, -32002, "server is not initialized");
        }
        match method {
            "ping" => rpc_result(id, json!({})),
            "tools/list" => {
                let mut tool = json!({
                    "name": "need_context",
                    "description": NEED_CONTEXT_DESCRIPTION,
                    "annotations": {
                        "readOnlyHint": true,
                        "destructiveHint": false,
                        "openWorldHint": false
                    },
                    "inputSchema": self.input_schema
                });
                if self.protocol == Some(NegotiatedProtocol::Modern) {
                    tool["outputSchema"] = self.output_schema.clone();
                }
                rpc_result(id, json!({"tools": [tool]}))
            }
            "tools/call" => self.tool_call(id, request.get("params")),
            _ => rpc_error(id, -32601, "method not found"),
        }
    }

    fn tool_call(&mut self, id: Value, params: Option<&Value>) -> Value {
        let params = match tool_call_params(params) {
            Ok(params) => params,
            Err(error) => return rpc_error(id, -32602, &error),
        };
        if params.get("name").and_then(Value::as_str) != Some("need_context") {
            return rpc_error(id, -32602, "unknown tool");
        }
        let Some(arguments) = params.get("arguments") else {
            return rpc_error(id, -32602, "missing arguments");
        };
        let encoded_bytes = serde_json::to_vec(arguments).map_or(usize::MAX, |value| value.len());
        let request = match serde_json::from_value::<McpNeedContextRequest>(arguments.clone()) {
            Ok(request) => request,
            Err(error) => return rpc_error(id, -32602, &format!("invalid arguments: {error}")),
        };
        let mapped = match request.validate_and_map(&self.routes, encoded_bytes) {
            Ok(mapped) => mapped,
            Err(error) => return rpc_error(id, -32602, &error),
        };
        let Some(contract) = needle_core::built_in_route_contracts()
            .into_iter()
            .find(|contract| contract.route.as_str() == mapped.request.route)
        else {
            return rpc_error(id, -32602, "selected route has no semantic contract");
        };
        let need = match needle_core::compile_need(
            &mapped.need_ir,
            Digest::blake3(b"needle-benchmark-fixture-repository"),
            &contract,
        ) {
            Ok(need) => need,
            Err(error) => return rpc_error(id, -32602, &error.to_string()),
        };
        self.ordinal = self.ordinal.saturating_add(1);
        let context = needle_bench::p2_payload(&mapped.compatibility_request);
        let response = McpNeedContextResponse {
            status: "generated".to_owned(),
            route: mapped.request.route,
            subject: mapped.request.subject,
            need_id: need.id.to_string(),
            step: McpStep { ordinal: self.ordinal, relation: "independent".to_owned() },
            satisfied: need
                .required
                .iter()
                .map(|item| capability_name(item.predicate).to_owned())
                .collect(),
            missing: Vec::new(),
            resolution: McpResolution::Bypass {
                reason: "deterministic benchmark fixture".to_owned(),
            },
            reuse_unit: "none".to_owned(),
            claim_ids: Vec::new(),
            cache_hit: false,
            worker_spawned: false,
            calibration: false,
            result_digest: Digest::blake3(context.as_bytes()).to_string(),
            context: context.clone(),
        };
        let mut result = json!({
            "content": [{"type": "text", "text": context}],
            "isError": false
        });
        if self.protocol == Some(NegotiatedProtocol::Modern) {
            result["structuredContent"] =
                serde_json::to_value(response).expect("benchmark MCP response serialization");
        }
        rpc_result(id, result)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NegotiatedProtocol {
    Modern,
    Legacy,
}

struct LedgerEntry {
    need: Need,
    satisfied: Vec<needle_core::ObligationId>,
}

struct ProductMcpServer {
    resolver: ProductResolver,
    cancellation: Arc<AtomicBool>,
    repository_root: PathBuf,
    repository_lineage: Digest,
    main_model: String,
    session_id: String,
    next_turn: u64,
    protocol: Option<NegotiatedProtocol>,
    initialized: bool,
    seen_request_ids: BTreeSet<String>,
    cancelled_request_ids: BTreeSet<String>,
    enabled_routes: Vec<String>,
    input_schema: Value,
    output_schema: Value,
    prepare_change_input_schema: Value,
    prepare_change_output_schema: Value,
    verify_change_input_schema: Value,
    verify_change_output_schema: Value,
    transport_definition_digest: Digest,
    changes_enabled: bool,
    policy: MultiNeedPolicy,
    ledger: Vec<LedgerEntry>,
    workers_started: u8,
    calibration_reuse: bool,
}

impl ProductMcpServer {
    fn new(config: ProductMcpConfig) -> Result<Self, String> {
        let data_directory = fs::canonicalize(&config.data_directory).map_err(|error| {
            format!(
                "cannot resolve Needle data directory {}: {error}",
                config.data_directory.display()
            )
        })?;
        let repository_root = fs::canonicalize(&config.repository_root).map_err(|error| {
            format!("cannot resolve repository root {}: {error}", config.repository_root.display())
        })?;
        let cancellation = Arc::new(AtomicBool::new(false));
        let changes_enabled = !config.cache_only;
        let resolver = ProductResolver::new_cancellable(
            &data_directory,
            if config.cache_only { WorkerPolicy::CacheOnly } else { WorkerPolicy::Allow },
            cancellation.clone(),
        )?;
        let (_, snapshot) = needle_runtime::capture_git_snapshot(&repository_root)
            .map_err(|error| error.to_string())?;
        let semantic_routes = needle_core::built_in_route_contracts()
            .into_iter()
            .map(|contract| contract.route.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        let mut enabled_routes = resolver
            .store()
            .routes()
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|route| route.matcher.need_key.as_str().to_owned())
            .filter(|route| semantic_routes.contains(route))
            .collect::<Vec<_>>();
        enabled_routes.sort();
        enabled_routes.dedup();
        let input_schema = schema::input_schema(&enabled_routes);
        let output_schema = schema::output_schema();
        let prepare_change_input_schema = change_schema::input_schema();
        let prepare_change_output_schema = change_schema::output_schema();
        let verify_change_input_schema = change_schema::verify_input_schema();
        let verify_change_output_schema = change_schema::verify_output_schema();
        let transport_definition_digest =
            transport_definition_digest(&enabled_routes, changes_enabled);
        let session_id = mcp_session_id(&repository_root);
        let profile_digest = Digest::blake3(b"needle.mcp-json-profile/1");
        resolver
            .store()
            .record_session_start_for_transport(
                &session_id,
                profile_digest,
                Some(&config.main_model),
                repository_root.to_str(),
                "mcp",
                transport_definition_digest,
                None,
            )
            .map_err(|error| error.to_string())?;
        let policy = resolver
            .store()
            .session(&session_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "MCP session was not persisted".to_owned())?
            .multi_need_policy;
        Ok(Self {
            resolver,
            cancellation,
            repository_root,
            repository_lineage: snapshot.repository_id,
            main_model: config.main_model,
            session_id,
            next_turn: 1,
            protocol: None,
            initialized: false,
            seen_request_ids: BTreeSet::new(),
            cancelled_request_ids: BTreeSet::new(),
            enabled_routes,
            input_schema,
            output_schema,
            prepare_change_input_schema,
            prepare_change_output_schema,
            verify_change_input_schema,
            verify_change_output_schema,
            transport_definition_digest,
            changes_enabled,
            policy,
            ledger: Vec::new(),
            workers_started: 0,
            calibration_reuse: config.calibration_reuse,
        })
    }

    fn response(&mut self, request: &Value) -> Value {
        if !request.is_object() || request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            return rpc_error(id, -32600, "invalid request");
        }
        let method = request.get("method").and_then(Value::as_str).unwrap_or_default();
        if request.get("id").is_none() {
            return self.notification(method, request.get("params"));
        }
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let id_key = id.to_string();
        if !self.seen_request_ids.insert(id_key.clone()) {
            return rpc_error(id, -32600, "duplicate request id");
        }
        if method != "initialize" && !self.initialized {
            return rpc_error(id, -32002, "server is not initialized");
        }
        match method {
            "initialize" => self.initialize(id, request.get("params")),
            "ping" => rpc_result(id, json!({})),
            "tools/list" => self.tools_list(id),
            "tools/call" => self.tool_call(id, id_key, request.get("params")),
            _ => rpc_error(id, -32601, "method not found"),
        }
    }

    fn notification(&mut self, method: &str, params: Option<&Value>) -> Value {
        match method {
            "notifications/initialized" if self.protocol.is_some() => {
                self.initialized = true;
            }
            "notifications/cancelled" => {
                if let Some(id) = params.and_then(|value| value.get("requestId")) {
                    self.cancelled_request_ids.insert(id.to_string());
                }
            }
            _ => {}
        }
        Value::Null
    }

    fn initialize(&mut self, id: Value, params: Option<&Value>) -> Value {
        if self.protocol.is_some() {
            return rpc_error(id, -32600, "initialize may be called only once");
        }
        let Some(version) =
            params.and_then(|value| value.get("protocolVersion")).and_then(Value::as_str)
        else {
            return rpc_error(id, -32602, "initialize requires protocolVersion");
        };
        let protocol = match version {
            MODERN_PROTOCOL_VERSION => NegotiatedProtocol::Modern,
            LEGACY_PROTOCOL_VERSION => NegotiatedProtocol::Legacy,
            _ => return rpc_error(id, -32602, "unsupported MCP protocol version"),
        };
        self.protocol = Some(protocol);
        rpc_result(
            id,
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "needle", "version": crate::VERSION},
                "instructions": MCP_SERVER_INSTRUCTIONS
            }),
        )
    }

    fn tools_list(&self, id: Value) -> Value {
        let mut tool = Map::new();
        tool.insert("name".to_owned(), json!("need_context"));
        tool.insert("description".to_owned(), json!(NEED_CONTEXT_DESCRIPTION));
        tool.insert(
            "annotations".to_owned(),
            json!({"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false}),
        );
        tool.insert("inputSchema".to_owned(), self.input_schema.clone());
        if self.protocol == Some(NegotiatedProtocol::Modern) {
            tool.insert("outputSchema".to_owned(), self.output_schema.clone());
        }
        let mut tools = vec![Value::Object(tool)];
        if self.changes_enabled {
            let mut prepare = Map::new();
            prepare.insert("name".to_owned(), json!("prepare_change"));
            prepare.insert("description".to_owned(), json!(PREPARE_CHANGE_DESCRIPTION));
            prepare.insert(
                "annotations".to_owned(),
                json!({
                    "readOnlyHint": false,
                    "destructiveHint": false,
                    "openWorldHint": false
                }),
            );
            prepare.insert("inputSchema".to_owned(), self.prepare_change_input_schema.clone());
            if self.protocol == Some(NegotiatedProtocol::Modern) {
                prepare
                    .insert("outputSchema".to_owned(), self.prepare_change_output_schema.clone());
            }
            tools.push(Value::Object(prepare));
            let mut verify = Map::new();
            verify.insert("name".to_owned(), json!("verify_change"));
            verify.insert("description".to_owned(), json!(VERIFY_CHANGE_DESCRIPTION));
            verify.insert(
                "annotations".to_owned(),
                json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "openWorldHint": false
                }),
            );
            verify.insert("inputSchema".to_owned(), self.verify_change_input_schema.clone());
            if self.protocol == Some(NegotiatedProtocol::Modern) {
                verify.insert("outputSchema".to_owned(), self.verify_change_output_schema.clone());
            }
            tools.push(Value::Object(verify));
        }
        rpc_result(id, json!({"tools": tools}))
    }

    fn tool_call(&mut self, id: Value, id_key: String, params: Option<&Value>) -> Value {
        if self.cancelled_request_ids.remove(&id_key) {
            return tool_error(id, "request was cancelled before resolution");
        }
        let params = match tool_call_params(params) {
            Ok(params) => params,
            Err(error) => return rpc_error(id, -32602, &error),
        };
        let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
        if name != "need_context" && name != "prepare_change" && name != "verify_change" {
            return rpc_error(id, -32602, "unknown tool");
        }
        let Some(arguments) = params.get("arguments") else {
            return rpc_error(id, -32602, "missing arguments");
        };
        let encoded_bytes = match serde_json::to_vec(arguments) {
            Ok(encoded) => encoded.len(),
            Err(error) => return rpc_error(id, -32602, &format!("invalid arguments: {error}")),
        };
        if encoded_bytes > MAX_MCP_REQUEST_BYTES {
            return rpc_error(id, -32602, "arguments exceed the 16 KiB request bound");
        }
        if name == "prepare_change" {
            return self.prepare_change_call(id, arguments, encoded_bytes);
        }
        if name == "verify_change" {
            return self.verify_change_call(id, arguments);
        }
        let request = match serde_json::from_value::<McpNeedContextRequest>(arguments.clone()) {
            Ok(request) => request,
            Err(error) => return rpc_error(id, -32602, &format!("invalid arguments: {error}")),
        };
        let mapped = match request.validate_and_map(&self.enabled_routes, encoded_bytes) {
            Ok(mapped) => mapped,
            Err(error) => return rpc_error(id, -32602, &error),
        };
        let ordinal =
            match u8::try_from(self.ledger.len()).ok().and_then(|value| value.checked_add(1)) {
                Some(ordinal) => ordinal,
                None => {
                    return self.limit_bypass(id, mapped, u8::MAX, "need sequence overflow", None);
                }
            };
        if !self.policy.multi_need_enabled || ordinal > self.policy.max_needs_per_task {
            return self.limit_bypass(
                id,
                mapped,
                ordinal,
                "multi-need task limit reached; continue natively",
                None,
            );
        }
        let turn_id = format!("mcp-turn-{}", self.next_turn);
        self.next_turn = self.next_turn.saturating_add(1);
        if let Err(error) = self.resolver.store().record_user_prompt(
            &self.session_id,
            Some(&turn_id),
            &mapped.request.task,
            self.repository_root.to_str(),
        ) {
            return tool_error(id, &format!("cannot persist MCP turn: {error}"));
        }
        let resolve_request = ResolveRequest {
            session_id: self.session_id.clone(),
            turn_id: turn_id.clone(),
            platform: "codex".to_owned(),
            main_model: self.main_model.clone(),
            cwd: self.repository_root.clone(),
            need: mapped.compatibility_request.clone(),
            need_ir: Some(mapped.need_ir.clone()),
            declared_test_plan: None,
        };
        let outcome = if self.workers_started >= self.policy.max_workers_per_task {
            self.resolver.resolve_semantic_required_cache_only(&resolve_request)
        } else if self.calibration_reuse {
            self.resolver.resolve_semantic_required_calibration(&resolve_request)
        } else {
            self.resolver.resolve_semantic_required(&resolve_request)
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) if self.cancellation.load(Ordering::Acquire) => {
                return self.cancelled_failure(id, &mapped, ordinal, &turn_id, &error);
            }
            Err(error) if error.contains("cache-only resolution") => {
                return self.limit_bypass(
                    id,
                    mapped,
                    ordinal,
                    "worker limit or cache-only policy requires native continuation",
                    Some(turn_id),
                );
            }
            Err(error) if error.contains("semantic-required resolution failed") => {
                return rpc_error(id, -32602, &error);
            }
            Err(error) => return tool_error(id, &error),
        };
        if outcome.worker_spawned {
            self.workers_started = self.workers_started.saturating_add(1);
        }
        let Some(need) = outcome.compiled_need.clone() else {
            return tool_error(id, "semantic resolver returned no compiled Need");
        };
        let relation = relation_against_ledger(&self.ledger, &need);
        let unsafe_resolution = matches!(
            outcome.cache_resolution,
            CacheResolution::Stale { .. }
                | CacheResolution::Contradicted { .. }
                | CacheResolution::Ambiguous { .. }
                | CacheResolution::Rejected { .. }
        );
        let delivered = !unsafe_resolution && (outcome.cache_hit || outcome.worker_spawned);
        let satisfied = if delivered {
            need.required.iter().map(|obligation| obligation.id).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let missing = need
            .required
            .iter()
            .filter(|obligation| !satisfied.contains(&obligation.id))
            .map(|obligation| obligation.id)
            .collect::<Vec<_>>();
        let artifacts = outcome.semantic_artifact_ids.clone();
        let proof = resolution_proof(&outcome.cache_resolution);
        let step_id = need_step_id(&self.session_id, ordinal, &turn_id, mapped.request_digest);
        let step = NeedStep {
            id: step_id,
            ordinal,
            turn_id: turn_id.clone(),
            need_id: need.id,
            coordination: NeedCoordination::WaitResponse,
            relation,
            state: NeedStepState::Requested,
            required: need.required.iter().map(|obligation| obligation.id).collect(),
            satisfied: satisfied.clone(),
            missing: missing.clone(),
            artifacts: artifacts.clone(),
            proof,
            delivery: Some(NeedDelivery::TurnStart),
            worker_avoided: outcome.cache_hit && !outcome.worker_spawned,
            main_discovery_tainted: false,
        };
        if let Err(error) = persist_step(
            self.resolver.store(),
            &self.session_id,
            &step,
            mapped.request_digest,
            &mapped.canonical_json,
            &mapped.need_ir,
        ) {
            return tool_error(id, &format!("cannot persist MCP need step: {error}"));
        }
        self.ledger.push(LedgerEntry { need: need.clone(), satisfied });
        let context =
            if unsafe_resolution { String::new() } else { bounded_context(&outcome.rendered) };
        let response =
            response_from_outcome(&mapped.request, &need, ordinal, relation, &outcome, context);
        if let Err(error) = record_observation(
            self.resolver.data_directory(),
            &turn_id,
            mapped.request_digest,
            &mapped.request.route,
            relation,
            &outcome,
        ) {
            return tool_error(id, &format!("cannot persist MCP observation: {error}"));
        }
        self.tool_success(id, &turn_id, mapped.request_digest, response)
    }

    fn prepare_change_call(&self, id: Value, arguments: &Value, encoded_bytes: usize) -> Value {
        if !self.changes_enabled {
            return rpc_error(id, -32602, "prepare_change is disabled in this session");
        }
        let request = match serde_json::from_value::<McpPrepareChangeRequest>(arguments.clone()) {
            Ok(request) => request,
            Err(error) => return rpc_error(id, -32602, &format!("invalid arguments: {error}")),
        };
        let request = match request.validate_and_map(encoded_bytes) {
            Ok(request) => request,
            Err(error) => return rpc_error(id, -32602, &error),
        };
        let context = match self.change_context(&request) {
            Ok(context) => context,
            Err(error) => return rpc_error(id, -32602, &error),
        };
        let settings = match self.resolver.store().settings() {
            Ok(settings) => settings,
            Err(error) => return tool_error(id, &format!("cannot load worker settings: {error}")),
        };
        let patcher = CodexPatchWorker::new(self.resolver.data_directory())
            .with_cancellation(Arc::clone(&self.cancellation));
        let outcome = match patcher.prepare(
            &settings.worker_config(),
            &self.repository_root,
            &request,
            &context,
        ) {
            Ok(outcome) => outcome,
            Err(error) => return tool_error(id, &error),
        };
        let request_digest = outcome.request_digest;
        let response = McpPrepareChangeResponse::from_outcome(
            outcome,
            request.artifact_ids.len(),
            request.claim_ids.len(),
        );
        let context = bounded_context(&response.context());
        let structured = serde_json::to_value(response).expect("prepare-change serialization");
        self.tool_success_value(id, &request_digest.to_hex(), request_digest, structured, context)
    }

    fn change_context(
        &self,
        request: &needle_core::ChangeRequest,
    ) -> Result<Vec<PatchContextItem>, String> {
        let (_, current_snapshot) = needle_runtime::capture_git_snapshot(&self.repository_root)
            .map_err(|error| format!("cannot capture current change snapshot: {error}"))?;
        let mut context = Vec::with_capacity(request.artifact_ids.len() + request.claim_ids.len());
        for id in &request.artifact_ids {
            let artifact = self
                .resolver
                .store()
                .semantic_artifact(&id.to_string())
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("artifact `{id}` does not exist"))?;
            let certificate = self
                .resolver
                .store()
                .validation_certificate_for_artifact(&id.to_string())
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("artifact `{id}` has no validation certificate"))?;
            if !artifact_and_certificate_are_fresh(&artifact, &certificate, &self.repository_root) {
                return Err(format!("artifact `{id}` is stale or invalid"));
            }
            if artifact.contract.cache_scope == needle_core::CacheScope::SnapshotExact
                && self
                    .resolver
                    .store()
                    .semantic_artifact_source_digest(*id)
                    .map_err(|error| error.to_string())?
                    != Some(current_snapshot.source_digest)
            {
                return Err(format!("snapshot-exact artifact `{id}` belongs to another snapshot"));
            }
            for entry in &certificate.coverage.entries {
                let subject = self
                    .resolver
                    .store()
                    .subject(entry.obligation.subject)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| format!("artifact `{id}` has an unknown subject"))?;
                if subject.repository_lineage != self.repository_lineage {
                    return Err(format!("artifact `{id}` belongs to another repository"));
                }
            }
            let content = serde_json::to_string(&json!({
                "artifact_id": id,
                "kind": artifact.contract.kind,
                "payload": artifact.payload
            }))
            .map_err(|error| error.to_string())?;
            if content.len() > 8 * 1024 {
                return Err(format!("artifact `{id}` exceeds the bounded patch context"));
            }
            context.push(PatchContextItem { label: format!("artifact:{id}"), content });
        }
        for id in &request.claim_ids {
            let claim = self
                .resolver
                .store()
                .semantic_claim(*id)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("claim `{id}` does not exist"))?;
            let certificate = self
                .resolver
                .store()
                .claim_validation_certificate_for_claim(*id)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("claim `{id}` has no validation certificate"))?;
            if certificate.claim != *id
                || !claim.is_canonical()
                || !certificate.is_canonical()
                || !claim_validation_certificate_is_fresh(&certificate, &self.repository_root)
            {
                return Err(format!("claim `{id}` is stale or invalid"));
            }
            let subject = self
                .resolver
                .store()
                .subject(certificate.subject)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("claim `{id}` has an unknown subject"))?;
            if subject.repository_lineage != self.repository_lineage {
                return Err(format!("claim `{id}` belongs to another repository"));
            }
            let content = serde_json::to_string(&json!({
                "claim_id": id,
                "kind": claim.kind,
                "payload": claim.payload
            }))
            .map_err(|error| error.to_string())?;
            if content.len() > 8 * 1024 {
                return Err(format!("claim `{id}` exceeds the bounded patch context"));
            }
            context.push(PatchContextItem { label: format!("claim:{id}"), content });
        }
        Ok(context)
    }

    fn verify_change_call(&self, id: Value, arguments: &Value) -> Value {
        if !self.changes_enabled {
            return rpc_error(id, -32602, "verify_change is disabled in this session");
        }
        let request = match serde_json::from_value::<McpVerifyChangeRequest>(arguments.clone()) {
            Ok(request) => request,
            Err(error) => return rpc_error(id, -32602, &format!("invalid arguments: {error}")),
        };
        let request_digest = request.digest();
        let settings = match self.resolver.store().settings() {
            Ok(settings) => settings,
            Err(error) => {
                return tool_error(id, &format!("cannot load verifier settings: {error}"));
            }
        };
        let verifier = CodexVerifier::new(self.resolver.data_directory())
            .with_cancellation(Arc::clone(&self.cancellation));
        let worker_config = settings.worker_config();
        let first = match verifier.verify(&worker_config, &self.repository_root, &request.change_id)
        {
            Ok(outcome) => outcome,
            Err(error) => return tool_error(id, &error),
        };
        let mut outcome = first;
        let mut repair_attempted = false;
        let mut repair_performed = false;
        let mut verification_attempts = 1_u8;
        if outcome.artifact.verdict == VerificationStatus::Repairable {
            repair_attempted = true;
            let patcher = CodexPatchWorker::new(self.resolver.data_directory())
                .with_cancellation(Arc::clone(&self.cancellation));
            match patcher.repair(&worker_config, &self.repository_root, &request.change_id) {
                Ok(_) => {
                    repair_performed = true;
                    verification_attempts = 2;
                    outcome = match verifier.verify(
                        &worker_config,
                        &self.repository_root,
                        &request.change_id,
                    ) {
                        Ok(outcome) => outcome,
                        Err(error) => match verifier.record_inconclusive(
                            &request.change_id,
                            &format!("verification after the one-shot repair failed: {error}"),
                        ) {
                            Ok(outcome) => outcome,
                            Err(record_error) => return tool_error(id, &record_error),
                        },
                    };
                }
                Err(error) => {
                    outcome = match verifier.record_inconclusive(
                        &request.change_id,
                        &format!("one-shot repair failed: {error}"),
                    ) {
                        Ok(outcome) => outcome,
                        Err(record_error) => return tool_error(id, &record_error),
                    };
                }
            }
        }
        let response = McpVerifyChangeResponse::from_outcome(
            outcome,
            repair_attempted,
            repair_performed,
            verification_attempts,
        );
        let context = bounded_context(&response.context());
        let structured = serde_json::to_value(response).expect("verify-change serialization");
        self.tool_success_value(id, request.change_id.as_str(), request_digest, structured, context)
    }

    fn cancelled_failure(
        &mut self,
        id: Value,
        mapped: &schema::MappedNeedContext,
        ordinal: u8,
        turn_id: &str,
        diagnostic: &str,
    ) -> Value {
        let Some(contract) = needle_core::built_in_route_contracts()
            .into_iter()
            .find(|contract| contract.route.as_str() == mapped.request.route)
        else {
            return tool_error(id, "cancelled request has no semantic route contract");
        };
        let need =
            match needle_core::compile_need(&mapped.need_ir, self.repository_lineage, &contract) {
                Ok(need) => need,
                Err(error) => {
                    return tool_error(id, &format!("cancelled request is invalid: {error}"));
                }
            };
        let relation = relation_against_ledger(&self.ledger, &need);
        let step = NeedStep {
            id: need_step_id(&self.session_id, ordinal, turn_id, mapped.request_digest),
            ordinal,
            turn_id: turn_id.to_owned(),
            need_id: need.id,
            coordination: NeedCoordination::WaitResponse,
            relation,
            state: NeedStepState::Requested,
            required: need.required.iter().map(|item| item.id).collect(),
            satisfied: Vec::new(),
            missing: need.required.iter().map(|item| item.id).collect(),
            artifacts: Vec::new(),
            proof: None,
            delivery: None,
            worker_avoided: false,
            main_discovery_tainted: false,
        };
        let persisted = self
            .resolver
            .store()
            .record_mcp_need_step(
                &self.session_id,
                &step,
                mapped.request_digest,
                &mapped.canonical_json,
                &mapped.need_ir,
            )
            .and_then(|_| {
                self.resolver.store().append_need_step_event(
                    step.id,
                    NeedStepState::Resolving,
                    "{}",
                )
            })
            .and_then(|_| {
                self.resolver.store().append_need_step_event(
                    step.id,
                    NeedStepState::Cancelled,
                    "{}",
                )
            });
        if let Err(error) = persisted {
            return tool_error(id, &format!("cancelled request audit failed: {error}"));
        }
        tool_error(id, &format!("request cancelled; resolver cleanup: {diagnostic}"))
    }

    fn limit_bypass(
        &mut self,
        id: Value,
        mapped: schema::MappedNeedContext,
        ordinal: u8,
        reason: &str,
        turn_id: Option<String>,
    ) -> Value {
        let contract = needle_core::built_in_route_contracts()
            .into_iter()
            .find(|contract| contract.route.as_str() == mapped.request.route);
        let Some(contract) = contract else {
            return rpc_error(id, -32602, "selected route has no semantic contract");
        };
        let need =
            match needle_core::compile_need(&mapped.need_ir, self.repository_lineage, &contract) {
                Ok(need) => need,
                Err(error) => return rpc_error(id, -32602, &error.to_string()),
            };
        let turn_id = turn_id.unwrap_or_else(|| {
            let turn_id = format!("mcp-turn-{}", self.next_turn);
            self.next_turn = self.next_turn.saturating_add(1);
            turn_id
        });
        if let Err(error) = self.resolver.store().record_user_prompt(
            &self.session_id,
            Some(&turn_id),
            &mapped.request.task,
            self.repository_root.to_str(),
        ) {
            return tool_error(id, &format!("cannot persist MCP bypass turn: {error}"));
        }
        let relation = relation_against_ledger(&self.ledger, &need);
        let missing_ids = need.required.iter().map(|item| item.id).collect::<Vec<_>>();
        let step = NeedStep {
            id: need_step_id(&self.session_id, ordinal, &turn_id, mapped.request_digest),
            ordinal,
            turn_id: turn_id.clone(),
            need_id: need.id,
            coordination: NeedCoordination::WaitResponse,
            relation,
            state: NeedStepState::Requested,
            required: missing_ids.clone(),
            satisfied: Vec::new(),
            missing: missing_ids,
            artifacts: Vec::new(),
            proof: None,
            delivery: Some(NeedDelivery::NativeFallback),
            worker_avoided: false,
            main_discovery_tainted: false,
        };
        if let Err(error) = self.resolver.store().record_mcp_need_step(
            &self.session_id,
            &step,
            mapped.request_digest,
            &mapped.canonical_json,
            &mapped.need_ir,
        ) {
            return tool_error(id, &format!("cannot persist MCP bypass step: {error}"));
        }
        if let Err(error) = self
            .resolver
            .store()
            .append_need_step_event(step.id, NeedStepState::NativeFallback, "{}")
            .and_then(|_| {
                self.resolver.store().append_need_step_event(
                    step.id,
                    NeedStepState::Delivered,
                    "{}",
                )
            })
        {
            return tool_error(id, &format!("cannot persist MCP bypass events: {error}"));
        }
        self.ledger.push(LedgerEntry { need: need.clone(), satisfied: Vec::new() });
        let context = format!("Needle bypassed this request: {reason}.");
        let response = McpNeedContextResponse {
            status: "bypass".to_owned(),
            route: mapped.request.route,
            subject: mapped.request.subject,
            need_id: need.id.to_string(),
            step: McpStep { ordinal: ordinal.min(8), relation: relation_name(relation).to_owned() },
            satisfied: Vec::new(),
            missing: mapped
                .need_ir
                .required
                .iter()
                .map(|obligation| obligation.predicate.as_str().replace('-', "_"))
                .collect(),
            resolution: McpResolution::Bypass { reason: reason.to_owned() },
            reuse_unit: "none".to_owned(),
            claim_ids: Vec::new(),
            cache_hit: false,
            worker_spawned: false,
            calibration: false,
            result_digest: Digest::blake3(context.as_bytes()).to_string(),
            context,
        };
        self.tool_success(id, &turn_id, mapped.request_digest, response)
    }

    fn tool_success(
        &self,
        id: Value,
        turn_id: &str,
        request_digest: Digest,
        response: McpNeedContextResponse,
    ) -> Value {
        let context = response.context.clone();
        let structured = serde_json::to_value(response).expect("MCP response serialization");
        self.tool_success_value(id, turn_id, request_digest, structured, context)
    }

    fn tool_success_value(
        &self,
        id: Value,
        turn_id: &str,
        request_digest: Digest,
        structured: Value,
        context: String,
    ) -> Value {
        let mut result = Map::new();
        result.insert("content".to_owned(), json!([{"type": "text", "text": context}]));
        result.insert("isError".to_owned(), json!(false));
        if self.protocol == Some(NegotiatedProtocol::Modern) {
            result.insert("structuredContent".to_owned(), structured);
        }
        result.insert(
            "_meta".to_owned(),
            json!({
                "interface_digest": self.transport_definition_digest,
                "session_digest": Digest::blake3(self.session_id.as_bytes()),
                "turn_digest": Digest::blake3(turn_id.as_bytes()),
                "request_digest": request_digest
            }),
        );
        rpc_result(id, Value::Object(result))
    }
}

fn response_from_outcome(
    request: &McpNeedContextRequest,
    need: &Need,
    ordinal: u8,
    relation: NeedStepRelation,
    outcome: &ResolveOutcome,
    context: String,
) -> McpNeedContextResponse {
    let delivered = !context.is_empty();
    let mut capabilities = need
        .required
        .iter()
        .map(|obligation| capability_name(obligation.predicate).to_owned())
        .collect::<Vec<_>>();
    capabilities.sort();
    capabilities.dedup();
    let (reuse_unit, claim_ids) = match &outcome.cache_resolution {
        CacheResolution::ClaimHit { claim_ids, .. }
        | CacheResolution::ClaimCompositeHit { claim_ids, .. } => {
            ("claim", claim_ids.iter().map(ToString::to_string).collect::<Vec<_>>())
        }
        CacheResolution::PartialHit { reused_claim_ids, .. } if !reused_claim_ids.is_empty() => {
            ("claim", reused_claim_ids.iter().map(ToString::to_string).collect::<Vec<_>>())
        }
        CacheResolution::ExactHit { .. }
        | CacheResolution::CoverageHit { .. }
        | CacheResolution::CompositeHit { .. }
        | CacheResolution::PartialHit { .. } => ("artifact", Vec::new()),
        _ => ("none", Vec::new()),
    };
    McpNeedContextResponse {
        status: if delivered {
            if outcome.cache_hit && !outcome.worker_spawned { "hit" } else { "generated" }
        } else {
            "bypass"
        }
        .to_owned(),
        route: request.route.clone(),
        subject: request.subject.clone(),
        need_id: need.id.to_string(),
        step: McpStep { ordinal, relation: relation_name(relation).to_owned() },
        satisfied: if delivered { capabilities.clone() } else { Vec::new() },
        missing: if delivered { Vec::new() } else { capabilities },
        resolution: McpResolution::from(&outcome.cache_resolution),
        reuse_unit: reuse_unit.to_owned(),
        claim_ids,
        cache_hit: outcome.cache_hit,
        worker_spawned: outcome.worker_spawned,
        calibration: outcome.calibration,
        result_digest: outcome.result_digest.to_string(),
        context,
    }
}

impl From<&CacheResolution> for McpResolution {
    fn from(value: &CacheResolution) -> Self {
        match value {
            CacheResolution::ExactHit {
                artifact_id,
                sufficiency_certificate_id,
                selected_plan_id,
                ..
            } => Self::ExactHit {
                artifact_ids: vec![artifact_id.to_string()],
                certificate_id: sufficiency_certificate_id.map(|value| value.to_string()),
                plan_id: selected_plan_id.map(|value| value.to_string()),
            },
            CacheResolution::CoverageHit {
                artifact_id,
                sufficiency_certificate_id,
                selected_plan_id,
                ..
            } => Self::CoverageHit {
                artifact_ids: vec![artifact_id.to_string()],
                certificate_id: sufficiency_certificate_id.to_string(),
                plan_id: selected_plan_id.to_string(),
            },
            CacheResolution::CompositeHit {
                artifact_ids,
                sufficiency_certificate_id,
                selected_plan_id,
                ..
            } => Self::CompositeHit {
                artifact_ids: artifact_ids.iter().map(ToString::to_string).collect(),
                certificate_id: sufficiency_certificate_id.map(|value| value.to_string()),
                plan_id: selected_plan_id.map(|value| value.to_string()),
            },
            CacheResolution::ClaimHit {
                artifact_ids,
                claim_ids,
                claim_set_certificate_id,
                selected_plan_id,
                ..
            } => Self::ClaimHit {
                artifact_ids: artifact_ids.iter().map(ToString::to_string).collect(),
                claim_ids: claim_ids.iter().map(ToString::to_string).collect(),
                claim_set_certificate_id: claim_set_certificate_id.to_string(),
                plan_id: selected_plan_id.to_string(),
            },
            CacheResolution::ClaimCompositeHit {
                artifact_ids,
                claim_ids,
                claim_set_certificate_id,
                selected_plan_id,
                ..
            } => Self::ClaimCompositeHit {
                artifact_ids: artifact_ids.iter().map(ToString::to_string).collect(),
                claim_ids: claim_ids.iter().map(ToString::to_string).collect(),
                claim_set_certificate_id: claim_set_certificate_id.to_string(),
                plan_id: selected_plan_id.to_string(),
            },
            CacheResolution::PartialHit {
                reused,
                reused_claim_ids,
                invalidated_nodes,
                selected_plan_id,
                ..
            } => Self::PartialHit {
                artifact_ids: reused.iter().map(ToString::to_string).collect(),
                claim_ids: reused_claim_ids.iter().map(ToString::to_string).collect(),
                invalidated_nodes: invalidated_nodes.clone(),
                plan_id: selected_plan_id.map(|value| value.to_string()),
            },
            CacheResolution::Miss => Self::Miss,
            CacheResolution::Stale { artifact_id, reason } => {
                Self::Stale { artifact_ids: vec![artifact_id.to_string()], reason: reason.clone() }
            }
            CacheResolution::Rejected { reason } => Self::Rejected { reason: reason.clone() },
            CacheResolution::Ambiguous { reason } => Self::Ambiguous { reason: reason.clone() },
            CacheResolution::Contradicted { reason } => {
                Self::Contradicted { reason: reason.clone() }
            }
            CacheResolution::Bypass { reason } => Self::Bypass { reason: reason.clone() },
        }
    }
}

fn persist_step(
    store: &needle_runtime::RuntimeStore,
    session_id: &str,
    step: &NeedStep,
    request_digest: Digest,
    canonical_json: &str,
    need_ir: &needle_core::NeedIr,
) -> Result<(), String> {
    store
        .record_mcp_need_step(session_id, step, request_digest, canonical_json, need_ir)
        .map_err(|error| error.to_string())?;
    store
        .append_need_step_event(step.id, NeedStepState::Resolving, "{}")
        .map_err(|error| error.to_string())?;
    store
        .append_need_step_event(step.id, NeedStepState::Resolved, "{}")
        .map_err(|error| error.to_string())?;
    for artifact in &step.artifacts {
        store
            .attach_need_step_artifact(step.id, *artifact, step.proof, "selected")
            .map_err(|error| error.to_string())?;
    }
    store
        .append_need_step_event(step.id, NeedStepState::Delivered, "{}")
        .map_err(|error| error.to_string())
}

fn relation_against_ledger(ledger: &[LedgerEntry], current: &Need) -> NeedStepRelation {
    let mut best = NeedStepRelation::Independent;
    for previous in ledger {
        let relation = classify_need_step(&previous.need, current, &previous.satisfied);
        if relation_rank(relation) > relation_rank(best) {
            best = relation;
        }
    }
    best
}

fn relation_rank(value: NeedStepRelation) -> u8 {
    match value {
        NeedStepRelation::Independent => 0,
        NeedStepRelation::Incompatible => 1,
        NeedStepRelation::Overlap => 2,
        NeedStepRelation::Extension => 3,
        NeedStepRelation::Residual => 4,
        NeedStepRelation::Repeat => 5,
    }
}

fn relation_name(value: NeedStepRelation) -> &'static str {
    match value {
        NeedStepRelation::Repeat => "repeat",
        NeedStepRelation::Residual => "residual",
        NeedStepRelation::Extension => "extension",
        NeedStepRelation::Overlap => "overlap",
        NeedStepRelation::Independent => "independent",
        NeedStepRelation::Incompatible => "incompatible",
    }
}

fn capability_name(value: PredicateKind) -> &'static str {
    match value {
        PredicateKind::ImplementationLocation => "implementation_location",
        PredicateKind::RuntimeFlow => "runtime_flow",
        PredicateKind::FocusedTests => "focused_tests",
    }
}

fn resolution_proof(resolution: &CacheResolution) -> Option<ReuseSufficiencyCertificateId> {
    match resolution {
        CacheResolution::ExactHit { sufficiency_certificate_id, .. }
        | CacheResolution::CompositeHit { sufficiency_certificate_id, .. } => {
            *sufficiency_certificate_id
        }
        CacheResolution::CoverageHit { sufficiency_certificate_id, .. } => {
            Some(*sufficiency_certificate_id)
        }
        CacheResolution::ClaimHit { .. } | CacheResolution::ClaimCompositeHit { .. } => None,
        _ => None,
    }
}

fn need_step_id(session_id: &str, ordinal: u8, turn_id: &str, request_digest: Digest) -> Digest {
    let mut hasher = CanonicalHasher::new(b"need-step");
    hasher.field_str(session_id);
    hasher.field_u8(ordinal);
    hasher.field_str(turn_id);
    hasher.field_digest(request_digest);
    hasher.finish()
}

fn bounded_context(value: &str) -> String {
    if value.chars().count() <= MAX_CONTEXT_CHARS {
        return value.to_owned();
    }
    value.chars().take(MAX_CONTEXT_CHARS).collect()
}

fn transport_definition_digest(routes: &[String], changes_enabled: bool) -> Digest {
    let mut hasher = CanonicalHasher::new(b"mcp-public-tools-interface");
    hasher.field_str(MODERN_PROTOCOL_VERSION);
    hasher.field_str(LEGACY_PROTOCOL_VERSION);
    hasher.field_str("need_context");
    hasher.field_str("json-claim-reuse-v2");
    hasher.field_u8(u8::from(changes_enabled));
    if changes_enabled {
        hasher.field_str("prepare_change");
        hasher.field_str("isolated-patch-v1");
        hasher.field_str("verify_change");
        hasher.field_str("independent-verifier-v1");
    }
    for route in routes {
        hasher.field_str(route);
    }
    hasher.finish()
}

fn mcp_session_id(repository_root: &Path) -> String {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    Digest::blake3(format!(
        "needle-mcp-session\n{}\n{}\n{nonce}",
        std::process::id(),
        repository_root.display()
    ))
    .to_hex()
}

fn record_observation(
    data_directory: &Path,
    turn_id: &str,
    request_digest: Digest,
    route: &str,
    relation: NeedStepRelation,
    outcome: &ResolveOutcome,
) -> Result<(), String> {
    let path = data_directory.join(OBSERVATION_FILE);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    serde_json::to_writer(
        &mut file,
        &json!({
            "schema": "needle.mcp-observation/3",
            "transport": "mcp",
            "request_format": "json",
            "turn_id": turn_id,
            "request_digest": request_digest,
            "route": route,
            "relation": relation_name(relation),
            "status": outcome.status,
            "cache_resolution": outcome.cache_resolution,
            "cache_hit": outcome.cache_hit,
            "worker_spawned": outcome.worker_spawned,
            "calibration": outcome.calibration,
            "result_digest": outcome.result_digest,
            "semantic_artifact_ids": outcome.semantic_artifact_ids,
        }),
    )
    .map_err(|error| error.to_string())?;
    file.write_all(b"\n").map_err(|error| error.to_string())
}

fn tool_error(id: Value, message: &str) -> Value {
    rpc_result(
        id,
        json!({
            "content": [{"type": "text", "text": message}],
            "isError": true
        }),
    )
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "result":result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use needle_core::{EvidenceFailurePolicy, MultiNeedPolicy};
    use needle_runtime::{RuntimeSettings, RuntimeStore};
    use std::process::Command;

    #[test]
    fn protocol_requires_initialize_and_exposes_closed_structured_schemas() {
        let (_root, mut server) = server_fixture();
        server.changes_enabled = true;
        let before = server.response(&json!({"jsonrpc":"2.0","id":0,"method":"tools/list"}));
        assert_eq!(before["error"]["code"], -32002);
        initialize(&mut server, MODERN_PROTOCOL_VERSION);
        let listed = server.response(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        let tool = &listed["result"]["tools"][0];
        assert_eq!(tool["name"], "need_context");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert_eq!(tool["outputSchema"]["additionalProperties"], false);
        let public_contract = format!("{MCP_SERVER_INSTRUCTIONS}{NEED_CONTEXT_DESCRIPTION}{tool}");
        assert!(!public_contract.contains("@@need"));
        assert!(tool["inputSchema"]["properties"].get("need").is_none());
        assert!(tool["inputSchema"]["properties"].get("request").is_none());
        let prepare = &listed["result"]["tools"][1];
        assert_eq!(prepare["name"], "prepare_change");
        assert_eq!(prepare["inputSchema"]["additionalProperties"], false);
        assert_eq!(prepare["outputSchema"]["additionalProperties"], false);
        assert_eq!(prepare["annotations"]["readOnlyHint"], false);
        assert!(!format!("{PREPARE_CHANGE_DESCRIPTION}{prepare}").contains("@@need"));
        let verify = &listed["result"]["tools"][2];
        assert_eq!(verify["name"], "verify_change");
        assert_eq!(verify["inputSchema"]["additionalProperties"], false);
        assert_eq!(verify["outputSchema"]["additionalProperties"], false);
        assert_eq!(verify["annotations"]["readOnlyHint"], true);
    }

    #[test]
    fn malformed_prepare_change_fails_before_starting_a_patch_worker() {
        let (_root, mut server) = server_fixture();
        server.changes_enabled = true;
        initialize(&mut server, MODERN_PROTOCOL_VERSION);
        let response = server.response(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "prepare_change",
                "arguments": {
                    "task": "Change the fixture.",
                    "acceptance_criteria": ["The fixture changes."],
                    "allowed_paths": [{"path": "../outside", "scope": "exact"}]
                }
            }
        }));
        assert_eq!(response["error"]["code"], -32602);
        assert!(
            server
                .resolver
                .store()
                .prepared_change(&needle_core::ChangeId::from_digest(Digest::blake3(b"absent")))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn malformed_json_arguments_fail_before_the_resolver() {
        let (_root, mut server) = server_fixture();
        initialize(&mut server, MODERN_PROTOCOL_VERSION);
        let response = server.response(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "need_context",
                "arguments": {
                    "route": "locate.implementation",
                    "subject": {"kind": "symbol", "name": "answer"},
                    "task": "Locate answer.",
                    "need": "legacy"
                }
            }
        }));
        assert_eq!(response["error"]["code"], -32602);
        assert!(server.ledger.is_empty());
    }

    #[test]
    fn tool_call_envelope_accepts_reserved_meta_only() {
        let accepted = json!({
            "name": "need_context",
            "arguments": {},
            "_meta": {"progressToken": 1}
        });
        assert!(tool_call_params(Some(&accepted)).is_ok());

        let unknown = json!({"name": "need_context", "arguments": {}, "trace": true});
        assert_eq!(
            tool_call_params(Some(&unknown)).unwrap_err(),
            "unknown tools/call parameter `trace`"
        );

        let malformed_meta = json!({"name": "need_context", "arguments": {}, "_meta": 1});
        assert_eq!(
            tool_call_params(Some(&malformed_meta)).unwrap_err(),
            "tools/call _meta must be an object"
        );
    }

    #[test]
    fn legacy_negotiation_keeps_the_json_input_and_text_fallback() {
        let (_root, mut server) = server_fixture();
        initialize(&mut server, LEGACY_PROTOCOL_VERSION);
        let listed = server.response(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
        assert!(listed["result"]["tools"][0].get("outputSchema").is_none());
        assert!(listed["result"]["tools"][0]["inputSchema"]["properties"]["route"].is_object());
    }

    #[test]
    fn duplicate_request_ids_are_rejected() {
        let (_root, mut server) = server_fixture();
        initialize(&mut server, MODERN_PROTOCOL_VERSION);
        let _ = server.response(&json!({"jsonrpc":"2.0","id":2,"method":"ping"}));
        let duplicate = server.response(&json!({"jsonrpc":"2.0","id":2,"method":"ping"}));
        assert_eq!(duplicate["error"]["code"], -32600);
    }

    #[test]
    fn cancellation_is_bound_to_the_active_json_rpc_id() {
        assert!(cancellation_targets(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#,
            &json!(7)
        ));
        assert!(!cancellation_targets(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":8}}"#,
            &json!(7)
        ));
    }

    #[test]
    fn cache_resolutions_map_to_the_public_tagged_union() {
        let artifact = Digest::blake3(b"artifact");
        let certificate = ReuseSufficiencyCertificateId(Digest::blake3(b"certificate"));
        let claim_certificate =
            needle_core::ClaimSetCertificateId(Digest::blake3(b"claim-certificate"));
        let claim = needle_core::ClaimId(Digest::blake3(b"claim"));
        let plan = needle_core::SelectedPlanId(Digest::blake3(b"plan"));
        let resolutions = [
            CacheResolution::ExactHit {
                artifact_id: artifact,
                sufficiency_certificate_id: Some(certificate),
                selected_plan_id: Some(plan),
                resolution_format_revision: Some(1),
            },
            CacheResolution::CoverageHit {
                artifact_id: artifact,
                sufficiency_certificate_id: certificate,
                selected_plan_id: plan,
                resolution_format_revision: 1,
            },
            CacheResolution::CompositeHit {
                artifact_ids: vec![artifact],
                sufficiency_certificate_id: Some(certificate),
                selected_plan_id: Some(plan),
                resolution_format_revision: Some(1),
            },
            CacheResolution::ClaimHit {
                artifact_ids: vec![artifact],
                claim_ids: vec![claim],
                claim_set_certificate_id: claim_certificate,
                selected_plan_id: plan,
                resolution_format_revision: 2,
            },
            CacheResolution::ClaimCompositeHit {
                artifact_ids: vec![artifact],
                claim_ids: vec![claim],
                claim_set_certificate_id: claim_certificate,
                selected_plan_id: plan,
                resolution_format_revision: 2,
            },
            CacheResolution::PartialHit {
                reused: vec![artifact],
                reused_claim_ids: vec![claim],
                invalidated_nodes: vec!["focused_tests".to_owned()],
                selected_plan_id: Some(plan),
                resolution_format_revision: Some(1),
            },
        ];
        let kinds = resolutions
            .iter()
            .map(|resolution| {
                serde_json::to_value(McpResolution::from(resolution)).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                "exact_hit",
                "coverage_hit",
                "composite_hit",
                "claim_hit",
                "claim_composite_hit",
                "partial_hit"
            ]
        );
        let claim_json = serde_json::to_value(McpResolution::from(&resolutions[3])).unwrap();
        assert_eq!(claim_json["claim_ids"], json!([claim.to_string()]));
        assert_eq!(claim_json["claim_set_certificate_id"], claim_certificate.to_string());
        let partial_json = serde_json::to_value(McpResolution::from(&resolutions[5])).unwrap();
        assert_eq!(partial_json["claim_ids"], json!([claim.to_string()]));
    }

    #[test]
    fn claim_hit_response_exposes_claim_reuse_without_worker_output() {
        let request: McpNeedContextRequest = serde_json::from_value(json!({
            "route": "locate.implementation",
            "subject": {"kind": "symbol", "name": "answer"},
            "required": [{
                "kind": "implementation_location",
                "polarity": "positive",
                "selection": "primary",
                "granularity": "exact_location"
            }],
            "preferred": [],
            "world": {"source": "current", "platform": "current", "features": "default"},
            "task": "Locate answer."
        }))
        .unwrap();
        let mapped =
            request.clone().validate_and_map(&["locate.implementation".to_owned()], 512).unwrap();
        let route = needle_core::built_in_route_contracts()
            .into_iter()
            .find(|contract| contract.route.as_str() == "locate.implementation")
            .unwrap();
        let need =
            needle_core::compile_need(&mapped.need_ir, Digest::blake3(b"repository"), &route)
                .unwrap();
        let artifact = Digest::blake3(b"artifact");
        let claim = needle_core::ClaimId(Digest::blake3(b"claim"));
        let mut outcome = ResolveOutcome {
            status: "hit".to_owned(),
            cache_resolution: CacheResolution::ClaimHit {
                artifact_ids: vec![artifact],
                claim_ids: vec![claim],
                claim_set_certificate_id: needle_core::ClaimSetCertificateId(Digest::blake3(
                    b"claim-certificate",
                )),
                selected_plan_id: needle_core::SelectedPlanId(Digest::blake3(b"plan")),
                resolution_format_revision: 2,
            },
            rendered: "bounded claim context".to_owned(),
            cache_hit: true,
            worker_spawned: false,
            calibration: false,
            result_digest: Digest::blake3(b"result"),
            semantic_artifact_ids: Vec::new(),
            compiled_need: Some(need.clone()),
        };
        let response = response_from_outcome(
            &request,
            &need,
            1,
            NeedStepRelation::Independent,
            &outcome,
            outcome.rendered.clone(),
        );
        assert_eq!(response.status, "hit");
        assert_eq!(response.reuse_unit, "claim");
        assert_eq!(response.claim_ids, vec![claim.to_string()]);
        assert!(!response.worker_spawned);
        assert_eq!(response.context, "bounded claim context");

        outcome.cache_resolution = CacheResolution::PartialHit {
            reused: vec![artifact],
            reused_claim_ids: vec![claim],
            invalidated_nodes: vec!["runtime-flow".to_owned()],
            selected_plan_id: Some(needle_core::SelectedPlanId(Digest::blake3(b"partial-plan"))),
            resolution_format_revision: Some(2),
        };
        outcome.worker_spawned = true;
        let partial = response_from_outcome(
            &request,
            &need,
            2,
            NeedStepRelation::Extension,
            &outcome,
            outcome.rendered.clone(),
        );
        assert_eq!(partial.reuse_unit, "claim");
        assert_eq!(partial.claim_ids, vec![claim.to_string()]);
        assert!(partial.worker_spawned);
    }

    #[test]
    fn structured_call_persists_an_mcp_need_step_and_returns_a_normal_cache_only_bypass() {
        let (root, mut server) = server_fixture();
        initialize(&mut server, MODERN_PROTOCOL_VERSION);
        let response = server.response(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "need_context",
                "arguments": {
                    "route": "locate.implementation",
                    "subject": {"kind": "symbol", "name": "answer"},
                    "required": [],
                    "preferred": [],
                    "world": {"source": "current", "platform": "current", "features": "default"},
                    "task": "Locate answer."
                }
            }
        }));
        assert_eq!(response["result"]["isError"], false);
        assert_eq!(response["result"]["structuredContent"]["status"], "bypass");
        assert_eq!(
            response["result"]["content"][0]["text"],
            response["result"]["structuredContent"]["context"]
        );
        assert_eq!(server.ledger.len(), 1);
        let steps = server.resolver.store().need_steps(&server.session_id).unwrap();
        assert_eq!(steps.len(), 1);
        let stored = server.resolver.store().need_step_request(steps[0].id).unwrap().unwrap();
        assert_eq!(stored.transport.as_deref(), Some("mcp"));
        assert_eq!(stored.request_format.as_deref(), Some("json"));
        assert!(stored.semantic_interrupt.is_none());
        assert!(stored.need_ir.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn three_json_calls_share_one_ledger_and_the_fourth_bypasses_the_limit() {
        let (root, mut server) = server_fixture();
        initialize(&mut server, MODERN_PROTOCOL_VERSION);
        let call = |id: u8, route: &str, capability: &str| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "need_context",
                    "arguments": {
                        "route": route,
                        "subject": {"kind": "symbol", "name": "answer"},
                        "required": [{"kind": capability}],
                        "preferred": [],
                        "task": "Find the evidence for answer."
                    }
                }
            })
        };
        let first = server.response(&call(2, "locate.implementation", "implementation_location"));
        let second = server.response(&call(3, "tests.relevant", "focused_tests"));
        let third = server.response(&call(4, "locate.implementation", "implementation_location"));
        let fourth = server.response(&call(5, "tests.relevant", "focused_tests"));
        assert_eq!(first["result"]["structuredContent"]["step"]["ordinal"], 1);
        assert_eq!(second["result"]["structuredContent"]["step"]["ordinal"], 2);
        assert_eq!(third["result"]["structuredContent"]["step"]["relation"], "repeat");
        assert_eq!(fourth["result"]["structuredContent"]["status"], "bypass");
        assert!(
            fourth["result"]["structuredContent"]["resolution"]["reason"]
                .as_str()
                .unwrap()
                .contains("limit")
        );
        assert_eq!(server.resolver.store().need_steps(&server.session_id).unwrap().len(), 4);
        let _ = fs::remove_dir_all(root);
    }

    fn initialize(server: &mut ProductMcpServer, version: &str) {
        let response = server.response(&json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize",
            "params":{"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"test","version":"1"}}
        }));
        assert_eq!(response["result"]["protocolVersion"], version);
        assert!(
            server
                .response(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .is_null()
        );
    }

    fn server_fixture() -> (PathBuf, ProductMcpServer) {
        let root = temporary_root();
        let repository = root.join("repo");
        let data_directory = root.join("data");
        fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "--quiet"]);
        git(&repository, &["config", "user.email", "needle@example.invalid"]);
        git(&repository, &["config", "user.name", "Needle Test"]);
        fs::write(repository.join("fixture.txt"), "fixture\n").unwrap();
        git(&repository, &["add", "fixture.txt"]);
        git(&repository, &["commit", "--quiet", "-m", "fixture"]);
        let store = RuntimeStore::new(data_directory.join("needle.sqlite3"));
        store
            .initialize_defaults(&RuntimeSettings {
                codex_executable: "codex".to_owned(),
                worker_model: "worker".to_owned(),
                worker_reasoning: "medium".to_owned(),
                worker_timeout_seconds: 5,
                evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
                trusted_test_execution: false,
                multi_need_policy: MultiNeedPolicy::default(),
            })
            .unwrap();
        let server = ProductMcpServer::new(ProductMcpConfig {
            data_directory,
            repository_root: repository,
            main_model: "main".to_owned(),
            cache_only: true,
            calibration_reuse: false,
        })
        .unwrap();
        (root, server)
    }

    fn temporary_root() -> PathBuf {
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        std::env::temp_dir().join(format!("needle-product-mcp-{}-{suffix}", std::process::id()))
    }

    fn git(repository: &Path, arguments: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(repository)
                .args(arguments)
                .status()
                .unwrap()
                .success()
        );
    }
}
