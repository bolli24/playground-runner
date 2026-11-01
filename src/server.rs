//! The HTTP API used by the playground.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use tokio::sync::Semaphore;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::config::Config;
use crate::executor::{ExecError, Executor, RunResult};
use crate::job::{Job, JobError};
use crate::registry::{Example, Registry, Snapshot};

pub struct AppState {
    pub executor: Arc<Executor>,
    pub registry: Arc<Registry>,
    /// Running plus waiting jobs. A request that can't get a permit is rejected with 429.
    admission: Arc<Semaphore>,
    /// Running jobs.
    slots: Arc<Semaphore>,
}

impl AppState {
    pub fn new(executor: Arc<Executor>, registry: Arc<Registry>) -> Self {
        let server = &executor.config().server;
        Self {
            admission: Arc::new(Semaphore::new(server.max_concurrent + server.max_queued)),
            slots: Arc::new(Semaphore::new(server.max_concurrent.max(1))),
            executor,
            registry,
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let config = state.executor.config();
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/languages", get(languages))
        .route("/v1/languages/{language}", get(language))
        .route("/v1/run", post(run))
        .route(
            "/v1/assets/{language}/{revision}/linter/{*path}",
            get(linter_asset),
        )
        .layer(DefaultBodyLimit::max(body_limit(config)))
        .layer(cors(config))
        .with_state(state)
}

/// Sources and stdin may grow when encoded as JSON strings; leave room for that.
fn body_limit(config: &Config) -> usize {
    2 * (config.limits.max_source_bytes + config.limits.max_stdin_bytes) + 64 * 1024
}

fn cors(config: &Config) -> CorsLayer {
    let origins: Vec<HeaderValue> = config
        .server
        .allowed_origins
        .iter()
        .filter_map(|origin| match HeaderValue::from_str(origin) {
            Ok(value) => Some(value),
            Err(_) => {
                eprintln!("ignoring invalid allowed origin {origin:?}");
                None
            }
        })
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE])
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LanguageInfo {
    language: String,
    revision: String,
    contract: u32,
    extensions: Vec<String>,
    capabilities: Vec<String>,
    views: Vec<String>,
    /// URL of the linter module, relative to the API's origin.
    linter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    examples: Option<Vec<Example>>,
}

impl LanguageInfo {
    fn new(snapshot: &Snapshot, with_examples: bool) -> Self {
        let image = &snapshot.image;
        Self {
            language: image.language.clone(),
            revision: image.revision.clone(),
            contract: image.contract,
            extensions: image.extensions.clone(),
            capabilities: image.capabilities.clone(),
            views: image.views.clone(),
            linter: snapshot.has_linter.then(|| {
                format!(
                    "/v1/assets/{}/{}/linter/index.js",
                    image.language, image.revision
                )
            }),
            examples: with_examples.then(|| snapshot.examples.clone()),
        }
    }
}

async fn languages(State(state): State<Arc<AppState>>) -> Json<Vec<LanguageInfo>> {
    Json(
        state
            .registry
            .all()
            .iter()
            .map(|snapshot| LanguageInfo::new(snapshot, false))
            .collect(),
    )
}

async fn language(
    State(state): State<Arc<AppState>>,
    Path(language): Path<String>,
) -> Result<Json<LanguageInfo>, ApiError> {
    let snapshot = state
        .registry
        .get(&language)
        .ok_or(ApiError::UnknownLanguage(language))?;
    Ok(Json(LanguageInfo::new(&snapshot, true)))
}

async fn run(
    State(state): State<Arc<AppState>>,
    Json(job): Json<Job>,
) -> Result<Json<RunResult>, ApiError> {
    let snapshot = state
        .registry
        .get(&job.language)
        .ok_or_else(|| ApiError::UnknownLanguage(job.language.clone()))?;
    job.validate(&snapshot.image, &state.executor.config().limits)?;
    let admission = state
        .admission
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::Busy)?;

    // Run in a task of its own: if the client disconnects, the handler is dropped, but the job
    // still finishes and cleans up its container.
    let task_state = state.clone();
    let task = tokio::spawn(async move {
        let _admission = admission;
        let _slot = task_state
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore is never closed");
        task_state.executor.run_on(&snapshot.image, &job).await
    });
    let result = task
        .await
        .map_err(|err| ApiError::Internal(format!("job failed: {err}")))?;
    Ok(Json(result?))
}

async fn linter_asset(
    State(state): State<Arc<AppState>>,
    Path((language, revision, path)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let file = state
        .registry
        .get(&language)
        .filter(|snapshot| snapshot.image.revision == revision)
        .and_then(|snapshot| snapshot.linter_file(&path))
        .ok_or(ApiError::NotFound)?;
    let bytes = tokio::fs::read(&file)
        .await
        .map_err(|err| ApiError::Internal(format!("unable to read {}: {err}", file.display())))?;
    let content_type = match file.extension().and_then(|ext| ext.to_str()) {
        Some("js" | "mjs") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            // The revision is part of the URL, so the file at this URL never changes
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response())
}

enum ApiError {
    UnknownLanguage(String),
    NotFound,
    Invalid(JobError),
    Busy,
    Internal(String),
}

impl From<JobError> for ApiError {
    fn from(err: JobError) -> Self {
        Self::Invalid(err)
    }
}

impl From<ExecError> for ApiError {
    fn from(err: ExecError) -> Self {
        match err {
            ExecError::Job(err) => Self::Invalid(err),
            ExecError::UnknownLanguage(language) => Self::UnknownLanguage(language),
            err => Self::Internal(err.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::UnknownLanguage(language) => (
                StatusCode::NOT_FOUND,
                format!("unknown language '{language}'"),
            ),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            Self::Invalid(err) => (StatusCode::BAD_REQUEST, err.to_string()),
            Self::Busy => {
                let body =
                    Json(json!({ "error": "too many programs are running, try again soon" }));
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, "5")],
                    body,
                )
                    .into_response();
            }
            Self::Internal(message) => {
                eprintln!("internal error: {message}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}
