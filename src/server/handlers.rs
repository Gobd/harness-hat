use anyhow::Result;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures::stream::unfold;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tracing::{instrument, warn};

use crate::activity::ActivityEvent;
use crate::shared_config::SharedConfig;
use crate::state::{AuditEntry, DecisionKind, StateManager};

/// Maximum body size accepted by control endpoints (defense-in-depth against
/// memory-amplification DoS through `Content-Length` lies on a POST).
const CONTROL_BODY_LIMIT_BYTES: usize = 8 * 1024;

/// Per-handler timeout used in lieu of a `tower_http::TimeoutLayer` (see below).
const CONTROL_HANDLER_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum number of in-flight control requests, enforced via a per-process
/// semaphore in lieu of a `tower::limit::ConcurrencyLimitLayer` (see below).
const CONTROL_CONCURRENCY_LIMIT: usize = 64;

/// Error payload returned by manager control endpoints.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub reason: String,
}

/// Request payload accepted by the container stop endpoint.
#[derive(Debug, Deserialize)]
pub struct StopRequest {}

/// Response payload returned by the container stop endpoint.
#[derive(Debug, Serialize)]
pub struct StopResponse {
    pub ok: bool,
}

/// Represents the identity of a running container session.
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    pub project: String,
    pub container_id: String,
    pub mount_target: String,
}

/// A registry for active container sessions, mapping session tokens to their identities.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<Mutex<HashMap<String, SessionIdentity>>>,
}

impl SessionRegistry {
    pub fn insert(&self, session_token: String, identity: SessionIdentity) {
        // Recover from a poisoned mutex rather than silently no-op'ing.
        // Silent swallowing would leak the session through `remove`/`get`
        // for the lifetime of the process if any holder ever panicked.
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(session_token, identity);
    }

    pub fn remove(&self, session_token: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(session_token);
    }

    pub fn get(&self, session_token: &str) -> Option<SessionIdentity> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.get(session_token).cloned()
    }
}

/// Shared server state for container lifecycle requests.
#[derive(Clone)]
pub struct ServerState {
    pub config: SharedConfig,
    pub state: StateManager,
    pub stop_tx: mpsc::Sender<ContainerStopItem>,
    pub launch_tx: mpsc::Sender<WorkspaceLaunchItem>,
    pub audit_tx: mpsc::Sender<AuditEntry>,
    pub token: String,
    pub sessions: SessionRegistry,
    // Bounded; see H12 comments at the construction site in manager::run.
    pub activity_tx: mpsc::Sender<ActivityEvent>,
}

/// A container stop request waiting to be handled by the TUI.
pub struct ContainerStopItem {
    pub project: String,
    pub container_id: String,
    pub response_tx: Option<oneshot::Sender<ContainerStopDecision>>,
}

/// The decision returned by the TUI for a stop request.
pub enum ContainerStopDecision {
    Stopped,
    NotFound,
}

/// Body accepted by `POST /workspace/launch`.
#[derive(Debug, Deserialize)]
pub struct WorkspaceLaunchRequest {
    pub workspace_name: String,
    pub template: String,
}

/// Final-success payload included in a `LaunchEvent::Launched`.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceLaunchResponse {
    pub session_token: String,
    pub alias: String,
    pub docker_name: String,
    pub workspace_name: String,
    pub template: String,
}

/// Events streamed back over `POST /workspace/launch`, one per NDJSON line.
/// The TUI emits these as it progresses through the launch (and any
/// intervening `docker build`); the CLI mirrors `status` / `build_output`
/// to stderr and treats `launched` / `error` as terminal.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LaunchEvent {
    /// Coarse-grained milestone ("checking image", "building", "launching").
    Status { message: String },
    /// One line of `docker build` output (stdout or stderr).
    BuildOutput { line: String, is_error: bool },
    /// Launch succeeded — terminal, the stream closes after this.
    Launched(WorkspaceLaunchResponse),
    /// Launch (or its prerequisite build) failed — terminal.
    Error { reason: String },
}

