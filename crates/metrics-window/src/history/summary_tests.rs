use super::summary::{Params, Summary, burst_limit, of};
use super::*;

/// 1 時間の標本 1 本 (時刻だけ決める)。
fn hour(t: u64) -> Sample {
    Sample {
        t,
        ..Sample::default()
    }
}

/// T14.0 の「平常時」(2026-09-16 の 72.7 時間) と同じ数字になる架空の 72 時間。
///
/// **p50 8.3 / p95 80.7 ms は手で置いた分布から出る値**:
/// 4,000 本を \[2,5\] に 1,670・\[5,10\] に 500・\[25,50\] に 1,323・\[50,100\] に 500・
/// \[100,250\] に 7 本置くと、p50 の順位 2,000 は \[5,10\] の 1,670 本目の次から
/// 330 / 500 = 0.66 の位置 → 5 + 5 × 0.66 = **8.3**、p95 の順位 3,800 は \[50,100\] の
/// 307 / 500 = 0.614 の位置 → 50 + 50 × 0.614 = **80.7**。
/// 名前解決は 1 標本 38 ミス・437 ms (= 11.5 ms/ミス) で、4,000 本に対して 0.551 回/接続。
/// バーストの 14 時間 (1 時間 350 本、500 ms、エラー 99、山 218) は平常時から外れる。
fn t14_0_hours() -> (Vec<Sample>, u64) {
    let t0 = 1_757_000_000 - 1_757_000_000 % 3600;
    let mut normal: Vec<Sample> = (0..58).map(|i| hour(t0 + i * 3600)).collect();
    for s in normal.iter_mut() {
        s.dns_misses = 38;
        s.dns_ms_sum = 437;
        s.active = 8;
        s.active_max = 8;
    }
    let mut idx = 0usize;
    let mut in_this = 0u64;
    for (ms, n) in [(3u64, 1670u64), (8, 500), (30, 1323), (60, 500), (250, 7)] {
        for _ in 0..n {
            normal[idx].connect.observe(ms);
            in_this += 1;
            // 1 標本 69 本まで (平常時の閾 300 のはるか下)
            if in_this == 69 && idx + 1 < normal.len() {
                idx += 1;
                in_this = 0;
            }
        }
    }
    let mut samples = normal;
    for i in 0..14u64 {
        let mut s = hour(t0 + (58 + i) * 3600);
        for _ in 0..350 {
            s.connect.observe(500);
        }
        s.errors = if i == 0 { 99 } else { 0 };
        s.errors_by_cause[0] = s.errors;
        s.dns_misses = 350;
        s.dns_ms_sum = 35_000;
        s.active = 218;
        s.active_max = 218;
        samples.push(s);
    }
    let last = t0 + 71 * 3600;
    (samples, last)
}

fn t14_0_summary(normal_hours_only: bool) -> Summary {
    let (samples, last) = t14_0_hours();
    let h = History::default();
    h.restore(2, samples);
    of(
        &h,
        &Params {
            since: 0,
            until: last,
            res: None,
            normal_hours_only,
        },
    )
}

/// 受け入れ基準: 既知の標本列から p50 / p95 が**手計算と一致**する。
#[test]
fn the_normal_hours_of_t14_0_come_back_with_the_same_p50_and_p95() {
    let s = t14_0_summary(true);
    assert_eq!(
        (s.samples, s.burst_samples),
        (58, 14),
        "平常時 58 / 72 標本"
    );
    assert_eq!(s.interval_secs, 3600, "期間が 1 日を越えるので 1 時間の窓");
    assert_eq!(s.connects(), 4000);
    // 手計算: 5 + 5 × (2000 − 1670)/500 = 8.3、50 + 50 × (3800 − 3493)/500 = 80.7
    assert!(
        (s.connect.quantile_ms(0.5) - 8.3).abs() < 1e-9,
        "p50 {}",
        s.connect.quantile_ms(0.5)
    );
    assert!(
        (s.connect.quantile_ms(0.95) - 80.7).abs() < 1e-9,
        "p95 {}",
        s.connect.quantile_ms(0.95)
    );
    // 名前解決は T14.0 の 0.55 回/接続・ミス 1 回 11.5 ms
    assert!(
        (s.dns_miss_per_connect() - 0.551).abs() < 1e-9,
        "{}",
        s.dns_miss_per_connect()
    );
    assert!(
        (s.dns_miss_avg_ms() - 11.5).abs() < 1e-9,
        "{}",
        s.dns_miss_avg_ms()
    );
    // バーストの時間帯のエラーと山は平常時に入らない (T14.0: 平常時 0、バーストで 99)
    assert_eq!(s.errors, 0);
    assert_eq!(s.active_max, 8);
    let json = s.to_json();
    assert!(json.contains("\"p50_ms\":8.3"), "{}", json);
    assert!(json.contains("\"p95_ms\":80.7"), "{}", json);
    assert!(json.contains("\"dns_miss_per_connect\":0.551"), "{}", json);
    assert!(json.contains("\"dns_miss_avg_ms\":11.5"), "{}", json);
    assert!(json.contains("\"normal_hours_only\":true"), "{}", json);
    // 標本そのものは返さない
    assert!(!json.contains("\"samples\":["), "{}", json);
}

