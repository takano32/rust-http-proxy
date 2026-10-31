//! `/status` の `memory` (RSS の内訳) の結合テスト (T14.21)。
//!
//! 見るのは 4 つ:
//!
//! 1. `memory.rss` は今読んだ値で、同じ読みの `rss_anon + rss_file + rss_shmem` と 1 頁以内で
//!    合い、`stacks_resident` は予約より小さいこと (T.32.4。それまでは実バイナリでは
//!    `cache.system.process_rss_bytes` と同じプローブの値で、起動直後は古かった)
//! 2. プローブが止まっているとき (テスト用プロキシ) は、その場で読んだ**今の** RSS が出ること
//! 3. `mallinfo2` が読める環境では `heap_used + heap_free + mmap ≤ rss × 1.1`、
//!    読めない環境 (musl / glibc 2.32 以下 / Linux 以外) では 3 つとも `null`
//! 4. `arenas` は `mallopt(M_ARENA_MAX)` で実際に掛けた上限 (`PROXY_MALLOC_ARENAS`)
//! 5. `rings_touched` は実バイナリ (既定のプロファイル、Linux) だけ `true` (T17.8)

use std::time::Duration;

mod common;
use common::*;

/// `"key":` の値を取る (`null` は `None`)。`memory` の節だけを渡して使う。
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

/// `/status` の `memory` の節を切り出す。
///
/// **最後の `"memory"` を取る**: キャッシュの中にも同じ名前の節がある
/// (`cache.memory` = キャッシュのメモリ層)。T14.21 の節は `/status` の末尾。
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
    panic!("memory の括弧が閉じていない: {}", rest);
}

/// `memory` の `,"trim":{…}` (T.30.1) から後ろを外した手前を返す。`trim` の後ろは常駐の内訳の
/// 4 つ (T.32.4) で、それが `memory` の最後の鍵であることも見る。
fn without_trim(mem: &str) -> &str {
    let (head, tail) = mem
        .rsplit_once(",\"trim\":{")
        .unwrap_or_else(|| panic!("trim が無い: {}", mem));
    let (trim, rest) = tail
        .split_once('}')
        .unwrap_or_else(|| panic!("trim が閉じていない: {}", mem));
    assert!(
        trim.starts_with("\"count\":") && !trim.contains('{'),
        "trim の形が違う: {}",
        mem
    );
    assert!(
        rest.starts_with(",\"rss_anon\":")
            && rest.contains(",\"rss_file\":")
            && rest.contains(",\"rss_shmem\":")
            && rest.contains(",\"stacks_resident\":")
            && rest.ends_with('}')
            && !rest.contains('{'),
        "trim の後ろが常駐の内訳の 4 つではない: {}",
        mem
    );
    head
}

/// 常駐の内訳 (T.32.4): `rss_anon + rss_file + rss_shmem` は同じ読みの `rss` に 1 頁以内で合い、
/// スタックの常駐は予約 (`stacks_estimate`) より小さい。
fn check_resident(mem: &str) {
    let rss = num(mem, "rss").expect("rss");
    if let (Some(a), Some(f), Some(sh)) = (
        num(mem, "rss_anon"),
        num(mem, "rss_file"),
        num(mem, "rss_shmem"),
    ) {
        assert!(
            (a + f + sh).abs_diff(rss) <= 4096,
            "rss_anon + rss_file + rss_shmem が rss と合わない: {}",
            mem
        );
    }
    if let (Some(res), Some(est)) = (num(mem, "stacks_resident"), num(mem, "stacks_estimate")) {
        assert!(res > 0 && res < est, "{}", mem);
    }
}

/// `memory` の `rings` / `rings_used` の 1 つを切り出す (同じ鍵が 2 度出るので必要。T15.0 (13))。
fn sub_object(mem: &str, key: &str) -> String {
    let pat = format!("\"{}\":{{", key);
    let at = mem
        .find(&pat)
        .unwrap_or_else(|| panic!("no {} in {}", key, mem));
    let rest = &mem[at + pat.len() - 1..];
    let end = rest
        .find('}')
        .unwrap_or_else(|| panic!("{} の括弧が閉じていない: {}", key, rest));
    rest[..=end].to_string()
}

