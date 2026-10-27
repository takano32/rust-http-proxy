//! クライアントの条件付き要求に対するオリジンの 304 でも、保存済みの表現を更新する (T.29.6)。
//!
//! 0.29.35 までは、期限切れの項目に `If-None-Match` で来た要求はそのままオリジンへ送り、返った 304
//! (`max-age=60`) を中継するだけだった。保存済みの表現は延ばされず、続く素の GET が古い先頭のまま
//! もう一度オリジンへ行っていた (RFC 9111 §4.3.4 は、304 で保存済みの応答を更新するよう求めている)。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

/// 最初の 200 は `max-age=0` + `ETag: "v1"` (保存するが、すぐ期限切れ)。そのあとの今の表現は
/// `current` で、`If-None-Match: <current>` には 304 (`max-age=60` と新しい `X-Rev: 2`)、
/// ほかの条件と素の GET には新しい 200 (`X-Rev: 3`) を返す。
fn origin(counter: Arc<AtomicUsize>, current: &'static str) -> u16 {
    start_origin(
        counter,
        Arc::new(move |req: &str, n| {
            if req.contains(&format!("If-None-Match: {}", current)) {
                return format!(
                    "HTTP/1.1 304 Not Modified\r\nETag: {}\r\nCache-Control: max-age=60\r\n\
                     X-Rev: 2\r\nConnection: close\r\n\r\n",
                    current
                )
                .into_bytes();
            }
            if n == 1 {
                return b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nCache-Control: max-age=0\r\n\
                    ETag: \"v1\"\r\nX-Rev: 1\r\nConnection: close\r\n\r\nbody"
                    .to_vec();
            }
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nCache-Control: max-age=60\r\n\
                 ETag: {}\r\nX-Rev: 3\r\nConnection: close\r\n\r\nbody3",
                current
            )
            .into_bytes()
        }),
    )
    .0
}

/// **受け入れ基準**: ETag が一致する 304 なら、続く素の GET が HIT (新しい先頭で)。
#[test]
fn test_integration_client_conditional_304_refreshes_the_stored_entry() {
    let counter = Arc::new(AtomicUsize::new(0));
    let port = origin(Arc::clone(&counter), "\"v1\"");
    let proxy_port =
        start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-client-cond-304"));
    let host = format!("127.0.0.1:{}", port);
    let url = format!("http://{}/r", host);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.ends_with("\r\n\r\nbody"), "{}", first);
    // クライアント自身の条件付き要求: オリジンの 304 がそのまま届く
    let cond = get_via_proxy_with(proxy_port, &url, &host, "If-None-Match: \"v1\"\r\n");
    assert!(cond.starts_with("HTTP/1.1 304 "), "{}", cond);
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    // 素の GET は保存から返る (延ばされた期限・304 で書き直した先頭)
    let plain = get_via_proxy(proxy_port, &url, &host);
    assert!(plain.contains("X-Cache: HIT"), "HIT でない: {}", plain);
    assert!(
        plain.contains("\r\nX-Rev: 2\r\n"),
        "先頭が古いまま: {}",
        plain
    );
    assert!(plain.ends_with("\r\n\r\nbody"), "{}", plain);
    assert_eq!(counter.load(Ordering::SeqCst), 2, "オリジンへ行った");
}

/// ETag が違う 304 (クライアントが持っている別の表現 `"v2"` の 304) では保存済みの `"v1"` を更新しない。
/// 続く素の GET は期限切れの `"v1"` を再検証し、オリジンの新しい 200 を受け取る。
#[test]
fn test_integration_client_conditional_304_with_another_etag_is_not_applied() {
    let counter = Arc::new(AtomicUsize::new(0));
    let port = origin(Arc::clone(&counter), "\"v2\"");
    let proxy_port =
        start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-client-cond-304-other"));
    let host = format!("127.0.0.1:{}", port);
    let url = format!("http://{}/r", host);

    let _ = get_via_proxy(proxy_port, &url, &host);
    let cond = get_via_proxy_with(proxy_port, &url, &host, "If-None-Match: \"v2\"\r\n");
    assert!(cond.starts_with("HTTP/1.1 304 "), "{}", cond);

    let plain = get_via_proxy(proxy_port, &url, &host);
    assert!(
        !plain.contains("X-Cache: HIT"),
        "更新してはいけない: {}",
        plain
    );
    assert!(!plain.contains("\r\nX-Rev: 2\r\n"), "{}", plain);
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}
