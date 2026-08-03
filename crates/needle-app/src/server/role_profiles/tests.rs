use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware;
use needle_core::{
    CodexHost, CodexRole, CommandPolicy, FallbackPolicy, FilesystemPolicy, NetworkPolicy,
    RepairPolicy, RoleProfileBudget, RoleProfileDefinitionInput, RoleProfileId, ServiceTier,
    TestPolicy, ToolPolicy,
};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

fn fixture_input() -> RoleProfileDefinitionInput {
    RoleProfileDefinitionInput {
        profile_id: RoleProfileId::new("explorer.default").unwrap(),
        role: CodexRole::Explorer,
        host: CodexHost::Codex,
        model: "gpt-5".to_owned(),
        reasoning: needle_core::ReasoningLevel::Medium,
        service_tier: ServiceTier::Default,
        timeout_seconds: 120,
        budget: RoleProfileBudget {
            max_turns: 2,
            max_output_tokens: 1200,
            max_cost_microusd: 1000,
        },
        prompt_profile_digest: Digest::blake3(b"prompt"),
        output_contract_digest: Digest::blake3(b"output"),
        tool_policy: ToolPolicy::ReadOnly,
        command_policy: CommandPolicy::ReadOnly,
        filesystem_policy: FilesystemPolicy::ReadOnlyCheckout,
        network_policy: NetworkPolicy::Denied,
        test_policy: TestPolicy::Disabled,
        repair_policy: RepairPolicy::None,
        fallback_policy: FallbackPolicy::Native,
        concurrency: 1,
        route_assignments: Vec::new(),
    }
}

fn input_with_model(model: &str) -> Value {
    let mut input = serde_json::to_value(fixture_input()).unwrap();
    input["model"] = json!(model);
    input
}

fn test_state() -> (AppState, PathBuf) {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let path = std::env::temp_dir().join(format!("needle-role-profile-http-{nanos}.sqlite3"));
    let store = RuntimeStore::new(&path);
    store.initialize().unwrap();
    (
        AppState {
            store,
            authority: "127.0.0.1:43210".to_owned(),
            launch_token: "launch".to_owned(),
            session_token: "session".to_owned(),
            csrf_token: "csrf".to_owned(),
            ipc_endpoint: "test".to_owned(),
            repository_root: PathBuf::from("."),
            apply_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            launch_consumed: std::sync::Arc::new(std::sync::Mutex::new(false)),
        },
        path,
    )
}

fn test_router(state: AppState) -> Router {
    routes(Router::new())
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state, super::super::security))
}

