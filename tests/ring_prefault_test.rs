//! 記録のリングを起動時に触ったあとは、満杯にしても RSS がほとんど増えないことの結合テスト (T17.8)。
//!
//! 作り方は `history_depth_test` と同じ (`History::restore` で 5 秒の標本を 6 時間ぶん一度に積む)
//! に、1 分・1 時間の標本、`closed` / `transfer` の窓、`/profile` の窓 (1 日ぶん) も満杯にする。
//!
//! - 触らない `Metrics` (今までどおり): 満杯にすると RSS が数 MB 増える (印字だけ)
//! - 触った `Metrics` ([`Metrics::prefault_rings`]): 増分が **1 MiB 未満** (受け入れ基準)
//! - どちらでも `memory.rings_used` は件数 × 1 件で、触ったぶんは入らない
//!   (`history_depth_test` の「差」が触ったぶんを含まないことの裏付け)
//!
//! RSS を比べるので、**このファイルにはテストを 1 本しか置かない** (同じバイナリの隣のテストが
//! 大きな応答を組むと RSS が揺れる。T17.14 の `history_depth_test` と同じ理由)。

#![cfg(target_os = "linux")]

use rust_http_proxy::history::{CAPACITY, RESOLUTIONS, Sample};
use rust_http_proxy::metrics::Metrics;
use rust_http_proxy::recent::{CloseReason, RecentEntry, SIDES, STAGES};

/// 受け入れ基準: 起動直後 (触ったあと) と満杯にしたあとの RSS の差の上限。
const MAX_RSS_GROWTH: u64 = 1024 * 1024;

/// 標本の並びの起点 (60 で割り切れる適当な epoch)。
const T0: u64 = 1_770_000_000 / 3600 * 3600;

/// 5 秒 × (17,280 + 24) = 1 日と 2 分。`/profile` の 1 分の窓 (1,440 本) と `closed` /
/// `transfer` の 1 分の窓 (1,440 本) が満杯になる長さ (1 分の窓は次の分に入ってから閉じるので、
/// ちょうど 1 日では 1 本足りない)。
const DAY_TICKS: u64 = 24 * 3600 / 5 + 24;

/// いまの RSS (`/proc/self/statm` の 2 つ目 × ページ)。
fn rss() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").expect("statm");
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .expect("statm の 2 つ目");
    pages * 4096
}

