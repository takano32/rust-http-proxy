//! 再検証の 304 が返す利用者あての値を、他の利用者へ配らない (T.29.0)。
//!
//! 0.28.48 までは T.28.2 の `merge_not_modified` が 304 の `Set-Cookie` を保存済みの先頭へ写し、
//! 次の利用者の `REVALIDATED` と `HIT` (メモリにもディスクにも) に同じ `Set-Cookie` が付いていた。
//! 304 の `Vary: Cookie`・`private`・`no-store` も写したまま配り続けていた (TTL が 0 になるだけ)。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rust_http_proxy::cache::{CacheConfig, MIB};

mod common;
use common::*;

const BODY: &str = "shared body";
const SECRET: &str = "SECRET-OF-USER-A";

/// 保存は `max-age=0` + `ETag` (毎回同期で再検証する)。**最初の** 304 だけに `extra` を付け、
/// 2 回目からの 304 は `max-age=600` だけ。オリジンが受けた要求を `seen` に残す。
fn start_origin_with_extra_on_first_304(
    counter: Arc<AtomicUsize>,
    extra: &'static str,
    seen: Arc<Mutex<Vec<String>>>,
) -> u16 {
    let first_304 = Arc::new(AtomicUsize::new(0));
    let (port, _origin) = start_origin(
        counter,
        Arc::new(move |req, _| {
            seen.lock().unwrap().push(req.to_string());
            if req.contains("If-None-Match: \"v1\"") {
                let extra = if first_304.fetch_add(1, Ordering::SeqCst) == 0 {
                    extra
                } else {
                    "Cache-Control: max-age=600\r\n"
                };
                return format!(
                    "HTTP/1.1 304 Not Modified\r\n{}ETag: \"v1\"\r\nConnection: close\r\n\r\n",
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
    port
}

fn body_of(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map_or("", |(_, b)| b)
}

fn disk_only(dir: &str) -> CacheConfig {
    let dir = std::env::temp_dir().join(dir);
    let _ = std::fs::remove_dir_all(&dir);
    CacheConfig {
        default_ttl: Duration::from_secs(60),
        ..CacheConfig::fixed(0, 2048 * MIB, dir)
    }
}

/// 304 の `Set-Cookie` は再検証した利用者 (B) にだけ渡り、次の利用者 (C の HIT・D の再検証) と
/// 保存した先頭には入らない。
fn check_set_cookie(cache: CacheConfig, source: &str) {
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let extra = "Cache-Control: max-age=600\r\nSet-Cookie: session=SECRET-OF-USER-A; Path=/\r\n";
    let origin_port = start_origin_with_extra_on_first_304(Arc::clone(&counter), extra, seen);
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache);
    let url = format!("http://127.0.0.1:{}/c", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let a = get_via_proxy(proxy_port, &url, &host);
    assert_eq!(body_of(&a), BODY, "{}", a);
    let b = get_via_proxy(proxy_port, &url, &host);
    assert!(b.contains("X-Cache: REVALIDATED"), "{}", b);
    assert!(b.contains(SECRET), "再検証した本人には渡す: {}", b);
    assert_eq!(body_of(&b), BODY, "{}", b);
    // C: 304 の `max-age=600` の内なので HIT。他人の Set-Cookie が付いていない
    let c = get_via_proxy(proxy_port, &url, &host);
    assert!(
        c.contains(&format!("X-Cache: HIT from rust-http-proxy ({})", source)),
        "{}",
        c
    );
    assert!(!c.contains("Set-Cookie"), "他の利用者へ配った: {}", c);
    assert_eq!(body_of(&c), BODY);
    // D: `no-cache` で再検証させる (304 に Set-Cookie は無い)。保存した先頭にも残っていない
    let d = get_via_proxy_with(proxy_port, &url, &host, "Cache-Control: no-cache\r\n");
    assert!(d.contains("X-Cache: REVALIDATED"), "{}", d);
    assert!(!d.contains("Set-Cookie"), "保存した先頭に入っていた: {}", d);
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

/// **受け入れ基準 (T.29.0)**: `Set-Cookie`、メモリ (L1)。
#[test]
fn test_integration_304_set_cookie_reaches_only_the_requester_in_memory() {
    check_set_cookie(cache_cfg("shp-it-304-cookie-mem"), "memory");
}

/// **受け入れ基準 (T.29.0)**: `Set-Cookie`、ディスク (L2) だけの項目。
#[test]
fn test_integration_304_set_cookie_reaches_only_the_requester_on_disk() {
    check_set_cookie(disk_only("shp-it-304-cookie-disk"), "disk");
}

/// 304 が保存を禁じた (`extra`): 再検証した本人 (B) には配るが、項目は消えて次の利用者 (C) は
/// オリジンから取り直す (条件付きでない要求)。C の応答に 304 の `forbidden` は無い。
fn check_forbidding_304(cache: CacheConfig, extra: &'static str, forbidden: &str) {
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let origin_port =
        start_origin_with_extra_on_first_304(Arc::clone(&counter), extra, Arc::clone(&seen));
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache);
    let url = format!("http://127.0.0.1:{}/f", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let a = get_via_proxy(proxy_port, &url, &host);
    assert_eq!(body_of(&a), BODY, "{}", a);
    let b = get_via_proxy(proxy_port, &url, &host);
    assert!(b.contains("X-Cache: REVALIDATED"), "{}", b);
    assert!(b.contains(forbidden), "本人へは 304 のヘッダーごと: {}", b);
    assert_eq!(body_of(&b), BODY, "{}", b);
    let c = get_via_proxy(proxy_port, &url, &host);
    assert!(!c.contains(forbidden), "他の利用者へ配った: {}", c);
    assert!(!c.contains("X-Cache: HIT"), "{}", c);
    assert!(!c.contains("X-Cache: REVALIDATED"), "{}", c);
    assert_eq!(body_of(&c), BODY);
    assert_eq!(counter.load(Ordering::SeqCst), 3);
    let seen = seen.lock().unwrap();
    assert!(
        !seen[2].contains("If-None-Match"),
        "項目が消えていない (条件付きで取り直した): {}",
        seen[2]
    );
}

/// **受け入れ基準 (T.29.0)**: `Vary: Cookie`。
#[test]
fn test_integration_304_vary_cookie_removes_the_entry() {
    check_forbidding_304(
        cache_cfg("shp-it-304-vary-cookie"),
        "Vary: Cookie\r\n",
        "Vary: Cookie",
    );
}

/// **受け入れ基準 (T.29.0)**: `private` (ディスクの項目でも消える)。
#[test]
fn test_integration_304_private_removes_the_entry() {
    check_forbidding_304(
        disk_only("shp-it-304-private"),
        "Cache-Control: private, max-age=600\r\n",
        "private",
    );
}

/// **受け入れ基準 (T.29.0)**: `no-store`。
#[test]
fn test_integration_304_no_store_removes_the_entry() {
    check_forbidding_304(
        cache_cfg("shp-it-304-no-store"),
        "Cache-Control: no-store\r\n",
        "no-store",
    );
}

/// 裏の再検証 (stale-while-revalidate) の 304 が保存を禁じたら項目を消す。304 の `Set-Cookie` は
/// 誰にも渡らない (その要求は利用者のものではない)。
#[test]
fn test_integration_background_304_forbidding_storage_removes_the_entry() {
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_origin = Arc::clone(&seen);
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(move |req, _| {
            seen_origin.lock().unwrap().push(req.to_string());
            if req.contains("If-None-Match: \"v1\"") {
                return b"HTTP/1.1 304 Not Modified\r\nCache-Control: no-store\r\n\
                         Set-Cookie: session=SECRET-OF-USER-A\r\nETag: \"v1\"\r\n\
                         Connection: close\r\n\r\n"
                    .to_vec();
            }
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age=1\r\n\
                 ETag: \"v1\"\r\nConnection: close\r\n\r\n{}",
                BODY.len(),
                BODY
            )
            .into_bytes()
        }),
    );
    let proxy_port =
        start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-304-forbid-background"));
    let url = format!("http://127.0.0.1:{}/bg", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let a = get_via_proxy(proxy_port, &url, &host);
    assert_eq!(body_of(&a), BODY, "{}", a);
    std::thread::sleep(Duration::from_millis(2100));
    let b = get_via_proxy(proxy_port, &url, &host);
    assert!(b.contains("X-Cache: REFRESHING"), "{}", b);
    wait_until(
        || counter.load(Ordering::SeqCst) == 2,
        "background revalidation",
    );
    // 裏の 304 が項目を消すまで待つ: 次の要求は条件付きでなくオリジンへ行く
    let mut c = String::new();
    for _ in 0..50 {
        c = get_via_proxy(proxy_port, &url, &host);
        if counter.load(Ordering::SeqCst) >= 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!c.contains(SECRET), "{}", c);
    assert!(!c.contains("no-store"), "{}", c);
    assert_eq!(body_of(&c), BODY, "{}", c);
    let seen = seen.lock().unwrap();
    assert!(seen.len() >= 3, "{:?}", seen);
    assert!(
        !seen[2].contains("If-None-Match"),
        "項目が消えていない: {}",
        seen[2]
    );
}
