//! 標本 1 本の判定 ([`judge`]) の単体試験 (T.34.2 で `slo.rs` の `mod tests` から移した。名前はそのまま)。

use super::*;

/// **T.28.45**: 確立の 2 つの閾は「確立 − RTT」の窓で判定する (距離で外れない)。
/// 確立 40 ms (tokyo の 1 RTT 18 ms の 2 往復ぶん) でも、RTT を引いた残りが 2 ms なら満たす。
#[test]
fn the_connect_thresholds_judge_the_excess_over_rtt() {
    let th = DEFAULT;
    assert_eq!((th.connect_p50_ms, th.connect_p95_ms), (5.0, 50.0));
    let mut far = Sample::default();
    for _ in 0..10 {
        far.connect.observe(40);
        far.excess.observe(2);
        far.conn_excess.observe(2);
    }
    let v = judge(&th, &far);
    assert!(v.met(), "{:?}", v);
    // 段 (1, 2] の中を補間した値 (確立の 40 ms ではない)
    assert!(v.values[0] <= 2.0 && v.values[1] <= 2.0, "{:?}", v);
    // RTT を引いても遅い (p50 が (5, 10] の段) なら外す
    let mut slow = far;
    slow.conn_excess = crate::history::Window::default();
    for _ in 0..10 {
        slow.conn_excess.observe(8);
    }
    assert_eq!(judge(&th, &slow).bits, 0b0001, "{:?}", judge(&th, &slow));
    // 残りが 70 ms (p95 の閾 50 を越える) なら 2 つとも外す
    slow.conn_excess = crate::history::Window::default();
    slow.conn_excess.observe(70);
    assert_eq!(judge(&th, &slow).bits, 0b0011, "{:?}", judge(&th, &slow));
    // **T.29.12**: 名前解決が遅いだけ (`dns + connect − RTT` は 300 ms、`connect − RTT` は 2 ms) なら
    // 確立の 2 つは外さない (メール宛のリゾルバの遅れは `dns_slow` が見る)
    let mut slow_dns = Sample::default();
    for _ in 0..10 {
        slow_dns.connect.observe(20);
        slow_dns.excess.observe(300);
        slow_dns.conn_excess.observe(2);
    }
    assert!(judge(&th, &slow_dns).met(), "{:?}", judge(&th, &slow_dns));
    // 窓が空 (T.29.12 より前の標本・RTT が読めなかった本だけ) は確立の 2 つを外さない
    let mut old = Sample::default();
    old.connect.observe(300);
    let v = judge(&th, &old);
    assert!(v.judged && v.met(), "{:?}", v);
}

/// **T.28.45 (E1)**: `error_rate` は名前解決に成功した試みあたり
/// (`(errors − dns) ÷ (試み − dns)`)。名前解決の失敗だけの標本は「判定なし」。
#[test]
fn the_error_rate_counts_only_attempts_whose_name_resolved() {
    let th = DEFAULT;
    // 確立 100 本 + メール宛の NXDOMAIN 3 件 = 0 / 100 → 満たす (前は 3 / 103 で外した)
    let mut s = sample(0, 100, 3);
    s.errors = 3;
    s.errors_by_cause[crate::metrics::ErrCause::Dns as usize] = 3;
    let v = judge(&th, &s);
    assert!(v.met(), "{:?}", v);
    assert_eq!(v.values[2], 0.0);
    // 引けたあとの拒否が 1 件増えると 1 / (100 + 4 − 3) = 1 / 101
    s.errors = 4;
    s.errors_by_cause[crate::metrics::ErrCause::Refused as usize] = 1;
    let v = judge(&th, &s);
    assert_eq!(v.bits, 0b0100, "{:?}", v);
    assert!((v.values[2] - 1.0 / 101.0).abs() < 1e-12, "{:?}", v);
    // 名前解決の失敗だけ (確立 0 本) の標本は判定しない (分母に入れない)
    let mut only = Sample {
        errors: 5,
        dns_misses: 5,
        dns_fail_misses: 5,
        ..Sample::default()
    };
    only.errors_by_cause[crate::metrics::ErrCause::Dns as usize] = 5;
    assert!(!judge(&th, &only).judged, "{:?}", judge(&th, &only));
    let mut tr = Tracker::new();
    tr.observe(&th, &only);
    assert_eq!(tr.hours().count(), 0);
}

