//! キャッシュの小物 (T.28.10): ホスト名の大小・応答の `Via`・要求の `Cache-Control`・壊れた `max-age`。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

/// `extra` を足した 200 (本文 `ok`) を返し、要求の数を数えるオリジン。
fn origin_with(counter: Arc<AtomicUsize>, extra: &'static str) -> u16 {
    start_origin(
        counter,
        Arc::new(move |_, _| {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n{}Connection: close\r\n\r\nok",
                extra
            )
            .into_bytes()
        }),
    )
    .0
}

/// **(a)** `http://LOCALHOST:P/x` は `http://localhost:P/x` の項目に当たる (スキームとホストの大小は区別しない)。
#[test]
fn test_integration_host_case_hits_the_same_entry() {
    let counter = Arc::new(AtomicUsize::new(0));
    let port = origin_with(Arc::clone(&counter), "Cache-Control: max-age=600\r\n");
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-host-case"));

    let first = get_via_proxy(
        proxy_port,
        &format!("http://localhost:{}/x", port),
        &format!("localhost:{}", port),
    );
    assert!(first.starts_with("HTTP/1.1 200 "), "{}", first);
    assert!(!first.contains("X-Cache"), "{}", first);
    let second = get_via_proxy(
        proxy_port,
        &format!("http://LOCALHOST:{}/x", port),
        &format!("LOCALHOST:{}", port),
    );
    assert!(second.contains("X-Cache: HIT"), "{}", second);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    // パスの大小は区別する
    let third = get_via_proxy(
        proxy_port,
        &format!("http://localhost:{}/X", port),
        &format!("localhost:{}", port),
    );
    assert!(!third.contains("X-Cache"), "{}", third);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// **(b)** 応答に自分の `Via` を足す (オリジンの `Via` は残して後ろに 1 行)。HIT の応答にも付く。
#[test]
fn test_integration_responses_carry_our_via() {
    let counter = Arc::new(AtomicUsize::new(0));
    let port = origin_with(
        Arc::clone(&counter),
        "Cache-Control: max-age=600\r\nETag: \"e1\"\r\nVia: 1.1 upstream\r\n",
    );
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-resp-via"));
    let url = format!("http://127.0.0.1:{}/v", port);
    let host = format!("127.0.0.1:{}", port);
    let ours = format!("\r\nVia: {}\r\n", rust_http_proxy::via::token());

    let miss = get_via_proxy(proxy_port, &url, &host);
    assert!(miss.contains("\r\nVia: 1.1 upstream\r\n"), "{}", miss);
    assert_eq!(miss.matches(&ours).count(), 1, "{}", miss);
    let hit = get_via_proxy(proxy_port, &url, &host);
    assert!(hit.contains("X-Cache: HIT"), "{}", hit);
    assert!(hit.contains("\r\nVia: 1.1 upstream\r\n"), "{}", hit);
    // 保存した先頭に自分の `Via` は入っていない (配るたびに 1 行だけ)
    assert_eq!(hit.matches(&ours).count(), 1, "{}", hit);
    // キャッシュが返す 304 にも
    let not_modified = get_via_proxy_with(proxy_port, &url, &host, "If-None-Match: \"e1\"\r\n");
    assert!(
        not_modified.starts_with("HTTP/1.1 304 "),
        "{}",
        not_modified
    );
    assert_eq!(not_modified.matches(&ours).count(), 1, "{}", not_modified);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

/// **(c)** 要求の `max-age=N` (> 0)・`min-fresh=N` は満たさなければ再検証し、`only-if-cached` で
/// 保存が無ければオリジンへ行かずに 504。
#[test]
fn test_integration_request_cache_control_is_honoured() {
    let counter = Arc::new(AtomicUsize::new(0));
    // `Age: 10` で、保存した時点で 10 秒たっている表現にする (`max-age=600` なので残り 590 秒)
    let port = origin_with(
        Arc::clone(&counter),
        "Cache-Control: max-age=600\r\nAge: 10\r\n",
    );
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-req-cc"));
    let url = format!("http://127.0.0.1:{}/c", port);
    let host = format!("127.0.0.1:{}", port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.starts_with("HTTP/1.1 200 "), "{}", first);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    // ふつうの要求と、満たしている指示は HIT
    for extra in [
        "",
        "Cache-Control: max-age=60\r\n",
        "Cache-Control: min-fresh=100\r\n",
    ] {
        let r = get_via_proxy_with(proxy_port, &url, &host, extra);
        assert!(r.contains("X-Cache: HIT"), "{:?}: {}", extra, r);
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    // 10 秒たった表現は `max-age=5` を満たさない → オリジンへ
    let r = get_via_proxy_with(proxy_port, &url, &host, "Cache-Control: max-age=5\r\n");
    assert!(!r.contains("X-Cache: HIT"), "{}", r);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    // 残り 590 秒は `min-fresh=1000` を満たさない → オリジンへ
    let r = get_via_proxy_with(proxy_port, &url, &host, "Cache-Control: min-fresh=1000\r\n");
    assert!(!r.contains("X-Cache: HIT"), "{}", r);
    assert_eq!(counter.load(Ordering::SeqCst), 3);

    // `only-if-cached`: 保存があれば HIT、無ければ 504 でオリジンに届かない
    let r = get_via_proxy_with(proxy_port, &url, &host, "Cache-Control: only-if-cached\r\n");
    assert!(r.contains("X-Cache: HIT"), "{}", r);
    let missing = format!("http://127.0.0.1:{}/nothing-here", port);
    let r = get_via_proxy_with(
        proxy_port,
        &missing,
        &host,
        "Cache-Control: only-if-cached\r\n",
    );
    assert!(r.starts_with("HTTP/1.1 504 "), "{}", r);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        3,
        "only-if-cached でオリジンへ行った"
    );
}

/// **(d)** 応答の `max-age=abc` は既定 TTL で新鮮にしない (古い = 保存しない)。
#[test]
fn test_integration_unreadable_max_age_is_not_fresh() {
    let counter = Arc::new(AtomicUsize::new(0));
    let port = origin_with(Arc::clone(&counter), "Cache-Control: max-age=abc\r\n");
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-bad-maxage"));
    let url = format!("http://127.0.0.1:{}/m", port);
    let host = format!("127.0.0.1:{}", port);
    for _ in 0..3 {
        let r = get_via_proxy(proxy_port, &url, &host);
        assert!(r.starts_with("HTTP/1.1 200 "), "{}", r);
        assert!(!r.contains("X-Cache: HIT"), "{}", r);
    }
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}
