use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures::StreamExt;
use globset::Glob;
use reqwest::StatusCode;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, copy_bidirectional};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

use crate::activity::{Activity, ActivityState, wait_cancelled};
use crate::config::Config;
use crate::proxy::SourceIdentityStatus;
use crate::proxy::http::is_hop_by_hop;

const DNS_LOOKUP_LIMIT: usize = 16;
const DNS_CACHE_TTL: Duration = Duration::from_secs(300);
const DNS_STALE_TTL: Duration = Duration::from_secs(1800);

static DNS_LOOKUP_LIMITER: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(DNS_LOOKUP_LIMIT)));
static DNS_CACHE: LazyLock<Mutex<HashMap<String, DnsCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
struct DnsCacheEntry {
    addrs: Vec<SocketAddr>,
    stored_at: Instant,
}

pub(crate) fn format_byte_count(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes}b");
    }

    let mut value = bytes as f64;
    let mut unit = "b";
    for next_unit in ["kb", "mb", "gb", "tb"] {
        value /= 1024.0;
        unit = next_unit;
        if value < 1024.0 {
            break;
        }
    }

    if value >= 10.0 || value.fract().abs() < f64::EPSILON {
        format!("{value:.0}{unit}")
    } else {
        format!("{value:.1}{unit}")
    }
}

pub(crate) async fn tunnel_with_activity<D, U>(
    state: &crate::proxy::ProxyState,
    activity: &Activity,
    downstream: &mut D,
    upstream: &mut U,
) -> Result<()>
where
    D: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    tokio::select! {
        result = copy_bidirectional(downstream, upstream) => match result {
            Ok((from_client, from_server)) => {
                state.activity_line(
                    &activity.id,
                    format!(
                        "tunnel closed after {} upstream, {} downstream",
                        format_byte_count(from_client),
                        format_byte_count(from_server)
                    ),
                );
                state.activity_finished(
                    &activity.id,
                    ActivityState::Complete,
                    Some("tunnel closed".to_string()),
                );
                Ok(())
            }
            Err(e) => {
                state.activity_finished(&activity.id, ActivityState::Failed, Some(e.to_string()));
                Err(e.into())
            }
        },
        _ = wait_cancelled(activity.cancel_flag.clone()) => {
            state.activity_finished(
                &activity.id,
                ActivityState::Cancelled,
                Some("cancelled".to_string()),
            );
            Ok(())
        }
    }
}

pub(crate) fn parse_source_from_headers(
    headers: &[(String, String)],
) -> (Option<String>, Option<String>, SourceIdentityStatus) {
    let auth = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("proxy-authorization"))
        .map(|(_, v)| v.as_str());
    let Some(auth) = auth else {
        return (None, None, SourceIdentityStatus::MissingProxyAuthorization);
    };
    decode_source_from_proxy_authorization(auth)
}

pub(crate) fn proxy_authorization_matches_token(
    headers: &[(String, String)],
    expected_token: &str,
) -> bool {
    let Some(auth) = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("proxy-authorization"))
        .map(|(_, v)| v.as_str())
    else {
        return false;
    };
    let Some((scheme, payload)) = auth.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        return false;
    }
    let Ok(decoded) = STANDARD.decode(payload.trim()) else {
        return false;
    };
    let Ok(creds) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((username, password)) = creds.split_once(':') else {
        return false;
    };
    let username = percent_decode_basic_credential(username);
    let password = percent_decode_basic_credential(password);
    username == "harness-hat" && password == expected_token
}

fn percent_decode_basic_credential(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn decode_source_from_proxy_authorization(
    value: &str,
) -> (Option<String>, Option<String>, SourceIdentityStatus) {
    let Some((scheme, payload)) = value.split_once(' ') else {
        return (None, None, SourceIdentityStatus::MalformedAuthHeader);
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        return (None, None, SourceIdentityStatus::UnsupportedAuthScheme);
    }
    let decoded = match STANDARD.decode(payload.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return (None, None, SourceIdentityStatus::InvalidBase64),
    };
    let creds = match String::from_utf8(decoded) {
        Ok(s) => s,
        Err(_) => return (None, None, SourceIdentityStatus::InvalidUtf8),
    };
    let Some((username, password)) = creds.split_once(':') else {
        return (
            None,
            None,
            SourceIdentityStatus::MissingUsernamePasswordDelimiter,
        );
    };
    if username != "zcsrc" {
        return (None, None, SourceIdentityStatus::UnexpectedUsername);
    }
    let Some((project_enc, container_enc)) = password.split_once('.') else {
        return (
            None,
            None,
            SourceIdentityStatus::MissingProjectContainerDelimiter,
        );
    };
    let project = match URL_SAFE_NO_PAD.decode(project_enc.as_bytes()) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(value) => value,
            Err(_) => return (None, None, SourceIdentityStatus::InvalidProjectEncoding),
        },
        Err(_) => return (None, None, SourceIdentityStatus::InvalidProjectEncoding),
    };
    let container = match URL_SAFE_NO_PAD.decode(container_enc.as_bytes()) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(value) => value,
            Err(_) => return (None, None, SourceIdentityStatus::InvalidContainerEncoding),
        },
        Err(_) => return (None, None, SourceIdentityStatus::InvalidContainerEncoding),
    };
    (Some(project), Some(container), SourceIdentityStatus::Ok)
}