/// A workspace-launch request waiting to be handled by the TUI. The TUI
/// reloads config from disk, swaps it into `SharedConfig`, looks up the named
/// workspace/template, builds the image if needed, and runs the same launch
/// path the in-TUI picker uses. Progress is streamed back through `event_tx`.
pub struct WorkspaceLaunchItem {
    pub workspace_name: String,
    pub template: String,
    pub event_tx: mpsc::Sender<LaunchEvent>,
}

/// Process-wide semaphore enforcing `CONTROL_CONCURRENCY_LIMIT`. Kept in a
/// `OnceLock` so that `ServerState`'s public struct layout doesn't change —
/// `manager.rs` still constructs it via struct-literal syntax.
///
/// TODO(handlers): once `tower::limit::ConcurrencyLimitLayer` and
/// `tower_http::timeout::TimeoutLayer` are added as direct deps in Cargo.toml,
/// replace this manual semaphore + per-handler `tokio::time::timeout` with the
/// idiomatic tower layers wrapping the router.
fn control_concurrency_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(CONTROL_CONCURRENCY_LIMIT)))
        .clone()
}

/// Runs the manager control server.
///
/// The server intentionally exposes only container lifecycle control.
#[instrument(skip(server_state, listener))]
pub async fn run_with_listener(
    server_state: ServerState,
    listener: tokio::net::TcpListener,
) -> Result<()> {
    // Defense-in-depth: cap request body size so a `Content-Length: 1GiB` POST
    // can't allocate before `Json<StopRequest>` rejects it. axum's
    // `DefaultBodyLimit` is built in; tower/tower-http are *not* direct deps,
    // so the timeout + concurrency limit are enforced inside `stop_handler`
    // via `tokio::time::timeout` and `control_concurrency_semaphore()`.
    let router = Router::new()
        .route("/container/stop", post(stop_handler))
        .route("/workspace/launch", post(workspace_launch_handler))
        .route("/healthz", get(healthz_handler))
        .layer(DefaultBodyLimit::max(CONTROL_BODY_LIMIT_BYTES))
        .with_state(Arc::new(server_state));

    axum::serve(listener, router).await?;
    Ok(())
}

/// Liveness probe used by `hh workspace` to fail fast with a clear message
/// when the manager isn't running. Intentionally no auth — the response
/// contains nothing sensitive, and requiring the token would force every CLI
/// caller to load and parse it just to print "manager not running."
async fn healthz_handler() -> Response {
    (StatusCode::OK, "ok").into_response()
}

pub(super) async fn stop_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Json(_req): Json<StopRequest>,
) -> Response {
    // Per-process in-flight cap (stand-in for ConcurrencyLimitLayer). We
    // `try_acquire` so a slow-loris burst gets a fast 503 rather than tying up
    // a runtime task waiting for a permit indefinitely.
    let semaphore = control_concurrency_semaphore();
    let _permit = match semaphore.try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            warn!("stop_handler rejecting request: concurrency limit reached");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "busy".into(),
                    reason: "control server is at its concurrency limit".into(),
                }),
            )
                .into_response();
        }
    };

    let identity = match require_session_identity(&state, &headers) {
        Ok(id) => id,
        Err(resp) => return resp,
    };

    let (response_tx, response_rx) = oneshot::channel::<ContainerStopDecision>();
    let item = ContainerStopItem {
        project: identity.project.clone(),
        container_id: identity.container_id.clone(),
        response_tx: Some(response_tx),
    };
    if state.stop_tx.send(item).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "manager_shutting_down".into(),
                reason: "manager is shutting down".into(),
            }),
        )
            .into_response();
    }

    match tokio::time::timeout(CONTROL_HANDLER_TIMEOUT, response_rx).await {
        Ok(Ok(ContainerStopDecision::Stopped)) => {
            record_audit(
                &state,
                stop_audit_entry(&identity.project, DecisionKind::Approved, "stopped"),
            )
            .await;
            Json(StopResponse { ok: true }).into_response()
        }
        Ok(Ok(ContainerStopDecision::NotFound)) => {
            record_audit(
                &state,
                stop_audit_entry(&identity.project, DecisionKind::Denied, "not_found"),
            )
            .await;
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "not_found".into(),
                    reason: "no running container matched the request".into(),
                }),
            )
                .into_response()
        }
        Ok(Err(_)) | Err(_) => {
            record_audit(
                &state,
                stop_audit_entry(&identity.project, DecisionKind::TimedOut, "timeout"),
            )
            .await;
            (
                StatusCode::REQUEST_TIMEOUT,
                Json(ErrorResponse {
                    error: "timeout".into(),
                    reason: "timed out waiting for the container stop request".into(),
                }),
            )
                .into_response()
        }
    }
}

