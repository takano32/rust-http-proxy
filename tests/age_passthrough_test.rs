//! 素通しの応答に `Age` を残す (T.29.4)。
//!
//! `Age` は end-to-end (RFC 9110 §7.6・RFC 9111 §4.2.3) なのに、0.29.35 までは枠組みの名前と一緒に
//! 無条件に落としていた。オリジン (の手前のキャッシュ) が `max-age=600, Age: 500` を返しても、
//! MISS / BYPASS の応答には `Age` が無く、下流は残り 600 秒新鮮と読んでいた (HIT にだけ付いた)。

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

mod common;
use common::*;

/// 応答の `Age` の値 (無ければ `None`)。
fn age_of(resp: &str) -> Option<u64> {
    let head = resp.split("\r\n\r\n").next().unwrap_or("");
    head.lines()
        .find_map(|l| l.strip_prefix("Age: "))
        .and_then(|v| v.trim().parse().ok())
}

fn start_age_origin() -> u16 {
    let (port, _handle) = start_origin(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(|req: &str, _n: usize| {
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("");
            match path {
                // 手前のキャッシュで 500 秒たった表現
                "/aged" | "/aged-bypass" => b"HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\n\
                    Age: 500\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
                    .to_vec(),
                // `Date` の方が古い: `now - Date` の方を使う
                "/old-date" => b"HTTP/1.1 200 OK\r\nCache-Control: no-store\r\n\
                    Date: Mon, 01 Sep 2025 00:00:00 GMT\r\nAge: 500\r\nContent-Length: 5\r\n\
                    Connection: close\r\n\r\nhello"
                    .to_vec(),
                // `Age` の無い応答には足さない
                _ => b"HTTP/1.1 200 OK\r\nCache-Control: max-age=600\r\nContent-Length: 5\r\n\
                    Connection: close\r\n\r\nhello"
                    .to_vec(),
            }
        }),
    );
    port
}

/// **受け入れ基準**: MISS / BYPASS の応答に `Age: 500` 以上が付き、HIT の `Age` は今までどおり。
#[test]
fn test_integration_age_survives_on_forwarded_responses() {
    let origin_port = start_age_origin();
    let proxy_port =
        start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-age-passthrough"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = |p: &str| format!("http://{}{}", host, p);

    // MISS (保存する): 受けた Age がそのまま付く
    let miss = get_via_proxy(proxy_port, &url("/aged"), &host);
    assert!(!miss.contains("X-Cache"), "{}", miss);
    let age = age_of(&miss).unwrap_or_else(|| panic!("MISS に Age が無い: {}", miss));
    assert!((500..510).contains(&age), "{}", miss);
    assert_eq!(miss.matches("\r\nAge: ").count(), 1, "{}", miss);

    // HIT: 保存したときの age から数える (今までと同じ値)
    let hit = get_via_proxy(proxy_port, &url("/aged"), &host);
    assert!(hit.contains("X-Cache: HIT"), "{}", hit);
    let age = age_of(&hit).unwrap_or_else(|| panic!("HIT に Age が無い: {}", hit));
    assert!((500..510).contains(&age), "{}", hit);
    assert_eq!(hit.matches("\r\nAge: ").count(), 1, "{}", hit);

    // BYPASS (認証つきの要求は保存も参照もしない) でも残る
    let bypass = get_via_proxy_with(
        proxy_port,
        &url("/aged-bypass"),
        &host,
        "Authorization: Basic dTpw\r\n",
    );
    let age = age_of(&bypass).unwrap_or_else(|| panic!("BYPASS に Age が無い: {}", bypass));
    assert!((500..510).contains(&age), "{}", bypass);

    // `Date` から数えた方が大きければそちら
    let old = get_via_proxy(proxy_port, &url("/old-date"), &host);
    let age = age_of(&old).unwrap_or_else(|| panic!("Age が無い: {}", old));
    assert!(age > 30 * 24 * 3600, "{}", old);

    // `Age` の無い応答 (MISS) には足さない
    let plain = get_via_proxy(proxy_port, &url("/plain"), &host);
    assert!(age_of(&plain).is_none(), "{}", plain);
}
