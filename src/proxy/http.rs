use anyhow::Result;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tracing::warn;

use crate::activity::{Activity, ActivityState, wait_cancelled};
use crate::config;
use crate::proxy::helpers::{
    connection_hop_tokens, ensure_host_header_matches_target, extract_host_port,
    is_hop_by_hop_with_extra, parse_source_from_headers, proxy_authorization_matches_token,
    strip_scheme_and_host, write_error_any, write_response_any,
};
use crate::proxy::{NetworkDecision, PendingNetworkItem, ProxyState, SourceIdentityStatus};
use crate::rules::NetworkPolicy;

const MAX_PROXY_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;

// ── Plain HTTP ────────────────────────────────────────────────────────────────

pub(crate) async fn handle_plain_http(mut stream: TcpStream, state: ProxyState) -> Result<()> {
    let (head, body_remainder) = read_request_head_any(&mut stream).await?;
    let head_str = match std::str::from_utf8(&head) {
        Ok(s) => s,
        Err(_) => {
            write_error_any(&mut stream, 400, "Bad Request").await?;
            return Ok(());
        }
    };

    let cfg = state.config.get();

    let (method, path, headers) = match parse_request_line_and_headers(head_str) {
        Some(r) => r,
        None => {
            write_error_any(&mut stream, 400, "Bad Request").await?;
            return Ok(());
        }
    };
    // H4: reject unknown / non-token HTTP methods up front rather than
    // silently rewriting them to GET further down.
    if reqwest::Method::from_bytes(method.as_bytes()).is_err() {
        write_error_any(&mut stream, 400, "Bad Request: unknown method").await?;
        return Ok(());
    }
    let (source_project, source_container, source_status, has_proxy_authorization): (
        Option<String>,
        Option<String>,
        SourceIdentityStatus,
        bool,
    ) = if let Some(fixed) = &state.fixed_source {
        if !proxy_authorization_matches_token(&headers, &fixed.auth_token) {
            write_error_any(&mut stream, 407, "Proxy Authentication Required").await?;
            return Ok(());
        }
        (
            Some(fixed.project.clone()),
            Some(fixed.container.clone()),
            SourceIdentityStatus::ListenerBoundSource,
            false,
        )
    } else {
        let (project, container, status) = parse_source_from_headers(&headers);

        let has_auth = headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("proxy-authorization"));
        (project, container, status, has_auth)
    };
    // H3: strict Host validation on the plain-HTTP path. `extract_host_port`
    // returns `None` for missing Host, duplicate Host, or invalid host syntax.
    let Some((host, port)) = extract_host_port(&headers, &path, 80) else {
        write_error_any(&mut stream, 400, "Missing or invalid Host header").await?;
        return Ok(());
    };
    let Some(_source_permit) =
        state.try_acquire_source_connection(source_project.as_deref(), source_container.as_deref())
    else {
        tracing::warn!(
            host = %host,
            port,
            source_project = ?source_project,
            source_container = ?source_container,
            source_status = source_status.as_str(),
            has_proxy_authorization,
            "proxy source connection limit reached"
        );
        write_error_any(&mut stream, 503, "Proxy connection limit reached").await?;
        return Ok(());
    };
    let path = strip_scheme_and_host(&path);

    let body = read_body_any(&mut stream, &headers, body_remainder).await?;
    let activity = state.start_network_activity(
        source_project.clone(),
        source_container.clone(),
        &method,
        &host,
        &path,
        "http",
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

    let rules = match config::load_composed_rules_for_workspace(&cfg, source_project.as_deref()) {
        Ok(rules) => rules,
        Err(e) => {
            warn!("proxy rules load error: {e}");
            state.activity_finished(
                &activity.id,
                ActivityState::Failed,
                Some("invalid harness-rules.toml configuration".to_string()),
            );
            write_error_any(&mut stream, 500, "Invalid harness-rules.toml configuration").await?;
            return Ok(());
        }
    };
    let policy = rules.match_network_for_port(&method, &host, &path, Some(port));

    if policy != NetworkPolicy::Deny {
        if let Err(e) = state.resolve_public_addrs(&host, port).await {
            state.activity_finished(&activity.id, ActivityState::Denied, Some(e.to_string()));
            write_error_any(&mut stream, 403, "Forbidden by harness-hat policy").await?;
            return Ok(());
        }
    }

    let allowed = network_policy_allows(
        &state,
        &activity,
        policy,
        "waiting for network approval",
        &method,
        &host,
        Some(port),
        &path,
        source_project.clone(),
        source_container.clone(),
        source_status.as_str(),
        has_proxy_authorization,
    )
    .await;

    if !allowed {
        finish_blocked_network_activity(&state, &activity);
        write_error_any(&mut stream, 403, "Forbidden by harness-hat policy").await?;
        return Ok(());
    }

    if activity.is_cancelled() {
        state.activity_finished(
            &activity.id,
            ActivityState::Cancelled,
            Some("cancelled before forwarding".to_string()),
        );
        return Ok(());
    }

    forward_request_with_activity(
        &state,
        &mut stream,
        &activity,
        "http",
        &host,
        port,
        &path,
        &method,
        &headers,
        body,
    )
    .await
}

