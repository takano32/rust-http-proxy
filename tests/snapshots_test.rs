//! 日次の自動 `/snapshot` 保存 (`$HOME/.rust-http-proxy/snapshots/`) の結合テスト (T14.34)。
//!
//! 見るのは**配線**: (1) 履歴スレッドが UTC の日付をまたいだら `/snapshot` を 1 ファイル
//! 書くこと (1 日待たないので、境目は `snapshots::shift_days_for_test` で差し替える。
//! **組み立てからファイルまでは本物の経路**)、(2) 置いてあるものが `/snapshots` に並び
//! `/snapshots/<date>` でそのまま読めること (`PROXY_ENDPOINTS_READONLY=on` でも読める。T14.18)、
//! (3) `PROXY_STATS_PERSIST=off` では 1 ファイルも書かないこと。
//!
//! 「31 個目で最古が消える」「同じ日に 2 回起動しても 1 ファイル」は
//! `crates/metrics/src/snapshots.rs` の単体テストで見ている (30 日待てないので)。

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{ProxyProcess, endpoint_json, proxy_config, raw_get, start_test_proxy_with_history};
use rust_http_proxy::snapshots;

/// `/snapshot` の上限 (`endpoints::recent::MAX_SNAPSHOT`)。
const MAX_SNAPSHOT: usize = 4 * 1024 * 1024;

fn home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rhp-t1434-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `$HOME/.env` と、置いてある日次の snapshot (架空の 3 日ぶん)。
fn seed(dir: &Path, extra_env: &str, days: &[&str]) -> PathBuf {
    std::fs::write(
        dir.join(".env"),
        format!(
            "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\nPROXY_CACHE_RESERVE=off\n{}",
            extra_env
        ),
    )
    .unwrap();
    let snaps = dir.join(".rust-http-proxy").join("snapshots");
    if !days.is_empty() {
        std::fs::create_dir_all(&snaps).unwrap();
        for day in days {
            std::fs::write(
                snaps.join(format!("{}.json", day)),
                format!(
                    "{{\"taken_at\":1789603200,\"version\":\"0.1.0+test\",\"day\":\"{}\",\
                     \"parts\":[\"status\"],\"dropped\":[],\"status\":{{\"requests\":7}}}}",
                    day
                ),
            )
            .unwrap();
        }
    }
    snaps
}

/// いまの UTC の日付 (`YYYY-MM-DD`)。
fn today() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    snapshots::day_name(now / 86_400)
}

/// 履歴スレッドが日付をまたいだら 1 ファイル書く (中身は `/snapshot` そのもの)。
#[test]
fn test_integration_snapshots_writes_a_file_when_the_day_changes() {
    let dir = home("rollover");
    let snaps = dir.join("snapshots");
    // 書き先と日数を決めてから履歴スレッドを起こす (本番の main.rs と同じ順)
    snapshots::configure(Some(snaps.clone()), snapshots::DEFAULT_DAYS);
    let (port, _metrics) = start_test_proxy_with_history(proxy_config(), Duration::from_millis(50));
    // 中身が空にならないように少しだけ動かす (自分宛ての要求も記録に残る)
    for _ in 0..3 {
        let _ = endpoint_json(port, "/status");
    }
    // 日が変わるまでは 1 つも書かない
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !snaps.exists(),
        "日が変わっていないのに書いている: {:?}",
        std::fs::read_dir(&snaps).map(|d| d.count())
    );

    // 境目を差し替える (テスト用の口。次の標本が「翌日」になるので、**終わった日** =
    // 今日の名前で 1 ファイル書かれる)
    let date = today();
    let path = snaps.join(format!("{}.json", date));
    snapshots::shift_days_for_test(1);
    common::wait_until(|| path.exists(), "日次の snapshot が書かれる");

    let body = std::fs::read_to_string(&path).unwrap();
    println!(
        "T14.34 日次の snapshot: {} は {} B (上限 {} B)",
        path.display(),
        body.len(),
        MAX_SNAPSHOT
    );
    assert!(body.len() <= MAX_SNAPSHOT, "{} B", body.len());
    for key in ["\"taken_at\":", "\"parts\":[", "\"status\":", "\"recent\":"] {
        assert!(body.contains(key), "{} が無い: {}", key, &body[..200]);
    }

    // `/snapshots` に一覧が出て、`/snapshots/<date>` でそのまま読める
    let list = endpoint_json(port, "/snapshots");
    assert!(list.contains(&format!("\"date\":\"{}\"", date)), "{}", list);
    assert!(list.contains("\"count\":1"), "{}", list);
    assert!(
        list.contains(&format!("\"days\":{}", snapshots::DEFAULT_DAYS)),
        "{}",
        list
    );
    assert!(list.contains(&snaps.display().to_string()), "{}", list);
    let served = endpoint_json(port, &format!("/snapshots/{}", date));
    assert_eq!(served, body, "ファイルと同じものを返していない");

    // 同じ日のうちは増えない (履歴スレッドは 50 ms ごとに回り続けている)
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(std::fs::read_dir(&snaps).unwrap().count(), 1);

    // 置いていない日と、日付として読めない名前は 404
    for bad in ["1970-01-02", "..%2F.env", "nope"] {
        let out = raw_get(
            port,
            &format!(
                "GET /snapshots/{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                bad, port
            ),
        );
        assert!(out.starts_with("HTTP/1.1 404 "), "{} -> {}", bad, out);
    }

    // 後始末 (この大域は同じテストバイナリの他のテストも触る)
    snapshots::shift_days_for_test(0);
    snapshots::configure(None, snapshots::DEFAULT_DAYS);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 置いてあるものを実バイナリが一覧して返す (`PROXY_ENDPOINTS_READONLY=on` でも読める)。
