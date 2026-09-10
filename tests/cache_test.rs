//! キャッシュの結合テスト (保存・再検証・古い表現の提供・範囲取得・無効化)。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rust_http_proxy::cache::MIB;
use rust_http_proxy::tls::TlsClient;

mod common;
use common::*;

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
        second.contains("X-Cache: HIT from rust-http-proxy (memory)"),
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
        .write_all(
            format!(
                "GET /status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                proxy_port
            )
            .as_bytes(),
        )
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
        second.contains("X-Cache: REVALIDATED from rust-http-proxy (memory)"),
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
        second.contains("X-Cache: STALE from rust-http-proxy (memory)"),
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
        head.contains("X-Cache: HIT from rust-http-proxy (disk)"),
        "{}",
        head
    );
    assert_eq!(got, body);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
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
fn test_integration_grace_serves_stale_and_refreshes_in_background() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(|req, n| {
            if req.contains("If-None-Match: \"g1\"") {
                // 再検証では鮮度を長めに延ばす。max-age=1 のままだと、再検証の完了を
                // 待っている間に再び期限が切れて、次が HIT にならないことがある
                return b"HTTP/1.1 304 Not Modified\r\nETag: \"g1\"\r\nCache-Control: max-age=60\r\nConnection: close\r\n\r\n".to_vec();
            }
            let body = format!("grace body {}", n);
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"g1\"\r\nCache-Control: max-age=1\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    );
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-grace"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/grace", host);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.ends_with("grace body 1"), "{}", first);
    thread::sleep(Duration::from_millis(1500));

    // 期限切れ直後: すぐ古い表現が返り (REFRESHING)、裏で再検証が走る
    let started = std::time::Instant::now();
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(
        second.contains("X-Cache: REFRESHING from rust-http-proxy (memory)"),
        "{}",
        second
    );
    assert!(second.ends_with("grace body 1"), "{}", second);
    // 裏で取り直すので、オリジンの応答を待っていない = 十分速い (負荷の高い CI でも通る幅にする)
    assert!(started.elapsed() < Duration::from_millis(2000));

    // 裏の再検証 (304) が「終わる」まで待つ。オリジンに届いた時点 (counter >= 2) では
    // まだ保存が終わっていないことがあるので、実行中の数が 0 に戻るのを見る
    wait_until(
        || {
            counter.load(Ordering::SeqCst) >= 2
                && status_json(proxy_port).contains("\"revalidating\":0")
        },
        "the background revalidation should finish",
    );
    let third = get_via_proxy(proxy_port, &url, &host);
    assert!(third.contains("X-Cache: HIT"), "{}", third);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "one conditional request in the background"
    );
}

/// 名前が `name` の OS スレッドの数。Linux 以外では数えられないので常に 0。
#[cfg(target_os = "linux")]
fn threads_named(name: &str) -> usize {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    dir.filter_map(|e| e.ok())
        .filter(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == name))
        .count()
}

#[cfg(not(target_os = "linux"))]
fn threads_named(_name: &str) -> usize {
    0
}

