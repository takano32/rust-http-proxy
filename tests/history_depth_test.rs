//! `/history?res=5` が 5 秒の標本を **6 時間ぶん**持つことの結合テスト (T14.32)。
//!
//! 頭打ちそのもの (5,000 回進めて 4,320 本) は `crates/metrics/src/history.rs` の
//! 単体テストで見ている。ここで見るのは**口の配線と、伸ばしていないもの**:
//!
//! - `/history?res=5` の**既定は今までどおり 720 本** (応答の大きさを変えない)
//! - `?n=4320` で 6 時間ぶん (4,320 本) 返り、`?n=` は上下に丸まる
//! - `?res=60` / `?res=3600` は 1 本も変えない
//! - **`.rrd` は 8,388,608 B のまま**で、書くのも読み戻すのも今までどおり最新 720 本
//! - メモリの増分 (`Sample` の `size_of` × 4,320 と、満杯にしたときの RSS) が 2.5 MiB 以下

mod common;

use common::*;
use rust_http_proxy::history::{CAPACITY, DEFAULT_N, History, RESOLUTIONS, Sample};

/// 受け入れ基準のメモリ上限 (2.5 MiB)。
const MAX_GROWTH: usize = 2_621_440;

/// `.rrd` の固定の大きさ (版 3。T14.14)。
const RRD_SIZE: u64 = 8 * 1024 * 1024;

/// 標本の並びの起点 (5 で割り切れる適当な epoch)。
const T0: u64 = 1_770_000_000;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rhp-t1432-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `"samples":[[..],[..]]` の行数 (入れ子の配列は 1 列として数えない)。
/// 最初の `"samples":[` = 標本の配列 (`closed` / `transfer` / `canary` / `kernel` の
/// 窓はその後ろに別の配列で付く)。
fn rows(json: &str) -> usize {
    let at = json.find("\"samples\":[").expect("no samples") + "\"samples\":[".len();
    let (mut depth, mut n) = (0usize, 0usize);
    for c in json[at..].chars() {
        match c {
            '[' => {
                depth += 1;
                if depth == 1 {
                    n += 1;
                }
            }
            ']' if depth == 0 => break,
            ']' => depth -= 1,
            _ => {}
        }
    }
    n
}

/// 標本の行の先頭 (= その行の `t`)。
fn first_t(json: &str) -> u64 {
    let at = json.find("\"samples\":[[").expect("no samples") + "\"samples\":[[".len();
    json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .expect("t が数ではない")
}

/// 時刻だけ決めた標本 (中身は 0 でよい。ここで見るのは本数と切り口)。
fn sample(t: u64) -> Sample {
    Sample {
        t,
        ..Sample::default()
    }
}

/// **受け入れ基準**: `res=5&n=4320` が 4,320 本まで返し、既定は今までどおり 720 本。
#[test]
fn test_integration_history_res5_keeps_six_hours_and_defaults_to_720() {
    let (port, metrics) = start_test_proxy_with_metrics(proxy_config());
    // 5 秒の標本を 5,000 本 (6 時間より多く) 積む → リングは 4,320 本で頭打ち
    metrics
        .history
        .restore(0, (0..5_000u64).map(|i| sample(T0 + i * 5)));
    let newest = T0 + 4_999 * 5;

    // 既定は今までどおり 720 本 (1 時間ぶん。応答の大きさを変えない)
    let body = endpoint_json(port, "/history?res=5");
    assert_eq!(rows(&body), DEFAULT_N, "既定が {} 本", rows(&body));
    assert_eq!(first_t(&body), newest - (DEFAULT_N as u64 - 1) * 5);
    assert!(body.contains(&format!("[{},", newest)), "最新の標本が無い");
    println!(
        "/history?res=5 の既定: {} 標本 / {} B ({} 〜 {})",
        rows(&body),
        body.len(),
        first_t(&body),
        newest
    );

    // `n=4320` で 6 時間ぶん (4,320 本 × 5 秒 = 21,600 秒)
    let six = endpoint_json(port, "/history?res=5&n=4320");
    assert_eq!(rows(&six), CAPACITY, "n=4320 で {} 本", rows(&six));
    assert_eq!(newest - first_t(&six), (CAPACITY as u64 - 1) * 5);
    assert_eq!(newest - first_t(&six), 6 * 3600 - 5, "6 時間ぶん");
    println!(
        "/history?res=5&n=4320: {} 標本 / {} B ({} 秒ぶん)",
        rows(&six),
        six.len(),
        newest - first_t(&six) + 5
    );

    // 上は 4,320 本で頭打ち、下は 1 本 (`/errors?n=` などと同じ作法)
    assert_eq!(
        rows(&endpoint_json(port, "/history?res=5&n=99999")),
        CAPACITY
    );
    assert_eq!(rows(&endpoint_json(port, "/history?res=5&n=0")), 1);
    assert_eq!(
        rows(&endpoint_json(port, "/history?res=5&n=abc")),
        DEFAULT_N
    );
    assert_eq!(rows(&endpoint_json(port, "/history?res=5&n=100")), 100);
    // 切るのは**新しい方から** (いちばん新しい標本は必ず入る)
    let one = endpoint_json(port, "/history?res=5&n=1");
    assert_eq!(rows(&one), 1);
    assert_eq!(first_t(&one), newest);

    // 1 分と 1 時間は 1 本も変えない (既定で全部返る)
    for res in [1usize, 2] {
        let secs = RESOLUTIONS[res].0;
        let cap = RESOLUTIONS[res].1;
        metrics
            .history
            .restore(res, (0..cap as u64).map(|i| sample(T0 + i * secs)));
        let body = endpoint_json(port, &format!("/history?res={}", secs));
        assert_eq!(rows(&body), cap, "res={} が {} 本", secs, rows(&body));
        assert_eq!(History::capacity(res), cap);
    }
}

