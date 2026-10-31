//! 要求に `Cookie` があり、応答に明示の鮮度も `public` も無ければ保存しない (T.32.35)。
//!
//! 0.30.4 までは鮮度の手がかりの無い 200 に既定 TTL を当てていたので、`Cookie: user=alice` の
//! `you are alice` を `user=bob` が HIT で受けていた。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

/// 要求の `Cookie` の値をそのまま本文に返すオリジン。`extra` は足す応答ヘッダー。
fn whoami_origin(counter: Arc<AtomicUsize>, extra: &'static str) -> u16 {
    start_origin(
        counter,
        Arc::new(move |req, _| {
            let who = req
                .lines()
                .find_map(|l| l.strip_prefix("Cookie: user="))
                .unwrap_or("nobody")
                .trim();
            let body = format!("you are {}", who);
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
                body.len(),
                extra,
                body
            )
            .into_bytes()
        }),
    )
    .0
}

fn body_of(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map_or("", |(_, b)| b)
}

#[test]
fn test_integration_cookie_request_without_explicit_freshness_is_not_stored() {
    let counter = Arc::new(AtomicUsize::new(0));
    let port = whoami_origin(Arc::clone(&counter), "");
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-cookie"));
    let url = format!("http://127.0.0.1:{}/whoami", port);
    let host = format!("127.0.0.1:{}", port);

    let alice = get_via_proxy_with(proxy_port, &url, &host, "Cookie: user=alice\r\n");
    assert_eq!(body_of(&alice), "you are alice", "{}", alice);
    let bob = get_via_proxy_with(proxy_port, &url, &host, "Cookie: user=bob\r\n");
    assert!(!bob.contains("X-Cache"), "{}", bob);
    assert_eq!(body_of(&bob), "you are bob", "他人の本文: {}", bob);
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    // `Cookie` の無い要求の経験則の保存は変わらない
    let anon = get_via_proxy(proxy_port, &url, &host);
    assert_eq!(body_of(&anon), "you are nobody", "{}", anon);
    let again = get_via_proxy(proxy_port, &url, &host);
    assert!(again.contains("X-Cache: HIT"), "{}", again);
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

/// 明示の `max-age` (と `public`) を持つ応答は、要求に `Cookie` があっても今までどおり保存して HIT。
#[test]
fn test_integration_cookie_request_with_explicit_freshness_is_stored() {
    for (dir, extra) in [
        ("shp-it-cookie-maxage", "Cache-Control: max-age=600\r\n"),
        ("shp-it-cookie-public", "Cache-Control: public\r\n"),
    ] {
        let counter = Arc::new(AtomicUsize::new(0));
        let port = whoami_origin(Arc::clone(&counter), extra);
        let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg(dir));
        let url = format!("http://127.0.0.1:{}/shared", port);
        let host = format!("127.0.0.1:{}", port);
        let first = get_via_proxy_with(proxy_port, &url, &host, "Cookie: user=alice\r\n");
        assert!(!first.contains("X-Cache"), "{}: {}", extra, first);
        let second = get_via_proxy_with(proxy_port, &url, &host, "Cookie: user=bob\r\n");
        assert!(second.contains("X-Cache: HIT"), "{}: {}", extra, second);
        assert_eq!(counter.load(Ordering::SeqCst), 1, "{}", extra);
    }
}
