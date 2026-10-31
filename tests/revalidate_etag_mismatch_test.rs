//! キャッシュ自身の再検証で、`ETag` が違う 304 を受けたら保存済みを延命しない (RFC 9111 §4.3.4。T.32.9)。
//!
//! 0.30.4 までは、`ETag: "e"` の保存済みの再検証にオリジンが `304, ETag "other", max-age=600` を返すと、
//! 古い本文に `ETag: "other"` を付けて 10 分配り続けた (同期・裏の両方)。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

mod common;
use common::*;

/// 1 本目は `ETag: "e"` の `old` (`max-age` は `first_max_age`)、条件付きの要求には `ETag: "other"` の 304、
/// 2 本目以降の条件なしの要求には `ETag: "other"` の `new` (`max-age=600`) を返すオリジン。
fn origin_switching_representation(counter: Arc<AtomicUsize>, first_max_age: u32) -> u16 {
    start_origin(
        counter,
        Arc::new(move |req, n| {
            if n == 1 {
                return format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nCache-Control: max-age={}\r\nETag: \"e\"\r\n\
                     Connection: close\r\n\r\nold",
                    first_max_age
                )
                .into_bytes();
            }
            if req.contains("If-None-Match:") {
                return b"HTTP/1.1 304 Not Modified\r\nETag: \"other\"\r\nCache-Control: max-age=600\r\n\
                         Connection: close\r\n\r\n"
                    .to_vec();
            }
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nCache-Control: max-age=600\r\nETag: \"other\"\r\n\
              Connection: close\r\n\r\nnew"
                .to_vec()
        }),
    )
    .0
}

fn body_of(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map_or("", |(_, b)| b)
}

/// 同期の再検証 (保存は `max-age=0`): 違う `ETag` の 304 で項目を消し、条件なしで取り直した新しい 200 を返す。
#[test]
fn test_integration_sync_revalidation_ignores_a_304_for_another_etag() {
    let counter = Arc::new(AtomicUsize::new(0));
    let port = origin_switching_representation(Arc::clone(&counter), 0);
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-etag-sync"));
    let url = format!("http://127.0.0.1:{}/s", port);
    let host = format!("127.0.0.1:{}", port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert_eq!(body_of(&first), "old", "{}", first);
    // 304 (`"other"`) → 取り直し (200 `new`) で、オリジンへは 2 本
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.starts_with("HTTP/1.1 200 "), "{}", second);
    assert_eq!(
        body_of(&second),
        "new",
        "古い本文に新しい ETag を付けた: {}",
        second
    );
    assert!(!second.contains("REVALIDATED"), "{}", second);
    assert_eq!(counter.load(Ordering::SeqCst), 3);
    // 取り直した表現が保存されている
    let third = get_via_proxy(proxy_port, &url, &host);
    assert!(third.contains("X-Cache: HIT"), "{}", third);
    assert_eq!(body_of(&third), "new", "{}", third);
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

/// 裏の再検証 (保存は `max-age=1`、期限切れの後の grace 内): 違う `ETag` の 304 で項目を消す。
/// 次の要求は普通のミスで新しい 200。
#[test]
fn test_integration_background_revalidation_ignores_a_304_for_another_etag() {
    let counter = Arc::new(AtomicUsize::new(0));
    let port = origin_switching_representation(Arc::clone(&counter), 1);
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-etag-bg"));
    let url = format!("http://127.0.0.1:{}/b", port);
    let host = format!("127.0.0.1:{}", port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert_eq!(body_of(&first), "old", "{}", first);
    std::thread::sleep(Duration::from_millis(2100));
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.contains("X-Cache: REFRESHING"), "{}", second);
    assert_eq!(body_of(&second), "old", "{}", second);
    wait_until(
        || counter.load(Ordering::SeqCst) >= 2,
        "background revalidation",
    );
    // 裏の仕事が項目を消し終えるのを待つ (304 を受けてから消すまで)
    std::thread::sleep(Duration::from_millis(300));
    let third = get_via_proxy(proxy_port, &url, &host);
    assert!(!third.contains("X-Cache"), "{}", third);
    assert_eq!(
        body_of(&third),
        "new",
        "古い本文に新しい ETag を付けた: {}",
        third
    );
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}
