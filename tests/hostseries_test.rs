//! ホスト別の時系列 `/hosts/series` の結合テスト (T14.22)。
//!
//! `/hosts` は**通算**しか無く (`avg_ms` は 3 日の平均)、`/history` は**全体**しか無い。
//! 「datadog が 15 時に遅かった」「mtalk だけ夜に再送が増えた」はどちらでも読めない。
//! ここで見るのは「実際に通した要求が、上位ホストの 5 分の標本として出てくるか」と
//! 「窓の境目で標本が進むか」だけ。
//!
//! **窓は 5 分では待てない**ので、テスト用の口 (`Metrics::set_host_series_window`) で
//! 1 秒に差し替える。上位の入れ替えを行うのは history スレッドなので、周期も短くして起こす
//! (`history::spawn_every`。`tests/bursts_test.rs` と同じ作法)。

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;

use rust_http_proxy::cache::{Cache, CacheConfig};
use rust_http_proxy::metrics::Metrics;

/// history スレッドの周期 (本番は 5 秒)。
const TICK: Duration = Duration::from_millis(25);
/// 標本の窓 (本番は 5 分)。
const WINDOW_SECS: u64 = 1;
/// 個票の応答の上限 (`endpoints::recent::MAX_BODY`)。
const MAX_BODY: usize = 256 * 1024;

/// 窓 1 秒・履歴スレッド付きのテスト用プロキシ。
fn series_proxy() -> (u16, Arc<Metrics>) {
    let (port, metrics) = start_test_proxy_with_metrics(proxy_config());
    // **履歴スレッドを起こす前に**窓を差し替える (最初の一拍で基準を置くため)
    metrics.set_host_series_window(WINDOW_SECS);
    let cache = Arc::new(Cache::new(CacheConfig::disabled()));
    rust_http_proxy::history::spawn_every(Arc::clone(&metrics), cache, None, TICK);
    (port, metrics)
}

/// このオリジンへ forward の要求を `n` 本通す。
fn get_n(proxy_port: u16, origin_port: u16, n: usize) {
    for _ in 0..n {
        let host = format!("127.0.0.1:{}", origin_port);
        let resp = get_via_proxy(proxy_port, &format!("http://{}/", host), &host);
        assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);
    }
}

/// ホスト表の鍵 (forward は `http://host:port`)。
fn host_key(origin_port: u16) -> String {
    format!("http://127.0.0.1:{}", origin_port)
}

/// `/hosts/series` の JSON から、あるホストの `samples` を取り出す (古い順)。
fn samples_of(json: &str, host: &str) -> Vec<[u64; 5]> {
    let at = json
        .find(&format!("{{\"host\":\"{}\"", host))
        .unwrap_or_else(|| panic!("{} の系列が無い: {}", host, &json[..json.len().min(400)]));
    let start = json[at..].find("\"samples\":[").expect("samples") + at + "\"samples\":[".len();
    let mut depth = 1usize;
    let mut end = start;
    for (i, c) in json[start..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = start + i;
                    break;
                }
            }
            _ => {}
        }
    }
    json[start..end]
        .split("],[")
        .map(|row| {
            let mut it = row.trim_matches(['[', ']']).split(',');
            let mut out = [0u64; 5];
            for v in out.iter_mut() {
                *v = it.next().unwrap().parse().unwrap();
            }
            out
        })
        .collect()
}

/// 空でない標本の (位置, 件数) を並べる。
fn nonempty(rows: &[[u64; 5]]) -> Vec<(usize, u64)> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r[0] > 0)
        .map(|(i, r)| (i, r[0]))
        .collect()
}

