use super::*;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware;
use needle_core::{
    AllowedPath, AllowedPathScope, ChangeApplyId, ChangeRequest, DevelopmentLifecycle,
    LifecycleArtifactKind, LifecycleArtifactRef, LifecycleBudget, LifecycleEvent, LifecycleSpec,
    LifecycleTestPlanBinding, LifecycleTransition, LifecycleUsage, LifecycleWorkerCompletion,
    LifecycleWorkerProfiles, PatchId, RoleProfileId, RoleProfileProvenance, TestPlan,
};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::path::{Path as FilePath, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

fn test_state() -> (AppState, PathBuf) {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let path = std::env::temp_dir()
        .join(format!("needle-lifecycle-http-{}-{nanos}.sqlite3", std::process::id()));
    let store = needle_runtime::RuntimeStore::new(&path);
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

async fn get(app: &Router, path: &str, authenticated: bool) -> (StatusCode, Value) {
    let mut builder =
        Request::builder().method(Method::GET).uri(path).header(header::HOST, "127.0.0.1:43210");
    if authenticated {
        builder = builder.header(header::COOKIE, "needle_session=session");
    }
    let response = app.clone().oneshot(builder.body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

fn profile(id: &str) -> RoleProfileProvenance {
    RoleProfileProvenance::new(RoleProfileId::new(id).unwrap(), 1, Digest::blake3(id)).unwrap()
}

fn lifecycle_spec(name: &str) -> LifecycleSpec {
    let test_identifier = "opaque-sensitive-sentinel".to_owned();
    LifecycleSpec {
        worker_depth_limit: 1,
        profiles: LifecycleWorkerProfiles {
            explore: profile(&format!("{name}.explore")),
            implement: profile(&format!("{name}.implement")),
            test: profile(&format!("{name}.test")),
            review: profile(&format!("{name}.review")),
            verify: profile(&format!("{name}.verify")),
        },
        budget: LifecycleBudget {
            max_worker_turns: 10,
            max_output_tokens: 10_000,
            max_cost_microusd: 100_000,
            max_concurrent_workers: 1,
        },
        test_plans: vec![LifecycleTestPlanBinding {
            plan: TestPlan {
                runner: "cargo".to_owned(),
                argv: vec![
                    "cargo".to_owned(),
                    "test".to_owned(),
                    test_identifier.clone(),
                    "--".to_owned(),
                    "--exact".to_owned(),
                ],
                cwd_relative: ".".to_owned(),
                test_identifier,
                requires_approval: true,
                execution_evidence_id: None,
            },
            certificate_digest: Digest::blake3(format!("{name}:certificate")),
        }],
    }
}

fn insert_event(connection: &Connection, event: &LifecycleEvent) {
    let payload = serde_json::to_string(event).unwrap();
    connection
        .execute(
            "INSERT INTO change_events(
                change_id, event_type, payload_digest, payload_json,
                created_unix_ms, lifecycle_sequence
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.change_id.to_string(),
                if event.sequence == 0 { "lifecycle_created" } else { "lifecycle_transitioned" },
                Digest::blake3(payload.as_bytes()).to_string(),
                payload,
                event.created_unix_ms,
                event.sequence,
            ],
        )
        .unwrap();
}

fn seed_lifecycle(
    state: &AppState,
    database: &FilePath,
    name: &str,
    with_transition: bool,
) -> ChangeId {
    let source = Digest::blake3(format!("{name}:source"));
    let change_id = ChangeId::from_digest(Digest::blake3(format!("{name}:change")));
    let request = ChangeRequest {
        task: "Expose bounded lifecycle state.".to_owned(),
        acceptance_criteria: vec!["Only safe projections are returned.".to_owned()],
        allowed_paths: vec![AllowedPath {
            path: "fixture.txt".to_owned(),
            scope: AllowedPathScope::Exact,
        }],
        artifact_ids: Vec::new(),
        claim_ids: Vec::new(),
        constraints: Vec::new(),
    };
    state
        .store
        .record_change_request(
            &change_id,
            Digest::blake3(format!("{name}:repository")),
            source,
            request.digest(source),
            &request,
        )
        .unwrap();

    let initial =
        DevelopmentLifecycle::new(change_id.clone(), source, lifecycle_spec(name), 10).unwrap();
    let created = LifecycleEvent::created(&initial).unwrap();
    let (lifecycle, transitioned) = if with_transition {
        let exploration_id = Digest::blake3(format!("{name}:exploration"));
        let worker = LifecycleWorkerCompletion {
            profile: initial.spec.profiles.explore.clone(),
            worker_depth: 1,
            logical_worker_spawns: 1,
            usage: LifecycleUsage { worker_turns: 1, output_tokens: 20, cost_microusd: 30 },
        };
        let (next, event) = initial
            .transition(
                LifecycleTransition::CompleteExplore {
                    worker,
                    artifacts: vec![LifecycleArtifactRef {
                        kind: LifecycleArtifactKind::Exploration,
                        id: exploration_id,
                        source_snapshot: source,
                    }],
                },
                11,
            )
            .unwrap();
        (next, Some(event))
    } else {
        (initial, None)
    };
    let state_json = serde_json::to_string(&lifecycle).unwrap();
    let state_digest = lifecycle.state_digest();
    let connection = Connection::open(database).unwrap();
    connection
        .execute(
            "INSERT INTO change_lifecycles(
                lifecycle_id, change_id, source_snapshot_digest, state_digest,
                generation, state_json, created_unix_ms, updated_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                lifecycle.id.to_string(),
                change_id.to_string(),
                source.to_string(),
                state_digest.to_string(),
                lifecycle.generation,
                state_json,
                lifecycle.created_unix_ms,
                lifecycle.updated_unix_ms,
            ],
        )
        .unwrap();
    insert_event(&connection, &created);
    if let Some(event) = transitioned {
        insert_event(&connection, &event);
    }
    change_id
}

fn seed_recovery(database: &FilePath, change_id: &ChangeId) {
    let patch_id = PatchId(Digest::blake3(b"http:patch"));
    let apply_id = ChangeApplyId(Digest::blake3(b"http:apply"));
    let older_apply_id = ChangeApplyId(Digest::blake3(b"http:older-apply"));
    let connection = Connection::open(database).unwrap();
    connection
        .execute(
            "INSERT INTO patch_artifacts(
                patch_id, change_id, revision, source_snapshot_digest, patch_digest,
                artifact_json, manifest_json, declared_output_json, discrepancies_json,
                created_unix_ms
             ) VALUES(?1, ?2, 1, ?3, ?4, '{}', '{}', '{}', '[]', 12)",
            params![
                patch_id.to_string(),
                change_id.to_string(),
                Digest::blake3(b"http:source").to_string(),
                Digest::blake3(b"http:patch-digest").to_string(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO change_applies(
                apply_id, change_id, patch_id, repository_root, pre_snapshot_digest,
                post_snapshot_digest, status, journal_json, created_unix_ms, completed_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'applied', '{}', 11, 12)",
            params![
                older_apply_id.to_string(),
                change_id.to_string(),
                patch_id.to_string(),
                "C:\\opaque-sensitive-sentinel\\older-repository",
                Digest::blake3(b"http:older-pre").to_string(),
                Digest::blake3(b"http:older-post").to_string(),
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO change_applies(
                apply_id, change_id, patch_id, repository_root, pre_snapshot_digest,
                post_snapshot_digest, status, journal_json, created_unix_ms, completed_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'recovery_conflict', ?7, 13, 14)",
            params![
                apply_id.to_string(),
                change_id.to_string(),
                patch_id.to_string(),
                "C:\\opaque-sensitive-sentinel\\repository",
                Digest::blake3(b"http:pre").to_string(),
                Digest::blake3(b"http:post").to_string(),
                r#"{"transcript":"opaque-sensitive-sentinel"}"#,
            ],
        )
        .unwrap();
}

fn lifecycle_snapshot(database: &FilePath) -> (u64, u64, u64, String, String) {
    let connection = Connection::open(database).unwrap();
    connection
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM change_lifecycles),
                (SELECT COUNT(*) FROM change_events),
                (SELECT COUNT(*) FROM change_applies),
                (SELECT group_concat(state_digest, ',')
                   FROM (SELECT state_digest FROM change_lifecycles ORDER BY change_id)),
                (SELECT group_concat(payload_digest, ',')
                   FROM (SELECT payload_digest FROM change_events ORDER BY event_id))",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap()
}

fn assert_safe(value: &Value) {
    let text = value.to_string();
    for forbidden in [
        "opaque-sensitive-sentinel",
        "repository_root",
        "journal_json",
        "transcript",
        "credentials",
        "cwd_relative",
        "argv",
    ] {
        assert!(!text.contains(forbidden), "response leaked {forbidden}");
    }
}

#[tokio::test]
async fn lifecycle_routes_are_authenticated_bounded_ordered_safe_and_read_only() {
    let (state, database) = test_state();
    let app = test_router(state.clone());

    let (status, _) = get(&app, "/api/v1/lifecycles", false).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, empty) = get(&app, "/api/v1/lifecycles", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["schema"], LIST_SCHEMA);
    assert!(empty["items"].as_array().unwrap().is_empty());

    let mut change_ids = [
        seed_lifecycle(&state, &database, "http-z", false),
        seed_lifecycle(&state, &database, "http-a", true),
        seed_lifecycle(&state, &database, "http-m", false),
    ];
    change_ids.sort_by_key(ToString::to_string);
    let transitioned_id = ChangeId::from_digest(Digest::blake3("http-a:change"));
    seed_recovery(&database, &transitioned_id);
    let before = lifecycle_snapshot(&database);

    let (status, list) = get(&app, "/api/v1/lifecycles?limit=2", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["schema"], LIST_SCHEMA);
    assert_eq!(list["limit"], 2);
    assert_eq!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["change_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        change_ids[..2].iter().map(ChangeId::as_str).collect::<Vec<_>>()
    );
    assert_safe(&list);

    let (status, maximum) = get(&app, "/api/v1/lifecycles?limit=100", true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(maximum["limit"], 100);
    assert_eq!(maximum["items"].as_array().unwrap().len(), 3);
    assert_safe(&maximum);

    for path in [
        "/api/v1/lifecycles?limit=0",
        "/api/v1/lifecycles?limit=101",
        "/api/v1/lifecycles?limit=abc",
        "/api/v1/lifecycles?unknown=1",
    ] {
        let (status, error) = get(&app, path, true).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error["schema"], ERROR_SCHEMA);
    }

    let detail_path = format!("/api/v1/lifecycles/{transitioned_id}");
    let (status, detail) = get(&app, &detail_path, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["schema"], DETAIL_SCHEMA);
    assert_eq!(detail["lifecycle"]["change_id"], transitioned_id.as_str());
    assert_eq!(detail["lifecycle"]["phase"], "implement");
    assert_eq!(detail["lifecycle"]["recovery"]["status"], "recovery_conflict");
    assert_safe(&detail);

    let events_path = format!("{detail_path}/events");
    let (status, events) = get(&app, &events_path, true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events["schema"], EVENTS_SCHEMA);
    assert_eq!(events["limit"], needle_core::MAX_LIFECYCLE_EVENTS);
    assert_eq!(
        events["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["sequence"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(events["items"][0]["action"]["kind"], "created");
    assert_eq!(events["items"][1]["action"]["kind"], "complete_explore");
    assert_safe(&events);

    let missing = ChangeId::from_digest(Digest::blake3(b"http:missing"));
    let (status, missing_body) = get(&app, &format!("/api/v1/lifecycles/{missing}"), true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing_body["code"], "not_found");
    let (status, malformed) = get(&app, "/api/v1/lifecycles/not-a-change", true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(malformed["code"], "invalid_change_id");

    assert_eq!(lifecycle_snapshot(&database), before);

    let connection = Connection::open(&database).unwrap();
    connection.execute("DROP TRIGGER change_events_no_update", []).unwrap();
    let payload = connection
        .query_row(
            "SELECT payload_json FROM change_events
             WHERE change_id=?1 AND lifecycle_sequence=0",
            [transitioned_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    let mut payload: Value = serde_json::from_str(&payload).unwrap();
    payload["status"] = serde_json::json!("unknown_status");
    let payload = serde_json::to_string(&payload).unwrap();
    connection
        .execute(
            "UPDATE change_events SET payload_json=?2, payload_digest=?3
             WHERE change_id=?1 AND lifecycle_sequence=0",
            params![
                transitioned_id.to_string(),
                &payload,
                Digest::blake3(payload.as_bytes()).to_string(),
            ],
        )
        .unwrap();
    drop(connection);
    for path in ["/api/v1/lifecycles".to_owned(), detail_path, events_path] {
        let (status, error) = get(&app, &path, true).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error["code"], "lifecycle_corruption");
        assert_safe(&error);
    }

    drop(app);
    drop(state);
    std::fs::remove_file(database).unwrap();
}
