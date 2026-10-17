//! `anomaly` の単体テスト (T22.2 で `anomaly.rs` の `mod tests` をそのまま移した。`anomaly.rs` が 2,000 行に
//! 届いていたため)。`check` を通すものは出来事のリングがプロセスで 1 組なので、今までどおり `reset` で片づける。

use super::*;

/// 5 秒周期の標本 1 本。
fn point(t: u64) -> Sample {
    Sample {
        t,
        ..Sample::default()
    }
}

/// `n` 本の CONNECT が `ms` ミリ秒で確立した標本。
fn connects(t: u64, n: usize, ms: u64) -> Sample {
    let mut p = point(t);
    for _ in 0..n {
        p.connect.observe(ms);
    }
    p
}

/// 標本を 1 本流す (**本番と同じ順**: 履歴に積んでから判定する)。
fn feed(h: &History, d: &mut Detector, s: Sample) -> Vec<Fired> {
    feed_with(h, d, s, Counters::default())
}

/// リングから読めない累計 (断った数・写真の通算) も付けて 1 本流す。
fn feed_with(h: &History, d: &mut Detector, s: Sample, c: Counters) -> Vec<Fired> {
    feed_conns(h, d, None, s, c)
}

/// 接続の表も渡して 1 本流す (`tunnel_spin` の判定に要る。T15.0 (6))。
fn feed_conns(
    h: &History,
    d: &mut Detector,
    conns: Option<&ConnTable>,
    s: Sample,
    c: Counters,
) -> Vec<Fired> {
    h.push(s);
    d.observe(
        h,
        conns,
        Counters {
            t: s.t,
            evicted_idle: s.evicted_idle,
            ..c
        },
    )
}

/// **起動からの秒**を渡せる版 (T15.0 (9))。[`feed`] は [`Counters::default`] を
/// 渡すので `uptime_secs` が 0 のまま = 暖機中扱いになる。
fn feed_uptime(h: &History, d: &mut Detector, s: Sample, uptime: u64) -> Vec<Fired> {
    feed_with(
        h,
        d,
        s,
        Counters {
            uptime_secs: uptime,
            ..Counters::default()
        },
    )
}

/// cgroup の CPU の**累計**を渡して 1 本流す (T15.0 (6))。
fn feed_cpu(
    h: &History,
    d: &mut Detector,
    s: Sample,
    throttled: u64,
    periods: u64,
    used_us: u64,
) -> Vec<Fired> {
    feed_with(
        h,
        d,
        s,
        Counters {
            cpu_nr_throttled: throttled,
            cpu_nr_periods: periods,
            cpu_quota_cores: 1.0,
            cpu_used_us: used_us,
            ..Counters::default()
        },
    )
}

/// 平常時を `secs` 秒ぶん流す (5 秒ごとに 10 ms の CONNECT が 2 本)。
fn calm(h: &History, d: &mut Detector, from: u64, secs: u64) -> (u64, Vec<Fired>) {
    let mut out = Vec::new();
    let mut t = from;
    while t < from + secs {
        out.extend(feed(h, d, connects(t, 2, 10)));
        t += 5;
    }
    (t, out)
}

/// (1) 確立の p95 が基準値の 3 倍以上かつ 50 ms 以上。
#[test]
fn a_slow_connect_p95_fires_once_and_clears() {
    let h = History::default();
    let mut d = Detector::new();
    // 1 時間の平常時 (基準値 = 10 ms)
    let (mut t, quiet) = calm(&h, &mut d, 0, BASE_SECS);
    assert!(quiet.is_empty(), "平常時に立ってはいけない: {:?}", quiet);
    // 10 分のバースト (250 ms)
    let mut fired = Vec::new();
    while t < BASE_SECS + 600 {
        fired.extend(feed(&h, &mut d, connects(t, 2, 250)));
        t += 5;
    }
    assert_eq!(fired.len(), 1, "立つのは 1 回だけ: {:?}", fired);
    assert!(!fired[0].cleared);
    assert_eq!(fired[0].kind, Kind::ConnectP95);
    assert_eq!(
        fired[0].text,
        // 基準値の窓 (1 時間) に入る本数は、5 秒のリングが 6 時間になった T14.32 以降 1,442
        "connect_p95: connect p95 136 ms over 5m is 13.9x the 1h baseline 9.8 ms (122 of 1442 connects)"
    );
    // 平常時に戻して 15 分 (窓から抜けるのに 5 分 + 外れたまま 5 分)
    let (_, back) = calm(&h, &mut d, t, 900);
    assert_eq!(back.len(), 1, "解除も 1 回だけ: {:?}", back);
    assert!(back[0].cleared);
    assert!(
        back[0].text.starts_with("cleared: connect_p95 after "),
        "{}",
        back[0].text
    );
    assert!(d.firing().is_empty());
}

