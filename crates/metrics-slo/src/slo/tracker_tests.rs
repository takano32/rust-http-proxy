//! 時間ごとの集計と `/slo` の応答の単体試験 (T.34.2 で `slo.rs` の `mod tests` から移した。名前はそのまま)。

use super::super::judge::SPEED_BITS;
use super::*;

/// **受け入れ基準**: 既知の標本列から達成率が手計算と一致し、外れた時間帯が 17〜23 時。
#[test]
fn the_burst_day_matches_the_hand_calculation() {
    let th = DEFAULT;
    // 2026-09-11 00:00:00 UTC
    let t0 = 1_789_084_800;
    assert_eq!(t0 % DAY, 0);
    let mut tr = Tracker::new();
    for s in burst_day(t0) {
        tr.observe(&th, &s);
    }
    let rep = tr.report(&th, t0 + DAY - 5, 1);
    // 手計算: 24 時間 × 720 標本 = 17,280 本すべて判定でき、
    // 外れるのは 17〜23 時の 7 時間 × 720 = 5,040 本。12,240 / 17,280 = 0.708333…
    assert_eq!(rep.total.judged, 17_280);
    assert_eq!(rep.total.met, 12_240);
    let ratio = rep.total.ratio().unwrap();
    assert!((ratio - 12_240.0 / 17_280.0).abs() < 1e-12, "{}", ratio);
    assert_eq!(rep.hourly.len(), 24);
    // 外れた時間帯は 1 行 (17 時から 24 時まで連続)
    assert_eq!(rep.breaches.len(), 1, "{:?}", rep.breaches);
    let b = rep.breaches[0];
    assert_eq!(b.from, t0 + 17 * HOUR);
    assert_eq!(b.to, t0 + 24 * HOUR);
    assert_eq!(b.hours, 7);
    assert_eq!(b.tally.judged, 5_040);
    assert_eq!(b.tally.met, 0);
    // 外したのは p50 / p95 / エラー率の 3 つで、名前解決は外していない
    assert_eq!(b.tally.miss, [5_040, 5_040, 5_040, 0], "{:?}", b);
    assert_eq!(b.tally.worst[0], 46.875, "{:?}", b.tally.worst);
    assert_eq!(b.tally.worst[1], 300.0, "{:?}", b.tally.worst);
    assert!(
        (b.tally.worst[2] - 0.125).abs() < 1e-12,
        "{:?}",
        b.tally.worst
    );
    assert_eq!(b.tally.worst[3], 0.0);
    // 日ごとは 1 行 (その日ぜんぶ)
    assert_eq!(rep.daily.len(), 1);
    assert_eq!(rep.daily[0].t, t0);
    assert_eq!(rep.daily[0].judged, 17_280);
    // 平常の時間は 100%、バーストの時間は 0%
    assert_eq!(rep.hourly[16].ratio(), Some(1.0));
    assert_eq!(rep.hourly[17].ratio(), Some(0.0));
    assert_eq!(rep.hourly[23].ratio(), Some(0.0));
}

/// 「99.2% の時間で満たした」の形 (1 時間の中で一部だけ外す)。
#[test]
fn a_partly_bad_hour_gives_a_fractional_ratio() {
    let th = DEFAULT;
    let t0 = 1_789_084_800;
    let mut tr = Tracker::new();
    // 1 時間 720 標本のうち 6 本だけ遅い = 714 / 720 = 0.991666…
    for i in 0..720u64 {
        let ms = if i < 6 { 300 } else { 3 };
        tr.observe(&th, &sample(t0 + i * 5, 1, ms));
    }
    let rep = tr.report(&th, t0 + HOUR, 1);
    assert_eq!((rep.total.judged, rep.total.met), (720, 714));
    assert_eq!(rep.breaches.len(), 1);
    assert_eq!(rep.breaches[0].hours, 1);
    let json = rep.to_json();
    assert!(json.contains("\"ratio\":0.99167"), "{}", json);
}

