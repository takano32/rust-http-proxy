//! `/status` の `memory.baseline` (起動 60 秒の heap の基準) の結合テスト (T24.4)。
//!
//! heap の行は今まで「0 時間の雪像」との差で読んでいたので、起動が変わると (ホストの管理者に
//! 再起動されても) 判定できなくなっていた (T22.99・T23.99 で 2 回続けて「判定できず」)。
//! 起動から 60 秒たった時点の `heap_used + mmap − cache_memory` を状態ファイルに 1 回だけ
//! 残せば、後の雪像 1 枚だけで「この起動でどれだけ増えたか」が読める。
//!
//! ここで見るのは 3 つ:
//!
//! 1. 待ち時間が済むと `memory.baseline` に `{"at":…,"bytes":…}` が出る
//! 2. **1 回だけ**書かれ、そのあと何周期たっても値が動かない
//! 3. `at` は起動の時刻より後 (= 待たずに 0 秒で書いていない)
//!
//! 60 秒を待たずに済むよう、周期は `history::spawn_every` で短く ([`start_test_proxy_with_store`]
//! が 20 ms で起こす) し、待ち時間は `persist::set_baseline_after_secs` で 1 秒にする
//! (このバイナリだけの差し替え)。

mod common;
use common::*;

use std::time::Duration;

/// `/status` の `memory` の節を切り出す (`tests/memory_test.rs` と同じ切り方。
/// **最後の `"memory":{`** を取る — キャッシュの中にも同じ名前の節がある)。
fn memory_of(status: &str) -> String {
    let at = status
        .rfind("\"memory\":{")
        .unwrap_or_else(|| panic!("no memory in {}", status));
    let rest = &status[at + "\"memory\":".len()..];
    let mut depth = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return rest[..=i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("memory is not closed: {}", rest);
}

/// `memory` の `baseline` の値を**そのまま**返す (`null` か `{"at":…,"bytes":…}`)。
fn baseline_of(mem: &str) -> &str {
    let at = mem
        .find("\"baseline\":")
        .unwrap_or_else(|| panic!("no baseline in {}", mem))
        + "\"baseline\":".len();
    let rest = &mem[at..];
    if rest.starts_with("null") {
        return "null";
    }
    match rest.find('}') {
        Some(i) => &rest[..=i],
        None => panic!("baseline is not closed: {}", rest),
    }
}

/// `{"at":N,"bytes":M}` から `(N, M)` を取る。
fn pair(json: &str) -> (u64, u64) {
    let body = json
        .strip_prefix("{\"at\":")
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or_else(|| panic!("baseline is not at/bytes: {}", json));
    let (at, bytes) = body
        .split_once(",\"bytes\":")
        .unwrap_or_else(|| panic!("baseline is not at/bytes: {}", json));
    (at.parse().expect("at"), bytes.parse().expect("bytes"))
}

/// **受け入れ基準**: 起動から（試験では 1 秒）たつと `memory.baseline` が出て、1 回だけ書かれる。
#[test]
fn test_integration_the_baseline_appears_after_the_wait_and_stays_put() {
    // 60 秒を待たない (周期も 20 ms にする)。**このバイナリだけ**の差し替え
    rust_http_proxy::persist::set_baseline_after_secs(1);
    assert_eq!(rust_http_proxy::persist::baseline_after_secs(), 1);

    let dir = std::env::temp_dir().join(format!("rhp-t244-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rrd = dir.join(".rust-http-proxy.rrd");
    let opened_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (port, _metrics, store) =
        start_test_proxy_with_store(proxy_config(), Duration::from_millis(20), rrd.clone());
    let store = store.expect("状態ファイル");

    // 待ち時間が済むまで (出来事で待つ。時間で待たない)
    wait_until(|| store.baseline().is_some(), "起動の基準");
    let mem = memory_of(&status_json(port));
    let first = baseline_of(&mem).to_string();
    let (at, bytes) = pair(&first);
    assert!(bytes > 0, "基準のバイト数が 0: {}", mem);
    assert!(
        at > opened_at,
        "待たずに 0 秒で書いている (at {} <= 起動 {} 秒)",
        at,
        opened_at
    );

    // 何周期たっても動かない (1 回だけ書く)
    std::thread::sleep(Duration::from_millis(300));
    let mem = memory_of(&status_json(port));
    assert_eq!(baseline_of(&mem), first, "1 回だけ書く: {}", mem);
    assert_eq!(store.baseline(), Some((at, bytes)));

    // 状態ファイルは 8 MiB のまま (記録は余白の 1 本。版は上げていない)
    assert_eq!(std::fs::metadata(&rrd).unwrap().len(), 8 * 1024 * 1024);

    let _ = std::fs::remove_dir_all(&dir);
}
