//! 1 相手の説明 `/explain?host=<name>` / `?client=<ip>` の結合テスト (T14.36)。
//!
//! T14.0 で「この宛先はなぜ遅いか」を調べたときは `/hosts` `/dns` `/recent` `/clients`
//! `/errors` を手で横断して読んだ。ここで見るのは「**その横断が 1 要求で返ってくるか**」
//! — 実際に通した 3 本の CONNECT が、統計・`/dns` の行・個票・1 行の判定として
//! 1 枚に揃うか、知らない相手は 200 のまま `"known":false` か、だけ。

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;

use common::*;

/// CONNECT を 1 本張って 1 往復し、閉じる (閉じたところで統計と個票に入る)。
fn connect_once(proxy_port: u16, target: &str) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "CONNECT {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: t1436/1.0\r\n\r\n",
        target, target
    );
    stream.write_all(req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{} -> {}", target, head);
    stream.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
}

/// 数字の欄 (`"numbers":{…`) の中だけを返す (同じ名前が統計にもあるので、
/// 数を見るときは必ずここから取る)。
fn numbers(json: &str) -> &str {
    json.split("\"numbers\":{")
        .nth(1)
        .unwrap_or_else(|| panic!("numbers が無い: {}", json))
}

/// `"key":"…"` / `"summary":"…"` のような文字列の値を 1 つ取る。
fn string_value(json: &str, key: &str) -> String {
    let pat = format!("\"{}\":\"", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("no {} in {}", key, json))
        + pat.len();
    let rest = &json[at..];
    let end = rest
        .find('"')
        .unwrap_or_else(|| panic!("no end of {}", key));
    rest[..end].to_string()
}

/// 1 ホストへ 3 本の CONNECT のあと、`/explain?host=` に統計・`/dns` の行・
/// 3 本の個票・1 行の判定が揃うこと (受け入れ基準そのもの)。
#[test]
fn test_integration_explain_host_gathers_stats_dns_and_the_records() {
    let echo = start_echo_server();
    let proxy_port = start_test_proxy(proxy_config());

    // まだ誰も通っていない = 知らない相手 (200 のまま `"known":false`)
    let empty = endpoint_json(proxy_port, "/explain?host=localhost");
    assert!(empty.contains("\"known\":false"), "{}", empty);
    assert!(empty.contains("\"kind\":\"host\""), "{}", empty);
    assert!(empty.contains("記録は 1 件も無い"), "{}", empty);

    // 名前宛ての CONNECT を 3 本 (`localhost` は IP リテラルではないので `/dns` を通る)
    let target = format!("localhost:{}", echo);
    for _ in 0..3 {
        connect_once(proxy_port, &target);
    }
    wait_until(
        || {
            let json = endpoint_json(proxy_port, "/explain?host=localhost");
            status_number(numbers(&json), "recent") >= 3
        },
        "/explain に 3 本の個票が出る",
    );

    let json = endpoint_json(proxy_port, "/explain?host=localhost");
    // (1) 応答は 64 KiB 以下 (個票は 10 本まで)
    assert!(json.len() <= 64 * 1024, "64 KiB を越えた: {}", json.len());
    assert!(json.contains("\"known\":true"), "{}", json);
    assert!(json.contains("\"truncated\":false"), "{}", json);

    // (2) ホスト別の統計 (要求数・平均・p50 / p95・名前解決・RTT・エラーの原因)
    assert!(
        json.contains(&format!("\"key\":\"connect://localhost:{}\"", echo)),
        "ホスト表の鍵が無い: {}",
        json
    );
    assert_eq!(status_number(numbers(&json), "requests"), 3, "{}", json);
    assert_eq!(
        status_number(numbers(&json), "connect_requests"),
        3,
        "{}",
        json
    );
    for key in [
        "\"avg_ms\":",
        "\"p50_ms\":",
        "\"p95_ms\":",
        "\"dns_ms\":",
        "\"connect_ms\":",
        "\"proxy_ms\":",
        "\"rtt_ms\":",
        "\"dns_misses\":",
        "\"errors_by_cause\":{\"dns\":",
    ] {
        assert!(json.contains(key), "{} が無い: {}", key, json);
    }

    // (3) `/dns` の行 (warm か、残り TTL、答え)
    assert!(json.contains("\"dns\":{\"host\":\"localhost\""), "{}", json);
    assert!(json.contains("\"ttl_left\":"), "{}", json);
    assert!(json.contains("\"warm\":"), "{}", json);
    // 答えの並びは OS の resolver しだい (CI の runner は `::1` が先) なので、順番は見ない
    let dns_block = json
        .split("\"dns\":{\"host\":\"localhost\"")
        .nth(1)
        .unwrap_or_else(|| panic!("dns が無い: {}", json));
    let addrs = dns_block
        .split("\"addrs\":[")
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .unwrap_or_else(|| panic!("addrs が無い: {}", json));
    assert!(
        addrs.contains("\"127.0.0.1\""),
        "addrs に 127.0.0.1 が無い: {}",
        json
    );

    // (4) 直近 10 本までの個票 (段階と閉じた理由)
    let recent = json
        .split("\"recent\":[")
        .nth(1)
        .unwrap_or_else(|| panic!("recent が無い: {}", json));
    assert_eq!(
        recent
            .matches(&format!("\"target\":\"localhost:{}\"", echo))
            .count(),
        3,
        "3 本ぶんの個票が無い: {}",
        json
    );
    assert!(recent.contains("\"reason\":\""), "{}", json);
    assert!(recent.contains("\"ms\":{\"dns\":"), "{}", json);
    assert!(
        json.contains("\"errors\":[]"),
        "エラーは 0 件のはず: {}",
        json
    );
    assert_eq!(
        status_number(numbers(&json), "recent_errors"),
        0,
        "{}",
        json
    );

    // (5) 末尾の 1 行の判定 (段階の数字を並べた日本語) と、同じ内容の数字の欄
    let summary = string_value(&json, "summary");
    assert!(
        summary.contains("3 要求 (CONNECT 3 / forward 0)"),
        "{}",
        summary
    );
    assert!(summary.contains("確立の平均"), "{}", summary);
    assert!(summary.contains("名前解決"), "{}", summary);
    assert!(summary.contains("接続"), "{}", summary);
    assert!(summary.contains("プロキシ側"), "{}", summary);
    assert!(summary.contains("名前解決の表: "), "{}", summary);
    assert!(summary.contains("閉じた接続の個票 3 本"), "{}", summary);
    // 文章の数字と `numbers` の数字が同じであること (平均で突き合わせる)
    let avg = numbers(&json)
        .split("\"avg_ms\":")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .unwrap_or_default()
        .to_string();
    assert!(
        summary.contains(&format!("確立の平均 {} ms", avg)),
        "文章 ({}) と数字 ({}) が合わない",
        summary,
        avg
    );

    // (6) ポートを書けばそのポートだけ、違うポートは知らない相手
    let with_port = endpoint_json(proxy_port, &format!("/explain?host=localhost:{}", echo));
    assert!(with_port.contains("\"known\":true"), "{}", with_port);
    assert_eq!(
        status_number(numbers(&with_port), "requests"),
        3,
        "{}",
        with_port
    );
    let other_port = endpoint_json(proxy_port, "/explain?host=localhost:9");
    assert_eq!(
        status_number(numbers(&other_port), "requests"),
        0,
        "{}",
        other_port
    );
    assert!(other_port.contains("\"keys\":[]"), "{}", other_port);

    // (7) 知らないホストは 200 のまま `"known":false`
    let unknown = endpoint_json(proxy_port, "/explain?host=t1436-no-such-host.invalid");
    assert!(unknown.contains("\"known\":false"), "{}", unknown);
    assert!(unknown.contains("\"dns\":null"), "{}", unknown);
    assert!(unknown.contains("\"recent\":[]"), "{}", unknown);
    assert!(unknown.contains("\"errors\":[]"), "{}", unknown);
    assert_eq!(
        status_number(numbers(&unknown), "requests"),
        0,
        "{}",
        unknown
    );
}