pub(crate) fn container_tls_passthrough_match<'a>(
    config: &'a Config,
    source_container: Option<&str>,
    host: &str,
) -> Option<&'a str> {
    let source_container = source_container?;
    let container = config
        .containers
        .iter()
        .find(|c| c.name == source_container)?;
    container
        .bypass_proxy
        .iter()
        .find(|pattern| bypass_host_matches(pattern, host))
        .map(String::as_str)
}

pub(crate) fn bypass_host_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if pattern == "*" {
        return true;
    }

    let host_lc = host.to_ascii_lowercase();
    let pattern_lc = pattern.to_ascii_lowercase();

    if let Some(apex) = pattern_lc.strip_prefix('.') {
        return host_lc == apex || host_lc.ends_with(&format!(".{apex}"));
    }

    if let Some(apex) = pattern_lc.strip_prefix("*.") {
        return host_lc == apex || host_lc.ends_with(&format!(".{apex}"));
    }

    if !pattern_lc.contains('*') {
        return host_lc == pattern_lc;
    }

    Glob::new(&pattern_lc)
        .ok()
        .map(|g| g.compile_matcher().is_match(&host_lc))
        .unwrap_or(false)
}

pub(crate) fn extract_host_port(
    headers: &[(String, String)],
    path: &str,
    default_port: u16,
) -> Option<(String, u16)> {
    if let Some((_, v)) = headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("host")) {
        return Some(split_host_port(v.trim(), default_port));
    }
    if path.starts_with("http://") || path.starts_with("https://") {
        if let Ok(url) = path.parse::<url::Url>() {
            let host = url.host_str()?.to_string();
            let port = url.port_or_known_default().unwrap_or(default_port);
            return Some((host, port));
        }
    }
    None
}

pub(crate) fn ensure_host_header_matches_target(
    headers: &[(String, String)],
    expected_host: &str,
    expected_port: u16,
) -> Result<()> {
    let mut host_headers = headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim());
    let Some(host_header) = host_headers.next() else {
        return Ok(());
    };
    anyhow::ensure!(
        host_headers.next().is_none(),
        "multiple Host headers are not allowed"
    );

    let (actual_host, actual_port) = split_host_port(host_header, expected_port);
    anyhow::ensure!(
        actual_port == expected_port && host_names_match(&actual_host, expected_host),
        "Host header {host_header} does not match TLS target {expected_host}:{expected_port}"
    );
    Ok(())
}

fn host_names_match(actual: &str, expected: &str) -> bool {
    let actual = actual.trim().trim_end_matches('.');
    let expected = expected.trim().trim_end_matches('.');

    if let (Ok(actual_ip), Ok(expected_ip)) = (actual.parse::<IpAddr>(), expected.parse::<IpAddr>())
    {
        return actual_ip == expected_ip;
    }

    actual.eq_ignore_ascii_case(expected)
}

pub(crate) fn split_host_port(authority: &str, default_port: u16) -> (String, u16) {
    let host = authority.trim();
    if host.starts_with('[') {
        if let Some(end) = host.find(']') {
            let port = host[end + 1..]
                .strip_prefix(':')
                .and_then(|raw| raw.parse::<u16>().ok())
                .unwrap_or(default_port);
            return (host[1..end].to_string(), port);
        }
        return (host.to_string(), default_port);
    }
    if host.matches(':').count() == 1
        && let Some((name, raw_port)) = host.rsplit_once(':')
        && !name.is_empty()
        && let Ok(port) = raw_port.parse::<u16>()
    {
        return (name.to_string(), port);
    }
    (host.to_string(), default_port)
}

pub(crate) fn strip_scheme_and_host(path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        if let Ok(url) = path.parse::<url::Url>() {
            let mut result = url.path().to_string();
            if let Some(q) = url.query() {
                result.push('?');
                result.push_str(q);
            }
            return result;
        }
    }
    path.to_string()
}

pub(crate) async fn write_response_any<W>(sink: &mut W, response: reqwest::Response) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let status = response.status().as_u16();
    let reason = response.status().canonical_reason().unwrap_or("Unknown");

    let resp_headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name.as_str()))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.to_string(), v.to_string()))
        })
        .collect();

    let content_length: Option<u64> = resp_headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok());

    let use_chunked = content_length.is_none();

    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in &resp_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if use_chunked {
        head.push_str("Transfer-Encoding: chunked\r\n");
    }
    head.push_str("Connection: close\r\n");
    head.push_str("\r\n");
    sink.write_all(head.as_bytes()).await?;

    let mut body_stream = response.bytes_stream();
    while let Some(chunk) = body_stream.next().await {
        let chunk = chunk?;
        if chunk.is_empty() {
            continue;
        }
        if use_chunked {
            sink.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                .await?;
            sink.write_all(&chunk).await?;
            sink.write_all(b"\r\n").await?;
        } else {
            sink.write_all(&chunk).await?;
        }
    }
    if use_chunked {
        sink.write_all(b"0\r\n\r\n").await?;
    }
    Ok(())
}

