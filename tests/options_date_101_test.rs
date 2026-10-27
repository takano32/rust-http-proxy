//! 小物 3 つ (T.29.9): `OPTIONS *`・応答の `Date`・頼んでいない 101。
//!
//! (1) `OPTIONS http://h:P` (空のパス) はオリジンへ `OPTIONS *` で送る (RFC 9112 §3.2.4)。`OPTIONS *` + `Host`
//!     は `Host` のオリジンへ `*` のまま送る (今までは `*` をホスト名として引いて 502)。`*` は OPTIONS 以外 400。
//! (2) `Date` の無い応答と自前の応答 (400 / 502 など) に `Date` を付ける (RFC 9110 §6.6.1)。
//! (3) `Upgrade` を頼んでいない要求への 101 は 502 (RFC 9110 §15.2.2。今まではそのまま返していた)。

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod common;
use common::*;

/// パスごとに応答を変え、受けた要求行を覚えるオリジン。
fn origin(seen: Arc<Mutex<Vec<String>>>) -> u16 {
    start_origin(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(move |req: &str, _| {
            let line = req.lines().next().unwrap_or("").to_string();
            seen.lock().unwrap().push(line.clone());
            if line.contains(" /switch ") {
                return b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                    Connection: Upgrade\r\n\r\n"
                    .to_vec();
            }
            if line.contains(" /dated ") {
                return b"HTTP/1.1 200 OK\r\nDate: Mon, 01 Sep 2025 00:00:00 GMT\r\n\
                    Content-Length: 2\r\nConnection: close\r\n\r\nok"
                    .to_vec();
            }
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec()
        }),
    )
    .0
}

fn head_of(resp: &str) -> &str {
    resp.split("\r\n\r\n").next().unwrap_or("")
}

/// `Date: ` の行の値を全部 (読めることも確かめる)。
fn dates(resp: &str) -> Vec<String> {
    head_of(resp)
        .lines()
        .filter_map(|l| l.strip_prefix("Date: "))
        .map(|v| {
            assert!(
                rust_http_proxy::httpdate::parse(v).is_some(),
                "読めない Date: {:?}",
                v
            );
            v.to_string()
        })
        .collect()
}

/// **(1)**: 空のパスの OPTIONS と `OPTIONS *` はオリジンへ `*` で届く。`*` は OPTIONS 以外 400。
#[test]
fn test_integration_options_asterisk() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = origin(Arc::clone(&seen));
    let proxy_port = start_test_proxy(proxy_config());
    let h = format!("127.0.0.1:{port}");

    let resp = raw_get(
        proxy_port,
        &format!("OPTIONS http://{h} HTTP/1.1\r\nHost: {h}\r\nConnection: close\r\n\r\n"),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    let resp = raw_get(
        proxy_port,
        &format!("OPTIONS * HTTP/1.1\r\nHost: {h}\r\nConnection: close\r\n\r\n"),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    // パスのある OPTIONS は今までどおり
    let resp = raw_get(
        proxy_port,
        &format!("OPTIONS http://{h}/x HTTP/1.1\r\nHost: {h}\r\nConnection: close\r\n\r\n"),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            "OPTIONS * HTTP/1.1".to_string(),
            "OPTIONS * HTTP/1.1".to_string(),
            "OPTIONS /x HTTP/1.1".to_string()
        ]
    );

    // `*` は OPTIONS だけ。GET * は 400 でオリジンへ届かない。`Host` の無い `OPTIONS *` も 400
    let resp = raw_get(
        proxy_port,
        &format!("GET * HTTP/1.1\r\nHost: {h}\r\nConnection: close\r\n\r\n"),
    );
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);
    let resp = raw_get(
        proxy_port,
        "OPTIONS * HTTP/1.1\r\nConnection: close\r\n\r\n",
    );
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(seen.lock().unwrap().len(), 3);
}

/// **(2)**: `Date` の無い応答には 1 本付き、ある応答はオリジンのまま 1 本。自前の 400 / 502 にも付く。
#[test]
fn test_integration_responses_carry_a_date() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = origin(Arc::clone(&seen));
    let proxy_port = start_test_proxy(proxy_config());
    let h = format!("127.0.0.1:{port}");

    let plain = get_via_proxy(proxy_port, &format!("http://{h}/plain"), &h);
    assert!(plain.starts_with("HTTP/1.1 200 "), "{:?}", plain);
    assert_eq!(dates(&plain).len(), 1, "{:?}", plain);
    let dated = get_via_proxy(proxy_port, &format!("http://{h}/dated"), &h);
    assert_eq!(
        dates(&dated),
        vec!["Mon, 01 Sep 2025 00:00:00 GMT".to_string()],
        "{:?}",
        dated
    );

    // 自前の 400 (要求行が壊れている) と 502 (頼んでいない 101)
    let bad = raw_get(proxy_port, "GET http://x/a b HTTP/1.1\r\n\r\n");
    assert!(bad.starts_with("HTTP/1.1 400 "), "{:?}", bad);
    assert_eq!(dates(&bad).len(), 1, "{:?}", bad);
    let gw = get_via_proxy(proxy_port, &format!("http://{h}/switch"), &h);
    assert!(gw.starts_with("HTTP/1.1 502 "), "{:?}", gw);
    assert_eq!(dates(&gw).len(), 1, "{:?}", gw);
}

/// **(3)**: `Upgrade` を頼んでいない要求への 101 は 502。頼んだ要求には今までどおり 101 を返す。
#[test]
fn test_integration_unsolicited_101_is_502() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = origin(Arc::clone(&seen));
    let proxy_port = start_test_proxy(proxy_config());
    let h = format!("127.0.0.1:{port}");

    let resp = get_via_proxy(proxy_port, &format!("http://{h}/switch"), &h);
    assert!(resp.starts_with("HTTP/1.1 502 "), "{:?}", resp);
    let resp = get_via_proxy_with(
        proxy_port,
        &format!("http://{h}/switch"),
        &h,
        "Upgrade: websocket\r\n",
    );
    assert!(resp.starts_with("HTTP/1.1 101 "), "{:?}", resp);
}