pub(crate) async fn prompt_network(
    state: &ProxyState,
    method: &str,
    host: &str,
    port: Option<u16>,
    path: &str,
    source_project: Option<String>,
    source_container: Option<String>,
    source_status: &str,
    has_proxy_authorization: bool,
    activity: Option<&Activity>,
) -> bool {
    let (tx, rx) = oneshot::channel();
    let (activity_id, cancel_flag) = activity
        .map(|activity| (activity.id.clone(), activity.cancel_flag.clone()))
        .unwrap_or_else(|| {
            (
                uuid::Uuid::new_v4().to_string(),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
        });
    let item = PendingNetworkItem {
        activity_id,
        cancel_flag,
        source_project,
        source_container,
        source_status: source_status.to_string(),
        has_proxy_authorization,
        method: method.to_string(),
        host: host.to_string(),
        port,
        path: path.to_string(),
        response_tx: tx,
        merged_response_txs: Vec::new(),
    };
    if state.pending_tx.send(item).await.is_err() {
        // M9: TUI receiver dropped (TUI quit or crashed). Distinguish from a
        // timeout so operators see something in logs other than silent deny.
        warn!(
            host = %host,
            method = %method,
            "proxy: pending_tx send failed — TUI receiver dropped; denying network request"
        );
        return false;
    }
    match tokio::time::timeout(Duration::from_secs(300), rx).await {
        Ok(Ok(NetworkDecision::Allow)) => true,
        Ok(Ok(NetworkDecision::Deny)) => false,
        Ok(Err(_recv_err)) => {
            warn!(
                host = %host,
                method = %method,
                "proxy: response channel closed before decision; denying"
            );
            false
        }
        Err(_elapsed) => false,
    }
}

pub(crate) async fn network_policy_allows(
    state: &ProxyState,
    activity: &Activity,
    policy: NetworkPolicy,
    pending_status: &str,
    method: &str,
    host: &str,
    port: Option<u16>,
    path: &str,
    source_project: Option<String>,
    source_container: Option<String>,
    source_status: &str,
    has_proxy_authorization: bool,
) -> bool {
    match policy {
        NetworkPolicy::Auto => true,
        NetworkPolicy::Deny => false,
        NetworkPolicy::Prompt => {
            state.activity_state(
                &activity.id,
                ActivityState::PendingApproval,
                Some(pending_status.to_string()),
            );
            prompt_network(
                state,
                method,
                host,
                port,
                path,
                source_project,
                source_container,
                source_status,
                has_proxy_authorization,
                Some(activity),
            )
            .await
        }
    }
}

pub(crate) fn finish_blocked_network_activity(state: &ProxyState, activity: &Activity) {
    let state_label = if activity.is_cancelled() {
        ActivityState::Cancelled
    } else {
        ActivityState::Denied
    };
    state.activity_finished(
        &activity.id,
        state_label,
        Some("blocked by network policy".to_string()),
    );
}

/// Handle a decrypted MITM request after the TLS tunnel is established: read the
/// inner request, enforce the Host header, evaluate network policy (prompting if
/// needed), then forward it upstream. Shared by the transparent-TLS (`core`) and
/// CONNECT (`connect`) MITM paths, which differ only in how they build
/// `tls_stream` and the destination `port`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_tls_inner_request<S>(
    state: &ProxyState,
    tls_stream: &mut S,
    rules: &crate::rules::ComposedRules,
    host: &str,
    port: u16,
    source_project: Option<String>,
    source_container: Option<String>,
    source_status: SourceIdentityStatus,
    has_proxy_authorization: bool,
) -> Result<()>
where
    S: AsyncRead + AsyncWriteExt + Unpin,
{
    let (inner_head, inner_remainder) = read_request_head_any(tls_stream).await?;
    let inner_str = match std::str::from_utf8(&inner_head) {
        Ok(s) => s,
        Err(_) => {
            write_error_any(tls_stream, 400, "Bad Request").await?;
            return Ok(());
        }
    };
    let (method, path, headers) = match parse_request_line_and_headers(inner_str) {
        Some(r) => r,
        None => {
            write_error_any(tls_stream, 400, "Bad Request").await?;
            return Ok(());
        }
    };
    // H4: reject unknown methods early on the MITM path too.
    if reqwest::Method::from_bytes(method.as_bytes()).is_err() {
        write_error_any(tls_stream, 400, "Bad Request: unknown method").await?;
        return Ok(());
    }
    // L16: `port` here is the CONNECT/TLS target port (e.g. 443 on the
    // transparent-TLS path). It must stay in sync with the listener handler's
    // hardcoded port — `proxy/core.rs::handle_transparent_tls` passes 443.
    if ensure_host_header_matches_target(&headers, host, port).is_err() {
        write_error_any(tls_stream, 400, "Bad Request").await?;
        return Ok(());
    }
    let body = read_body_any(tls_stream, &headers, inner_remainder).await?;
    let activity = state.start_network_activity(
        source_project.clone(),
        source_container.clone(),
        &method,
        host,
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

    let policy = rules.match_network_for_port(&method, host, &path, Some(port));
    let allowed = network_policy_allows(
        state,
        &activity,
        policy,
        "waiting for network approval",
        &method,
        host,
        Some(port),
        &path,
        source_project,
        source_container,
        source_status.as_str(),
        has_proxy_authorization,
    )
    .await;
    if !allowed {
        finish_blocked_network_activity(state, &activity);
        write_error_any(tls_stream, 403, "Forbidden by harness-hat policy").await?;
        return Ok(());
    }
    forward_request_with_activity(
        state,
        tls_stream,
        &activity,
        "https",
        host,
        port,
        &path,
        &method,
        &headers,
        body,
    )
    .await
}

pub(crate) async fn forward_request_with_activity<W>(
    state: &ProxyState,
    stream: &mut W,
    activity: &Activity,
    scheme: &str,
    host: &str,
    port: u16,
    path: &str,
    method: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    if activity.is_cancelled() {
        state.activity_finished(
            &activity.id,
            ActivityState::Cancelled,
            Some("cancelled before forwarding".to_string()),
        );
        return Ok(());
    }

    let url = build_url(scheme, host, port, path);
    state.activity_state(
        &activity.id,
        ActivityState::Forwarding,
        Some(format!("forwarding to {url}")),
    );

    let response = tokio::select! {
        response = forward_request(state, method, scheme, host, port, path, headers, body) => match response {
            Ok(response) => response,
            Err(e) => {
                state.activity_finished(&activity.id, ActivityState::Failed, Some(e.to_string()));
                return Err(e);
            }
        },
        _ = wait_cancelled(activity.cancel_flag.clone()) => {
            state.activity_finished(
                &activity.id,
                ActivityState::Cancelled,
                Some("cancelled".to_string()),
            );
            return Ok(());
        }
    };

    state.activity_line(
        &activity.id,
        format!("upstream response {}", response.status()),
    );

    match write_response_any(stream, response).await {
        Ok(()) => {
            state.activity_finished(
                &activity.id,
                ActivityState::Complete,
                Some("response forwarded".to_string()),
            );
            Ok(())
        }
        Err(e) => {
            state.activity_finished(&activity.id, ActivityState::Failed, Some(e.to_string()));
            Err(e)
        }
    }
}

pub(crate) async fn forward_request(
    state: &ProxyState,
    method: &str,
    scheme: &str,
    host: &str,
    port: u16,
    path: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
) -> Result<reqwest::Response> {
    // H4: unknown HTTP methods must be rejected (caller turns this into 400),
    // not silently rewritten to GET.
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|e| anyhow::anyhow!("invalid HTTP method: {e}"))?;
    let addrs = state.resolve_public_addrs(host, port).await?;
    // M1: reuse a cached reqwest client across requests instead of rebuilding
    // (and re-doing TLS bootstrap) per call.
    let client = state.http_client(host, &addrs)?;
    let url = build_url(scheme, host, port, path);

    let mut req = client.request(method, url);
    // H2: strip not just the fixed hop-by-hop list but every header token
    // named in the request's `Connection:` value.
    let extra_hop = connection_hop_tokens(headers);
    for (name, value) in headers {
        if !is_hop_by_hop_with_extra(name, &extra_hop) && !name.eq_ignore_ascii_case("host") {
            req = req.header(name.as_str(), value.as_str());
        }
    }
    if !body.is_empty() {
        req = req.body(body);
    }
    let response = req.send().await?;
    Ok(response)
}

