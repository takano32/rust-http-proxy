use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};

use crate::headers;

pub fn handle_http(
    client: TcpStream,
    peer_addr: Option<SocketAddr>,
    request_line: String,
    mut reader: BufReader<TcpStream>,
    conn_id: usize,
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
    handle_http_with_headers(client, peer_addr, request_line, raw_headers, reader, conn_id)
}

pub fn handle_http_with_headers(
    mut client: TcpStream,
    peer_addr: Option<SocketAddr>,
    request_line: String,
    raw_headers: Vec<String>,
    mut reader: BufReader<TcpStream>,
    conn_id: usize,
) -> io::Result<()> {
    let mut host_header = None;
    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;

    for line in &raw_headers {
        if let Some((k, v)) = line.split_once(':') {
            let k_lower = k.trim().to_ascii_lowercase();
            let v_trim = v.trim();
            if k_lower == "host" {
                host_header = Some(v_trim.to_string());
            } else if k_lower == "content-length" {
                content_length = v_trim.parse().ok();
            } else if k_lower == "transfer-encoding" && v_trim.eq_ignore_ascii_case("chunked") {
                is_chunked = true;
            }
        }
    }

    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
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

    println!("[Conn #{}] {} http://{}{}", conn_id, method, server_addr, path);

    let mut server = match TcpStream::connect(&server_addr) {
        Ok(s) => s,
        Err(e) => {
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
            return Err(e);
        }
    };

    let forward_request_line = format!("{} {} {}\r\n", method, path, version);
    server.write_all(forward_request_line.as_bytes())?;

    let sanitized_headers = headers::sanitize_and_inject_headers(&raw_headers, peer_addr);
    for h in sanitized_headers {
        server.write_all(h.as_bytes())?;
    }
    server.write_all(b"\r\n")?;

    if let Some(len) = content_length {
        if len > 0 {
            let mut body_reader = (&mut reader).take(len as u64);
            io::copy(&mut body_reader, &mut server)?;
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
        }
    }

    server.flush()?;
    let _ = io::copy(&mut server, &mut client);

    Ok(())
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
}