/// (2) 名前解決のミス 1 回の平均が 100 ms 以上。
#[test]
fn a_slow_dns_miss_fires_once_and_clears() {
    let h = History::default();
    let mut d = Detector::new();
    let (mut t, _) = calm(&h, &mut d, 0, 60);
    let mut fired = Vec::new();
    for _ in 0..12 {
        let mut p = connects(t, 2, 10);
        p.dns_misses = 3;
        p.dns_ms_sum = 3 * 140;
        fired.extend(feed(&h, &mut d, p));
        t += 5;
    }
    assert_eq!(fired.len(), 1, "{:?}", fired);
    assert_eq!(fired[0].kind, Kind::DnsSlow);
    assert_eq!(
        fired[0].text,
        "dns_slow: dns miss 140 ms avg over 5m (threshold 100 ms; 3 misses, 420 ms total)"
    );
    let (_, back) = calm(&h, &mut d, t, 900);
    assert_eq!(back.len(), 1, "{:?}", back);
    assert!(back[0].cleared);
    assert!(
        back[0].text.contains("dns miss 0.0 ms avg over 5m"),
        "{}",
        back[0].text
    );
}

/// 平常時の 1 時間のあと、`misses` 回のミス (1 回 `each_ms`) を 1 分おきに流し、
/// canary の基準線 `canary` を付けて、出た変わり目を全部返す (T17.1)。
fn dns_misses_once_a_minute(misses: u64, each_ms: u64, canary: Option<u64>) -> Vec<Fired> {
    let h = History::default();
    let mut d = Detector::new();
    let (mut t, quiet) = calm(&h, &mut d, 0, BASE_SECS);
    assert!(quiet.is_empty(), "{:?}", quiet);
    let mut out = Vec::new();
    // 4 分 (48 標本) のうち、1 分ごとの頭の標本にだけミスを 1 回
    for i in 0..48u64 {
        let mut p = connects(t, 2, 10);
        if i % 12 == 0 && i / 12 < misses {
            p.dns_misses = 1;
            p.dns_ms_sum = each_ms;
        }
        let c = Counters {
            t,
            canary_dns_p50_ms: canary,
            ..Counters::default()
        };
        out.extend(feed_with(&h, &mut d, p, c));
        t += 5;
    }
    out
}

/// T17.1: 5 分に 1 回だけの重いミス (400 ms) では `dns_slow` を立てない。
///
/// T16.99 の 29 件はほぼ全部これ (5 分に 1 回、240〜470 ms)。
#[test]
fn one_slow_dns_miss_alone_does_not_fire() {
    let out = dns_misses_once_a_minute(1, 400, None);
    assert!(out.is_empty(), "1 回では立たない: {:?}", out);
    assert_eq!(DNS_MIN_MISSES, 3);
}

/// T17.1: 5 分に 3 回の 400 ms は立つ (canary が無ければ閾は 100 ms)。
#[test]
fn three_slow_dns_misses_fire() {
    let out = dns_misses_once_a_minute(3, 400, None);
    assert_eq!(out.len(), 1, "{:?}", out);
    assert_eq!(out[0].kind, Kind::DnsSlow);
    assert_eq!(
        out[0].text,
        "dns_slow: dns miss 400 ms avg over 5m (threshold 100 ms; 3 misses, 1200 ms total)"
    );
    // 2 回目までは立たない (3 回目の標本で立つ)
    assert_eq!(dns_misses_once_a_minute(2, 400, None), Vec::new());
}

/// T17.1: canary の名前解決が 200 ms の網では閾は 3 倍の 600 ms。500 ms は立たない。
#[test]
fn a_slow_canary_raises_the_dns_threshold() {
    let out = dns_misses_once_a_minute(3, 500, Some(200));
    assert!(
        out.is_empty(),
        "canary 200 ms なら 500 ms は平常: {:?}",
        out
    );
    // 同じ網で 3 倍を越えれば立ち、説明の閾は基準線から引いた値になる
    let out = dns_misses_once_a_minute(3, 700, Some(200));
    assert_eq!(out.len(), 1, "{:?}", out);
    assert!(
        out[0].text.contains("(threshold 600 ms; 3 misses"),
        "{}",
        out[0].text
    );
    // 速い canary (9 ms) では 100 ms の下限が残る (27 ms には下げない)
    assert_eq!(dns_miss_limit(Some(9)), DNS_MISS_MS);
    assert_eq!(dns_miss_limit(None), DNS_MISS_MS);
}

/// (3) エラーが 5 分で 5 件以上。
#[test]
fn errors_in_five_minutes_fire_once_and_clear() {
    let h = History::default();
    let mut d = Detector::new();
    let (mut t, _) = calm(&h, &mut d, 0, 60);
    let mut fired = Vec::new();
    // 1 標本に 3 件ずつ 2 回 = 6 件 (2 回目で越える)
    for _ in 0..2 {
        let mut p = point(t);
        p.errors = 3;
        p.errors_by_cause[0] = 2;
        p.errors_by_cause[4] = 1;
        fired.extend(feed(&h, &mut d, p));
        t += 5;
    }
    assert_eq!(fired.len(), 1, "{:?}", fired);
    assert_eq!(fired[0].kind, Kind::Errors);
    assert_eq!(
        fired[0].text,
        "errors: 6 errors in 5m (threshold 5): dns 4, reset 2"
    );
    // 窓から抜けるのに 5 分 + 外れたまま 5 分
    let (_, back) = calm(&h, &mut d, t, 900);
    assert_eq!(back.len(), 1, "{:?}", back);
    assert_eq!(back[0].kind, Kind::Errors);
    assert!(
        back[0].text.ends_with("(0 errors in 5m)"),
        "{}",
        back[0].text
    );
}

