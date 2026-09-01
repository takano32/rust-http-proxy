//! HTTP (平文) リクエストの転送とキャッシュの適用。
//!
//! 流れ: リクエスト解析 → キャッシュ参照 (新鮮なら配信、期限切れでもバリデータ付きなら
//! 再検証用に保持) → オリジンへ転送 (必要なら条件付き) → 応答を配信しつつストリーミングで
//! 保存。304 なら保存済みの表現を延命して配信し、オリジン障害時は stale を配信する。

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cache::{Cache, CacheSource, CachedResponse, cache_key_variant, now_epoch};
use crate::freshness::{self, CachedHead};
use crate::headers;
use crate::log::{Access, access};
use crate::metrics::Metrics;
use crate::net;
use crate::tunnel::connect_with_timeout;
use crate::{log_debug, log_trace, log_warn};

const COPY_BUF_SIZE: usize = 64 * 1024;
/// オリジンがこのステータスを返したら stale を配信する (RFC 5861 stale-if-error 相当)。
const STALE_ON_STATUS: &[u16] = &[500, 502, 503, 504];

/// (生バイト列, ステータスコード, 小文字化したヘッダー名と値の組)
pub type ResponseHead = (Vec<u8>, u16, Vec<(String, String)>);

#[allow(clippy::too_many_arguments)]
pub fn handle_http(
    client: TcpStream,
    peer_addr: Option<SocketAddr>,
    request_line: String,
    mut reader: BufReader<TcpStream>,
    timeout: Duration,
    conn_id: usize,
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
) -> io::Result<()> {
    let mut raw_headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        raw_headers.push(line);
    }
    handle_http_with_headers(
        client,
        peer_addr,
        request_line,
        raw_headers,
        reader,
        timeout,
        conn_id,
        metrics,
        cache,
    )
}

/// リクエストヘッダーのうち転送・キャッシュ判断に使うもの。
#[derive(Default)]
struct RequestHeaders {
    host: Option<String>,
    content_length: Option<usize>,
    chunked: bool,
    authorization: bool,
    cache_control: String,
    if_none_match: Option<String>,
    if_modified_since: Option<String>,
    range: bool,
    accept_encoding: Option<String>,
}

fn parse_request_headers(raw_headers: &[String], conn_id: usize) -> RequestHeaders {
    let mut h = RequestHeaders::default();
    for line in raw_headers {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k_lower = k.trim().to_ascii_lowercase();
        let v_trim = v.trim();
        log_trace!(Some(conn_id), "req header  {}: {}", k.trim(), v_trim);
        match k_lower.as_str() {
            "host" => h.host = Some(v_trim.to_string()),
            "content-length" => h.content_length = v_trim.parse().ok(),
            "transfer-encoding" if v_trim.eq_ignore_ascii_case("chunked") => h.chunked = true,
            "authorization" => h.authorization = true,
            "cache-control" | "pragma" => {
                if !h.cache_control.is_empty() {
                    h.cache_control.push(',');
                }
                h.cache_control.push_str(&v_trim.to_ascii_lowercase());
            }
            "if-none-match" => h.if_none_match = Some(v_trim.to_string()),
            "if-modified-since" => h.if_modified_since = Some(v_trim.to_string()),
            "range" => h.range = true,
            "accept-encoding" => h.accept_encoding = Some(v_trim.to_string()),
            _ => {}
        }
    }
    h
}

/// アクセスログと配信に必要なリクエストの文脈。
struct Ctx<'a> {
    client_ip: &'a str,
    method: &'a str,
    url: &'a str,
    version: &'a str,
    started: Instant,
    conn_id: usize,
    metrics: &'a Metrics,
    req: &'a RequestHeaders,
}

