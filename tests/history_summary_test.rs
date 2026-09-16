//! `/history?since=&until=&summary=1` の結合テスト (T14.24)。
//!
//! 畳み方そのもの (T14.0 の表と同じ数字になる架空の標本列、解像度の自動選択、4 KiB) は
//! `crates/metrics/src/history.rs` の単体テストで見ている。ここで見るのは**実際の口の配線**:
//! HTTP で叩いたときに要約だけが返ること、`normal_hours_only=1` がバーストを外すこと、
//! **`since=restart` が `/status` の `since_start_secs` と合う**こと、
//! `summary` を付けない `/history` が 1 バイトも変わっていないこと。

mod common;

use common::*;
use rust_http_proxy::history::Sample;

/// 要約の上限 (`history::summary::MAX_BODY`)。
const MAX_BODY: usize = 4 * 1024;

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// JSON の `"key":<数>` を読む (小数も負も無いので `status_number` で足りない分だけ)。
fn number(json: &str, key: &str) -> f64 {
    let pat = format!("\"{}\":", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, json))
        + pat.len();
    json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} が数ではない: {}", key, json))
}

/// T14.0 の平常時と同じ数字 (p50 8.3 / p95 80.7 ms) になる架空の 1 時間の標本列。
///
/// 4,000 本を \[2,5\] 1,670・\[5,10\] 500・\[25,50\] 1,323・\[50,100\] 500・\[100,250\] 7 本に
/// 置くと、p50 = 5 + 5 × (2000 − 1670)/500 = **8.3**、
/// p95 = 50 + 50 × (3800 − 3493)/500 = **80.7**。平常時の閾 (1 時間 300 本) を越えないように
/// 1 標本 200 本ずつ 20 時間に分け、そのあとにバーストの 2 時間 (1 時間 400 本、500 ms) を足す。
fn t14_0_hours(t0: u64) -> Vec<Sample> {
    let mut normal: Vec<Sample> = (0..20)
        .map(|i| {
            let mut s = Sample {
                t: t0 + i * 3600,
                ..Sample::default()
            };
            s.dns_misses = 110;
            s.dns_ms_sum = 1265; // 11.5 ms/ミス
            s.active = 8;
            s.active_max = 8;
            s
        })
        .collect();
    let (mut idx, mut in_this) = (0usize, 0u64);
    for (ms, n) in [(3u64, 1670u64), (8, 500), (30, 1323), (60, 500), (250, 7)] {
        for _ in 0..n {
            normal[idx].connect.observe(ms);
            in_this += 1;
            if in_this == 200 && idx + 1 < normal.len() {
                idx += 1;
                in_this = 0;
            }
        }
    }
    for i in 0..2u64 {
        let mut s = Sample {
            t: t0 + (20 + i) * 3600,
            active: 218,
            active_max: 218,
            errors: 99,
            ..Sample::default()
        };
        s.errors_by_cause[0] = 99;
        for _ in 0..400 {
            s.connect.observe(500);
        }
        normal.push(s);
    }
    normal
}