fn build_url(scheme: &str, host: &str, port: u16, path: &str) -> String {
    format!(
        "{scheme}://{}{}",
        authority_for_url(scheme, host, port),
        path
    )
}

fn authority_for_url(scheme: &str, host: &str, port: u16) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let default_port = (scheme == "http" && port == 80) || (scheme == "https" && port == 443);
    if default_port {
        host
    } else {
        format!("{host}:{port}")
    }
}

pub(crate) fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

pub(crate) async fn read_request_head_any<R>(stream: &mut R) -> Result<(Vec<u8>, Vec<u8>)>
where
    R: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if contains_double_crlf(&buf) {
            break;
        }
        if buf.len() > 64 * 1024 {
            anyhow::bail!("request head too large");
        }
    }
    // CR3: reject heads framed with bare LF (\n with no preceding \r). Real
    // clients send CRLF; bare-LF is a smuggling vector between front and
    // back-end servers that interpret framing differently.
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("incomplete request head"))?;
    let head_slice = &buf[..head_end + 4];
    validate_head_line_endings(head_slice)?;
    split_head_and_remainder(buf)
}

/// Reject bare CR or bare LF inside the head (anywhere outside a proper CRLF
/// pair) and reject obs-fold continuation lines that start with whitespace.
/// Returns `Ok(())` for an RFC-7230-conformant head.
fn validate_head_line_endings(head: &[u8]) -> Result<()> {
    let mut i = 0;
    let mut at_line_start = true;
    while i < head.len() {
        let b = head[i];
        if b == b'\r' {
            // CR must be followed by LF.
            if head.get(i + 1) != Some(&b'\n') {
                anyhow::bail!("bare CR in request head");
            }
            i += 2;
            // After CRLF a header line is either content or end-of-head.
            // Reject obs-fold: a line that begins with SP or HTAB is a
            // continuation of the previous header. RFC 7230 says obs-fold is
            // a MUST-NOT for proxies to forward.
            if let Some(&next) = head.get(i)
                && (next == b' ' || next == b'\t')
                && !at_line_start_is_end_of_head(head, i)
            {
                anyhow::bail!("obs-fold continuation in request head");
            }
            at_line_start = true;
            continue;
        }
        if b == b'\n' {
            // Bare LF (no preceding CR).
            anyhow::bail!("bare LF in request head");
        }
        at_line_start = false;
        i += 1;
    }
    let _ = at_line_start;
    Ok(())
}