impl Ctx<'_> {
    fn log(&self, status: &str, bytes: u64, cache: &str) {
        access(
            self.conn_id,
            &Access {
                client: self.client_ip,
                method: self.method,
                target: self.url,
                version: self.version,
                status,
                bytes,
                duration_ms: self.started.elapsed().as_secs_f64() * 1000.0,
                cache,
            },
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub fn handle_http_with_headers(
    mut client: TcpStream,
    peer_addr: Option<SocketAddr>,
    request_line: String,
    raw_headers: Vec<String>,
    mut reader: BufReader<TcpStream>,
    timeout: Duration,
    conn_id: usize,
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
) -> io::Result<()> {
    let started = Instant::now();
    let req = parse_request_headers(&raw_headers, conn_id);

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        log_warn!(
            Some(conn_id),
            "malformed request line: {:?}",
            request_line.trim()
        );
        return Ok(());
    }
    let method = parts[0];
    let target = parts[1];
    let version = if parts.len() > 2 {
        parts[2]
    } else {
        "HTTP/1.1"
    };

    let (host_port, path) = parse_target(target, req.host.as_deref())?;
    let server_addr = net::with_default_port(host_port, 80);
    let url = format!("http://{}{}", server_addr, path);
    let client_ip = peer_addr
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "-".to_string());
    log_debug!(Some(conn_id), "start {} {} {}", method, url, version);
    let ctx = Ctx {
        client_ip: &client_ip,
        method,
        url: &url,
        version,
        started,
        conn_id,
        metrics: &metrics,
        req: &req,
    };

    // ---- キャッシュ参照 ----
    let cfg = cache.config();
    let variant = freshness::accept_encoding_variant(req.accept_encoding.as_deref());
    let key = cache_key_variant(method, &url, &variant);
    let client_no_store = freshness::has_directive(&req.cache_control, "no-store");
    let force_revalidate = freshness::has_directive(&req.cache_control, "no-cache")
        || freshness::directive_value(&req.cache_control, "max-age") == Some(0);
    let req_allows_cache = cache.enabled()
        && method.eq_ignore_ascii_case("GET")
        && !req.authorization
        && !client_no_store
        && !req.range;
    let client_conditional = req.if_none_match.is_some() || req.if_modified_since.is_some();
    let now = now_epoch();

    let mut stale: Option<(CachedResponse, CacheSource)> = None;
    if req_allows_cache {
        if let Some((entry, source)) = cache.get(key, conn_id) {
            if entry.is_fresh(now) && !force_revalidate {
                let ttl_left = entry.ttl_left(now);
                return serve_cached(&mut client, entry, source, "HIT", ttl_left, &ctx);
            }
            // クライアント自身の条件付き要求は、そのままオリジンに判断させる
            if !client_conditional {
                stale = Some((entry, source));
            }
        }
    } else if cache.enabled() {
        log_debug!(
            Some(conn_id),
            "cache BYPASS (method={} auth={} range={} cc='{}')",
            method,
            req.authorization,
            req.range,
            req.cache_control
        );
    }

    // ---- オリジンへ転送 ----
    log_debug!(Some(conn_id), "connecting to origin {}", server_addr);
    let mut server = match connect_with_timeout(&server_addr, timeout) {
        Ok(s) => s,
        Err(e) => {
            log_warn!(
                Some(conn_id),
                "502 Bad Gateway: connect {} failed: {}",
                server_addr,
                e
            );
            if let Some((entry, source)) = stale.take()
                && can_serve_stale(&entry)
            {
                return serve_cached(&mut client, entry, source, "STALE", 0, &ctx);
            }
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
            return Err(e);
        }
    };

    server.set_read_timeout(Some(timeout))?;
    server.set_write_timeout(Some(timeout))?;

    let forward_request_line = format!("{} {} {}\r\n", method, path, version);
    server.write_all(forward_request_line.as_bytes())?;

    let sanitized_headers = headers::sanitize_and_inject_headers(&raw_headers, peer_addr);
    for h in &sanitized_headers {
        log_trace!(Some(conn_id), "fwd header  {}", h.trim_end());
        server.write_all(h.as_bytes())?;
    }
    if let Some((entry, _)) = &stale {
        // 保存済みの表現で再検証する (304 なら本文転送なしで延命)
        for line in freshness::conditional_headers(&freshness::parse_cached_head(&entry.head)) {
            log_trace!(Some(conn_id), "fwd header  {} (revalidation)", line);
            server.write_all(line.as_bytes())?;
            server.write_all(b"\r\n")?;
        }
    }
    server.write_all(b"\r\n")?;

    let mut request_body_bytes = 0u64;
    if let Some(len) = req.content_length {
        if len > 0 {
            let mut body_reader = (&mut reader).take(len as u64);
            request_body_bytes = io::copy(&mut body_reader, &mut server)?;
        }
    } else if req.chunked {
        loop {
            let mut chunk_header = String::new();
            reader.read_line(&mut chunk_header)?;
            server.write_all(chunk_header.as_bytes())?;
            let hex_str = chunk_header.trim().split(';').next().unwrap_or("");
            let chunk_size = usize::from_str_radix(hex_str, 16).unwrap_or(0);
            if chunk_size == 0 {
                let mut trail = String::new();
                reader.read_line(&mut trail)?;
                server.write_all(trail.as_bytes())?;
                break;
            }
            let mut chunk_data = vec![0u8; chunk_size + 2];
            reader.read_exact(&mut chunk_data)?;
            server.write_all(&chunk_data)?;
            request_body_bytes += chunk_size as u64;
        }
    }
    server.flush()?;
    if request_body_bytes > 0 {
        log_debug!(
            Some(conn_id),
            "forwarded request body {}B",
            request_body_bytes
        );
    }

    // ---- レスポンス受信 ----
    let mut server_reader = BufReader::with_capacity(COPY_BUF_SIZE, server);
    let (head, status, resp_headers) = match read_response_head(&mut server_reader) {
        Ok(v) => v,
        Err(e) => {
            log_warn!(Some(conn_id), "failed to read origin response: {}", e);
            if let Some((entry, source)) = stale.take()
                && can_serve_stale(&entry)
            {
                return serve_cached(&mut client, entry, source, "STALE", 0, &ctx);
            }
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
            return Err(e);
        }
    };
    for (k, v) in &resp_headers {
        log_trace!(Some(conn_id), "res header  {}: {}", k, v);
    }

    // 304: 保存済みの表現がまだ有効。延命して配信する
    if status == 304
        && let Some((entry, source)) = stale.take()
    {
        let cached_head = freshness::parse_cached_head(&entry.head);
        let ttl = freshness::revalidated_ttl(&resp_headers, &cached_head, cfg, now);
        cache.refresh(key, ttl, conn_id);
        return serve_cached(
            &mut client,
            entry,
            source,
            "REVALIDATED",
            ttl.as_secs(),
            &ctx,
        );
    }
    // オリジン障害: stale を配信 (must-revalidate でなければ)
    if STALE_ON_STATUS.contains(&status)
        && let Some((entry, source)) = stale.take()
        && can_serve_stale(&entry)
    {
        log_debug!(
            Some(conn_id),
            "origin returned {}: serving stale entry",
            status
        );
        return serve_cached(&mut client, entry, source, "STALE", 0, &ctx);
    }
    if req_allows_cache {
        metrics.inc_cache_miss();
    }

    // ---- 配信しつつ保存 ----
    let policy = if req_allows_cache {
        freshness::response_policy(status, &resp_headers, cfg, now)
    } else {
        None
    };
    if policy.is_none() && stale.is_some() {
        // 新しい表現は保存できないので、古い表現も捨てる
        cache.remove(key);
    }
    let content_length = resp_headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<u64>().ok());
    let mut sink = policy.map(|p| {
        cache.begin_store(
            key,
            &url,
            p.ttl,
            p.validators,
            content_length.map(|l| l.saturating_add(head.len() as u64)),
            conn_id,
        )
    });
    if let Some(s) = sink.as_mut() {
        s.write(&head);
    }

    client.write_all(&head)?;
    let mut body_bytes = 0u64;
    let mut chunk = vec![0u8; COPY_BUF_SIZE];
    let mut truncated = false;
    loop {
        let n = match server_reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(ref e) if is_eof_like(e) => {
                log_debug!(Some(conn_id), "origin read ended: {}", e);
                truncated = true;
                break;
            }
            Err(e) => {
                if let Some(s) = sink.take() {
                    s.abort();
                }
                return Err(e);
            }
        };
        client.write_all(&chunk[..n])?;
        body_bytes += n as u64;
        if let Some(s) = sink.as_mut() {
            s.write(&chunk[..n]);
        }
    }
    client.flush()?;
    if truncated && let Some(s) = sink.take() {
        s.abort();
    }

    let total = head.len() as u64 + body_bytes;
    metrics.add_bytes(total + request_body_bytes);

    let cache_state = match (policy, sink) {
        (Some(p), Some(s)) => {
            let out = s.finish();
            if out.memory || out.disk {
                format!("MISS stored ttl={}s", p.ttl.as_secs())
            } else {
                "MISS".to_string()
            }
        }
        _ if !req_allows_cache => "BYPASS".to_string(),
        _ => "MISS".to_string(),
    };
    ctx.log(&status.to_string(), total, &cache_state);
    Ok(())
}

