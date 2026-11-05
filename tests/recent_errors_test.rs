//! 個票のエンドポイント `/errors` の結合テスト (T13.4)。
//!
//! 初めは `tests/recent_test.rs` にあった (T.34.117 で割った)。

mod common;

use std::net::TcpListener;

use common::*;

/// 閉じたポートへの CONNECT が `/errors` に 1 件 (原因 `refused`、宛先、接続 ms) 残ること。
#[test]
fn test_integration_errors_records_a_refused_connect() {
    let dead_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let proxy_port = start_test_proxy(proxy_config());

    // 空の `/errors` (まだ何も起きていない)
    let empty = endpoint_json(proxy_port, "/errors");
    assert!(empty.contains("\"errors\":[]"), "{}", empty);
    assert!(empty.contains("\"recorded\":0"), "{}", empty);
    assert!(empty.contains("\"capacity\":500"), "{}", empty);

    let target = format!("127.0.0.1:{}", dead_port);
    let out = raw_get(
        proxy_port,
        &format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", target, target),
    );
    assert!(out.starts_with("HTTP/1.1 502"), "{}", out);

    let json = endpoint_json(proxy_port, "/errors");
    assert!(json.contains("\"recorded\":1"), "{}", json);
    assert!(json.contains("\"count\":1"), "{}", json);
    assert!(json.contains("\"kind\":\"connect\""), "{}", json);
    assert!(json.contains("\"cause\":\"refused\""), "{}", json);
    assert!(json.contains("\"status\":502"), "{}", json);
    assert!(
        json.contains(&format!("\"target\":\"{}\"", target)),
        "宛先が無い: {}",
        json
    );
    assert!(json.contains("\"client\":\"127.0.0.1\""), "{}", json);
    // 名前解決と接続の ms、時刻 (epoch 秒) の欄があること
    assert!(json.contains("\"dns_ms\":"), "{}", json);
    assert!(json.contains("\"connect_ms\":"), "{}", json);
    let at = status_number(&json, "at");
    assert!(at > 1_700_000_000, "時刻が epoch 秒でない: {}", at);
    assert!(!json.contains("\"truncated\":true"), "{}", json);
}