pub(crate) async fn write_error_any<W>(sink: &mut W, code: u16, msg: &str) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let body = msg.as_bytes();
    let reason = StatusCode::from_u16(code)
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        .canonical_reason()
        .unwrap_or("Unknown");

    let out = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut raw = out.into_bytes();
    raw.extend_from_slice(body);
    sink.write_all(&raw).await?;
    Ok(())
}

pub(crate) fn is_expected_disconnect(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("close_notify")
        || msg.contains("unexpected eof")
        || msg.contains("connection reset by peer")
        || msg.contains("broken pipe")
}

#[cfg(test)]
pub(crate) async fn resolve_public_addrs(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    resolve_public_addrs_with_priority(host, port, false).await
}

pub(crate) async fn resolve_public_addrs_with_priority(
    host: &str,
    port: u16,
    primary: bool,
) -> Result<Vec<SocketAddr>> {
    anyhow::ensure!(!host.trim().is_empty(), "destination host is empty");

    if let Ok(ip) = host.parse::<IpAddr>() {
        ensure_public_ip(ip, host)?;
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let cache_key = dns_cache_key(host, port);
    if let Some(addrs) = cached_dns_addrs(&cache_key, DNS_CACHE_TTL) {
        return Ok(addrs);
    }

    let _lookup_permit = if primary {
        None
    } else {
        DNS_LOOKUP_LIMITER.clone().acquire_owned().await.ok()
    };

    let attempts = if primary { 4 } else { 2 };
    let mut last_error = None;
    for attempt in 0..attempts {
        match tokio::net::lookup_host((host, port)).await {
            Ok(resolved) => {
                let addrs = resolved.collect::<Vec<_>>();
                anyhow::ensure!(!addrs.is_empty(), "no addresses resolved for {host}:{port}");
                for addr in &addrs {
                    ensure_public_ip(addr.ip(), host)?;
                }
                store_dns_addrs(cache_key, addrs.clone());
                return Ok(addrs);
            }
            Err(e) => {
                last_error = Some(e);
                if attempt + 1 < attempts {
                    let delay_ms = if primary { 25 * (attempt + 1) } else { 75 };
                    tokio::time::sleep(Duration::from_millis(delay_ms as u64)).await;
                }
            }
        }
    }

    if let Some(addrs) = cached_dns_addrs(&cache_key, DNS_STALE_TTL) {
        return Ok(addrs);
    }

    let error = last_error
        .map(|e| e.to_string())
        .unwrap_or_else(|| "unknown resolver error".to_string());
    Err(anyhow::anyhow!("resolving {host}:{port}: {error}"))
}

pub(crate) async fn connect_public_tcp_with_priority(
    host: &str,
    port: u16,
    primary: bool,
) -> Result<TcpStream> {
    let addrs = resolve_public_addrs_with_priority(host, port, primary).await?;
    TcpStream::connect(addrs.as_slice())
        .await
        .map_err(|e| anyhow::anyhow!("connect to {host}:{port} failed: {e}"))
}

fn dns_cache_key(host: &str, port: u16) -> String {
    format!("{}:{port}", host.trim_end_matches('.').to_ascii_lowercase())
}

fn cached_dns_addrs(key: &str, max_age: Duration) -> Option<Vec<SocketAddr>> {
    let cache = DNS_CACHE.lock().ok()?;
    let entry = cache.get(key)?;
    if entry.stored_at.elapsed() <= max_age {
        Some(entry.addrs.clone())
    } else {
        None
    }
}

fn store_dns_addrs(key: String, addrs: Vec<SocketAddr>) {
    if let Ok(mut cache) = DNS_CACHE.lock() {
        cache.insert(
            key,
            DnsCacheEntry {
                addrs,
                stored_at: Instant::now(),
            },
        );
    }
}

fn ensure_public_ip(ip: IpAddr, host: &str) -> Result<()> {
    anyhow::ensure!(
        !is_restricted_ip(ip),
        "destination {host} resolved to restricted address {ip}"
    );
    Ok(())
}

fn is_restricted_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_restricted_ipv4(ip),
        IpAddr::V6(ip) => is_restricted_ipv6(ip),
    }
}

fn is_restricted_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 224
}

fn is_restricted_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_restricted_ipv4(mapped);
    }

    let segments = ip.segments();
    ip == Ipv6Addr::LOCALHOST
        || ip == Ipv6Addr::UNSPECIFIED
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xff00) == 0xff00
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
}