#[test]
fn test_integration_snapshots_lists_and_serves_saved_days() {
    let dir = home("list");
    let days = ["2026-09-13", "2026-09-14", "2026-09-15"];
    let snaps = seed(
        &dir,
        "PROXY_ENDPOINTS_READONLY=on\nPROXY_SNAPSHOT_DAYS=7\n",
        &days,
    );
    let proxy = ProxyProcess::start(&dir);

    let list = endpoint_json(proxy.port, "/snapshots");
    assert!(list.contains("\"count\":3"), "{}", list);
    assert!(list.contains("\"shown\":3"), "{}", list);
    assert!(list.contains("\"truncated\":false"), "{}", list);
    // `PROXY_SNAPSHOT_DAYS` がそのまま出る (設定の配線)
    assert!(list.contains("\"days\":7"), "{}", list);
    assert!(list.contains("\"max_bytes\":4194304"), "{}", list);
    assert!(list.contains(&snaps.display().to_string()), "{}", list);
    // 並びは**古い順**
    let first = list.find(days[0]).expect("最初の日が無い");
    let last = list.find(days[2]).expect("最後の日が無い");
    assert!(first < last, "古い順に並んでいない: {}", list);

    // 中身はファイルのものがそのまま
    let body = endpoint_json(proxy.port, &format!("/snapshots/{}", days[1]));
    assert_eq!(
        body,
        std::fs::read_to_string(snaps.join(format!("{}.json", days[1]))).unwrap()
    );
    assert!(body.contains("\"requests\":7"), "{}", body);

    // 置いていない日は 404 (読む口なので `readonly` でも 405 にはならない)
    let out = raw_get(
        proxy.port,
        &format!(
            "GET /snapshots/2026-01-01 HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            proxy.port
        ),
    );
    assert!(out.starts_with("HTTP/1.1 404 "), "{}", out);
    assert!(out.contains("no snapshot for that day"), "{}", out);

    // `/` の案内に載っている
    let listing = endpoint_json(proxy.port, "/");
    assert!(listing.contains("/snapshots"), "{}", listing);
    assert!(listing.contains("/snapshots/<YYYY-MM-DD>"), "{}", listing);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `PROXY_STATS_PERSIST=off` では書き先を持たない (置き場所も作らない)。
#[test]
fn test_integration_snapshots_write_nothing_when_persistence_is_off() {
    let dir = home("off");
    let snaps = seed(&dir, "PROXY_STATS_PERSIST=off\n", &[]);
    let proxy = ProxyProcess::start(&dir);

    let list = endpoint_json(proxy.port, "/snapshots");
    assert!(
        list.contains("\"dir\":null"),
        "書き先を持っている: {}",
        list
    );
    assert!(list.contains("\"count\":0"), "{}", list);
    assert!(list.contains("\"files\":[]"), "{}", list);
    assert!(!snaps.exists(), "置き場所を作っている");

    let _ = std::fs::remove_dir_all(&dir);
}
