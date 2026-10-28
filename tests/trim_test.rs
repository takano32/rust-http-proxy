//! `/status` の `memory.trim` (1 時間に 1 回の `malloc_trim` の通算) の結合テスト (T.30.1)。
//!
//! T.29.25 の `malloc_trim` が返したかは info の 1 行にしか出ず、tokyo では `/log` の 1,000 行が
//! warn で埋まって見えなかった。`/status` の `memory` の末尾に `trim` を足したので、ここで見るのは 2 つ:
//!
//! 1. `history` スレッドが動かない形 (`--lite` / `PROXY_STATS_PERSIST=off` と同じ) では、
//!    欄の形はそのままで `count` / `skipped` が 0、`last_*` が `null`
//! 2. 間隔を縮めた (`sysinfo::set_trim_every_secs`。このバイナリだけの差し替え) `history`
//!    スレッドが 1 回呼ぶと、`count` か `skipped` が増える。返せる空きを作っておけば `count`
//!    が増え、`last_at` / `last_returned_bytes` / `last_us` が数になる

mod common;
use common::*;

use std::time::Duration;

/// `/status` の `memory.trim` の節を切り出す (**最後の `"trim":{`**。`memory` は `/status` の末尾)。
fn trim_of(status: &str) -> String {
    let at = status
        .rfind("\"trim\":{")
        .unwrap_or_else(|| panic!("no memory.trim in {}", status));
    let rest = &status[at + "\"trim\":".len()..];
    let end = rest
        .find('}')
        .unwrap_or_else(|| panic!("trim の括弧が閉じていない: {}", rest));
    rest[..=end].to_string()
}

/// `"key":` の値を取る (`null` は `None`)。
fn num(json: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{}\":", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("no {} in {}", key, json))
        + pat.len();
    let rest = &json[at..];
    if rest.starts_with("null") {
        return None;
    }
    Some(
        rest.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or_else(|_| panic!("{} is not a number in {}", key, json)),
    )
}

/// **受け入れ基準**: 履歴スレッドが無ければ 0 / `null`、間隔を縮めて 1 回呼ばれると数が増える。
#[test]
fn test_integration_memory_trim_counts_after_a_shortened_interval() {
    // 1. 履歴スレッドが無い (`--lite` と同じ) 形。欄の並びは固定で、`memory` の末尾
    let plain = start_test_proxy(proxy_config());
    let status = status_json(plain);
    let mem_at = status.rfind("\"memory\":{").expect("memory");
    assert!(
        status[mem_at..].contains(",\"baseline\":"),
        "trim は baseline の後 (memory の末尾): {}",
        &status[mem_at..]
    );
    let t = trim_of(&status);
    assert!(
        t.starts_with("{\"count\":0,\"skipped\":0,\"last_at\":null,\"last_returned_bytes\":null,\"total_returned_bytes\":0,\"last_us\":null,\"every_secs\":"),
        "履歴スレッドが無ければ 0 / null: {}",
        t
    );
    assert_eq!(num(&t, "every_secs"), Some(3600), "本番の既定は 1 時間");

    // 返せる空きを作る: 天辺に使用中の塊を 1 つ残して、下の 64 MiB (mmap の閾値より小さい塊) を
    // 解放する。`heap_free > heap_used` になるので、間隔が来れば `skipped` ではなく `count` が増える
    let mut blocks: Vec<Vec<u8>> = (0..4096)
        .map(|i| vec![(i % 251) as u8 + 1; 16 << 10])
        .collect();
    let keep = vec![7u8; 16 << 10];
    blocks.clear();
    blocks.shrink_to_fit();

    // 2. 間隔を 1 秒に縮め (**このバイナリだけ**)、周期 20 ms の履歴スレッドを起こす
    rust_http_proxy::sysinfo::set_trim_every_secs(1);
    let (port, _metrics) = start_test_proxy_with_history(proxy_config(), Duration::from_millis(20));
    if rust_http_proxy::sysinfo::malloc_info().is_none() {
        // `mallinfo2` が無い環境 (musl / glibc 2.32 以下 / Linux 以外) は見ない = 数も増えない
        return;
    }
    wait_until(
        || {
            let t = trim_of(&status_json(port));
            num(&t, "count").unwrap() + num(&t, "skipped").unwrap() >= 1
        },
        "trim_if_due が 1 回呼ばれる",
    );
    let t = trim_of(&status_json(port));
    assert_eq!(num(&t, "every_secs"), Some(1), "{}", t);
    let count = num(&t, "count").unwrap();
    assert!(count >= 1, "返せる空きがあるのに呼んでいない: {}", t);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let at = num(&t, "last_at").expect("呼んだら last_at は数");
    assert!(at <= now && at + 60 > now, "last_at {} / now {}", at, now);
    let last = num(&t, "last_returned_bytes").expect("呼んだら last_returned_bytes は数");
    assert!(num(&t, "last_us").is_some(), "{}", t);
    assert!(num(&t, "total_returned_bytes").unwrap() >= last, "{}", t);
    assert_eq!(keep[0], 7);
}
