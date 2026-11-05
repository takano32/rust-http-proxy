//! `anomaly` の単体テスト (T22.2 で `anomaly.rs` の `mod tests` をそのまま移した。`anomaly.rs` が 2,000 行に
//! 届いていたため)。`check` を通すものは出来事のリングがプロセスで 1 組なので、今までどおり `reset` で片づける。
//!
//! 共通の道具と、規則をまたぐ試験をここに置き、規則ごとの試験は子のファイルに割った (T.34.47)。

mod clients;
mod cpu_spin_rate;
mod dns;
mod errors_active_rejected;
mod speed;

use super::detector::{dns_miss_limit, fold};
use super::*;
use crate::history::History;
use crate::metrics::ErrCause;
use crate::recent::ConnTable;

/// 5 秒周期の標本 1 本。
fn point(t: u64) -> Sample {
    Sample {
        t,
        ..Sample::default()
    }
}

/// `n` 本の CONNECT が `ms` ミリ秒で確立した標本 (RTT は 0 = 「確立 − RTT」も `ms`。T.29.11)。
fn connects(t: u64, n: usize, ms: u64) -> Sample {
    connects_far(t, n, ms, 0)
}

/// `n` 本の CONNECT が `ms` ミリ秒で確立し、オリジンまでの RTT が `rtt` ms の標本 (T.29.11)。
fn connects_far(t: u64, n: usize, ms: u64, rtt: u64) -> Sample {
    let mut p = point(t);
    for _ in 0..n {
        p.connect.observe(ms);
        p.excess.observe(ms.saturating_sub(rtt));
        p.conn_excess.observe(ms.saturating_sub(rtt));
    }
    p
}

/// 標本の名前解決に成功したミスを、全部 443・80 宛てのものとして数える (T.33.3。`dns_slow` は
/// `web_dns_misses`・`web_dns_ms_sum` で判定する。T.33.3 より前の試験はどれも 443 宛てのつもりで書いてある)。
fn all_web(mut p: Sample) -> Sample {
    p.web_dns_misses = p.dns_misses - p.dns_fail_misses;
    p.web_dns_ms_sum = p.dns_ms_sum - p.dns_fail_ms_sum;
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
        p.errors_by_cause[ErrCause::Timeout as usize] = 3;
        p.active_max = 218;
        let c = Counters {
            rejected_overload: (t - start) / 5,
            ..Counters::default()
        };
        fired.extend(feed_with(&h, &mut d, all_web(p), c));
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