/// Buffer for the NDJSON event stream. 64 entries leaves comfortable
/// headroom for fast `docker build` output bursts (one line per send) without
/// risking unbounded growth if the CLI is slow to read.
const LAUNCH_EVENT_CHANNEL_CAPACITY: usize = 64;

pub(super) async fn workspace_launch_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Json(req): Json<WorkspaceLaunchRequest>,
) -> Response {
    let semaphore = control_concurrency_semaphore();
    // Use a permit that lives for the duration of the streaming body, not just
    // the handler future, so a slow client holding the stream open still
    // counts against the concurrency limit. `OwnedSemaphorePermit` is moved
    // into the stream closure below.
    let permit = match semaphore.try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            warn!("workspace_launch_handler rejecting request: concurrency limit reached");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "busy".into(),
                    reason: "control server is at its concurrency limit".into(),
                }),
            )
                .into_response();
        }
    };

    if let Err(resp) = require_bearer(&state, &headers) {
        return resp;
    }

    let workspace_name = req.workspace_name.trim().to_string();
    let template = req.template.trim().to_string();
    if workspace_name.is_empty() || template.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "bad_request".into(),
                reason: "workspace_name and template must be non-empty".into(),
            }),
        )
            .into_response();
    }

    let (event_tx, event_rx) = mpsc::channel::<LaunchEvent>(LAUNCH_EVENT_CHANNEL_CAPACITY);
    let item = WorkspaceLaunchItem {
        workspace_name,
        template,
        event_tx,
    };
    if state.launch_tx.send(item).await.is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "manager_shutting_down".into(),
                reason: "manager is shutting down".into(),
            }),
        )
            .into_response();
    }

    // Stream events as NDJSON. `event_tx` lives in the TUI's `WorkspaceLaunchItem`
    // until the launch flow completes (success, build failure, or launch failure);
    // dropping it terminates the receiver and closes the body.
    let stream = unfold((event_rx, permit), |(mut rx, permit)| async move {
        let event = rx.recv().await?;
        let mut bytes = match serde_json::to_vec(&event) {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "serializing launch event failed; closing stream");
                return None;
            }
        };
        bytes.push(b'\n');
        Some((
            Ok::<Bytes, std::convert::Infallible>(Bytes::from(bytes)),
            (rx, permit),
        ))
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/x-ndjson")
        // Tell intermediaries (and reqwest) not to buffer; we want each line
        // visible to the CLI as the build emits it.
        .header("cache-control", "no-cache")
        .body(Body::from_stream(stream))
        .expect("static response builder cannot fail")
}

/// Build an `AuditEntry` describing a `/container/stop` outcome. Centralized
/// here so each match arm above stays a one-liner.
fn stop_audit_entry(project: &str, decision: DecisionKind, reason: &str) -> AuditEntry {
    AuditEntry {
        project: project.to_string(),
        argv: vec!["container/stop".to_string(), reason.to_string()],
        cwd: String::new(),
        decision,
        exit_code: None,
        duration_ms: None,
        timestamp: chrono::Utc::now(),
    }
}