/// `?client=<ip>` に `/clients` の行と個票が入ること。
#[test]
fn test_integration_explain_client_gathers_the_row_and_the_records() {
    let echo = start_echo_server();
    let proxy_port = start_test_proxy(proxy_config());
    let target = format!("127.0.0.1:{}", echo);
    for _ in 0..3 {
        connect_once(proxy_port, &target);
    }
    wait_until(
        || {
            let json = endpoint_json(proxy_port, "/explain?client=127.0.0.1");
            status_number(numbers(&json), "recent") >= 3
        },
        "/explain?client= に 3 本の個票が出る",
    );

    let json = endpoint_json(proxy_port, "/explain?client=127.0.0.1");
    assert!(json.len() <= 64 * 1024, "64 KiB を越えた: {}", json.len());
    assert!(json.contains("\"kind\":\"client\""), "{}", json);
    assert!(json.contains("\"known\":true"), "{}", json);
    // `/clients` の行がそのまま入る (`User-Agent`・宛先の種類・ポート)
    assert!(
        json.contains("\"stats\":{\"client\":\"127.0.0.1\""),
        "{}",
        json
    );
    assert!(json.contains("\"agents\":[\"t1436/1.0\"]"), "{}", json);
    assert!(json.contains("\"literal_targets\":3"), "{}", json);
    assert!(
        json.contains(&format!("{{\"port\":{},\"requests\":3}}", echo)),
        "{}",
        json
    );
    // 個票 (直近 10 本まで) と RTT
    let recent = json
        .split("\"recent\":[")
        .nth(1)
        .unwrap_or_else(|| panic!("recent が無い: {}", json));
    assert_eq!(
        recent.matches("\"client\":\"127.0.0.1\"").count(),
        3,
        "3 本ぶんの個票が無い: {}",
        json
    );
    assert!(json.contains("\"rtt_ms\":"), "{}", json);
    // 同じ形の 1 行の判定
    let summary = string_value(&json, "summary");
    assert!(summary.contains("3 要求。応答まで"), "{}", summary);
    assert!(summary.contains("宛先 1 種"), "{}", summary);
    assert!(summary.contains("閉じた接続の個票 3 本"), "{}", summary);

    // 知らない接続元は 200 のまま `"known":false`
    let unknown = endpoint_json(proxy_port, "/explain?client=198.51.100.7");
    assert!(unknown.contains("\"known\":false"), "{}", unknown);
    assert!(unknown.contains("\"stats\":null"), "{}", unknown);
    assert!(unknown.contains("\"recent\":[]"), "{}", unknown);
}

/// 引数が無ければ 400 で使い方を返す (`/lookup` と同じ方針)。案内にも載っている。
#[test]
fn test_integration_explain_without_a_target_answers_400() {
    let proxy_port = start_test_proxy(proxy_config());
    let out = raw_get(
        proxy_port,
        &format!(
            "GET /explain HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            proxy_port
        ),
    );
    assert!(out.starts_with("HTTP/1.1 400 "), "{}", out);
    assert!(out.contains("/explain?host="), "{}", out);
    // `/` の案内に 1 行ある
    let listing = raw_get(
        proxy_port,
        &format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            proxy_port
        ),
    );
    assert!(listing.contains("/explain?host=<name>"), "{}", listing);
}
