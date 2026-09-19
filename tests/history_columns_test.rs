//! `/history` の標本に足した 8 列と `key_kinds` の結合テスト (T15.0 (8) + (10))。
//!
//! 畳み方と `.rrd` の往復そのものは `crates/metrics-window/src/history.rs` の単体テストで
//! 見ている。ここで見るのは**口の配線**:
//!
//! - `keys` が 40 個で、末尾が `waits` … `active_peak` の順に並ぶこと
//! - `key_kinds` が `keys` と**同じ長さ**で、決めた 6 種類しか出ないこと
//! - 標本の 1 行の列数が `keys` と合っていること (入れ子の配列は 1 列)
//! - **`active_peak` が 5 秒より短い山を拾う** (瞬間値の `active_max` は 0 のまま)
//! - `requests_delta` / `bytes_delta` が**その区間だけ**を数え、通算の `requests` /
//!   `bytes` はそのまま残っていること (1 本目の標本は前の通算が無いので 0)
//! - `dns_warm` が `/status` の `dns.warm` と同じ数であること
//!   (**warm な名前を 1 つ作ってから**比べる。名前を引かないと両方 0 で恒等式になる)
//!
//! 周期は `history::spawn_every` で縮める (`tests/rate_test.rs` と同じ作法)。
#![cfg(target_os = "linux")]

use std::time::Duration;

mod common;
use common::*;

/// history スレッドの周期 (本番は 5 秒)。
const TICK: Duration = Duration::from_millis(300);

/// 山として張る本数 (受け入れ基準の 50 本)。
const SPIKE: usize = 50;

/// `key_kinds` に出てよい綴り (`crates/metrics-window/src/history.rs` の `KEY_KINDS`)。
const KINDS: [&str; 6] = ["time", "cumulative", "delta", "gauge", "peak", "buckets"];

/// `"keys":[…]` / `"key_kinds":[…]` の中身を綴りの並びとして取り出す。
fn str_array(json: &str, key: &str) -> Vec<String> {
    let pat = format!("\"{}\":[", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("no {} in {}", key, &json[..200]))
        + pat.len();
    let end = at + json[at..].find(']').expect("unterminated array");
    json[at..end]
        .split(',')
        .map(|s| s.trim_matches('"').to_string())
        .collect()
}

/// 標本の配列の各行を、**入れ子の配列を 1 列と数えて**切り出す。
fn rows(json: &str) -> Vec<Vec<String>> {
    let at = json.find("\"samples\":[").expect("no samples") + "\"samples\":[".len();
    let mut out = Vec::new();
    let (mut depth, mut cur, mut field) = (0usize, Vec::new(), String::new());
    for c in json[at..].chars() {
        match c {
            '[' if depth == 0 => depth = 1,
            '[' => {
                depth += 1;
                field.push(c);
            }
            ',' if depth == 1 => {
                cur.push(std::mem::take(&mut field));
            }
            ']' if depth == 1 => {
                cur.push(std::mem::take(&mut field));
                out.push(std::mem::take(&mut cur));
                depth = 0;
            }
            ']' if depth == 0 => break,
            ']' => {
                depth -= 1;
                field.push(c);
            }
            _ if depth >= 1 => field.push(c),
            _ => {}
        }
    }
    out
}

/// 列の名前で 1 行から値を引く。
fn col(keys: &[String], row: &[String], name: &str) -> u64 {
    let i = keys
        .iter()
        .position(|k| k == name)
        .unwrap_or_else(|| panic!("no column {}", name));
    row[i]
        .parse()
        .unwrap_or_else(|_| panic!("{} = {:?} は数字ではない", name, row[i]))
}

fn history_json(port: u16) -> String {
    endpoint_json(port, "/history?res=5")
}