/// 外れた時間が離れていれば別の行、続いていれば 1 行。
#[test]
fn breaches_merge_only_while_they_are_next_to_each_other() {
    let th = DEFAULT;
    let t0 = 1_789_084_800;
    let mut tr = Tracker::new();
    // 0 時と 1 時が外れ、2 時は満たし、3 時がまた外れ → 2 行
    for (hour, ms) in [(0u64, 300u64), (1, 300), (2, 3), (3, 300)] {
        tr.observe(&th, &sample(t0 + hour * HOUR, 1, ms));
    }
    let rep = tr.report(&th, t0 + 4 * HOUR, 1);
    assert_eq!(rep.breaches.len(), 2, "{:?}", rep.breaches);
    assert_eq!(rep.breaches[0].hours, 2);
    assert_eq!(rep.breaches[0].from, t0);
    assert_eq!(rep.breaches[0].to, t0 + 2 * HOUR);
    assert_eq!(rep.breaches[1].hours, 1);
    assert_eq!(rep.breaches[1].from, t0 + 3 * HOUR);
    // 記録の無い時間は行にならない (穴は畳まない)
    let mut gap = Tracker::new();
    gap.observe(&th, &sample(t0, 1, 300));
    gap.observe(&th, &sample(t0 + 5 * HOUR, 1, 300));
    assert_eq!(gap.report(&th, t0 + 6 * HOUR, 1).breaches.len(), 2);
}

#[test]
fn the_window_is_utc_days_and_today_is_the_last_one() {
    let th = DEFAULT;
    let t0 = 1_789_084_800; // 2026-09-11 00:00 UTC
    let mut tr = Tracker::new();
    for d in 0..9u64 {
        tr.observe(&th, &sample(t0 + d * DAY + 12 * HOUR, 1, 3));
    }
    let now = t0 + 8 * DAY + 20 * HOUR;
    let rep = tr.report(&th, now, 7);
    // 今日を含めて 7 日 = 2 日目から 8 日目
    assert_eq!(rep.from, t0 + 2 * DAY);
    assert_eq!(rep.today_t, t0 + 8 * DAY);
    assert_eq!(rep.daily.len(), 7);
    assert_eq!(rep.total.judged, 7);
    assert_eq!(rep.today().judged, 1);
    // `days` は 1〜31 に丸める
    assert_eq!(tr.report(&th, now, 0).days, 1);
    assert_eq!(tr.report(&th, now, 9_999).days, MAX_DAYS);
}

#[test]
fn the_ring_keeps_31_days_of_hours() {
    let th = DEFAULT;
    let mut tr = Tracker::new();
    for i in 0..(HOURS as u64 + 100) {
        tr.observe(&th, &sample(i * HOUR, 1, 3));
    }
    assert_eq!(tr.hours().count(), HOURS);
    assert_eq!(tr.hours().next().unwrap().t, 100 * HOUR);
}

/// 応答は 64 KiB 以下 (**最悪の形**: 31 日ぶん、1 時間おきに外れる)。
#[test]
fn the_body_stays_under_64_kib() {
    let th = DEFAULT;
    let mut tr = Tracker::new();
    for i in 0..HOURS as u64 {
        // 1 時間おきに外す = 外れた時間帯が畳めずに最多になる
        let ms = if i % 2 == 0 { 300 } else { 3 };
        for k in 0..720u64 {
            tr.observe(&th, &sample(i * HOUR + k * 5, 1, ms));
        }
    }
    let rep = tr.report(&th, HOURS as u64 * HOUR, MAX_DAYS);
    let json = rep.to_json();
    println!(
        "最悪 (31 日・1 時間おきに外れ): {} B / 外れた時間帯 {} 件",
        json.len(),
        rep.breaches.len()
    );
    assert!(json.len() <= MAX_BODY, "{} B", json.len());
    assert!(json.contains("\"truncated\":true"), "{}", &json[..200]);
    assert!(json.ends_with('}'));
    // 空でも読める形 (判定 0 本のときは `ratio` が null)
    let empty = Tracker::new().report(&th, 0, 7).to_json();
    assert!(empty.contains("\"ratio\":null"), "{}", empty);
    assert!(empty.contains("\"truncated\":false"), "{}", empty);
    assert!(empty.len() < 1536, "{} B", empty.len());
}

