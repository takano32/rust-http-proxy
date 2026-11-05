//! (2) `dns_slow` の試験 (名前解決に成功した 443・80 宛てのミスの重さ)。

use super::*;

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
        fired.extend(feed(&h, &mut d, all_web(p)));
        t += 5;
    }
    assert_eq!(fired.len(), 1, "{:?}", fired);
    assert_eq!(fired[0].kind, Kind::DnsSlow);
    // 1 本目 (3 回・420 ms) と 2 本目 (6 回・840 ms) は合計が [`DNS_MIN_SUM_MS`] に届かず、
    // 3 本目で立つ (T22.2)
    assert_eq!(
        fired[0].text,
        "dns_slow: dns miss 140 ms avg over 5m (threshold 100 ms; 9 misses, 1260 ms total)"
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
        out.extend(feed_with(&h, &mut d, all_web(p), c));
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

/// T22.2: ミスが少なく合計の小さい窓では立たない (平均が閾を越えていても)。
///
/// tokyo の 24 時間で立った 9 件のうち 5 件がこれ (3〜5 回で合計 311〜632 ms)。
#[test]
fn few_misses_with_a_small_sum_do_not_fire() {
    assert_eq!(DNS_MIN_SUM_MS, 1_000);
    let out = dns_misses_once_a_minute(3, 140, None);
    assert!(out.is_empty(), "3 回・合計 420 ms は立たない: {:?}", out);
    let out = dns_misses_once_a_minute(4, 240, None);
    assert!(out.is_empty(), "4 回・合計 960 ms は立たない: {:?}", out);
}

/// T22.2: 3 回でも合計が [`DNS_MIN_SUM_MS`] を越えれば立つ (1 回 334 ms 以上)。
#[test]
fn three_misses_with_a_large_sum_fire() {
    let out = dns_misses_once_a_minute(3, 340, None);
    assert_eq!(out.len(), 1, "{:?}", out);
    assert_eq!(
        out[0].text,
        "dns_slow: dns miss 340 ms avg over 5m (threshold 100 ms; 3 misses, 1020 ms total)"
    );
    // 4 回目で合計が届くときは 4 回目の標本で立つ
    let out = dns_misses_once_a_minute(4, 260, None);
    assert_eq!(out.len(), 1, "{:?}", out);
    assert!(
        out[0].text.contains("4 misses, 1040 ms total"),
        "{}",
        out[0].text
    );
}

/// 平常時の 1 時間のあと、5 秒ごとに 1 回 `each_ms` のミスを `secs` 秒流し、続けて
/// 静かな `calm_secs` 秒を流して、出た変わり目を全部返す (T22.2)。
fn dns_miss_every_sample(each_ms: u64, secs: u64, calm_secs: u64) -> Vec<Fired> {
    let h = History::default();
    let mut d = Detector::new();
    let (mut t, quiet) = calm(&h, &mut d, 0, BASE_SECS);
    assert!(quiet.is_empty(), "{:?}", quiet);
    let mut out = Vec::new();
    let end = t + secs;
    while t < end {
        let mut p = connects(t, 2, 10);
        p.dns_misses = 1;
        p.dns_ms_sum = each_ms;
        out.extend(feed(&h, &mut d, all_web(p)));
        t += 5;
    }
    out.extend(calm(&h, &mut d, t, calm_secs).1);
    out
}

/// T22.2: 多いミスで遅い窓 (本当に遅い名前解決) は今までどおり立ち、収まれば解除する。
#[test]
fn many_slow_misses_fire_and_clear() {
    // 5 秒ごとに 110 ms を 2 分 = 24 回。10 回目 (合計 1,100 ms) の標本で立つ
    let out = dns_miss_every_sample(110, 120, 900);
    assert_eq!(out.len(), 2, "{:?}", out);
    assert_eq!(out[0].kind, Kind::DnsSlow);
    assert_eq!(
        out[0].text,
        "dns_slow: dns miss 110 ms avg over 5m (threshold 100 ms; 10 misses, 1100 ms total)"
    );
    // 解除の条件は今までどおり (外れたまま 5 分)
    assert!(out[1].cleared);
    assert!(
        out[1].text.starts_with("cleared: dns_slow after "),
        "{}",
        out[1].text
    );
    // 多くても速いミス (50 ms) は立たない
    assert!(dns_miss_every_sample(50, 120, 0).is_empty());
}

/// T22.2: 立っている間に合計が [`DNS_MIN_SUM_MS`] を割っても、すぐには解除しない
/// (外れたまま [`CLEAR_SECS`] 続いたときだけ)。
#[test]
fn dns_slow_clears_only_after_five_calm_minutes() {
    // 2 分ぶん立てたあと、ミスが止まって約 4 分で窓のミスが 10 回を割り、合計が 1,000 ms を
    // 下回る (回数 3 以上・平均 110 ms のままでも外れる)。そこから 5 分は解除しないので、
    // 静けさ 400 秒ではまだ立っている
    let out = dns_miss_every_sample(110, 120, 400);
    assert_eq!(out.len(), 1, "{:?}", out);
    assert!(!out[0].cleared);
    // 静けさ 900 秒なら解除の 1 件が出る
    let out = dns_miss_every_sample(110, 120, 900);
    assert_eq!(out.len(), 2, "{:?}", out);
    assert!(out[1].cleared);
}

/// 平常時の 1 時間のあと、1 分ごとの頭の標本に名前解決のミスを 1 回ずつ `n` 回流す (`fail` 回目までは
/// 名前解決に失敗した試みのミス)。1 回は `each_ms`。`dns_slow` の変わり目だけを返す (T.32.16)。
fn dns_misses_with_failures(n: u64, fail: u64, each_ms: u64) -> Vec<Fired> {
    let h = History::default();
    let mut d = Detector::new();
    let (mut t, _) = calm(&h, &mut d, 0, BASE_SECS);
    let mut out = Vec::new();
    // 4 分 (48 標本) に収める: 1 標本に 1 回、5 秒おき
    for i in 0..48u64 {
        let mut p = connects(t, 2, 10);
        if i < n {
            p.dns_misses = 1;
            p.dns_ms_sum = each_ms;
            if i < fail {
                p.errors = 1;
                p.errors_by_cause[ErrCause::Dns as usize] = 1;
                p.dns_fail_misses = 1;
                p.dns_fail_ms_sum = each_ms;
            }
        }
        out.extend(feed(&h, &mut d, all_web(p)));
        t += 5;
    }
    out.retain(|f| f.kind == Kind::DnsSlow);
    out
}

/// T.32.16 の受け入れ基準: 名前解決の**失敗** 3 回 × 2 秒では立たない (0.30.4 の 62 件の大半がこれ。
/// `imap.froggy.com.au` の nxdomain 1 回が 2,151 ms)。成功した名前 10 回の平均 300 ms では立つ。
#[test]
fn slow_dns_failures_do_not_fire_but_slow_resolved_misses_do() {
    assert!(
        dns_misses_with_failures(3, 3, 2_000).is_empty(),
        "失敗だけでは立たない"
    );
    let out = dns_misses_with_failures(10, 0, 300);
    assert_eq!(out.len(), 1, "{:?}", out);
    // 4 回目 (合計 1,200 ms) で立つ
    assert_eq!(
        out[0].text,
        "dns_slow: dns miss 300 ms avg over 5m (threshold 100 ms; 4 misses, 1200 ms total)"
    );
    // 失敗 3 回 (2 秒) と成功 2 回 (50 ms) が混ざっても、成功が 3 回に届かないので立たない
    // (今までは 5 回・合計 6,100 ms で立っていた)
    assert!(dns_misses_with_failures(5, 3, 2_000).is_empty());
}

/// T.32.16: 失敗のぶんを外して立ったときは、外した回数を文面に添える (成功したミスの数字で書く)。
#[test]
fn dns_slow_says_how_many_failed_misses_it_left_out() {
    let h = History::default();
    let mut d = Detector::new();
    let (mut t, _) = calm(&h, &mut d, 0, BASE_SECS);
    let mut out = Vec::new();
    for _ in 0..4 {
        let mut p = connects(t, 2, 10);
        // 成功 1 回 400 ms + 失敗 1 回 2,000 ms
        p.dns_misses = 2;
        p.dns_ms_sum = 2_400;
        p.dns_fail_misses = 1;
        p.dns_fail_ms_sum = 2_000;
        p.errors = 1;
        p.errors_by_cause[ErrCause::Dns as usize] = 1;
        out.extend(feed(&h, &mut d, all_web(p)));
        t += 5;
    }
    out.retain(|f| f.kind == Kind::DnsSlow);
    assert_eq!(out.len(), 1, "{:?}", out);
    assert_eq!(
        out[0].text,
        "dns_slow: dns miss 400 ms avg over 5m (threshold 100 ms; 3 misses, 1200 ms total, \
         3 failed not counted)"
    );
    assert!(out[0].text.len() <= crate::events::MAX_TEXT);
}

/// 平常時の 1 時間のあと、5 秒ごとの標本に名前解決に成功したミスを 1 回ずつ流す: 443・80 以外の宛て (`:993`) を
/// `other` 回 × `other_ms`、そのあと 443・80 宛てを `web` 回 × `web_ms`。`dns_slow` の変わり目だけを返す (T.33.3)。
fn dns_misses_by_port(other: u64, other_ms: u64, web: u64, web_ms: u64) -> Vec<Fired> {
    let h = History::default();
    let mut d = Detector::new();
    let (mut t, _) = calm(&h, &mut d, 0, BASE_SECS);
    let mut out = Vec::new();
    // 4 分 (48 標本) に収める
    for i in 0..48u64 {
        let mut p = connects(t, 2, 10);
        if i < other {
            p.dns_misses = 1;
            p.dns_ms_sum = other_ms;
        } else if i < other + web {
            p.dns_misses = 1;
            p.dns_ms_sum = web_ms;
            p.web_dns_misses = 1;
            p.web_dns_ms_sum = web_ms;
        }
        out.extend(feed(&h, &mut d, p));
        t += 5;
    }
    out.retain(|f| f.kind == Kind::DnsSlow);
    out
}

/// T.33.3 の受け入れ基準 (利用者の決定): `dns_slow` は 443・80 宛ての接続のための名前解決だけで判定する。
/// `:993` 宛ての成功したミス 10 回 × 400 ms (0.32.39 の 15 件の形。NAT の接続元が初めて引いたメールのホスト)
/// では立たず、`:443` 宛ての 5 回 × 300 ms では立つ。外した 443・80 以外の回数は文面に添える。
#[test]
fn only_misses_for_443_and_80_make_dns_slow_fire() {
    assert!(
        dns_misses_by_port(10, 400, 0, 0).is_empty(),
        "`:993` 宛ての遅い名前解決だけでは立たない"
    );
    let out = dns_misses_by_port(0, 0, 5, 300);
    assert_eq!(out.len(), 1, "{:?}", out);
    // 4 回目 (合計 1,200 ms) で立つ
    assert_eq!(
        out[0].text,
        "dns_slow: dns miss 300 ms avg over 5m (threshold 100 ms; 4 misses, 1200 ms total)"
    );
    // 両方が混ざっても 443・80 のぶんだけで読み、外した回数を添える (失敗も外していればその回数も)
    let out = dns_misses_by_port(10, 400, 5, 300);
    assert_eq!(out.len(), 1, "{:?}", out);
    assert_eq!(
        out[0].text,
        "dns_slow: dns miss 300 ms avg over 5m (threshold 100 ms; 4 misses, 1200 ms total, \
         10 not to 443/80 not counted)"
    );
    // 443 宛ては 2 回 (合計 600 ms) だけ: `:993` の 10 回 × 400 ms を足せば今までは立っていた
    assert!(dns_misses_by_port(10, 400, 2, 300).is_empty());
}

/// T.33.3: 443・80 以外の宛てと失敗の両方を外したときの文面 (長さも出来事の上限に収まる)。
#[test]
fn dns_slow_says_how_many_misses_to_other_ports_and_failures_it_left_out() {
    let h = History::default();
    let mut d = Detector::new();
    let (mut t, _) = calm(&h, &mut d, 0, BASE_SECS);
    let mut out = Vec::new();
    for _ in 0..4 {
        let mut p = connects(t, 2, 10);
        // 443 宛ての成功 1 回 400 ms + `:995` 宛ての成功 2 回 + 失敗 1 回 2,000 ms
        p.dns_misses = 4;
        p.dns_ms_sum = 400 + 2 * 600 + 2_000;
        p.web_dns_misses = 1;
        p.web_dns_ms_sum = 400;
        p.dns_fail_misses = 1;
        p.dns_fail_ms_sum = 2_000;
        p.errors = 1;
        p.errors_by_cause[ErrCause::Dns as usize] = 1;
        out.extend(feed(&h, &mut d, p));
        t += 5;
    }
    out.retain(|f| f.kind == Kind::DnsSlow);
    assert_eq!(out.len(), 1, "{:?}", out);
    assert_eq!(
        out[0].text,
        "dns_slow: dns miss 400 ms avg over 5m (threshold 100 ms; 3 misses, 1200 ms total, \
         6 not to 443/80, 3 failed not counted)"
    );
    assert!(out[0].text.len() <= crate::events::MAX_TEXT);
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
