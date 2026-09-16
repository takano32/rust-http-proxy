//! `/events` (起きたことの時系列) の結合テスト (T14.11)。
//!
//! **実バイナリを起こして `.env` を書き換える**: 「ファイル → inotify → `reload::Live` →
//! 出来事のリング → `/events`」の配線を丸ごと見る。単体では「リングに入れたものが
//! 出てくる」ことしか見られず、**起動と再読込が本当に 1 本の時系列になるか**が
//! 分からないため (T14.11 の受け入れ基準はこの 1 本)。

mod common;

use std::path::Path;
use std::time::Duration;

use common::{ProxyProcess, endpoint_json};

/// テスト用の `.env` を書く (変えるのは `PROXY_TIMEOUT_SECS` だけ)。
///
/// 履歴スレッドとキャッシュを止めてあるのは、`ipv6` / `pressure` / `ballast` の 3 種
/// (履歴スレッドの周期で拾う出来事) を混ぜずに `start` と `reload` だけを見るため。
fn write_env(dir: &Path, timeout_secs: u32) {
    std::fs::write(
        dir.join(".env"),
        format!(
            "SERVER_PORT=0\n\
             PROXY_BIND=127.0.0.1\n\
             PROXY_LOG_LEVEL=info\n\
             PROXY_STATS_PERSIST=off\n\
             PROXY_CACHE_ENABLED=off\n\
             PROXY_TIMEOUT_SECS={}\n",
            timeout_secs
        ),
    )
    .unwrap();
}

/// JSON から `"key":<数>` を 1 つ読む (最初の 1 件 = いちばん新しい出来事)。
fn first_number(json: &str, key: &str) -> u64 {
    let needle = format!("\"{}\":", key);
    let at = json
        .find(&needle)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, json));
    json[at + needle.len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} が数でない: {}", key, json))
}

/// 起動 → `.env` の `PROXY_TIMEOUT_SECS` を書き換え → `/events` に `start` と `reload`
/// (`PROXY_TIMEOUT_SECS 30 → 10`) が時刻つきで見えること。`?since=` で絞れること。
#[test]
fn test_integration_events_shows_the_start_and_the_reload() {
    let dir = std::env::temp_dir().join(format!("rhp-t1411-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_env(&dir, 30);
    let proxy = ProxyProcess::start(&dir);

    // 1 件目は起動 (版と設定の要約)
    let body = endpoint_json(proxy.port, "/events");
    assert!(
        body.contains("\"kind\":\"start\""),
        "起動が残っていない: {}",
        body
    );
    assert!(
        body.contains(&format!("version {} on port", rust_http_proxy::VERSION)),
        "起動の 1 件に版が無い: {}",
        body
    );
    assert!(
        body.contains("timeout 30s"),
        "起動の 1 件に設定の要約が無い: {}",
        body
    );
    let start_at = first_number(&body, "at");
    assert!(start_at > 1_600_000_000, "時刻が入っていない: {}", body);
    assert!(body.contains("\"count\":1"), "起動直後は 1 件: {}", body);

    // 書き換える (エディタと同じく上書き。inotify の IN_CLOSE_WRITE で気づく)。
    // 1 秒あけるのは `start` と `reload` を別の秒にして `?since=` を見るため
    std::thread::sleep(Duration::from_millis(1100));
    write_env(&dir, 10);
    proxy.wait_for_log("settings reloaded");

    let body = endpoint_json(proxy.port, "/events");
    assert!(
        body.contains("PROXY_TIMEOUT_SECS 30 \u{2192} 10"),
        "前後の値が出ていない: {}",
        body
    );
    assert!(
        body.contains("\"kind\":\"reload\""),
        "再読込が残っていない: {}",
        body
    );
    assert!(body.contains("\"count\":2"), "2 件になる: {}", body);
    assert!(body.contains("\"recorded\":2"), "通算も 2 件: {}", body);
    assert!(body.contains("\"capacity\":512"), "{}", body);
    assert!(
        body.len() <= 256 * 1024,
        "応答が 256 KiB を越えた: {} B",
        body.len()
    );
    // 新しい順 (先頭が再読込)
    let reload_at = first_number(&body, "at");
    assert!(
        body.find("reload").unwrap() < body.find("\"kind\":\"start\"").unwrap(),
        "新しい順でない: {}",
        body
    );
    assert!(reload_at > start_at, "{} <= {}", reload_at, start_at);

    // `?since=` で絞れる (再読込だけが残る)
    let narrowed = endpoint_json(proxy.port, &format!("/events?since={}", reload_at));
    assert!(
        narrowed.contains("\"count\":1"),
        "1 件に絞れる: {}",
        narrowed
    );
    assert!(
        !narrowed.contains("\"kind\":\"start\""),
        "古い 1 件が落ちる: {}",
        narrowed
    );
    assert!(
        narrowed.contains("\"recorded\":2"),
        "通算は絞っても残る: {}",
        narrowed
    );
    let future = endpoint_json(proxy.port, &format!("/events?since={}", reload_at + 3600));
    assert!(
        future.starts_with("{\"events\":[]"),
        "先の時刻なら 0 件: {}",
        future
    );
    // `?n=` でも絞れる
    let one = endpoint_json(proxy.port, "/events?n=1");
    assert!(one.contains("\"count\":1"), "n=1 で 1 件: {}", one);

    // 案内と `/snapshot` からも辿れること
    let index = endpoint_json(proxy.port, "/");
    assert!(index.contains("/events"), "`/` の案内に無い: {}", index);
    let snapshot = endpoint_json(proxy.port, "/snapshot");
    assert!(snapshot.contains("\"events\""), "/snapshot に入っていない");
    assert!(
        snapshot.contains("PROXY_TIMEOUT_SECS 30 \u{2192} 10"),
        "/snapshot の events が空"
    );

    drop(proxy);
    let _ = std::fs::remove_dir_all(&dir);
}
