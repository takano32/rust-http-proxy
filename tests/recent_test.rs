//! 個票のエンドポイントの結合テスト (T13.4): `/errors` `/connections` `/dns` `/log` `/hosts`。
//!
//! 集計 (`/status`) と時系列 (`/history`) では読めない「**誰が・いつ・なぜ**」を
//! 出す口なので、見るのは「実際に起きたことがその形で出てくるか」だけ。

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
#[test]
fn test_integration_errors_keeps_the_newest_first_and_honours_n() {
    let dead_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let proxy_port = start_test_proxy(proxy_config());

    // 2 つの宛先へ順に失敗させる (2 件目が新しい)
    for path in ["first", "second"] {
        let dead = format!("127.0.0.1:{}", dead_port);
        let r = get_via_proxy(proxy_port, &format!("http://{}/{}", dead, path), &dead);
        assert!(r.starts_with("HTTP/1.1 502"), "{}", r);
    }
    let json = endpoint_json(proxy_port, "/errors");
    assert!(json.contains("\"recorded\":2"), "{}", json);
    assert!(json.contains("\"kind\":\"forward\""), "{}", json);
    assert!(json.contains("\"cause\":\"refused\""), "{}", json);
    // forward の宛先はホスト別統計と同じ鍵 (`scheme://host:port`)
    assert!(
        json.contains(&format!("\"target\":\"http://127.0.0.1:{}\"", dead_port)),
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
}