/// **T.30.3**: `error_rate` は宛先のポートが 443・80 の試みだけ (利用者の決定)。5 秒の標本の
/// `other_port_attempts` / `other_port_errors` を今までの分子と分母から引く。どちらも 0 の標本
/// (T.30.3 より前のレコード) は全部のポートのまま。443・80 以外のエラーだけの標本は「判定なし」。
#[test]
fn the_error_rate_counts_only_the_attempts_to_ports_443_and_80() {
    let th = DEFAULT;
    // `:443` の確立 100 本 + `:993` の時間切れ 10 本 (試み 10 本・エラー 10 本)
    let mut s = sample(0, 100, 3);
    s.errors = 10;
    s.errors_by_cause[crate::metrics::ErrCause::Timeout as usize] = 10;
    let old = judge(&th, &s);
    assert_eq!(
        (old.num[0], old.den[0]),
        (10, 110),
        "旧の数え方 (全部のポート)"
    );
    assert_eq!(old.bits, 0b0100, "{:?}", old);
    s.other_port_attempts = 10;
    s.other_port_errors = 10;
    let v = judge(&th, &s);
    assert_eq!((v.num[0], v.den[0]), (0, 100), "{:?}", v);
    assert!(v.met(), "{:?}", v);
    // `:443` のエラーは数える (1 / 101)
    s.errors = 11;
    s.errors_by_cause[crate::metrics::ErrCause::Refused as usize] = 1;
    let v = judge(&th, &s);
    assert_eq!((v.num[0], v.den[0]), (1, 101), "{:?}", v);
    // 443・80 以外の時間切れだけの 5 秒は判定しない (分子も分母も 0)
    let mut mail = Sample {
        errors: 5,
        other_port_attempts: 5,
        other_port_errors: 5,
        ..Sample::default()
    };
    mail.errors_by_cause[crate::metrics::ErrCause::Timeout as usize] = 5;
    let v = judge(&th, &mail);
    assert!(!v.judged, "{:?}", v);
    assert_eq!((v.num[0], v.den[0]), (0, 0), "{:?}", v);
    // 名前解決の失敗は引く前に除いてある (`:993` の NXDOMAIN は other_port に入らない)
    let mut nx = sample(0, 100, 3);
    nx.errors = 3;
    nx.errors_by_cause[crate::metrics::ErrCause::Dns as usize] = 3;
    nx.errors += 2;
    nx.errors_by_cause[crate::metrics::ErrCause::Timeout as usize] = 2;
    nx.other_port_attempts = 2;
    nx.other_port_errors = 2;
    let v = judge(&th, &nx);
    assert_eq!((v.num[0], v.den[0]), (0, 100), "{:?}", v);
    // 確立の 2 つと `dns_miss_per_connect` は全部のポートのまま
    assert_eq!(v.den[1], 100);
}

/// **T.28.45**: `dns_miss_per_connect` は名前解決に失敗した試みのミスを除く
/// (`(dns_misses − dns_fail_misses) ÷ 確立`)。
#[test]
fn the_miss_rate_leaves_out_the_misses_of_failed_lookups() {
    let th = DEFAULT;
    let mut s = sample(0, 10, 3);
    s.dns_misses = 5;
    s.dns_fail_misses = 3;
    let v = judge(&th, &s);
    assert!(v.met(), "閾ちょうど 2 / 10: {:?}", v);
    assert!((v.values[3] - 0.2).abs() < 1e-12, "{:?}", v);
    s.dns_fail_misses = 2;
    assert_eq!(judge(&th, &s).bits, 0b1000, "3 / 10");
    // 食い違った標本 (失敗のミスがミス全体より多い) でも負にならない
    s.dns_fail_misses = 9;
    assert_eq!(judge(&th, &s).values[3], 0.0);
}

#[test]
fn a_sample_without_a_connect_or_an_error_is_not_judged() {
    let th = DEFAULT;
    // forward が通っただけの標本も今までどおり判定しない
    let mut empty = Sample::default();
    empty.forward.observe(5);
    let v = judge(&th, &empty);
    assert!(!v.judged, "{:?}", v);
    assert!(!v.met());
    // 達成率の分母にも入らない (T.29.33 から、転送の 1 本は比の 1 時間の合計の分母には入る)
    let mut tr = Tracker::new();
    tr.observe(&th, &empty);
    let hours: Vec<Hour> = tr.hours().collect();
    assert!(
        hours.iter().all(|h| h.judged == 0 && h.ratio().is_none()),
        "{:?}",
        hours
    );
    assert_eq!(hours.iter().map(|h| h.den[0]).sum::<u64>(), 1);
}

