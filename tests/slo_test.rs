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
            if (17..24).contains(&(i / 720)) {
                for _ in 0..4 {
                    s.connect.observe(30);
                }
                for _ in 0..3 {
                    s.connect.observe(300);
                }
                s.errors = 1;
            } else {
                for _ in 0..2 {
                    s.connect.observe(3);
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
    let defaults = "\"thresholds\":{\"connect_p50_ms\":10,\"connect_p95_ms\":100,\
                    \"error_rate\":0.005,\"dns_miss_per_connect\":0.2}";
    assert!(empty.contains(defaults), "{}", empty);
    let cfg = endpoint_json(port, "/config");
    assert!(
        cfg.contains(
            "\"PROXY_SLO\":{\"value\":\"connect_p50_ms=10,connect_p95_ms=100,\
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
        body.contains("\"hourly_keys\":[\"t\",\"judged\",\"met\",\"misses\"]"),
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
