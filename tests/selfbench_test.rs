//! 起動時の自己ベンチ (`PROXY_SELF_BENCH=on`、既定 `off`。T14.43) の結合テスト。
//!
//! **実バイナリを起こす**: 見たいのは「待ち受けを開いた直後に自分へ 3 秒打ち、その結果が
//! `/status` の `self_bench` に出る」という配線そのもので、単体ではプロキシが要る側
//! (打ち手・内蔵オリジン・`PROXY_ALLOW_LOCAL=off` の判定) が丸ごと抜けてしまうため。
//!
//! `.env` に `PROXY_ALLOW_LOCAL` は**書かない** (既定の `off` のまま): ループバック宛ての
//! 403 を自己ベンチの 2 ポートだけ通す穴 (`acl::set_self_bench_ports`) が効いていることも、
//! ここで一緒に見る。

mod common;

use std::path::Path;

use common::{ProxyProcess, endpoint_json};

/// テスト用の `.env` を書く (`self_bench` を on にするかだけが違う)。
///
/// 履歴スレッドとキャッシュを止めてあるのは、測るものを増やさないため
/// (自己ベンチが見るのは素通しの forward と CONNECT だけ)。
fn write_env(dir: &Path, self_bench: bool) {
    std::fs::write(
        dir.join(".env"),
        format!(
            "SERVER_PORT=0\n\
             PROXY_BIND=127.0.0.1\n\
             PROXY_LOG_LEVEL=info\n\
             PROXY_STATS_PERSIST=off\n\
             PROXY_CACHE_ENABLED=off\n\
             PROXY_SELF_BENCH={}\n",
            if self_bench { "on" } else { "off" }
        ),
    )
    .unwrap();
}

/// `"key":<数>` を 1 つ読む (`null` なら `None`)。
fn number(json: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{}\":", key);
    let at = json
        .find(&needle)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, json));
    let rest = &json[at + needle.len()..];
    let value: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if value.is_empty() {
        assert!(rest.starts_with("null"), "{} が数でも null でもない", key);
        return None;
    }
    Some(value.parse().unwrap())
}

/// `PROXY_SELF_BENCH=on` の実バイナリで `/status` の `self_bench` に値が出ること。
#[test]
fn test_integration_self_bench_fills_status() {
    let dir = std::env::temp_dir().join(format!("rhp-t1443-on-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_env(&dir, true);
    let proxy = ProxyProcess::start(&dir);
    // 3 秒回してから 1 行出る (`log_info!` の英語 1 行 = `/events` に添える要約と同じ)
    let line = proxy.wait_for_log("self_bench forward");

    // `/status` の末尾の `self_bench` だけを見る (`hosts[]` にも `requests` があるので、
    // 本文の頭から探すと別の数を読んでしまう)
    let body = endpoint_json(proxy.port, "/status");
    let start = body
        .find("\"self_bench\":")
        .unwrap_or_else(|| panic!("self_bench が無い: {}", body));
    let body = body[start..].to_string();
    let at = number(&body, "at").expect("測った時刻");
    assert!(at > 1_700_000_000.0, "時刻が epoch 秒でない: {}", at);
    let forward = number(&body, "forward_us").expect("forward の CPU/要求");
    let connect = number(&body, "connect_us").expect("CONNECT の CPU/本");
    let requests = number(&body, "requests").expect("要求数");
    let connects = number(&body, "connects").expect("本数");
    assert_eq!(number(&body, "secs"), Some(3.0), "3 秒: {}", body);
    assert!(
        number(&body, "cores").unwrap_or(0.0) >= 1.0,
        "コア数が出ていない: {}",
        body
    );
    assert!(
        body.contains("\"note\":null"),
        "断られた理由が付いている: {}",
        line
    );
    // 数字そのものは機械で変わるので、**あり得ない値でないこと**だけを見る
    // (1 要求 1 us 未満はあり得ず、1 要求 10 ms もあり得ない)
    assert!(
        (1.0..10_000.0).contains(&forward),
        "forward の CPU/要求 が変: {} ({})",
        forward,
        line
    );
    assert!(
        (1.0..10_000.0).contains(&connect),
        "CONNECT の CPU/本 が変: {} ({})",
        connect,
        line
    );
    // ループバック宛ての 403 を通す穴が効いていないと、ここが 0 になる
    assert!(requests > 100.0, "要求が通っていない: {}", line);
    assert!(connects > 100.0, "CONNECT が通っていない: {}", line);

    // 出来事の時系列にも 1 件 (種類は増やさず `start` に添える)
    let events = endpoint_json(proxy.port, "/events");
    assert!(
        events.contains("self_bench forward"),
        "/events に自己ベンチの 1 件が無い: {}",
        events
    );
    assert!(
        !events.contains("\"kind\":\"self_bench\""),
        "種類を増やしてはいけない: {}",
        events
    );

    // 穴は 3 秒で閉じる: ループバック宛ての要求は元どおり 403
    let refused = common::raw_get(
        proxy.port,
        "GET http://127.0.0.1:1/ HTTP/1.1\r\nHost: 127.0.0.1:1\r\nConnection: close\r\n\r\n",
    );
    assert!(
        refused.starts_with("HTTP/1.1 403 "),
        "ループバック宛てが通ってしまった: {}",
        refused
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 既定 (`off`) では `/status` の `self_bench` は `null` で、`/events` にも 1 件も無いこと。
#[test]
fn test_integration_self_bench_is_null_by_default() {
    let dir = std::env::temp_dir().join(format!("rhp-t1443-off-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_env(&dir, false);
    let proxy = ProxyProcess::start(&dir);
    let body = endpoint_json(proxy.port, "/status");
    assert!(
        body.contains("\"self_bench\":null"),
        "既定で回ってしまっている: {}",
        body
    );
    let events = endpoint_json(proxy.port, "/events");
    assert!(
        !events.contains("self_bench"),
        "既定で 1 件書かれている: {}",
        events
    );
    let _ = std::fs::remove_dir_all(&dir);
}