/// 受け入れ基準その 1: 2 ホストへ要求を流すと `/hosts/series?top=16` に 2 本の系列と件数が出る。
#[test]
fn test_integration_two_hosts_get_two_series_with_counts() {
    let (origin_a, _a) = start_mock_origin();
    let (origin_b, _b) = start_mock_origin();
    let (proxy_port, metrics) = series_proxy();

    // まだ 1 本も無い (誰も要求を通していない)
    let empty = endpoint_json(proxy_port, "/hosts/series?top=16");
    assert!(empty.contains("\"series\":[]"), "{}", empty);
    assert_eq!(
        status_number(&empty, "window_secs"),
        WINDOW_SECS,
        "{}",
        empty
    );
    assert_eq!(status_number(&empty, "samples"), 288, "{}", empty);
    assert_eq!(status_number(&empty, "slots"), 16, "{}", empty);

    // 1 本ずつ通して、履歴スレッドが 2 つとも上位に入れるのを待つ
    get_n(proxy_port, origin_a, 1);
    get_n(proxy_port, origin_b, 1);
    wait_until(
        || metrics.host_series(None, 16).tracked == 2,
        "two hosts to enter the top 16",
    );

    // 上位に入ってから通したぶんが標本になる
    get_n(proxy_port, origin_a, 3);
    get_n(proxy_port, origin_b, 2);

    let json = endpoint_json(proxy_port, "/hosts/series?top=16");
    assert!(json.len() <= MAX_BODY, "{} B", json.len());
    assert!(json.contains("\"truncated\":false"), "{}", json);
    assert_eq!(status_number(&json, "count"), 2, "{}", json);
    assert_eq!(status_number(&json, "tracked"), 2, "{}", json);
    assert!(
        json.contains("\"keys\":[\"count\",\"ms_sum\",\"ms_max\",\"dns_ms\",\"errors\"]"),
        "{}",
        json
    );
    let a: u64 = nonempty(&samples_of(&json, &host_key(origin_a)))
        .iter()
        .map(|(_, n)| n)
        .sum();
    let b: u64 = nonempty(&samples_of(&json, &host_key(origin_b)))
        .iter()
        .map(|(_, n)| n)
        .sum();
    assert_eq!((a, b), (3, 2), "{}", json);
    // 系列は直近 1 時間の要求数の多い順 (a のほうが多い)
    let first = json.find(&host_key(origin_a)).unwrap();
    let second = json.find(&host_key(origin_b)).unwrap();
    assert!(first < second, "{}", json);

    // `?host=` はその 1 本だけ
    let one = endpoint_json(
        proxy_port,
        &format!("/hosts/series?host={}", host_key(origin_b)),
    );
    assert_eq!(status_number(&one, "count"), 1, "{}", one);
    assert!(one.contains(&host_key(origin_b)), "{}", one);
    assert!(!one.contains(&host_key(origin_a)), "{}", one);

    // 知らないホストは空 (系列を持っているのは上位だけ)
    let none = endpoint_json(proxy_port, "/hosts/series?host=http://no.such.host:80");
    assert!(none.contains("\"series\":[]"), "{}", none);
    assert_eq!(status_number(&none, "tracked"), 2, "{}", none);
}

/// 受け入れ基準その 2: 窓の境目で標本が進む (前の窓の標本はそのまま残る)。
#[test]
fn test_integration_samples_move_on_at_the_window_boundary() {
    let (origin, _o) = start_mock_origin();
    let (proxy_port, metrics) = series_proxy();

    get_n(proxy_port, origin, 1);
    wait_until(
        || metrics.host_series(None, 16).tracked == 1,
        "the host to enter the top 16",
    );

    get_n(proxy_port, origin, 2);
    let before = samples_of(
        &endpoint_json(proxy_port, "/hosts/series?top=16"),
        &host_key(origin),
    );
    let filled = nonempty(&before);
    assert_eq!(
        filled.iter().map(|(_, n)| n).sum::<u64>(),
        2,
        "{:?}",
        filled
    );
    let (last_idx, last_count) = *filled.last().unwrap();

    // 窓の境目を 1 つ越える (履歴スレッドが標本を進める)
    let rolls = metrics.host_series(None, 16).rotations;
    wait_until(
        || metrics.host_series(None, 16).rotations > rolls,
        "the window to roll over",
    );
    get_n(proxy_port, origin, 3);

    let json = endpoint_json(proxy_port, "/hosts/series?top=16");
    let after = samples_of(&json, &host_key(origin));
    let filled = nonempty(&after);
    assert_eq!(
        filled.iter().map(|(_, n)| n).sum::<u64>(),
        5,
        "{:?}",
        filled
    );
    // 前の窓の標本は動かず (位置は窓が進んだぶん前へずれる)、新しいぶんは**後ろの標本**に入る
    let (new_idx, _) = *filled.last().unwrap();
    let (old_idx, old_count) = *filled.first().unwrap();
    assert!(new_idx > old_idx, "標本が進んでいない: {:?}", filled);
    assert_eq!(
        old_count, last_count,
        "前の窓の標本が書き換わった: {:?}",
        filled
    );
    assert!(
        old_idx < last_idx,
        "古い標本が前へずれていない: {} -> {}",
        last_idx,
        old_idx
    );
    // 合計も出る (`total` は `keys` と同じ順)
    assert!(json.contains("\"total\":[5,"), "{}", json);
    assert!(json.len() <= MAX_BODY, "{} B", json.len());
}

/// `/` の案内に載っていること (共通の決まり)。
#[test]
fn test_integration_the_endpoint_is_listed_on_the_index() {
    let proxy_port = start_test_proxy(proxy_config());
    let body = raw_get(
        proxy_port,
        &format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            proxy_port
        ),
    );
    assert!(
        body.contains("/hosts/series?top=16&host=<name>"),
        "{}",
        body
    );
}
