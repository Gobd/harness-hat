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
use lru::LruCache;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
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
    canonicalize_host, connect_public_tcp_with_priority, container_tls_passthrough_match,
    is_expected_disconnect, is_valid_signing_host, resolve_public_addrs_with_priority,
    tunnel_with_activity, write_error_any,
};
use crate::proxy::http::{
    finish_blocked_network_activity, handle_plain_http, handle_tls_inner_request,
    network_policy_allows,
};
use crate::rules::{ComposedRules, NetworkPolicy};
use crate::shared_config::SharedConfig;
use tracing::instrument;

const REQWEST_CLIENT_CACHE_CAPACITY: usize = 256;
const RULES_CACHE_CAPACITY: usize = 64;

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
    // Bounded (H12); senders use `try_send` and drop with a debug log on full.
    pub activity_tx: mpsc::Sender<ActivityEvent>,
    pub(crate) fixed_source: Option<FixedSourceIdentity>,
    connection_limiter: Arc<Semaphore>,
    connection_limit: usize,
    scoped_connection_limiter: Arc<Semaphore>,
    scoped_limited_connection_limiter: Arc<Semaphore>,
    source_connection_limiters: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    /// M1: bounded LRU of per-host reqwest clients, avoids rebuilding the TLS
    /// context + connection pool on every forwarded request.
    http_client_cache: Arc<Mutex<LruCache<String, reqwest::Client>>>,
    /// M2: cached composed rules per workspace, keyed by `(workspace_dir,
    /// mtime)`. On stat / parse failure we fall back to the last-known-good
    /// entry rather than returning 500 to the in-container client.
    rules_cache: Arc<Mutex<LruCache<PathBuf, RulesCacheEntry>>>,
}

