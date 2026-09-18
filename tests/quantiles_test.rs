//! 直近の窓の正確な分位点 `/status` の `recent_quantiles` の結合テスト (T14.31)。
//!
//! いままでの p50 / p95 は **12 段の区間の内側を線形補間した値**で (T12.4 (3))、
//! 5〜10 ms の区間に入る限り 5.0〜10.0 のどこかを返していた。T14.99 の判定
//! 「平常時の CONNECT 確立 p50 8.3 → 6 ms 以下」を **1 ms 単位で読む**ために、
//! 確立時間そのものを 1,024 本 (CONNECT / forward 別) の環状に持つ。
//!
//! 単体テスト (分位点そのものの定義) は `crates/metrics/src/quantiles.rs` にある。
//! ここで見るのは「**実際に通した接続が `/status` に出てくるか**」だけ:
//! (a) CONNECT と forward が別々に数えられ、p50 ≤ p90 ≤ p99 ≤ max で出る、
//! (b) `--lite` (段階の窓と同じ旗) では 1 本も書かない、
//! (c) `/history?summary=1` (T14.24) にも**別の鍵で**同じ値が添う、
//! (d) `memory.rings.quantiles` が 24 KiB 固定。
//!
//! 3 本目の環 `wait` (利用者が待つ時間 = `queue + client_read + dns + connect`。
//! T15.0 (2)) も同じ 4 点で見る。縛るのは **`wait.n == connect.n`** と
//! **`wait` の各分位点 ≥ `connect` の同じ分位点** (`queue` が 0 でない状況は手元では
//! 作りにくいので、数字そのものは縛らない)。
//!
//! **`--lite` の旗は処理系で 1 つ**なので、この 2 本のテストは鍵を取って順に回す
//! (`tests/*.rs` は 1 ファイル 1 プロセス)。

mod common;

use std::io::Write;
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;

use common::*;

/// `--lite` の旗を取り合わないように、この 2 本は順に回す。
static ORDER: Mutex<()> = Mutex::new(());

/// 通す CONNECT の本数 (環は 1,024 本なので落ちない数)。
const TUNNELS: usize = 12;
/// 通す forward の本数。
const FORWARDS: usize = 8;

/// `"key":{...}` の中身を切り出す (入れ子つき)。
fn object_of(json: &str, key: &str) -> String {
    object_at(json, key, false)
}

/// 同じ名前が入れ子にもあるとき (`memory` は `cache.memory` が先に出る) の**最後の 1 つ**。
fn last_object_of(json: &str, key: &str) -> String {
    object_at(json, key, true)
}

fn object_at(json: &str, key: &str, last: bool) -> String {
    let pat = format!("\"{}\":{{", key);
    let found = if last {
        json.rfind(&pat)
    } else {
        json.find(&pat)
    };
    let at = found.unwrap_or_else(|| panic!("{} が無い: {}", key, json)) + pat.len() - 1;
    let rest = &json[at..];
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
    panic!("{} が閉じていない: {}", key, rest);
}

/// `"key":<数>` を読む (小数も負号も読める)。
fn num(obj: &str, key: &str) -> f64 {
    let pat = format!("\"{}\":", key);
    let at = obj
        .find(&pat)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, obj))
        + pat.len();
    obj[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} が数でない: {}", key, obj))
}

/// 1 本の CONNECT を張って、すぐ閉じる (確立の時間だけを数えさせる)。
fn one_tunnel(proxy_port: u16, origin_port: u16) {
    let mut t = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    t.set_nodelay(true).unwrap();
    t.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    t.write_all(
        format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            origin_port, origin_port
        )
        .as_bytes(),
    )
    .unwrap();
    let head = read_connect_response(&mut t);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
}

/// 5 つの欄が揃っていて、p50 ≤ p90 ≤ p99 ≤ max であること。
fn check_shape(obj: &str, what: &str) -> (f64, f64) {
    let (n, p50, p90, p99, max) = (
        num(obj, "n"),
        num(obj, "p50"),
        num(obj, "p90"),
        num(obj, "p99"),
        num(obj, "max"),
    );
    assert!(num(obj, "window_secs") >= 0.0, "{}: {}", what, obj);
    assert!(n <= 1024.0, "{}: n が 1,024 を超えた: {}", what, obj);
    assert!(
        p50 <= p90 && p90 <= p99 && p99 <= max,
        "{}: p50 ≤ p90 ≤ p99 ≤ max になっていない: {}",
        what,
        obj
    );
    (n, p50)
}

