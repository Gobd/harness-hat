/// MITM HTTP/HTTPS proxy enforcing network policies from harness-rules.toml.
///
/// Containers route all traffic through this proxy. Plain HTTP requests are
/// intercepted and parsed directly. HTTPS traffic is intercepted via CONNECT
/// tunnels: the proxy terminates TLS with a per-domain leaf cert signed by
/// the harness-hat CA (which containers are configured to trust), inspects the
/// inner HTTP request, then forwards to the real server.
///
/// Network policy (auto/prompt/deny) is determined by matching the composed
/// rules against method + host + path of each request.
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

use crate::activity::{Activity, ActivityEvent, ActivityKind, ActivityState, payload_preview};
use crate::ca::CaStore;
use crate::config;
use crate::proxy::connect::{handle_connect, parse_sni_from_tls_client_hello};
use crate::proxy::helpers::{
    connect_public_tcp_with_priority, container_tls_passthrough_match,
    ensure_host_header_matches_target, is_expected_disconnect, resolve_public_addrs_with_priority,
    tunnel_with_activity, write_error_any,
};
use crate::proxy::http::{
    finish_blocked_network_activity, forward_request_with_activity, handle_plain_http,
    network_policy_allows, parse_request_line_and_headers, read_body_any, read_request_head_any,
};
use crate::rules::NetworkPolicy;
use crate::shared_config::SharedConfig;
use tracing::instrument;

const FIRST_BYTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const ROOT_PROXY_CONNECTION_LIMIT: usize = 256;
const SCOPED_PROXY_CONNECTION_LIMIT: usize = 128;
const SCOPED_PROXY_TOTAL_CONNECTION_LIMIT: usize = 192;
const SCOPED_PROXY_LIMITED_CONNECTION_LIMIT: usize = 64;
const ROOT_SOURCE_PROXY_CONNECTION_LIMIT: usize = 32;
const LIMITED_SOURCE_PROXY_CONNECTION_LIMIT: usize = 32;

/// A network request waiting on the TUI for an allow/deny decision.
pub struct PendingNetworkItem {
    pub activity_id: String,
    pub cancel_flag: Arc<std::sync::atomic::AtomicBool>,
    pub source_project: Option<String>,
    pub source_container: Option<String>,
    pub source_status: String,
    pub has_proxy_authorization: bool,
    pub method: String,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub response_tx: oneshot::Sender<NetworkDecision>,
    pub merged_response_txs: Vec<oneshot::Sender<NetworkDecision>>,
}

/// The result returned by the TUI for a pending network request.
#[derive(Debug, Clone, Copy)]
pub enum NetworkDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone)]
pub(crate) struct FixedSourceIdentity {
    pub(crate) project: String,
    pub(crate) container: String,
    pub(crate) auth_token: String,
    pub(crate) limiter_key: String,
    pub(crate) priority: SourcePriority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePriority {
    Primary,
    Limited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceIdentityStatus {
    Ok,
    ListenerBoundSource,
    MissingProxyAuthorization,
    MalformedAuthHeader,
    UnsupportedAuthScheme,
    InvalidBase64,
    InvalidUtf8,
    MissingUsernamePasswordDelimiter,
    UnexpectedUsername,
    MissingProjectContainerDelimiter,
    InvalidProjectEncoding,
    InvalidContainerEncoding,
}

impl SourceIdentityStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::ListenerBoundSource => "listener_bound_source",
            Self::MissingProxyAuthorization => "missing_proxy_authorization",
            Self::MalformedAuthHeader => "malformed_auth_header",
            Self::UnsupportedAuthScheme => "unsupported_auth_scheme",
            Self::InvalidBase64 => "invalid_base64",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::MissingUsernamePasswordDelimiter => "missing_username_password_delimiter",
            Self::UnexpectedUsername => "unexpected_username",
            Self::MissingProjectContainerDelimiter => "missing_project_container_delimiter",
            Self::InvalidProjectEncoding => "invalid_project_encoding",
            Self::InvalidContainerEncoding => "invalid_container_encoding",
        }
    }
}

