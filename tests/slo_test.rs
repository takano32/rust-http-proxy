//! `/slo?days=7` の結合テスト (T14.50)。
//!
//! 判定そのもの (4 つの閾・標本ごとの 4 ビット・T14.0 の 09-11 を模した標本列からの
//! 達成率と外れた時間帯) は `crates/metrics/src/slo.rs` の単体テスト 11 本で見ている。
//! ここで見るのは**実際の口の配線**: HTTP で叩いたら 200 が返り、応答が 64 KiB 以下で、
//! `thresholds` が `PROXY_SLO` の既定値 (= `crates/config` の `DEFAULT_SLO` と同じ) で、
//! 履歴スレッドが積んだ集計がそのまま日ごと・時間ごとの達成率として読めること。

mod common;

use common::*;
use rust_http_proxy::history::Sample;
use rust_http_proxy::slo;

/// UTC の今日の 0 時 (epoch 秒)。標本列はここから 24 時間ぶん置く。
fn today_utc() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    now - now % 86_400
}

/// T14.0 の 2026-09-11 のバーストを模した 1 日 (**17〜23 時だけ閾を外す**)。
///
/// 5 秒の標本を 24 時間ぶん (720 × 24 = 17,280 本)。平常時は 1 標本 2 本を 3 ms、
/// バーストの 7 時間は 4 本を 30 ms・3 本を 300 ms にしてエラーを 1 件足す。
fn burst_day(t0: u64) -> Vec<Sample> {
    (0..24 * 720u64)
        .map(|i| {
            let t = t0 + i * 5;
            let mut s = Sample {
                t,
                ..Sample::default()
            };
            // 確立の判定は「確立 − RTT」の窓 (T.28.45。T.29.12 から名前解決を除いた `conn_excess`)
            // なので、同じ値を 3 つに入れる
            let mut put = |ms: u64| {
                s.connect.observe(ms);
                s.excess.observe(ms);
                s.conn_excess.observe(ms);
            };
            if (17..24).contains(&(i / 720)) {
                for _ in 0..4 {
                    put(30);
                }
                for _ in 0..3 {
                    put(300);
                }
                s.errors = 1;
            } else {
                for _ in 0..2 {
                    put(3);
                }
            }
            s
        })
        .collect()
}

/// JSON の `"key":<数>` を読む (`null` は `None`)。
fn number(json: &str, key: &str) -> Option<f64> {
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
        .ok()
}

/// **受け入れ基準**: `/slo` が 200・64 KiB 以下・`thresholds` が既定値。
/// あわせて、履歴スレッドが積んだ集計が日ごと・時間ごとの達成率として読めること。
#[test]
fn test_integration_slo_reports_the_share_of_time_the_thresholds_were_met() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    slo::reset();
    let port = start_test_proxy(proxy_config());

    // まだ 1 本も判定していない状態でも 200 で返る (`ratio` は null = 0% ではない)
    let empty = endpoint_json(port, "/slo");
    println!("/slo (空) -> {} ({} B)", empty, empty.len());
    assert!(empty.len() <= slo::MAX_BODY, "{} B", empty.len());
    assert!(empty.contains("\"ratio\":null"), "{}", empty);
    assert!(empty.contains("\"judged\":0"), "{}", empty);
    assert_eq!(number(&empty, "days"), Some(7.0), "{}", empty);

    // **閾は `PROXY_SLO` の既定** (`crates/config` の `DEFAULT_SLO` と同じ値であること
    // — 層が違うので数値を 2 か所に持っている。食い違ったらここで落ちる)
    // T.28.45 で既定を 5 / 50 ms (確立 − RTT) にし、3 つの読み方を `basis` に出した
    // (T.29.12 で確立から名前解決を外し、T.29.33 で比の 2 つを 1 時間の合計にした)
    let defaults = "\"thresholds\":{\"connect_p50_ms\":5,\"connect_p95_ms\":50,\
                    \"error_rate\":0.005,\"dns_miss_per_connect\":0.2},\
                    \"basis\":{\"connect\":\"connect_minus_rtt\",\
                    \"error_rate\":\"resolved_attempts_hourly_sum\",\
                    \"dns_miss_per_connect\":\"excluding_failed_lookups_hourly_sum\"}";
    assert!(empty.contains(defaults), "{}", empty);
    let cfg = endpoint_json(port, "/config");
    assert!(
        cfg.contains(
            "\"PROXY_SLO\":{\"value\":\"connect_p50_ms=5,connect_p95_ms=50,\
             error_rate=0.005,dns_miss_per_connect=0.2\",\"source\":\"default\"}"
        ),
        "{}",
        cfg
    );

    // 履歴スレッドが 5 秒ごとに呼ぶのと同じ口へ、T14.0 を模した 1 日を流し込む
    let t0 = today_utc();
    for s in burst_day(t0) {
        slo::observe(&s);
    }

    let body = endpoint_json(port, "/slo?days=1");
    println!("/slo?days=1 -> {} B", body.len());
    assert!(body.len() <= slo::MAX_BODY, "{} B (64 KiB 超)", body.len());
    assert!(body.contains(defaults), "{}", body);
    // 手計算: 17,280 標本のうち 17〜23 時の 5,040 本が外れ → 12,240 / 17,280 = 0.70833
    assert_eq!(number(&body, "judged"), Some(17_280.0), "{}", body);
    assert_eq!(number(&body, "met"), Some(12_240.0), "{}", body);
    assert!(body.contains("\"ratio\":0.70833"), "{}", body);
    assert_eq!(number(&body, "hours"), Some(24.0), "{}", body);
    // 外れた時間帯は 1 行 (17 時から 24 時まで連続)、外した閾は 3 つ
    assert!(
        body.contains(&format!(
            "\"breaches\":[{{\"from\":{},\"to\":{},",
            t0 + 17 * 3600,
            t0 + 24 * 3600
        )),
        "{}",
        body
    );
    assert!(body.contains("\"hours\":7,"), "{}", body);
    for name in ["connect_p50_ms", "connect_p95_ms", "error_rate"] {
        assert!(
            body.contains(&format!("{{\"name\":\"{}\",\"samples\":5040,", name)),
            "{} が外れた閾に無い: {}",
            name,
            body
        );
    }
    // 名前解決は外していないので行が無い
    assert!(
        !body.contains("\"name\":\"dns_miss_per_connect\""),
        "{}",
        body
    );
    // 今日 (UTC) の 1 枚がダッシュボードの KPI の元になる
    assert!(body.contains("\"today\":{\"date\":\""), "{}", body);
    assert!(
        body.contains("\"hourly_keys\":[\"t\",\"judged\",\"met\",\"misses\",\"rate_sums\"]"),
        "{}",
        body
    );

    // `?days=` は 1〜31 に丸め、数でなければ既定 (7)
    assert_eq!(
        number(&endpoint_json(port, "/slo?days=999"), "days"),
        Some(slo::MAX_DAYS as f64)
    );
    assert_eq!(
        number(&endpoint_json(port, "/slo?days=abc"), "days"),
        Some(slo::DEFAULT_DAYS as f64)
    );

    // `/` の案内に載っている
    let list = endpoint_json(port, "/");
    assert!(list.contains("/slo?days=7"), "{}", list);
    slo::reset();
}

