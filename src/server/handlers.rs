use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::instrument;

use crate::activity::ActivityEvent;
use crate::shared_config::SharedConfig;
use crate::state::{AuditEntry, StateManager};

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
        if let Ok(mut map) = self.inner.lock() {
            map.insert(session_token, identity);
        }
    }

    pub fn remove(&self, session_token: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(session_token);
        }
    }

    pub fn get(&self, session_token: &str) -> Option<SessionIdentity> {
        self.inner
            .lock()
            .ok()
            .and_then(|map| map.get(session_token).cloned())
    }
}

/// Shared server state for container lifecycle requests.
#[derive(Clone)]
pub struct ServerState {
    pub config: SharedConfig,
    pub state: StateManager,
    pub stop_tx: mpsc::Sender<ContainerStopItem>,
    pub audit_tx: mpsc::Sender<AuditEntry>,
    pub token: String,
    pub sessions: SessionRegistry,
    pub activity_tx: mpsc::UnboundedSender<ActivityEvent>,
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

/// Runs the manager control server.
///
/// The server intentionally exposes only container lifecycle control.
#[instrument(skip(server_state, listener))]
pub async fn run_with_listener(
    server_state: ServerState,
    listener: tokio::net::TcpListener,
) -> Result<()> {
    let router = Router::new()
        .route("/container/stop", post(stop_handler))
        .with_state(Arc::new(server_state));

    axum::serve(listener, router).await?;
    Ok(())
}

pub(super) async fn stop_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Json(_req): Json<StopRequest>,
) -> Response {
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

    match tokio::time::timeout(Duration::from_secs(10), response_rx).await {
        Ok(Ok(ContainerStopDecision::Stopped)) => Json(StopResponse { ok: true }).into_response(),
        Ok(Ok(ContainerStopDecision::NotFound)) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "not_found".into(),
                reason: "no running container matched the request".into(),
            }),
        )
            .into_response(),
        Ok(Err(_)) | Err(_) => (
            StatusCode::REQUEST_TIMEOUT,
            Json(ErrorResponse {
                error: "timeout".into(),
                reason: "timed out waiting for the container stop request".into(),
            }),
        )
            .into_response(),
    }
}

#[allow(clippy::result_large_err)]
pub(super) fn require_session_identity(
    state: &ServerState,
    headers: &HeaderMap,
) -> Result<SessionIdentity, Response> {
    require_session_context(state, headers).map(|(_, identity)| identity)
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
    if auth != expected {
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

#[allow(dead_code)]
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
        let (audit_tx, _audit_rx) = mpsc::channel(1);
        let (activity_tx, _activity_rx) = mpsc::unbounded_channel();
        let state_dir =
            std::env::temp_dir().join(format!("harness-hat-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        let state = ServerState {
            config: SharedConfig::new(Arc::new(crate::config::Config::default())),
            state: StateManager::open(&state_dir).expect("state"),
            stop_tx,
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
