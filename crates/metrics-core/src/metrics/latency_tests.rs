//! 応答時間の分位点・区間の段・名前解決の失敗の内訳・内訳の行への乗り方の試験
//! (T34.33 で `latency_tests.rs` を話題ごとに割った残り)。

use super::*;

#[test]
fn quantiles_interpolate_within_buckets() {
    let m = Metrics::new();
    for ms in [5, 20, 40, 80, 200, 400, 800, 2000, 4000, 9000] {
        m.record_host_timed(
            "http://a:80",
            HostOutcome::Miss,
            0,
            Duration::from_millis(ms),
        );
    }
    let (_, s) = &m.hosts_sorted()[0];
    assert_eq!(s.timed, 10);
    assert_eq!(s.duration_ms_max, 9000);
    assert!((s.avg_ms() - 1654.5).abs() < 0.01);
    // 24 段では 1 件ずつ別の区間に入る: p50 は 200 ms の入る (140, 210] の上端
    assert!(
        (s.quantile_ms(0.5) - 210.0).abs() < 1e-6,
        "{}",
        s.quantile_ms(0.5)
    );
    let p95 = s.quantile_ms(0.95);
    assert!(p95 > 5000.0 && p95 <= 9000.0, "{}", p95);
    assert_eq!(s.quantile_ms(1.0), 9000.0);
    assert_eq!(HostStats::default().quantile_ms(0.5), 0.0);
}

/// T12.4 (1) の受け入れ基準: **区間が 10 段だった頃は 257 ms が (250, 500] に全部入り、
/// 補間しても観測した最大値で頭打ちになって p50 = p95 = max = 290 になっていた**。
/// 24 段では 257 ms と 290 ms が (210, 315] に入り、区間内の補間が意味を持つ。
#[test]
fn the_deployed_shape_of_257ms_gets_a_real_median() {
    let m = Metrics::new();
    for _ in 0..90 {
        m.record_host_timed(
            "connect://www.dlsite.com:443",
            HostOutcome::Bypass,
            0,
            Duration::from_millis(257),
        );
    }
    for _ in 0..10 {
        m.record_host_timed(
            "connect://www.dlsite.com:443",
            HostOutcome::Bypass,
            0,
            Duration::from_millis(290),
        );
    }
    let (_, s) = &m.hosts_sorted()[0];
    let (p50, p95) = (s.quantile_ms(0.5), s.quantile_ms(0.95));
    assert!((250.0..=265.0).contains(&p50), "p50 {}", p50);
    assert!(p95 >= 280.0, "p95 {}", p95);
    assert!(p50 < p95, "p50 {} p95 {}", p50, p95);
}

/// 同じく T12.4 (1): AAAA の無いホスト (デプロイ先の p50 5.1 ms) が潰れないこと。
#[test]
fn a_five_millisecond_host_keeps_a_five_millisecond_median() {
    let m = Metrics::new();
    for _ in 0..100 {
        m.record_host_timed(
            "connect://discord.com:443",
            HostOutcome::Bypass,
            0,
            Duration::from_millis(5),
        );
    }
    let (_, s) = &m.hosts_sorted()[0];
    let p50 = s.quantile_ms(0.5);
    assert!((4.0..=6.0).contains(&p50), "p50 {}", p50);
}

/// 区間は 1 ms から 10 s まで単調増加で、公比はおよそ 1.5。
#[test]
fn the_bounds_are_a_geometric_ladder() {
    assert_eq!(LATENCY_BOUNDS_MS.len(), 24);
    assert_eq!(LATENCY_BOUNDS_MS[0], 1);
    assert_eq!(LATENCY_BOUNDS_MS[23], 10_000);
    for w in LATENCY_BOUNDS_MS.windows(2) {
        assert!(w[0] < w[1], "{:?}", w);
    }
    // 5 ms より上は公比 1.33〜1.55 に収まっている (下の端はもっと細かい)
    for w in LATENCY_BOUNDS_MS.windows(2).skip(4) {
        let r = w[1] as f64 / w[0] as f64;
        assert!((1.3..=1.6).contains(&r), "{:?} -> {}", w, r);
    }
}