#[allow(clippy::result_large_err)]
pub(super) fn require_session_identity(
    state: &ServerState,
    headers: &HeaderMap,
) -> Result<SessionIdentity, Response> {
    require_session_context(state, headers).map(|(_, identity)| identity)
}

/// Constant-time `Authorization: Bearer <token>` check with no session
/// requirement. Used by endpoints (like `/workspace/launch`) called from the
/// host CLI, before any session exists.
#[allow(clippy::result_large_err)]
pub(super) fn require_bearer(state: &ServerState, headers: &HeaderMap) -> Result<(), Response> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = format!("Bearer {}", state.token);
    let auth_bytes = auth.as_bytes();
    let expected_bytes = expected.as_bytes();
    let auth_ok =
        auth_bytes.len() == expected_bytes.len() && bool::from(auth_bytes.ct_eq(expected_bytes));
    if !auth_ok {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".into(),
                reason: "invalid or missing token".into(),
            }),
        )
            .into_response());
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
pub(super) fn require_session_context(
    state: &ServerState,
    headers: &HeaderMap,
) -> Result<(String, SessionIdentity), Response> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = format!("Bearer {}", state.token);
    // Constant-time comparison so the 64-hex-char token can't be recovered
    // byte-by-byte over a timing channel. We bail on length mismatch first —
    // the token is fixed-length, so that branch doesn't leak useful info, and
    // `ct_eq` requires equal-length inputs to make sense.
    let auth_bytes = auth.as_bytes();
    let expected_bytes = expected.as_bytes();
    let auth_ok =
        auth_bytes.len() == expected_bytes.len() && bool::from(auth_bytes.ct_eq(expected_bytes));
    if !auth_ok {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".into(),
                reason: "invalid or missing token".into(),
            }),
        )
            .into_response());
    }

    let session_token = headers
        .get("x-harness-hat-session-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim();
    if session_token.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".into(),
                reason: "missing session token".into(),
            }),
        )
            .into_response());
    }

    let identity = state.sessions.get(session_token).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "unauthorized".into(),
                reason: "unknown session token".into(),
            }),
        )
            .into_response()
    })?;
    Ok((session_token.to_string(), identity))
}

pub(super) async fn record_audit(state: &ServerState, entry: AuditEntry) {
    let _ = state.audit_tx.send(entry.clone()).await;
    let state_clone = state.state.clone();
    tokio::task::spawn_blocking(move || {
        let _ = state_clone.log_audit(&entry);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn session_registry_round_trips_identity() {
        let registry = SessionRegistry::default();
        registry.insert(
            "session".to_string(),
            SessionIdentity {
                project: "workspace".to_string(),
                container_id: "container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );

        let identity = registry.get("session").expect("session identity");
        assert_eq!(identity.project, "workspace");
        assert_eq!(identity.container_id, "container");

        registry.remove("session");
        assert!(registry.get("session").is_none());
    }

    #[test]
    fn require_session_context_accepts_registered_session() {
        let registry = SessionRegistry::default();
        registry.insert(
            "session".to_string(),
            SessionIdentity {
                project: "workspace".to_string(),
                container_id: "container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );
        let (stop_tx, _stop_rx) = mpsc::channel(1);
        let (launch_tx, _launch_rx) = mpsc::channel(1);
        let (audit_tx, _audit_rx) = mpsc::channel(1);
        let (activity_tx, _activity_rx) = mpsc::channel(16);
        let state_dir =
            std::env::temp_dir().join(format!("harness-hat-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        let state = ServerState {
            config: SharedConfig::new(Arc::new(crate::config::Config::default())),
            state: StateManager::open(&state_dir).expect("state"),
            stop_tx,
            launch_tx,
            audit_tx,
            token: "token".to_string(),
            sessions: registry,
            activity_tx,
        };

        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer token"));
        headers.insert(
            "x-harness-hat-session-token",
            HeaderValue::from_static("session"),
        );
        let (session_token, identity) =
            require_session_context(&state, &headers).expect("session context");
        assert_eq!(session_token, "session");
        assert_eq!(identity.project, "workspace");
    }
}
