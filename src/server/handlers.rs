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
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
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

/// Request payload accepted by `POST /exec`.
#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    #[serde(default, alias = "command")]
    pub argv: Vec<String>,
    pub cwd: String,
    #[serde(default, alias = "image")]
    pub image: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub detach: bool,
}

/// Response payload returned by `POST /exec`.
#[derive(Debug, Serialize)]
pub struct ExecResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// A command request waiting for developer approval in the TUI.
pub struct PendingItem {
    pub id: String,
    pub activity_id: String,
    pub cancel_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub workspace_name: String,
    pub container_id: Option<String>,
    pub argv: Vec<String>,
    pub image: Option<String>,
    pub timeout_secs: u64,
    pub cwd: PathBuf,
    pub rule_cwd: PathBuf,
    pub matched_command: Option<String>,
    pub response_tx: Option<oneshot::Sender<ApprovalDecision>>,
}

pub enum ApprovalDecision {
    Approve { remember: bool },
    Deny,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecJobState {
    Running,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecJobPhase {
    PendingApproval,
    PullingImage,
    StartingCommand,
    RunningCommand,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ExecJobProgress {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecJobStatus {
    pub state: ExecJobState,
    pub job_id: String,
    #[serde(skip_serializing)]
    pub workspace_name: String,
    #[serde(skip_serializing)]
    pub session_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub timeout_secs: u64,
    pub argv: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<ExecJobPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<ExecJobProgress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing)]
    pub cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    #[serde(skip_serializing)]
    pub stdin_tx: Option<mpsc::Sender<Vec<u8>>>,
    #[serde(skip_serializing)]
    pub created_at: Instant,
}

#[derive(Clone, Default)]
pub struct ExecJobRegistry {
    inner: Arc<Mutex<HashMap<String, ExecJobStatus>>>,
}

/// Ceiling on concurrently running (non-terminal) exec jobs across the whole
/// manager. A container flooding `/exec --detach` cannot spawn unbounded host
/// processes; once this many jobs are running, new job creation is refused (H3).
const MAX_ACTIVE_EXEC_JOBS: usize = 64;
/// Ceiling on total tracked jobs (running + finished). When exceeded, finished
/// jobs are evicted to bound the registry's memory footprint (H3).
const MAX_TOTAL_EXEC_JOBS: usize = 512;
const MAX_TOTAL_EXEC_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const FINISHED_EXEC_JOB_TTL: Duration = Duration::from_secs(24 * 60 * 60);

impl ExecJobRegistry {
    /// Insert a new job, enforcing the active-job ceiling and pruning finished
    /// jobs to bound memory. Returns `None` when the active-job ceiling is
    /// reached, which callers surface as a 503 (H3).
    pub fn insert(&self, mut status: ExecJobStatus) -> Option<ExecJobStatus> {
        if status.job_id.is_empty() {
            status.job_id = uuid::Uuid::new_v4().to_string();
        }
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired_finished_jobs(&mut map);

        let active = map
            .values()
            .filter(|job| job.state == ExecJobState::Running)
            .count();
        if active >= MAX_ACTIVE_EXEC_JOBS {
            return None;
        }

        // Evict finished jobs if we are at the total ceiling. Only terminal jobs
        // are dropped, so a running job is never lost; the worst case is that a
        // client can no longer fetch the output of an old, completed job.
        if map.len() >= MAX_TOTAL_EXEC_JOBS {
            let mut finished = map
                .iter()
                .filter(|(_, job)| job.state != ExecJobState::Running)
                .map(|(id, job)| (job.created_at, id.clone()))
                .collect::<Vec<_>>();
            finished.sort_by_key(|(created_at, _)| *created_at);
            for (_, id) in finished
                .into_iter()
                .take(map.len().saturating_sub(MAX_TOTAL_EXEC_JOBS) + 1)
            {
                map.remove(&id);
            }
        }

        map.insert(status.job_id.clone(), status.clone());
        Some(status)
    }

    pub fn update(&self, job_id: &str, f: impl FnOnce(&mut ExecJobStatus)) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(status) = map.get_mut(job_id) {
            f(status);
        }
        prune_expired_finished_jobs(&mut map);
        prune_finished_output_to_budget(&mut map);
    }

    pub fn has_active_capacity(&self) -> bool {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired_finished_jobs(&mut map);
        map.values()
            .filter(|job| job.state == ExecJobState::Running)
            .count()
            < MAX_ACTIVE_EXEC_JOBS
    }

    pub fn get_for_session(&self, job_id: &str, session_token: &str) -> Option<ExecJobStatus> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired_finished_jobs(&mut map);
        map.get(job_id)
            .filter(|job| job.session_token == session_token)
            .cloned()
    }

    pub fn list_for_session(&self, session_token: &str) -> Vec<ExecJobStatus> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        prune_expired_finished_jobs(&mut map);
        let mut jobs = map
            .values()
            .filter(|job| job.session_token == session_token)
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| job.created_at);
        jobs
    }
}

