//! 絶対 URI のスキームの大小 (T.28.33): `HTTP://` と `Http://` も `http://` と同じに読む
//! (RFC 9110 §4.2.3: スキームは大小を区別しない)。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

mod common;
use common::*;

/// パスが `/s` なら保存できる (`max-age=600`)、ほかは `no-store` の 200 (本文 `ok`) を返し、
/// 届いた要求の全文を `seen` に残すオリジン。
fn recording_origin(counter: Arc<AtomicUsize>, seen: Arc<Mutex<Vec<String>>>) -> u16 {
    start_origin(
        counter,
        Arc::new(move |req, _| {
            seen.lock().unwrap().push(req.to_string());
            let cc = if req.starts_with("GET /s ") {
                "max-age=600"
            } else {
                "no-store"
            };
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nCache-Control: {}\r\nConnection: close\r\n\r\nok",
                cc
            )
            .into_bytes()
        }),
    )
    .0
}

/// 応答から状態行と本文だけを取り出す (`Date` などの行は要求ごとに変わりうる)。
fn status_and_body(resp: &str) -> (&str, &str) {
    let status = resp.split("\r\n").next().unwrap_or("");
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body)
}

/// `HTTP://` と `Http://` の要求は、小文字と同じキャッシュの項目に当たる。
#[test]
fn test_integration_upper_case_scheme_hits_the_same_entry() {
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = recording_origin(Arc::clone(&counter), Arc::clone(&seen));
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-scheme-case"));
    let host = format!("localhost:{}", port);

    let first = get_via_proxy(proxy_port, &format!("http://localhost:{}/s", port), &host);
    assert!(first.starts_with("HTTP/1.1 200 "), "{}", first);
    assert!(!first.contains("X-Cache"), "{}", first);
    for scheme in ["HTTP", "Http", "hTtP"] {
        let r = get_via_proxy(
            proxy_port,
            &format!("{}://LOCALHOST:{}/s", scheme, port),
            &host,
        );
        assert!(r.starts_with("HTTP/1.1 200 "), "{}: {}", scheme, r);
        assert!(r.contains("X-Cache: HIT"), "{}: {}", scheme, r);
        assert_eq!(status_and_body(&r), status_and_body(&first), "{}", scheme);
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

/// 保存しない応答でも、大文字のスキームの要求は小文字と同じ要求としてオリジンへ届き、同じ応答が返る。
#[test]
fn test_integration_upper_case_scheme_reaches_the_origin_as_lower_case() {
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = recording_origin(Arc::clone(&counter), Arc::clone(&seen));
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", port);

    let mut responses = Vec::new();
    for scheme in ["http", "HTTP", "Http"] {
        let r = get_via_proxy(
            proxy_port,
            &format!("{}://127.0.0.1:{}/u?q=1", scheme, port),
            &host,
        );
        assert!(r.starts_with("HTTP/1.1 200 "), "{}: {}", scheme, r);
        responses.push(r);
    }
    assert_eq!(counter.load(Ordering::SeqCst), 3);
    for r in &responses[1..] {
        assert_eq!(status_and_body(r), status_and_body(&responses[0]));
    }
    // オリジンが受け取ったバイト列は 3 本とも同じ (小文字の要求のバイト列も今までどおり)
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(
        seen[0].starts_with("GET /u?q=1 HTTP/1.1\r\n"),
        "{}",
        seen[0]
    );
    assert_eq!(seen[1], seen[0]);
    assert_eq!(seen[2], seen[0]);
}