/// (4) 同時接続の山が `max_conns` の 50% 以上。
#[test]
fn a_high_active_peak_fires_once_and_clears() {
    let h = History::default();
    let mut d = Detector::new();
    d.set_limits(240, 120);
    let (mut t, _) = calm(&h, &mut d, 0, 60);
    let mut fired = Vec::new();
    for active in [195, 218, 201] {
        let mut p = connects(t, 2, 10);
        p.active_max = active;
        fired.extend(feed(&h, &mut d, p));
        t += 5;
    }
    assert_eq!(fired.len(), 1, "{:?}", fired);
    assert_eq!(fired[0].kind, Kind::ActiveHigh);
    assert_eq!(
        fired[0].text,
        "active_high: active connections peaked at 195 in 5m of max_conns 240 (81%, threshold 120)"
    );
    let (_, back) = calm(&h, &mut d, t, 900);
    assert_eq!(back.len(), 1, "{:?}", back);
    assert!(
        back[0].text.starts_with("cleared: active_high after "),
        "{}",
        back[0].text
    );
}

/// (4) 写真が撮れていればその番号を持つ (T14.6 の `seq`)。
#[test]
fn a_new_burst_shot_carries_its_number() {
    let h = History::default();
    let mut d = Detector::new();
    d.set_limits(240, 120);
    let shots = |n| Counters {
        shots: n,
        ..Counters::default()
    };
    let empty = feed_with(&h, &mut d, point(0), shots(6));
    assert!(empty.is_empty(), "1 本目は比べる相手が無い");
    let mut p = point(5);
    p.active_max = 130;
    let fired = feed_with(&h, &mut d, p, shots(7));
    assert_eq!(fired.len(), 1);
    assert_eq!(
        fired[0].text,
        "active_high: active connections peaked at 130 in 5m of max_conns 240 (54%, threshold 120), burst shot #7"
    );
}

/// 5 秒の標本が山を見逃しても写真が気づく (accept の経路は越えた瞬間に旗を立てる)。
/// そのときは「写真が越えた」と書く (標本の山と数字が食い違って見えないように)。
#[test]
fn a_spike_between_two_samples_is_written_as_the_photo() {
    let h = History::default();
    let mut d = Detector::new();
    d.set_limits(4, 2);
    assert!(feed(&h, &mut d, point(0)).is_empty());
    let mut p = point(5);
    p.active_max = 1;
    let fired = feed_with(
        &h,
        &mut d,
        p,
        Counters {
            shots: 1,
            ..Counters::default()
        },
    );
    assert_eq!(fired.len(), 1);
    assert_eq!(
        fired[0].text,
        "active_high: burst shot #1 crossed the threshold 2 of max_conns 4 (sampled peak 1 in 5m)"
    );
}

/// (5) 断った / 追い出した数が増えた。
#[test]
fn growing_reject_counters_fire_once_and_clear() {
    let h = History::default();
    let mut d = Detector::new();
    let (mut t, _) = calm(&h, &mut d, 0, 60);
    let rejected = |n| Counters {
        rejected_overload: n,
        ..Counters::default()
    };
    let mut fired = Vec::new();
    let mut p = connects(t, 2, 10);
    p.evicted_idle = 3;
    fired.extend(feed_with(&h, &mut d, p, rejected(12)));
    t += 5;
    // 同じ山でもう 1 回増えても 2 件目は書かない
    let mut p = connects(t, 2, 10);
    p.evicted_idle = 3;
    fired.extend(feed_with(&h, &mut d, p, rejected(20)));
    t += 5;
    assert_eq!(fired.len(), 1, "{:?}", fired);
    assert_eq!(fired[0].kind, Kind::Rejected);
    assert_eq!(
        fired[0].text,
        "rejected: rejected_overload +12 (total 12), evicted_idle +3 (total 3)"
    );
    // 増えなくなってから 5 分で解除 (累計は 20 / 3 のまま)
    let mut back = Vec::new();
    while t < 60 + 700 {
        let mut p = connects(t, 2, 10);
        p.evicted_idle = 3;
        back.extend(feed_with(&h, &mut d, p, rejected(20)));
        t += 5;
    }
    assert_eq!(back.len(), 1, "{:?}", back);
    assert_eq!(
        back[0].text,
        "cleared: rejected after 6m (no new rejects for 5m)"
    );
}

