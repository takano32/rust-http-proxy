//! [`Detector::hits`]・[`Detector::transition`]・[`CpuWindow`] の単体試験 (T34.44)。
//!
//! 窓を畳まずに [`Reading`] を直に組んで渡す (履歴のリングも時計も要らない)。

use super::detector::{CpuWindow, Reading};
use super::*;
use crate::history::summary::Summary;
use crate::window::Window;

/// `ms` の確立が `n` 本入った窓。
fn excess(n: u64, ms: u64) -> Window {
    let mut w = Window::default();
    for _ in 0..n {
        w.observe(ms);
    }
    w
}

/// 規則 1 本だけを見る: その規則の当たりと、ほかの 7 本が全部外れていること。
fn only(d: &Detector, r: &Reading, i: usize) -> bool {
    let hits = d.hits(r);
    for (j, h) in hits.iter().enumerate() {
        if j != i {
            assert!(!h, "{} が立った", KINDS[j].name());
        }
    }
    hits[i]
}

#[test]
fn connect_p95_needs_the_ratio_the_floor_and_twenty_samples() {
    let d = Detector::new();
    // 直近 5 分は 60 ms が 20 本 (p95 60 ms)。基準は 10 ms が 1,000 本を足して p95 は 10 ms 弱
    let mut base = excess(1000, 10);
    base.merge(&excess(20, 60));
    let r = |n: u64| Reading {
        w5: Summary {
            conn_excess: excess(n, 60),
            ..Summary::default()
        },
        base: Summary {
            conn_excess: base,
            ..Summary::default()
        },
        ..Reading::default()
    };
    assert!(
        !only(&d, &r(CONNECT_MIN_SAMPLES - 1), I_CONNECT_P95),
        "19 本では立たない"
    );
    assert!(
        only(&d, &r(CONNECT_MIN_SAMPLES), I_CONNECT_P95),
        "20 本で立つ"
    );
    // 基準の窓が空なら立たない
    let mut empty_base = r(CONNECT_MIN_SAMPLES);
    empty_base.base = Summary::default();
    assert!(!only(&d, &empty_base, I_CONNECT_P95));
    // 50 ms に届かない遅れは立たない (速いところの 3 倍は異常ではない)
    let mut fast = r(CONNECT_MIN_SAMPLES);
    fast.w5.conn_excess = excess(CONNECT_MIN_SAMPLES, 40);
    assert!(!only(&d, &fast, I_CONNECT_P95));
}

#[test]
fn dns_slow_needs_three_misses_the_average_and_the_sum() {
    let d = Detector::new();
    let r = |misses: u64, sum: u64| Reading {
        w5: Summary {
            web_dns_misses: misses,
            web_dns_ms_sum: sum,
            ..Summary::default()
        },
        dns_limit: DNS_MISS_MS,
        ..Reading::default()
    };
    assert!(only(&d, &r(DNS_MIN_MISSES, DNS_MIN_SUM_MS), I_DNS_SLOW));
    assert!(!only(
        &d,
        &r(DNS_MIN_MISSES - 1, DNS_MIN_SUM_MS),
        I_DNS_SLOW
    ));
    assert!(!only(
        &d,
        &r(DNS_MIN_MISSES, DNS_MIN_SUM_MS - 1),
        I_DNS_SLOW
    ));
    // 平均が閾ちょうど (10 回で 1,000 ms = 100 ms) は立ち、1 ms 足りなければ立たない
    assert!(only(&d, &r(10, 1_000), I_DNS_SLOW));
    let mut short = r(10, 1_000);
    short.dns_limit = DNS_MISS_MS + 1.0;
    assert!(!only(&d, &short, I_DNS_SLOW));
}

#[test]
fn errors_fire_from_the_fifth() {
    let d = Detector::new();
    let r = |errors: u64| Reading {
        w5: Summary {
            errors,
            ..Summary::default()
        },
        ..Reading::default()
    };
    assert!(!only(&d, &r(ERRORS_MIN - 1), I_ERRORS));
    assert!(only(&d, &r(ERRORS_MIN), I_ERRORS));
}

#[test]
fn active_high_fires_at_the_threshold_or_with_a_shot() {
    let mut d = Detector::new();
    let r = |peak: u64, shot: Option<u64>| Reading {
        w5: Summary {
            active_max: peak,
            ..Summary::default()
        },
        shot,
        ..Reading::default()
    };
    // 閾が 0 (`max_conns` 無し) なら山では立たない
    assert!(!only(&d, &r(1_000, None), I_ACTIVE_HIGH));
    d.set_limits(20, 10);
    assert!(!only(&d, &r(9, None), I_ACTIVE_HIGH));
    assert!(only(&d, &r(10, None), I_ACTIVE_HIGH));
    // 写真が撮れていれば標本の山が低くても立つ
    assert!(only(&d, &r(0, Some(3)), I_ACTIVE_HIGH));
}

#[test]
fn rejected_fires_on_any_increase() {
    let d = Detector::new();
    let r = |delta: [u64; 3]| Reading {
        delta,
        ..Reading::default()
    };
    assert!(!only(&d, &r([0, 0, 0]), I_REJECTED));
    for i in 0..3 {
        let mut delta = [0; 3];
        delta[i] = 1;
        assert!(only(&d, &r(delta), I_REJECTED), "{} 番目", i);
    }
}

