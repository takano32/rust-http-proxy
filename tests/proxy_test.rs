use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicUsize, Ordering};

use sorahost_http_proxy::cache::{Cache, CacheConfig, MIB};
use sorahost_http_proxy::config::Config;
use sorahost_http_proxy::handle_client;
use sorahost_http_proxy::metrics::Metrics;

fn start_mock_origin() -> (u16, thread::JoinHandle<()>) {
    start_counting_origin(Arc::new(AtomicUsize::new(0)), "")
}

/// オリジンへの到達回数を数えるモックサーバー。`extra_headers` は追加のレスポンスヘッダー。
fn start_counting_origin(
    counter: Arc<AtomicUsize>,
    extra_headers: &'static str,
) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let counter = Arc::clone(&counter);
            thread::spawn(move || {
                // ヘッダー終端まで読み切ってから応答する (読み残しがあると close 時に
                // RST が飛び、プロキシ側でレスポンスが「途中で切れた」扱いになる)
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let body = "hello from mock origin";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
                    body.len(),
                    extra_headers,
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            });
        }
    });

    (port, handle)
}

fn start_test_proxy(config: Config) -> u16 {
    start_test_proxy_with_cache(config, CacheConfig::disabled())
}

fn start_test_proxy_with_cache(config: Config, cache_cfg: CacheConfig) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let cfg = Arc::new(config);
    let metrics = Arc::new(Metrics::new());
    let cache = Arc::new(Cache::new(cache_cfg));

    thread::spawn(move || {
        for (conn_id, stream) in listener.incoming().flatten().enumerate() {
            let c = Arc::clone(&cfg);
            let m = Arc::clone(&metrics);
            let ch = Arc::clone(&cache);
            thread::spawn(move || {
                let _ = handle_client(stream, c, m, ch, conn_id);
            });
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
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(response.contains("\"status\":\"ok\""));
}

fn get_via_proxy(proxy_port: u16, url: &str, host: &str) -> String {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!("GET {} HTTP/1.1\r\nHost: {}\r\n\r\n", url, host);
    stream.write_all(req.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// 固定上限 (メモリ 200 MiB / ディスク 2048 MiB) のテスト用キャッシュ設定。
fn cache_cfg(dir: &str) -> CacheConfig {
    let dir = std::env::temp_dir().join(dir);
    let _ = std::fs::remove_dir_all(&dir);
    CacheConfig {
        default_ttl: Duration::from_secs(60),
        ..CacheConfig::fixed(200 * MIB, 2048 * MIB, dir)
    }
}

#[test]
fn test_integration_cache_hit_serves_second_request() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let config = Config::new("0", None, None, Duration::from_secs(5)).unwrap();
    let proxy_port = start_test_proxy_with_cache(config, cache_cfg("shp-it-hit"));

    let url = format!("http://127.0.0.1:{}/cached", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.contains("hello from mock origin"));
    assert!(
        !first.contains("X-Cache"),
        "first response must be a MISS: {}",
        first
    );

    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.starts_with("HTTP/1.1 200 OK"));
    assert!(
        second.contains("X-Cache: HIT from sorahost-http-proxy (memory)"),
        "{}",
        second
    );
    assert!(second.contains("Age: "));
    assert!(second.contains("hello from mock origin"));

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "origin should be hit once"
    );
}

#[test]
fn test_integration_no_store_response_is_not_cached() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        start_counting_origin(Arc::clone(&counter), "Cache-Control: no-store\r\n");
    let config = Config::new("0", None, None, Duration::from_secs(5)).unwrap();
    let proxy_port = start_test_proxy_with_cache(config, cache_cfg("shp-it-nostore"));

    let url = format!("http://127.0.0.1:{}/private", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    get_via_proxy(proxy_port, &url, &host);
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(!second.contains("X-Cache"), "{}", second);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "origin should be hit twice"
    );
}

#[test]
fn test_integration_status_reports_cache_limits() {
    let config = Config::new("0", None, None, Duration::from_secs(5)).unwrap();
    let proxy_port = start_test_proxy_with_cache(config, cache_cfg("shp-it-status"));

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .write_all(b"GET /status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    // memory 200MiB / disk 2048MiB
    assert!(
        response.contains("\"limit_bytes\":209715200"),
        "{}",
        response
    );
    assert!(
        response.contains("\"limit_bytes\":2147483648"),
        "{}",
        response
    );
    assert!(response.contains("\"mode\":\"fixed\""), "{}", response);
    assert!(response.contains("\"cache_hits\":0"), "{}", response);
}
