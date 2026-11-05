//! (7) `cpu_throttled`・(8) `tunnel_spin`・(9) `dns_miss_rate` の試験。

use super::*;

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

/// 起動から 24 時間の 1 時間ちょっとに、1 標本 20 本の確立と `misses` 回のミス (うち `failed` 回は
/// 名前解決に失敗した試みのぶん) を流し、`dns_miss_rate` の変わり目を返す (T.32.36)。
fn miss_rate_fired(misses: u64, failed: u64) -> Vec<Fired> {
    let h = History::default();
    let mut d = Detector::new();
    let mut t = 0u64;
    let mut out = Vec::new();
    while t <= BASE_SECS + 60 {
        let mut p = connects(t, 20, 10);
        p.dns_misses = misses;
        p.dns_fail_misses = failed;
        p.dns_ms_sum = misses * 10;
        out.extend(feed_uptime(&h, &mut d, p, 24 * 3600));
        t += 5;
    }
    out.retain(|f| f.kind == Kind::DnsMissRate);
    out
}

/// (9) 名前解決に成功したミスが 0.3 回/接続 なら立つ (T.32.36 の受け入れ基準)。
#[test]
fn resolved_misses_at_point_three_per_connect_fire() {
    let out = miss_rate_fired(6, 0);
    assert_eq!(out.len(), 1, "{:?}", out);
    assert!(
        out[0]
            .text
            .starts_with("dns_miss_rate: dns misses 0.30/connect over 1h"),
        "{}",
        out[0].text
    );
}

/// (9) 名前解決に失敗した試みのミスは数えない (`/slo` と同じ。T.32.36)。0.30.4 の 1 時間の値は
/// 今までの数え方で最大 2.76、失敗を引くと最大 0.136 だった。失敗を外したら文面にその回数が入る
#[test]
fn failed_dns_misses_do_not_count_toward_the_rate() {
    // 1 標本 55 回のミスのうち 54 回が失敗 = 今までなら 2.75/接続、名前解決に成功したぶんは 0.05
    assert!(miss_rate_fired(55, 54).is_empty());
    // 成功したぶんが閾を越えれば立ち、外した回数を添える
    let out = miss_rate_fired(10, 4);
    assert_eq!(out.len(), 1, "{:?}", out);
    assert!(
        out[0].text.contains("failed not counted)"),
        "{}",
        out[0].text
    );
    assert!(
        out[0].text.len() <= crate::events::MAX_TEXT,
        "{}",
        out[0].text
    );
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
