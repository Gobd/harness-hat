use anyhow::Result;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tracing::warn;

use crate::activity::{Activity, ActivityState, wait_cancelled};
use crate::config;
use crate::proxy::helpers::{
    extract_host_port, parse_source_from_headers, proxy_authorization_matches_token,
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
    let Some((host, port)) = extract_host_port(&headers, &path, 80) else {
        write_error_any(&mut stream, 400, "Missing destination host").await?;
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
        return false;
    }
    match tokio::time::timeout(Duration::from_secs(300), rx).await {
        Ok(Ok(NetworkDecision::Allow)) => true,
        _ => false,
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
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let addrs = state.resolve_public_addrs(host, port).await?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, &addrs)
        .build()?;
    let url = build_url(scheme, host, port, path);

    let mut req = client.request(method, url);
    for (name, value) in headers {
        if !is_hop_by_hop(name) && !name.eq_ignore_ascii_case("host") {
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
    split_head_and_remainder(buf)
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
    let content_length = content_length_from_headers(headers);
    if content_length == 0 {
        return Ok(vec![]);
    }
    if content_length > MAX_PROXY_REQUEST_BODY_BYTES {
        anyhow::bail!(
            "request body too large: {content_length} bytes exceeds {MAX_PROXY_REQUEST_BODY_BYTES}"
        );
    }
    let mut body = Vec::with_capacity(content_length);
    body.extend_from_slice(&initial[..initial.len().min(content_length)]);
    if body.len() < content_length {
        let mut rest = vec![0u8; content_length - body.len()];
        stream.read_exact(&mut rest).await?;
        body.extend_from_slice(&rest);
    }
    Ok(body)
}

pub(crate) fn content_length_from_headers(headers: &[(String, String)]) -> usize {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
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

pub(crate) fn parse_request_line_and_headers(
    head: &str,
) -> Option<(String, String, Vec<(String, String)>)> {
    let mut lines = head.lines();
    let first = lines.next()?;
    let parts: Vec<&str> = first.splitn(3, ' ').collect();
    if parts.len() < 2 {
        return None;
    }
    let method = parts[0].to_string();
    let path = parts[1].to_string();

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    Some((method, path, headers))
}

pub(crate) fn parse_source_from_connect_head(
    head: &str,
) -> (Option<String>, Option<String>, SourceIdentityStatus) {
    let Some((_, _, headers)) = parse_request_line_and_headers(head) else {
        return (None, None, SourceIdentityStatus::MalformedAuthHeader);
    };
    parse_source_from_headers(&headers)
}

pub(crate) fn connect_head_has_proxy_authorization(head: &str) -> bool {
    let Some((_, _, headers)) = parse_request_line_and_headers(head) else {
        return false;
    };
    headers
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("proxy-authorization"))
}