/// 通した CONNECT と forward が、そのまま `/status` の `recent_quantiles` に出ること。
#[test]
fn test_integration_a_status_shows_the_exact_recent_quantiles() {
    let _order = ORDER.lock().unwrap_or_else(|e| e.into_inner());
    // 既定のプロファイル (= `--lite` ではない) で回す。段階の窓と同じ旗 (T14.3)
    rust_http_proxy::profile::set_enabled(true);

    let echo_port = start_echo_server();
    let (origin_port, _origin) = start_mock_origin();
    let (proxy_port, metrics) = start_test_proxy_with_metrics(park_config());

    for _ in 0..TUNNELS {
        one_tunnel(proxy_port, echo_port);
    }
    for _ in 0..FORWARDS {
        let host = format!("127.0.0.1:{}", origin_port);
        let resp = get_via_proxy(proxy_port, &format!("http://{}/", host), &host);
        assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);
    }
    // 統計を書くのはトンネルの終わり (`tunnel::report`) なので、指標を直に見て待つ
    wait_until(
        || metrics.recent_quantiles().0.n >= TUNNELS,
        "the tunnels to be sampled",
    );

    let status = status_json(proxy_port);
    let q = object_of(&status, "recent_quantiles");
    let connect = object_of(&q, "connect");
    let forward = object_of(&q, "forward");

    let (cn, cp50) = check_shape(&connect, "connect");
    assert_eq!(cn as usize, TUNNELS, "CONNECT の本数: {}", connect);
    // loopback でも確立は 0.1 ms 前後かかる = **us で数えているから 0 にならない**
    // (ms 刻みなら全部 0 に潰れて、ベンチの p50 と ±0.05 ms で比べられない)
    assert!(cp50 > 0.0, "確立の p50 が 0: {}", connect);
    assert!(cp50 < 1000.0, "loopback で 1 秒はかからない: {}", connect);

    // 3 本目の環 `wait` (T15.0 (2))。本数は確立と同じで、各分位点は確立以上
    let wait = object_of(&q, "wait");
    let (wn, wp50) = check_shape(&wait, "wait");
    assert_eq!(wn, cn, "`wait` の本数が確立と違う: {}", q);
    assert!(wp50 >= cp50, "wait p50 {} < connect p50 {}", wp50, cp50);
    for key in ["p50", "p90", "p99", "max"] {
        assert!(
            num(&wait, key) >= num(&connect, key),
            "wait {} {} < connect {}: {}",
            key,
            num(&wait, key),
            num(&connect, key),
            q
        );
    }

    let (fnum, _) = check_shape(&forward, "forward");
    // `/status` を取りに行った自分宛ての要求は数えない (`counted` は宛先のホストだけ)
    assert!(
        fnum as usize >= FORWARDS,
        "forward の本数 {} < {}: {}",
        fnum,
        FORWARDS,
        forward
    );

    // `memory.rings.quantiles` は固定 24 KiB (3 系統 × 1,024 本 × 8 B。T15.0 (2))
    let rings = object_of(&last_object_of(&status, "memory"), "rings");
    assert_eq!(num(&rings, "quantiles"), 24576.0, "{}", rings);
    assert!(
        num(&rings, "total") >= 24576.0,
        "合計に入っていない: {}",
        rings
    );

    // `/history?summary=1` (T14.24) にも**別の鍵で**同じ値が添う (`p50_ms` は区間の補間)
    let summary = endpoint_json(proxy_port, "/history?summary=1&since=restart");
    let sq = object_of(&object_of(&summary, "recent_quantiles"), "connect");
    assert_eq!(num(&sq, "n") as usize, TUNNELS, "{}", summary);
    assert!(summary.contains("\"p50_ms\":"), "区間の補間も残ること");
    assert!(
        summary.len() <= 4096,
        "?summary=1 が 4 KiB を超えた: {}",
        summary.len()
    );
}

/// `--lite` (段階の窓と同じ旗) では 1 本も書かない。
#[test]
fn test_integration_b_lite_records_no_samples() {
    let _order = ORDER.lock().unwrap_or_else(|e| e.into_inner());
    rust_http_proxy::profile::set_enabled(false);

    let echo_port = start_echo_server();
    let (proxy_port, metrics) = start_test_proxy_with_metrics(park_config());
    for _ in 0..TUNNELS {
        one_tunnel(proxy_port, echo_port);
    }
    // トンネルが数え終わるのを待つ (ホスト別統計は `--lite` でも生きている)
    let host_key = format!("connect://127.0.0.1:{}", echo_port);
    wait_until(
        || {
            metrics
                .hosts_sorted()
                .iter()
                .any(|(h, s)| *h == host_key && s.requests >= TUNNELS as u64)
        },
        "the tunnels to be counted",
    );

    let (c, f, w) = metrics.recent_quantiles();
    assert_eq!(
        (c.n, c.total, f.n, f.total, w.n, w.total),
        (0, 0, 0, 0, 0, 0),
        "`--lite` で書いた"
    );
    let q = object_of(&status_json(proxy_port), "recent_quantiles");
    assert_eq!(num(&object_of(&q, "connect"), "n"), 0.0, "{}", q);
    assert_eq!(num(&object_of(&q, "connect"), "max"), 0.0, "{}", q);
    // 3 本目も同じ (`queue` と `client_read` が 0 なので「4 段の和」を名乗れない)
    assert_eq!(num(&object_of(&q, "wait"), "n"), 0.0, "{}", q);
    assert_eq!(num(&object_of(&q, "wait"), "max"), 0.0, "{}", q);

    // 後始末 (このプロセスの他のテストを巻き込まない)
    rust_http_proxy::profile::set_enabled(true);
}
