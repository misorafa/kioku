//! The JSON HTTP API of spec §9 + M2 §9 (`/api/v1/*`): thin handlers that run one `Store` call
//! each on the blocking pool, plus the error type mapping core errors to status codes.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use kioku_core::{
    Error, HandoffInput, NewObservation, SessionStartRequest, Store, VERSION, WritePageRequest,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::shared::{DEFAULT_HTTP_LIMIT, blocking, clamp_limit, resolve_scope, with_project_hint};

/// Version of the kioku-server crate, reported by `GET /api/v1/status`.
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// An HTTP error response: `{error: "<message>"}` with a status code.
#[derive(Debug)]
pub struct ApiError {
    /// HTTP status.
    pub status: StatusCode,
    /// Human-readable message.
    pub message: String,
}

impl ApiError {
    /// A 400 error.
    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}

impl From<Error> for ApiError {
    fn from(err: Error) -> ApiError {
        let status = match &err {
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::InvalidInput(_) => StatusCode::BAD_REQUEST,
            Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = format!("{err:#}"), "request failed");
        }
        ApiError {
            status,
            message: err.to_string(),
        }
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rej: JsonRejection) -> ApiError {
        ApiError::bad_request(rej.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rej: QueryRejection) -> ApiError {
        ApiError::bad_request(rej.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

/// Serializes a handler result into a JSON response body.
fn to_json<T: serde::Serialize>(value: T) -> ApiResult {
    serde_json::to_value(value)
        .map(Json)
        .map_err(|e| Error::Internal(anyhow::Error::from(e)).into())
}

/// Routes that require the bearer token (everything except health).
pub fn protected_routes(store: Arc<Store>) -> Router {
    Router::new()
        .route("/api/v1/sessions/start", post(start_session))
        .route("/api/v1/sessions/{id}", get(session_info))
        .route("/api/v1/sessions/{id}/context", get(session_context))
        .route("/api/v1/sessions/{id}/finalize", post(finalize_session))
        .route("/api/v1/observations", post(add_observation))
        .route("/api/v1/search", get(search))
        .route("/api/v1/pages/{*path}", get(read_page))
        .route("/api/v1/pages", put(write_page))
        .route("/api/v1/handoffs/pending", get(pending_handoff))
        .route("/api/v1/handoffs", post(write_handoff))
        .route("/api/v1/status", get(status))
        .route("/api/v1/reindex", post(reindex))
        .with_state(store)
}

/// Routes reachable without a token.
pub fn public_routes() -> Router {
    Router::new().route("/api/v1/health", get(health))
}

/// `GET /api/v1/health` → `{ok, version}`.
async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": VERSION }))
}

/// `POST /api/v1/sessions/start`.
async fn start_session(
    State(store): State<Arc<Store>>,
    body: Result<Json<SessionStartRequest>, JsonRejection>,
) -> ApiResult {
    let Json(req) = body?;
    to_json(blocking(&store, move |s| s.start_session(&req)).await?)
}

/// `GET /api/v1/sessions/{id}`.
async fn session_info(State(store): State<Arc<Store>>, Path(id): Path<String>) -> ApiResult {
    to_json(blocking(&store, move |s| s.session_info(&id)).await?)
}

/// `GET /api/v1/sessions/{id}/context` → the SessionStart response shape, read-only (M2 §9.1).
async fn session_context(State(store): State<Arc<Store>>, Path(id): Path<String>) -> ApiResult {
    to_json(blocking(&store, move |s| s.session_context(&id)).await?)
}

/// Optional body of `POST /api/v1/sessions/{id}/finalize`.
#[derive(Debug, Default, Deserialize)]
struct FinalizeBody {
    #[serde(default)]
    reason: Option<String>,
}

/// `POST /api/v1/sessions/{id}/finalize` (body `{reason?}` is optional).
async fn finalize_session(
    State(store): State<Arc<Store>>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    if !body.iter().all(u8::is_ascii_whitespace) {
        let parsed: FinalizeBody = serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid finalize body: {e}")))?;
        if let Some(reason) = parsed.reason {
            tracing::debug!(session = %id, reason = %reason, "finalize");
        }
    }
    to_json(blocking(&store, move |s| s.finalize_session(&id)).await?)
}

/// `POST /api/v1/observations` → `{seq}`.
async fn add_observation(
    State(store): State<Arc<Store>>,
    body: Result<Json<NewObservation>, JsonRejection>,
) -> ApiResult {
    let Json(obs) = body?;
    let seq = blocking(&store, move |s| s.add_observation(&obs)).await?;
    Ok(Json(json!({ "seq": seq })))
}

/// Query string of `GET /api/v1/search`.
#[derive(Debug, Deserialize)]
struct SearchParams {
    q: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /api/v1/search?q=&project=&scope=&limit=` → `{hits}`.
async fn search(
    State(store): State<Arc<Store>>,
    params: Result<Query<SearchParams>, QueryRejection>,
) -> ApiResult {
    let Query(p) = params?;
    let scope = resolve_scope(p.scope.as_deref(), p.project.as_deref())?;
    let limit = clamp_limit(p.limit, DEFAULT_HTTP_LIMIT);
    let hits = blocking(&store, move |s| s.search(&p.q, &scope, limit)).await?;
    Ok(Json(json!({ "hits": hits })))
}

/// `GET /api/v1/pages/*path` → `{path, frontmatter, body}`.
async fn read_page(State(store): State<Arc<Store>>, Path(path): Path<String>) -> ApiResult {
    to_json(blocking(&store, move |s| s.read_page(&path)).await?)
}

/// `PUT /api/v1/pages` → `{path}`.
async fn write_page(
    State(store): State<Arc<Store>>,
    body: Result<Json<WritePageRequest>, JsonRejection>,
) -> ApiResult {
    let Json(req) = body?;
    let path = blocking(&store, move |s| {
        s.write_page(&req)
            .map_err(|e| with_project_hint(s, e, req.project.as_deref()))
    })
    .await?;
    Ok(Json(json!({ "path": path })))
}

/// Query string of `GET /api/v1/handoffs/pending`.
#[derive(Debug, Deserialize)]
struct PendingParams {
    project: String,
    #[serde(default)]
    accept: bool,
    #[serde(default)]
    session: Option<String>,
}

/// `GET /api/v1/handoffs/pending?project=&accept=&session=` → `{handoff}` (null when none).
async fn pending_handoff(
    State(store): State<Arc<Store>>,
    params: Result<Query<PendingParams>, QueryRejection>,
) -> ApiResult {
    let Query(p) = params?;
    let handoff = blocking(&store, move |s| {
        s.pending_handoff(&p.project, p.accept, p.session.as_deref())
    })
    .await?;
    Ok(Json(json!({ "handoff": handoff })))
}

/// `POST /api/v1/handoffs` → `{id, project_id}`.
async fn write_handoff(
    State(store): State<Arc<Store>>,
    body: Result<Json<HandoffInput>, JsonRejection>,
) -> ApiResult {
    let Json(input) = body?;
    let handoff = blocking(&store, move |s| {
        s.write_handoff(&input)
            .map_err(|e| with_project_hint(s, e, Some(&input.project)))
    })
    .await?;
    Ok(Json(
        json!({ "id": handoff.id, "project_id": handoff.project_id }),
    ))
}

/// `GET /api/v1/status` (M2 §9.2: `version` is this server crate's version).
async fn status(State(store): State<Arc<Store>>) -> ApiResult {
    let (mut report, projects) = blocking(&store, |s| {
        let projects: Vec<String> = s.list_projects()?.into_iter().map(|p| p.id).collect();
        Ok((s.status()?, projects))
    })
    .await?;
    report.version = SERVER_VERSION.to_string();
    // M2 §20.1: `project_ids` is additive (the `kioku mcp` bridge prints it like `/mcp`).
    let mut v =
        serde_json::to_value(report).map_err(|e| Error::Internal(anyhow::Error::from(e)))?;
    v["project_ids"] = json!(projects);
    Ok(Json(v))
}

/// `POST /api/v1/reindex` → `{docs}` (planner addition: remote CLIs have no data dir).
async fn reindex(State(store): State<Arc<Store>>) -> ApiResult {
    let docs = blocking(&store, |s| s.reindex()).await?;
    Ok(Json(json!({ "docs": docs })))
}