/// 5 種が同時に立っても、それぞれ 1 回ずつ (受け入れ基準)。
#[test]
fn each_of_the_five_fires_once_and_clears_once() {
    let h = History::default();
    let mut d = Detector::new();
    d.set_limits(240, 120);
    let (mut t, _) = calm(&h, &mut d, 0, BASE_SECS);
    let mut fired = Vec::new();
    let start = t;
    while t < start + 600 {
        let mut p = connects(t, 4, 300);
        p.dns_misses = 2;
        p.dns_ms_sum = 2 * 250;
        p.errors = 3;
        p.errors_by_cause[0] = 3;
        p.active_max = 218;
        let c = Counters {
            rejected_overload: (t - start) / 5,
            ..Counters::default()
        };
        fired.extend(feed_with(&h, &mut d, p, c));
        t += 5;
    }
    let mut kinds: Vec<&str> = fired.iter().map(|f| f.kind.name()).collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [
            "active_high",
            "connect_p95",
            "dns_slow",
            "errors",
            "rejected"
        ],
        "5 種が 1 回ずつ: {:?}",
        fired
    );
    assert!(fired.iter().all(|f| !f.cleared));
    assert!(
        fired
            .iter()
            .all(|f| f.text.len() <= crate::events::MAX_TEXT)
    );
    // 収まったら 5 種とも解除が 1 回ずつ
    let (_, back) = calm(&h, &mut d, t, 1200);
    let mut kinds: Vec<&str> = back.iter().map(|f| f.kind.name()).collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [
            "active_high",
            "connect_p95",
            "dns_slow",
            "errors",
            "rejected"
        ],
        "解除も 1 回ずつ: {:?}",
        back
    );
    assert!(back.iter().all(|f| f.cleared));
    assert!(back.iter().all(|f| f.text.len() <= crate::events::MAX_TEXT));
    assert!(d.firing().is_empty());
}

/// 静かなプロセスでは 1 件も書かない (2 時間)。
#[test]
fn a_quiet_process_writes_nothing() {
    let h = History::default();
    let mut d = Detector::new();
    d.set_limits(240, 120);
    let (t, out) = calm(&h, &mut d, 0, 2 * BASE_SECS);
    assert!(out.is_empty(), "{:?}", out);
    // 標本が 1 本も無い (要求が来ていない) 時間帯も同じ
    let mut t = t;
    let mut out = Vec::new();
    for _ in 0..720 {
        out.extend(feed(&h, &mut d, point(t)));
        t += 5;
    }
    assert!(out.is_empty(), "{:?}", out);
}

/// 窓は直近 5 分と直近 1 時間 (どちらも T14.24 の [`summary::of`] が切る)。
///
/// 5 秒の標本しか読まないので、基準値は**リングが覆う 1 時間**そのもの。
#[test]
fn the_windows_are_five_minutes_and_one_hour() {
    let h = History::default();
    let mut d = Detector::new();
    let (t, _) = calm(&h, &mut d, 0, 2 * BASE_SECS);
    let w5 = fold(&h, t - 5, WINDOW_SECS);
    let base = fold(&h, t - 5, BASE_SECS);
    // 窓は両端を含むので 5 分 = 61 標本、1 時間 = 721 標本。
    // **T14.32 で 5 秒のリングが 1 時間 (720 本) から 6 時間 (4,320 本) になった**ので、
    // 基準値の窓はリングに切られず 721 本そろう (それ以前は 720 本で頭打ちだった)
    assert_eq!(w5.samples, WINDOW_SECS / 5 + 1);
    assert_eq!(w5.connect.count, 2 * (WINDOW_SECS / 5 + 1));
    assert_eq!(base.samples, BASE_SECS / 5 + 1, "リングが覆うのは 6 時間");
    assert_eq!(base.connect.count, 2 * (BASE_SECS / 5 + 1));
    assert_eq!(base.interval_secs, 5);
}

#[test]
fn the_eight_kinds_have_distinct_names() {
    let mut names: Vec<&str> = KINDS.iter().map(|k| k.name()).collect();
    assert_eq!(names.len(), 8);
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 8, "名前が重なっている");
    // **添字の定数と `KINDS` の並びが合っていること** (ずれても型では落ちない)
    for (i, kind) in [
        (I_CONNECT_P95, Kind::ConnectP95),
        (I_DNS_SLOW, Kind::DnsSlow),
        (I_ERRORS, Kind::Errors),
        (I_ACTIVE_HIGH, Kind::ActiveHigh),
        (I_REJECTED, Kind::Rejected),
        (I_CPU_THROTTLED, Kind::CpuThrottled),
        (I_TUNNEL_SPIN, Kind::TunnelSpin),
        (I_DNS_MISS_RATE, Kind::DnsMissRate),
    ] {
        assert_eq!(KINDS[i], kind, "添字 {} がずれている", i);
    }
    // 既存 5 種の添字は動いていない (T14.9 以来の並び)
    assert_eq!(I_REJECTED, 4);
}

// ------------------------------------------------- T15.0 (6)(9) で足した 3 種