/// 平常時で切らなければバーストが混ざり、**同じ期間でも数字が化ける** (T14.0 の (参考) の行)。
#[test]
fn without_the_normal_hours_filter_the_bursts_change_the_answer() {
    let s = t14_0_summary(false);
    assert_eq!((s.samples, s.burst_samples), (72, 0));
    assert_eq!(s.connects(), 4000 + 14 * 350);
    // 順位 4,450 は [250,500] の 450 / 4,900 の位置 = 250 + 250 × 0.0918
    assert!(
        (s.connect.quantile_ms(0.5) - 272.9591836734694).abs() < 1e-9,
        "p50 {}",
        s.connect.quantile_ms(0.5)
    );
    assert_eq!(s.errors, 99);
    assert_eq!(s.active_max, 218);
    assert!(s.dns_miss_avg_ms() > 70.0, "{}", s.dns_miss_avg_ms());
    assert!(
        s.to_json().contains("\"normal_hours_only\":false"),
        "{}",
        s.to_json()
    );
}

/// 期間は `?res=` があればそれ、無ければ長さで選ぶ (1 時間 → 5 秒、1 日 → 60 秒、それ以上 → 1 時間)。
#[test]
fn the_resolution_follows_the_span_unless_res_is_given() {
    let now = 1_757_000_000;
    let span = |secs: u64| Params {
        since: now - secs,
        until: now,
        res: None,
        normal_hours_only: false,
    };
    assert_eq!(span(300).res_index(), 0);
    assert_eq!(span(3600).res_index(), 0, "直近 1 時間は 5 秒の窓");
    assert_eq!(span(3601).res_index(), 1);
    assert_eq!(span(86_400).res_index(), 1, "1 日は 60 秒の窓");
    assert_eq!(span(86_401).res_index(), 2, "それ以上は 1 時間の窓");
    // `since` を書かなければ残っている全部 = いちばん粗い窓
    assert_eq!(
        Params {
            until: now,
            ..Params::default()
        }
        .res_index(),
        2
    );
    for (secs, want) in [(5u64, 0usize), (60, 1), (3600, 2), (7, 0)] {
        assert_eq!(
            Params {
                res: Some(secs),
                ..span(86_401)
            }
            .res_index(),
            want,
            "res={}",
            secs
        );
    }
    // 平常時の閾は 1 時間 300 本を解像度で割った値 (`snapshot-diff.py` と同じ)
    assert_eq!(burst_limit(3600), 300);
    assert_eq!(burst_limit(60), 5);
    assert_eq!(burst_limit(5), 1);
}

/// 窓は標本の時刻 (`t`) で切る (`since` / `until` はそのまま返す = `since=restart` の確認に使う)。
#[test]
fn the_period_is_cut_at_since_and_until() {
    let h = History::default();
    let t0 = 1_757_000_000;
    let mut samples = Vec::new();
    for i in 0..10u64 {
        let mut s = Sample {
            t: t0 + i * 5,
            ..Sample::default()
        };
        s.connect.observe(10 + i);
        samples.push(s);
    }
    h.restore(0, samples);
    let s = of(
        &h,
        &Params {
            since: t0 + 10,
            until: t0 + 25,
            res: Some(5),
            normal_hours_only: false,
        },
    );
    assert_eq!((s.from, s.to), (t0 + 10, t0 + 25), "要求された期間を返す");
    assert_eq!((s.first_t, s.last_t), (t0 + 10, t0 + 25), "入った標本の端");
    assert_eq!((s.samples, s.connects()), (4, 4));
    // 期間の外の標本は 1 本も入らない
    let none = of(
        &h,
        &Params {
            since: t0 + 1000,
            until: t0 + 2000,
            res: Some(5),
            normal_hours_only: false,
        },
    );
    assert_eq!((none.samples, none.connects()), (0, 0));
    assert!(
        none.to_json().contains("\"p50_ms\":0.0"),
        "{}",
        none.to_json()
    );
}

/// 受け入れ基準: 応答 4 KiB 以下 (1 年動かしたあとの桁で埋めても)。
#[test]
fn the_summary_response_fits_in_4_kib() {
    let h = History::default();
    let mut s = Sample {
        t: 1_757_000_000,
        active: 240,
        active_max: 240,
        errors: 12_345_678,
        errors_by_cause: [
            111_111_111,
            222_222_222,
            333_333_333,
            444_444_444,
            555_555_555,
            666_666_666,
            777_777_777,
            888_888_888,
        ],
        dns_misses: 99_999_999,
        dns_ms_sum: 888_888_888_888,
        ..Sample::default()
    };
    for b in s.connect.buckets.iter_mut() {
        *b = 999_999_999;
    }
    s.connect.count = 12_999_998_987;
    s.connect.ms_sum = 3_333_333_333_333;
    s.connect.ms_max = 30_000;
    s.forward = s.connect;
    h.restore(2, vec![s]);
    let json = of(
        &h,
        &Params {
            since: 0,
            until: u64::MAX,
            res: Some(3600),
            // 平常時で切ると 1 時間 300 本以上のこの標本は外れて 0 が並ぶので、
            // **大きい桁がそのまま載る**方 (切らない) で上限を見る
            normal_hours_only: false,
        },
    )
    .to_json();
    assert!(
        json.len() <= crate::history::summary::MAX_BODY,
        "/history?summary=1 が {} B (4 KiB 超)",
        json.len()
    );
    println!("summary の大きさ: {} B\n{}", json.len(), json);
}
