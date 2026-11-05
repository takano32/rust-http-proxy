//! `/snapshot` (1 要求で全部取る。T14.4) の結合テスト。
//!
//! 初めは `tests/recent_test.rs` にあった (T34.117 で割った)。

mod common;

use std::net::TcpListener;

use common::*;

// ---------------------------------------------------------------------------
// `/snapshot` — 1 要求で全部取る (T14.4)

// ---------------------------------------------------------------------------

/// JSON として括弧と引用符の釣り合いが取れているか (外部クレートを足さずに形だけ見る)。
///
/// `/snapshot` は部品の文字列を手で並べて組むので、**入れ子の閉じ忘れ**が最も起きやすい。
/// 本当の構文検査は `scripts/collect-deployed.sh` が Python で通すが、回帰はここで止める。
fn json_is_balanced(s: &str) -> bool {
    let mut depth: i64 = 0;
    let (mut in_str, mut escaped) = (false, false);
    for c in s.chars() {
        if in_str {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !in_str
}

/// (d) `/snapshot` が 1 要求で全部を含み、4 MiB 以下であること。
#[test]
fn test_integration_snapshot_has_every_part_in_one_request() {
    let (origin_port, _origin) = common::start_mock_origin();
    let dead_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let proxy_port = start_test_proxy(proxy_config());

    // 個票に中身があるようにしておく (成功 1 本、失敗 1 本)
    let host = format!("127.0.0.1:{}", origin_port);
    let ok = get_via_proxy(proxy_port, &format!("http://{}/x", host), &host);
    assert!(ok.starts_with("HTTP/1.1 200"), "{}", ok);
    let dead = format!("127.0.0.1:{}", dead_port);
    let bad = raw_get(
        proxy_port,
        &format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", dead, dead),
    );
    assert!(bad.starts_with("HTTP/1.1 502"), "{}", bad);

    let json = endpoint_json(proxy_port, "/snapshot");
    assert!(json_is_balanced(&json), "JSON の括弧が合わない: {}", json);
    assert!(
        json.len() <= 4 * 1024 * 1024,
        "4 MiB を越えた: {} B",
        json.len()
    );

    // 頭 (いつ・どの版・どれが入っているか)
    assert!(json.contains("\"taken_at\":"), "{}", json);
    assert!(json.contains("\"version\":"), "{}", json);
    assert!(json.contains("\"uptime_secs\":"), "{}", json);
    assert!(json.contains("\"dropped\":[]"), "{}", json);
    // 中身 (17 本の URL を手で叩いていたぶん)
    for key in [
        "\"status\":{",
        "\"status_errors\":{",
        "\"status_dns\":{",
        "\"history\":{\"5\":{",
        "\"60\":{",
        "\"3600\":{",
        "\"dns\":{",
        "\"errors\":{",
        "\"connections\":{",
        "\"recent\":{",
        "\"hosts\":{",
        "\"log\":{",
    ] {
        assert!(
            json.contains(key),
            "{} が無い: {}",
            key,
            &json[..600.min(json.len())]
        );
    }
    // `parts` に名前が並ぶ (この版に何が入っていたかが JSON だけで分かる)
    for name in ["status", "history.5", "recent", "log"] {
        assert!(json.contains(&format!("\"{}\"", name)), "{}", name);
    }
    // 部品の中身が実際に入っていること (集計・履歴・個票の 3 層)
    assert!(json.contains("\"total_requests\":"), "{}", "/status の中身");
    assert!(
        json.contains("\"cause\":\"refused\""),
        "{}",
        "/errors の中身"
    );
    assert!(
        json.contains(&format!("\"target\":\"http://{}\"", host))
            || json.contains(&format!("\"target\":\"{}\"", host)),
        "{}",
        "/recent の中身"
    );
    assert!(json.contains("\"keys\":"), "{}", "/history の中身");

    // `/snapshot` は取るだけで何も保持しない (2 回取っても形は同じ)
    let again = endpoint_json(proxy_port, "/snapshot");
    assert!(json_is_balanced(&again));
    assert!(again.contains("\"dropped\":[]"), "{}", again);
}