/// (7) CPU の絞りは**割合**で立つ (50% 以上) — 5 分ぶん溜まるまでは判定しない。
///
/// 累計そのものは親の階層の値なので、見るのは 5 分の窓の増分どうしの比。
#[test]
fn cpu_throttling_fires_over_half_and_clears_under_a_quarter() {
    let h = History::default();
    let mut d = Detector::new();
    // 起動より前から動いている累計 (0 時間の雪像にも 915 が乗っていた)
    let (mut throttled, mut periods, mut used) = (915u64, 8_000u64, 0u64);
    let mut t = 0u64;
    let mut fired = Vec::new();
    while t <= WINDOW_SECS {
        fired.extend(feed_cpu(&h, &mut d, point(t), throttled, periods, used));
        if t < WINDOW_SECS {
            assert!(
                fired.is_empty(),
                "5 分に満たない窓では判定しない: {:?}",
                fired
            );
        }
        t += 5;
        periods += 50; // 5 秒 = 100 ms の期間 50 個
        throttled += 50; // そのうち全部絞られた (2026-09-18 のデプロイ先は約 100%)
        used += 4_000_000; // 4 秒ぶん = 0.8 コア
    }
    assert_eq!(fired.len(), 1, "立つのは 1 回だけ: {:?}", fired);
    assert_eq!(fired[0].kind, Kind::CpuThrottled);
    assert_eq!(
        fired[0].text,
        "cpu_throttled: 100% of 3000 cpu periods throttled in 5m \
         (threshold 50%; quota 1.00 cores, used 0.80)"
    );
    assert!(fired[0].text.len() <= crate::events::MAX_TEXT);
    // 絞られなくなったら、窓から抜けて 25% を下回り、そこから 5 分で解除
    let mut back = Vec::new();
    while t <= 1_500 {
        back.extend(feed_cpu(&h, &mut d, point(t), throttled, periods, used));
        t += 5;
        periods += 50; // 期間は過ぎ続けるが絞られない
        used += 1_000_000;
    }
    assert_eq!(back.len(), 1, "解除も 1 回だけ: {:?}", back);
    assert!(back[0].cleared);
    assert_eq!(
        back[0].text,
        "cleared: cpu_throttled after 9m (0% of 3000 cpu periods throttled in 5m)"
    );
    assert!(d.firing().is_empty());
}

/// (7) **刻みが 1 秒ずれても立つ** (T15.0 単位 4 のレビュー)。
///
/// 履歴スレッドの 1 周は `sleep(5s)` の**あと**に 1 周ぶんの仕事をするので、
/// 5 秒ちょうどにはならず 6 秒の間隔が周期的に混ざる。「5 分ぶん溜まったか」を
/// **窓の両端の差**で見ていたころは、この系列では門が 1 度も開かなかった
/// (差は 296〜299 秒にしかならず、300 に一致する刻みが来ない)。
#[test]
fn cpu_throttling_fires_even_when_the_ticks_drift() {
    let h = History::default();
    let mut d = Detector::new();
    let (mut throttled, mut periods, mut used) = (915u64, 8_000u64, 0u64);
    let (mut t, mut step) = (0u64, 0usize);
    let mut fired = Vec::new();
    while t <= 2 * WINDOW_SECS {
        fired.extend(feed_cpu(&h, &mut d, point(t), throttled, periods, used));
        if t < WINDOW_SECS {
            assert!(
                fired.is_empty(),
                "5 分に満たない間は判定しない: {:?}",
                fired
            );
        }
        // 5, 5, 6, 5, 5, 6, … (1 周が 5 秒より長い = 本番の周期)
        let gap = if step % 3 == 2 { 6 } else { 5 };
        step += 1;
        t += gap;
        periods += 10 * gap; // 100 ms の期間が 1 秒に 10 個
        throttled += 10 * gap; // そのうち全部絞られた
        used += 800_000 * gap; // 0.8 コア
    }
    assert_eq!(fired.len(), 1, "立つのは 1 回だけ: {:?}", fired);
    assert_eq!(fired[0].kind, Kind::CpuThrottled);
    assert!(d.firing().contains(&Kind::CpuThrottled), "{:?}", d.firing());
}

/// (7) `cpu.stat` が読めない環境 (cgroup v1・Linux 以外) では 1 件も立たない。
#[test]
fn a_machine_without_a_cpu_quota_never_fires_cpu_throttled() {
    let h = History::default();
    let mut d = Detector::new();
    let (_, out) = calm(&h, &mut d, 0, 2 * BASE_SECS);
    assert!(out.is_empty(), "{:?}", out);
    assert!(
        !d.firing().contains(&Kind::CpuThrottled),
        "{:?}",
        d.firing()
    );
}