/// `keys` は 40 個で末尾が新しい 8 列、`key_kinds` は同じ長さで 6 種類しか出ない。
#[test]
fn test_integration_history_keys_carry_the_new_columns_and_their_kinds() {
    let (port, _metrics) = start_test_proxy_with_history(proxy_config(), TICK);
    let json = history_json(port);
    let keys = str_array(&json, "keys");
    let kinds = str_array(&json, "key_kinds");

    assert_eq!(keys.len(), 40, "{:?}", keys);
    assert_eq!(kinds.len(), keys.len(), "keys {:?} kinds {:?}", keys, kinds);
    assert_eq!(
        &keys[32..],
        &[
            "waits",
            "wait_ms_sum",
            "wait_ms_max",
            "wait_buckets",
            "dns_warm",
            "requests_delta",
            "bytes_delta",
            "active_peak",
        ],
        "末尾に足す (既存の 32 列は 1 つも動かさない)"
    );
    // 既存の綴りが 1 つも変わっていないこと (読む側は名前で引く)
    assert_eq!(keys[0], "t");
    assert_eq!(keys[1], "requests");
    assert_eq!(keys[31], "evicted_idle");
    for (k, kind) in keys.iter().zip(&kinds) {
        assert!(KINDS.contains(&kind.as_str()), "{} の kind が {}", k, kind);
    }
    // 取り違えの元だったところ: 通算と区間が隣り合っている
    let kind = |name: &str| kinds[keys.iter().position(|k| k == name).unwrap()].clone();
    assert_eq!(kind("requests"), "cumulative");
    assert_eq!(kind("requests_delta"), "delta");
    assert_eq!(kind("connects"), "delta");
    assert_eq!(kind("dns_warm"), "gauge");
    assert_eq!(kind("active_peak"), "peak");
    assert_eq!(kind("wait_buckets"), "buckets");

    // 標本の 1 行の列数も同じ (入れ子の配列は 1 列)
    let rows = rows(&json);
    assert!(!rows.is_empty(), "標本が 1 本も無い: {}", json);
    for row in &rows {
        assert_eq!(row.len(), keys.len(), "{:?}", row);
    }
}

/// **5 秒より短い山**は瞬間値 (`active_max`) からは見えないが `active_peak` には残る。
#[test]
fn test_integration_active_peak_catches_a_spike_shorter_than_the_sample() {
    let (port, metrics) = start_test_proxy_with_history(proxy_config(), TICK);
    // 1 本目の標本が積まれるまで待つ (`spawn_every` は起こす前に 1 本撮る)
    wait_until(|| !rows(&history_json(port)).is_empty(), "最初の標本");

    // 張ってすぐ閉じる = 標本と標本の間で終わる山。**実際に socket を張らない**のは、
    // 50 本の accept が周期をまたいでしまうと「短い山」にならないため。
    //
    // この山は原子を直に叩いているので、**接続の経路に `inc_active_conn` が居ること**は
    // ここでは縛れない。それを見ているのは `tests/overload_test.rs` (8 本張って
    // `metrics.active_connections` が 8、`/status` の `active_connections` は
    // 9 = 8 本 + `/status` 自身) と `tests/bursts_test.rs` (4 本 → 8 本 → 2 本)。
    // `active_connections` を増やすのは
    // `Metrics::inc_active_conn` だけ (`crates/metrics-core/src/metrics.rs`) なので、
    // あの 2 本が通る限り `Conn::new` (`crates/server/src/lib.rs`) からの呼び出しは生きている
    for _ in 0..SPIKE {
        metrics.inc_active_conn();
    }
    for _ in 0..SPIKE {
        metrics.dec_active_conn();
    }
    // 山はもう終わっている (`/status` を引くとその 1 本が数えられてしまうので、
    // ここは原子を直に読む)
    let now = metrics
        .active_connections
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(now < SPIKE, "まだ {} 本張っている", now);

    let json = history_json(port);
    let keys = str_array(&json, "keys");
    wait_until(
        || {
            rows(&history_json(port))
                .iter()
                .any(|r| col(&keys, r, "active_peak") >= SPIKE as u64)
        },
        "active_peak が山を拾う",
    );

    let json = history_json(port);
    for row in rows(&json) {
        let (peak, max) = (
            col(&keys, &row, "active_peak"),
            col(&keys, &row, "active_max"),
        );
        assert!(peak >= max, "active_peak {} < active_max {}", peak, max);
    }
    // 瞬間値は山を 1 本も見ていない (張って閉じるまでが 1 周期より短いので)
    let seen_max = rows(&json)
        .iter()
        .map(|r| col(&keys, r, "active_max"))
        .max()
        .unwrap_or(0);
    assert!(
        seen_max < SPIKE as u64,
        "active_max が {} で山を見てしまった (この山は周期より短いはず)",
        seen_max
    );
}

