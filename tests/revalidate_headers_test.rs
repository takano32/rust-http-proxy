//! 304 で届いた新しいヘッダーで保存済みの応答を書き直す (RFC 9111 §3.2・§4.3.4。T28.2)。
//!
//! 0.27.8 までは、再検証の 304 が `Cache-Control: max-age=600` と `X-Ver: 2` を返しても、
//! 内部の期限 (TTL) だけ延ばし、配る先頭は古いまま (`max-age=0`・`X-Ver: 1`) だった。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// **受け入れ基準 (T28.2)**: メモリ (L1) の項目。
#[test]
fn test_integration_304_rewrites_the_stored_head_in_memory() {
    check_sync_revalidation(cache_cfg("shp-it-304-head-mem"), "memory");
}

/// ディスク (L2) だけの項目 (メモリの上限 0)。書き直したファイルから HIT になる。
#[test]
fn test_integration_304_rewrites_the_stored_head_on_disk() {
    let dir = fresh_dir("shp-it-304-head-disk");
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
    // キャッシュの時計は手で進める (T35.1。TTL の切れを実時間で待たない)
    let clock = ManualClock::starting_now();
    let (proxy_port, _) = start_test_proxy_clocked(
        proxy_config(),
        cache_cfg("shp-it-304-head-background"),
        clock.clone(),
    );
    let url = format!("http://127.0.0.1:{}/r", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.contains("X-Ver: 1"), "{}", first);
    clock.advance(Duration::from_secs(2));
    // 期限切れ直後: 古い表現をすぐ返し、裏で再検証する
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.contains("X-Cache: REFRESHING"), "{}", second);
    // **裏の再検証が保存し終わるまで値で待つ** (T38.2)。オリジンが 304 を受けた (`counter` が 2) だけ
    // では、まだ先頭を書き直していない (その間の要求は古い表現の `REFRESHING` になる。T38.10)。
    // 書き直したあとで数が増える `background_revalidations` と、印が消えた `revalidating` を見る
    wait_until(
        || {
            let status = status_json(proxy_port);
            json::uint(&status, "background_revalidations") == 1
                && json::uint(&status, "revalidating") == 0
        },
        "background revalidation",
    );
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    // 裏の 304 が書き直した先頭が配られる
    let third = get_via_proxy(proxy_port, &url, &host);
    assert!(third.contains("X-Cache: HIT"), "{}", third);
    assert!(third.contains("Cache-Control: max-age=600"), "{}", third);
    assert!(third.contains("X-Ver: 2"), "{}", third);
    assert_eq!(body_of(&third), BODY);
}

/// 裏で再検証している間に来た同じキーの要求は、古い表現を `REFRESHING` で返し、オリジンへ
/// もう 1 本出さない (T38.10)。前は同期の再検証に回り、`REVALIDATED` でオリジンへ 2 本目を出していた。
#[test]
fn test_integration_request_during_background_revalidation_gets_the_stale_copy() {
    let counter = Arc::new(AtomicUsize::new(0));
    // 304 は `release` が真になるまで返さない (裏の再検証を「進行中」に留めておく)
    let release = Arc::new(AtomicBool::new(false));
    let gate = Arc::clone(&release);
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(move |req, _| {
            if req.contains("If-None-Match: \"v1\"") {
                // 待つのは最大 10 秒 (直す前の形なら 3 本目がここで待たされ、`REVALIDATED` で落ちる)
                for _ in 0..2000 {
                    if gate.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                return b"HTTP/1.1 304 Not Modified\r\nCache-Control: max-age=600\r\nX-Ver: 2\r\n\
                         ETag: \"v1\"\r\nConnection: close\r\n\r\n"
                    .to_vec();
            }
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age=1\r\nX-Ver: 1\r\n\
                 ETag: \"v1\"\r\nConnection: close\r\n\r\n{}",
                BODY.len(),
                BODY
            )
            .into_bytes()
        }),
    );
    let clock = ManualClock::starting_now();
    let (proxy_port, _) = start_test_proxy_clocked(
        proxy_config(),
        cache_cfg("shp-it-304-during-background"),
        clock.clone(),
    );
    let url = format!("http://127.0.0.1:{}/r", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.contains("X-Ver: 1"), "{}", first);
    clock.advance(Duration::from_secs(2));
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.contains("X-Cache: REFRESHING"), "{}", second);
    // 裏の再検証がオリジンに届き、304 を待たされている
    wait_until(
        || counter.load(Ordering::SeqCst) == 2,
        "background revalidation reaches the origin",
    );
    assert_eq!(json::uint(&status_json(proxy_port), "revalidating"), 1);
    // その間の要求は古い表現をすぐ返す (オリジンへは行かない)
    let third = get_via_proxy(proxy_port, &url, &host);
    assert!(third.contains("X-Cache: REFRESHING"), "{}", third);
    assert!(third.contains("X-Ver: 1"), "{}", third);
    assert_eq!(body_of(&third), BODY);
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    release.store(true, Ordering::SeqCst);
    wait_until(
        || {
            let status = status_json(proxy_port);
            json::uint(&status, "background_revalidations") == 1
                && json::uint(&status, "revalidating") == 0
        },
        "background revalidation",
    );
    let fourth = get_via_proxy(proxy_port, &url, &host);
    assert!(fourth.contains("X-Cache: HIT"), "{}", fourth);
    assert!(fourth.contains("X-Ver: 2"), "{}", fourth);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// 応答の `Age` の値 (無ければ `None`)。
fn age_of(resp: &str) -> Option<u64> {
    resp.split("\r\n")
        .find_map(|l| l.strip_prefix("Age: "))
        .and_then(|v| v.trim().parse().ok())
}

/// 保存 (`max-age=0`) から 3 秒たってから再検証し、REVALIDATED の応答の `Age` が 0〜1 であること
/// (T28.34)。直す前は古い `stored_at` から数えて 3 だった。`bare_304` なら 304 に写す
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
    // キャッシュの時計は手で進める (T35.1。TTL の切れを実時間で待たない)
    let clock = ManualClock::starting_now();
    let (proxy_port, _) = start_test_proxy_clocked(proxy_config(), cache, clock.clone());
    let url = format!("http://127.0.0.1:{}/age", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert_eq!(body_of(&first), BODY, "{}", first);
    clock.advance(Duration::from_secs(3));
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

/// **受け入れ基準 (T28.34)**: メモリ (L1) の項目、304 のヘッダーで先頭を書き直す経路。
#[test]
fn test_integration_revalidated_age_counts_from_the_refresh_in_memory() {
    check_age_after_revalidation(cache_cfg("shp-it-304-age-mem"), "memory", false);
}

/// ディスク (L2) だけの項目 (メモリの上限 0)。
#[test]
fn test_integration_revalidated_age_counts_from_the_refresh_on_disk() {
    let dir = fresh_dir("shp-it-304-age-disk");
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