/// (8) 回りっ放しのトンネルは **1 分続いてから** 1 件だけ立つ。
#[test]
fn a_spinning_tunnel_fires_once_after_a_minute() {
    let h = History::default();
    let mut d = Detector::new();
    let conns = ConnTable::new();
    let slot = conns
        .register(77, "198.51.100.5", std::time::Instant::now())
        .expect("枠ができる");
    slot.begin_tunnel("origin.example:443");
    slot.set_half_closed(crate::recent::CLIENT_SIDE);
    conns.update_rates(0); // 1 回目は控えるだけ (起動直後の周期)
    // 「起こされたのに 1 バイトも進まなかった」が 1 周期で 45,000 回
    slot.set_relaying(0, 45_000, 0);
    conns.update_rates(5_000); // 2 回目で差分が出る
    assert_eq!(slot.spins_delta(), 45_000);

    let mut t = 0u64;
    let mut fired = Vec::new();
    while t <= TUNNEL_SPIN_SECS {
        fired.extend(feed_conns(
            &h,
            &mut d,
            Some(&conns),
            point(t),
            Counters::default(),
        ));
        if t < TUNNEL_SPIN_SECS {
            assert!(
                fired.is_empty(),
                "1 分に満たないうちは立たない: {:?}",
                fired
            );
        }
        t += 5;
    }
    assert_eq!(fired.len(), 1, "立つのは 1 回だけ: {:?}", fired);
    assert_eq!(fired[0].kind, Kind::TunnelSpin);
    assert_eq!(
        fired[0].text,
        "tunnel_spin: conn#77 spun 45000 times/tick for 60s \
         (origin.example:443, age 0s, half_closed client)"
    );
    assert!(fired[0].text.len() <= crate::events::MAX_TEXT);
    // 回らなくなったら 5 分で解除 (差分は 0 に戻る)
    slot.set_relaying(1_000, 45_000, 0);
    conns.update_rates(5_000);
    assert_eq!(slot.spins_delta(), 0);
    let mut back = Vec::new();
    while t <= TUNNEL_SPIN_SECS + CLEAR_SECS + 10 {
        back.extend(feed_conns(
            &h,
            &mut d,
            Some(&conns),
            point(t),
            Counters::default(),
        ));
        t += 5;
    }
    assert_eq!(back.len(), 1, "{:?}", back);
    assert_eq!(
        back[0].text,
        "cleared: tunnel_spin after 6m (no tunnel spinning for 5m)"
    );
}

/// (8) 表を渡さなければ (テストと `--lite`) `tunnel_spin` は判定しない。
#[test]
fn without_a_conn_table_the_spin_rule_is_quiet() {
    let h = History::default();
    let mut d = Detector::new();
    let (_, out) = calm(&h, &mut d, 0, 600);
    assert!(out.is_empty(), "{:?}", out);
}

/// (9) ミスの**率**が 0.40/接続 以上で立つ。ただし**起動から 6 時間**は立てない。
///
/// 再起動の直後はどの名前も warm でないので、率は構造的に高い (実測 0.291)。
#[test]
fn a_high_dns_miss_rate_fires_after_the_warmup() {
    let h = History::default();
    let mut d = Detector::new();
    // 1 標本 20 本の確立に 11 回のミス = 0.55/接続 (Phase 13 と同じ高さ)。
    // ミス 1 回は 10 ms なので `dns_slow` (100 ms) は立たない
    let sample = |t: u64| {
        let mut p = connects(t, 20, 10);
        p.dns_misses = 11;
        p.dns_ms_sum = 11 * 10;
        p
    };
    // 起動から 3 時間: 率は 0.55 でも立たない (暖機中)
    let mut t = 0u64;
    let mut warmup = Vec::new();
    while t <= BASE_SECS {
        warmup.extend(feed_uptime(&h, &mut d, sample(t), 3 * 3600));
        t += 5;
    }
    assert!(warmup.is_empty(), "暖機中は書かない: {:?}", warmup);
    // 6 時間を越えると同じ率で立つ
    let fired = feed_uptime(&h, &mut d, sample(t), DNS_MISS_RATE_WARMUP_SECS);
    assert_eq!(fired.len(), 1, "{:?}", fired);
    assert_eq!(fired[0].kind, Kind::DnsMissRate);
    assert_eq!(
        fired[0].text,
        "dns_miss_rate: dns misses 0.55/connect over 1h \
         (threshold 0.20; 7931 misses, 14420 connects)"
    );
    assert!(fired[0].text.len() <= crate::events::MAX_TEXT);
    t += 5;
    // 同じ種類は収まるまで 1 回だけ
    assert!(feed_uptime(&h, &mut d, sample(t), DNS_MISS_RATE_WARMUP_SECS).is_empty());
}

/// (9) いまの切り方 (0.094〜0.171) では立たない。
#[test]
fn a_normal_dns_miss_rate_never_fires() {
    let h = History::default();
    let mut d = Detector::new();
    // 1 標本 20 本の確立に 3 回のミス = 0.15/接続 (2026-09-18 の実測の上の方)
    let mut t = 0u64;
    let mut out = Vec::new();
    while t <= BASE_SECS + 600 {
        let mut p = connects(t, 20, 10);
        p.dns_misses = 3;
        p.dns_ms_sum = 3 * 10;
        out.extend(feed_uptime(&h, &mut d, p, 24 * 3600));
        t += 5;
    }
    assert!(out.is_empty(), "0.15/接続 では書かない: {:?}", out);
}