/// stale のまま配信してよいか (`must-revalidate` / `proxy-revalidate` なら不可)。
fn can_serve_stale(entry: &CachedResponse) -> bool {
    !freshness::parse_cached_head(&entry.head).must_revalidate
}

/// キャッシュ済みレスポンスを配信する。クライアントの条件付き要求に合致すれば 304 を返す。
fn serve_cached(
    client: &mut TcpStream,
    entry: CachedResponse,
    source: CacheSource,
    label: &str,
    ttl_left: u64,
    ctx: &Ctx<'_>,
) -> io::Result<()> {
    let age = entry.age();
    let cached_head = freshness::parse_cached_head(&entry.head);
    let status = cached_head.status.to_string();
    let conditional = ctx.req.if_none_match.is_some() || ctx.req.if_modified_since.is_some();
    if conditional
        && freshness::client_not_modified(
            &cached_head,
            ctx.req.if_none_match.as_deref(),
            ctx.req.if_modified_since.as_deref(),
        )
    {
        let written = write_not_modified(client, &cached_head, label, source, age)?;
        ctx.metrics.inc_cache_hit();
        ctx.metrics.add_bytes(written);
        ctx.log(
            "304",
            written,
            &format!("{}({},304) age={}s", label, source.as_str(), age),
        );
        return Ok(());
    }
    let written = write_cached_response(client, entry, label, source, age)?;
    ctx.metrics.inc_cache_hit();
    ctx.metrics.add_bytes(written);
    ctx.log(
        &status,
        written,
        &format!(
            "{}({}) age={}s ttl_left={}s",
            label,
            source.as_str(),
            age,
            ttl_left
        ),
    );
    Ok(())
}

