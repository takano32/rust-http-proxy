pub mod acl;
pub mod cache;
pub mod config;
pub mod headers;
pub mod http;
pub mod log;
pub mod metrics;
pub mod tunnel;

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Instant;

use cache::Cache;
use config::Config;
use metrics::Metrics;

const FORBIDDEN_RESPONSE: &[u8] = b"HTTP/1.1 403 Forbidden\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Length: 13\r\n\
\r\n\
403 Forbidden";

pub fn handle_client(
    mut client: TcpStream,
    config: Arc<Config>,
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    conn_id: usize,
) -> io::Result<()> {
    let started = Instant::now();
    metrics.inc_active_conn();
    metrics.inc_requests();

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

    let peer_addr = client.peer_addr().ok();
    log_debug!(
        Some(conn_id),
        "accepted connection from {}",
        peer_addr
            .map(|a| a.to_string())
            .unwrap_or_else(|| "<unknown>".to_string())
    );

    client.set_read_timeout(Some(config.timeout))?;
    client.set_write_timeout(Some(config.timeout))?;

    let mut reader = BufReader::new(client.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        log_debug!(Some(conn_id), "client closed before sending a request line");
        return Ok(());
    }
    log_trace!(Some(conn_id), "request line: {}", request_line.trim_end());

    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        log_warn!(Some(conn_id), "malformed request line: {:?}", request_line.trim());
        return Ok(());
    }

    let method = parts[0];
    let target = parts[1];

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
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }

        if let Some((k, v)) = line.split_once(':') {
            let k_lower = k.trim().to_ascii_lowercase();
            if k_lower == "host" {
                host_header = Some(v.trim().to_string());
            }
        }
        raw_headers.push(line);
    }

    // ACL / Host Check
    let target_host = if method.eq_ignore_ascii_case("CONNECT") {
        target
    } else {
        match http::parse_target(target, host_header.as_deref()) {
            Ok((h, _)) => h,
            Err(e) => {
                log_warn!(Some(conn_id), "400 Bad Request: {}", e);
                let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
                return Ok(());
            }
        }
    };

    if !config.acl.is_allowed(target_host) {
        log_warn!(Some(conn_id), "403 Forbidden (ACL blocked host: {})", target_host);
        client.write_all(FORBIDDEN_RESPONSE)?;
        client.flush()?;
        return Ok(());
    }

    // Forward or Tunnel
    if method.eq_ignore_ascii_case("CONNECT") {
        tunnel::handle_connect(client, target, config.timeout, conn_id, Arc::clone(&metrics))?;
    } else {
        http::handle_http_with_headers(
            client,
            peer_addr,
            request_line,
            raw_headers,
            reader,
            config.timeout,
            conn_id,
            Arc::clone(&metrics),
            Arc::clone(&cache),
        )?;
    }

    Ok(())
}