/// `requests_delta` / `bytes_delta` は**その区間だけ**。通算の 2 列はそのまま残る。
#[test]
fn test_integration_request_deltas_count_only_their_own_interval() {
    let (origin_port, _origin) = start_mock_origin();
    let (port, metrics) = start_test_proxy_with_history(proxy_config(), TICK);
    wait_until(|| !rows(&history_json(port)).is_empty(), "最初の標本");

    let before = rows(&history_json(port)).len();
    for _ in 0..3 {
        let body = get_via_proxy(
            port,
            &format!("http://127.0.0.1:{}/", origin_port),
            &format!("127.0.0.1:{}", origin_port),
        );
        assert!(body.contains("200"), "{}", body);
    }
    // 要求のあとの標本が 2 本積まれるまで待つ (1 本目で差が出る)
    wait_until(
        || rows(&history_json(port)).len() >= before + 2,
        "要求のあとの標本",
    );

    let json = history_json(port);
    let keys = str_array(&json, "keys");
    let rows = rows(&json);
    // 1 本目は前の通算が無いので 0 (リングから引き算で作っていたらここが狂う)
    assert_eq!(col(&keys, &rows[0], "requests_delta"), 0);
    assert_eq!(col(&keys, &rows[0], "bytes_delta"), 0);

    let total: u64 = rows.iter().map(|r| col(&keys, r, "requests_delta")).sum();
    let last = col(&keys, rows.last().unwrap(), "requests");
    assert!(total >= 3, "区間の合計が {} 件 (3 件は通したはず)", total);
    assert!(
        total <= last,
        "区間の合計 {} が通算 {} を越えた",
        total,
        last
    );
    // 通算の列は今までどおり `total_requests` そのもの (標本を撮ったあとに通した
    // `/history` の 1 本が足されていることがあるので「以下」で見る)
    let now = metrics
        .total_requests
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        last <= now && last >= 3,
        "通算の列 {} が total_requests {} と合わない",
        last,
        now
    );
    // バイトも同じ形 (通算は据え置き、区間はその窓だけ)
    let bytes: u64 = rows.iter().map(|r| col(&keys, r, "bytes_delta")).sum();
    assert!(
        bytes <= col(&keys, rows.last().unwrap(), "bytes"),
        "bytes_delta の合計 {} が通算を越えた",
        bytes
    );
}

/// `/status` の `dns` の中の `warm` (同じ綴りの鍵が他にもあるので切り出してから引く)。
fn status_dns_warm(port: u16) -> u64 {
    let status = status_json(port);
    let dns = &status[status.find("\"dns\":{").expect("no dns in status")..];
    status_number(dns, "warm")
}

/// `dns_warm` は `/status` の `dns.warm` と同じ**件数**。
///
/// **名前を 1 つも引かないと `dns.warm` も列も 0 で、`assert_eq!(0, 0)` の恒等式になる**
/// (`tick.rs` の `dns_warm: …warm_count()` を `0` に書き換えても通ってしまう)。
/// そうならないように、`tests/dns_test.rs` と同じ作法で **warm な名前を 1 つ作ってから**
/// 比べ、どちらも 0 でないことを見る (IP リテラルは表に載らないので、名前で通すこと)。
#[test]
fn test_integration_dns_warm_column_matches_the_status_gauge() {
    // 名前解決の表はプロセスで 1 つ。このテストバイナリで**名前**を引くのはここだけで、
    // 他の 3 本は IP リテラル (表に載らない) なので、鍵は要らない
    rust_http_proxy::dns::set_ttl(Duration::from_secs(60));
    rust_http_proxy::dns::set_warm_window(Duration::from_secs(900));
    rust_http_proxy::dns::clear();

    let (origin_port, _origin) = start_mock_origin();
    let (port, _metrics) = start_test_proxy_with_history(proxy_config(), TICK);
    wait_until(|| !rows(&history_json(port)).is_empty(), "最初の標本");
    assert_eq!(status_dns_warm(port), 0, "まだ名前を引いていない");

    // 直近 900 秒に **2 回**使われた名前が warm になる (1 回目は表に載るだけ)
    let url = format!("http://localhost:{}/", origin_port);
    let host = format!("localhost:{}", origin_port);
    for _ in 0..2 {
        let res = get_via_proxy(port, &url, &host);
        assert!(res.starts_with("HTTP/1.1 200"), "{}", res);
    }
    let warm = status_dns_warm(port);
    assert!(warm > 0, "2 回通しても warm が 0 (この比較は恒等式になる)");

    // warm になったあとの標本を待つ (それより前の標本は 0 のまま = 正しい)
    let keys = str_array(&history_json(port), "keys");
    wait_until(
        || {
            let json = history_json(port);
            rows(&json).last().map(|r| col(&keys, r, "dns_warm")) == Some(warm)
        },
        "dns_warm が warm を写した標本",
    );

    let json = history_json(port);
    let rows = rows(&json);
    let last = col(&keys, rows.last().unwrap(), "dns_warm");
    assert!(last > 0, "最後の標本の dns_warm が 0: {:?}", rows.last());
    assert_eq!(
        last,
        status_dns_warm(port),
        "/status の dns.warm と食い違う: {:?}",
        rows.last()
    );
    for row in &rows {
        let v = col(&keys, row, "dns_warm");
        assert!(
            v == 0 || v == warm,
            "dns_warm が 0 でも {} でもない {}",
            warm,
            v
        );
    }

    rust_http_proxy::dns::clear();
    rust_http_proxy::dns::set_warm_window(rust_http_proxy::dns::WARM);
}