#[test]
fn the_json_carries_the_thresholds_and_the_bad_hours() {
    let th = DEFAULT;
    let t0 = 1_789_084_800;
    let mut tr = Tracker::new();
    for s in burst_day(t0) {
        tr.observe(&th, &s);
    }
    let json = tr.report(&th, t0 + DAY - 5, 1).to_json();
    println!("{}", json);
    assert!(json.len() <= MAX_BODY, "{} B", json.len());
    assert!(
        json.contains(
            "\"thresholds\":{\"connect_p50_ms\":5,\"connect_p95_ms\":50,\
             \"error_rate\":0.005,\"dns_miss_per_connect\":0.2},\"basis\":{\
             \"connect\":\"connect_minus_rtt\",\"error_rate\":\"resolved_web_attempts_hourly_sum_excluding_top_destination\",\
             \"dns_miss_per_connect\":\"excluding_failed_lookups_hourly_sum\"},\"names\":["
        ),
        "{}",
        json
    );
    assert!(json.contains("\"ratio\":0.70833"), "{}", json);
    assert!(
        json.contains("\"from_hour\":\"2026-09-11T17Z\",\"to_hour\":\"2026-09-12T00Z\""),
        "{}",
        json
    );
    assert!(
        json.contains(
            "{\"name\":\"connect_p50_ms\",\"samples\":5040,\"worst\":46.875,\"threshold\":5}"
        ),
        "{}",
        json
    );
    assert!(json.contains("\"date\":\"2026-09-11\""), "{}", json);
    assert!(
        !json.contains("dns_miss_per_connect\",\"samples\""),
        "{}",
        json
    );
}

#[test]
fn out_of_order_samples_land_in_their_own_hour() {
    let th = DEFAULT;
    let t0 = 1_789_084_800;
    let mut tr = Tracker::new();
    tr.observe(&th, &sample(t0 + HOUR, 1, 3));
    // 1 つ前の時間に戻った標本 (読み戻し) は、その時間がまだ無ければ捨てる
    tr.observe(&th, &sample(t0, 1, 300));
    assert_eq!(tr.hours().count(), 1);
    // 同じ時間に戻った標本は足す
    tr.observe(&th, &sample(t0 + HOUR + 5, 1, 300));
    let h: Vec<Hour> = tr.hours().collect();
    assert_eq!((h[0].judged, h[0].met), (2, 1));
}

/// **T.29.33 の受け入れ基準 (単体)**: 1 時間に確立 1,000 本・エラー 4 本なら比は満たし
/// (4 / 1,004 = 0.0040)、6 本なら外す (6 / 1,006 = 0.0060)。エラーを確立 1 本の 5 秒に置いても
/// (5 秒ごとなら 0.5 で外れる) 確立の多い 5 秒に置いても結果は同じ。
#[test]
fn the_rates_are_judged_on_the_sum_of_the_hour() {
    let th = DEFAULT;
    let t0 = 1_789_084_800;
    for spread in [false, true] {
        for (errors, ok) in [(4u64, true), (6, false)] {
            let mut tr = Tracker::new();
            let per_sample_misses = hour_of(t0, 1000, errors, spread)
                .iter()
                .map(|s| tr.observe(&th, s))
                .filter(|v| v.bits & 0b0100 != 0)
                .count();
            if !spread {
                // 5 秒ごとに判定したら、エラーを置いた 5 秒は全部外れていた
                assert_eq!(per_sample_misses, errors as usize);
            }
            let rep = tr.report(&th, t0 + HOUR - 5, 1);
            let h = rep.hourly[0];
            assert_eq!((h.num[0], h.den[0]), (errors, 1000 + errors));
            assert_eq!(h.judged, 720);
            if ok {
                assert_eq!((h.met, h.miss[2]), (720, 0), "spread={} {:?}", spread, h);
                assert_eq!(h.rate_hours[0], [1, 0]);
                assert!(rep.breaches.is_empty());
            } else {
                assert_eq!((h.met, h.miss[2]), (0, 720), "spread={} {:?}", spread, h);
                assert_eq!(h.rate_hours[0], [0, 1]);
                assert!((h.worst[2] - 6.0 / 1006.0).abs() < 1e-12, "{:?}", h);
                let json = rep.to_json();
                assert!(
                    json.contains(
                        "{\"name\":\"error_rate\",\"samples\":720,\"worst\":0.005964,\
                         \"threshold\":0.005,\"hours\":1,\"num\":6,\"den\":1006}"
                    ),
                    "{}",
                    json
                );
            }
            // 確立の多い 5 秒に置いても少ない 5 秒に置いても同じ日の数字
            let d = rep.today();
            assert_eq!((d.judged, d.met), (h.judged, h.met));
        }
    }
}