fn is_eof_like(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
    )
}

fn x_cache_line(label: &str, source: CacheSource, age: u64) -> String {
    format!(
        "X-Cache: {} from sorahost-http-proxy ({})\r\nAge: {}\r\n",
        label,
        source.as_str(),
        age
    )
}

/// キャッシュ済みレスポンスに `X-Cache` / `Age` を付与してクライアントへ書き出す。
pub fn write_cached_response(
    client: &mut impl Write,
    entry: CachedResponse,
    label: &str,
    source: CacheSource,
    age: u64,
) -> io::Result<u64> {
    let head = entry.head.clone();
    let split = find_subslice(&head, b"\r\n").map(|p| p + 2).unwrap_or(0);
    let extra = x_cache_line(label, source, age);
    client.write_all(&head[..split])?;
    client.write_all(extra.as_bytes())?;
    client.write_all(&head[split..])?;
    let body = io::copy(&mut entry.into_body_reader(), client)?;
    client.flush()?;
    Ok(head.len() as u64 + extra.len() as u64 + body)
}

/// クライアントの条件付き要求に対する 304 応答。
fn write_not_modified(
    client: &mut impl Write,
    head: &CachedHead,
    label: &str,
    source: CacheSource,
    age: u64,
) -> io::Result<u64> {
    let mut out = String::from("HTTP/1.1 304 Not Modified\r\n");
    out.push_str(&x_cache_line(label, source, age));
    for line in freshness::not_modified_headers(head) {
        out.push_str(&line);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    client.write_all(out.as_bytes())?;
    client.flush()?;
    Ok(out.len() as u64)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// ステータス行とヘッダー部を読み切り、[`ResponseHead`] を返す。
pub fn read_response_head<R: BufRead>(reader: &mut R) -> io::Result<ResponseHead> {
    let mut head = Vec::with_capacity(1024);
    let mut status_line = String::new();
    if reader.read_line(&mut status_line)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "origin closed before sending a status line",
        ));
    }
    head.extend_from_slice(status_line.as_bytes());

    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        head.extend_from_slice(line.as_bytes());
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    Ok((head, status, headers))
}