// `io::Error` から 8 つの原因へ畳む試験 (`io_errors_fold_into_eight_causes`) は、判定を持つ
// 下の層 `proxy-metrics-types` の `metrics::causes` へ移した (T34.1)

/// 名前解決の失敗を `nxdomain` / `dns_temp` に割り、`errors_by_cause` の**うしろ**に
/// 足すこと (T25.0)。**先頭 8 つの意味と並びは動かさない** (過去の雪像は 8 要素)。
#[test]
fn dns_failures_are_split_after_the_eight_causes() {
    use std::io::{Error, ErrorKind};
    let nx = Error::other("failed to lookup address information: Name or service not known");
    let temp =
        Error::other("failed to lookup address information: Temporary failure in name resolution");
    // 8 つの原因は今までどおりで、その内側にだけ内訳が付く
    for (e, want) in [(&nx, DnsCause::Nxdomain), (&temp, DnsCause::DnsTemp)] {
        let f = Failure::from_io(e);
        assert_eq!(f.cause, ErrCause::Dns, "8 つの原因は今までどおり");
        assert_eq!(f.dns, Some(want));
        assert_eq!(f.dns.unwrap().name(), want.name());
    }
    // DNS 以外の失敗は内訳を持たない (組み合わせがずれない)
    assert_eq!(
        Failure::from_io(&Error::new(ErrorKind::ConnectionRefused, "x")).dns,
        None
    );
    // 名前が無い / 住所が無いと言われたときだけ `nxdomain`、読めないものは `dns_temp`
    assert_eq!(
        DnsCause::of_text("No address associated with hostname"),
        DnsCause::Nxdomain
    );
    assert_eq!(
        DnsCause::of_text("nodename nor servname provided"),
        DnsCause::Nxdomain
    );
    assert_eq!(
        DnsCause::of_text("Could not resolve host"),
        DnsCause::Nxdomain
    );
    assert_eq!(
        DnsCause::of_text("Temporary failure in name resolution"),
        DnsCause::DnsTemp
    );
    assert_eq!(DnsCause::of_text("something new"), DnsCause::DnsTemp);

    let m = Metrics::new();
    let detail = |f: Failure, ms: u64| Detail {
        cause: Some(f.cause),
        dns_cause: f.dns,
        dns_ms: ms,
        dns_misses: 1,
        ..Detail::default()
    };
    let d1 = detail(Failure::from_io(&nx), 2);
    let d2 = detail(Failure::from_io(&temp), 30);
    m.record_host_detail(
        "connect://mail.example.jp:993",
        HostOutcome::Error,
        0,
        None,
        d1,
    );
    m.record_host_detail(
        "connect://mail.example.jp:993",
        HostOutcome::Error,
        0,
        None,
        d2,
    );
    m.record_error(true, "mail.example.jp:993", "198.51.100.7", 502, &d1);
    m.record_error(true, "mail.example.jp:993", "198.51.100.7", 502, &d2);

    let s = m.hosts_sorted()[0].1.clone();
    assert_eq!(s.errors, 2);
    assert_eq!(
        s.errors_by_cause[ErrCause::Dns as usize],
        2,
        "`dns` は合計のまま"
    );
    assert_eq!(s.dns_by_cause, [1, 1]);
    // 合計と内訳が一致する (`dns == nxdomain + dns_temp`)
    assert_eq!(
        s.errors_by_cause[ErrCause::Dns as usize],
        s.dns_by_cause.iter().sum::<u64>()
    );
    // JSON は 10 要素で、**先頭 8 つは今までと同じ位置**
    let json = stats_json(&s, true);
    assert!(
        json.contains("\"errors_by_cause\":[2,0,0,0,0,0,0,0,1,1]"),
        "{}",
        json
    );
    // `/status` にも同じ並びで出る (`hosts[]` の行)
    let status = m.to_json();
    assert!(
        status.contains("\"errors_by_cause\":[2,0,0,0,0,0,0,0,1,1]"),
        "{}",
        status
    );

    // `/errors` の個票は内訳の名前で出る (合計の `dns` ではない)
    let (rows, total) = m.errors.recent(crate::recent::MAX_ERRORS);
    assert_eq!(total, 2);
    let cause = |kv: &crate::recent::ErrorEntry| kv.cause.name();
    assert_eq!(
        rows[0].cause,
        crate::recent::EntryCause::ErrorDns(DnsCause::DnsTemp)
    );
    assert_eq!(cause(&rows[0]), "dns_temp");
    assert_eq!(cause(&rows[1]), "nxdomain");
    // 状態ファイルの符号 (20・21) でも往復する
    for e in &rows {
        assert_eq!(
            crate::recent::EntryCause::from_code(e.cause.code()),
            e.cause
        );
    }
}

