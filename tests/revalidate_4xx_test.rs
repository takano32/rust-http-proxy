//! 裏の再検証がオリジンの 4xx (404・410) を受けたら古い表現を残さない (RFC 9111 §4.3.3。T.32.13)。
//!
//! 0.30.4 までは 200〜399 以外をすべて「stale を残す」にしていて、オリジンが 404 を返した後も
//! grace の間は古い 200 を `REFRESHING` で配り、配るたびに再検証を 1 本出していた。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

mod common;
use common::*;

/// 1 本目は `max-age=1`・`ETag` の 200 (`old`)、2 本目からは `later` を返すオリジン。
fn origin_then(counter: Arc<AtomicUsize>, later: &'static str) -> u16 {
    start_origin(
        counter,
        Arc::new(move |_, n| {
            if n == 1 {
                return b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nCache-Control: max-age=1\r\nETag: \"e\"\r\n\
                         Connection: close\r\n\r\nold"
                    .to_vec();
            }
            later.as_bytes().to_vec()
        }),
    )
    .0
}

/// 保存 → 期限切れ → grace 内の要求 (`REFRESHING`) で裏の再検証を 1 本出し、終わるまで待って 3 本目を返す。
fn third_after_background(later: &'static str, dir: &str) -> (String, Arc<AtomicUsize>) {
    let counter = Arc::new(AtomicUsize::new(0));
    let port = origin_then(Arc::clone(&counter), later);
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg(dir));
    let url = format!("http://127.0.0.1:{}/d", port);
    let host = format!("127.0.0.1:{}", port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.ends_with("old"), "{}", first);
    std::thread::sleep(Duration::from_millis(2100));
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.contains("X-Cache: REFRESHING"), "{}", second);
    wait_until(
        || counter.load(Ordering::SeqCst) >= 2,
        "background revalidation",
    );
    // 裏の仕事が応答を読んで置き換える・消すまで
    std::thread::sleep(Duration::from_millis(300));
    (get_via_proxy(proxy_port, &url, &host), counter)
}

#[test]
fn test_integration_background_revalidation_replaces_on_404() {
    let (third, counter) = third_after_background(
        "HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\nConnection: close\r\n\r\ngone",
        "shp-it-reval-404",
    );
    // 保存できる 404 (経験則の negative TTL) で置き換わっている: HIT で 404、オリジンへは 2 本のまま
    assert!(
        third.starts_with("HTTP/1.1 404 "),
        "古い 200 が残った: {}",
        third
    );
    assert!(third.contains("X-Cache: HIT"), "{}", third);
    assert!(third.ends_with("gone"), "{}", third);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_integration_background_revalidation_removes_on_unstorable_4xx() {
    let (third, counter) = third_after_background(
        "HTTP/1.1 403 Forbidden\r\nContent-Length: 2\r\nConnection: close\r\n\r\nno",
        "shp-it-reval-403",
    );
    // 保存できない 403: 項目を消すので、次は普通のミスでオリジンの 403
    assert!(
        third.starts_with("HTTP/1.1 403 "),
        "古い 200 が残った: {}",
        third
    );
    assert!(!third.contains("X-Cache"), "{}", third);
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

#[test]
fn test_integration_background_revalidation_keeps_stale_on_5xx() {
    let (third, _) = third_after_background(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "shp-it-reval-503",
    );
    // 5xx では今までどおり古い表現を残す (grace の間はまた `REFRESHING`)
    assert!(third.starts_with("HTTP/1.1 200 "), "{}", third);
    assert!(third.ends_with("old"), "{}", third);
}
