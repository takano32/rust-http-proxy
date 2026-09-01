use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

static CONN_COUNTER: AtomicUsize = AtomicUsize::new(1);

fn main() {
    let port_str = env::var("SERVER_PORT").unwrap_or_else(|_| {
        eprintln!("SERVER_PORT is not set, defaulting to 8080");
        "8080".to_string()
    });

    let port: u16 = match port_str.parse() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Invalid SERVER_PORT '{}': {}", port_str, e);
            process::exit(1);
        }
    };

    let addr = format!("0.0.0.0:{}", port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind on {}: {}", addr, e);
            process::exit(1);
        }
    };

    println!("HTTP/HTTPS Proxy listening on {}", addr);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let conn_id = CONN_COUNTER.fetch_add(1, Ordering::Relaxed);
                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, conn_id) {
                        if e.kind() != io::ErrorKind::UnexpectedEof
                            && e.kind() != io::ErrorKind::ConnectionReset
                            && e.kind() != io::ErrorKind::BrokenPipe
                        {
                            eprintln!("[Conn #{}] Error: {}", conn_id, e);
                        }
                    }
                });
            }
            Err(e) => {
                eprintln!("Accept failed: {}", e);
            }
        }
    }
}

fn handle_client(client: TcpStream, conn_id: usize) -> io::Result<()> {
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

    if method.eq_ignore_ascii_case("CONNECT") {
        handle_connect(client, target, conn_id)?;
    } else {
        handle_http(client, request_line, reader, conn_id)?;
    }

    Ok(())
}

fn handle_connect(mut client: TcpStream, target: &str, conn_id: usize) -> io::Result<()> {
    let addr = if target.contains(':') {
        target.to_string()
    } else {
        format!("{}:443", target)
    };

    println!("[Conn #{}] CONNECT {}", conn_id, addr);

    let server = match TcpStream::connect(&addr) {
        Ok(s) => s,
        Err(e) => {
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
            return Err(e);
        }
    };

    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    client.flush()?;

    client.set_read_timeout(None)?;
    client.set_write_timeout(None)?;
    server.set_read_timeout(None)?;
    server.set_write_timeout(None)?;

    tunnel(client, server)
}

fn handle_http(
    mut client: TcpStream,
    request_line: String,
    mut reader: BufReader<TcpStream>,
    conn_id: usize,
) -> io::Result<()> {
    let mut headers = Vec::new();
    let mut host_header = None;
    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;

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
            let v_trim = v.trim();
            if k_lower == "host" {
                host_header = Some(v_trim.to_string());
            } else if k_lower == "content-length" {
                content_length = v_trim.parse().ok();
            } else if k_lower == "transfer-encoding" && v_trim.eq_ignore_ascii_case("chunked") {
                is_chunked = true;
            } else if k_lower == "proxy-connection" {
                continue;
            }
        }
        headers.push(line);
    }

    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }
    let method = parts[0];
    let target = parts[1];
    let version = if parts.len() > 2 { parts[2] } else { "HTTP/1.1" };

    let (host_port, path) = if let Some(stripped) = target.strip_prefix("http://") {
        if let Some(pos) = stripped.find('/') {
            (&stripped[..pos], &stripped[pos..])
        } else {
            (stripped, "/")
        }
    } else if target.starts_with('/') {
        if let Some(ref h) = host_header {
            (h.as_str(), target)
        } else {
            let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
            return Ok(());
        }
    } else {
        (target, "/")
    };

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

    for h in headers {
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

fn tunnel(mut client: TcpStream, mut server: TcpStream) -> io::Result<()> {
    let mut client_clone = client.try_clone()?;
    let mut server_clone = server.try_clone()?;

    let t1 = thread::spawn(move || {
        let _ = io::copy(&mut client, &mut server);
        let _ = server.shutdown(std::net::Shutdown::Write);
    });

    let t2 = thread::spawn(move || {
        let _ = io::copy(&mut server_clone, &mut client_clone);
        let _ = client_clone.shutdown(std::net::Shutdown::Write);
    });

    let _ = t1.join();
    let _ = t2.join();
    Ok(())
}