/// 繋がらない CONNECT だけの標本 (T20.4 のあとは確立 0 本・エラー N 件) は、
/// **エラー率だけ外した標本**として数える (速さとミスの 3 つは外さない)。
#[test]
fn a_sample_with_only_errors_misses_the_error_rate_alone() {
    let th = DEFAULT;
    let only_errors = Sample {
        t: 0,
        errors: 9,
        dns_misses: 9,
        ..Sample::default()
    };
    let v = judge(&th, &only_errors);
    assert!(v.judged, "{:?}", v);
    assert_eq!(v.bits, 0b0100, "{:?}", v);
    assert_eq!(v.values, [0.0, 0.0, 1.0, 0.0]);
    assert!(!v.met());
    let mut tr = Tracker::new();
    tr.observe(&th, &only_errors);
    let h = tr.hours().next().expect("判定した標本が 1 つある");
    assert_eq!((h.judged, h.met, h.miss), (1, 0, [0, 0, 1, 0]));
}

#[test]
fn each_of_the_four_thresholds_sets_its_own_bit() {
    let th = DEFAULT;
    // 確立 1 本 3 ms、エラーも名前解決も無い = 4 つとも満たす
    let ok = sample(0, 1, 3);
    assert_eq!(judge(&th, &ok).bits, 0);
    assert!(judge(&th, &ok).met());
    // p50 と p95 (1 本しか無いので同じ値が両方に効く)
    let slow = sample(0, 1, 300);
    assert_eq!(judge(&th, &slow).bits, 0b0011, "{:?}", judge(&th, &slow));
    // エラー率だけ: 確立 100 本 + エラー 1 件 = 1/101 = 0.0099 > 0.005
    let mut err = sample(0, 100, 3);
    err.errors = 1;
    let v = judge(&th, &err);
    assert_eq!(v.bits, 0b0100, "{:?}", v);
    assert!((v.values[2] - 1.0 / 101.0).abs() < 1e-12, "{:?}", v);
    // 名前解決だけ: 確立 10 本にミス 3 回 = 0.3 > 0.2
    let mut dns = sample(0, 10, 3);
    dns.dns_misses = 3;
    let v = judge(&th, &dns);
    assert_eq!(v.bits, 0b1000, "{:?}", v);
    assert!((v.values[3] - 0.3).abs() < 1e-12, "{:?}", v);
    // 閾ちょうどは満たす (満たす = 閾以下)
    let mut edge = sample(0, 10, 3);
    edge.dns_misses = 2;
    assert_eq!(judge(&th, &edge).bits, 0);
}

/// **T20.4**: `connects` は「繋がった本数」(失敗した CONNECT は確立の窓に入らず、
/// `errors` にだけ居る) なので、`attempts = connects + forwards + errors` は
/// 失敗 1・成功 1 で 2 になる (`0.18.0` までは失敗が `connects` にも居て 3 だった)。
#[test]
fn one_failure_and_one_success_are_two_attempts() {
    let th = DEFAULT;
    // 本体 (`Metrics::record`) が作る形: 成功の 1 本だけが確立の窓に居て、失敗は `errors`
    let mut s = sample(0, 1, 3);
    s.errors = 1;
    let v = judge(&th, &s);
    assert!(v.judged);
    assert_eq!(v.values[2], 0.5, "エラー率 = 1 / (1 + 1): {:?}", v);
    // 失敗の時間は確立の窓に無いので、p50 / p95 は成功の 1 本 (3 ms) のまま
    assert_eq!(v.bits, 0b0100, "{:?}", v);
    // forward の成功も試みに入る: 1 / (1 + 2 + 1) = 0.25
    s.forward.observe(4);
    s.forward.observe(4);
    assert_eq!(judge(&th, &s).values[2], 0.25);
    // 率は 1 を超えない。失敗だけの標本はエラー率 1.0 で判定し、確立が 0 本なので
    // ミスの率は 0 で割らずに 0 のまま
    let mut only = sample(0, 0, 0);
    only.errors = 3;
    only.dns_misses = 3;
    let v = judge(&th, &only);
    assert!(v.judged, "{:?}", v);
    assert_eq!(v.values, [0.0, 0.0, 1.0, 0.0]);
}