/// True if position `i` points at the end-of-head marker (CRLF already
/// consumed), so a leading-whitespace byte at `i` is actually trailing body
/// padding rather than an obs-fold. In practice the head we receive is sliced
/// at `\r\n\r\n` exactly, so this only guards against off-by-one near the
/// final blank line.
fn at_line_start_is_end_of_head(head: &[u8], i: usize) -> bool {
    head.get(i.saturating_sub(2)..i) == Some(b"\r\n")
        && head.get(i.saturating_sub(4)..i.saturating_sub(2)) == Some(b"\r\n")
}

pub(crate) fn contains_double_crlf(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n")
}

pub(crate) fn split_head_and_remainder(buf: Vec<u8>) -> Result<(Vec<u8>, Vec<u8>)> {
    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        let end = end + 4;
        Ok((buf[..end].to_vec(), buf[end..].to_vec()))
    } else {
        anyhow::bail!("incomplete request head")
    }
}

pub(crate) async fn read_body_any<R>(
    stream: &mut R,
    headers: &[(String, String)],
    initial: Vec<u8>,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    // CR3: reject conflicting framing on the request side.
    // Smuggling primer: a request with both `Content-Length` and
    // `Transfer-Encoding` is interpreted differently by front and back-end
    // proxies. Refuse it outright.
    let has_te = headers
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("transfer-encoding"));
    let cl_values: Vec<&str> = headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.as_str())
        .collect();
    if !cl_values.is_empty() && has_te {
        anyhow::bail!("request has both Content-Length and Transfer-Encoding");
    }
    if cl_values.len() > 1 {
        // Duplicate Content-Length values — even if equal, refuse (RFC 7230 §3.3.2
        // allows servers to fold equal values, but proxies are safer rejecting).
        anyhow::bail!("request has multiple Content-Length headers");
    }
    let content_length = cl_values
        .first()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if content_length == 0 {
        // Any bytes past the head when CL=0 (and no TE) are a smuggling signal.
        if !initial.is_empty() {
            anyhow::bail!(
                "request has trailing body bytes despite Content-Length: 0 and no Transfer-Encoding"
            );
        }
        return Ok(vec![]);
    }
    if content_length > MAX_PROXY_REQUEST_BODY_BYTES {
        anyhow::bail!(
            "request body too large: {content_length} bytes exceeds {MAX_PROXY_REQUEST_BODY_BYTES}"
        );
    }
    // CR3: reject bytes past Content-Length. Previously these were silently
    // dropped with `.min(content_length)`.
    if initial.len() > content_length {
        anyhow::bail!(
            "request has {} bytes of body but Content-Length is {content_length}",
            initial.len()
        );
    }
    let mut body = Vec::with_capacity(content_length);
    body.extend_from_slice(&initial);
    if body.len() < content_length {
        let mut rest = vec![0u8; content_length - body.len()];
        stream.read_exact(&mut rest).await?;
        body.extend_from_slice(&rest);
    }
    Ok(body)
}