/// **受け入れ基準**: `.rrd` の大きさが変わらず (8,388,608 B)、書くのも読み戻すのも
/// 今までどおり最新 720 本。
#[test]
fn test_integration_the_state_file_still_keeps_only_720_five_second_samples() {
    let dir = temp_dir("rrd");
    let rrd = dir.join(".rust-http-proxy.rrd");
    let (_port, metrics) = start_test_proxy_with_metrics(proxy_config());
    // 履歴スレッドは起こさない (標本はこのテストが手で積んで、手で書く)
    let (store, _) = rust_http_proxy::persist::Store::open(rrd.clone()).expect("状態ファイル");

    // `.rrd` の 720 本より多く (1,000 本 = 83 分) 書く
    for i in 0..1_000u64 {
        let pushed = metrics.history.push(sample(T0 + i * 5));
        store.write_samples(&pushed);
    }
    assert_eq!(
        std::fs::metadata(&rrd).unwrap().len(),
        RRD_SIZE,
        ".rrd の大きさが変わっている"
    );
    // メモリには 1,000 本ぜんぶ残っている (6 時間ぶんのリング)
    assert_eq!(metrics.history.len(), 1_000);

    // 読み戻すと最新 720 本だけ (ファイルの形は 1 バイトも変えていない)
    let (_, loaded) = rust_http_proxy::persist::Store::open(rrd.clone()).expect("読み戻せない");
    assert_eq!(loaded.size, RRD_SIZE);
    assert_eq!(loaded.history[0].len(), RESOLUTIONS[0].1, "5 秒の標本");
    assert_eq!(loaded.history[0].first().unwrap().t, T0 + (1_000 - 720) * 5);
    assert_eq!(loaded.history[0].last().unwrap().t, T0 + 999 * 5);

    // 読み戻した 720 本はリングの末尾に入る (残り 3,600 本ぶんは空いたまま)
    let h = rust_http_proxy::history::History::default();
    h.restore(0, loaded.history[0].clone());
    assert_eq!(h.len(), RESOLUTIONS[0].1);
    assert_eq!(rows(&h.to_json_res_n(0, Some(CAPACITY))), RESOLUTIONS[0].1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// **受け入れ基準**: 増えるメモリは `Sample` の `size_of` × 4,320 だけで 2.5 MiB 以下。
/// 満杯にしたときの RSS の増分も 2.5 MiB 以下 (debug で見る)。
#[test]
fn test_integration_six_hours_of_samples_cost_less_than_2_5_mib() {
    let one = size_of::<Sample>();
    let full = one * CAPACITY;
    let grown = one * (CAPACITY - RESOLUTIONS[0].1);
    println!(
        "Sample {} B × {} = {} B ({:.2} MiB)。720 本からの増分 {} B ({:.2} MiB)",
        one,
        CAPACITY,
        full,
        full as f64 / 1_048_576.0,
        grown,
        grown as f64 / 1_048_576.0
    );
    assert!(full <= MAX_GROWTH, "5 秒のリングが満杯で {} B", full);

    let (port, metrics) = start_test_proxy_with_metrics(proxy_config());
    // `/status` を 1 回通してからの差で見る (最初の 1 回で組み立て用の確保が済む)
    let warm = status_json(port);
    assert!(warm.contains("\"rings\":"), "{}", warm);
    let rings = status_number(&status_json(port), "history");
    println!("memory.rings.history = {} B", rings);
    assert!(
        rings >= full as u64,
        "rings.history {} が標本のぶん {} に届いていない",
        rings,
        full
    );

    let before = status_number(&status_json(port), "rss");
    // 6 時間ぶんを一度に積む (`Vec` を作らず反復子のまま渡すので、確保はリングのぶんだけ)
    metrics
        .history
        .restore(0, (0..CAPACITY as u64).map(|i| sample(T0 + i * 5)));
    assert_eq!(metrics.history.len(), CAPACITY);
    let after = status_number(&status_json(port), "rss");
    let delta = after.saturating_sub(before);
    println!(
        "RSS {} → {} B (増分 {} B = {:.2} MiB)",
        before,
        after,
        delta,
        delta as f64 / 1_048_576.0
    );
    assert!(
        delta <= MAX_GROWTH as u64,
        "RSS の増分が {} B (2.5 MiB 超)",
        delta
    );
}