/// `rings` と `rings_used` が持つ鍵 (`total` を除く。この順でなくてよい)。
///
/// `quantiles` は直近 1,024 本の標本の環状 (T14.31)、`readers` は内部エンドポイントを
/// 引いた接続元の表 (環状ではない。T14.53)、`profile` は段階とスレッドの窓
/// (T14.3。T14.21 が落としていたのを T15.0 (13) で足した)。
const RING_KEYS: [&str; 11] = [
    "recent",
    "errors",
    "bursts",
    "log",
    "events",
    "trace",
    "history",
    "hostseries",
    "quantiles",
    "readers",
    "profile",
];

/// `null` を含めて `process_rss_bytes` を文字列で取る。
fn cache_rss(status: &str) -> String {
    status
        .split_once("\"process_rss_bytes\":")
        .map(|(_, r)| r.chars().take_while(|c| *c != ',').collect::<String>())
        .expect("cache の system に process_rss_bytes がある")
}

/// ヒープの 3 つは「揃って読める」か「揃って null」か。読めるなら RSS に収まる。
fn check_heap(mem: &str, rss: Option<u64>) {
    let heap = rust_http_proxy::sysinfo::malloc_info();
    let (used, free, mmap) = (
        num(mem, "heap_used"),
        num(mem, "heap_free"),
        num(mem, "mmap"),
    );
    assert_eq!(heap.is_some(), used.is_some(), "{}", mem);
    assert_eq!(heap.is_some(), free.is_some(), "{}", mem);
    assert_eq!(heap.is_some(), mmap.is_some(), "{}", mem);

    if let (Some(rss), Some(used), Some(free), Some(mmap)) = (rss, used, free, mmap) {
        assert!(used > 0, "使っているヒープが 0: {}", mem);
        // ヒープは RSS に収まる (`heap_free` と、触っていない `mmap` のページは常駐しないので
        // ふつうは RSS より小さく出る。1 割の余裕は読んだ時刻の差のぶん)
        assert!(
            (used + free + mmap) as f64 <= rss as f64 * 1.1,
            "ヒープ {} + {} + {} が RSS {} を超える: {}",
            used,
            free,
            mmap,
            rss,
            mem
        );
    }
}

