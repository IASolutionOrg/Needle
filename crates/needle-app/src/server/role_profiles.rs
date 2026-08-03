use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, FailedToBufferBody, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use needle_core::{
    CanonicalHasher, Digest, RoleProfileDefinition, RoleProfileDefinitionInput, RoleProfileId,
    RoleProfileRevision, RoleProfileState, WorkerProfile,
};
use needle_runtime::{RoleProfileAuditRecord, RoleProfileStateRecord, RuntimeStore, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{AppState, IfMatchError, require_if_match};

const SCHEMA: &str = "needle.role-profiles/1";
const ERROR_SCHEMA: &str = "needle.role-profile-error/1";
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 100;
const MAX_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RevisionQuery {
    pub revision: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LimitQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivateBody {
    revision: u64,
    #[serde(alias = "confirmed_definition_digest")]
    definition_digest: Digest,
    confirm: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeactivateBody {
    #[serde(alias = "definition_digest")]
    active_definition_digest: Digest,
    confirm: bool,
}

#[derive(Debug, Serialize)]
struct RoleProfileApiError {
    schema: &'static str,
    code: &'static str,
    message: String,
}

#[derive(Debug)]
struct RoleProfileError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl RoleProfileError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into() }
    }

    fn into_response(self) -> Response {
        role_error(self.status, self.code, &self.message)
    }
}

#[derive(Debug, Serialize)]
struct RevisionSummary {
    revision: u64,
    definition_digest: Digest,
    role: needle_core::CodexRole,
    host: needle_core::CodexHost,
    model: String,
    reasoning: needle_core::ReasoningLevel,
    service_tier: needle_core::ServiceTier,
    state: RoleProfileState,
    created_unix_ms: u64,
    activated_unix_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct ProfileSummary {
    profile_id: RoleProfileId,
    role: needle_core::CodexRole,
    host: needle_core::CodexHost,
    latest_revision: u64,
    latest_definition_digest: Digest,
    active_revision: Option<u64>,
    active_definition_digest: Option<Digest>,
    state: RoleProfileState,
    state_digest: Digest,
    updated_unix_ms: u64,
}

pub(super) fn routes(router: Router<AppState>) -> Router<AppState> {
    let role_profiles = Router::new()
        .route("/api/v1/role-profiles", get(list_role_profiles))
        .route("/api/v1/role-profiles/{id}", get(get_role_profile))
        .route("/api/v1/role-profiles/{id}/revisions", get(list_revisions))
        .route("/api/v1/role-profiles/{id}/audit", get(list_audit))
        .route("/api/v1/role-profiles/{id}/preflight", post(preflight))
        .route("/api/v1/role-profiles/{id}/draft", post(save_draft))
        .route("/api/v1/role-profiles/{id}/activate", post(activate))
        .route("/api/v1/role-profiles/{id}/deactivate", post(deactivate))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));
    router.merge(role_profiles)
}

pub(super) async fn list_role_profiles(
    State(state): State<AppState>,
    query: Result<Query<LimitQuery>, QueryRejection>,
) -> Response {
    let query = match parse_query(query) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let limit = match bounded_limit(query.limit) {
        Ok(limit) => limit,
        Err(error) => return error.into_response(),
    };
    let states = match state.store.list_role_profile_states(limit) {
        Ok(states) => states,
        Err(error) => return store_error(error),
    };
    let mut items = Vec::with_capacity(states.len());
    for state_record in states {
        let profile_id = state_record.profile_id.clone();
        let profile_state = state_record.state();
        let revision =
            match state.store.read_role_profile_revision(&profile_id, state_record.latest_revision)
            {
                Ok(revision) => revision,
                Err(error) => return store_error(error),
            };
        items.push(ProfileSummary {
            profile_id,
            role: revision.definition.role,
            host: revision.definition.host,
            latest_revision: state_record.latest_revision,
            latest_definition_digest: state_record.latest_definition_digest,
            active_revision: state_record.active_revision,
            active_definition_digest: state_record.active_definition_digest,
            state: profile_state,
            state_digest: state_record.state_digest,
            updated_unix_ms: state_record.updated_unix_ms,
        });
    }
    Json(json!({"schema": SCHEMA, "items": items, "limit": limit})).into_response()
}