/// max-age=1 + ETag の表現を返し、条件付き要求には `delay` だけ待ってから 304 を返すオリジン。
fn slow_revalidating_origin(
    counter: Arc<AtomicUsize>,
    delay: Duration,
) -> (u16, thread::JoinHandle<()>) {
    start_origin(
        counter,
        Arc::new(move |req: &str, _n| {
            if req.contains("If-None-Match:") {
                // 再検証を重ならせるために、条件付き要求だけ遅らせる
                thread::sleep(delay);
                return b"HTTP/1.1 304 Not Modified\r\nETag: \"w1\"\r\nCache-Control: max-age=60\r\nConnection: close\r\n\r\n".to_vec();
            }
            let body = "worker pool body";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"w1\"\r\nCache-Control: max-age=1\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    )
}

/// 裏側の再検証は接続スレッドの置き場の中で走る (T11.3)。
///
/// 以前は再検証のたびに `thread::spawn` していたので、`PROXY_MAX_THREADS` (T10.5) の
/// **外**にスレッドが増えた。再検証が集中しても
///
/// - `Workers` の外に「revalidate」スレッドが 1 本も出ないこと
/// - 生きているスレッドが上限を超えないこと
///
/// を、burst の最中に細かく見張って確かめる。
#[test]
fn test_integration_background_revalidation_stays_inside_the_worker_pool() {
    const MAX_THREADS: usize = 4;
    const URLS: usize = 8;

    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        slow_revalidating_origin(Arc::clone(&counter), Duration::from_millis(300));
    let mut cfg = proxy_config();
    cfg.max_threads = MAX_THREADS;
    let (proxy_port, workers) = start_test_proxy_with_workers(cfg, cache_cfg("shp-it-revalpool"));
    let host = format!("127.0.0.1:{}", origin_port);
    let urls: Vec<String> = (0..URLS)
        .map(|i| format!("http://{}/pool{}", host, i))
        .collect();

    // まず全部を保存する (max-age=1)
    for url in &urls {
        let r = get_via_proxy(proxy_port, url, &host);
        assert!(r.ends_with("worker pool body"), "{}", r);
    }
    thread::sleep(Duration::from_millis(1200));
    let before_reval = json_number(&status_json(proxy_port), "background_revalidations");

    // burst の最中ずっと見張る
    let stop = Arc::new(AtomicBool::new(false));
    let max_live = Arc::new(AtomicUsize::new(0));
    let stray = Arc::new(AtomicUsize::new(0));
    let watcher = {
        let (stop, max_live, stray, workers) = (
            Arc::clone(&stop),
            Arc::clone(&max_live),
            Arc::clone(&stray),
            Arc::clone(&workers),
        );
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                max_live.fetch_max(workers.live_count(), Ordering::SeqCst);
                stray.fetch_max(threads_named("revalidate"), Ordering::SeqCst);
                thread::sleep(Duration::from_millis(2));
            }
        })
    };

    // 期限切れの表現を次々に叩く。1 本ずつでも、オリジンが 300 ms 待つので裏側の再検証は
    // 重なっていく (上限 4 のうち 1 本は要求を処理しているので、裏に回せるのは 3 本まで)
    for url in &urls {
        let r = get_via_proxy(proxy_port, url, &host);
        assert!(r.starts_with("HTTP/1.1 200 OK"), "{}", r);
        assert!(r.ends_with("worker pool body"), "{}", r);
    }
    // 裏で走っている再検証が終わるまで見張り続ける
    wait_until(
        || status_json(proxy_port).contains("\"revalidating\":0"),
        "background revalidations should finish",
    );
    stop.store(true, Ordering::SeqCst);
    watcher.join().expect("watcher thread");

    assert_eq!(
        stray.load(Ordering::SeqCst),
        0,
        "再検証が置き場の外でスレッドを起こしている"
    );
    assert!(
        max_live.load(Ordering::SeqCst) <= MAX_THREADS,
        "生きているスレッドが上限 {} を超えた: {}",
        MAX_THREADS,
        max_live.load(Ordering::SeqCst)
    );
    // 上限 4 に 8 本ぶつけたので、裏で走ったものと捨てたものの両方が出る
    let status = status_json(proxy_port);
    let done = json_number(&status, "background_revalidations") - before_reval;
    let dropped = json_number(&status, "revalidations_dropped");
    assert!(
        done >= 1,
        "空きがあるときは裏で走らせるはず: done={} dropped={}",
        done,
        dropped
    );
    assert_eq!(
        done + dropped,
        URLS as u64,
        "どの要求も「裏で走る」か「捨てる」のどちらか: done={} dropped={}",
        done,
        dropped
    );
}

/// `\"key\":N` を数として取り出す (テスト用の雑な取り出し)。
fn json_number(json: &str, key: &str) -> u64 {
    let needle = format!("\"{}\":", key);
    let at = json.find(&needle).unwrap_or_else(|| panic!("no {}", key)) + needle.len();
    json[at..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|d| d.parse().ok())
        .unwrap_or_else(|| panic!("not a number: {}", key))
}

/// 空いている接続スレッドが 1 本も無ければ、裏側の再検証は**待ち行列に積まず捨てる** (T11.3)。
///
/// 捨てても正しさは崩れない: その要求はそのまま同期の再検証に回り、クライアントは
/// 新しい表現を受け取る。捨てた回数は `/status` に出る。
#[test]
fn test_integration_background_revalidation_is_dropped_when_threads_are_capped() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        slow_revalidating_origin(Arc::clone(&counter), Duration::from_millis(0));
    let mut cfg = proxy_config();
    // 上限 1 = いま要求を処理しているスレッドで全部。裏の再検証に回せる空きは無い
    cfg.max_threads = 1;
    let proxy_port = start_test_proxy_with_cache(cfg, cache_cfg("shp-it-revaldrop"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/capped", host);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.ends_with("worker pool body"), "{}", first);
    thread::sleep(Duration::from_millis(1200));

    // 期限切れ直後。裏へ回せないので REFRESHING にはならず、同期で再検証して返す
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.starts_with("HTTP/1.1 200 OK"), "{}", second);
    assert!(second.ends_with("worker pool body"), "{}", second);
    assert!(
        !second.contains("REFRESHING"),
        "空きが無いのに裏へ回してはいけない: {}",
        second
    );

    let status = status_json(proxy_port);
    assert_eq!(
        json_number(&status, "revalidations_dropped"),
        1,
        "捨てた再検証が数えられていない: {}",
        status
    );
    assert_eq!(
        json_number(&status, "revalidating"),
        0,
        "捨てたのに「再検証中」の印が残っている: {}",
        status
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "オリジンへは最初の取得と同期の再検証の 2 回"
    );
}

