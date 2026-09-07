//! キャッシュの結合テスト (保存・再検証・古い表現の提供・範囲取得・無効化)。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicUsize, Ordering};

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