pub(super) async fn get_role_profile(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<RevisionQuery>, QueryRejection>,
) -> Response {
    let query = match parse_query(query) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let id = match parse_id(&id) {
        Ok(id) => id,
        Err(error) => return error.into_response(),
    };
    let state_record = match state.store.role_profile_state(&id) {
        Ok(state_record) => state_record,
        Err(error) => return store_error(error),
    };
    let revision_number = query.revision.unwrap_or(state_record.latest_revision);
    let revision = match state.store.read_role_profile_revision(&id, revision_number) {
        Ok(revision) => revision,
        Err(error) => return store_error(error),
    };
    profile_detail_response(&state_record, &revision)
}

pub(super) async fn list_revisions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<LimitQuery>, QueryRejection>,
) -> Response {
    let query = match parse_query(query) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let id = match parse_id(&id) {
        Ok(id) => id,
        Err(error) => return error.into_response(),
    };
    let limit = match bounded_limit(query.limit) {
        Ok(limit) => limit,
        Err(error) => return error.into_response(),
    };
    let (revisions, total) = match state.store.list_role_profile_revisions_bounded(&id, limit) {
        Ok(revisions) => revisions,
        Err(error) => return store_error(error),
    };
    let items = revisions.iter().map(revision_summary).collect::<Vec<_>>();
    Json(
        json!({"schema": SCHEMA, "profile_id": id, "items": items, "limit": limit, "total": total}),
    )
    .into_response()
}

pub(super) async fn list_audit(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<LimitQuery>, QueryRejection>,
) -> Response {
    let query = match parse_query(query) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let id = match parse_id(&id) {
        Ok(id) => id,
        Err(error) => return error.into_response(),
    };
    let limit = match bounded_limit(query.limit) {
        Ok(limit) => limit,
        Err(error) => return error.into_response(),
    };
    let records = match state.store.read_role_profile_audit(&id, limit) {
        Ok(records) => records,
        Err(error) => return store_error(error),
    };
    let items = records.iter().map(audit_summary).collect::<Vec<_>>();
    Json(json!({"schema": SCHEMA, "profile_id": id, "items": items, "limit": limit}))
        .into_response()
}

pub(super) async fn preflight(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(error) => return body_rejection(error),
    };
    let id = match parse_id(&id) {
        Ok(id) => id,
        Err(error) => return error.into_response(),
    };
    let input = match parse_definition_input(&body) {
        Ok(input) => input,
        Err(error) => return error.into_response(),
    };
    if input.profile_id != id {
        return role_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "profile_id_mismatch",
            "request profile_id does not match the path profile id",
        );
    }
    let definition = match RoleProfileDefinition::new(input) {
        Ok(definition) => definition,
        Err(error) => return preflight_failure("invalid", None, None, vec![error.to_string()]),
    };
    let (worker, worker_failures) = worker_projection(&definition);
    let (operation, if_match) = match current_state_or_absence(&state.store, &id) {
        Ok(Some(state_record)) => ("revise", state_record.state_digest),
        Ok(None) => ("create", absence_digest(&id)),
        Err(error) => return store_error(error),
    };
    let failures = worker_failures;
    if failures.is_empty() {
        Json(json!({
            "schema": SCHEMA,
            "profile_id": id,
            "operation": operation,
            "passed": true,
            "failures": [],
            "if_match": if_match,
            "definition": definition,
            "definition_digest": definition.definition_digest,
            "worker_profile": worker,
            "worker_profile_digest": worker.as_ref().map(|value| value.definition_digest)
        }))
        .into_response()
    } else {
        preflight_failure(operation, Some(definition), worker, failures)
    }
}

pub(super) async fn save_draft(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(error) => return body_rejection(error),
    };
    let id = match parse_id(&id) {
        Ok(id) => id,
        Err(error) => return error.into_response(),
    };
    let input = match parse_definition_input(&body) {
        Ok(input) => input,
        Err(error) => return error.into_response(),
    };
    if input.profile_id != id {
        return role_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "profile_id_mismatch",
            "request profile_id does not match the path profile id",
        );
    }
    let definition = match RoleProfileDefinition::new(input) {
        Ok(definition) => definition,
        Err(error) => {
            return role_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_definition",
                &error.to_string(),
            );
        }
    };
    let current = match current_state_or_absence(&state.store, &id) {
        Ok(current) => current,
        Err(error) => return store_error(error),
    };
    let expected = current
        .as_ref()
        .map(|state_record| state_record.state_digest)
        .unwrap_or_else(|| absence_digest(&id));
    if let Err(error) = require_if_match(&headers, &expected.to_string()) {
        return if_match_error(error, "role-profile state changed");
    }
    let result = match current {
        Some(_) => state.store.revise_role_profile(&id, expected, definition),
        None => state.store.create_role_profile(definition),
    };
    let revision = match result {
        Ok(revision) => revision,
        Err(StoreError::RoleProfileConflict(message)) => {
            let observed = match current_state_or_absence(&state.store, &id) {
                Ok(Some(state_record)) => state_record.state_digest,
                Ok(None) => absence_digest(&id),
                Err(error) => return store_error(error),
            };
            if observed != expected {
                return role_error(StatusCode::PRECONDITION_FAILED, "stale_or_duplicate", &message);
            }
            return role_error(StatusCode::CONFLICT, "conflict", &message);
        }
        Err(error) => return store_error(error),
    };
    let state_record = match state.store.role_profile_state(&id) {
        Ok(state_record) => state_record,
        Err(error) => return store_error(error),
    };
    Json(json!({
        "schema": SCHEMA,
        "operation": if revision.revision == 1 { "create" } else { "revise" },
        "profile": profile_detail(&state_record, &revision),
        "state_digest": state_record.state_digest
    }))
    .into_response()
}