async fn call(
    app: &Router,
    method: Method,
    path: &str,
    body: Option<Value>,
    if_match: Option<&str>,
    authenticated: bool,
    csrf: bool,
) -> (StatusCode, Value) {
    let mut builder =
        Request::builder().method(method).uri(path).header(header::HOST, "127.0.0.1:43210");
    if authenticated {
        builder = builder.header(header::COOKIE, "needle_session=session");
    }
    if csrf {
        builder = builder.header("x-csrf-token", "csrf");
    }
    if let Some(if_match) = if_match {
        builder = builder.header(header::IF_MATCH, format!("\"{if_match}\""));
    }
    let request = builder
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.map_or_else(String::new, |value| value.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

fn assert_no_sensitive_keys(value: &Value) {
    let text = value.to_string();
    for token in [
        "opaque-sensitive-sentinel",
        "repository_root",
        "transcript",
        "credentials",
        "absolute_path",
    ] {
        assert!(!text.contains(token), "response unexpectedly contains {token}");
    }
}

#[tokio::test]
async fn authenticated_http_lifecycle_is_digest_bound_and_bounded() {
    let (state, path) = test_state();
    let app = test_router(state);
    let input = serde_json::to_value(fixture_input()).unwrap();

    let mut unknown = input.clone();
    unknown["access_token"] = json!("opaque-sensitive-sentinel");
    let (status, body) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/preflight",
        Some(unknown),
        None,
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_no_sensitive_keys(&body);
    let (status, list) =
        call(&app, Method::GET, "/api/v1/role-profiles", None, None, true, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["items"].as_array().unwrap().len(), 0);

    let (status, preflight) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/preflight",
        Some(input.clone()),
        None,
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preflight["passed"], true);
    assert_eq!(preflight["operation"], "create");
    let absence = preflight["if_match"].as_str().unwrap().to_owned();

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/draft",
        Some(input.clone()),
        None,
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/draft",
        Some(json!({"unknown": true})),
        Some(&absence),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, draft) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/draft",
        Some(input),
        Some(&absence),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let state_digest = draft["state_digest"].as_str().unwrap().to_owned();
    let definition_digest = draft["profile"]["definition_digest"].as_str().unwrap().to_owned();
    assert!(draft.to_string().contains("definition_digest"));

    let (status, detail) =
        call(&app, Method::GET, "/api/v1/role-profiles/explorer.default", None, None, true, false)
            .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["profile"]["state"], "draft");
    assert_eq!(detail["profile"]["preflight"]["passed"], true);
    assert!(detail["profile"]["preflight"]["failures"].as_array().unwrap().is_empty());
    assert_no_sensitive_keys(&detail);

    let (status, history) = call(
        &app,
        Method::GET,
        "/api/v1/role-profiles/explorer.default/revisions?limit=10",
        None,
        None,
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history["items"].as_array().unwrap().len(), 1);
    assert_eq!(history["total"], 1);
    assert_no_sensitive_keys(&history);

    let revised_input = input_with_model("gpt-5-mini");
    let (status, revised_preflight) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/preflight",
        Some(revised_input.clone()),
        None,
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(revised_preflight["operation"], "revise");
    assert_eq!(revised_preflight["if_match"], state_digest);

    let (status, revised) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/draft",
        Some(revised_input.clone()),
        Some(&state_digest),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(revised["operation"], "revise");
    let revised_state_digest = revised["state_digest"].as_str().unwrap().to_owned();
    let revised_definition_digest =
        revised["profile"]["definition_digest"].as_str().unwrap().to_owned();

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/draft",
        Some(input_with_model("gpt-5-pro")),
        Some(&state_digest),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/draft",
        Some(revised_input),
        Some(&revised_state_digest),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, history) = call(
        &app,
        Method::GET,
        "/api/v1/role-profiles/explorer.default/revisions?limit=10",
        None,
        None,
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history["items"].as_array().unwrap().len(), 2);
    assert_eq!(history["total"], 2);

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/activate",
        Some(json!({"revision": 99, "definition_digest": revised_definition_digest, "confirm": true})),
        Some(&revised_state_digest),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/activate",
        Some(json!({"revision": 2, "definition_digest": definition_digest, "confirm": true})),
        Some(&revised_state_digest),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, unchanged) =
        call(&app, Method::GET, "/api/v1/role-profiles/explorer.default", None, None, true, false)
            .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(unchanged["profile"]["state_digest"], revised_state_digest);
    assert_eq!(unchanged["profile"]["active_revision"], Value::Null);

    let (status, activated) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/activate",
        Some(
            json!({"revision": 2, "definition_digest": revised_definition_digest, "confirm": true}),
        ),
        Some(&revised_state_digest),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let active_digest =
        activated["profile"]["active_definition_digest"].as_str().unwrap().to_owned();
    let active_state_digest = activated["state_digest"].as_str().unwrap().to_owned();
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/deactivate",
        Some(json!({"active_definition_digest": active_digest, "confirm": true})),
        Some(&active_state_digest),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, audit) = call(
        &app,
        Method::GET,
        "/api/v1/role-profiles/explorer.default/audit?limit=100",
        None,
        None,
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(audit["items"].as_array().unwrap().len(), 4);
    assert_no_sensitive_keys(&audit);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn authenticated_http_rejects_missing_session_csrf_and_stale_state_without_mutation() {
    let (state, path) = test_state();
    let app = test_router(state);
    let input = serde_json::to_value(fixture_input()).unwrap();
    let (status, _) =
        call(&app, Method::GET, "/api/v1/role-profiles", None, None, false, false).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/preflight",
        Some(input.clone()),
        None,
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/draft",
        Some(input),
        Some("b3:wrong"),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    let (status, list) =
        call(&app, Method::GET, "/api/v1/role-profiles", None, None, true, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["items"].as_array().unwrap().len(), 0);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn http_rejections_are_stable_and_bounded_without_mutation() {
    let (state, path) = test_state();
    let app = test_router(state);
    let (status, invalid_limit) =
        call(&app, Method::GET, "/api/v1/role-profiles?limit=abc", None, None, true, false).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid_limit["schema"], ERROR_SCHEMA);
    assert_eq!(invalid_limit["code"], "invalid_query");
    let (status, unknown_query) =
        call(&app, Method::GET, "/api/v1/role-profiles?unexpected=true", None, None, true, false)
            .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown_query["schema"], ERROR_SCHEMA);
    assert_eq!(unknown_query["code"], "invalid_query");

    let marker = "sensitive-body-marker";
    let oversized = json!({"payload": format!("{marker}{}", "x".repeat(70 * 1024))});
    let (status, body_error) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/preflight",
        Some(oversized),
        None,
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body_error["schema"], ERROR_SCHEMA);
    assert_eq!(body_error["code"], "body_too_large");
    assert!(!body_error.to_string().contains(marker));
    let (status, list) =
        call(&app, Method::GET, "/api/v1/role-profiles", None, None, true, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["items"].as_array().unwrap().len(), 0);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn authenticated_http_revise_audit_preserves_active_pointer() {
    let (state, path) = test_state();
    let app = test_router(state);
    let input = serde_json::to_value(fixture_input()).unwrap();
    let (status, preflight) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/preflight",
        Some(input.clone()),
        None,
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let absence = preflight["if_match"].as_str().unwrap().to_owned();
    let (status, draft) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/draft",
        Some(input),
        Some(&absence),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let state_digest = draft["state_digest"].as_str().unwrap().to_owned();
    let definition_digest = draft["profile"]["definition_digest"].as_str().unwrap().to_owned();
    let (status, activated) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/activate",
        Some(json!({"revision": 1, "definition_digest": definition_digest, "confirm": true})),
        Some(&state_digest),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let active_state_digest = activated["state_digest"].as_str().unwrap().to_owned();

    let revised_input = input_with_model("gpt-5-mini");
    let (status, revised_preflight) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/preflight",
        Some(revised_input.clone()),
        None,
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(revised_preflight["if_match"], active_state_digest);
    let (status, revised) = call(
        &app,
        Method::POST,
        "/api/v1/role-profiles/explorer.default/draft",
        Some(revised_input),
        Some(&active_state_digest),
        true,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(revised["operation"], "revise");

    let (status, audit) = call(
        &app,
        Method::GET,
        "/api/v1/role-profiles/explorer.default/audit?limit=100",
        None,
        None,
        true,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let revise_audit = audit["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["operation"] == "revise")
        .unwrap();
    assert_eq!(revise_audit["prior_active_revision"], 1);
    assert_eq!(revise_audit["prior_active_digest"], definition_digest);
    let (status, detail) =
        call(&app, Method::GET, "/api/v1/role-profiles/explorer.default", None, None, true, false)
            .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["profile"]["active_revision"], 1);
    assert_eq!(detail["profile"]["latest_revision"], 2);
    let _ = std::fs::remove_file(path);
}
