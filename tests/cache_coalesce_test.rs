//! 同じ表現を取りに行く要求の合流 (と、保存できない応答は合流させないこと) の結合テスト。
//!
//! 初めは `tests/cache_test.rs` にあった (T34.118 で割った)。

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

#[test]
fn test_integration_concurrent_misses_are_coalesced() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(|_req, _n| {
            thread::sleep(Duration::from_millis(700));
            let body = "coalesced body";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: max-age=60\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    );
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-coalesce"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/big-asset", host);

    let handles: Vec<_> = (0..6)
        .map(|i| {
            let (url, host) = (url.clone(), host.clone());
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(i * 20));
                get_via_proxy(proxy_port, &url, &host)
            })
        })
        .collect();
    let responses: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for r in &responses {
        assert!(
            r.starts_with("HTTP/1.1 200 OK") && r.ends_with("coalesced body"),
            "{}",
            r
        );
    }
    // オリジンへ行くのは 1 本だけ、残りは合流 (COALESCED) か、その後の保存済み (HIT) から返る。
    // どちらになるかは到着の順番次第なので、まとめて数える (負荷の高い CI でも安定する)
    let from_cache = responses
        .iter()
        .filter(|r| r.contains("X-Cache: COALESCED") || r.contains("X-Cache: HIT"))
        .count();
    assert!(
        from_cache >= 5,
        "expected all but the leader to be served without touching the origin, got {}",
        from_cache
    );
    assert!(
        responses.iter().any(|r| r.contains("X-Cache: COALESCED")),
        "at least one request should have joined the in-flight fetch"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1, "origin fetched once");
}

/// 保存されない URL では、2 回目以降の同時ミスは合流しない (T11.9)。
/// 合流は「1 本が取ってきて保存し、待っていた側はキャッシュから受け取る」ための仕組みなので、
/// 保存されない応答では待つだけ損になる (起きてから結局自分でオリジンへ行く)。
#[test]
fn test_integration_uncacheable_misses_do_not_wait_for_each_other() {
    let counter = Arc::new(AtomicUsize::new(0));
    // オリジンが同時に何本抱えたかを数える。合流していれば leader の 1 本しか来ない
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (live, high) = (Arc::clone(&in_flight), Arc::clone(&peak));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(move |_req, _n| {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            high.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(400));
            live.fetch_sub(1, Ordering::SeqCst);
            let body = "uncacheable body";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    );
    let proxy_port =
        start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-no-coalesce-nostore"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/uncacheable", host);

    // 1 本目で「この URL は保存されない」と分かる (ここまでは合流の対象)
    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.ends_with("uncacheable body"), "{}", first);
    assert!(
        !first.contains("X-Cache"),
        "a miss has no X-Cache: {}",
        first
    );
    assert_eq!(peak.swap(0, Ordering::SeqCst), 1, "the first one is alone");

    // 2 回目以降の同時ミスは合流を通らないので、4 本とも同時にオリジンへ届く
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let (url, host) = (url.clone(), host.clone());
            thread::spawn(move || get_via_proxy(proxy_port, &url, &host))
        })
        .collect();
    let responses: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for r in &responses {
        assert!(
            r.starts_with("HTTP/1.1 200 OK") && r.ends_with("uncacheable body"),
            "{}",
            r
        );
        assert!(
            !r.contains("X-Cache"),
            "nothing is served from cache: {}",
            r
        );
    }
    assert_eq!(
        counter.load(Ordering::SeqCst),
        5,
        "every request is forwarded"
    );
    // 合流していると leader の 1 本が終わってから残りが動くので、同時には 3 本までしか届かない
    assert_eq!(
        peak.load(Ordering::SeqCst),
        4,
        "all four reach the origin at the same time"
    );
}
