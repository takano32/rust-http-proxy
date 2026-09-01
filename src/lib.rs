pub mod acl;
pub mod body;
pub mod cache;
pub mod config;
pub mod envfile;
pub mod freshness;
pub mod headers;
pub mod http;
pub mod httpdate;
pub mod log;
pub mod metrics;
pub mod net;
pub mod origin;
pub mod pool;
pub mod signal;
pub mod sysinfo;
pub mod tls;
pub mod tunnel;

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Instant;

use cache::Cache;
use config::Config;
use metrics::Metrics;
use pool::Pool;
use tls::TlsClient;

/// オリジンへ向かう側の共有状態 (接続プールと TLS クライアント)。
pub struct Upstream {
    pub pool: Pool,
    pub tls: Option<TlsClient>,
}

const FORBIDDEN_RESPONSE: &[u8] = b"HTTP/1.1 403 Forbidden\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Length: 13\r\n\
Connection: close\r\n\
\r\n\
403 Forbidden";

/// 1 つのクライアント接続で処理する最大要求数 (keep-alive)。
const MAX_REQUESTS_PER_CONNECTION: usize = 1000;
/// 要求行・ヘッダー行 1 本の最大長と、ヘッダー行数の上限 (超えたら 414 / 431)。
const MAX_LINE: usize = 64 * 1024;
const MAX_HEADER_LINES: usize = 256;

/// 長さ制限付きで 1 行読む。制限を超えたら `Ok(None)`。
fn read_limited_line(
    reader: &mut BufReader<TcpStream>,
    line: &mut String,
) -> io::Result<Option<usize>> {
    let n = reader.by_ref().take(MAX_LINE as u64).read_line(line)?;
    if n == MAX_LINE && !line.ends_with('\n') {
        return Ok(None);
    }
    Ok(Some(n))
}

fn reject(client: &mut TcpStream, status: u16, reason: &str) -> io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        status, reason
    );
    client.write_all(resp.as_bytes())?;
    client.flush()
}

/// 1 つのクライアント接続を、keep-alive なら複数の要求にわたって処理する。
pub fn handle_client(
    mut client: TcpStream,
    config: Arc<Config>,
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    upstream: Arc<Upstream>,
    conn_id: usize,
) -> io::Result<()> {
    let started = Instant::now();
    metrics.inc_active_conn();

    struct ConnGuard(Arc<Metrics>, usize, Instant);
    impl Drop for ConnGuard {
        fn drop(&mut self) {
            self.0.dec_active_conn();
            log_debug!(
                Some(self.1),
                "connection closed after {:.1}ms (active={})",
                self.2.elapsed().as_secs_f64() * 1000.0,
                self.0
                    .active_connections
                    .load(std::sync::atomic::Ordering::Relaxed)
            );
        }
    }
    let _guard = ConnGuard(Arc::clone(&metrics), conn_id, started);

    let peer_addr = client.peer_addr().ok().map(net::canonical_addr);
    log_debug!(
        Some(conn_id),
        "accepted connection from {}",
        peer_addr
            .map(|a| a.to_string())
            .unwrap_or_else(|| "<unknown>".to_string())
    );
    client.set_write_timeout(Some(config.timeout))?;
    let mut reader = BufReader::new(client.try_clone()?);
    let mut served = 0usize;

    loop {
        // 最初の要求は通常のタイムアウト、2 回目以降は keep-alive のアイドル時間で待つ
        let wait = if served == 0 {
            config.timeout
        } else {
            config.keepalive
        };
        client.set_read_timeout(Some(wait))?;
        let mut request_line = String::new();
        match read_limited_line(&mut reader, &mut request_line) {
            Ok(None) => {
                log_warn!(
                    Some(conn_id),
                    "414 URI Too Long (request line over {} bytes)",
                    MAX_LINE
                );
                return reject(&mut client, 414, "URI Too Long");
            }
            Ok(Some(0)) => {
                log_debug!(Some(conn_id), "client closed ({} requests served)", served);
                return Ok(());
            }
            Ok(Some(_)) => {}
            Err(e)
                if served > 0
                    && matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::ConnectionReset
                    ) =>
            {
                log_debug!(Some(conn_id), "keep-alive idle timeout: {}", e);
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        // 要求の前の空行は読み飛ばす (RFC 9112 §2.2)
        if request_line.trim().is_empty() {
            continue;
        }
        client.set_read_timeout(Some(config.timeout))?;
        metrics.inc_requests();
        log_trace!(Some(conn_id), "request line: {}", request_line.trim_end());

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            log_warn!(
                Some(conn_id),
                "malformed request line: {:?}",
                request_line.trim()
            );
            return Ok(());
        }
        let method = parts[0].to_string();
        let target = parts[1].to_string();

        // Health check endpoint handling
        if (target == "/healthz" || target == "/status") && method.eq_ignore_ascii_case("GET") {
            let json_body = metrics.to_json_with_cache(Some(&cache));
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                Content-Type: application/json\r\n\
                Content-Length: {}\r\n\
                Connection: close\r\n\
                \r\n\
                {}",
                json_body.len(),
                json_body
            );
            client.write_all(response.as_bytes())?;
            client.flush()?;
            log_info!(Some(conn_id), "GET {} -> 200 (status endpoint)", target);
            return Ok(());
        }

        let mut raw_headers = Vec::new();
        let mut host_header = None;
        loop {
            let mut line = String::new();
            match read_limited_line(&mut reader, &mut line)? {
                None => {
                    log_warn!(Some(conn_id), "431 Request Header Fields Too Large");
                    return reject(&mut client, 431, "Request Header Fields Too Large");
                }
                Some(0) => break,
                Some(_) => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if raw_headers.len() >= MAX_HEADER_LINES {
                log_warn!(
                    Some(conn_id),
                    "431 Request Header Fields Too Large (too many lines)"
                );
                return reject(&mut client, 431, "Request Header Fields Too Large");
            }
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("host")
            {
                host_header = Some(v.trim().to_string());
            }
            raw_headers.push(line);
        }

        // ACL / Host Check
        let is_connect = method.eq_ignore_ascii_case("CONNECT");
        let target_host = if is_connect {
            target.clone()
        } else {
            match http::parse_origin(&target, host_header.as_deref()) {
                Ok(o) => o.host_port,
                Err(e) => {
                    log_warn!(Some(conn_id), "400 Bad Request: {}", e);
                    let _ = client.write_all(
                        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return Ok(());
                }
            }
        };
        if !config.acl.is_allowed(&target_host) {
            log_warn!(
                Some(conn_id),
                "403 Forbidden (ACL blocked host: {})",
                target_host
            );
            client.write_all(FORBIDDEN_RESPONSE)?;
            client.flush()?;
            return Ok(());
        }

        if is_connect {
            // 先読みしてしまったバイト (TLS ClientHello など) はトンネルへ渡す
            let prefix = reader.buffer().to_vec();
            return tunnel::handle_connect(
                client,
                &target,
                &prefix,
                config.timeout,
                conn_id,
                Arc::clone(&metrics),
            );
        }

        let shared = http::Shared {
            timeout: config.timeout,
            keepalive: config.keepalive,
            conn_id,
            metrics: &metrics,
            cache: &cache,
            pool: &upstream.pool,
            tls: upstream.tls.as_ref(),
        };
        let keep = http::handle_http_with_headers(
            &mut client,
            peer_addr,
            &request_line,
            &raw_headers,
            &mut reader,
            &shared,
        )?;
        served += 1;
        if !keep || served >= MAX_REQUESTS_PER_CONNECTION {
            return Ok(());
        }
    }
}