/// 1 時間 (720 本の 5 秒) に確立 1,000 本と、名前解決に成功した試みのエラー `errors` 件 (T.29.33)。
/// エラーは**確立 1 本だけの 5 秒**に置く (5 秒ごとに判定すると 1 / 2 = 0.5 でその 5 秒が外れる形)。
fn hour_with_errors(t0: u64, errors: usize) -> Vec<Sample> {
    (0..720usize)
        .map(|i| {
            let mut s = Sample {
                t: t0 + i as u64 * 5,
                ..Sample::default()
            };
            // 先頭の 719 本に 1 本ずつ、最後の 5 秒に残りの 281 本
            let n = if i < 719 { 1 } else { 1000 - 719 };
            for _ in 0..n {
                s.connect.observe(3);
                s.excess.observe(1);
                s.conn_excess.observe(1);
            }
            if i < errors {
                s.errors = 1;
                s.errors_by_cause[1] = 1; // refused
            }
            s
        })
        .collect()
}

/// **T.29.33 の受け入れ基準**: 比 (`error_rate`) は 1 時間の合計で判定する。1 時間に確立 1,000 本・
/// エラー 4 本 (4 / 1,004) なら満たし、6 本 (6 / 1,006) なら外す。どちらもエラーは確立 1 本の 5 秒に
/// 置いてあり、5 秒ごとの判定なら 4 本でも 4 つの 5 秒が外れていた。`/slo` に 1 時間の合計
/// (分子と分母)・比ごとの満たした / 外した時間の数・直近 1 時間の合計が出る。
#[test]
fn test_integration_slo_judges_the_rates_on_the_sum_of_the_hour() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    slo::reset();
    let port = start_test_proxy(proxy_config());
    let t0 = today_utc();
    for s in hour_with_errors(t0, 4)
        .into_iter()
        .chain(hour_with_errors(t0 + 3600, 6))
    {
        slo::observe(&s);
    }
    let body = endpoint_json(port, "/slo?days=1");
    println!("/slo?days=1 -> {}", body);
    assert!(
        body.contains(
            "\"basis\":{\"connect\":\"connect_minus_rtt\",\
             \"error_rate\":\"resolved_attempts_hourly_sum\",\
             \"dns_miss_per_connect\":\"excluding_failed_lookups_hourly_sum\"}"
        ),
        "{}",
        body
    );
    // 1 時間目は 720 本とも達成、2 時間目は比を外したので 0 本
    assert!(
        body.contains(&format!("[{},720,720,[0,0,0,0],[[4,1004],[0,1000]]]", t0)),
        "{}",
        body
    );
    assert!(
        body.contains(&format!(
            "[{},720,0,[0,0,720,0],[[6,1006],[0,1000]]]",
            t0 + 3600
        )),
        "{}",
        body
    );
    assert_eq!(number(&body, "met"), Some(720.0), "{}", body);
    // 日ごとの行に合計と時間の数
    assert!(
        body.contains(
            "\"rates\":{\"error_rate\":{\"num\":10,\"den\":2010,\"value\":0.00498,\
             \"hours_met\":1,\"hours_missed\":1}"
        ),
        "{}",
        body
    );
    // 外れた時間帯は 2 時間目だけで、その時間の合計を添える
    assert!(
        body.contains(
            "{\"name\":\"error_rate\",\"samples\":720,\"worst\":0.005964,\"threshold\":0.005,\
             \"hours\":1,\"num\":6,\"den\":1006}"
        ),
        "{}",
        body
    );
    assert!(body.contains("\"rate_window_secs\":3600"), "{}", body);
    slo::reset();
}

/// 2 本の試験は同じ大域の集計 (`slo::observe`) を使うので直列にする。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