#[derive(Clone)]
struct RulesCacheEntry {
    /// `None` if the rules file does not exist or stat failed.
    mtime: Option<SystemTime>,
    rules: Arc<ComposedRules>,
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
        activity_tx: mpsc::Sender<ActivityEvent>,
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
            http_client_cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(REQWEST_CLIENT_CACHE_CAPACITY)
                    .expect("non-zero client cache cap"),
            ))),
            rules_cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(RULES_CACHE_CAPACITY).expect("non-zero rules cache cap"),
            ))),
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

    pub(crate) fn has_configured_localhost_forward(&self, host: &str, port: u16) -> bool {
        self.localhost_forward_host_port(host, port).is_some()
    }

    pub(crate) async fn resolve_request_addrs(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>> {
        if let Some(host_port) = self.localhost_forward_host_port(host, port) {
            return Ok(vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host_port),
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), host_port),
            ]);
        }
        resolve_public_addrs_with_priority(host, port, self.is_primary_source()).await
    }

    pub(crate) async fn connect_public_tcp(&self, host: &str, port: u16) -> Result<TcpStream> {
        connect_public_tcp_with_priority(host, port, self.is_primary_source()).await
    }

    fn localhost_forward_host_port(&self, host: &str, port: u16) -> Option<u16> {
        if !is_container_host_alias(host) {
            return None;
        }
        let fixed = self.fixed_source.as_ref()?;
        let cfg = self.config.get();
        let ctr = cfg
            .containers
            .iter()
            .find(|ctr| ctr.name == fixed.container)?;
        ctr.localhost_forwards
            .iter()
            .find(|forward| forward.container_port == port)
            .map(|forward| forward.effective_host_port())
    }

    /// Return a `reqwest::Client` keyed on `host`, with the given resolved
    /// addresses pinned via `resolve_to_addrs`. Builds a fresh client on miss
    /// (and inserts into the LRU), returns a clone of the cached client on
    /// hit. Sharing clients reuses the TLS context and connection pool.
    pub(crate) fn http_client(
        &self,
        host: &str,
        port: u16,
        addrs: &[std::net::SocketAddr],
    ) -> Result<reqwest::Client> {
        let cache_key = format!("{host}:{port}");
        if let Ok(mut cache) = self.http_client_cache.lock()
            && let Some(client) = cache.get(&cache_key)
        {
            return Ok(client.clone());
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(host, addrs)
            .build()?;
        if let Ok(mut cache) = self.http_client_cache.lock() {
            cache.put(cache_key, client.clone());
        }
        Ok(client)
    }

    /// Load composed rules for `source_project`, caching the result keyed by
    /// the workspace path and the per-file mtimes. On stat/parse failure we
    /// fall back to the previously cached entry rather than returning an error
    /// so that a user editing `harness-rules.toml` in their editor does not
    /// cause transient 500s to forwarded requests.
    pub(crate) fn load_composed_rules(
        &self,
        source_project: Option<&str>,
    ) -> Result<Arc<ComposedRules>> {
        let cfg = self.config.get();
        let cache_key = self.rules_cache_key(&cfg, source_project);
        // Compute the current "version" of inputs via mtimes; if it matches a
        // cached entry, reuse it without parsing.
        let current_mtime = self.rules_inputs_mtime(&cfg, source_project);
        if let Ok(mut cache) = self.rules_cache.lock()
            && let Some(entry) = cache.get(&cache_key)
            && entry.mtime == current_mtime
        {
            return Ok(entry.rules.clone());
        }
        match config::load_composed_rules_for_workspace(&cfg, source_project) {
            Ok(rules) => {
                let arc = Arc::new(rules);
                if let Ok(mut cache) = self.rules_cache.lock() {
                    cache.put(
                        cache_key,
                        RulesCacheEntry {
                            mtime: current_mtime,
                            rules: arc.clone(),
                        },
                    );
                }
                Ok(arc)
            }
            Err(e) => {
                // Fall back to the last-known-good entry if we have one. This
                // covers the editor-saved-partial-file race (M2).
                if let Ok(mut cache) = self.rules_cache.lock()
                    && let Some(entry) = cache.get(&cache_key)
                {
                    warn!("proxy rules reload failed ({e}); falling back to last-known-good rules");
                    return Ok(entry.rules.clone());
                }
                Err(e)
            }
        }
    }

    fn rules_cache_key(
        &self,
        cfg: &crate::config::Config,
        source_project: Option<&str>,
    ) -> PathBuf {
        // The cache key combines the workspace directory (or a sentinel for
        // global-only) with the global rules file path so different workspaces
        // get their own entries.
        let workspace = source_project
            .and_then(|name| cfg.workspaces.iter().find(|p| p.name == name))
            .map(|w| w.canonical_path.clone())
            .unwrap_or_else(|| PathBuf::from("<global-only>"));
        // Include the global rules file in the key so swapping it invalidates.
        let mut key = workspace;
        key.push(
            cfg.manager
                .global_rules_file
                .file_name()
                .unwrap_or_default(),
        );
        key
    }

    fn rules_inputs_mtime(
        &self,
        cfg: &crate::config::Config,
        source_project: Option<&str>,
    ) -> Option<SystemTime> {
        // Combine the two input file mtimes into the LATEST one. If either
        // file is missing we still cache (mtime tracks Option::None too).
        let mut latest: Option<SystemTime> = None;
        if let Ok(meta) = std::fs::metadata(&cfg.manager.global_rules_file)
            && let Ok(mtime) = meta.modified()
        {
            latest = Some(latest.map_or(mtime, |cur| cur.max(mtime)));
        }
        if let Some(name) = source_project
            && let Some(project) = cfg.workspaces.iter().find(|p| p.name == name)
        {
            let path = project.canonical_path.join("harness-rules.toml");
            if let Ok(meta) = std::fs::metadata(&path)
                && let Ok(mtime) = meta.modified()
            {
                latest = Some(latest.map_or(mtime, |cur| cur.max(mtime)));
            }
        }
        latest
    }
}

