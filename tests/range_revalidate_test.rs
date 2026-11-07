//! `Range` 要求の再検証でオリジンが 206 を返しても、期限切れの全体の表現を捨てない (T34.143)。
//!
//! 0.34.x までは、期限切れの表現を持ったまま `Range` 要求をオリジンへ回して 206 を受けると、
//! 「新しい表現が届いたのに保存できない」と見て古い全体を消していた。206 は全体の置き換えでは
//! ないので、次の素の GET は保存済みの表現を再検証 (304) で延ばせるはずが、MISS になっていた。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod common;
use common::*;

const BODY_LEN: usize = 10 * 1024;

/// 1 本目は `max-age=1`・`ETag: "e"` の 10 KiB。`Range` 付きには 206 (先頭 10 B)、
/// `If-None-Match: "e"` には 304 (`max-age=60`)、それ以外は 200 の全体。受けた要求は `seen` に残す。
fn ranged_origin(counter: Arc<AtomicUsize>, seen: Arc<Mutex<Vec<String>>>) -> u16 {
    start_origin(
        counter,
        Arc::new(move |req, n| {
            seen.lock().unwrap().push(req.to_string());
            let body = "x".repeat(BODY_LEN);
            if n > 1 && req.contains("Range: bytes=0-9") {
                return format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/{}\r\nContent-Length: 10\r\n\
                     Cache-Control: max-age=1\r\nETag: \"e\"\r\nConnection: close\r\n\r\n{}",
                    BODY_LEN,
                    &body[..10]
                )
                .into_bytes();
            }
            if n > 1 && req.contains("If-None-Match: \"e\"") {
                return b"HTTP/1.1 304 Not Modified\r\nETag: \"e\"\r\nCache-Control: max-age=60\r\n\
                         Connection: close\r\n\r\n"
                    .to_vec();
            }
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age=1\r\nETag: \"e\"\r\n\
                 Connection: close\r\n\r\n{}",
                BODY_LEN, body
            )
            .into_bytes()
        }),
    )
    .0
}

#[test]
fn test_integration_a_206_on_revalidation_keeps_the_stale_full_representation() {
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = ranged_origin(Arc::clone(&counter), Arc::clone(&seen));
    // 猶予 0: 期限切れの表現を `REFRESHING` で配らず、オリジンへ回す
    let cfg = rust_http_proxy::cache::CacheConfig {
        grace: Duration::ZERO,
        ..cache_cfg("shp-it-range-reval")
    };
    // キャッシュの時計は手で進める (T35.1。TTL の切れを実時間で待たない)
    let clock = ManualClock::starting_now();
    let (proxy_port, _) = start_test_proxy_clocked(proxy_config(), cfg, clock.clone());
    let url = format!("http://127.0.0.1:{}/r", port);
    let host = format!("127.0.0.1:{}", port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.starts_with("HTTP/1.1 200 "), "{}", first);
    // `max-age=1` の期限を過ぎる (猶予 0 なのでオリジンへ回る)
    clock.advance(Duration::from_secs(2));

    let ranged = get_via_proxy_with(proxy_port, &url, &host, "Range: bytes=0-9\r\n");
    assert!(ranged.starts_with("HTTP/1.1 206 "), "{}", ranged);
    assert!(ranged.ends_with("xxxxxxxxxx"), "{}", ranged);
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    // 素の GET: 保存済みの全体を条件付きで再検証し (304)、保存から全体を返す
    let plain = get_via_proxy(proxy_port, &url, &host);
    assert!(plain.starts_with("HTTP/1.1 200 "), "{}", &plain[..200]);
    assert!(
        plain.contains("X-Cache: REVALIDATED"),
        "期限切れの全体が捨てられた: {}",
        &plain[..plain.find("\r\n\r\n").unwrap_or(plain.len())]
    );
    assert_eq!(plain.split_once("\r\n\r\n").unwrap().1.len(), BODY_LEN);
    assert_eq!(counter.load(Ordering::SeqCst), 3);
    let last = seen.lock().unwrap().last().cloned().unwrap();
    assert!(last.contains("If-None-Match: \"e\""), "{}", last);
}
