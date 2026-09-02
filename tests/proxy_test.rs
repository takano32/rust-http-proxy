use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicUsize, Ordering};

use sorahost_http_proxy::cache::{Cache, CacheConfig, MIB};
use sorahost_http_proxy::config::Config;
use sorahost_http_proxy::metrics::Metrics;
use sorahost_http_proxy::pool::Pool;
use sorahost_http_proxy::tls::TlsClient;
use sorahost_http_proxy::{Upstream, handle_client};

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
    start_test_proxy_full(config, cache_cfg, None)
}

/// `ca_file` を渡すと、その証明書だけを信頼する TLS クライアント付きで起動する。
fn start_test_proxy_full(
    config: Config,
    cache_cfg: CacheConfig,
    ca_file: Option<std::path::PathBuf>,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let cfg = Arc::new(config);
    let metrics = Arc::new(Metrics::new());
    let cache = Arc::new(Cache::new(cache_cfg));
    let tls = TlsClient::load(true, ca_file.as_deref()).ok().flatten();
    let pool = Arc::new(Upstream {
        pool: Pool::new(cfg.pool_per_host, Duration::from_secs(30)),
        tls,
    });

    thread::spawn(move || {
        for (conn_id, stream) in listener.incoming().flatten().enumerate() {
            let c = Arc::clone(&cfg);
            let m = Arc::clone(&metrics);
            let ch = Arc::clone(&cache);
            let p = Arc::clone(&pool);
            thread::spawn(move || {
                let _ = handle_client(stream, c, m, ch, p, conn_id);
            });
        }
    });

    port
}

fn proxy_config() -> Config {
    let mut cfg = Config::new("0", None, None, Duration::from_secs(5)).unwrap();
    cfg.keepalive = Duration::from_secs(2);
    cfg
}

/// 1 本の接続で複数の要求を送るための、Content-Length 付き応答の読み取り。
fn read_response(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).unwrap() == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).unwrap();
    (head, body)
}

/// HTTP/1.1 keep-alive で複数の要求に応答するモックオリジン。接続数と要求数を数える。
fn start_keepalive_origin(
    connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            connections.fetch_add(1, Ordering::SeqCst);
            let requests = Arc::clone(&requests);
            thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut lines = Vec::new();
                    loop {
                        let mut line = String::new();
                        if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line.trim().is_empty() {
                            break;
                        }
                        lines.push(line);
                    }
                    if lines.is_empty() {
                        return;
                    }
                    let n = requests.fetch_add(1, Ordering::SeqCst) + 1;
                    let body = format!("response #{}", n);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    if stream.write_all(resp.as_bytes()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (port, handle)
}

#[test]
fn test_integration_http_forwarding() {
    let (origin_port, _origin_handle) = start_mock_origin();
    let proxy_port = start_test_proxy(proxy_config());

    // Send HTTP proxy request without any authentication
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "GET http://127.0.0.1:{}/test HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
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
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{}\r\n",
        url, host, extra
    );
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
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{}\r\n",
            url, host, extra
        );
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