pub(super) async fn activate(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(error) => return body_rejection(error),
    };
    let id = match parse_id(&id) {
        Ok(id) => id,
        Err(error) => return error.into_response(),
    };
    let body: ActivateBody = match parse_json(&body) {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    if !body.confirm {
        return role_error(
            StatusCode::BAD_REQUEST,
            "confirmation_required",
            "activation requires confirm=true",
        );
    }
    let state_record = match state.store.role_profile_state(&id) {
        Ok(state_record) => state_record,
        Err(error) => return store_error(error),
    };
    if let Err(error) = require_if_match(&headers, &state_record.state_digest.to_string()) {
        return if_match_error(error, "role-profile state changed");
    }
    if body.revision == 0 {
        return role_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_revision",
            "activation revision must be nonzero",
        );
    }
    let revision = match state.store.read_role_profile_revision(&id, body.revision) {
        Ok(revision) => revision,
        Err(error) => return store_error(error),
    };
    if revision.definition.definition_digest != body.definition_digest {
        return role_error(
            StatusCode::CONFLICT,
            "definition_digest_mismatch",
            "activation digest does not match the selected revision",
        );
    }
    let (worker, failures) = worker_projection(&revision.definition);
    if !failures.is_empty() {
        return preflight_failure("activate", Some(revision.definition), worker, failures);
    }
    let activated = match state.store.activate_role_profile_checked(
        &id,
        body.revision,
        state_record.state_digest,
        state_record.active_definition_digest,
    ) {
        Ok(revision) => revision,
        Err(StoreError::RoleProfileConflict(message)) => {
            return role_error(StatusCode::PRECONDITION_FAILED, "stale_or_conflict", &message);
        }
        Err(error) => return store_error(error),
    };
    let next_state = match state.store.role_profile_state(&id) {
        Ok(state_record) => state_record,
        Err(error) => return store_error(error),
    };
    Json(json!({"schema": SCHEMA, "operation": "activate", "profile": profile_detail(&next_state, &activated), "state_digest": next_state.state_digest})).into_response()
}

pub(super) async fn deactivate(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(error) => return body_rejection(error),
    };
    let id = match parse_id(&id) {
        Ok(id) => id,
        Err(error) => return error.into_response(),
    };
    let body: DeactivateBody = match parse_json(&body) {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    if !body.confirm {
        return role_error(
            StatusCode::BAD_REQUEST,
            "confirmation_required",
            "deactivation requires confirm=true",
        );
    }
    let state_record = match state.store.role_profile_state(&id) {
        Ok(state_record) => state_record,
        Err(error) => return store_error(error),
    };
    if let Err(error) = require_if_match(&headers, &state_record.state_digest.to_string()) {
        return if_match_error(error, "role-profile state changed");
    }
    let Some(active_digest) = state_record.active_definition_digest else {
        return role_error(StatusCode::CONFLICT, "not_active", "profile has no active revision");
    };
    if body.active_definition_digest != active_digest {
        return role_error(
            StatusCode::CONFLICT,
            "active_digest_mismatch",
            "active role-profile digest changed",
        );
    }
    let deactivated = match state.store.deactivate_role_profile_checked(
        &id,
        state_record.state_digest,
        Some(active_digest),
    ) {
        Ok(revision) => revision,
        Err(StoreError::RoleProfileConflict(message)) => {
            return role_error(StatusCode::PRECONDITION_FAILED, "stale_or_conflict", &message);
        }
        Err(error) => return store_error(error),
    };
    let next_state = match state.store.role_profile_state(&id) {
        Ok(state_record) => state_record,
        Err(error) => return store_error(error),
    };
    Json(json!({"schema": SCHEMA, "operation": "deactivate", "profile": profile_detail(&next_state, &deactivated), "state_digest": next_state.state_digest})).into_response()
}

