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

type Handler = dyn Fn(&str, usize) -> Vec<u8> + Send + Sync;

fn start_mock_origin() -> (u16, thread::JoinHandle<()>) {
    start_counting_origin(Arc::new(AtomicUsize::new(0)), "")
}

/// オリジンへの到達回数を数えるモックサーバー。`extra_headers` は追加のレスポンスヘッダー。
fn start_counting_origin(
    counter: Arc<AtomicUsize>,
    extra_headers: &'static str,
) -> (u16, thread::JoinHandle<()>) {
    start_origin(
        counter,
        Arc::new(move |_req, _n| {
            let body = "hello from mock origin";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
                body.len(),
                extra_headers,
                body
            )
            .into_bytes()
        }),
    )
}

/// リクエスト全文と通し番号 (1 始まり) を受け取って応答を返すモックサーバー。
fn start_origin(counter: Arc<AtomicUsize>, handler: Arc<Handler>) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let counter = Arc::clone(&counter);
            let handler = Arc::clone(&handler);
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
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let resp = handler(&String::from_utf8_lossy(&req), n);
                let _ = stream.write_all(&resp);
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

fn proxy_config() -> Config {
    Config::new("0", None, None, Duration::from_secs(5)).unwrap()
}

#[test]
fn test_integration_http_forwarding() {
    let (origin_port, _origin_handle) = start_mock_origin();
    let proxy_port = start_test_proxy(proxy_config());

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
    let proxy_port = start_test_proxy(proxy_config());

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
    get_via_proxy_with(proxy_port, url, host, "")
}

/// `extra` は追加のリクエストヘッダー行 (CRLF 終端)。
fn get_via_proxy_with(proxy_port: u16, url: &str, host: &str, extra: &str) -> String {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!("GET {} HTTP/1.1\r\nHost: {}\r\n{}\r\n", url, host, extra);
    stream.write_all(req.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    String::from_utf8_lossy(&response).into_owned()
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
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-hit"));

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
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-nostore"));

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
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-status"));

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

/// ETag 付きで常に再検証が必要 (max-age=0) な表現を返し、If-None-Match が一致すれば 304 を返す。
fn revalidating_origin(
    counter: Arc<AtomicUsize>,
    fail_after_first: bool,
) -> (u16, thread::JoinHandle<()>) {
    start_origin(
        counter,
        Arc::new(move |req, n| {
            if fail_after_first && n > 1 {
                return b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
            }
            if req.contains("If-None-Match: \"v1\"") {
                return b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n"
                    .to_vec();
            }
            let body = "revalidated body";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"v1\"\r\nCache-Control: max-age=0\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    )
}

#[test]
fn test_integration_stale_entry_is_revalidated_with_304() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = revalidating_origin(Arc::clone(&counter), false);
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-reval"));
    let url = format!("http://127.0.0.1:{}/reval", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(
        first.contains("revalidated body") && !first.contains("X-Cache"),
        "{}",
        first
    );

    // max-age=0 なので 2 回目は条件付きで再検証 → 304 → 保存済み本文を配信
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.starts_with("HTTP/1.1 200 OK"), "{}", second);
    assert!(
        second.contains("X-Cache: REVALIDATED from sorahost-http-proxy (memory)"),
        "{}",
        second
    );
    assert!(second.contains("revalidated body"), "{}", second);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "origin sees a conditional request"
    );
}

#[test]
fn test_integration_stale_is_served_when_origin_fails() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = revalidating_origin(Arc::clone(&counter), true);
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-stale"));
    let url = format!("http://127.0.0.1:{}/stale", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    get_via_proxy(proxy_port, &url, &host);
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.starts_with("HTTP/1.1 200 OK"), "{}", second);
    assert!(
        second.contains("X-Cache: STALE from sorahost-http-proxy (memory)"),
        "{}",
        second
    );
    assert!(second.contains("revalidated body"), "{}", second);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_integration_client_conditional_request_gets_304_from_cache() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(
        Arc::clone(&counter),
        "ETag: \"v1\"\r\nCache-Control: max-age=60\r\n",
    );
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-cond"));
    let url = format!("http://127.0.0.1:{}/cond", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    get_via_proxy(proxy_port, &url, &host);
    let resp = get_via_proxy_with(proxy_port, &url, &host, "If-None-Match: \"v1\"\r\n");
    assert!(resp.starts_with("HTTP/1.1 304 Not Modified"), "{}", resp);
    assert!(resp.contains("ETag: \"v1\""), "{}", resp);
    assert!(!resp.contains("hello from mock origin"));
    assert_eq!(counter.load(Ordering::SeqCst), 1, "answered from cache");

    // 一致しない ETag なら通常のヒット
    let resp = get_via_proxy_with(proxy_port, &url, &host, "If-None-Match: \"other\"\r\n");
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{}", resp);
    assert!(resp.contains("hello from mock origin"));
}

#[test]
fn test_integration_large_response_streams_through_disk() {
    let counter = Arc::new(AtomicUsize::new(0));
    let body: Vec<u8> = (0..(3 * MIB as usize)).map(|i| (i % 251) as u8).collect();
    let body_for_origin = body.clone();
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(move |_req, _n| {
            let mut resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age=60\r\nConnection: close\r\n\r\n",
                body_for_origin.len()
            )
            .into_bytes();
            resp.extend_from_slice(&body_for_origin);
            resp
        }),
    );
    let mut cfg = cache_cfg("shp-it-large");
    cfg.mem_max_object_size = 64 * 1024;
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cfg);
    let url = format!("http://127.0.0.1:{}/big.bin", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let fetch = |extra: &str| {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        let req = format!("GET {} HTTP/1.1\r\nHost: {}\r\n{}\r\n", url, host, extra);
        stream.write_all(req.as_bytes()).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        (
            String::from_utf8_lossy(&response[..split]).into_owned(),
            response[split..].to_vec(),
        )
    };
    let (head, got) = fetch("");
    assert!(head.starts_with("HTTP/1.1 200 OK"));
    assert_eq!(got, body);
    let (head, got) = fetch("");
    assert!(
        head.contains("X-Cache: HIT from sorahost-http-proxy (disk)"),
        "{}",
        head
    );
    assert_eq!(got, body);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}
