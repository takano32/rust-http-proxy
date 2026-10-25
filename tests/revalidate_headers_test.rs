//! 304 で届いた新しいヘッダーで保存済みの応答を書き直す (RFC 9111 §3.2・§4.3.4。T.28.2)。
//!
//! 0.27.8 までは、再検証の 304 が `Cache-Control: max-age=600` と `X-Ver: 2` を返しても、
//! 内部の期限 (TTL) だけ延ばし、配る先頭は古いまま (`max-age=0`・`X-Ver: 1`) だった。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rust_http_proxy::cache::{CacheConfig, MIB};

mod common;
use common::*;

const BODY: &str = "stored body, byte for byte";

/// 1 回目は `max-age` が `first_max_age` の 200 (`X-Ver: 1`)、`If-None-Match: "v1"` には
/// `max-age=600` と `X-Ver: 2` の 304 を返すオリジン。
fn start_origin_with_new_headers_on_304(
    counter: Arc<AtomicUsize>,
    first_max_age: u32,
) -> (u16, std::thread::JoinHandle<()>) {
    start_origin(
        counter,
        Arc::new(move |req, _| {
            if req.contains("If-None-Match: \"v1\"") {
                return b"HTTP/1.1 304 Not Modified\r\nCache-Control: max-age=600\r\nX-Ver: 2\r\n\
                         ETag: \"v1\"\r\nConnection: close\r\n\r\n"
                    .to_vec();
            }
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age={}\r\nX-Ver: 1\r\n\
                 ETag: \"v1\"\r\nConnection: close\r\n\r\n{}",
                BODY.len(),
                first_max_age,
                BODY
            )
            .into_bytes()
        }),
    )
}

fn body_of(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map_or("", |(_, b)| b)
}

/// 同期の再検証 (保存は `max-age=0`)。304 の応答と、その後の HIT が新しい先頭を返し、本文は同じ。
fn check_sync_revalidation(cache: CacheConfig, source: &str) {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin_with_new_headers_on_304(Arc::clone(&counter), 0);
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache);
    let url = format!("http://127.0.0.1:{}/r", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.contains("X-Ver: 1"), "{}", first);
    assert_eq!(body_of(&first), BODY);

    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(
        second.contains(&format!(
            "X-Cache: REVALIDATED from rust-http-proxy ({})",
            source
        )),
        "{}",
        second
    );
    assert!(second.contains("Cache-Control: max-age=600"), "{}", second);
    assert!(second.contains("X-Ver: 2"), "{}", second);
    assert!(!second.contains("X-Ver: 1"), "{}", second);
    assert!(!second.contains("max-age=0"), "{}", second);
    assert_eq!(body_of(&second), BODY, "本文が変わった");

    // 書き直した先頭が保存されている: 次は HIT で、新しい先頭のまま
    let third = get_via_proxy(proxy_port, &url, &host);
    assert!(
        third.contains(&format!("X-Cache: HIT from rust-http-proxy ({})", source)),
        "{}",
        third
    );
    assert!(third.contains("Cache-Control: max-age=600"), "{}", third);
    assert!(third.contains("X-Ver: 2"), "{}", third);
    assert!(!third.contains("X-Ver: 1"), "{}", third);
    assert_eq!(body_of(&third), BODY, "本文が変わった");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// **受け入れ基準 (T.28.2)**: メモリ (L1) の項目。
#[test]
fn test_integration_304_rewrites_the_stored_head_in_memory() {
    check_sync_revalidation(cache_cfg("shp-it-304-head-mem"), "memory");
}

/// ディスク (L2) だけの項目 (メモリの上限 0)。書き直したファイルから HIT になる。
#[test]
fn test_integration_304_rewrites_the_stored_head_on_disk() {
    let dir = std::env::temp_dir().join("shp-it-304-head-disk");
    let _ = std::fs::remove_dir_all(&dir);
    let cache = CacheConfig {
        default_ttl: Duration::from_secs(60),
        ..CacheConfig::fixed(0, 2048 * MIB, dir)
    };
    check_sync_revalidation(cache, "disk");
}

