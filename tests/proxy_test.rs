use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use sorahost_http_proxy::config::Config;
use sorahost_http_proxy::handle_client;
use sorahost_http_proxy::metrics::Metrics;

fn start_mock_origin() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(mut stream) = stream {
                thread::spawn(move || {
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf);
                    let body = "hello from mock origin";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                });
            }
        }
    });

    (port, handle)
}

fn start_test_proxy(config: Config) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let cfg = Arc::new(config);
    let metrics = Arc::new(Metrics::new());

    thread::spawn(move || {
        let mut conn_id = 0;
        for stream in listener.incoming() {
            if let Ok(stream) = stream {
                conn_id += 1;
                let c = Arc::clone(&cfg);
                let m = Arc::clone(&metrics);
                thread::spawn(move || {
                    let _ = handle_client(stream, c, m, conn_id);
                });
            }
        }
    });

    port
}

#[test]
fn test_integration_http_forwarding() {
    let (origin_port, _origin_handle) = start_mock_origin();
    let config = Config::new("0", None, None, Duration::from_secs(5)).unwrap();
    let proxy_port = start_test_proxy(config);

    // Send HTTP proxy request without any authentication
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "GET http://127.0.0.1:{}/test HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        origin_port, origin_port
    );
    stream.write_all(req.as_bytes()).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("hello from mock origin"));
}

#[test]
fn test_integration_acl_denied() {
    let (origin_port, _origin_handle) = start_mock_origin();
    let config = Config::new(
        "0",
        None,
        Some("blocked.com, 127.0.0.1"),
        Duration::from_secs(5),
    )
    .unwrap();
    let proxy_port = start_test_proxy(config);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "GET http://127.0.0.1:{}/test HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        origin_port, origin_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
}

#[test]
fn test_integration_healthz() {
    let config = Config::new("0", None, None, Duration::from_secs(5)).unwrap();
    let proxy_port = start_test_proxy(config);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("\"status\":\"ok\""));
}
