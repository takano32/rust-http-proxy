//! 日次の要約 (`$HOME/.rust-http-proxy.daily.jsonl`) を読む口の結合テスト (T14.20)。
//!
//! 日付の境目と「同じ日に 2 回起動しても 1 行」は `crates/metrics/src/daily.rs` の
//! 単体テストで見ている (1 日待てないので)。ここで見るのは**実バイナリの配線**:
//! `/daily` が置いてあるファイルをそのまま返すこと、`?n=` が効くこと、
//! `PROXY_STATS_PERSIST=off` では書き先を持たない (`path` が `null`) こと、
//! `/` の案内に載っていること。

mod common;

use common::{ProxyProcess, endpoint_json, status_number};

/// 個票の上限 (`endpoints::recent::MAX_BODY`)。
const MAX_BODY: usize = 256 * 1024;

fn home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rhp-t1420-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 置いてある要約 (架空の 3 日ぶん)。
fn seed(dir: &std::path::Path, extra_env: &str) {
    std::fs::write(
        dir.join(".env"),
        format!(
            "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\nPROXY_CACHE_RESERVE=off\n{}",
            extra_env
        ),
    )
    .unwrap();
    let mut out = String::new();
    for (i, day) in ["2026-09-13", "2026-09-14", "2026-09-15"]
        .iter()
        .enumerate()
    {
        out.push_str(&format!(
            "{{\"day\":\"{}\",\"t\":{},\"secs\":86395,\"samples\":17279,\"requests\":{},\
             \"bytes\":9876543210,\"connects\":500,\"connect_p50_ms\":8.8,\"connect_p95_ms\":43.8,\
             \"dns_misses\":123,\"dns_per_connect\":0.246,\"dns_miss_ms\":32.4,\"errors\":5,\
             \"bursts\":2,\"active_max\":42,\"evicted_idle\":3,\"rss_max\":58720256,\
             \"rss_avg\":52428800,\"version\":\"0.1.0+test\"}}\n",
            day,
            1_789_257_600u64 + i as u64 * 86_400,
            1_000 + i,
        ));
    }
    std::fs::write(dir.join(".rust-http-proxy.daily.jsonl"), out).unwrap();
}

#[test]
fn test_integration_daily_serves_the_summary_file_and_honours_n() {
    let dir = home("daily");
    seed(&dir, "");
    let proxy = ProxyProcess::start(&dir);

    let body = endpoint_json(proxy.port, "/daily");
    assert!(body.len() <= MAX_BODY, "{} B", body.len());
    assert_eq!(status_number(&body, "count"), 3, "{}", body);
    assert_eq!(status_number(&body, "shown"), 3, "{}", body);
    assert!(body.contains("\"truncated\":false"), "{}", body);
    assert!(body.contains("\"max_line\":512"), "{}", body);
    assert!(body.contains("\"max_bytes\":2097152"), "{}", body);
    assert!(
        body.contains(".rust-http-proxy.daily.jsonl"),
        "書き先が出ていない: {}",
        body
    );
    // 行はファイルのものがそのまま、**古い順**に並ぶ
    let first = body.find("2026-09-13").expect("最初の日が無い");
    let last = body.find("2026-09-15").expect("最後の日が無い");
    assert!(first < last, "古い順に並んでいない: {}", body);
    assert!(body.contains("\"version\":\"0.1.0+test\""), "{}", body);

    // `?n=` は新しい方から数えた日数
    let one = endpoint_json(proxy.port, "/daily?n=1");
    assert_eq!(status_number(&one, "shown"), 1, "{}", one);
    assert!(
        one.contains("2026-09-15") && !one.contains("2026-09-13"),
        "{}",
        one
    );
    assert!(one.contains("\"truncated\":true"), "{}", one);

    // `/` の案内に載っている
    let listing = endpoint_json(proxy.port, "/");
    assert!(listing.contains("/daily?n=365"), "{}", listing);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_integration_daily_writes_nothing_when_persistence_is_off() {
    let dir = home("daily-off");
    seed(&dir, "PROXY_STATS_PERSIST=off\n");
    let proxy = ProxyProcess::start(&dir);

    let body = endpoint_json(proxy.port, "/daily");
    assert!(
        body.contains("\"path\":null"),
        "書き先を持っている: {}",
        body
    );
    assert!(body.contains("\"days\":[]"), "{}", body);
    assert_eq!(status_number(&body, "count"), 0, "{}", body);
    // 置いてあるファイルには手を付けない (消さない・書き足さない)
    let kept = std::fs::read_to_string(dir.join(".rust-http-proxy.daily.jsonl")).unwrap();
    assert_eq!(kept.lines().count(), 3, "{}", kept);

    let _ = std::fs::remove_dir_all(&dir);
}