fn is_container_host_alias(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if matches!(
        host.as_str(),
        "localhost" | "host.docker.internal" | "host.containers.internal"
    ) {
        return true;
    }
    host.parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
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
            .try_send(ActivityEvent::Started(Box::new(activity.clone())));
        activity
    }

    pub(crate) fn activity_state(
        &self,
        id: &str,
        state: ActivityState,
        status: impl Into<Option<String>>,
    ) {
        let _ = self.activity_tx.try_send(ActivityEvent::State {
            id: id.to_string(),
            state,
            status: status.into(),
        });
    }

    pub(crate) fn activity_line(&self, id: &str, line: impl Into<String>) {
        let _ = self.activity_tx.try_send(ActivityEvent::Line {
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
        let _ = self.activity_tx.try_send(ActivityEvent::Finished {
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
                // M8: opportunistically reap finished tasks to keep the JoinSet
                // from growing forever under steady accept load. The select
                // arm below only fires when select picks it, which is rare on
                // a busy listener.
                while let Some(joined) = tasks.try_join_next() {
                    if let Err(e) = joined {
                        debug!("proxy connection task ended: {e}");
                    }
                }
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
    // L1: `TcpStream::peek` can return short — e.g. just 1 or 2 bytes — even
    // when the client sent the full "CONNECT " token. The previous code would
    // misroute such a short peek into the TLS or plain-HTTP arm. Loop until
    // we have at least 7 bytes (the length of "CONNECT") or the first-byte
    // timeout elapses.
    let mut peek = [0u8; 8];
    let mut n = 0usize;
    let deadline = tokio::time::Instant::now() + FIRST_BYTE_TIMEOUT;
    while n < 7 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.peek(&mut peek)).await {
            Ok(Ok(0)) => break, // remote closed
            Ok(Ok(got)) => {
                if got <= n {
                    // No new bytes since last peek; yield briefly so we don't
                    // busy-loop. peek() does not advance, so a short peek
                    // followed by another peek for the same bytes is normal.
                    tokio::task::yield_now().await;
                }
                n = got;
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "proxy connection timed out waiting for first byte"
                ));
            }
        }
    }

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
        // M7: an `AsyncRead` impl must tolerate a zero-remaining buffer; the
        // previous `debug_assert!(after > before)` would have panicked here.
        if buf.remaining() == 0 {
            return std::task::Poll::Ready(Ok(()));
        }
        if (self.prefix.position() as usize) < self.prefix.get_ref().len() {
            let pos = self.prefix.position();
            let rem = &self.prefix.get_ref()[pos as usize..];
            let to_copy = rem.len().min(buf.remaining());
            buf.put_slice(&rem[..to_copy]);
            self.prefix.set_position(pos + to_copy as u64);
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

    let prefix = read_tls_client_hello_prefix(&mut stream).await?;
    let Some(sni_raw) = parse_sni_from_tls_client_hello(&prefix) else {
        warn!("transparent TLS connection missing SNI; dropping");
        return Ok(());
    };
    // CR4: canonicalize before anything policy-related touches it. SNI
    // strings often arrive lowercased already but we accept whatever the
    // client gave us; rules matching expects the normalized form.
    let host = match canonicalize_host(&sni_raw) {
        Ok(h) => h,
        Err(e) => {
            warn!(sni = %sni_raw, "transparent TLS rejected: {e}");
            return Ok(());
        }
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

    if let Some(bypass_pattern) = container_tls_passthrough_match(
        &state.config.get(),
        source_container.as_deref(),
        &host,
    )
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

    let rules = match state.load_composed_rules(source_project.as_deref()) {
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
        if let Err(e) = state.resolve_request_addrs(&host, 443).await {
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

    let prefixed = PrefixedTcpStream {
        prefix: std::io::Cursor::new(prefix),
        inner: stream,
    };

    // Gate attacker-controllable SNI/Host before it reaches the cert signer so
    // it can never become a cache key or certificate subject of arbitrary shape.
    if !is_valid_signing_host(&host) {
        state.activity_finished(
            &connect_activity.id,
            ActivityState::Failed,
            Some(format!(
                "refusing to sign leaf cert for invalid host {host:?}"
            )),
        );
        return Ok(());
    }
    let server_config = state.ca.leaf_server_config(&host).await?;
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

    handle_tls_inner_request(
        &state,
        &mut tls_stream,
        rules.as_ref(),
        &host,
        443,
        source_project,
        source_container,
        source_status,
        has_proxy_authorization,
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
