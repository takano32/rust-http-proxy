//! `/status?hosts=0` の結合テスト (T25.1 段 2)。
//!
//! `/status` は `hosts[]` (最大 50 行) が応答の 8 割を占め、宛先が増えるほど育つ
//! (上限の `MAX_HOSTS` = 1,000 行で約 538 KB)。監視が 30 秒ごとに引くと 1 日 1.5 GB に
//! なるので、**クエリで `hosts` を落とせる**ようにした。ここで縛るのは 3 つ:
//!
//! 1. **既定は 1 バイトも変えない** — 書いていないとき・`hosts=1`・知らない値は
//!    今までどおり `hosts[]` を出す
//! 2. `?hosts=0` は **`hosts` の鍵だけ** を落とし、ほかの鍵はそのまま残る
//! 3. `/snapshot` の `parts` と各部の形は変えない (`status` の部にも `hosts[]` が入る)

mod common;
use common::*;

use std::thread;

/// 1 本だけプロキシを通してから返す (`/status` の `hosts[]` に 1 行入る)。
///
/// 集計は**応答を返したあと**に書かれるので、`sleep` を伸ばさず値で待つ
/// (このリポジトリの試験の作法)。
fn proxy_with_one_host() -> (u16, thread::JoinHandle<()>) {
    let (origin_port, handle) = start_mock_origin();
    let port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);
    let body = get_via_proxy(port, &format!("http://{}/light", host), &host);
    assert!(body.contains("hello from mock origin"), "{}", body);
    wait_until(
        || endpoint_json(port, "/status").contains("\"hosts\":[{\"host\":"),
        "hosts[] に 1 行入る",
    );
    (port, handle)
}

/// `?hosts=0` は `hosts` の鍵だけを落とし、ほかの鍵は 1 つも欠けないこと。
#[test]
fn test_integration_status_without_hosts_drops_only_the_hosts_table() {
    let (port, _origin) = proxy_with_one_host();

    let full = endpoint_json(port, "/status");
    let light = endpoint_json(port, "/status?hosts=0");

    // 既定は今までどおり `hosts[]` を出す
    assert!(full.contains("\"hosts\":[{\"host\":"), "{}", full);
    // 軽い方には `hosts` の鍵が無い (`clients[]` の `host` の欄は別物なので残る)
    assert!(!light.contains("\"hosts\":"), "{}", light);

    // ほかの鍵はそのまま (落としたのは表だけで、集計は痩せない)
    for key in [
        "\"schema\":1",
        "\"status\":\"ok\"",
        "\"version\":",
        "\"uptime_secs\":",
        "\"total_requests\":",
        "\"restored_since\":",
        "\"clients\":[{\"client\":",
        "\"cache\":",
        "\"memory\":",
        "\"recent_quantiles\":",
    ] {
        assert!(full.contains(key), "既定に {} が無い: {}", key, full);
        assert!(light.contains(key), "軽い方に {} が無い: {}", key, light);
    }

    // 軽い方が小さい (この時点の `hosts[]` は 1 行だけ。育つほど差が開く)
    assert!(
        light.len() < full.len(),
        "light {} B / full {} B",
        light.len(),
        full.len()
    );
}

/// 既定に倒れる値 (`1` / 空 / 知らない値) と `?sort=` との組み合わせ。
#[test]
fn test_integration_status_keeps_the_hosts_table_unless_hosts_is_zero() {
    let (port, _origin) = proxy_with_one_host();

    for path in [
        "/status",
        "/status?hosts=1",
        "/status?hosts=",
        "/status?hosts=off",
        "/status?sort=errors",
        "/status?hosts=1&sort=dns",
    ] {
        let body = endpoint_json(port, path);
        assert!(
            body.contains("\"hosts\":[{\"host\":"),
            "{} は既定 (全部) のはず: {}",
            path,
            body
        );
    }
    // `0` のときだけ省く。`?sort=` と一緒でも読める (順は問わない)
    for path in [
        "/status?hosts=0",
        "/status?sort=errors&hosts=0",
        "/status?hosts=0&sort=slow",
    ] {
        let body = endpoint_json(port, path);
        assert!(!body.contains("\"hosts\":"), "{} -> {}", path, body);
        assert!(body.contains("\"clients\":["), "{} -> {}", path, body);
    }
}

/// `/snapshot` の `parts` と `status` の部は今までどおりであること。
///
/// 軽い形は `/status?hosts=0` だけの話で、1 要求で全部取る `/snapshot` は変えない。
#[test]
fn test_integration_snapshot_still_carries_the_hosts_table() {
    let (port, _origin) = proxy_with_one_host();

    let snap = endpoint_json(port, "/snapshot");
    // `parts` の一覧に `status` の 3 通りが並ぶ (T14.4 の形のまま)
    for name in ["\"status\"", "\"status_errors\"", "\"status_dns\""] {
        assert!(snap.contains(name), "parts に {} が無い", name);
    }
    // `status` の部に `hosts[]` が入っている (切り出して 1 本の応答として読める)
    let at = snap.find("\"status\":{").expect("status の部");
    let end = snap
        .find("\"status_errors\":")
        .expect("status_errors の部")
        .max(at);
    let status_part = &snap[at..end];
    assert!(
        status_part.contains("\"hosts\":[{\"host\":"),
        "status の部から hosts[] が消えている: {}",
        status_part
    );
}
