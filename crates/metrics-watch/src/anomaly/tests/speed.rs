//! (1) `connect_p95` の試験 (確立の「確立 − RTT」の p95)。

use super::*;

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
        "connect_p95: connect-rtt p95 136 ms over 5m is 13.9x the 1h baseline 9.8 ms (122 of 1442 connects)"
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

/// **T29.11**: `connect_p95` は「確立 − RTT」(名前解決を除く) で読む。遠い宛先の割合が動いて確立そのものの
/// p95 が上がっても (0.28.48 の 00:25 の窓: 確立 p95 122 ms・`connect − RTT` の p95 1 ms) 立たず、
/// RTT を引いても 100 ms 遅い確立が 20 本あれば立つ。基準値の p95 が 0 ms (全部 RTT ちょうど) でも立てる。
#[test]
fn connect_p95_reads_the_excess_over_rtt() {
    // (a) 遠い宛先 (RTT 250 ms) の確立が増えただけ = 確立は 252 ms でも RTT を引けば 2 ms
    let h = History::default();
    let mut d = Detector::new();
    let mut t = 0;
    while t < BASE_SECS {
        assert!(feed(&h, &mut d, connects_far(t, 2, 12, 10)).is_empty());
        t += 5;
    }
    let mut fired = Vec::new();
    while t < BASE_SECS + 600 {
        fired.extend(feed(&h, &mut d, connects_far(t, 2, 252, 250)));
        t += 5;
    }
    assert!(fired.is_empty(), "距離で立ってはいけない: {:?}", fired);
    // (b) RTT を引いても 100 ms 遅い確立が 5 分に 20 本 → 立つ (基準値の p95 は 0〜1 ms)
    let h = History::default();
    let mut d = Detector::new();
    let mut t = 0;
    while t < BASE_SECS {
        assert!(feed(&h, &mut d, connects_far(t, 2, 10, 10)).is_empty());
        t += 5;
    }
    // 5 分ぶん静かにして、直近の窓から確立を抜く
    while t < BASE_SECS + WINDOW_SECS + 5 {
        assert!(feed(&h, &mut d, point(t)).is_empty());
        t += 5;
    }
    let mut p = connects_far(t, 19, 110, 10);
    p.t = t;
    assert!(feed(&h, &mut d, p).is_empty(), "19 本では立たない");
    t += 5;
    let fired = feed(&h, &mut d, connects_far(t, 1, 110, 10));
    assert_eq!(fired.len(), 1, "{:?}", fired);
    assert_eq!(fired[0].kind, Kind::ConnectP95);
    assert!(
        fired[0]
            .text
            .starts_with("connect_p95: connect-rtt p95 98 ms over 5m"),
        "{}",
        fired[0].text
    );
}