pub fn parse_target<'a>(
    target: &'a str,
    host_header: Option<&'a str>,
) -> io::Result<(&'a str, &'a str)> {
    if let Some(stripped) = target.strip_prefix("http://") {
        if let Some(pos) = stripped.find('/') {
            Ok((&stripped[..pos], &stripped[pos..]))
        } else {
            Ok((stripped, "/"))
        }
    } else if target.starts_with('/') {
        if let Some(h) = host_header {
            Ok((h, target))
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Missing host in HTTP request",
            ))
        }
    } else {
        Ok((target, "/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Body, Meta};

    #[test]
    fn test_parse_target_absolute_url() {
        let (host, path) = parse_target("http://example.com/test?a=1", None).unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(path, "/test?a=1");

        let (host2, path2) = parse_target("http://example.com:8080", None).unwrap();
        assert_eq!(host2, "example.com:8080");
        assert_eq!(path2, "/");

        let (host3, path3) = parse_target("http://[2001:db8::1]:8080/v6", None).unwrap();
        assert_eq!(host3, "[2001:db8::1]:8080");
        assert_eq!(path3, "/v6");
        assert_eq!(net::with_default_port(host3, 80), "[2001:db8::1]:8080");
        let (host4, _) = parse_target("http://[::1]/", None).unwrap();
        assert_eq!(net::with_default_port(host4, 80), "[::1]:80");
    }

    #[test]
    fn test_parse_target_relative_url() {
        let (host, path) = parse_target("/index.html", Some("example.com")).unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(path, "/index.html");

        assert!(parse_target("/index.html", None).is_err());
    }

    #[test]
    fn test_read_response_head() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nCache-Control: max-age=60\r\n\r\nhello";
        let mut reader = BufReader::new(&raw[..]);
        let (head, status, headers) = read_response_head(&mut reader).unwrap();
        assert_eq!(status, 200);
        assert!(head.ends_with(b"\r\n\r\n"));
        assert_eq!(headers.len(), 2);
        assert_eq!(
            headers[1],
            ("cache-control".to_string(), "max-age=60".to_string())
        );

        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"hello");
    }

    fn cached(wire: &[u8]) -> CachedResponse {
        let offset = wire.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        CachedResponse {
            head: wire[..offset].to_vec(),
            size: wire.len() as u64,
            body: Body::Memory {
                data: Arc::new(wire.to_vec()),
                offset,
            },
            meta: Meta {
                stored_at: 0,
                expires_at: u64::MAX,
                validators: false,
            },
        }
    }

    #[test]
    fn test_write_cached_response_injects_headers() {
        let entry = cached(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi");
        let mut out = Vec::new();
        let n = write_cached_response(&mut out, entry, "HIT", CacheSource::Disk, 42).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with(
            "HTTP/1.1 200 OK\r\nX-Cache: HIT from sorahost-http-proxy (disk)\r\nAge: 42\r\n"
        ));
        assert!(text.ends_with("\r\n\r\nhi"));
        assert_eq!(n, text.len() as u64);
    }

    #[test]
    fn test_write_not_modified_keeps_validators_only() {
        let head = freshness::parse_cached_head(
            b"HTTP/1.1 200 OK\r\nETag: \"x\"\r\nContent-Length: 2\r\nContent-Type: text/plain\r\n\r\n",
        );
        let mut out = Vec::new();
        write_not_modified(&mut out, &head, "HIT", CacheSource::Memory, 3).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 304 Not Modified\r\nX-Cache: HIT from sorahost-http-proxy (memory)\r\nAge: 3\r\n"));
        assert!(text.contains("ETag: \"x\"\r\n"));
        assert!(!text.contains("Content-Length"));
        assert!(text.ends_with("\r\n\r\n"));
    }
}