/// 内訳はホスト別の行と区間の合計の両方に乗る (T12.4 (2) / (3))。
#[test]
fn the_breakdown_lands_on_the_host_row_and_the_interval() {
    let m = Metrics::new();
    m.record_host_detail(
        "connect://a:443",
        HostOutcome::Bypass,
        0,
        Some(Duration::from_millis(257)),
        Detail {
            dns_ms: 12,
            dns_misses: 1,
            connect_ms: 245,
            family_v6: Some(false),
            cause: None,
            first_byte_ms: None,
            ..Detail::default()
        },
    );
    m.record_host_detail(
        "connect://a:443",
        HostOutcome::Error,
        0,
        Some(Duration::from_millis(300)),
        Detail {
            family_v6: Some(true),
            cause: Some(ErrCause::Refused),
            ..Detail::default()
        },
    );
    let (host, s) = &m.hosts_sorted()[0];
    assert_eq!(host, "connect://a:443");
    assert_eq!((s.dns_ms_sum, s.dns_misses, s.connect_ms_sum), (12, 1, 245));
    assert_eq!((s.v4_wins, s.v6_wins), (1, 1));
    assert_eq!(s.errors_by_cause[ErrCause::Refused as usize], 1);
    // `connect://` の鍵は CONNECT の窓へ入る (**繋がった 1 本だけ**。失敗した 1 本は
    // `errors` とホスト別の行に残り、確立の窓には入らない。T20.4)
    let iv = m.totals();
    assert_eq!(iv.connect.count, 1);
    assert_eq!(iv.forward.count, 0);
    assert_eq!(iv.errors, 1);
    assert_eq!(iv.errors_by_cause[ErrCause::Refused as usize], 1);
    assert_eq!((iv.dns_misses, iv.dns_ms_sum), (1, 12));
    // 区間は読むと 0 に戻る
    assert_eq!(m.take_interval().connect.count, 1);
    assert_eq!(m.take_interval().connect.count, 0);
    assert_eq!(m.totals().connect.count, 1, "累計は残る");
    // forward は初バイトの値が窓に入る (応答全体の時間ではない)
    m.record_host_detail(
        "http://b:80",
        HostOutcome::Miss,
        0,
        Some(Duration::from_millis(900)),
        Detail {
            first_byte_ms: Some(7),
            ..Detail::default()
        },
    );
    let iv = m.take_interval();
    assert_eq!(iv.forward.count, 1);
    assert_eq!(iv.forward.ms_max, 7);
    // ホスト別の応答時間はこれまでどおり応答全体
    let b = m
        .hosts_sorted()
        .into_iter()
        .find(|(h, _)| h == "http://b:80")
        .unwrap()
        .1;
    assert_eq!(b.duration_ms_max, 900);
    // `/status` にはホスト別だけ内訳が出る (接続元別には出ない)
    let json = m.to_json();
    assert!(json.contains("\"v6_wins\":1"), "{}", json);
    assert!(json.contains("\"errors_by_cause\":["), "{}", json);
}