pub(super) fn control_plane_envelope(store: &RuntimeStore) -> Result<Value, StoreError> {
    let states = store.list_role_profile_states(DEFAULT_LIMIT)?;
    let mut summaries = Vec::with_capacity(states.len());
    for state in states {
        let profile_id = state.profile_id.clone();
        let profile_state = state.state();
        let revision = store.read_role_profile_revision(&profile_id, state.latest_revision)?;
        summaries.push(ProfileSummary {
            profile_id,
            role: revision.definition.role,
            host: revision.definition.host,
            latest_revision: state.latest_revision,
            latest_definition_digest: state.latest_definition_digest,
            active_revision: state.active_revision,
            active_definition_digest: state.active_definition_digest,
            state: profile_state,
            state_digest: state.state_digest,
            updated_unix_ms: state.updated_unix_ms,
        });
    }
    Ok(json!({
        "schema": SCHEMA,
        "status": "configuration_only",
        "capability": "codex",
        "codex": {"available": true, "execution_binding": false},
        "non_codex": {"available": false, "reason": "only codex host profiles are supported"},
        "items": summaries,
        "bounded": {"limit": DEFAULT_LIMIT}
    }))
}

fn profile_detail_response(
    state: &RoleProfileStateRecord,
    revision: &RoleProfileRevision,
) -> Response {
    Json(json!({"schema": SCHEMA, "profile": profile_detail(state, revision)})).into_response()
}

fn profile_detail(state: &RoleProfileStateRecord, revision: &RoleProfileRevision) -> Value {
    let worker = revision.to_worker_profile().ok();
    let preflight = preflight_summary(&revision.definition);
    json!({
        "profile_id": revision.profile_id,
        "revision": revision.revision,
        "state": revision.state,
        "definition": revision.definition,
        "definition_digest": revision.definition.definition_digest,
        "worker_profile": worker,
        "worker_profile_digest": worker.as_ref().map(|profile| profile.definition_digest),
        "preflight": preflight,
        "created_unix_ms": revision.created_unix_ms,
        "activated_unix_ms": revision.activated_unix_ms,
        "state_digest": state.state_digest,
        "latest_revision": state.latest_revision,
        "latest_definition_digest": state.latest_definition_digest,
        "active_revision": state.active_revision,
        "active_definition_digest": state.active_definition_digest,
        "updated_unix_ms": state.updated_unix_ms
    })
}

fn revision_summary(revision: &RoleProfileRevision) -> RevisionSummary {
    RevisionSummary {
        revision: revision.revision,
        definition_digest: revision.definition.definition_digest,
        role: revision.definition.role,
        host: revision.definition.host,
        model: revision.definition.model.clone(),
        reasoning: revision.definition.reasoning,
        service_tier: revision.definition.service_tier,
        state: revision.state,
        created_unix_ms: revision.created_unix_ms,
        activated_unix_ms: revision.activated_unix_ms,
    }
}

fn audit_summary(record: &RoleProfileAuditRecord) -> Value {
    json!({
        "audit_id": record.audit_id,
        "profile_id": record.profile_id,
        "revision": record.revision,
        "definition_digest": record.definition_digest,
        "operation": record.operation,
        "prior_state": record.prior_state,
        "resulting_state": record.resulting_state,
        "prior_state_digest": record.prior_state_digest,
        "resulting_state_digest": record.resulting_state_digest,
        "prior_active_revision": record.prior_active_revision,
        "prior_active_digest": record.prior_active_digest,
        "resulting_active_revision": record.resulting_active_revision,
        "resulting_active_digest": record.resulting_active_digest,
        "created_unix_ms": record.created_unix_ms
    })
}

fn parse_id(value: &str) -> Result<RoleProfileId, RoleProfileError> {
    RoleProfileId::new(value.to_owned()).map_err(|error| {
        RoleProfileError::new(StatusCode::BAD_REQUEST, "invalid_profile_id", error.to_string())
    })
}

fn parse_query<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, RoleProfileError> {
    query.map(|Query(value)| value).map_err(|_| {
        RoleProfileError::new(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "role-profile query is invalid",
        )
    })
}

