use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cache::{cache_key, Cache, CacheSource};
use crate::headers;
use crate::metrics::Metrics;
use crate::tunnel::connect_with_timeout;
use crate::log::{access, Access};
use crate::{log_debug, log_trace, log_warn};

const COPY_BUF_SIZE: usize = 32 * 1024;

/// キャッシュ対象となりうるレスポンスステータス (RFC 9111 6.1 heuristically cacheable)。
const CACHEABLE_STATUS: &[u16] = &[200, 203, 204, 300, 301, 308, 404, 405, 410, 414, 501];

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
    let mut host_header = None;
    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    let mut has_authorization = false;
    let mut req_cache_control = String::new();

    for line in &raw_headers {
        if let Some((k, v)) = line.split_once(':') {
            let k_lower = k.trim().to_ascii_lowercase();
            let v_trim = v.trim();
            log_trace!(Some(conn_id), "req header  {}: {}", k.trim(), v_trim);
            match k_lower.as_str() {
                "host" => host_header = Some(v_trim.to_string()),
                "content-length" => content_length = v_trim.parse().ok(),
                "transfer-encoding" if v_trim.eq_ignore_ascii_case("chunked") => is_chunked = true,
                "authorization" => has_authorization = true,
                "cache-control" | "pragma" => {
                    if !req_cache_control.is_empty() {
                        req_cache_control.push(',');
                    }
                    req_cache_control.push_str(&v_trim.to_ascii_lowercase());
                }
                _ => {}
            }
        }
    }

    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        log_warn!(Some(conn_id), "malformed request line: {:?}", request_line.trim());
        return Ok(());
    }
    let method = parts[0];
    let target = parts[1];
    let version = if parts.len() > 2 { parts[2] } else { "HTTP/1.1" };

    let (host_port, path) = parse_target(target, host_header.as_deref())?;

    let server_addr = if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{}:80", host_port)
    };

    let url = format!("http://{}{}", server_addr, path);
    let client_ip = peer_addr
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "-".to_string());
    log_debug!(Some(conn_id), "start {} {} {}", method, url, version);

    // ---- キャッシュ参照 ----
    let key = cache_key(method, &url);
    let req_allows_cache = cache.enabled()
        && method.eq_ignore_ascii_case("GET")
        && !has_authorization
        && !req_cache_control.contains("no-store")
        && !req_cache_control.contains("no-cache");

    if req_allows_cache {
        if let Some((entry, source)) = cache.get(&key, conn_id) {
            let age = entry.age();
            let written = write_cached_response(&mut client, &entry.bytes, source, age)?;
            metrics.inc_cache_hit();
            metrics.add_bytes(written as u64);
            let status = cached_status(&entry.bytes);
            access(
                conn_id,
                &Access {
                    client: &client_ip,
                    method,
                    target: &url,
                    version,
                    status: &status,
                    bytes: written as u64,
                    duration_ms: started.elapsed().as_secs_f64() * 1000.0,
                    cache: &format!(
                        "HIT({}) age={}s ttl_left={}s",
                        source.as_str(),
                        age,
                        entry.expires_at.saturating_sub(crate::cache::now_epoch())
                    ),
                },
            );
            return Ok(());
        }
        metrics.inc_cache_miss();
    } else if cache.enabled() {
        log_debug!(
            Some(conn_id),
            "cache BYPASS (method={} auth={} cc='{}')",
            method,
            has_authorization,
            req_cache_control
        );
    }

    // ---- オリジンへ転送 ----
    log_debug!(Some(conn_id), "connecting to origin {}", server_addr);
    let mut server = match connect_with_timeout(&server_addr, timeout) {
        Ok(s) => s,
        Err(e) => {
            log_warn!(Some(conn_id), "502 Bad Gateway: connect {} failed: {}", server_addr, e);
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
    server.write_all(b"\r\n")?;

    let mut request_body_bytes = 0u64;
    if let Some(len) = content_length {
        if len > 0 {
            let mut body_reader = (&mut reader).take(len as u64);
            request_body_bytes = io::copy(&mut body_reader, &mut server)?;
        }
    } else if is_chunked {
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
        log_debug!(Some(conn_id), "forwarded request body {}B", request_body_bytes);
    }

    // ---- レスポンス受信・転送・キャッシュ格納 ----
    let mut server_reader = BufReader::new(server);
    let (head, status, resp_headers) = match read_response_head(&mut server_reader) {
        Ok(v) => v,
        Err(e) => {
            log_warn!(Some(conn_id), "failed to read origin response: {}", e);
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
            return Err(e);
        }
    };

    for (k, v) in &resp_headers {
        log_trace!(Some(conn_id), "res header  {}: {}", k, v);
    }

    let ttl = if req_allows_cache {
        response_ttl(status, &resp_headers, cache.config().default_ttl)
    } else {
        None
    };
    let max_object = cache.config().max_object_size as usize;
    let mut buffer: Option<Vec<u8>> = ttl.map(|_| head.clone());

    client.write_all(&head)?;
    let mut body_bytes = 0u64;
    let mut chunk = vec![0u8; COPY_BUF_SIZE];
    loop {
        let n = match server_reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(ref e) if is_eof_like(e) => {
                log_debug!(Some(conn_id), "origin read ended: {}", e);
                buffer = None;
                break;
            }
            Err(e) => return Err(e),
        };
        client.write_all(&chunk[..n])?;
        body_bytes += n as u64;
        if let Some(buf) = buffer.as_mut() {
            if buf.len() + n > max_object {
                log_debug!(
                    Some(conn_id),
                    "cache SKIP: response exceeds max object size ({}B)",
                    max_object
                );
                buffer = None;
            } else {
                buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
    client.flush()?;

    let total = head.len() as u64 + body_bytes;
    metrics.add_bytes(total + request_body_bytes);

    let cache_state = match (ttl, buffer) {
        (Some(ttl), Some(buf)) => {
            cache.put(&key, &url, buf, ttl, conn_id);
            format!("MISS stored ttl={}s", ttl.as_secs())
        }
        _ if !req_allows_cache => "BYPASS".to_string(),
        _ => "MISS".to_string(),
    };

    access(
        conn_id,
        &Access {
            client: &client_ip,
            method,
            target: &url,
            version,
            status: &status.to_string(),
            bytes: total,
            duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            cache: &cache_state,
        },
    );

    Ok(())
}

/// キャッシュ済みバイト列のステータス行からステータスコードを取り出す。
fn cached_status(bytes: &[u8]) -> String {
    let line_end = find_subslice(bytes, b"\r\n").unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..line_end])
        .split_whitespace()
        .nth(1)
        .unwrap_or("-")
        .to_string()
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

/// キャッシュ済みレスポンスに `X-Cache` / `Age` を付与してクライアントへ書き出す。
pub fn write_cached_response(
    client: &mut impl Write,
    bytes: &[u8],
    source: CacheSource,
    age: u64,
) -> io::Result<usize> {
    let split = find_subslice(bytes, b"\r\n").map(|p| p + 2).unwrap_or(0);
    let extra = format!("X-Cache: HIT from sorahost-http-proxy ({})\r\nAge: {}\r\n", source.as_str(), age);
    client.write_all(&bytes[..split])?;
    client.write_all(extra.as_bytes())?;
    client.write_all(&bytes[split..])?;
    client.flush()?;
    Ok(bytes.len() + extra.len())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// ステータス行とヘッダー部を読み切り、(生バイト列, ステータスコード, ヘッダー) を返す。
pub fn read_response_head<R: BufRead>(
    reader: &mut R,
) -> io::Result<(Vec<u8>, u16, Vec<(String, String)>)> {
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

/// レスポンスのキャッシュ可否と TTL を判定する (RFC 9111 の簡易実装)。
pub fn response_ttl(
    status: u16,
    headers: &[(String, String)],
    default_ttl: Duration,
) -> Option<Duration> {
    if !CACHEABLE_STATUS.contains(&status) {
        return None;
    }

    let mut cache_control = String::new();
    for (k, v) in headers {
        match k.as_str() {
            "set-cookie" => return None,
            "vary" if v.trim() == "*" => return None,
            "cache-control" | "pragma" => {
                if !cache_control.is_empty() {
                    cache_control.push(',');
                }
                cache_control.push_str(&v.to_ascii_lowercase());
            }
            _ => {}
        }
    }

    if cache_control.contains("no-store")
        || cache_control.contains("no-cache")
        || cache_control.contains("private")
    {
        return None;
    }

    // s-maxage は共有キャッシュで max-age より優先される
    for directive in ["s-maxage", "max-age"] {
        if let Some(secs) = directive_value(&cache_control, directive) {
            return if secs == 0 {
                None
            } else {
                Some(Duration::from_secs(secs))
            };
        }
    }

    Some(default_ttl)
}

fn directive_value(cache_control: &str, name: &str) -> Option<u64> {
    for part in cache_control.split(',') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name) {
            if let Some(v) = rest.trim_start().strip_prefix('=') {
                return v.trim().trim_matches('"').parse::<u64>().ok();
            }
        }
    }
    None
}

pub fn parse_target<'a>(target: &'a str, host_header: Option<&'a str>) -> io::Result<(&'a str, &'a str)> {
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
            Err(io::Error::new(io::ErrorKind::InvalidInput, "Missing host in HTTP request"))
        }
    } else {
        Ok((target, "/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_parse_target_absolute_url() {
        let (host, path) = parse_target("http://example.com/test?a=1", None).unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(path, "/test?a=1");

        let (host2, path2) = parse_target("http://example.com:8080", None).unwrap();
        assert_eq!(host2, "example.com:8080");
        assert_eq!(path2, "/");
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
        assert_eq!(headers[1], ("cache-control".to_string(), "max-age=60".to_string()));

        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"hello");
    }

    #[test]
    fn test_response_ttl_default() {
        let ttl = response_ttl(200, &hdrs(&[("content-type", "text/html")]), Duration::from_secs(300));
        assert_eq!(ttl, Some(Duration::from_secs(300)));
    }

    #[test]
    fn test_response_ttl_max_age_and_s_maxage() {
        assert_eq!(
            response_ttl(200, &hdrs(&[("cache-control", "public, max-age=120")]), Duration::from_secs(300)),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            response_ttl(
                200,
                &hdrs(&[("cache-control", "max-age=120, s-maxage=600")]),
                Duration::from_secs(300)
            ),
            Some(Duration::from_secs(600))
        );
    }

    #[test]
    fn test_response_not_cacheable() {
        let d = Duration::from_secs(300);
        assert_eq!(response_ttl(200, &hdrs(&[("cache-control", "no-store")]), d), None);
        assert_eq!(response_ttl(200, &hdrs(&[("cache-control", "private")]), d), None);
        assert_eq!(response_ttl(200, &hdrs(&[("cache-control", "max-age=0")]), d), None);
        assert_eq!(response_ttl(200, &hdrs(&[("set-cookie", "a=b")]), d), None);
        assert_eq!(response_ttl(200, &hdrs(&[("vary", "*")]), d), None);
        assert_eq!(response_ttl(500, &hdrs(&[]), d), None);
        assert_eq!(response_ttl(302, &hdrs(&[]), d), None);
    }

    #[test]
    fn test_cached_status() {
        assert_eq!(cached_status(b"HTTP/1.1 301 Moved\r\nA: b\r\n\r\n"), "301");
        assert_eq!(cached_status(b"garbage"), "-");
    }

    #[test]
    fn test_write_cached_response_injects_headers() {
        let cached = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        let mut out = Vec::new();
        let n = write_cached_response(&mut out, cached, CacheSource::Disk, 42).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\nX-Cache: HIT from sorahost-http-proxy (disk)\r\nAge: 42\r\n"));
        assert!(text.ends_with("\r\n\r\nhi"));
        assert_eq!(n, text.len());
    }
}