/// `/status` の `memory` の `rings_used.total` と `rings_touched`。
fn used_and_touched(m: &Metrics) -> (u64, bool) {
    let json = m.to_json();
    let at = json.rfind("\"rings_used\":").expect("no rings_used");
    let rest = &json[at..];
    let t = rest.find("\"total\":").expect("no total") + "\"total\":".len();
    let total = rest[t..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .expect("total が数ではない");
    let touched = if rest.contains("\"rings_touched\":true") {
        true
    } else {
        assert!(rest.contains("\"rings_touched\":false"), "{}", rest);
        false
    };
    (total, touched)
}

fn closed_entry() -> RecentEntry {
    RecentEntry {
        id: 1,
        at: T0,
        client: "198.51.100.7".to_string(),
        target: "example.net:443".to_string(),
        connect: true,
        secs: 3,
        requests: 0,
        up: 2048,
        down: 4096,
        reason: CloseReason::ClientEof,
        status: 0,
        parked_secs: 0,
        parks: 0,
        stage_ms: [0; STAGES],
        rtt_us: [0; SIDES],
        retrans: [0; SIDES],
        sni: None,
        syn_retrans: 0,
        stall_ms: [0; SIDES],
        spins: 0,
        half_closed: None,
    }
}

/// `/history` (3 解像度の標本と `closed` / `transfer` の窓) と `/profile` の窓を満杯にする。
fn fill(m: &Metrics) {
    // 標本は `history_depth_test` と同じく反復子のまま渡す (確保はリングのぶんだけ)
    for (res, &(secs, _)) in RESOLUTIONS.iter().enumerate() {
        let n = rust_http_proxy::history::History::capacity(res) as u64;
        m.history.restore(
            res,
            (0..n).map(|i| Sample {
                t: T0 + i * secs,
                ..Sample::default()
            }),
        );
    }
    assert_eq!(m.history.len(), CAPACITY);
    let entry = closed_entry();
    for i in 0..DAY_TICKS {
        let t = T0 + i * 5;
        m.history.closed.observe(&entry);
        m.history
            .transfer
            .observe(1024, std::time::Duration::from_millis(20), None, [0; SIDES]);
        m.history.closed.roll(t);
        m.history.transfer.roll(t);
        m.profile.push(rust_http_proxy::profile::Sample {
            t,
            ..Default::default()
        });
    }
    let p = rust_http_proxy::profile::RESOLUTIONS;
    assert_eq!(m.profile.len(0), p[0].1, "/profile の 5 秒の窓が満杯でない");
    assert_eq!(m.profile.len(1), p[1].1, "/profile の 1 分の窓が満杯でない");
    let (fine, minute, _) = m.history.closed.counts();
    assert_eq!((fine, minute), (RESOLUTIONS[0].1, RESOLUTIONS[1].1));
}

/// **受け入れ基準**: 触ったあとは、満杯にしても RSS の増分が 1 MiB 未満。
#[test]
fn test_integration_prefaulted_rings_do_not_grow_the_rss_when_filled() {
    // 1 本目: 今までどおり (触らない)。先に測る (後に回すと 2 本目が空けた置き場を使い回す)
    let plain = Metrics::new();
    let (used0, touched) = used_and_touched(&plain);
    assert!(
        !touched,
        "prefault_rings を呼んでいないのに rings_touched が true"
    );
    let before = rss();
    fill(&plain);
    let plain_growth = rss().saturating_sub(before);

    // 2 本目: 起動時に触る (`--lite` 以外の Linux の `main` と同じ呼び方)
    let touched_m = Metrics::new();
    let start = rss();
    let t = std::time::Instant::now();
    let bytes = touched_m.prefault_rings(true);
    let took = t.elapsed();
    let (used1, touched) = used_and_touched(&touched_m);
    assert!(touched, "rings_touched が立っていない");
    // 触ったぶんは `rings_used` に入らない (件数は 0 のまま)
    assert_eq!(
        used1, used0,
        "触っただけで rings_used が {} → {}",
        used0, used1
    );
    let after_touch = rss();
    fill(&touched_m);
    let after_fill = rss();
    let growth = after_fill.saturating_sub(after_touch);
    let (used_full, _) = used_and_touched(&touched_m);

    println!(
        "触った置き場 {} B ({:.2} MiB) を {:.1} ms。RSS: 触る前 {} → 触ったあと {} (+{} B) → 満杯 {} (+{} B)",
        bytes,
        bytes as f64 / 1_048_576.0,
        took.as_secs_f64() * 1000.0,
        start,
        after_touch,
        after_touch.saturating_sub(start),
        after_fill,
        growth
    );
    println!(
        "触らない場合の満杯までの RSS の増分 {} B ({:.2} MiB)。rings_used.total 満杯 {} B",
        plain_growth,
        plain_growth as f64 / 1_048_576.0,
        used_full
    );
    assert!(
        growth < MAX_RSS_GROWTH,
        "触ったのに満杯までで RSS が {} B 増えた (上限 {} B)",
        growth,
        MAX_RSS_GROWTH
    );
    // 触ったぶんは起動時に RSS に出ている (天井を先に見せる)
    assert!(
        after_touch.saturating_sub(start) + MAX_RSS_GROWTH >= bytes as u64,
        "触ったのに RSS が {} B しか増えていない (触った置き場 {} B)",
        after_touch.saturating_sub(start),
        bytes
    );
}