/// **T.29.33**: 速さ (確立の 2 つ) は今までどおり 5 秒の標本ごと。比を満たした時間の達成は
/// 速さを満たした 5 秒の数そのもの。
#[test]
fn the_speed_is_still_judged_every_five_seconds() {
    let th = DEFAULT;
    let t0 = 1_789_084_800;
    let mut tr = Tracker::new();
    let mut slow = 0;
    for i in 0..720u64 {
        // 6 本に 1 本だけ遅い (確立 − RTT 70 ms)
        let ms = if i % 6 == 0 { 70 } else { 2 };
        let mut s = sample(t0 + i * 5, 1, ms);
        s.conn_excess = crate::history::Window::default();
        s.conn_excess.observe(ms);
        let v = tr.observe(&th, &s);
        if v.bits & SPEED_BITS != 0 {
            slow += 1;
        }
    }
    let h = tr.report(&th, t0 + HOUR, 1).hourly[0];
    assert_eq!(slow, 120);
    assert_eq!(
        (h.judged, h.met, h.miss[0], h.miss[1]),
        (720, 600, 120, 120)
    );
}

/// **T.29.33**: 直近 1 時間 (`last_hour`) は時間の区切りをまたいで、いまから 3,600 秒の合計。
/// 時間の区切りの判定とは別に読める (0 時台は 3 / 363 で外し、直近 1 時間は 3 / 723 で満たす)。
#[test]
fn the_last_hour_is_a_rolling_sum() {
    let th = DEFAULT;
    let t0 = 1_789_084_800;
    let mut tr = Tracker::new();
    // 0 時台の後半 30 分にエラー 3 件、1 時台の前半 30 分は確立だけ
    for i in 0..720u64 {
        let mut s = sample(t0 + 1800 + i * 5, 1, 2);
        if i < 3 {
            s.errors = 1;
        }
        tr.observe(&th, &s);
    }
    let now = t0 + 1800 + 719 * 5;
    let lh = tr.last_hour(now);
    assert_eq!(lh.samples, 720);
    assert_eq!((lh.num[0], lh.den[0]), (3, 723));
    let json = tr.report(&th, now, 1).to_json();
    assert!(
        json.contains("\"rate_window_secs\":3600,\"last_hour\":{"),
        "{}",
        json
    );
    assert!(
        json.contains("\"error_rate\":{\"num\":3,\"den\":723,\"value\":0.00415,\"met\":true,\"excluded\":{\"errors\":0,\"attempts\":0,\"destinations\":[]}}"),
        "{}",
        json
    );
    // 30 分先では前半の 3 件は窓の外
    let later = tr.last_hour(now + 1800);
    assert_eq!(later.num[0], 0);
    // 日ごとの行に、比ごとの合計と満たした / 外した時間の数 (0 時台は 3 / 363 = 0.0083 で外した)
    assert!(
        json.contains(
            "\"rates\":{\"error_rate\":{\"num\":3,\"den\":723,\"value\":0.00415,\
                       \"hours_met\":1,\"hours_missed\":1,\"excluded\":{\"hours\":0,\"errors\":0,\"attempts\":0}}"
        ),
        "{}",
        json
    );
    assert_eq!(tr.rate_hours_of_day(&th, t0), [[1, 1], [2, 0]]);
}