/// RSS の内訳が、同じ `/status` の他の数字と辻褄が合っていること。
///
/// テスト用プロキシはキャッシュのプローブを回さない (`probe_interval` が 0) ので、
/// `memory.rss` は**その場で読んだ今の値**になる (`process_rss_bytes` は起動時の 1 回きりで
/// 古い)。プローブが回っているときに一致することは下のテストで見る。
#[test]
fn test_integration_memory_breakdown_agrees_with_the_rest_of_status() {
    let port = start_test_proxy_with_cache(proxy_config(), cache_cfg("rhp-t1421-memory"));
    let status = status_json(port);
    let mem = memory_of(&status);

    // (1) その場で読んだ RSS = このテストのプロセスの RSS (同じプロセスなので近い値)
    let rss = num(&mem, "rss");
    let fresh = rust_http_proxy::sysinfo::process_rss();
    assert_eq!(rss.is_some(), fresh.is_some(), "{}", mem);
    if let (Some(rss), Some(fresh)) = (rss, fresh) {
        assert!(
            rss.abs_diff(fresh) < fresh / 4,
            "memory.rss {} が今の RSS {} と離れすぎ: {}",
            rss,
            fresh,
            mem
        );
    }

    // (2) ヒープと常駐の内訳 (T.32.4)
    check_heap(&mem, rss);
    check_resident(&mem);

    // (3) スタックの予約は「接続スレッド 256 KiB + それ以外 2 MiB」
    let threads = status_number(&status, "threads");
    let live = status_number(&status, "live_threads");
    if threads > 0 {
        let conn = live.min(threads);
        assert_eq!(
            num(&mem, "stacks_estimate"),
            Some(conn * 256 * 1024 + (threads - conn) * 2 * 1024 * 1024),
            "threads={} live={}: {}",
            threads,
            live,
            mem
        );
    }

    // (4) リングの容量は足し算が合っていて、`/status` を太らせない大きさに収まっている
    let rings_json = sub_object(&mem, "rings");
    let rings: u64 = RING_KEYS
        .iter()
        .map(|k| num(&rings_json, k).unwrap_or_else(|| panic!("{} が null: {}", k, rings_json)))
        .sum();
    assert_eq!(num(&rings_json, "total"), Some(rings), "{}", rings_json);
    assert!(rings > 0 && rings < 64 * 1024 * 1024, "{}", mem);
    assert!(mem.len() < 1024, "{} バイト: {}", mem.len(), mem);

    // (4b) いま埋まっているぶんは同じ鍵で、どれも容量を越えない (T15.0 (13))
    let used_json = sub_object(&mem, "rings_used");
    let used: u64 = RING_KEYS
        .iter()
        .map(|k| {
            let cap = num(&rings_json, k).unwrap();
            let now = num(&used_json, k).unwrap_or_else(|| panic!("{} が null: {}", k, used_json));
            assert!(now <= cap, "rings_used.{} {} > rings.{} {}", k, now, k, cap);
            now
        })
        .sum();
    assert_eq!(num(&used_json, "total"), Some(used), "{}", used_json);
    assert!(
        used <= rings,
        "rings_used.total {} > rings.total {}",
        used,
        rings
    );

    // (5) キャッシュは空なのでヒープには何も持っていない。`mallopt` を掛けるのは
    // `main.rs` だけなので、この場では `0` (= glibc の既定のまま) が正しい
    assert_eq!(num(&mem, "cache_memory"), Some(0), "{}", mem);
    assert_eq!(num(&mem, "arenas"), Some(0), "{}", mem);

    // (6) リングを起動時に触るのは `main` だけ (T17.8)。テスト用プロキシは触らないので `false`。
    // 鍵は `memory` の末尾に足した (`rings_used` までの並びは動かさない)。
    // **その後ろに `baseline` が付く** (状態ファイルを持たないテスト用プロキシなので `null`。T24.4)。
    // さらに後ろが `trim` (T.30.1。中身は `tests/trim_test.rs`)
    let head = without_trim(&mem);
    assert!(
        head.ends_with("},\"rings_touched\":false,\"baseline\":null"),
        "rings_touched と baseline が末尾に無い: {}",
        mem
    );
}

/// 実バイナリでは、`memory.rss` が今読んだ値で常駐の内訳と合い (T.32.4。それまでは
/// `cache.system.process_rss_bytes` と同じ値で、起動直後は古かった)、
/// `arenas` に `PROXY_MALLOC_ARENAS` が出る。
///
/// どちらも実バイナリでしか見られない: プローブのスレッドを起こすのも、
/// `mallopt(M_ARENA_MAX)` をスレッドを作る前に 1 回掛けるのも `main.rs` の仕事。
#[test]
fn test_integration_memory_rss_matches_the_probe_in_the_real_binary() {
    let dir = std::env::temp_dir().join(format!("rhp-t1421-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_MALLOC_ARENAS=4\nPROXY_CACHE_PROBE_SECS=1\n",
    )
    .unwrap();
    let proxy = ProxyProcess::start(&dir);
    // プローブを 1 周させる (回っていることを見たいので 1 周期待つ)
    std::thread::sleep(Duration::from_millis(1200));

    let status = endpoint_json(proxy.port, "/status");
    let mem = memory_of(&status);
    let rss = num(&mem, "rss");
    // プローブの値も出ている (同じ時刻の読みではないので一致は見ない)
    assert_ne!(cache_rss(&status), "null", "{}", status);
    check_resident(&mem);
    assert!(rss.unwrap_or(0) > 0, "{}", mem);
    check_heap(&mem, rss);
    assert_eq!(num(&mem, "arenas"), Some(4), "{}", mem);
    // 起動しただけのプロキシでも、リングの容量とスタックの予約は出ている
    assert!(num(&mem, "total").unwrap_or(0) > 0, "{}", mem);
    assert!(num(&mem, "stacks_estimate").unwrap_or(0) > 0, "{}", mem);
    // 既定のプロファイルの Linux では起動時にリングを触っている (T17.8)
    assert_eq!(
        without_trim(&mem).ends_with(",\"rings_touched\":true,\"baseline\":null"),
        cfg!(target_os = "linux"),
        "{}",
        mem
    );

    drop(proxy);
    let _ = std::fs::remove_dir_all(&dir);
}
