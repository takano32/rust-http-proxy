pub mod acl;
pub mod auth;
pub mod config;
pub mod headers;
pub mod http;
pub mod metrics;
pub mod tunnel;

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;

use config::Config;
use metrics::Metrics;

const AUTH_REQUIRED_RESPONSE: &[u8] = b"HTTP/1.1 407 Proxy Authentication Required\r\n\
Proxy-Authenticate: Basic realm=\"sorahost-http-proxy\"\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Length: 30\r\n\
\r\n\
407 Proxy Authentication Required";

const FORBIDDEN_RESPONSE: &[u8] = b"HTTP/1.1 403 Forbidden\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Length: 13\r\n\
\r\n\
403 Forbidden";

pub fn handle_client(
    mut client: TcpStream,
    config: Arc<Config>,
    metrics: Arc<Metrics>,
    conn_id: usize,
) -> io::Result<()> {
    metrics.inc_active_conn();
    metrics.inc_requests();

    struct ConnGuard(Arc<Metrics>);
    impl Drop for ConnGuard {
        fn drop(&mut self) {
            self.0.dec_active_conn();
        }
    }
    let _guard = ConnGuard(Arc::clone(&metrics));

    let peer_addr = client.peer_addr().ok();
    client.set_read_timeout(Some(config.timeout))?;
    client.set_write_timeout(Some(config.timeout))?;

    let mut reader = BufReader::new(client.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }

    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }

    let method = parts[0];
    let target = parts[1];

    // Health check endpoint handling
    if (target == "/healthz" || target == "/status") && method.eq_ignore_ascii_case("GET") {
        let json_body = metrics.to_json();
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
        return Ok(());
    }

    let mut raw_headers = Vec::new();
    let mut proxy_auth_header = None;
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
            if k_lower == "proxy-authorization" {
                proxy_auth_header = Some(v.trim().to_string());
            } else if k_lower == "host" {
                host_header = Some(v.trim().to_string());
            }
        }
        raw_headers.push(line);
    }

    // 1. Auth check
    if config.auth.is_enabled() && !config.auth.validate(proxy_auth_header.as_deref()) {
        println!("[Conn #{}] 407 Unauthorized (Auth failed)", conn_id);
        client.write_all(AUTH_REQUIRED_RESPONSE)?;
        client.flush()?;
        return Ok(());
    }

    // 2. ACL / Host Check
    let target_host = if method.eq_ignore_ascii_case("CONNECT") {
        target
    } else {
        match http::parse_target(target, host_header.as_deref()) {
            Ok((h, _)) => h,
            Err(_) => {
                let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
                return Ok(());
            }
        }
    };

    if !config.acl.is_allowed(target_host) {
        println!("[Conn #{}] 403 Forbidden (ACL blocked host: {})", conn_id, target_host);
        client.write_all(FORBIDDEN_RESPONSE)?;
        client.flush()?;
        return Ok(());
    }

    // 3. Forward or Tunnel
    if method.eq_ignore_ascii_case("CONNECT") {
        tunnel::handle_connect(client, target, config.timeout, conn_id)?;
    } else {
        http::handle_http_with_headers(
            client,
            peer_addr,
            request_line,
            raw_headers,
            reader,
            config.timeout,
            conn_id,
        )?;
    }

    Ok(())
}