/// **T.30.4 の受け入れ基準 (単体)**: エラーが 2 件以上で、いちばん多い宛先がその時間のエラーの半分以上なら
/// その宛先の試行とエラーを外す。4 つの宛先に 1 件ずつ・1 件だけの時間は外さない (今までどおり)。
#[test]
fn the_error_rate_leaves_out_one_destination_with_most_of_the_errors_of_the_hour() {
    let th = DEFAULT;
    let t0 = 1_789_084_800;
    // 規則そのもの
    assert!(excludes(&dest(t0, 5, "connect://pravda.example:443", 4, 4)));
    assert!(
        excludes(&dest(t0, 2, "connect://a:443", 1, 9)),
        "ちょうど半分"
    );
    assert!(
        !excludes(&dest(t0, 4, "connect://a:443", 1, 9)),
        "4 つに 1 件ずつ"
    );
    assert!(!excludes(&dest(t0, 1, "connect://a:443", 1, 9)), "1 件だけ");
    assert!(!excludes(&dest(t0, 5, "connect://a:443", 2, 9)), "半分未満");
    assert!(
        !excludes(&dest(t0, 5, "", 0, 0)),
        "宛先が分からない (溢れた行だけ)"
    );
    // 1 時間に確立 400 本 + 時間切れ 4 件 (1 つの宛先) + 拒否 1 件 (別の宛先) = 5 / 405 で外れる
    let samples = hour_of(t0, 400, 5, true);
    let mut tr = Tracker::new();
    for s in &samples {
        tr.observe(&th, s);
    }
    let h = tr.hours().next().unwrap();
    assert_eq!((h.num[0], h.den[0]), (5, 405));
    assert!(h.breached(), "旧の数え方なら外れる: {:?}", h);
    // 宛先の表を渡すと 1 / 401 = 0.0025 で満たす
    tr.observe_destinations(None, &dest(t0, 5, "connect://pravda.example:443", 4, 4));
    let h = tr.hours().next().unwrap();
    assert_eq!((h.num[0], h.den[0]), (1, 401), "{:?}", h);
    assert_eq!((h.excluded, h.excluded_hours), ([4, 4], 1));
    assert!(!h.breached(), "{:?}", h);
    assert_eq!(h.met, h.judged);
    let json = tr.report(&th, t0 + 3599, 1).to_json();
    assert!(
        json.contains(&format!(
            "[{},401,401,[0,0,0,0],[[1,401],[0,400]],{{\"destination\":\
             \"connect://pravda.example:443\",\"errors\":4,\"attempts\":4,\"hour_errors\":5}}]",
            t0
        )),
        "{}",
        json
    );
    assert!(
        json.contains(
            "\"error_rate\":{\"num\":1,\"den\":401,\"value\":0.00249,\"hours_met\":1,\
             \"hours_missed\":0,\"excluded\":{\"hours\":1,\"errors\":4,\"attempts\":4}}"
        ),
        "{}",
        json
    );
    // いまの時間 (窓にまるごと入る) はそのまま外す
    assert!(
        json.contains(
            "\"last_hour\":{\"from\":1789084799,\"to\":1789088399,\"samples\":401,\"rates\":\
             {\"error_rate\":{\"num\":1,\"den\":401,\"value\":0.00249,\"met\":true,\"excluded\":\
             {\"errors\":4,\"attempts\":4,\"destinations\":[\"connect://pravda.example:443\"]}}"
        ),
        "{}",
        json
    );
    // 1 時間あと (30 分先) は前の時間のぶんを窓に掛かる秒の割合 (半分) で外す
    let lh = tr.last_hour(t0 + 3600 + 1800);
    assert_eq!(lh.excluded, [2, 2], "{:?}", lh);
    // 4 つの宛先に 1 件ずつなら外さない (5 / 405 で外れたまま)。表の差し替えも効く
    tr.observe_destinations(None, &dest(t0, 5, "connect://a:443", 1, 101));
    let h = tr.hours().next().unwrap();
    assert_eq!((h.num[0], h.den[0], h.excluded_hours), (5, 405, 0));
    assert!(h.breached());
    // 1 件だけの時間も外さない (1 / 401 はもともと満たす)
    let mut one = Tracker::new();
    for s in &hour_of(t0, 400, 1, false) {
        one.observe(&th, s);
    }
    one.observe_destinations(None, &dest(t0, 1, "connect://pravda.example:443", 1, 1));
    let h = one.hours().next().unwrap();
    assert_eq!((h.num[0], h.den[0], h.excluded_hours), (1, 401, 0));
    // 外したあとも外れた時間は `breaches` に外した宛先が出る
    let mut bad = Tracker::new();
    for s in &hour_of(t0, 400, 9, false) {
        bad.observe(&th, s);
    }
    bad.observe_destinations(None, &dest(t0, 9, "connect://pravda.example:443", 5, 5));
    let json = bad.report(&th, t0 + 3599, 1).to_json();
    assert!(
        json.contains(
            "\"num\":4,\"den\":404}],\"excluded\":{\"hours\":1,\"errors\":5,\"attempts\":5,\
             \"destinations\":[\"connect://pravda.example:443\"]}}"
        ),
        "{}",
        json
    );
}

#[test]
fn the_global_tracker_is_configurable_and_resettable() {
    reset();
    assert_eq!(thresholds(), DEFAULT);
    configure(Thresholds {
        connect_p50_ms: 1.0,
        ..DEFAULT
    });
    assert_eq!(thresholds().connect_p50_ms, 1.0);
    let v = observe(&sample(1_789_084_800, 1, 3));
    assert_eq!(v.bits, 0b0001, "{:?}", v);
    assert_eq!(report(1_789_084_800, 1).total.judged, 1);
    reset();
    assert_eq!(thresholds(), DEFAULT);
    assert_eq!(report(1_789_084_800, 1).total.judged, 0);
}