#[test]
fn cpu_throttled_needs_a_full_window_and_half_the_periods() {
    let d = Detector::new();
    let r = |full: bool, throttled: u64| Reading {
        cpu: CpuWindow {
            full,
            throttled,
            periods: 100,
            ..CpuWindow::default()
        },
        ..Reading::default()
    };
    assert!(!only(&d, &r(true, 49), I_CPU_THROTTLED));
    assert!(only(&d, &r(true, 50), I_CPU_THROTTLED));
    assert!(
        !only(&d, &r(false, 100), I_CPU_THROTTLED),
        "5 分溜まるまでは見ない"
    );
}

#[test]
fn tunnel_spin_needs_a_minute() {
    let d = Detector::new();
    let r = |total: usize, secs: u64| Reading {
        spinning_total: total,
        spin_secs: secs,
        ..Reading::default()
    };
    assert!(!only(&d, &r(1, TUNNEL_SPIN_SECS - 1), I_TUNNEL_SPIN));
    assert!(only(&d, &r(1, TUNNEL_SPIN_SECS), I_TUNNEL_SPIN));
    assert!(!only(&d, &r(0, TUNNEL_SPIN_SECS), I_TUNNEL_SPIN));
}

#[test]
fn dns_miss_rate_needs_the_warmup_thirty_connects_and_the_rate() {
    let d = Detector::new();
    let r = |uptime: u64, connects: u64, misses: u64| Reading {
        base: Summary {
            connect: excess(connects, 10),
            dns_misses: misses,
            ..Summary::default()
        },
        uptime_secs: uptime,
        ..Reading::default()
    };
    let warm = DNS_MISS_RATE_WARMUP_SECS;
    // 30 本に 6 回 = 0.20 ちょうどで立ち、5 回 (0.17) では立たない
    assert!(only(&d, &r(warm, 30, 6), I_DNS_MISS_RATE));
    assert!(!only(&d, &r(warm, 30, 5), I_DNS_MISS_RATE));
    assert!(!only(&d, &r(warm - 1, 30, 6), I_DNS_MISS_RATE), "暖機中");
    assert!(
        !only(&d, &r(warm, 29, 6), I_DNS_MISS_RATE),
        "分母が足りない"
    );
}

#[test]
fn transition_fires_once_and_clears_after_five_calm_minutes() {
    let mut d = Detector::new();
    let r = |now: u64| Reading {
        now,
        ..Reading::default()
    };
    let mut hit = [false; KINDS.len()];
    hit[I_ERRORS] = true;
    let calm = [false; KINDS.len()];
    let fired = d.transition(&r(1_000), &hit);
    assert_eq!(fired.len(), 1);
    assert_eq!((fired[0].kind, fired[0].cleared), (Kind::Errors, false));
    // 立っている間は二度書かない
    assert!(d.transition(&r(1_005), &hit).is_empty());
    // 外れた最初の時刻から 300 秒で解除。途中で当たれば数え直す
    assert!(d.transition(&r(1_010), &calm).is_empty());
    assert!(d.transition(&r(1_100), &hit).is_empty());
    assert!(d.transition(&r(1_105), &calm).is_empty());
    assert!(d.transition(&r(1_105 + CLEAR_SECS - 1), &calm).is_empty());
    let cleared = d.transition(&r(1_105 + CLEAR_SECS), &calm);
    assert_eq!(cleared.len(), 1);
    assert!(cleared[0].cleared);
    // 1,000 → 1,405 秒 = 405 秒 = 7 分 (切り上げ)
    assert!(
        cleared[0].text.starts_with("cleared: errors after 7m"),
        "{}",
        cleared[0].text
    );
    assert!(d.firing().is_empty());
    // 解除のあとはまた立てる
    assert_eq!(d.transition(&r(2_000), &hit).len(), 1);
}

#[test]
fn transition_keeps_the_baseline_of_the_moment_it_fired() {
    let mut d = Detector::new();
    let mut low_base = excess(1000, 10);
    low_base.merge(&excess(20, 60));
    let fire = Reading {
        w5: Summary {
            conn_excess: excess(20, 60),
            ..Summary::default()
        },
        base: Summary {
            conn_excess: low_base,
            ..Summary::default()
        },
        ..Reading::default()
    };
    // 山が 1 時間続いて基準値そのものが山になった窓 (今の基準値と比べれば倍率は 1)
    let later = Reading {
        w5: fire.w5,
        base: Summary {
            conn_excess: excess(20, 60),
            ..Summary::default()
        },
        ..Reading::default()
    };
    assert!(!d.hits(&later)[I_CONNECT_P95], "立つ前は今の基準値と比べる");
    let hits = d.hits(&fire);
    assert_eq!(d.transition(&fire, &hits).len(), 1);
    assert!(
        d.hits(&later)[I_CONNECT_P95],
        "立っている間は立った瞬間の基準値と比べる"
    );
}

#[test]
fn the_cpu_window_reads_ratio_cores_and_top() {
    let w = CpuWindow {
        secs: 300,
        throttled: 25,
        periods: 100,
        used_us: 150_000_000,
        top_tid: 42,
        top_role: 1,
        ..CpuWindow::default()
    };
    assert_eq!(w.ratio(), Some(0.25));
    assert_eq!(w.cores(), 0.5);
    assert_eq!(w.top(), ", tid 42 conn");
    let empty = CpuWindow::default();
    assert_eq!(empty.ratio(), None, "期間が 1 つも過ぎていない");
    assert_eq!(empty.cores(), 0.0, "点が 1 つしか無い");
    assert_eq!(empty.top(), "", "標本が無い");
    let unknown = CpuWindow {
        top_tid: 7,
        top_role: 200,
        ..CpuWindow::default()
    };
    assert_eq!(unknown.top(), ", tid 7 other");
}
