//! (3) `errors`・(4) `active_high`・(5) `rejected` の試験。

use super::*;

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
        p.errors_by_cause[1] = 2;
        p.errors_by_cause[4] = 1;
        fired.extend(feed(&h, &mut d, p));
        t += 5;
    }
    assert_eq!(fired.len(), 1, "{:?}", fired);
    assert_eq!(fired[0].kind, Kind::Errors);
    assert_eq!(
        fired[0].text,
        "errors: 6 errors in 5m (threshold 5): refused 4, reset 2"
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

/// 平常時の 1 分のあと、`n` 本の標本に `each` を当てて流し、`errors` の変わり目だけを返す (T32.36)。
fn errors_fired(n: usize, each: impl Fn(&mut Sample)) -> Vec<Fired> {
    let h = History::default();
    let mut d = Detector::new();
    let (mut t, _) = calm(&h, &mut d, 0, 60);
    let mut fired = Vec::new();
    for _ in 0..n {
        let mut p = point(t);
        each(&mut p);
        fired.extend(feed(&h, &mut d, p));
        t += 5;
    }
    fired.retain(|f| f.kind == Kind::Errors);
    fired
}

/// (3) 名前解決の失敗 (原因 `dns` の全部。`nxdomain` も `dns_temp` も) だけのエラーは何件あっても立たない
/// (T28.17 → T32.36)。メール宛の総当たりで 5 分に 6〜20 件の `nxdomain` が続き、`/events` の環が 5 日で回っていた。
#[test]
fn dns_failures_alone_do_not_fire() {
    let fired = errors_fired(120, |p| {
        p.errors = 4;
        p.errors_by_cause[ErrCause::Dns as usize] = 4;
        p.dns_misses = 4;
        p.dns_fail_misses = 4;
    });
    assert!(
        fired.is_empty(),
        "名前解決の失敗だけでは立たない: {:?}",
        fired
    );
}

/// (3) 443 宛ての `refused` が 5 分に 5 件で立つ (T32.36 の受け入れ基準)。
#[test]
fn five_refused_to_443_in_five_minutes_fire() {
    let fired = errors_fired(5, |p| {
        p.errors = 1;
        p.errors_by_cause[ErrCause::Refused as usize] = 1;
    });
    assert_eq!(fired.len(), 1, "{:?}", fired);
    assert_eq!(
        fired[0].text,
        "errors: 5 errors in 5m (threshold 5): refused 5"
    );
    // 4 件では立たない
    assert!(
        errors_fired(4, |p| {
            p.errors = 1;
            p.errors_by_cause[ErrCause::Refused as usize] = 1;
        })
        .is_empty()
    );
}

/// (3) `:993` (443・80 以外) の確立の時間切れは 5 分に 100 件でも立たない (T32.36 の受け入れ基準。
/// 0.30.4 の `history["60"]` の再生で、今までの数え方の 27 回はどれもこれと名前解決の失敗だった)。
#[test]
fn timeouts_to_other_ports_do_not_fire() {
    let fired = errors_fired(10, |p| {
        p.errors = 10;
        p.errors_by_cause[ErrCause::Timeout as usize] = 10;
        p.other_port_attempts = 10;
        p.other_port_errors = 10;
    });
    assert!(
        fired.is_empty(),
        "443・80 以外だけでは立たない: {:?}",
        fired
    );
}

/// (3) 外した残りが 5 件以上なら立ち、文面に外した数 (名前解決の失敗と 443・80 以外) が入る (T32.36)。
#[test]
fn errors_left_after_the_exclusions_still_fire_and_say_what_was_left_out() {
    // 1 標本に 443 の timeout 2 件 + nxdomain 3 件 + :993 の timeout 1 件 を 3 回 = 数えるのは 6 件
    let fired = errors_fired(3, |p| {
        p.errors = 6;
        p.errors_by_cause[ErrCause::Dns as usize] = 3;
        p.errors_by_cause[ErrCause::Timeout as usize] = 3;
        p.other_port_attempts = 1;
        p.other_port_errors = 1;
    });
    assert_eq!(fired.len(), 1, "{:?}", fired);
    assert_eq!(
        fired[0].text,
        "errors: 6 errors in 5m (threshold 5; 9 dns not counted; 3 not to 443/80): \
         dns 9, timeout 9"
    );
    assert!(fired[0].text.len() <= crate::events::MAX_TEXT);
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
