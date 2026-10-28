//! HTTP API that web pages call to reach local printers.

mod guard;
mod zebra;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{middleware, Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;
use zprint_core::status::{parse_host_status, HostStatus};
use zprint_core::{Error, PrinterInfo, Transport};

pub(crate) const MAX_BODY: usize = 32 * 1024 * 1024;
const DEFAULT_READ_TIMEOUT_MS: u64 = 3_000;
const MAX_READ_TIMEOUT_MS: u64 = 30_000;

const TEST_PAGE: &str = include_str!("../../../js/test.html");
const CLIENT_JS: &str = include_str!("../../../js/zprint.js");

/// The printers and web origins currently in effect. Replaced as a whole on reload.
pub struct Printers {
    pub list: Vec<Arc<dyn Transport>>,
    pub default_printer: Option<String>,
    /// Extra web origins allowed to call the API, e.g. "https://app.example.com".
    pub allowed_origins: Vec<String>,
}

impl Printers {
    pub fn get(&self, id: &str) -> Option<Arc<dyn Transport>> {
        self.list.iter().find(|p| p.info().id == id).cloned()
    }

    /// The chosen default printer, or the first one when none is chosen.
    pub fn default(&self) -> Option<Arc<dyn Transport>> {
        match &self.default_printer {
            Some(id) => self.get(id),
            None => self.list.first().cloned(),
        }
    }
}

pub type Loader = Box<dyn Fn() -> anyhow::Result<Printers> + Send + Sync>;

const MAX_BLOCKED: usize = 20;

pub struct AppState {
    /// Port the agent listens on; its own origins are always allowed.
    pub port: u16,
    /// Second port for Zebra BrowserPrint compatibility (9100), when enabled.
    pub zebra_port: Option<u16>,
    current: RwLock<Arc<Printers>>,
    loader: Loader,
    changed: watch::Sender<u64>,
    /// Printer replies waiting for a BrowserPrint /read, by printer id.
    replies: Mutex<HashMap<String, Vec<u8>>>,
    /// Web origins that tried to print but are not allowed, newest last.
    blocked: Mutex<Vec<String>>,
}

impl AppState {
    /// `loader` builds the printer set from the config; it runs now and on every reload.
    pub fn new(port: u16, zebra_port: Option<u16>, loader: Loader) -> anyhow::Result<Self> {
        let first = loader()?;
        Ok(Self {
            port,
            zebra_port,
            current: RwLock::new(Arc::new(first)),
            loader,
            changed: watch::channel(0).0,
            replies: Mutex::new(HashMap::new()),
            blocked: Mutex::new(Vec::new()),
        })
    }

    pub fn printers(&self) -> Arc<Printers> {
        self.current.read().unwrap().clone()
    }

    pub fn reload(&self) -> anyhow::Result<()> {
        let fresh = (self.loader)()?;
        self.blocked
            .lock()
            .unwrap()
            .retain(|o| !fresh.allowed_origins.contains(o));
        *self.current.write().unwrap() = Arc::new(fresh);
        self.changed.send_modify(|n| *n += 1);
        tracing::info!("reloaded config");
        Ok(())
    }

    /// Fires after every reload (used by the tray to refresh its menu).
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Remember an origin that was refused, so Settings can offer to allow it.
    pub(crate) fn note_blocked(&self, origin: &str) {
        let mut blocked = self.blocked.lock().unwrap();
        blocked.retain(|o| o != origin);
        blocked.push(origin.to_owned());
        if blocked.len() > MAX_BLOCKED {
            blocked.remove(0);
        }
    }

    fn printer(&self, id: &str) -> Result<Arc<dyn Transport>, ApiError> {
        self.printers()
            .get(id)
            .ok_or_else(|| Error::NotFound(id.to_owned()).into())
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(TEST_PAGE) }))
        .route("/zprint.js", get(client_js))
        .route("/v1/version", get(version))
        .route("/v1/printers", get(list_printers))
        .route("/v1/printers/default", get(default_printer))
        .route("/v1/printers/{id}/write", post(write))
        .route("/v1/printers/{id}/read", post(read))
        .route("/v1/printers/{id}/status", get(status))
        .route("/v1/admin/reload", post(reload))
        .route("/v1/admin/blocked", get(blocked))
        .merge(zebra::routes())
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(state.clone(), guard::guard))
        .with_state(state)
}

async fn client_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], CLIENT_JS)
}

async fn version() -> Json<serde_json::Value> {
    Json(json!({ "name": "Z Browser Print", "version": env!("CARGO_PKG_VERSION") }))
}

async fn list_printers(State(st): State<Arc<AppState>>) -> Json<Vec<PrinterInfo>> {
    Json(st.printers().list.iter().map(|p| p.info().clone()).collect())
}

async fn default_printer(State(st): State<Arc<AppState>>) -> Result<Json<PrinterInfo>, ApiError> {
    let printer = st
        .printers()
        .default()
        .ok_or_else(|| Error::NotFound("no printers configured".into()))?;
    Ok(Json(printer.info().clone()))
}

async fn write(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    if body.is_empty() {
        return Err(ApiError::BadRequest("request body is empty"));
    }
    let printer = st.printer(&id)?;
    printer.write(&body).await?;
    tracing::info!(printer = %id, bytes = body.len(), "printed");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ReadParams {
    timeout_ms: Option<u64>,
}

async fn read(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<ReadParams>,
    body: Bytes,
) -> Result<Response, ApiError> {
    if body.is_empty() {
        return Err(ApiError::BadRequest("request body (the query) is empty"));
    }
    let ms = params
        .timeout_ms
        .unwrap_or(DEFAULT_READ_TIMEOUT_MS)
        .clamp(100, MAX_READ_TIMEOUT_MS);
    let reply = st.printer(&id)?.read(&body, Duration::from_millis(ms)).await?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], reply).into_response())
}

async fn status(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<HostStatus>, ApiError> {
    let raw = st
        .printer(&id)?
        .read(b"~HS", Duration::from_millis(DEFAULT_READ_TIMEOUT_MS))
        .await?;
    Ok(Json(parse_host_status(&raw)?))
}

/// Re-read the config file. Called by the settings window after it saves.
async fn reload(State(st): State<Arc<AppState>>) -> Response {
    match st.reload() {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "reload failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": format!("{e:#}") })))
                .into_response()
        }
    }
}

/// Websites that tried to print but are not allowed (for the settings window).
async fn blocked(State(st): State<Arc<AppState>>) -> Json<Vec<String>> {
    Json(st.blocked.lock().unwrap().clone())
}

enum ApiError {
    Printer(Error),
    BadRequest(&'static str),
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError::Printer(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, message) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.to_owned()),
            ApiError::Printer(e) => {
                let code = match e {
                    Error::NotFound(_) => StatusCode::NOT_FOUND,
                    Error::Timeout => StatusCode::GATEWAY_TIMEOUT,
                    Error::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
                    Error::Io(_) | Error::BadResponse(_) | Error::Failed(_) => StatusCode::BAD_GATEWAY,
                };
                tracing::warn!(error = %e, "request failed");
                (code, e.to_string())
            }
        };
        (code, Json(json!({ "error": message }))).into_response()
    }
}