/// forward の 502 も `/errors` に残り、`?n=` が件数を絞ること (新しい順)。
///
/// **宛先は 2 つの別のポート** (T20.3)。同じ宛先への同じ失敗は 1 行にまとまるので、
/// 「2 行ある」を見るには鍵 (`target`) が違う要がある。最後に同じ失敗をもう 1 回送り、
/// 行が増えずに `repeats` が 2 になることも見る。
#[test]
fn test_integration_errors_keeps_the_newest_first_and_honours_n() {
    // 2 つを同時に束縛してから手放す (順に取ると同じ番号が 2 回配られうる)
    let (first_port, second_port) = {
        let a = TcpListener::bind("127.0.0.1:0").unwrap();
        let b = TcpListener::bind("127.0.0.1:0").unwrap();
        (
            a.local_addr().unwrap().port(),
            b.local_addr().unwrap().port(),
        )
    };
    let proxy_port = start_test_proxy(proxy_config());

    // 2 つの宛先へ順に失敗させる (2 件目が新しい)
    for (path, port) in [("first", first_port), ("second", second_port)] {
        let dead = format!("127.0.0.1:{}", port);
        let r = get_via_proxy(proxy_port, &format!("http://{}/{}", dead, path), &dead);
        assert!(r.starts_with("HTTP/1.1 502"), "{}", r);
    }
    let json = endpoint_json(proxy_port, "/errors");
    assert!(json.contains("\"recorded\":2"), "{}", json);
    assert!(json.contains("\"kind\":\"forward\""), "{}", json);
    assert!(json.contains("\"cause\":\"refused\""), "{}", json);
    // forward の宛先はホスト別統計と同じ鍵 (`scheme://host:port`)
    let first = format!("\"target\":\"http://127.0.0.1:{}\"", first_port);
    let second = format!("\"target\":\"http://127.0.0.1:{}\"", second_port);
    assert!(json.contains(&first), "{}", json);
    // 新しい順 (2 件目が先)
    assert!(
        json.find(&second).expect("2 件目が無い") < json.find(&first).unwrap(),
        "{}",
        json
    );
    // まとめていない行は `repeats` 1 で、`last_at` は `at` と同じ (T20.3)
    assert_eq!(
        json.matches("\"repeats\":1,\"last_at\":").count(),
        2,
        "{}",
        json
    );

    // `?n=1` は 1 件だけ (リングに 2 件あっても)
    let one = endpoint_json(proxy_port, "/errors?n=1");
    assert!(one.contains("\"count\":1"), "{}", one);
    assert!(one.contains("\"kept\":2"), "{}", one);
    assert_eq!(one.matches("\"cause\":").count(), 1, "{}", one);
    // 知らない / 壊れた問い合わせは既定に倒す
    let bad = endpoint_json(proxy_port, "/errors?n=abc&x=1");
    assert!(bad.contains("\"count\":2"), "{}", bad);

    // 同じ失敗をもう 1 回 (古い方の宛先へ)。行は増えず、その行の `repeats` が 2 になり、
    // 通算は 3 になる。並びは行ができた順のまま (T20.3)
    let dead = format!("127.0.0.1:{}", first_port);
    let r = get_via_proxy(proxy_port, &format!("http://{}/again", dead), &dead);
    assert!(r.starts_with("HTTP/1.1 502"), "{}", r);
    let again = endpoint_json(proxy_port, "/errors");
    assert!(again.contains("\"recorded\":3"), "{}", again);
    assert!(again.contains("\"kept\":2"), "{}", again);
    assert!(again.contains("\"count\":2"), "{}", again);
    assert_eq!(
        again.matches("\"repeats\":2,\"last_at\":").count(),
        1,
        "{}",
        again
    );
    let at_first = again.find(&first).unwrap();
    assert!(again.find(&second).unwrap() < at_first, "{}", again);
    assert!(
        again[at_first..].contains("\"repeats\":2"),
        "古い方の行に足していない: {}",
        again
    );
}

/// ACL で拒否した CONNECT が `/errors` に `acl` として見えること (T14.2 (4))。
///
/// 403 は 5xx ではないので集計 (`errors_by_cause`) には乗らない。**誰が何を拒否されたか**を
/// 読めるのは個票だけなので、そこに出ることを見る。
#[test]
fn test_integration_errors_records_a_403_with_its_reason() {
    use rust_http_proxy::config::Config;
    use std::time::Duration;

    let mut cfg = Config::new(
        "0",
        None,
        Some("blocked.example, 127.0.0.1"),
        Duration::from_secs(5),
    )
    .unwrap();
    cfg.keepalive = Duration::from_secs(2);
    let proxy_port = start_test_proxy(cfg);

    // ACL で拒否される CONNECT (127.0.0.1 は deny_hosts に入れてある)
    let target = "127.0.0.1:443";
    let out = raw_get(
        proxy_port,
        &format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", target, target),
    );
    assert!(out.starts_with("HTTP/1.1 403"), "{}", out);

    let json = endpoint_json(proxy_port, "/errors");
    assert!(json.contains("\"recorded\":1"), "{}", json);
    assert!(json.contains("\"cause\":\"acl\""), "{}", json);
    assert!(json.contains("\"status\":403"), "{}", json);
    assert!(json.contains("\"kind\":\"connect\""), "{}", json);
    assert!(
        json.contains(&format!("\"target\":\"{}\"", target)),
        "宛先が無い: {}",
        json
    );
    assert!(json.contains("\"client\":\"127.0.0.1\""), "{}", json);

    // 集計の方は今までどおり: 403 は `blocked` に数え、`errors_by_cause` は全部 0 のまま
    // (10 列 = 8 つの原因 + T25.0 の `dns` の内訳 2 つ)
    let status = endpoint_json(proxy_port, "/status");
    assert!(status.contains("\"blocked\":1"), "{}", status);
    assert!(
        status.contains("\"errors_by_cause\":[0,0,0,0,0,0,0,0,0,0]"),
        "403 が集計の原因別に混ざっている: {}",
        status
    );
}