#[test]
fn test_integration_slow_origin_gives_up_and_serves_stale() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(|_req, n| {
            if n > 1 {
                thread::sleep(Duration::from_secs(4));
            }
            let body = "slow body";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"s1\"\r\nCache-Control: max-age=0\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    );
    let mut cfg = cache_cfg("shp-it-slow");
    cfg.stale_wait = Duration::from_secs(1);
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cfg);
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/slow", host);

    get_via_proxy(proxy_port, &url, &host);
    // 期限切れ (max-age=0) → 同期再検証だが、1 秒待って応答が無ければ stale を返す
    let started = std::time::Instant::now();
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(
        second.contains("X-Cache: STALE from rust-http-proxy (memory)"),
        "{}",
        second
    );
    assert!(second.ends_with("slow body"), "{}", second);
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn test_integration_concurrent_misses_are_coalesced() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(|_req, _n| {
            thread::sleep(Duration::from_millis(700));
            let body = "coalesced body";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age=60\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    );
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-coalesce"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/big-asset", host);

    let handles: Vec<_> = (0..6)
        .map(|i| {
            let (url, host) = (url.clone(), host.clone());
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(i * 20));
                get_via_proxy(proxy_port, &url, &host)
            })
        })
        .collect();
    let responses: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for r in &responses {
        assert!(
            r.starts_with("HTTP/1.1 200 OK") && r.ends_with("coalesced body"),
            "{}",
            r
        );
    }
    // オリジンへ行くのは 1 本だけ、残りは合流 (COALESCED) か、その後の保存済み (HIT) から返る。
    // どちらになるかは到着の順番次第なので、まとめて数える (負荷の高い CI でも安定する)
    let from_cache = responses
        .iter()
        .filter(|r| r.contains("X-Cache: COALESCED") || r.contains("X-Cache: HIT"))
        .count();
    assert!(
        from_cache >= 5,
        "expected all but the leader to be served without touching the origin, got {}",
        from_cache
    );
    assert!(
        responses.iter().any(|r| r.contains("X-Cache: COALESCED")),
        "at least one request should have joined the in-flight fetch"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1, "origin fetched once");
}

/// 保存されない URL では、2 回目以降の同時ミスは合流しない (T11.9)。
/// 合流は「1 本が取ってきて保存し、待っていた側はキャッシュから受け取る」ための仕組みなので、
/// 保存されない応答では待つだけ損になる (起きてから結局自分でオリジンへ行く)。
#[test]
fn test_integration_uncacheable_misses_do_not_wait_for_each_other() {
    let counter = Arc::new(AtomicUsize::new(0));
    // オリジンが同時に何本抱えたかを数える。合流していれば leader の 1 本しか来ない
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (live, high) = (Arc::clone(&in_flight), Arc::clone(&peak));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(move |_req, _n| {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            high.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(400));
            live.fetch_sub(1, Ordering::SeqCst);
            let body = "uncacheable body";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    );
    let proxy_port =
        start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-no-coalesce-nostore"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/uncacheable", host);

    // 1 本目で「この URL は保存されない」と分かる (ここまでは合流の対象)
    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.ends_with("uncacheable body"), "{}", first);
    assert!(
        !first.contains("X-Cache"),
        "a miss has no X-Cache: {}",
        first
    );
    assert_eq!(peak.swap(0, Ordering::SeqCst), 1, "the first one is alone");

    // 2 回目以降の同時ミスは合流を通らないので、4 本とも同時にオリジンへ届く
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let (url, host) = (url.clone(), host.clone());
            thread::spawn(move || get_via_proxy(proxy_port, &url, &host))
        })
        .collect();
    let responses: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for r in &responses {
        assert!(
            r.starts_with("HTTP/1.1 200 OK") && r.ends_with("uncacheable body"),
            "{}",
            r
        );
        assert!(
            !r.contains("X-Cache"),
            "nothing is served from cache: {}",
            r
        );
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        5,
        "every request is forwarded"
    );
    // 合流していると leader の 1 本が終わってから残りが動くので、同時には 3 本までしか届かない
    assert_eq!(
        peak.load(Ordering::SeqCst),
        4,
        "all four reach the origin at the same time"
    );
}