fn prune_finished_output_to_budget(map: &mut HashMap<String, ExecJobStatus>) {
    let output_bytes = |job: &ExecJobStatus| {
        job.stdout.as_ref().map_or(0, String::len) + job.stderr.as_ref().map_or(0, String::len)
    };
    let mut total = map.values().map(output_bytes).sum::<usize>();
    if total <= MAX_TOTAL_EXEC_OUTPUT_BYTES {
        return;
    }
    let mut finished = map
        .values()
        .filter(|job| job.state != ExecJobState::Running)
        .map(|job| (job.created_at, job.job_id.clone(), output_bytes(job)))
        .collect::<Vec<_>>();
    finished.sort_by_key(|(created_at, _, _)| *created_at);
    for (_, job_id, bytes) in finished {
        map.remove(&job_id);
        total = total.saturating_sub(bytes);
        if total <= MAX_TOTAL_EXEC_OUTPUT_BYTES {
            break;
        }
    }
}

fn prune_expired_finished_jobs(map: &mut HashMap<String, ExecJobStatus>) {
    let now = Instant::now();
    map.retain(|_, job| {
        job.state == ExecJobState::Running
            || now.saturating_duration_since(job.created_at) <= FINISHED_EXEC_JOB_TTL
    });
}

impl ExecJobStatus {
    pub fn without_output(mut self) -> Self {
        self.stdout = None;
        self.stderr = None;
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecJobListResponse {
    pub jobs: Vec<ExecJobStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecJobOutputResponse {
    pub job_id: String,
    pub state: ExecJobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<ExecJobPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ExecJobOutputQuery {
    #[serde(default)]
    pub stream: Option<String>,
    #[serde(default)]
    pub tail: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecJobKillResponse {
    pub ok: bool,
    pub job_id: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct ExecJobSendRequest {
    pub input: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecJobSendResponse {
    pub ok: bool,
    pub job_id: String,
    pub message: String,
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
    pub workspace_name: String,
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
    pub pending_tx: mpsc::Sender<PendingItem>,
    pub stop_tx: mpsc::Sender<ContainerStopItem>,
    pub launch_tx: mpsc::Sender<WorkspaceLaunchItem>,
    pub audit_tx: mpsc::Sender<AuditEntry>,
    pub token: String,
    pub sessions: SessionRegistry,
    pub exec_jobs: ExecJobRegistry,
    // Bounded; see H12 comments at the construction site in manager::run.
    pub activity_tx: mpsc::Sender<ActivityEvent>,
}

/// A container stop request waiting to be handled by the TUI.
pub struct ContainerStopItem {
    pub workspace_name: String,
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

/// Max concurrent `/exec` handler invocations. Separate from the control
/// semaphore so a burst of long-running host commands cannot starve container
/// lifecycle endpoints (stop/launch), and vice versa. A synchronous exec holds
/// a permit for the command's duration; a detached exec holds it only briefly
/// while it spawns, with the running-job ceiling (`MAX_ACTIVE_EXEC_JOBS`)
/// bounding detached concurrency (H3).
const HOSTDO_EXEC_CONCURRENCY_LIMIT: usize = 32;

pub(crate) fn hostdo_exec_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(HOSTDO_EXEC_CONCURRENCY_LIMIT)))
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
        .route("/exec", post(crate::server::core::exec_handler))
        .route(
            "/exec/jobs",
            get(crate::server::core::exec_jobs_list_handler),
        )
        .route("/exec/jobs/:id", get(crate::server::core::exec_job_handler))
        .route(
            "/exec/jobs/:id/output",
            get(crate::server::core::exec_job_output_handler),
        )
        .route(
            "/exec/jobs/:id/kill",
            post(crate::server::core::exec_job_kill_handler),
        )
        .route(
            "/exec/jobs/:id/input",
            post(crate::server::core::exec_job_send_handler),
        )
        .route("/healthz", get(healthz_handler))
        .layer(DefaultBodyLimit::max(CONTROL_BODY_LIMIT_BYTES))
        .with_state(Arc::new(server_state));

    axum::serve(listener, router).await?;
    Ok(())
}

/// Liveness probe used by `hht workspace` to fail fast with a clear message
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
        workspace_name: identity.workspace_name.clone(),
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
                stop_audit_entry(&identity.workspace_name, DecisionKind::Approved, "stopped"),
            )
            .await;
            Json(StopResponse { ok: true }).into_response()
        }
        Ok(Ok(ContainerStopDecision::NotFound)) => {
            record_audit(
                &state,
                stop_audit_entry(&identity.workspace_name, DecisionKind::Denied, "not_found"),
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
                stop_audit_entry(&identity.workspace_name, DecisionKind::TimedOut, "timeout"),
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
        workspace_name: project.to_string(),
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
    if state.audit_tx.send(entry.clone()).await.is_err() {
        warn!("audit event channel is closed; continuing with durable log write");
    }
    let state_clone = state.state.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(error) = state_clone.log_audit(&entry) {
            warn!(error = %error, "failed to write audit event to disk");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn test_server_state(registry: SessionRegistry, exec_jobs: ExecJobRegistry) -> ServerState {
        let (pending_tx, _pending_rx) = mpsc::channel(1);
        let (stop_tx, _stop_rx) = mpsc::channel(1);
        let (launch_tx, _launch_rx) = mpsc::channel(1);
        let (audit_tx, _audit_rx) = mpsc::channel(1);
        let (activity_tx, _activity_rx) = mpsc::channel(16);
        let state_dir =
            std::env::temp_dir().join(format!("harness-hat-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        ServerState {
            config: SharedConfig::new(Arc::new(crate::config::Config::default())),
            state: StateManager::open(&state_dir).expect("state"),
            pending_tx,
            stop_tx,
            launch_tx,
            audit_tx,
            token: "token".to_string(),
            sessions: registry,
            exec_jobs,
            activity_tx,
        }
    }

    #[test]
    fn session_registry_round_trips_identity() {
        let registry = SessionRegistry::default();
        registry.insert(
            "session".to_string(),
            SessionIdentity {
                workspace_name: "workspace".to_string(),
                container_id: "container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );
        registry.insert(
            "other-session".to_string(),
            SessionIdentity {
                workspace_name: "workspace".to_string(),
                container_id: "other-container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );

        let identity = registry.get("session").expect("session identity");
        assert_eq!(identity.workspace_name, "workspace");
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
                workspace_name: "workspace".to_string(),
                container_id: "container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );
        let state = test_server_state(registry, ExecJobRegistry::default());

        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer token"));
        headers.insert(
            "x-harness-hat-session-token",
            HeaderValue::from_static("session"),
        );
        let (session_token, identity) =
            require_session_context(&state, &headers).expect("session context");
        assert_eq!(session_token, "session");
        assert_eq!(identity.workspace_name, "workspace");
    }

    #[tokio::test]
    async fn exec_job_uuid_routes_resolve_under_axum_0_7() {
        let registry = SessionRegistry::default();
        registry.insert(
            "session".to_string(),
            SessionIdentity {
                workspace_name: "workspace".to_string(),
                container_id: "container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );
        registry.insert(
            "other-session".to_string(),
            SessionIdentity {
                workspace_name: "workspace".to_string(),
                container_id: "other-container".to_string(),
                mount_target: "/workspace".to_string(),
            },
        );
        let exec_jobs = ExecJobRegistry::default();
        let job_id = "151fb311-2a78-458d-b036-eb9a59e7f0ad";
        exec_jobs.insert(ExecJobStatus {
            state: ExecJobState::Complete,
            job_id: job_id.to_string(),
            workspace_name: "workspace".to_string(),
            session_token: "session".to_string(),
            container: Some("container".to_string()),
            timeout_secs: 60,
            argv: vec!["curl".to_string(), "example.com".to_string()],
            cwd: Some("/workspace".to_string()),
            phase: None,
            image: None,
            message: "Command finished with exit code 0.".to_string(),
            progress: None,
            poll_after_ms: None,
            exit_code: Some(0),
            stdout: Some("ok\n".to_string()),
            stderr: Some(String::new()),
            reason: None,
            cancel_flag: None,
            stdin_tx: None,
            created_at: Instant::now(),
        });
        let state = test_server_state(registry, exec_jobs);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            run_with_listener(state, listener)
                .await
                .expect("control server")
        });
        let client = reqwest::Client::new();

        let status_response = client
            .get(format!("http://{addr}/exec/jobs/{job_id}"))
            .bearer_auth("token")
            .header("x-harness-hat-session-token", "session")
            .send()
            .await
            .expect("job status request");
        assert_eq!(status_response.status(), StatusCode::OK);
        let status_body: serde_json::Value = status_response.json().await.expect("job status json");
        assert_eq!(status_body["job_id"], job_id);
        assert!(status_body.get("stdout").is_none());

        let cross_session_response = client
            .get(format!("http://{addr}/exec/jobs/{job_id}"))
            .bearer_auth("token")
            .header("x-harness-hat-session-token", "other-session")
            .send()
            .await
            .expect("cross-session job request");
        assert_eq!(cross_session_response.status(), StatusCode::NOT_FOUND);

        let output_response = client
            .get(format!("http://{addr}/exec/jobs/{job_id}/output"))
            .bearer_auth("token")
            .header("x-harness-hat-session-token", "session")
            .send()
            .await
            .expect("job output request");
        assert_eq!(output_response.status(), StatusCode::OK);
        let output_body: serde_json::Value = output_response.json().await.expect("job output json");
        assert_eq!(output_body["job_id"], job_id);
        assert_eq!(output_body["stdout"], "ok\n");

        server.abort();
    }
}