// ── Proxy state ───────────────────────────────────────────────────────────────

#[derive(Clone)]
/// Shared proxy state used by all listener tasks.
pub struct ProxyState {
    pub ca: Arc<CaStore>,
    pub config: SharedConfig,
    pub pending_tx: mpsc::Sender<PendingNetworkItem>,
    pub activity_tx: mpsc::UnboundedSender<ActivityEvent>,
    pub(crate) fixed_source: Option<FixedSourceIdentity>,
    connection_limiter: Arc<Semaphore>,
    connection_limit: usize,
    scoped_connection_limiter: Arc<Semaphore>,
    scoped_limited_connection_limiter: Arc<Semaphore>,
    source_connection_limiters: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
}

pub(crate) struct ProxyConnectionPermit {
    _listener: Option<tokio::sync::OwnedSemaphorePermit>,
    _scoped_total: Option<tokio::sync::OwnedSemaphorePermit>,
    _scoped_limited: Option<tokio::sync::OwnedSemaphorePermit>,
}

pub(crate) struct SourceConnectionPermit {
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl ProxyState {
    pub fn new(
        ca: Arc<CaStore>,
        config: SharedConfig,
        pending_tx: mpsc::Sender<PendingNetworkItem>,
        activity_tx: mpsc::UnboundedSender<ActivityEvent>,
    ) -> Result<Self> {
        Ok(Self {
            ca,
            config,
            pending_tx,
            activity_tx,
            fixed_source: None,
            connection_limiter: Arc::new(Semaphore::new(ROOT_PROXY_CONNECTION_LIMIT)),
            connection_limit: ROOT_PROXY_CONNECTION_LIMIT,
            scoped_connection_limiter: Arc::new(Semaphore::new(
                SCOPED_PROXY_TOTAL_CONNECTION_LIMIT,
            )),
            scoped_limited_connection_limiter: Arc::new(Semaphore::new(
                SCOPED_PROXY_LIMITED_CONNECTION_LIMIT,
            )),
            source_connection_limiters: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub(crate) fn with_fixed_source(
        &self,
        project: &str,
        container: &str,
        auth_token: &str,
        priority: SourcePriority,
    ) -> Self {
        let mut cloned = self.clone();
        cloned.fixed_source = Some(FixedSourceIdentity {
            project: project.to_string(),
            container: container.to_string(),
            auth_token: auth_token.to_string(),
            limiter_key: auth_token.to_string(),
            priority,
        });
        cloned.connection_limiter = Arc::new(Semaphore::new(SCOPED_PROXY_CONNECTION_LIMIT));
        cloned.connection_limit = SCOPED_PROXY_CONNECTION_LIMIT;
        cloned
    }

    pub(crate) fn try_acquire_connection(&self) -> Option<ProxyConnectionPermit> {
        let listener = if self.is_primary_source() {
            None
        } else {
            Some(self.connection_limiter.clone().try_acquire_owned().ok()?)
        };
        let scoped_limited = if self.is_limited_source() {
            Some(
                self.scoped_limited_connection_limiter
                    .clone()
                    .try_acquire_owned()
                    .ok()?,
            )
        } else {
            None
        };
        let scoped_total = if self.is_limited_source() {
            Some(
                self.scoped_connection_limiter
                    .clone()
                    .try_acquire_owned()
                    .ok()?,
            )
        } else {
            None
        };
        Some(ProxyConnectionPermit {
            _listener: listener,
            _scoped_total: scoped_total,
            _scoped_limited: scoped_limited,
        })
    }

    pub(crate) fn try_acquire_source_connection(
        &self,
        source_project: Option<&str>,
        source_container: Option<&str>,
    ) -> Option<SourceConnectionPermit> {
        if self.is_primary_source() {
            return Some(SourceConnectionPermit { _permit: None });
        }

        let key = self
            .fixed_source
            .as_ref()
            .map(|fixed| source_connection_key(Some(&fixed.project), Some(&fixed.limiter_key)))
            .unwrap_or_else(|| source_connection_key(source_project, source_container));
        let limiter = {
            let mut limiters = self.source_connection_limiters.lock().ok()?;
            limiters
                .entry(key)
                .or_insert_with(|| Arc::new(Semaphore::new(self.source_connection_limit())))
                .clone()
        };
        Some(SourceConnectionPermit {
            _permit: Some(limiter.try_acquire_owned().ok()?),
        })
    }

    fn source_connection_limit(&self) -> usize {
        match self.fixed_source.as_ref().map(|fixed| fixed.priority) {
            Some(SourcePriority::Limited) => LIMITED_SOURCE_PROXY_CONNECTION_LIMIT,
            Some(SourcePriority::Primary) => usize::MAX,
            None => ROOT_SOURCE_PROXY_CONNECTION_LIMIT,
        }
    }

    fn is_primary_source(&self) -> bool {
        self.fixed_source
            .as_ref()
            .is_some_and(|fixed| fixed.priority == SourcePriority::Primary)
    }

    fn is_limited_source(&self) -> bool {
        self.fixed_source
            .as_ref()
            .is_some_and(|fixed| fixed.priority == SourcePriority::Limited)
    }

    pub(crate) async fn resolve_public_addrs(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<std::net::SocketAddr>> {
        resolve_public_addrs_with_priority(host, port, self.is_primary_source()).await
    }

    pub(crate) async fn connect_public_tcp(&self, host: &str, port: u16) -> Result<TcpStream> {
        connect_public_tcp_with_priority(host, port, self.is_primary_source()).await
    }
}

fn source_connection_key(source_project: Option<&str>, source_container: Option<&str>) -> String {
    format!(
        "{}\0{}",
        source_project.unwrap_or("<unknown-project>"),
        source_container.unwrap_or("<unknown-container>")
    )
}

impl ProxyState {
    pub(crate) fn start_network_activity(
        &self,
        source_project: Option<String>,
        source_container: Option<String>,
        method: &str,
        host: &str,
        path: &str,
        protocol: &str,
        headers: &[(String, String)],
        body: &[u8],
        state: ActivityState,
    ) -> Activity {
        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (payload_preview, payload_truncated) = payload_preview(body);
        let content_type = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone());
        let content_length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok());
        let mut activity = Activity::new(
            source_project.unwrap_or_else(|| "unknown-workspace".to_string()),
            source_container,
            ActivityKind::Network {
                method: method.to_string(),
                host: host.to_string(),
                path: path.to_string(),
                protocol: protocol.to_string(),
                payload_preview,
                payload_truncated,
                content_type,
                content_length,
            },
            state,
            cancel_flag,
        );
        activity.session_token = self
            .fixed_source
            .as_ref()
            .map(|source| source.auth_token.clone());
        let _ = self
            .activity_tx
            .send(ActivityEvent::Started(Box::new(activity.clone())));
        activity
    }

    pub(crate) fn activity_state(
        &self,
        id: &str,
        state: ActivityState,
        status: impl Into<Option<String>>,
    ) {
        let _ = self.activity_tx.send(ActivityEvent::State {
            id: id.to_string(),
            state,
            status: status.into(),
        });
    }

    pub(crate) fn activity_line(&self, id: &str, line: impl Into<String>) {
        let _ = self.activity_tx.send(ActivityEvent::Line {
            id: id.to_string(),
            line: line.into(),
        });
    }

    pub(crate) fn activity_finished(
        &self,
        id: &str,
        state: ActivityState,
        status: impl Into<Option<String>>,
    ) {
        let _ = self.activity_tx.send(ActivityEvent::Finished {
            id: id.to_string(),
            state,
            status: status.into(),
        });
    }
}

/// A scoped listener task that is aborted when dropped.
pub struct ScopedProxyListener {
    pub addr: String,
    proxy_auth_token: String,
    abort_handle: tokio::task::AbortHandle,
}

impl ScopedProxyListener {
    pub fn proxy_url(&self) -> String {
        format!("http://harness-hat:{}@{}", self.proxy_auth_token, self.addr)
    }

    pub fn proxy_auth_token(&self) -> &str {
        &self.proxy_auth_token
    }
}

impl Drop for ScopedProxyListener {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[instrument(skip(state))]
pub async fn run(state: ProxyState, addr: String) -> Result<()> {
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("proxy bind {addr}: {e}"))?;
    run_with_listener(state, listener).await
}

#[instrument(skip(state, listener))]
async fn run_with_listener(state: ProxyState, listener: TcpListener) -> Result<()> {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _peer) = accepted?;
                let Some(permit) = state.try_acquire_connection() else {
                    warn!(
                        limit = state.connection_limit,
                        "proxy connection limit reached; rejecting connection"
                    );
                    let _ = write_error_any(&mut stream, 503, "Proxy connection limit reached").await;
                    continue;
                };
                let state = state.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    if let Err(e) = handle_connection(stream, state).await {
                        if is_expected_disconnect(&e) {
                            debug!("proxy: {e}");
                        } else {
                            error!("proxy: {e}");
                        }
                    }
                });
            }
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(e)) = joined {
                    debug!("proxy connection task ended: {e}");
                }
            }
        }
    }
}