#[test]
fn test_integration_keepalive_serves_multiple_requests_per_connection() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-keepalive"));
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    for i in 0..3 {
        let req = format!(
            "GET http://{}/ka{} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, i, host
        );
        stream.write_all(req.as_bytes()).unwrap();
        let (head, body) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
        assert!(head.contains("Connection: keep-alive"), "{}", head);
        assert_eq!(body, b"hello from mock origin");
    }
    // 同じ接続でキャッシュヒットも返る
    let req = format!("GET http://{}/ka0 HTTP/1.1\r\nHost: {}\r\n\r\n", host, host);
    stream.write_all(req.as_bytes()).unwrap();
    let (head, body) = read_response(&mut stream);
    assert!(head.contains("X-Cache: HIT"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 3);

    // Connection: close で終わる
    let req = format!(
        "GET http://{}/ka1 HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        host, host
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    let text = String::from_utf8_lossy(&rest);
    assert!(text.contains("Connection: close"), "{}", text);
    assert!(text.ends_with("hello from mock origin"));

    // HTTP/1.0 の要求は応答後に閉じられる
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!("GET http://{}/ka2 HTTP/1.0\r\nHost: {}\r\n\r\n", host, host);
    stream.write_all(req.as_bytes()).unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    assert!(String::from_utf8_lossy(&rest).contains("Connection: close"));
}

#[test]
fn test_integration_origin_connections_are_pooled() {
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        start_keepalive_origin(Arc::clone(&connections), Arc::clone(&requests));
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-pool"));
    let host = format!("127.0.0.1:{}", origin_port);

    for i in 0..4 {
        let resp = get_via_proxy(proxy_port, &format!("http://{}/p{}", host, i), &host);
        assert!(resp.contains(&format!("response #{}", i + 1)), "{}", resp);
    }
    assert_eq!(requests.load(Ordering::SeqCst), 4);
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "one pooled origin connection"
    );

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .write_all(b"GET /status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .unwrap();
    let mut status = String::new();
    stream.read_to_string(&mut status).unwrap();
    assert!(
        status.contains("\"origin_connections\":{\"new\":1,\"reused\":3}"),
        "{}",
        status
    );
}

#[test]
fn test_integration_chunked_origin_is_dechunked_cached_and_reframed() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(|_req, _n| {
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nCache-Control: max-age=60\r\nConnection: close\r\n\r\n5\r\nhello\r\n7\r\n chunks\r\n0\r\n\r\n".to_vec()
        }),
    );
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-chunked"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/chunked", host);

    // HTTP/1.0 クライアントには解読済みの本文を close 区切りで
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!("GET {} HTTP/1.0\r\nHost: {}\r\n\r\n", url, host);
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(!text.contains("Transfer-Encoding"), "{}", text);
    assert!(text.ends_with("\r\n\r\nhello chunks"), "{}", text);

    // HTTP/1.1 クライアントには再 chunk (終端チャンク付き)
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "GET {}?v2 HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        url, host
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(text.contains("Transfer-Encoding: chunked"), "{}", text);
    assert!(text.ends_with("0\r\n\r\n"), "{}", text);
    assert!(text.contains("hello") && text.contains(" chunks"));

    // キャッシュからは Content-Length 付きで
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.contains("X-Cache: HIT"), "{}", second);
    assert!(second.contains("Content-Length: 12"), "{}", second);
    assert!(second.ends_with("hello chunks"), "{}", second);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_integration_range_and_head_from_cache() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(
        Arc::clone(&counter),
        "ETag: \"r1\"\r\nCache-Control: max-age=60\r\n",
    );
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-range"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/range", host);

    // 未キャッシュの Range 要求はそのまま転送 (モックは無視して 200 を返す) され、保存されない
    let first = get_via_proxy_with(proxy_port, &url, &host, "Range: bytes=0-4\r\n");
    assert!(
        first.starts_with("HTTP/1.1 200 OK") && !first.contains("X-Cache"),
        "{}",
        first
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    // 全体を取得してキャッシュ
    let full = get_via_proxy(proxy_port, &url, &host);
    assert!(full.ends_with("hello from mock origin"));
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    let part = get_via_proxy_with(proxy_port, &url, &host, "Range: bytes=6-9\r\n");
    assert!(part.starts_with("HTTP/1.1 206 Partial Content"), "{}", part);
    assert!(part.contains("Content-Range: bytes 6-9/22"), "{}", part);
    assert!(part.contains("Content-Length: 4"), "{}", part);
    assert!(part.ends_with("\r\n\r\nfrom"), "{}", part);

    let tail = get_via_proxy_with(proxy_port, &url, &host, "Range: bytes=-6\r\n");
    assert!(
        tail.contains("Content-Range: bytes 16-21/22") && tail.ends_with("origin"),
        "{}",
        tail
    );

    let bad = get_via_proxy_with(proxy_port, &url, &host, "Range: bytes=100-\r\n");
    assert!(
        bad.starts_with("HTTP/1.1 416 Range Not Satisfiable"),
        "{}",
        bad
    );
    assert!(bad.contains("Content-Range: bytes */22"), "{}", bad);

    // If-Range が合わなければ全体
    let whole = get_via_proxy_with(
        proxy_port,
        &url,
        &host,
        "Range: bytes=0-1\r\nIf-Range: \"other\"\r\n",
    );
    assert!(
        whole.starts_with("HTTP/1.1 200 OK") && whole.ends_with("hello from mock origin"),
        "{}",
        whole
    );

    // HEAD はキャッシュからヘッダーだけ
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "HEAD {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        url, host
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut head = String::new();
    stream.read_to_string(&mut head).unwrap();
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert!(
        head.contains("Content-Length: 22") && head.contains("X-Cache: HIT"),
        "{}",
        head
    );
    assert!(head.ends_with("\r\n\r\n"), "{}", head);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "range and head answered from cache"
    );
}