/// 置いた標本列から、HTTP 越しに T14.0 の表と同じ p50 / p95 が返ること。
#[test]
fn test_integration_history_summary_folds_the_period_into_one_row() {
    let (port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let t0 = 1_757_000_000 - 1_757_000_000 % 3600;
    let last = t0 + 21 * 3600;
    metrics.history.restore(2, t14_0_hours(t0));

    let url = format!(
        "/history?summary=1&since={}&until={}&res=3600&normal_hours_only=1",
        t0, last
    );
    let body = endpoint_json(port, &url);
    println!("{} ->\n{} ({} B)", url, body, body.len());
    assert!(body.len() <= MAX_BODY, "{} B (4 KiB 超)", body.len());
    // 標本そのものは返さない (`/history` は 284 KB、これは 1 行)
    assert!(!body.contains("\"keys\":"), "{}", body);
    assert_eq!(number(&body, "from"), t0 as f64, "{}", body);
    assert_eq!(number(&body, "to"), last as f64, "{}", body);
    assert_eq!(number(&body, "interval_secs"), 3600.0, "{}", body);
    assert_eq!(number(&body, "samples"), 20.0, "{}", body);
    assert_eq!(number(&body, "burst_samples"), 2.0, "{}", body);
    assert_eq!(number(&body, "connects"), 4000.0, "{}", body);
    assert_eq!(number(&body, "p50_ms"), 8.3, "{}", body);
    assert_eq!(number(&body, "p95_ms"), 80.7, "{}", body);
    assert_eq!(number(&body, "dns_miss_per_connect"), 0.55, "{}", body);
    assert_eq!(number(&body, "dns_miss_avg_ms"), 11.5, "{}", body);
    // バーストの 2 時間のエラーと山は平常時に入らない
    assert_eq!(number(&body, "errors"), 0.0, "{}", body);
    assert_eq!(number(&body, "active_max"), 8.0, "{}", body);
    assert!(body.contains("\"normal_hours_only\":true"), "{}", body);

    // 平常時で切らなければ同じ期間でも数字が化ける (T14.0 の「(参考) バースト込み」)
    let all = endpoint_json(
        port,
        &format!("/history?summary=1&since={}&until={}&res=3600", t0, last),
    );
    assert_eq!(number(&all, "samples"), 22.0, "{}", all);
    assert_eq!(number(&all, "connects"), 4800.0, "{}", all);
    // 順位 2,400 が [25,50] の 230 / 1,323 の位置に落ちる: 25 + 25 × 0.1738 = 29.3
    assert_eq!(number(&all, "p50_ms"), 29.3, "{}", all);
    assert_eq!(number(&all, "errors"), 198.0, "{}", all);
    assert_eq!(number(&all, "active_max"), 218.0, "{}", all);

    // `summary` を付けない `/history` は今までどおり (標本の配列を返す)
    let full = endpoint_json(port, "/history?res=3600");
    assert!(full.contains("\"keys\":[\"t\","), "{}", &full[..80]);
    assert!(full.contains("\"samples\":[["), "{}", &full[..80]);
    assert!(!full.contains("\"normal_hours_only\""), "{}", &full[..80]);
}

/// `since=restart` は `/status` の `since_start_secs` と同じ起動時刻で切る。
#[test]
fn test_integration_history_summary_since_restart_matches_since_start_secs() {
    let (port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let now = now_epoch();
    // 再起動より前の標本 (読み戻したもの) と、この起動で取った標本
    let mut old = Sample {
        t: now - 7_200,
        ..Sample::default()
    };
    old.connect.observe(250);
    let mut fresh = Sample {
        t: now - 5,
        ..Sample::default()
    };
    fresh.connect.observe(7);
    metrics.history.restore(0, vec![old, fresh]);

    let body = endpoint_json(port, "/history?summary=1&since=restart");
    println!("since=restart -> {}", body);
    let uptime = status_number(&status_json(port), "since_start_secs");
    let (from, to) = (number(&body, "from") as u64, number(&body, "to") as u64);
    // **受け入れ基準**: `since=restart` の切り口が `/status` の `since_start_secs` と合う
    assert!(
        to.saturating_sub(from).abs_diff(uptime) <= 2,
        "to − from = {} だが since_start_secs は {} ({})",
        to.saturating_sub(from),
        uptime,
        body
    );
    assert!(from >= now.saturating_sub(2) && from <= now + 2, "{}", body);
    // 起動より前の標本は入らない (T14.0 が手でやっていた「再起動時刻で切る」)。
    // この起動は始まったばかりなので窓はほぼ 0 秒 — 入るとしても新しい 1 本だけ
    assert!(number(&body, "samples") <= 1.0, "{}", body);
    assert!(
        !body.contains("\"max_ms\":250"),
        "再起動前の標本が入っている: {}",
        body
    );
    // 期間が 1 時間に満たないので 5 秒の窓が選ばれる
    assert_eq!(number(&body, "interval_secs"), 5.0, "{}", body);
    assert!(body.len() <= MAX_BODY, "{} B", body.len());

    // 同じ 2 本を epoch で切れば両方入る (= 上で外れたのは `since=restart` の働き)
    let both = endpoint_json(
        port,
        &format!(
            "/history?summary=1&since={}&until={}&res=5",
            now - 7_200,
            now
        ),
    );
    assert_eq!(number(&both, "samples"), 2.0, "{}", both);
    assert_eq!(number(&both, "connects"), 2.0, "{}", both);
    assert_eq!(number(&both, "max_ms"), 250.0, "{}", both);

    // `/` の案内に載っている
    let list = endpoint_json(port, "/");
    assert!(
        list.contains("/history?since=&until=&summary=1"),
        "{}",
        list
    );
}