/// Start a per-container proxy listener bound to the supplied host/port.
#[instrument(skip(state))]
pub fn spawn_scoped_listener(
    state: &ProxyState,
    bind_host: &str,
    project: &str,
    container: &str,
    auth_token: &str,
    priority: SourcePriority,
) -> Result<ScopedProxyListener> {
    let bind_addr = format!("{bind_host}:0");
    let std_listener = std::net::TcpListener::bind(&bind_addr)
        .map_err(|e| anyhow::anyhow!("proxy bind {bind_addr}: {e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("proxy set_nonblocking {bind_addr}: {e}"))?;
    let local_addr = std_listener.local_addr()?;
    let listener = TcpListener::from_std(std_listener)?;
    let addr = format!("{}:{}", bind_host, local_addr.port());
    let fixed_state = state.with_fixed_source(project, container, auth_token, priority);
    let task = tokio::spawn(async move {
        if let Err(e) = run_with_listener(fixed_state, listener).await {
            error!("scoped proxy server error: {e}");
        }
    });
    Ok(ScopedProxyListener {
        addr,
        proxy_auth_token: auth_token.to_string(),
        abort_handle: task.abort_handle(),
    })
}

// ── Connection dispatch ───────────────────────────────────────────────────────

async fn handle_connection(stream: TcpStream, state: ProxyState) -> Result<()> {
    let mut peek = [0u8; 8];
    let n = tokio::time::timeout(FIRST_BYTE_TIMEOUT, stream.peek(&mut peek))
        .await
        .map_err(|_| anyhow::anyhow!("proxy connection timed out waiting for first byte"))??;

    // Prefer explicit CONNECT first, then fall back to sniffing for raw TLS.
    // This lets the same listener handle both proxy-aware clients and clients
    // that try to talk TLS directly to the gateway.
    if n >= 7 && &peek[..7] == b"CONNECT" {
        handle_connect(stream, state).await
    } else if looks_like_tls_client_hello(&peek[..n]) {
        handle_transparent_tls(stream, state).await
    } else {
        handle_plain_http(stream, state).await
    }
}

fn looks_like_tls_client_hello(buf: &[u8]) -> bool {
    buf.len() >= 3 && buf[0] == 0x16 && buf[1] == 0x03 && (0x01..=0x04).contains(&buf[2])
}

// ── Transparent TLS (no CONNECT) ─────────────────────────────────────────────

struct PrefixedTcpStream {
    prefix: std::io::Cursor<Vec<u8>>,
    inner: TcpStream,
}

impl AsyncRead for PrefixedTcpStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if (self.prefix.position() as usize) < self.prefix.get_ref().len() {
            let before = buf.filled().len();
            let pos = self.prefix.position();
            let rem = &self.prefix.get_ref()[pos as usize..];
            let to_copy = rem.len().min(buf.remaining());
            buf.put_slice(&rem[..to_copy]);
            self.prefix.set_position(pos + to_copy as u64);
            let after = buf.filled().len();
            debug_assert!(after > before);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for PrefixedTcpStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

async fn handle_transparent_tls(mut stream: TcpStream, state: ProxyState) -> Result<()> {
    if let Some(fixed) = &state.fixed_source {
        warn!(
            source_project = %fixed.project,
            source_container = %fixed.container,
            "dropping transparent TLS connection to scoped proxy without proxy authentication"
        );
        return Ok(());
    }

    let (source_project, source_container, source_status, has_proxy_authorization) = (
        None,
        None,
        SourceIdentityStatus::MissingProxyAuthorization,
        false,
    );
    let Some(_source_permit) =
        state.try_acquire_source_connection(source_project.as_deref(), source_container.as_deref())
    else {
        return Ok(());
    };

    let cfg = state.config.get();

    let prefix = read_tls_client_hello_prefix(&mut stream).await?;
    let Some(host) = parse_sni_from_tls_client_hello(&prefix) else {
        warn!("transparent TLS connection missing SNI; dropping");
        return Ok(());
    };

    let connect_activity = state.start_network_activity(
        source_project.clone(),
        source_container.clone(),
        "CONNECT",
        &host,
        "/",
        "transparent-tls",
        &[],
        &[],
        ActivityState::Forwarding,
    );
    state.activity_line(&connect_activity.id, format!("target {host}:443"));

    let rules = match config::load_composed_rules_for_workspace(&cfg, source_project.as_deref()) {
        Ok(rules) => rules,
        Err(e) => {
            warn!("proxy rules load error: {e}");
            state.activity_finished(
                &connect_activity.id,
                ActivityState::Failed,
                Some("invalid harness-rules.toml configuration".to_string()),
            );
            return Ok(());
        }
    };
    let preflight_policy = rules.match_connect(&host, 443);
    if preflight_policy != NetworkPolicy::Deny {
        if let Err(e) = state.resolve_public_addrs(&host, 443).await {
            state.activity_finished(
                &connect_activity.id,
                ActivityState::Denied,
                Some(e.to_string()),
            );
            return Ok(());
        }
    }
    let preflight_allowed = network_policy_allows(
        &state,
        &connect_activity,
        preflight_policy,
        "waiting for CONNECT approval",
        "CONNECT",
        &host,
        Some(443),
        "/",
        source_project.clone(),
        source_container.clone(),
        source_status.as_str(),
        has_proxy_authorization,
    )
    .await;
    if !preflight_allowed {
        finish_blocked_network_activity(&state, &connect_activity);
        return Ok(());
    }

    if let Some(bypass_pattern) =
        container_tls_passthrough_match(&cfg, source_container.as_deref(), &host)
    {
        info!(
            host = %host,
            bypass_pattern = %bypass_pattern,
            source_project = ?source_project,
            source_container = ?source_container,
            source_status = source_status.as_str(),
            "proxy transparent TLS passthrough"
        );
        let mut upstream = state.connect_public_tcp(&host, 443).await.map_err(|e| {
            anyhow::anyhow!("transparent passthrough connect to {host}:443 failed: {e}")
        })?;
        upstream.write_all(&prefix).await?;
        state.activity_state(
            &connect_activity.id,
            ActivityState::Forwarding,
            Some(format!("tunneling {host}:443")),
        );
        let _ = tunnel_with_activity(&state, &connect_activity, &mut stream, &mut upstream).await;
        return Ok(());
    }

    let prefixed = PrefixedTcpStream {
        prefix: std::io::Cursor::new(prefix),
        inner: stream,
    };

    let server_config = state.ca.leaf_server_config(&host)?;
    let acceptor = TlsAcceptor::from(server_config);
    let mut tls_stream = acceptor.accept(prefixed).await.map_err(|e| {
        state.activity_finished(
            &connect_activity.id,
            ActivityState::Failed,
            Some(format!("TLS accept for {host}: {e}")),
        );
        anyhow::anyhow!("TLS accept for {host}: {e}")
    })?;

    debug!("proxy TLS established for host={host} (transparent)");
    state.activity_finished(
        &connect_activity.id,
        ActivityState::Complete,
        Some("TLS tunnel established".to_string()),
    );

    let (inner_head, inner_remainder) = read_request_head_any(&mut tls_stream).await?;
    let inner_str = match std::str::from_utf8(&inner_head) {
        Ok(s) => s,
        Err(_) => {
            write_error_any(&mut tls_stream, 400, "Bad Request").await?;
            return Ok(());
        }
    };
    let (method, path, headers) = match parse_request_line_and_headers(inner_str) {
        Some(r) => r,
        None => {
            write_error_any(&mut tls_stream, 400, "Bad Request").await?;
            return Ok(());
        }
    };
    if ensure_host_header_matches_target(&headers, &host, 443).is_err() {
        write_error_any(&mut tls_stream, 400, "Bad Request").await?;
        return Ok(());
    }
    let body = read_body_any(&mut tls_stream, &headers, inner_remainder).await?;
    let activity = state.start_network_activity(
        source_project.clone(),
        source_container.clone(),
        &method,
        &host,
        &path,
        "https",
        &headers,
        &body,
        ActivityState::Forwarding,
    );
    state.activity_line(&activity.id, "request body read");

    if source_project.is_none() {
        warn!(
            host = %host,
            method = %method,
            path = %path,
            source_container = ?source_container,
            source_status = source_status.as_str(),
            has_proxy_authorization,
            "proxy request missing source project metadata; permanent network rule persistence will not know which project to update"
        );
    }

    let policy = rules.match_network_for_port(&method, &host, &path, Some(443));
    let allowed = network_policy_allows(
        &state,
        &activity,
        policy,
        "waiting for network approval",
        &method,
        &host,
        Some(443),
        &path,
        source_project.clone(),
        source_container.clone(),
        source_status.as_str(),
        has_proxy_authorization,
    )
    .await;
    if !allowed {
        finish_blocked_network_activity(&state, &activity);
        write_error_any(&mut tls_stream, 403, "Forbidden by harness-hat policy").await?;
        return Ok(());
    }
    forward_request_with_activity(
        &state,
        &mut tls_stream,
        &activity,
        "https",
        &host,
        443,
        &path,
        &method,
        &headers,
        body,
    )
    .await
}

async fn read_tls_client_hello_prefix(stream: &mut TcpStream) -> Result<Vec<u8>> {
    // We only need enough of the ClientHello to recover SNI and route policy;
    // the rest of the handshake is forwarded untouched.
    let mut hdr = [0u8; 5];
    stream.read_exact(&mut hdr).await?;
    if hdr[0] != 0x16 {
        anyhow::bail!("not a TLS handshake record");
    }
    let len = u16::from_be_bytes([hdr[3], hdr[4]]) as usize;
    if len > 64 * 1024 {
        anyhow::bail!("TLS record too large");
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    let mut out = Vec::with_capacity(5 + len);
    out.extend_from_slice(&hdr);
    out.extend_from_slice(&body);
    Ok(out)
}