pub(crate) fn parse_connect_target(head: &str) -> Option<(String, u16)> {
    let first_line = head.lines().next()?;
    let parts: Vec<&str> = first_line.splitn(3, ' ').collect();
    if parts.len() < 2 {
        return None;
    }
    let authority = parts[1];
    if authority.starts_with('[') {
        let end = authority.find(']')?;
        let host = authority[1..end].to_string();
        let port = authority[end + 1..].strip_prefix(':')?.parse().ok()?;
        return Some((host, port));
    }
    let (host, port) = authority.rsplit_once(':')?;
    Some((host.to_string(), port.parse().ok()?))
}

/// Collect `Name: Value` header pairs from the lines after the request line,
/// stopping at the first blank line. Shared by the head-parsing helpers below.
fn collect_headers<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    headers
}

/// True when the request line has at least a method and a target.
fn request_line_is_valid(first: &str) -> bool {
    first.splitn(3, ' ').count() >= 2
}

pub(crate) fn parse_request_line_and_headers(
    head: &str,
) -> Option<(String, String, Vec<(String, String)>)> {
    let mut lines = head.lines();
    let first = lines.next()?;
    let parts: Vec<&str> = first.splitn(3, ' ').collect();
    if parts.len() < 2 {
        return None;
    }
    Some((
        parts[0].to_string(),
        parts[1].to_string(),
        collect_headers(lines),
    ))
}

/// Parse only the header pairs from an HTTP head, skipping the method/path
/// allocations that header-only callers discard. Returns `None` for a malformed
/// request line, matching `parse_request_line_and_headers`.
fn headers_from_head(head: &str) -> Option<Vec<(String, String)>> {
    let mut lines = head.lines();
    if !request_line_is_valid(lines.next()?) {
        return None;
    }
    Some(collect_headers(lines))
}

pub(crate) fn parse_source_from_connect_head(
    head: &str,
) -> (Option<String>, Option<String>, SourceIdentityStatus) {
    let Some(headers) = headers_from_head(head) else {
        return (None, None, SourceIdentityStatus::MalformedAuthHeader);
    };
    parse_source_from_headers(&headers)
}

pub(crate) fn connect_head_has_proxy_authorization(head: &str) -> bool {
    headers_from_head(head).is_some_and(|headers| {
        headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("proxy-authorization"))
    })
}