fn body_rejection(error: BytesRejection) -> Response {
    match error {
        BytesRejection::FailedToBufferBody(error) => match error {
            FailedToBufferBody::LengthLimitError(_) => role_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "role-profile request body exceeds the bounded limit",
            ),
            FailedToBufferBody::UnknownBodyError(_) => role_error(
                StatusCode::BAD_REQUEST,
                "body_unavailable",
                "role-profile request body could not be read",
            ),
            _ => role_error(
                StatusCode::BAD_REQUEST,
                "body_unavailable",
                "role-profile request body could not be read",
            ),
        },
        _ => role_error(
            StatusCode::BAD_REQUEST,
            "body_unavailable",
            "role-profile request body could not be read",
        ),
    }
}

fn bounded_limit(limit: Option<usize>) -> Result<usize, RoleProfileError> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(RoleProfileError::new(
            StatusCode::BAD_REQUEST,
            "invalid_limit",
            "limit must be between 1 and 100",
        ));
    }
    Ok(limit)
}

fn parse_definition_input(body: &Bytes) -> Result<RoleProfileDefinitionInput, RoleProfileError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(RoleProfileError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "role-profile request body exceeds the bounded limit",
        ));
    }
    parse_json(body)
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, RoleProfileError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(RoleProfileError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "role-profile request body exceeds the bounded limit",
        ));
    }
    serde_json::from_slice(body).map_err(|error| {
        RoleProfileError::new(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            format!("request JSON is invalid: {error}"),
        )
    })
}

fn worker_projection(definition: &RoleProfileDefinition) -> (Option<WorkerProfile>, Vec<String>) {
    match definition.to_worker_profile() {
        Ok(worker) => (Some(worker), Vec::new()),
        Err(error) => (None, vec![error.to_string()]),
    }
}

fn preflight_summary(definition: &RoleProfileDefinition) -> Value {
    let (worker, failures) = worker_projection(definition);
    json!({
        "passed": failures.is_empty(),
        "failures": failures.into_iter().take(16).collect::<Vec<_>>(),
        "worker_profile_digest": worker.as_ref().map(|value| value.definition_digest),
    })
}

fn current_state_or_absence(
    store: &RuntimeStore,
    id: &RoleProfileId,
) -> Result<Option<RoleProfileStateRecord>, StoreError> {
    match store.role_profile_state(id) {
        Ok(state) => Ok(Some(state)),
        Err(StoreError::RoleProfileNotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn absence_digest(id: &RoleProfileId) -> Digest {
    let mut hasher = CanonicalHasher::new(b"needle-codex-role-profile-absence-v1");
    hasher.field_str(id.as_str());
    hasher.finish()
}

fn preflight_failure(
    operation: &str,
    definition: Option<RoleProfileDefinition>,
    worker: Option<WorkerProfile>,
    failures: Vec<String>,
) -> Response {
    role_json(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({
            "schema": SCHEMA,
            "operation": operation,
            "passed": false,
            "failures": failures.into_iter().take(16).collect::<Vec<_>>(),
            "definition": definition,
            "definition_digest": definition.as_ref().map(|value| value.definition_digest),
            "worker_profile": worker,
            "worker_profile_digest": worker.as_ref().map(|value| value.definition_digest)
        }),
    )
}

fn if_match_error(error: IfMatchError, message: &str) -> Response {
    match error {
        IfMatchError::Missing => role_error(
            StatusCode::PRECONDITION_REQUIRED,
            "if_match_required",
            "If-Match is required",
        ),
        IfMatchError::Changed => {
            role_error(StatusCode::PRECONDITION_FAILED, "if_match_changed", message)
        }
    }
}

fn store_error(error: StoreError) -> Response {
    match error {
        StoreError::RoleProfileNotFound(message) => {
            role_error(StatusCode::NOT_FOUND, "not_found", &message)
        }
        StoreError::RoleProfileValidation(message) => {
            role_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_role_profile", &message)
        }
        StoreError::RoleProfileConflict(message) => {
            role_error(StatusCode::CONFLICT, "conflict", &message)
        }
        StoreError::RoleProfileCorruption(message) => {
            role_error(StatusCode::INTERNAL_SERVER_ERROR, "role_profile_corruption", &message)
        }
        other => role_error(StatusCode::INTERNAL_SERVER_ERROR, "storage_error", &other.to_string()),
    }
}

fn role_error(status: StatusCode, code: &'static str, message: &str) -> Response {
    role_json(
        status,
        json!(RoleProfileApiError { schema: ERROR_SCHEMA, code, message: message.to_owned() }),
    )
}

fn role_json(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

#[cfg(test)]
#[path = "role_profiles/tests.rs"]
mod tests;