/// (9) 確立が 20 本に満たない 5 分の窓では `connect_p95` を立てない。
///
/// p95 は 250 ms 超が 1〜2 本しかない窓では**窓の最大値に潰れる**
/// (仕様なので触らない)。257 / 265 / 275 ms で立った件はどれもこれだった。
#[test]
fn connect_p95_needs_twenty_samples_in_the_window() {
    let h = History::default();
    let mut d = Detector::new();
    // 基準値 (1 時間で 10 ms) を作る
    let (mut t, quiet) = calm(&h, &mut d, 0, BASE_SECS);
    assert!(quiet.is_empty(), "{:?}", quiet);
    // 5 分ぶん静かにして、直近の窓から確立を抜く
    while t < BASE_SECS + WINDOW_SECS + 5 {
        assert!(feed(&h, &mut d, point(t)).is_empty());
        t += 5;
    }
    // 19 本の 250 ms では立たない (窓の最大値に潰れた p95 で立てない)
    let mut p = connects(t, 19, 250);
    p.t = t;
    assert!(feed(&h, &mut d, p).is_empty(), "19 本では立ってはいけない");
    t += 5;
    // 20 本目で立つ
    let fired = feed(&h, &mut d, connects(t, 1, 250));
    assert_eq!(fired.len(), 1, "{:?}", fired);
    assert_eq!(fired[0].kind, Kind::ConnectP95);
    assert_eq!(CONNECT_MIN_SAMPLES, 20);
}

/// 閾は `PROXY_BURST_PERCENT` と同じ本数。写真を撮らない設定なら本文の 50%。
#[test]
fn the_threshold_follows_the_burst_photo() {
    reset();
    configure(240, 120);
    assert_eq!(THRESHOLD.load(Ordering::Relaxed), 120);
    configure(240, usize::MAX);
    assert_eq!(THRESHOLD.load(Ordering::Relaxed), 120, "50% を自分で当てる");
    configure(0, usize::MAX);
    assert_eq!(
        THRESHOLD.load(Ordering::Relaxed),
        0,
        "上限なしなら判定しない"
    );
    reset();
}

/// 履歴スレッドの口 ([`check`]) が `/events` に `anomaly` で 1 件書く。
///
/// 出来事のリングは静的に 1 本なので、[`crate::events`] のテストと同じ鍵で 1 つずつ通す。
#[test]
fn check_writes_one_anomaly_event() {
    let _g = crate::events::TEST_LOCK.locked();
    crate::events::clear();
    reset();
    let metrics = Metrics::new();
    let mut sample = Sample {
        t: 1_789_251_465,
        ..Sample::default()
    };
    check(&metrics, &sample);
    assert!(crate::events::is_empty(), "1 本目では立たない");
    metrics
        .rejected_client_acl
        .fetch_add(7, std::sync::atomic::Ordering::Relaxed);
    sample.t += 5;
    check(&metrics, &sample);
    let (events, _) = crate::events::select(0, 10);
    assert_eq!(events.len(), 1, "{:?}", events);
    assert_eq!(events[0].kind, crate::events::EventKind::Anomaly);
    assert_eq!(events[0].text, "rejected: rejected_client_acl +7 (total 7)");
    // 同じ種類は収まるまで 1 回だけ
    sample.t += 5;
    check(&metrics, &sample);
    assert_eq!(crate::events::len(), 1);
    crate::events::clear();
    reset();
}

// ------------------------------------------------------------ 規則 6 (T14.54)

/// 接続元 1 件を 1 要求ぶん記録する (本番と同じ口だけを使う)。
fn visit(m: &Metrics, client: &str, agent: Option<&str>, target: &str) {
    if let Some(a) = agent {
        // `User-Agent` を読むのは接続の最初の要求だけ (T14.7)
        m.record_client_agent(client, a);
    }
    m.record_client(
        client,
        crate::metrics::HostOutcome::Bypass,
        0,
        (0, 0),
        None,
        Some(target),
    );
}

/// いま入っている接続元の `first_seen` (**本物の壁時計**)。
///
/// 窓はこの値に合わせて置く: 標本の時刻は注入できても `first_seen` は
/// [`crate::cache::now_epoch`] なので、勝手な時刻で窓を切ると秒の境目で落ちる。
fn first_seen_of(m: &Metrics, client: &str) -> u64 {
    m.clients_first_seen_in(0, u64::MAX, 100)
        .0
        .iter()
        .find(|c| c.client == client)
        .unwrap_or_else(|| panic!("{} が居ない", client))
        .first_seen
}