#[test]
fn test_integration_unsafe_method_invalidates_cached_get() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(|req, n| {
            let body = format!("version {}", n);
            let status = if req.starts_with("POST") {
                "204 No Content"
            } else {
                "200 OK"
            };
            if req.starts_with("POST") {
                return format!(
                    "HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    status
                )
                .into_bytes();
            }
            format!(
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nCache-Control: max-age=60\r\nConnection: close\r\n\r\n{}",
                status, body.len(), body
            )
            .into_bytes()
        }),
    );
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-invalidate"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/item", host);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.ends_with("version 1"), "{}", first);
    let hit = get_via_proxy(proxy_port, &url, &host);
    assert!(
        hit.contains("X-Cache: HIT") && hit.ends_with("version 1"),
        "{}",
        hit
    );

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        url, host
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 204"), "{}", resp);

    // POST 後は古い表現が消えていて、オリジンから取り直す
    let after = get_via_proxy(proxy_port, &url, &host);
    assert!(
        !after.contains("X-Cache") && after.ends_with("version 3"),
        "{}",
        after
    );
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

/// 自己署名証明書を作り、python の https サーバーを立てる。道具が無ければ None (テストはスキップ)。
fn start_tls_origin(
    dir: &std::path::Path,
) -> Option<(u16, std::path::PathBuf, std::process::Child)> {
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    std::fs::create_dir_all(dir.join("www")).ok()?;
    std::fs::write(dir.join("www/hello.txt"), b"hello over tls").ok()?;
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let ok = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-keyout",
            key.to_str()?,
            "-out",
            cert.to_str()?,
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=IP:127.0.0.1,DNS:localhost",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?
        .success();
    if !ok {
        return None;
    }
    let script = format!(
        "import http.server, ssl, functools, sys\n\
         class H(http.server.SimpleHTTPRequestHandler):\n\
             protocol_version = 'HTTP/1.1'\n\
             def log_message(self, *a): pass\n\
         ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)\n\
         ctx.load_cert_chain({cert:?}, {key:?})\n\
         srv = http.server.ThreadingHTTPServer(('127.0.0.1', 0), functools.partial(H, directory={www:?}))\n\
         srv.socket = ctx.wrap_socket(srv.socket, server_side=True)\n\
         print(srv.server_address[1], flush=True)\n\
         srv.serve_forever()\n",
        cert = cert.to_str()?,
        key = key.to_str()?,
        www = dir.join("www").to_str()?,
    );
    let mut child = Command::new("python3")
        .args(["-c", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take()?)
        .read_line(&mut line)
        .ok()?;
    let port: u16 = line.trim().parse().ok()?;
    Some((port, cert, child))
}

#[test]
fn test_integration_https_origin_is_fetched_and_cached() {
    if TlsClient::load(true, None).ok().flatten().is_none() {
        eprintln!("skipping: libssl not available");
        return;
    }
    let dir = std::env::temp_dir().join("shp-it-tls");
    let _ = std::fs::remove_dir_all(&dir);
    let Some((origin_port, cert, mut child)) = start_tls_origin(&dir) else {
        eprintln!("skipping: openssl or python3 with ssl not available");
        return;
    };
    // 自己署名の CA を信頼するプロキシ
    let proxy_port =
        start_test_proxy_full(proxy_config(), cache_cfg("shp-it-tls-cache"), Some(cert));
    let mapped = format!("/https/127.0.0.1:{}/hello.txt", origin_port);

    let first = get_via_proxy(proxy_port, &mapped, "proxy.local");
    assert!(first.starts_with("HTTP/1.1 200 OK"), "{}", first);
    assert!(first.ends_with("hello over tls"), "{}", first);
    assert!(!first.contains("X-Cache"));

    let second = get_via_proxy(proxy_port, &mapped, "proxy.local");
    assert!(second.contains("X-Cache: HIT"), "{}", second);
    assert!(second.ends_with("hello over tls"), "{}", second);

    // 絶対形式でも同じキャッシュに当たる
    let absolute = get_via_proxy(
        proxy_port,
        &format!("https://127.0.0.1:{}/hello.txt", origin_port),
        &format!("127.0.0.1:{}", origin_port),
    );
    assert!(absolute.contains("X-Cache: HIT"), "{}", absolute);

    // CA を渡さないプロキシは検証に失敗して 502
    let strict_port = start_test_proxy_full(proxy_config(), cache_cfg("shp-it-tls-strict"), None);
    let rejected = get_via_proxy(strict_port, &mapped, "proxy.local");
    assert!(rejected.starts_with("HTTP/1.1 502"), "{}", rejected);

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn test_integration_metrics_purge_and_lookup_endpoints() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        start_counting_origin(Arc::clone(&counter), "Cache-Control: max-age=60\r\n");
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-endpoints"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/asset", host);

    get_via_proxy(proxy_port, &url, &host);
    let hit = get_via_proxy(proxy_port, &url, &host);
    assert!(hit.contains("X-Cache: HIT"));

    let endpoint = |req: &str| {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        stream.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    };

    // /lookup はエントリを報告する (LRU には触らない)
    let looked = endpoint(&format!(
        "GET /lookup?url={} HTTP/1.1\r\nHost: x\r\n\r\n",
        url
    ));
    assert!(looked.starts_with("HTTP/1.1 200 OK"), "{}", looked);
    assert!(
        looked.contains("\"found\":true") && looked.contains("\"memory\":true"),
        "{}",
        looked
    );

    // /metrics は Prometheus 形式でヒット数とホスト別統計を出す
    let metrics = endpoint("GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(metrics.contains("text/plain; version=0.0.4"), "{}", metrics);
    assert!(
        metrics.contains("sorahost_cache_hits_total{tier=\"memory\"} 1"),
        "{}",
        metrics
    );
    assert!(
        metrics.contains(&format!(
            "sorahost_host_hits_total{{host=\"http://{}\"}} 1",
            host
        )),
        "{}",
        metrics
    );
    assert!(
        metrics.contains(&format!(
            "sorahost_host_misses_total{{host=\"http://{}\"}} 1",
            host
        )),
        "{}",
        metrics
    );

    // /status にもホスト別が入る
    let status = endpoint("GET /status HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(
        status.contains(&format!(
            "\"host\":\"http://{}\",\"requests\":2,\"hits\":1,\"misses\":1",
            host
        )),
        "{}",
        status
    );

    // PURGE メソッドで消える → 次は MISS
    let purged = endpoint(&format!("PURGE {} HTTP/1.1\r\nHost: {}\r\n\r\n", url, host));
    assert!(
        purged.starts_with("HTTP/1.1 200 OK") && purged.contains("\"purged\":1"),
        "{}",
        purged
    );
    let after = get_via_proxy(proxy_port, &url, &host);
    assert!(!after.contains("X-Cache"), "{}", after);
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    // /purge?all=1 で全消去、/lookup は 404
    let all = endpoint("GET /purge?all=1 HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(all.contains("\"all\":true"), "{}", all);
    let gone = endpoint(&format!(
        "GET /lookup?url={} HTTP/1.1\r\nHost: x\r\n\r\n",
        url
    ));
    assert!(
        gone.starts_with("HTTP/1.1 404") && gone.contains("\"found\":false"),
        "{}",
        gone
    );
    let bad = endpoint("GET /purge HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(bad.starts_with("HTTP/1.1 400"), "{}", bad);
}
