pub mod auth;
pub mod config;
pub mod headers;
pub mod http;
pub mod tunnel;

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use config::Config;

const AUTH_REQUIRED_RESPONSE: &[u8] = b"HTTP/1.1 407 Proxy Authentication Required\r\n\
Proxy-Authenticate: Basic realm=\"sorahost-http-proxy\"\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Length: 30\r\n\
\r\n\
407 Proxy Authentication Required";

pub fn handle_client(mut client: TcpStream, config: Arc<Config>, conn_id: usize) -> io::Result<()> {
    let peer_addr = client.peer_addr().ok();
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    client.set_write_timeout(Some(Duration::from_secs(30)))?;

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

    if config.auth.is_enabled() {
        // Read headers to check Proxy-Authorization
        let mut proxy_auth_header = None;
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

            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case("proxy-authorization") {
                    proxy_auth_header = Some(v.trim().to_string());
                }
            }
            raw_headers.push(line);
        }

        if !config.auth.validate(proxy_auth_header.as_deref()) {
            println!("[Conn #{}] 407 Unauthorized (Auth failed)", conn_id);
            client.write_all(AUTH_REQUIRED_RESPONSE)?;
            client.flush()?;
            return Ok(());
        }

        if method.eq_ignore_ascii_case("CONNECT") {
            tunnel::handle_connect(client, target, conn_id)?;
        } else {
            http::handle_http_with_headers(
                client,
                peer_addr,
                request_line,
                raw_headers,
                reader,
                conn_id,
            )?;
        }
    } else if method.eq_ignore_ascii_case("CONNECT") {
        tunnel::handle_connect(client, target, conn_id)?;
    } else {
        http::handle_http(client, peer_addr, request_line, reader, conn_id)?;
    }

    Ok(())
}