/// 裏の再検証 (stale-while-revalidate) でも書き直す。
#[test]
fn test_integration_background_304_rewrites_the_stored_head() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin_with_new_headers_on_304(Arc::clone(&counter), 1);
    let proxy_port =
        start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-304-head-background"));
    let url = format!("http://127.0.0.1:{}/r", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.contains("X-Ver: 1"), "{}", first);
    std::thread::sleep(Duration::from_millis(2100));
    // 期限切れ直後: 古い表現をすぐ返し、裏で再検証する
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.contains("X-Cache: REFRESHING"), "{}", second);
    wait_until(
        || counter.load(Ordering::SeqCst) == 2,
        "background revalidation",
    );
    // 裏の 304 が書き直した先頭が配られる
    let mut third = String::new();
    for _ in 0..50 {
        third = get_via_proxy(proxy_port, &url, &host);
        if third.contains("X-Ver: 2") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(third.contains("X-Cache: HIT"), "{}", third);
    assert!(third.contains("Cache-Control: max-age=600"), "{}", third);
    assert!(third.contains("X-Ver: 2"), "{}", third);
    assert_eq!(body_of(&third), BODY);
}

/// 応答の `Age` の値 (無ければ `None`)。
fn age_of(resp: &str) -> Option<u64> {
    resp.split("\r\n")
        .find_map(|l| l.strip_prefix("Age: "))
        .and_then(|v| v.trim().parse().ok())
}

/// 保存 (`max-age=0`) から 3 秒たってから再検証し、REVALIDATED の応答の `Age` が 0〜1 であること
/// (T.28.34)。直す前は古い `stored_at` から数えて 3 だった。`bare_304` なら 304 に写す
/// ヘッダーが無い (先頭を書き直さず期限だけ延ばす経路)。
fn check_age_after_revalidation(cache: CacheConfig, source: &str, bare_304: bool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(move |req, _| {
            if req.contains("If-None-Match: \"v1\"") {
                let extra = if bare_304 {
                    ""
                } else {
                    "Cache-Control: max-age=600\r\nETag: \"v1\"\r\n"
                };
                return format!(
                    "HTTP/1.1 304 Not Modified\r\n{}Connection: close\r\n\r\n",
                    extra
                )
                .into_bytes();
            }
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age=0\r\n\
                 ETag: \"v1\"\r\nConnection: close\r\n\r\n{}",
                BODY.len(),
                BODY
            )
            .into_bytes()
        }),
    );
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache);
    let url = format!("http://127.0.0.1:{}/age", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert_eq!(body_of(&first), BODY, "{}", first);
    std::thread::sleep(Duration::from_secs(3));
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(
        second.contains(&format!(
            "X-Cache: REVALIDATED from rust-http-proxy ({})",
            source
        )),
        "{}",
        second
    );
    let age = age_of(&second).unwrap_or_else(|| panic!("Age が無い: {}", second));
    assert!(age <= 1, "再検証した直後の Age が {}: {}", age, second);
    assert_eq!(body_of(&second), BODY, "本文が変わった");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// **受け入れ基準 (T.28.34)**: メモリ (L1) の項目、304 のヘッダーで先頭を書き直す経路。
#[test]
fn test_integration_revalidated_age_counts_from_the_refresh_in_memory() {
    check_age_after_revalidation(cache_cfg("shp-it-304-age-mem"), "memory", false);
}

/// ディスク (L2) だけの項目 (メモリの上限 0)。
#[test]
fn test_integration_revalidated_age_counts_from_the_refresh_on_disk() {
    let dir = std::env::temp_dir().join("shp-it-304-age-disk");
    let _ = std::fs::remove_dir_all(&dir);
    let cache = CacheConfig {
        default_ttl: Duration::from_secs(60),
        ..CacheConfig::fixed(0, 2048 * MIB, dir)
    };
    check_age_after_revalidation(cache, "disk", false);
}

/// 304 に写すヘッダーが無い (期限だけ延ばす) 経路でも同じ。
#[test]
fn test_integration_revalidated_age_counts_from_the_refresh_without_new_headers() {
    check_age_after_revalidation(cache_cfg("shp-it-304-age-bare"), "memory", true);
}