/// (6) 初めて見た接続元が 1 回だけ出る (2 回目の要求では増えない)。
#[test]
fn f_a_new_client_is_written_once_per_client() {
    let m = Metrics::new();
    let mut d = Detector::new();
    visit(
        &m,
        "198.51.100.7",
        Some("t1454/1.0"),
        "connect://a.example:443",
    );
    let ta = first_seen_of(&m, "198.51.100.7");
    assert!(
        d.observe_clients(&m, ta - 1).is_empty(),
        "1 本目は基準にするだけ"
    );
    assert_eq!(
        d.observe_clients(&m, ta),
        vec![
            "new_client: 198.51.100.7 first seen \
             (1 req, first target port 443 (name), agent \"t1454/1.0\")"
                .to_string()
        ]
    );
    // 2 回目の要求では増えない。窓 (`[ta, tb]`) はこの接続元の `first_seen` を
    // また含むので、「もう書いた」を覚えていないと 2 件目が出る
    visit(
        &m,
        "198.51.100.7",
        Some("t1454/1.0"),
        "connect://b.example:443",
    );
    // 別の接続元は出る (IP リテラル宛て・`User-Agent` 無し)
    visit(&m, "198.51.100.8", None, "connect://203.0.113.9:8443");
    let tb = first_seen_of(&m, "198.51.100.8");
    assert_eq!(
        d.observe_clients(&m, tb),
        vec![
            "new_client: 198.51.100.8 first seen \
             (1 req, first target port 8443 (literal), no agent)"
                .to_string()
        ]
    );
}

/// (6) 読み戻した接続元 (`first_seen == 0`) は「新しい」ではない。
#[test]
fn f_restored_clients_are_not_new() {
    let m = Metrics::new();
    let mut d = Detector::new();
    let t0 = crate::cache::now_epoch();
    m.restore(
        Vec::new(),
        vec![(
            "198.51.100.1".to_string(),
            crate::metrics::HostStats {
                requests: 42,
                ..Default::default()
            },
        )],
    );
    assert!(d.observe_clients(&m, t0 - 1).is_empty());
    assert!(
        d.observe_clients(&m, t0 + 300).is_empty(),
        "前から居た接続元は新しくない"
    );
}

/// (6) 1 周期に何百も現れたら (走査) 上限で切って、残りは 1 行にまとめる。
#[test]
fn f_a_scan_is_folded_into_one_line() {
    let m = Metrics::new();
    let mut d = Detector::new();
    for i in 1..=(NEW_CLIENT_MAX + 3) {
        visit(
            &m,
            &format!("198.51.100.{}", i),
            None,
            "connect://a.example:443",
        );
    }
    let seen: Vec<u64> = m
        .clients_first_seen_in(0, u64::MAX, 100)
        .0
        .iter()
        .map(|c| c.first_seen)
        .collect();
    let (lo, hi) = (*seen.iter().min().unwrap(), *seen.iter().max().unwrap());
    assert!(d.observe_clients(&m, lo - 1).is_empty());
    let out = d.observe_clients(&m, hi);
    assert_eq!(out.len(), NEW_CLIENT_MAX + 1, "{:?}", out);
    assert!(
        out[NEW_CLIENT_MAX].starts_with("new_client: 3 more clients first seen in the same "),
        "{}",
        out[NEW_CLIENT_MAX]
    );
    assert!(
        out[NEW_CLIENT_MAX].ends_with(&format!("window (wrote {})", NEW_CLIENT_MAX)),
        "{}",
        out[NEW_CLIENT_MAX]
    );
    // まとめた分も「もう書いた」に入るので、次の周期では出てこない
    assert!(d.observe_clients(&m, hi).is_empty());
}

/// 履歴スレッドの口 ([`check`]) が `/events` に `new_client` で 1 件書き、
/// 長い `User-Agent` でも 1 件 128 B に収まる (接続元と数字は残る)。
#[test]
fn check_writes_one_new_client_event() {
    let _g = crate::events::TEST_LOCK.locked();
    crate::events::clear();
    reset();
    let metrics = Metrics::new();
    let mut sample = Sample {
        t: crate::cache::now_epoch(),
        ..Sample::default()
    };
    check(&metrics, &sample);
    assert!(crate::events::is_empty(), "1 本目では書かない");
    visit(
        &metrics,
        "198.51.100.9",
        Some(&"u".repeat(200)),
        "connect://a.example:443",
    );
    sample.t = first_seen_of(&metrics, "198.51.100.9");
    check(&metrics, &sample);
    let (events, _) = crate::events::select(0, 10);
    assert_eq!(events.len(), 1, "{:?}", events);
    assert_eq!(events[0].kind, crate::events::EventKind::NewClient);
    assert!(
        events[0]
            .text
            .starts_with("new_client: 198.51.100.9 first seen (1 req, first target port 443"),
        "{}",
        events[0].text
    );
    assert!(
        events[0].text.len() <= crate::events::MAX_TEXT,
        "{} B",
        events[0].text.len()
    );
    // 同じ接続元は 1 回だけ (同じ秒の窓をもう一度見ても増えない)
    check(&metrics, &sample);
    assert_eq!(crate::events::len(), 1);
    crate::events::clear();
    reset();
}
