use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use needle_core::{
    ApprovalDecision, ApprovalDecisionSource, CapabilityMode, ChangeId, Digest,
    EvidenceFailurePolicy, ModelPolicy, PatchOperation, WorkerProfile,
    built_in_predicate_contracts, built_in_route_contracts, built_in_route_plans,
};
use needle_platform_codex::CodexWorker;
use needle_runtime::{
    ActivationRecord, ActivationScope, ChangeApplyError, PreparedChangeRecord, ProofCandidate,
    ProofPlanner, RuntimeSettings, RuntimeStore, StoreError, apply_verified_change,
    artifact_and_certificate_are_fresh, built_in_routes, recover_pending_change_applies,
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::runtime_instance::InstanceGuard;

#[path = "server/lifecycles.rs"]
mod lifecycles;
#[path = "server/role_profiles.rs"]
mod role_profiles;

#[derive(RustEmbed)]
#[folder = "web/dist"]
struct WebAssets;

#[derive(Clone)]
struct AppState {
    store: RuntimeStore,
    authority: String,
    launch_token: String,
    session_token: String,
    csrf_token: String,
    ipc_endpoint: String,
    repository_root: PathBuf,
    apply_lock: Arc<tokio::sync::Mutex<()>>,
    launch_consumed: Arc<Mutex<bool>>,
}

#[derive(Debug, Deserialize)]
struct LaunchQuery {
    launch_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApprovalQuery {
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NeedStepQuery {
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NeedStepEventsQuery {
    session_id: Option<String>,
    after: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionBody {
    decision: ApprovalDecision,
    payload_digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteStateBody {
    enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsBody {
    worker_model: String,
    worker_reasoning: String,
    worker_timeout_seconds: u64,
    evidence_failure_policy: EvidenceFailurePolicy,
    trusted_test_execution: bool,
    multi_need_enabled: bool,
    continue_working_enabled: bool,
    max_needs_per_task: u8,
    max_workers_per_task: u8,
    pending_main_tools: needle_core::PendingMainTools,
    resolver_concurrency: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelProfileBody {
    model: String,
    reasoning: String,
    service_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilityModeBody {
    mode: CapabilityMode,
    evidence_digest: Option<String>,
    confirm: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyChangeBody {
    confirm: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivationBody {
    enabled: bool,
    expected_state_digest: Option<Digest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
enum ModelPolicyBody {
    FixedOrder { profiles: Vec<ModelProfileBody>, repair_once: bool, native_fallback: bool },
    CheapestValidatedFirst { profiles: Vec<ModelProfileBody>, native_fallback: bool },
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IfMatchError {
    Missing,
    Changed,
}

pub(crate) fn run(
    data_directory: PathBuf,
    repository_root: PathBuf,
    open_browser: bool,
) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    runtime.block_on(async move {
        let repository_root = std::fs::canonicalize(repository_root)
            .map_err(|error| format!("repository root is unavailable: {error}"))?;
        let instance = InstanceGuard::acquire(&data_directory)?;
        let store = RuntimeStore::new(data_directory.join("needle.sqlite3"));
        store.initialize().map_err(|error| error.to_string())?;
        recover_pending_change_applies(&store, &repository_root)
            .map_err(|error| format!("pending change recovery failed: {error}"))?;
        match store.settings() {
            Ok(_) => {}
            Err(needle_runtime::StoreError::MissingSetting(_)) => store
                .initialize_defaults(&RuntimeSettings {
                    codex_executable: "codex".to_owned(),
                    worker_model: "gpt-5.6-luna".to_owned(),
                    worker_reasoning: "medium".to_owned(),
                    worker_timeout_seconds: 180,
                    evidence_failure_policy: EvidenceFailurePolicy::DiscardInvalidFact,
                    trusted_test_execution: false,
                    multi_need_policy: needle_core::MultiNeedPolicy::default(),
                })
                .map_err(|error| error.to_string())?,
            Err(error) => return Err(error.to_string()),
        }
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .map_err(|error| error.to_string())?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        let authority = address.to_string();
        let ipc_endpoint = crate::runtime_instance::endpoint(&data_directory);
        let ipc_token = random_token()?;
        let mut ipc_task = tokio::spawn(crate::runtime_instance::serve_ipc(
            ipc_endpoint.clone(),
            ipc_token.clone(),
            store.clone(),
            data_directory.clone(),
        ));
        if let Err(error) =
            crate::runtime_instance::wait_until_ready(&ipc_endpoint, &ipc_token).await
        {
            ipc_task.abort();
            return Err(format!("resident runtime IPC health handshake failed: {error}"));
        }
        instance.publish(&authority, &ipc_endpoint, &ipc_token)?;
        let state = AppState {
            store,
            authority: authority.clone(),
            launch_token: random_token()?,
            session_token: random_token()?,
            csrf_token: random_token()?,
            ipc_endpoint,
            repository_root,
            apply_lock: Arc::new(tokio::sync::Mutex::new(())),
            launch_consumed: Arc::new(Mutex::new(false)),
        };
        let app = Router::new()
            .route("/", get(index))
            .route("/health", get(health))
            .route("/api/v1/approvals", get(list_approvals))
            .route("/api/v1/approvals/events", get(approval_events))
            .route("/api/v1/approvals/{id}/decision", post(decide_approval))
            .route("/api/v1/routes/{id}/state", post(set_route_state))
            .route("/api/v1/settings", post(set_settings))
            .route("/api/v1/model-policy", post(set_model_policy))
            .route("/api/v1/needs", get(list_needs))
            .route("/api/v1/needs/{id}", get(get_need))
            .route("/api/v1/need-steps", get(list_need_steps))
            .route("/api/v1/need-steps/events", get(need_step_events))
            .route("/api/v1/need-steps/{id}", get(get_need_step))
            .route("/api/v1/subjects", get(list_subjects))
            .route("/api/v1/contracts", get(list_contracts))
            .route("/api/v1/plans/{id}", get(get_plan))
            .route("/api/v1/proofs/{id}", get(get_proof))
            .route("/api/v1/proofs/{id}/replay", post(replay_proof))
            .route("/api/v1/capabilities", get(list_capabilities))
            .route("/api/v1/capabilities/{id}/mode", post(set_capability_mode))
            .route("/api/v1/changes", get(list_changes))
            .route("/api/v1/changes/{id}", get(get_change))
            .route("/api/v1/changes/{id}/diff", get(get_change_diff))
            .route("/api/v1/changes/{id}/apply", post(apply_change))
            .route("/api/v1/activation", post(set_activation))
            .route("/api/v1/control-plane", get(control_plane));
        let app = lifecycles::routes(role_profiles::routes(app))
            .fallback(get(static_asset))
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(state.clone(), security));
        let launch_url = format!("http://{authority}/?launch_token={}", state.launch_token);
        println!("Needle control plane: {launch_url}");
        if open_browser && let Err(error) = open_control_plane(&launch_url) {
            eprintln!("needle: cannot open the default browser ({error}); open the URL above");
        }
        let http = async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await
                .map_err(|error| error.to_string())
        };
        tokio::pin!(http);
        let result = tokio::select! {
            result = &mut http => result,
            result = &mut ipc_task => match result {
                Ok(Ok(())) => Err("resident runtime IPC exited unexpectedly".to_owned()),
                Ok(Err(error)) => Err(format!("resident runtime IPC failed: {error}")),
                Err(error) => Err(format!("resident runtime IPC task failed: {error}")),
            },
        };
        ipc_task.abort();
        result
    })
}

fn open_control_plane(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    command.spawn().map(|_| ()).map_err(|error| error.to_string())
}

async fn index(State(state): State<AppState>, Query(query): Query<LaunchQuery>) -> Response {
    let valid = query.launch_token.as_deref() == Some(state.launch_token.as_str());
    if !consume_launch_token(&state.launch_consumed, valid) {
        return api_error(StatusCode::UNAUTHORIZED, "invalid or consumed launch token");
    }
    let mut response = embedded_index(&state.csrf_token);
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "needle_session={}; HttpOnly; SameSite=Strict; Path=/",
            state.session_token
        ))
        .expect("random token is a valid header value"),
    );
    response
}

fn consume_launch_token(consumed: &Mutex<bool>, token_matches: bool) -> bool {
    let Ok(mut consumed) = consumed.lock() else {
        return false;
    };
    if !token_matches || *consumed {
        return false;
    }
    *consumed = true;
    true
}

async fn static_asset(State(state): State<AppState>, request: Request) -> Response {
    let path = request.uri().path().trim_start_matches('/');
    if path.is_empty() || !path.contains('.') {
        return embedded_index(&state.csrf_token);
    }
    match WebAssets::get(path) {
        Some(asset) => {
            let content_type = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, content_type.as_ref())], Body::from(asset.data))
                .into_response()
        }
        None => api_error(StatusCode::NOT_FOUND, "asset not found"),
    }
}

fn embedded_index(csrf_token: &str) -> Response {
    let Some(asset) = WebAssets::get("index.html") else {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "embedded web bundle is missing");
    };
    let Ok(template) = std::str::from_utf8(asset.data.as_ref()) else {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "embedded index is not UTF-8");
    };
    Html(template.replace("__NEEDLE_CSRF__", csrf_token)).into_response()
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok", "schema": "needle.runtime-health/1"}))
}

async fn list_approvals(
    State(state): State<AppState>,
    Query(query): Query<ApprovalQuery>,
) -> Response {
    if query.status.as_deref().unwrap_or("pending") != "pending" {
        return api_error(StatusCode::BAD_REQUEST, "only status=pending is supported");
    }
    match state.store.pending_approvals() {
        Ok(approvals) => Json(approvals).into_response(),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn control_plane(State(state): State<AppState>) -> Response {
    let mut routes = match state.store.routes() {
        Ok(routes) => routes,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    if routes.is_empty() {
        routes = built_in_routes();
    }
    let settings = state.store.settings().ok();
    let settings_digest = settings.as_ref().and_then(|value| configuration_digest(value).ok());
    let cache = match state.store.cache_records() {
        Ok(cache) => cache,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let worker_runs = state.store.worker_run_count().unwrap_or_default();
    let execution_attempts = state.store.execution_attempt_count().unwrap_or_default();
    let command_evidence = state.store.command_evidence_count().unwrap_or_default();
    let artifacts = match state.store.artifacts() {
        Ok(artifacts) => artifacts,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let approvals = state.store.pending_approvals().unwrap_or_default();
    let model_policy = state.store.model_policy().ok();
    let model_policy_digest =
        model_policy.as_ref().and_then(|value| configuration_digest(value).ok());
    let role_profiles = match role_profiles::control_plane_envelope(&state.store) {
        Ok(role_profiles) => role_profiles,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let route_promotions = state.store.route_promotions().unwrap_or_default();
    let change_records = match state.store.changes(50) {
        Ok(changes) => changes,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let mut change_workflows = Vec::with_capacity(change_records.len());
    for change in change_records {
        let attempts = match state.store.change_attempts(&change.patch.change_id) {
            Ok(attempts) => attempts,
            Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
        };
        let applies = match state.store.change_applies(&change.patch.change_id) {
            Ok(applies) => applies,
            Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
        };
        let verification = match state.store.latest_verification_artifact(&change.patch.change_id) {
            Ok(verification) => verification,
            Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
        };
        change_workflows.push(serde_json::json!({
            "change_id": change.patch.change_id,
            "patch_id": change.patch.id,
            "revision": change.patch.revision,
            "state": change.state,
            "attempts": attempts,
            "verification": verification,
            "applies": applies
        }));
    }
    let needs = state.store.needs(200).unwrap_or_default();
    let subjects = state.store.subjects(200).unwrap_or_default();
    let capabilities = state.store.capability_classes().unwrap_or_default();
    let selected_plans = state.store.selected_plans(200).unwrap_or_default();
    let proofs = state.store.proof_certificates(200).unwrap_or_default();
    let proof_accounting = state.store.proof_accounting(200).unwrap_or_default();
    let need_step_events = state.store.need_step_events(None, 0, 200).unwrap_or_default();
    let mut seen_need_steps = BTreeSet::new();
    let recent_need_steps = need_step_events
        .iter()
        .rev()
        .filter_map(|event| {
            if !seen_need_steps.insert(event.need_step_id) {
                return None;
            }
            state.store.need_step(event.need_step_id).ok().flatten().map(|step| {
                let observations = state
                    .store
                    .main_turn_observations(&event.session_id)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|observation| observation.need_step_id == Some(event.need_step_id))
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "session_id": event.session_id,
                    "step": step,
                    "request": state.store.need_step_request(event.need_step_id).ok().flatten(),
                    "main_turn_observations": observations,
                    "cost_microcredits": null
                })
            })
        })
        .collect::<Vec<_>>();
    let authoritative_full_reuse = selected_plans
        .iter()
        .filter(|plan| plan.decision_reason.starts_with("Authoritative") && plan.missing_mask == 0)
        .count();
    let authoritative_partial_reuse = selected_plans
        .iter()
        .filter(|plan| plan.decision_reason.starts_with("Authoritative") && plan.missing_mask != 0)
        .count();
    let proof_overhead_micros = proof_accounting
        .iter()
        .map(|record| {
            record
                .lookup_micros
                .saturating_add(record.validation_micros)
                .saturating_add(record.planning_micros)
                .saturating_add(record.projection_micros)
        })
        .sum::<u64>();
    let stale_candidates =
        proof_accounting.iter().map(|record| record.stale_candidates).sum::<u64>();
    let active_contradictions = state.store.active_contradiction_count().unwrap_or_default();
    let activation = match state.store.activation_status(&state.repository_root) {
        Ok(activation) => activation,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let desktop_skill = desktop_skill_status();
    Json(serde_json::json!({
        "schema": "needle.control-plane/1",
        "activation": activation,
        "integrations": {
            "desktop_skill": desktop_skill
        },
        "runtime": {
            "status": "healthy",
            "transport": "codex-app-server",
            "sandbox": "read-only",
            "approval_policy": "on-request",
            "storage": "sqlite",
            "external_telemetry": false
            ,"ipc_endpoint": state.ipc_endpoint
        },
        "phases": [
            {"id": 0, "name": "v0.3 baseline", "available": true},
            {"id": 1, "name": "NeedIR shadow", "available": true},
            {"id": 2, "name": "Subjects and contracts", "available": true},
            {"id": 3, "name": "Coverage and proofs", "available": true},
            {"id": 4, "name": "Single-artifact authority", "available": true},
            {"id": 5, "name": "Composite and partial reuse", "available": true}
        ],
        "routes": routes,
        "plans": built_in_route_plans(),
        "settings": settings.map(|settings| serde_json::json!({
            "worker_model": settings.worker_model,
            "worker_reasoning": settings.worker_reasoning,
            "worker_timeout_seconds": settings.worker_timeout_seconds,
            "evidence_failure_policy": settings.evidence_failure_policy,
            "trusted_test_execution": settings.trusted_test_execution
            ,"multi_need_enabled": settings.multi_need_policy.multi_need_enabled
            ,"continue_working_enabled": settings.multi_need_policy.continue_working_enabled
            ,"max_needs_per_task": settings.multi_need_policy.max_needs_per_task
            ,"max_workers_per_task": settings.multi_need_policy.max_workers_per_task
            ,"pending_main_tools": settings.multi_need_policy.pending_main_tools
            ,"resolver_concurrency": settings.multi_need_policy.resolver_concurrency
        })),
        "settings_digest": settings_digest,
        "cache": cache.into_iter().map(|record| serde_json::json!({
            "identity_digest": record.identity_digest,
            "logical_digest": record.logical_digest,
            "source_digest": record.source_digest,
            "created_unix_ms": record.created_unix_ms,
            "hit_count": record.hit_count
        })).collect::<Vec<_>>(),
        "artifacts": artifacts.into_iter().map(|artifact| serde_json::json!({
            "id": artifact.id,
            "request_id": artifact.request_id,
            "contract_id": artifact.contract.id,
            "kind": artifact.contract.kind,
            "scope": artifact.dependency_manifest.scope,
            "dependency_count": artifact.dependency_manifest.dependencies.len(),
            "validation_count": artifact.validations.len(),
            "created_unix_ms": artifact.created_unix_ms
        })).collect::<Vec<_>>(),
        "worker_runs": worker_runs,
        "execution_attempts": execution_attempts,
        "command_evidence": command_evidence,
        "pending_approvals": approvals.len(),
        "model_policy": model_policy,
        "model_policy_digest": model_policy_digest,
        "role_profiles": role_profiles,
        "route_promotions": route_promotions,
        "changes": change_workflows,
        "semantic": {
            "format_revision": 1,
            "needs": needs,
            "subjects": subjects,
            "capabilities": capabilities,
            "selected_plans": selected_plans,
            "proofs": proofs,
            "proof_accounting": proof_accounting,
            "need_steps": recent_need_steps,
            "metrics": {
                "authoritative_full_reuse": authoritative_full_reuse,
                "authoritative_partial_reuse": authoritative_partial_reuse,
                "worker_avoided": authoritative_full_reuse,
                "proof_overhead_micros": proof_overhead_micros,
                "stale_candidates": stale_candidates,
                "active_contradictions": active_contradictions
            },
            "predicate_contracts": built_in_predicate_contracts(),
            "route_contracts": built_in_route_contracts()
        },
        "cost_observations": []
    }))
    .into_response()
}

async fn set_activation(
    State(state): State<AppState>,
    Json(body): Json<ActivationBody>,
) -> Response {
    let profile_id = if body.enabled {
        let settings = match state.store.settings() {
            Ok(settings) => settings,
            Err(error) => return api_error(StatusCode::CONFLICT, &error.to_string()),
        };
        let isolation = match CodexWorker::verify_isolation(&settings.codex_executable) {
            Ok(isolation) => isolation,
            Err(error) => return api_error(StatusCode::CONFLICT, &error),
        };
        if !isolation.verified() {
            return api_error(
                StatusCode::CONFLICT,
                "the configured Codex binary does not satisfy Needle isolation requirements",
            );
        }
        match crate::onboarding::ensure_default_profile(
            &state.store,
            &settings,
            crate::onboarding::DEFAULT_MAX_COST_MICROUSD,
        ) {
            Ok(profile_id) => Some(profile_id),
            Err(error) => return api_error(StatusCode::CONFLICT, &error.to_string()),
        }
    } else {
        None
    };
    let scope = match ActivationScope::repository(&state.repository_root) {
        Ok(scope) => scope,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let result = commit_activation_and_reconcile(
        &state.store,
        scope,
        body.enabled,
        profile_id.as_ref(),
        body.expected_state_digest,
        |enabled| {
            if enabled {
                crate::codex_skill::ensure_installed().map(|_| ())
            } else {
                crate::codex_skill::remove_managed().map(|_| ())
            }
        },
    );
    match result {
        Ok(_) => {}
        Err(ActivationMutationError::Store(error)) => {
            let status = if matches!(error, StoreError::ActivationConflict(_)) {
                StatusCode::PRECONDITION_FAILED
            } else {
                StatusCode::CONFLICT
            };
            return api_error(status, &error.to_string());
        }
        Err(ActivationMutationError::Integration(error)) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": error,
                    "activation_committed": true
                })),
            )
                .into_response();
        }
    }
    match state.store.activation_status(&state.repository_root) {
        Ok(status) => Json(status).into_response(),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

#[derive(Debug)]
enum ActivationMutationError {
    Store(StoreError),
    Integration(String),
}

fn commit_activation_and_reconcile(
    store: &RuntimeStore,
    scope: ActivationScope,
    enabled: bool,
    profile_id: Option<&needle_core::RoleProfileId>,
    expected_state_digest: Option<Digest>,
    reconcile: impl FnOnce(bool) -> Result<(), String>,
) -> Result<ActivationRecord, ActivationMutationError> {
    let activation = store
        .compare_and_set_activation(scope, enabled, profile_id, expected_state_digest)
        .map_err(ActivationMutationError::Store)?;
    reconcile(enabled).map_err(ActivationMutationError::Integration)?;
    Ok(activation)
}

fn desktop_skill_status() -> serde_json::Value {
    match crate::codex_skill::inspect() {
        Ok(status) => serde_json::json!({
            "installed": status.installed,
            "managed": status.managed,
            "ready": status.current,
            "error": null
        }),
        Err(error) => {
            eprintln!("needle: cannot inspect managed Codex Desktop skill ({error})");
            serde_json::json!({
                "installed": null,
                "managed": null,
                "ready": false,
                "error": "managed Codex Desktop skill status is unavailable"
            })
        }
    }
}

#[cfg(test)]
#[path = "server/activation_tests.rs"]
mod activation_tests;

async fn list_needs(State(state): State<AppState>) -> Response {
    match state.store.needs(200) {
        Ok(needs) => Json(needs).into_response(),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn get_need(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.store.need(&id) {
        Ok(Some(need)) => Json(need).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "need was not found"),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn list_need_steps(
    State(state): State<AppState>,
    Query(query): Query<NeedStepQuery>,
) -> Response {
    let Some(session_id) = query.session_id.as_deref() else {
        return api_error(StatusCode::BAD_REQUEST, "session_id is required");
    };
    match state.store.need_steps(session_id) {
        Ok(steps) => Json(steps).into_response(),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn get_need_step(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let id = match Digest::parse(&id) {
        Ok(id) => id,
        Err(_) => return api_error(StatusCode::BAD_REQUEST, "invalid need step id"),
    };
    let step = match state.store.need_step(id) {
        Ok(Some(step)) => step,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "need step not found"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let session_id = match state.store.need_step_session_id(id) {
        Ok(Some(session_id)) => session_id,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "need step not found"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let events = state
        .store
        .need_step_events(Some(&session_id), 0, 200)
        .unwrap_or_default()
        .into_iter()
        .filter(|event| event.need_step_id == id)
        .collect::<Vec<_>>();
    let observations = state
        .store
        .main_turn_observations(&session_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|observation| observation.need_step_id == Some(id))
        .collect::<Vec<_>>();
    let request = match state.store.need_step_request(id) {
        Ok(request) => request,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    Json(serde_json::json!({
        "step": step,
        "request": request,
        "events": events,
        "main_turn_observations": observations
    }))
    .into_response()
}

async fn list_subjects(State(state): State<AppState>) -> Response {
    match state.store.subjects(200) {
        Ok(subjects) => Json(subjects).into_response(),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn list_contracts() -> Response {
    Json(serde_json::json!({
        "predicate_contracts": built_in_predicate_contracts(),
        "route_contracts": built_in_route_contracts()
    }))
    .into_response()
}

async fn get_plan(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.store.selected_plan(&id) {
        Ok(Some(plan)) => Json(plan).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "selected plan was not found"),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn get_proof(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.store.proof_certificate(&id) {
        Ok(Some(proof)) => Json(proof).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "proof was not found"),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn replay_proof(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let proof = match state.store.proof_certificate(&id) {
        Ok(Some(proof)) => proof,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "proof was not found"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let need = match state.store.need(&proof.need.to_string()) {
        Ok(Some(need)) => need,
        Ok(None) => return api_error(StatusCode::CONFLICT, "proof need is unavailable"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let root = state.store.repository_root_for_need(&proof.need.to_string()).ok().flatten();
    let mut candidates = Vec::new();
    let mut fresh = root.is_some();
    for certificate_id in &proof.validation_certificates {
        let certificate = match state.store.validation_certificate(&certificate_id.to_string()) {
            Ok(Some(certificate)) => certificate,
            _ => {
                fresh = false;
                continue;
            }
        };
        let artifact = match state.store.semantic_artifact(&certificate.artifact.to_string()) {
            Ok(Some(artifact)) => artifact,
            _ => {
                fresh = false;
                continue;
            }
        };
        if let Some(root) = root.as_deref() {
            fresh &= artifact_and_certificate_are_fresh(&artifact, &certificate, root);
        }
        candidates.push(ProofCandidate {
            artifact: certificate.artifact,
            validation_certificate: certificate.id,
            coverage: certificate
                .coverage
                .entries
                .iter()
                .map(|entry| entry.obligation.clone())
                .collect(),
            exact_request: false,
            expected_reuse_microusd: 0,
            claim_ids: Vec::new(),
            claim_validation_certificate_ids: Vec::new(),
            claim_set_certificate_id: None,
        });
    }
    let structural = ProofPlanner::new().replay(&need, &proof, &candidates);
    let contradiction_free = match state.store.active_contradiction(&need) {
        Ok(active) => !active,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    Json(serde_json::json!({
        "proof_id": proof.id,
        "structural_valid": structural,
        "fresh": fresh,
        "contradiction_free": contradiction_free,
        "replay_valid": structural && fresh && contradiction_free,
        "model_invoked": false
    }))
    .into_response()
}

async fn list_capabilities(State(state): State<AppState>) -> Response {
    match state.store.capability_classes() {
        Ok(capabilities) => Json(capabilities).into_response(),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn list_changes(State(state): State<AppState>) -> Response {
    let changes = match state.store.changes(100) {
        Ok(changes) => changes,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let mut output = Vec::with_capacity(changes.len());
    for change in changes {
        let digest = match state.store.change_digest(&change.patch.change_id) {
            Ok(Some(digest)) => digest,
            Ok(None) => continue,
            Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
        };
        output.push(change_summary(&change, digest));
    }
    Json(output).into_response()
}

async fn get_change(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let id = match ChangeId::parse(&id) {
        Ok(id) => id,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error),
    };
    let change = match state.store.prepared_change(&id) {
        Ok(Some(change)) => change,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "change was not found"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let verification = match state.store.latest_verification_artifact(&id) {
        Ok(verification) => verification,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let attempts = match state.store.change_attempts(&id) {
        Ok(attempts) => attempts,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let applies = match state.store.change_applies(&id) {
        Ok(applies) => applies,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let digest = match state.store.change_digest(&id) {
        Ok(Some(digest)) => digest,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "change was not found"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let apply_allowed = verification.as_ref().is_some_and(|artifact| {
        artifact.patch_id == change.patch.id
            && artifact.verdict == needle_core::VerificationStatus::Verified
            && change.state == "verified"
    });
    let mut response = Json(serde_json::json!({
        "change": change,
        "verification": verification,
        "attempts": attempts,
        "applies": applies,
        "change_digest": digest,
        "apply_allowed": apply_allowed
    }))
    .into_response();
    set_etag(&mut response, digest);
    response
}

async fn get_change_diff(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let id = match ChangeId::parse(&id) {
        Ok(id) => id,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error),
    };
    let change = match state.store.prepared_change(&id) {
        Ok(Some(change)) => change,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "change was not found"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let blobs = match state.store.patch_file_blobs(change.patch.id) {
        Ok(blobs) => blobs,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let diff = match render_patch_diff(&change, &blobs) {
        Ok(diff) => diff,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    let digest = match state.store.change_digest(&id) {
        Ok(Some(digest)) => digest,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "change was not found"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let mut response = diff.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/x-diff; charset=utf-8"));
    set_etag(&mut response, digest);
    response
}

async fn apply_change(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ApplyChangeBody>,
) -> Response {
    if !body.confirm {
        return api_error(
            StatusCode::BAD_REQUEST,
            "applying a verified change requires explicit confirmation",
        );
    }
    let id = match ChangeId::parse(&id) {
        Ok(id) => id,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error),
    };
    let expected = match state.store.change_digest(&id) {
        Ok(Some(digest)) => digest,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "change was not found"),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    if let Err(error) = require_if_match(&headers, &expected.to_string()) {
        return if_match_response(error, "change state changed");
    }
    let _guard = state.apply_lock.lock().await;
    match apply_verified_change(&state.store, &state.repository_root, &id, expected) {
        Ok(record) => Json(record).into_response(),
        Err(error) => change_apply_error_response(error),
    }
}

fn change_summary(change: &PreparedChangeRecord, digest: Digest) -> serde_json::Value {
    serde_json::json!({
        "change_id": change.patch.change_id,
        "state": change.state,
        "patch_id": change.patch.id,
        "revision": change.patch.revision,
        "summary": change.patch.summary,
        "changed_files": change.patch.files.iter().map(|file| serde_json::json!({
            "path": file.path,
            "operation": file.operation
        })).collect::<Vec<_>>(),
        "change_digest": digest,
        "created_unix_ms": change.created_unix_ms
    })
}

fn render_patch_diff(
    change: &PreparedChangeRecord,
    blobs: &[needle_runtime::PatchFileBlob],
) -> Result<String, String> {
    let mut output = String::new();
    for file in &change.patch.files {
        let blob = blobs
            .iter()
            .find(|blob| blob.path == file.path)
            .ok_or_else(|| format!("patch blob is missing for `{}`", file.path))?;
        let before = blob
            .before
            .as_deref()
            .map(std::str::from_utf8)
            .transpose()
            .map_err(|_| format!("before blob is not UTF-8 for `{}`", file.path))?;
        let after = blob
            .after
            .as_deref()
            .map(std::str::from_utf8)
            .transpose()
            .map_err(|_| format!("after blob is not UTF-8 for `{}`", file.path))?;
        let before_name =
            if file.operation == PatchOperation::Create { "/dev/null" } else { &file.path };
        let after_name =
            if file.operation == PatchOperation::Delete { "/dev/null" } else { &file.path };
        output.push_str(&format!("--- {before_name}\n+++ {after_name}\n"));
        output.push_str(&format!(
            "@@ -1,{} +1,{} @@\n",
            before.map_or(0, |value| value.lines().count()),
            after.map_or(0, |value| value.lines().count())
        ));
        append_diff_lines(&mut output, '-', before.unwrap_or_default());
        append_diff_lines(&mut output, '+', after.unwrap_or_default());
        if output.len() > needle_core::MAX_PATCH_DIFF_BYTES.saturating_mul(3) {
            return Err("rendered patch diff exceeds the bounded response limit".to_owned());
        }
    }
    Ok(output)
}

fn append_diff_lines(output: &mut String, prefix: char, value: &str) {
    for line in value.split_inclusive('\n') {
        output.push(prefix);
        output.push_str(line);
        if !line.ends_with('\n') {
            output.push('\n');
        }
    }
}

fn set_etag(response: &mut Response, digest: Digest) {
    let value = format!("\"{digest}\"");
    if let Ok(value) = HeaderValue::from_str(&value) {
        response.headers_mut().insert(header::ETAG, value);
    }
}

fn change_apply_error_response(error: ChangeApplyError) -> Response {
    match error {
        ChangeApplyError::NotFound => api_error(StatusCode::NOT_FOUND, &error.to_string()),
        ChangeApplyError::DigestMismatch => {
            api_error(StatusCode::PRECONDITION_FAILED, &error.to_string())
        }
        ChangeApplyError::NotVerified | ChangeApplyError::SnapshotDrift => {
            api_error(StatusCode::CONFLICT, &error.to_string())
        }
        ChangeApplyError::Materialization(_) | ChangeApplyError::Recovery(_) => {
            api_error(StatusCode::CONFLICT, &error.to_string())
        }
        ChangeApplyError::Store(_) | ChangeApplyError::Snapshot(_) | ChangeApplyError::Io(_) => {
            api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string())
        }
    }
}

async fn set_capability_mode(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CapabilityModeBody>,
) -> Response {
    if !body.confirm {
        return api_error(
            StatusCode::BAD_REQUEST,
            "capability authority changes require explicit confirmation",
        );
    }
    let classes = match state.store.capability_classes() {
        Ok(classes) => classes,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let Some(class) = classes.into_iter().find(|class| class.id == id) else {
        return api_error(StatusCode::NOT_FOUND, "capability was not found");
    };
    if let Err(error) = require_if_match(&headers, &class.definition_digest.to_string()) {
        return if_match_response(error, "capability definition changed");
    }
    let evidence = match body.evidence_digest {
        Some(value) => match Digest::parse(&value) {
            Ok(digest) => Some(digest),
            Err(error) => return api_error(StatusCode::BAD_REQUEST, &error.to_string()),
        },
        None => None,
    };
    match state.store.set_capability_mode(&id, class.definition_digest, body.mode, evidence) {
        Ok(Some(updated)) => Json(updated).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "capability was not found"),
        Err(needle_runtime::StoreError::DefinitionDigest(error)) => {
            api_error(StatusCode::CONFLICT, &error)
        }
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn decide_approval(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DecisionBody>,
) -> Response {
    let digest = match Digest::parse(&body.payload_digest) {
        Ok(digest) => digest,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    match state.store.decide_approval(&id, body.decision, ApprovalDecisionSource::WebUser, digest) {
        Ok(request) => Json(request).into_response(),
        Err(needle_runtime::StoreError::ApprovalConflict(_)) => {
            api_error(StatusCode::CONFLICT, "approval was resolved or its payload changed")
        }
        Err(needle_runtime::StoreError::ApprovalExpired(_)) => {
            api_error(StatusCode::GONE, "approval expired")
        }
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn set_route_state(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<RouteStateBody>,
) -> Response {
    let routes = match state.store.routes() {
        Ok(routes) => routes,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let Some(route) = routes.into_iter().find(|route| route.id == id) else {
        return api_error(StatusCode::NOT_FOUND, "route is not configured");
    };
    let expected = route.definition_digest.to_string();
    if let Err(error) = require_if_match(&headers, &expected) {
        return if_match_response(error, "route definition changed");
    }
    match state.store.set_route_enabled(&id, body.enabled) {
        Ok(true) => {
            let mut updated = route;
            updated.enabled = body.enabled;
            Json(updated).into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "route is not configured"),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn set_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SettingsBody>,
) -> Response {
    let current = match state.store.settings() {
        Ok(settings) => settings,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let expected = match configuration_digest(&current) {
        Ok(digest) => digest.to_string(),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    if let Err(error) = require_if_match(&headers, &expected) {
        return if_match_response(error, "settings changed");
    }
    let updated = RuntimeSettings {
        codex_executable: current.codex_executable,
        worker_model: body.worker_model,
        worker_reasoning: body.worker_reasoning,
        worker_timeout_seconds: body.worker_timeout_seconds,
        evidence_failure_policy: body.evidence_failure_policy,
        trusted_test_execution: body.trusted_test_execution,
        multi_need_policy: needle_core::MultiNeedPolicy {
            multi_need_enabled: body.multi_need_enabled,
            continue_working_enabled: body.continue_working_enabled,
            max_needs_per_task: body.max_needs_per_task,
            max_workers_per_task: body.max_workers_per_task,
            pending_main_tools: body.pending_main_tools,
            resolver_concurrency: body.resolver_concurrency,
        },
    };
    match state.store.set_runtime_settings(&updated) {
        Ok(()) => {
            let digest = match configuration_digest(&updated) {
                Ok(digest) => digest,
                Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
            };
            Json(serde_json::json!({
                "settings": {
                    "worker_model": updated.worker_model,
                    "worker_reasoning": updated.worker_reasoning,
                    "worker_timeout_seconds": updated.worker_timeout_seconds,
                    "evidence_failure_policy": updated.evidence_failure_policy,
                "trusted_test_execution": updated.trusted_test_execution
                ,"multi_need_enabled": updated.multi_need_policy.multi_need_enabled
                ,"continue_working_enabled": updated.multi_need_policy.continue_working_enabled
                ,"max_needs_per_task": updated.multi_need_policy.max_needs_per_task
                ,"max_workers_per_task": updated.multi_need_policy.max_workers_per_task
                ,"pending_main_tools": updated.multi_need_policy.pending_main_tools
                ,"resolver_concurrency": updated.multi_need_policy.resolver_concurrency
                },
                "settings_digest": digest
            }))
            .into_response()
        }
        Err(needle_runtime::StoreError::DefinitionDigest(_)) => {
            api_error(StatusCode::BAD_REQUEST, "settings are invalid")
        }
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn set_model_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ModelPolicyBody>,
) -> Response {
    let current = match state.store.model_policy() {
        Ok(policy) => policy,
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let expected = match configuration_digest(&current) {
        Ok(digest) => digest.to_string(),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    if let Err(error) = require_if_match(&headers, &expected) {
        return if_match_response(error, "model policy changed");
    }
    let profile = |value: ModelProfileBody| {
        WorkerProfile::new("codex", value.model, value.reasoning, value.service_tier)
    };
    let policy = match body {
        ModelPolicyBody::FixedOrder { profiles, repair_once, native_fallback } => {
            ModelPolicy::FixedOrder {
                profiles: profiles.into_iter().map(profile).collect(),
                repair_once,
                native_fallback,
            }
        }
        ModelPolicyBody::CheapestValidatedFirst { profiles, native_fallback } => {
            ModelPolicy::CheapestValidatedFirst {
                promoted_profiles: profiles.into_iter().map(profile).collect(),
                native_fallback,
            }
        }
    };
    if let ModelPolicy::CheapestValidatedFirst { promoted_profiles, .. } = &policy {
        let routes = match state.store.routes() {
            Ok(routes) => routes,
            Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
        };
        let route_keys = routes
            .iter()
            .filter(|route| route.enabled)
            .map(|route| route.matcher.need_key.as_str())
            .collect::<BTreeSet<_>>();
        if route_keys.is_empty() {
            return api_error(
                StatusCode::CONFLICT,
                "CheapestValidatedFirst requires an enabled promoted route",
            );
        }
        for route_key in route_keys {
            let promoted = match state.store.promoted_profile_digests(route_key) {
                Ok(promoted) => promoted,
                Err(error) => {
                    return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
                }
            };
            if promoted_profiles
                .iter()
                .any(|profile| !promoted.contains(&profile.definition_digest))
            {
                return api_error(
                    StatusCode::CONFLICT,
                    "CheapestValidatedFirst contains an unpromoted route/profile pair",
                );
            }
        }
    }
    match state.store.set_model_policy(&policy) {
        Ok(()) => {
            let digest = match configuration_digest(&policy) {
                Ok(digest) => digest,
                Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &error),
            };
            Json(serde_json::json!({
                "model_policy": policy,
                "model_policy_digest": digest
            }))
            .into_response()
        }
        Err(needle_runtime::StoreError::DefinitionDigest(_)) => {
            api_error(StatusCode::BAD_REQUEST, "model policy is invalid")
        }
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn approval_events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream =
        stream::unfold((state.store, BTreeSet::<String>::new()), |(store, previous)| async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let _ = store.expire_approvals();
            let current = store
                .pending_approvals()
                .unwrap_or_default()
                .into_iter()
                .map(|request| request.id)
                .collect::<BTreeSet<_>>();
            let created = current.difference(&previous).cloned().collect::<Vec<_>>();
            let resolved = previous.difference(&current).cloned().collect::<Vec<_>>();
            let event = if created.is_empty() && resolved.is_empty() {
                Event::default().comment("idle")
            } else {
                let payload = serde_json::json!({
                    "created": created,
                    "resolved_or_timed_out": resolved
                });
                Event::default()
                    .event("approval-change")
                    .json_data(payload)
                    .unwrap_or_else(|_| Event::default().event("approval-change"))
            };
            Some((Ok(event), (store, current)))
        });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn need_step_events(
    State(state): State<AppState>,
    Query(query): Query<NeedStepEventsQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let initial_after = query.after.unwrap_or_default();
    let stream = stream::unfold(
        (state.store, query.session_id, initial_after, VecDeque::new()),
        |(store, session_id, mut after, mut pending)| async move {
            if pending.is_empty() {
                tokio::time::sleep(Duration::from_millis(250)).await;
                pending.extend(
                    store.need_step_events(session_id.as_deref(), after, 200).unwrap_or_default(),
                );
            }
            let event = if let Some(record) = pending.pop_front() {
                after = record.event_id;
                let name = match record.state {
                    needle_core::NeedStepState::Requested => "need.requested",
                    needle_core::NeedStepState::Queued => "need.queued",
                    needle_core::NeedStepState::Resolved => "need.resolved",
                    needle_core::NeedStepState::Delivered => "need.delivered",
                    needle_core::NeedStepState::NativeFallback
                    | needle_core::NeedStepState::Failed => "need.fallback",
                    needle_core::NeedStepState::Cancelled => "need.cancelled",
                    needle_core::NeedStepState::Resolving => "need.resolving",
                };
                Event::default()
                    .id(record.event_id.to_string())
                    .event(name)
                    .json_data(record)
                    .unwrap_or_else(|_| Event::default().event(name))
            } else {
                Event::default().comment("idle")
            };
            Some((Ok(event), (store, session_id, after, pending)))
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn security(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(rejection) =
        security_rejection(&state.authority, &state.session_token, &state.csrf_token, &request)
    {
        return rejection;
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

fn security_rejection(
    authority: &str,
    session_token: &str,
    csrf_token: &str,
    request: &Request,
) -> Option<Response> {
    if request.headers().get(header::HOST).and_then(|value| value.to_str().ok()) != Some(authority)
    {
        return Some(api_error(StatusCode::BAD_REQUEST, "invalid Host"));
    }
    if let Some(origin) =
        request.headers().get(header::ORIGIN).and_then(|value| value.to_str().ok())
        && origin != format!("http://{authority}")
    {
        return Some(api_error(StatusCode::FORBIDDEN, "invalid Origin"));
    }
    let path = request.uri().path();
    if path != "/" && path != "/health" {
        let authenticated = cookie_value(request.headers(), "needle_session")
            .is_some_and(|value| value == session_token);
        if !authenticated {
            return Some(api_error(StatusCode::UNAUTHORIZED, "missing runtime session"));
        }
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        let csrf = request.headers().get("x-csrf-token").and_then(|value| value.to_str().ok());
        if csrf != Some(csrf_token) {
            return Some(api_error(StatusCode::FORBIDDEN, "invalid CSRF token"));
        }
    }
    None
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|cookie| cookie.strip_prefix(name)?.strip_prefix('='))
}

fn api_error(status: StatusCode, message: &str) -> Response {
    (status, Json(ApiError { error: message.to_owned() })).into_response()
}

fn require_if_match(headers: &HeaderMap, expected: &str) -> Result<(), IfMatchError> {
    let supplied = headers.get(header::IF_MATCH).and_then(|value| value.to_str().ok());
    let expected = format!("\"{expected}\"");
    match supplied {
        None => Err(IfMatchError::Missing),
        Some(value) if value == expected => Ok(()),
        Some(_) => Err(IfMatchError::Changed),
    }
}

fn if_match_response(error: IfMatchError, changed_message: &str) -> Response {
    match error {
        IfMatchError::Missing => {
            api_error(StatusCode::PRECONDITION_REQUIRED, "If-Match is required")
        }
        IfMatchError::Changed => api_error(StatusCode::PRECONDITION_FAILED, changed_message),
    }
}

fn configuration_digest(value: &impl Serialize) -> Result<Digest, String> {
    serde_json::to_vec(value).map(Digest::blake3).map_err(|error| error.to_string())
}

fn random_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
pub(crate) fn test_nonce() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_are_parsed_exactly_without_prefix_confusion() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=x; needle_session=secret; needle_session_extra=no"),
        );
        assert_eq!(cookie_value(&headers, "needle_session"), Some("secret"));
        assert_eq!(cookie_value(&headers, "missing"), None);
    }

    #[test]
    fn launch_tokens_are_high_entropy_and_distinct() {
        let left = random_token().unwrap();
        let right = random_token().unwrap();
        assert_eq!(left.len(), 64);
        assert_ne!(left, right);
    }

    #[test]
    fn launch_token_is_consumed_atomically_once() {
        let consumed = Mutex::new(false);
        assert!(consume_launch_token(&consumed, true));
        assert!(!consume_launch_token(&consumed, true));
        assert!(!consume_launch_token(&Mutex::new(false), false));
    }

    #[test]
    fn if_match_requires_the_exact_quoted_digest() {
        let mut headers = HeaderMap::new();
        assert_eq!(require_if_match(&headers, "b3:expected"), Err(IfMatchError::Missing));
        headers.insert(header::IF_MATCH, HeaderValue::from_static("b3:expected"));
        assert_eq!(require_if_match(&headers, "b3:expected"), Err(IfMatchError::Changed));
        headers.insert(header::IF_MATCH, HeaderValue::from_static("\"\"b3:expected\"\""));
        assert_eq!(require_if_match(&headers, "b3:expected"), Err(IfMatchError::Changed));
        headers.insert(header::IF_MATCH, HeaderValue::from_static("\"b3:wrong\""));
        assert_eq!(require_if_match(&headers, "b3:expected"), Err(IfMatchError::Changed));
        headers.insert(header::IF_MATCH, HeaderValue::from_static("\"b3:expected\""));
        assert!(require_if_match(&headers, "b3:expected").is_ok());
    }

    #[test]
    fn mutation_security_requires_session_and_exact_csrf_token() {
        let authority = "127.0.0.1:43210";
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/capabilities/test/mode")
            .header(header::HOST, authority)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            security_rejection(authority, "session", "csrf", &request).unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/capabilities/test/mode")
            .header(header::HOST, authority)
            .header(header::COOKIE, "needle_session=session")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            security_rejection(authority, "session", "csrf", &request).unwrap().status(),
            StatusCode::FORBIDDEN
        );

        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/capabilities/test/mode")
            .header(header::HOST, authority)
            .header(header::ORIGIN, format!("http://{authority}"))
            .header(header::COOKIE, "needle_session=session")
            .header("x-csrf-token", "csrf")
            .body(Body::empty())
            .unwrap();
        assert!(security_rejection(authority, "session", "csrf", &request).is_none());
    }
}
