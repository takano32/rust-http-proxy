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

/// `io::Error` から 8 つの原因へ畳めること (T12.4 (2))。
#[test]
fn io_errors_fold_into_eight_causes() {
    use std::io::{Error, ErrorKind};
    let c = |k: ErrorKind| ErrCause::from_io(&Error::new(k, "x"));
    assert_eq!(c(ErrorKind::ConnectionRefused), ErrCause::Refused);
    assert_eq!(c(ErrorKind::NetworkUnreachable), ErrCause::Unreachable);
    assert_eq!(c(ErrorKind::HostUnreachable), ErrCause::Unreachable);
    assert_eq!(c(ErrorKind::TimedOut), ErrCause::Timeout);
    // `SO_RCVTIMEO` の締め切りは Linux では EAGAIN で返る (T.28.5)
    assert_eq!(c(ErrorKind::WouldBlock), ErrCause::Timeout);
    #[cfg(target_os = "linux")]
    assert_eq!(
        ErrCause::from_io(&Error::from_raw_os_error(11)),
        ErrCause::Timeout
    );
    assert_eq!(c(ErrorKind::ConnectionReset), ErrCause::Reset);
    assert_eq!(c(ErrorKind::BrokenPipe), ErrCause::Reset);
    assert_eq!(c(ErrorKind::NotFound), ErrCause::Dns);
    assert_eq!(c(ErrorKind::PermissionDenied), ErrCause::Other);
    // `getaddrinfo` の失敗は種別が付かないことがあるので文言も見る
    assert_eq!(
        ErrCause::from_io(&Error::other("failed to lookup address information: x")),
        ErrCause::Dns
    );
    assert_eq!(
        ErrCause::from_io(&Error::other("TLS handshake failed")),
        ErrCause::Tls
    );
    assert_eq!(ErrCause::Loop.name(), "loop");
    assert_eq!(ERR_CAUSE_NAMES.len(), ERR_CAUSES);
}

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

/// 利用者が待つ時間の系列 `wait` (T15.0 (2))。
///
/// 4 つの値がそろうのは [`Metrics::record`] だけなので、和を作るのもここ 1 か所。
/// **既存の `connect` の系列は 1 バイトも触らない** (新しい系列として足す)。
#[test]
fn the_wait_series_is_the_sum_of_four_stages() {
    // 段階の窓と同じ旗 (`--lite` では書かない)。旗は処理系で 1 つなので、
    // 旗を見るテスト (T20.4 の 3 本) とは鍵を取って順に回す
    let _flag = PROFILE_FLAG.lock().unwrap_or_else(|e| e.into_inner());
    crate::profile::set_enabled(true);
    let m = Metrics::new();
    m.record_host_detail(
        "connect://w:443",
        HostOutcome::Bypass,
        0,
        Some(Duration::from_millis(30)),
        Detail {
            dns_ms: 10,
            dns_misses: 1,
            // 30 ms のうち 4 ms は窓の内側の名前解決 (残り 6 ms は入口の ACL)
            connect_ms: 26,
            stages: StageMs {
                queue: 5,
                client_read: 2,
                ..StageMs::default()
            },
            ..Detail::default()
        },
    );
    let (c, f, w) = m.recent_quantiles();
    assert_eq!((c.n, w.n), (1, 1), "確立と待ちは同じ本数");
    assert_eq!(f.n, 0, "forward には入らない");
    // 5 + 2 + 10 + 26 = 43 ms (環は us。確立の段は `took` の端数を残す)
    assert_eq!(w.p50_us, 43_000);
    assert!(w.p50_us >= c.p50_us, "待ちが確立より短いことはない");
    // 区間の窓 (`/history` の列を足すのは T15.0 (10))
    let iv = m.take_interval();
    assert_eq!((iv.wait.count, iv.wait.ms_sum, iv.wait.ms_max), (1, 43, 43));
    assert_eq!(iv.connect.count, 1, "確立の窓は今までどおり");
    assert_eq!(iv.connect.ms_max, 30, "確立の値は動かさない");
    assert_eq!(m.take_interval().wait.count, 0, "読むと 0 に戻る");

    // `--lite` では 1 本も書かない (環も窓も増えない)
    crate::profile::set_enabled(false);
    m.record_host_detail(
        "connect://w:443",
        HostOutcome::Bypass,
        0,
        Some(Duration::from_millis(30)),
        Detail {
            stages: StageMs {
                queue: 5,
                ..StageMs::default()
            },
            ..Detail::default()
        },
    );
    assert_eq!(m.recent_quantiles().2.n, 1, "`--lite` で環に書いた");
    assert_eq!(m.take_interval().wait.count, 0, "`--lite` で窓に書いた");
    assert_eq!(
        m.totals().connect.count,
        2,
        "確立の窓は `--lite` でも数える (今までどおり)"
    );
}

/// `profile` の旗 (`--lite`) を切り替えるテストどうしを順に回す鍵。
static PROFILE_FLAG: Mutex<()> = Mutex::new(());

/// 失敗した CONNECT 1 件の形 (1.3 秒かけて `unreachable`。名前解決のミス 1 回つき)。
fn failed_connect(m: &Metrics, host: &str) {
    m.record_host_detail(
        host,
        HostOutcome::Error,
        0,
        Some(Duration::from_millis(1300)),
        Detail {
            dns_ms: 4,
            dns_misses: 1,
            connect_ms: 1296,
            cause: Some(ErrCause::Unreachable),
            stages: StageMs {
                queue: 1,
                client_read: 2,
                ..StageMs::default()
            },
            ..Detail::default()
        },
    );
}

/// **受け入れ基準 (T20.4)**: 失敗した CONNECT は `errors`・`errors_by_cause`・ホスト別の
/// 行には増え、確立の窓 (`connects` / `connect_buckets`)・直近の標本・`wait`・段階の
/// 窓には増えない。
#[test]
fn a_failed_connect_stays_out_of_the_windows_and_the_recent_samples() {
    let _flag = PROFILE_FLAG.lock().unwrap_or_else(|e| e.into_inner());
    crate::profile::set_enabled(true);
    let m = Metrics::new();
    failed_connect(&m, "connect://v6only:443");

    // 残る方: 全体のエラーと原因別、名前解決のミス (どちらも今までどおり)
    let iv = m.totals();
    assert_eq!(iv.errors, 1);
    assert_eq!(iv.errors_by_cause[ErrCause::Unreachable as usize], 1);
    assert_eq!((iv.dns_misses, iv.dns_ms_sum), (1, 4));
    // 残る方: ホスト別の行 (「その相手に掛かった時間」なので失敗の 1.3 秒も入れたまま)
    let (host, s) = &m.hosts_sorted()[0];
    assert_eq!(host, "connect://v6only:443");
    assert_eq!((s.requests, s.errors, s.timed), (1, 1, 1));
    assert_eq!((s.duration_ms_sum, s.duration_ms_max), (1300, 1300));
    assert_eq!(s.errors_by_cause[ErrCause::Unreachable as usize], 1);
    assert_eq!((s.dns_misses, s.dns_ms_sum, s.connect_ms_sum), (1, 4, 1296));

    // 入らない方: 確立の窓 (`/history` の `connects` / `connect_ms_*` / `connect_buckets`)
    assert_eq!(iv.connect, Default::default(), "累計");
    assert_eq!(iv.forward.count, 0);
    // 入らない方: 直近の標本 (`recent_quantiles`) と `wait`
    let (c, f, w) = m.recent_quantiles();
    assert_eq!((c.n, c.total, f.n, w.n, w.total), (0, 0, 0, 0, 0));
    assert_eq!(iv.wait.count, 0, "累計の `wait`");
    let taken = m.take_interval();
    assert_eq!(taken.connect.count, 0, "区間");
    assert_eq!(taken.connect.buckets.iter().sum::<u64>(), 0, "区間の区切り");
    assert_eq!(taken.wait.count, 0, "区間の `wait`");
    assert_eq!(taken.errors, 1, "区間のエラーは増える");
    // 入らない方: 段階の窓 (`/profile`)
    assert!(m.take_stages().is_empty(), "段階の窓に入った");
}

/// **受け入れ基準 (T20.4)**: forward も同じ。エラーで終わった 1 件 (繋がらなかった 502 も、
/// オリジンが 5xx を返した 1 件も `HostOutcome::Error`) は初バイトの窓と標本に入らない。
#[test]
fn a_failed_forward_stays_out_of_the_windows_and_the_recent_samples() {
    let _flag = PROFILE_FLAG.lock().unwrap_or_else(|e| e.into_inner());
    crate::profile::set_enabled(true);
    let m = Metrics::new();
    // 繋がらなかった 502 (初バイトは無い)
    m.record_host_detail(
        "http://dead:80",
        HostOutcome::from_access("BYPASS", 502),
        0,
        Some(Duration::from_millis(1300)),
        Detail {
            connect_ms: 1300,
            cause: Some(ErrCause::Refused),
            ..Detail::default()
        },
    );
    // オリジンが返した 503 (繋がって初バイトも来ているが、分類は `Error`)
    m.record_host_detail(
        "http://dead:80",
        HostOutcome::from_access("MISS", 503),
        120,
        Some(Duration::from_millis(40)),
        Detail {
            first_byte_ms: Some(35),
            ..Detail::default()
        },
    );
    let iv = m.totals();
    assert_eq!(iv.errors, 2);
    assert_eq!(iv.errors_by_cause[ErrCause::Refused as usize], 1);
    assert_eq!(iv.forward, Default::default());
    assert_eq!(iv.connect.count, 0);
    let (c, f, w) = m.recent_quantiles();
    assert_eq!((c.n, f.n, f.total, w.n), (0, 0, 0, 0));
    assert!(m.take_stages().is_empty(), "段階の窓に入った");
    // ホスト別の行には 2 件とも残る
    let (_, s) = &m.hosts_sorted()[0];
    assert_eq!((s.requests, s.errors, s.timed), (2, 2, 2));
    assert_eq!((s.duration_ms_sum, s.duration_ms_max), (1340, 1300));
}

/// **受け入れ基準 (T20.4)**: 成功は今までどおり増える (失敗 3 本のあとの成功 1 本で、
/// 窓と標本に居るのはその 1 本だけ)。403 とキャッシュ HIT の扱いも今までどおり。
#[test]
fn successes_are_counted_as_before_next_to_failures() {
    let _flag = PROFILE_FLAG.lock().unwrap_or_else(|e| e.into_inner());
    crate::profile::set_enabled(true);
    let m = Metrics::new();
    for _ in 0..3 {
        failed_connect(&m, "connect://v6only:443");
    }
    m.record_host_detail(
        "connect://ok:443",
        HostOutcome::Bypass,
        10,
        Some(Duration::from_millis(7)),
        Detail {
            connect_ms: 7,
            ..Detail::default()
        },
    );
    let iv = m.totals();
    assert_eq!((iv.connect.count, iv.connect.ms_sum), (1, 7));
    assert_eq!(iv.connect.ms_max, 7, "失敗の 1.3 秒が最大に出ていない");
    assert_eq!(iv.connect.buckets.iter().sum::<u64>(), 1);
    assert_eq!((iv.wait.count, iv.wait.ms_max), (1, 7));
    assert_eq!(iv.errors, 3);
    assert_eq!(iv.errors_by_cause[ErrCause::Unreachable as usize], 3);
    assert_eq!(iv.dns_misses, 3, "失敗が払った名前解決のミスは残る");
    let (c, _, w) = m.recent_quantiles();
    assert_eq!((c.n, c.max_us, w.n), (1, 7_000, 1));
    // 段階の窓も成功の 1 本だけ (`queue` は 0 ms の 1 本)
    assert_eq!(m.take_stages().folded_connect()[0].count, 1);

    // forward: 成功 (オリジンの 404 も「繋がった」ので入る)・キャッシュ HIT は入り、
    // 403 で断った 1 件 (`blocked://`、時間なし) は今までどおりどの窓にも入らない
    m.record_host_detail(
        "http://b:80",
        HostOutcome::from_access("MISS", 404),
        0,
        Some(Duration::from_millis(900)),
        Detail {
            first_byte_ms: Some(9),
            ..Detail::default()
        },
    );
    m.record_host_timed(
        "http://b:80",
        HostOutcome::from_access("HIT(memory) age=1s", 200),
        5,
        Duration::from_millis(2),
    );
    m.record_host("blocked://ads.example", HostOutcome::Blocked, 0);
    let iv = m.totals();
    assert_eq!((iv.forward.count, iv.forward.ms_max), (2, 9));
    assert_eq!(iv.connect.count, 1);
    assert_eq!(iv.errors, 3, "403 はエラーに数えない");
    assert_eq!(m.recent_quantiles().1.n, 2);
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

/// `/status?sort=` が上位を切り出す鍵だけを変えること (T13.3)。
///
/// 鍵ごとに先頭が入れ替わり、**JSON の形は変わらない** (件数もキーもそのまま)。
#[test]
fn the_sort_key_only_changes_which_hosts_are_cut_out() {
    let m = Metrics::new();
    // 要求は多いが健全なホスト
    for _ in 0..10 {
        m.record_host_detail(
            "connect://busy:443",
            HostOutcome::Bypass,
            0,
            Some(Duration::from_millis(5)),
            Detail::default(),
        );
    }
    // 名前解決に時間を払っているホスト (要求は 2 件)
    for _ in 0..2 {
        m.record_host_detail(
            "connect://slow-dns:443",
            HostOutcome::Bypass,
            0,
            Some(Duration::from_millis(60)),
            Detail {
                dns_ms: 50,
                dns_misses: 1,
                connect_ms: 10,
                ..Detail::default()
            },
        );
    }
    // エラーだけのホスト (要求 1 件)
    m.record_host_detail(
        "connect://broken:443",
        HostOutcome::Error,
        0,
        None,
        Detail {
            cause: Some(ErrCause::Dns),
            dns_misses: 1,
            ..Detail::default()
        },
    );
    // 遠いホスト (1 件だけだが平均が飛び抜けて遅い)
    m.record_host_detail(
        "connect://far-away:443",
        HostOutcome::Bypass,
        0,
        Some(Duration::from_millis(900)),
        Detail {
            connect_ms: 900,
            ..Detail::default()
        },
    );
    let first = |sort| m.hosts_sorted_by(sort)[0].0.clone();
    assert_eq!(first(HostSort::Requests), "connect://busy:443");
    assert_eq!(first(HostSort::Errors), "connect://broken:443");
    assert_eq!(first(HostSort::Dns), "connect://slow-dns:443");
    assert_eq!(first(HostSort::Slow), "connect://far-away:443");
    // 知らない値と空は既定 (要求数順) に倒れる
    for v in ["", "requests", "REQUESTS", "errors?", "なにか"] {
        assert_eq!(
            m.hosts_sorted_by(HostSort::from_param(v))[0].0,
            "connect://busy:443",
            "{}",
            v
        );
    }
    // 同点は要求数 → 名前で崩すので、どの鍵でも順序は 1 つに決まる
    assert_eq!(
        m.hosts_sorted_by(HostSort::Errors),
        m.hosts_sorted_by(HostSort::Errors)
    );
    // JSON の形は鍵で変わらない (件数もキーも同じ。先頭のホストだけが違う)
    let of = |sort| {
        m.to_json_with_cache(
            None,
            StatusExtras {
                sort,
                ..StatusExtras::default()
            },
        )
    };
    let (a, b) = (of(HostSort::Requests), of(HostSort::Errors));
    // 数えるのは `hosts[]` の要素 (`{"host":` で始まる) だけ。`canary` にも
    // `host` の欄がある (T14.10) ので、鍵の名前だけで数えると 1 件多くなる
    assert_eq!(a.matches("{\"host\":").count(), 4);
    assert_eq!(
        a.matches("{\"host\":").count(),
        b.matches("{\"host\":").count()
    );
    assert_eq!(
        a.matches("\"errors_by_cause\":[").count(),
        b.matches("\"errors_by_cause\":[").count()
    );
    assert!(
        a.contains("\"hosts\":[{\"host\":\"connect://busy:443\""),
        "{}",
        a
    );
    assert!(
        b.contains("\"hosts\":[{\"host\":\"connect://broken:443\""),
        "{}",
        b
    );
}

/// `/status` の応答は上位 50 ホスト + 上位 50 接続元でも 64 KiB に収まること (T13.3)。
///
/// 監視が 5 秒ごとに引く口なので、太らせない。長い名前 (RFC の上限に近い 200 バイト) を
/// 並べた最悪に近い形で測る。
#[test]
fn the_status_json_stays_under_64_kib() {
    let m = Metrics::new();
    for i in 0..200 {
        let host = format!("connect://{}{:03}.example.net:443", "n".repeat(180), i);
        m.record_host_detail(
            &host,
            HostOutcome::Error,
            u64::MAX / 2,
            Some(Duration::from_millis(1234)),
            Detail {
                dns_ms: 9999,
                dns_misses: 7,
                connect_ms: 8888,
                family_v6: Some(true),
                cause: Some(ErrCause::Dns),
                first_byte_ms: None,
                ..Detail::default()
            },
        );
        m.record_client(
            &format!("2001:db8:{:04x}:{:04x}::{:04x}", i, i, i),
            HostOutcome::Error,
            u64::MAX / 2,
            // 向き別も桁を振り切らせる (1 行の JSON を最悪にする。T14.26)
            (u64::MAX / 2, u64::MAX / 2),
            Some(Duration::from_millis(1234)),
            Some("connect://very-long-host-name.example.com:443"),
        );
    }
    for sort in [
        HostSort::Requests,
        HostSort::Errors,
        HostSort::Dns,
        HostSort::Slow,
    ] {
        let json = m.to_json_with_cache(
            None,
            StatusExtras {
                sort,
                ..StatusExtras::default()
            },
        );
        assert!(json.len() <= 64 * 1024, "{} バイト", json.len());
    }
}

/// `?hosts=0` の軽い口 (T25.1 段 2): **`hosts` の鍵だけ**が消え、ほかの欄は変わらないこと。
///
/// 既定 (`hosts: true`) は今までどおり `hosts[]` を出すので、その側も一緒に見る。
/// **2 回の組み立てをバイトで比べない**: `uptime_secs` や `/proc` の値は呼ぶたびに動く
/// (この repo の他の試験と同じ作法で、動かない欄だけを見る)。
#[test]
fn the_light_status_drops_only_the_hosts_table() {
    let m = Metrics::new();
    m.record_host_detail(
        "connect://light.example.net:443",
        HostOutcome::Hit,
        100,
        Some(Duration::from_millis(12)),
        Detail::default(),
    );
    m.record_client(
        "10.0.0.1",
        HostOutcome::Hit,
        100,
        (40, 60),
        None,
        Some("connect://light.example.net:443"),
    );

    let full = m.to_json_with_cache(None, StatusExtras::default());
    let light = m.to_json_with_cache(
        None,
        StatusExtras {
            hosts: false,
            ..StatusExtras::default()
        },
    );

    // 既定は今までどおり `hosts[]` を出す
    assert!(
        full.contains("\"hosts\":[{\"host\":\"connect://light.example.net:443\""),
        "{}",
        full
    );
    // 軽い方には `hosts` の鍵が無い (`clients[]` の `host` の欄は別物なので残る)
    assert!(!light.contains("\"hosts\":"), "{}", light);
    // 表の外は同じ: 接続元の行と、通算の始まり (restored_since) が一致する
    assert!(
        light.contains("\"clients\":[{\"client\":\"10.0.0.1\""),
        "{}",
        light
    );
    let at = number_of(&full, "\"restored_since\":");
    assert_ne!(at, 0, "表に 1 行あるので 0 ではない: {}", full);
    assert_eq!(at, number_of(&light, "\"restored_since\":"), "{}", light);
    // 軽い方が小さい
    assert!(
        light.len() < full.len(),
        "light {} B / full {} B",
        light.len(),
        full.len()
    );
    // どの `?sort=` でも軽い方の形は同じ (並べ替える行が無いので鍵だけが違う)
    for sort in [HostSort::Errors, HostSort::Dns, HostSort::Slow] {
        let light_sorted = m.to_json_with_cache(
            None,
            StatusExtras {
                hosts: false,
                sort,
                ..StatusExtras::default()
            },
        );
        assert!(!light_sorted.contains("\"hosts\":"), "{}", light_sorted);
        assert!(
            light_sorted.contains("\"clients\":[{\"client\":\"10.0.0.1\""),
            "{}",
            light_sorted
        );
    }
}

/// `restored_since` の値が無いときは 0 で、`clients[]` の行だけが「表が空ではない」を
/// 語る — **表が空 (`hosts[]` も `clients[]` も空) の軽い口**も壊れないこと。
#[test]
fn the_light_status_of_an_empty_table_is_still_valid() {
    let m = Metrics::new();
    let light = m.to_json_with_cache(
        None,
        StatusExtras {
            hosts: false,
            ..StatusExtras::default()
        },
    );
    assert!(!light.contains("\"hosts\":"), "{}", light);
    assert_eq!(number_of(&light, "\"restored_since\":"), 0, "{}", light);
    // 鍵は `clients` で途切れず、次の鍵へ続く (`,` の付け忘れ・二重をここで見る)
    assert!(light.contains("\"clients\":[],"), "{}", light);
}

/// `"key":` の直後の整数を読む (試験の中だけ。status の JSON は 1 行)。
fn number_of(json: &str, key: &str) -> u64 {
    let at = json
        .find(key)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, json))
        + key.len();
    json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}

/// 接続元の個票 (T14.7): `User-Agent`・宛先の種類・ポート・IP リテラル宛て。
#[test]
fn the_client_card_counts_agents_targets_ports_and_literals() {
    let m = Metrics::new();
    // `User-Agent` は接続の最初の要求で 1 回だけ渡る (前後の空白は落ちる)
    m.record_client_agent("10.0.0.1", " t147/1.0 ");
    for t in [
        "connect://example.com:443",
        "http://example.com:80",
        "192.0.2.7:8443",
    ] {
        m.record_client("10.0.0.1", HostOutcome::Bypass, 1, (0, 1), None, Some(t));
    }
    let v = m.clients_sorted_by(ClientSort::Requests);
    assert_eq!(v[0].0, "10.0.0.1");
    let c = &v[0].1;
    assert_eq!(c.stats.requests, 3);
    assert_eq!(c.agents, vec!["t147/1.0".to_string()]);
    assert_eq!(c.agent(), Some("t147/1.0"));
    assert_eq!(c.agents_dropped, 0);
    // 宛先は example.com と 192.0.2.7 の 2 種 (ポートが違っても同じホスト)
    assert_eq!(c.distinct_targets(), 2);
    assert!(!c.targets_capped);
    assert_eq!(c.literal_targets, 1);
    assert_eq!(c.nonstandard_ports, 1);
    // 同数のポートは番号の小さい順 (順序は 1 つに決まる)
    assert_eq!(c.ports_sorted(), vec![(80, 1), (443, 1), (8443, 1)]);
    assert_eq!(c.ports_other, 0);
    assert!(c.first_seen > 1_700_000_000);
    // 宛先の指紋は大小を同一視する
    m.record_client(
        "10.0.0.1",
        HostOutcome::Bypass,
        0,
        (0, 0),
        None,
        Some("EXAMPLE.COM:443"),
    );
    let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
    assert_eq!(c.distinct_targets(), 2);
    assert_eq!(c.ports_sorted()[0], (443, 2));
    // `/status` の欄と `/clients` の行
    assert!(
        c.status_json().starts_with(",\"first_seen\":"),
        "{}",
        c.status_json()
    );
    let row = c.to_json("10.0.0.1");
    assert!(
        row.starts_with("{\"client\":\"10.0.0.1\",\"requests\":4"),
        "{}",
        row
    );
    assert!(row.contains("\"agent\":\"t147/1.0\""), "{}", row);
    assert!(row.contains("\"agents\":[\"t147/1.0\"]"), "{}", row);
    assert!(
        row.contains("\"ports\":[{\"port\":443,\"requests\":2}"),
        "{}",
        row
    );
}

/// `User-Agent` は 4 種まで覚え、5 種類目からは数えるだけ。長いものは 128 B で切る。
#[test]
fn the_client_card_keeps_four_agents() {
    let m = Metrics::new();
    for i in 0..6 {
        m.record_client_agent("10.0.0.1", &format!("ua/{}", i));
    }
    m.record_client("10.0.0.1", HostOutcome::Bypass, 0, (0, 0), None, None);
    let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
    assert_eq!(c.agents.len(), MAX_CLIENT_AGENTS);
    assert_eq!(c.agents_dropped, 2);
    // 覚えた中で最後に見たものが `/status` に出る 1 つ
    assert_eq!(c.agent(), Some("ua/3"));
    m.record_client_agent("10.0.0.1", "ua/0");
    let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
    assert_eq!(c.agent(), Some("ua/0"));
    assert_eq!(c.agents.len(), MAX_CLIENT_AGENTS, "並びは変えない");

    // 同じ長い `User-Agent` は 1 種のまま (切った形で見比べるため)
    let long = "x".repeat(300);
    let m2 = Metrics::new();
    m2.record_client_agent("10.0.0.2", &long);
    m2.record_client_agent("10.0.0.2", &long);
    m2.record_client("10.0.0.2", HostOutcome::Bypass, 0, (0, 0), None, None);
    let c2 = m2.clients_sorted_by(ClientSort::Requests).remove(0).1;
    assert_eq!(c2.agents.len(), 1);
    assert_eq!(c2.agents_dropped, 0);
    assert!(
        c2.agents[0].len() <= MAX_AGENT_BYTES,
        "{}",
        c2.agents[0].len()
    );
}

/// 宛先は 256 種、ポートは 8 種で頭打ち (あふれた分は旗と `ports_other`)。
#[test]
fn the_client_card_caps_targets_and_ports() {
    let m = Metrics::new();
    for i in 0..(MAX_CLIENT_TARGETS + 10) {
        m.record_client(
            "10.0.0.1",
            HostOutcome::Bypass,
            0,
            (0, 0),
            None,
            Some(&format!("h{}.example:443", i)),
        );
    }
    let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
    assert_eq!(c.distinct_targets(), MAX_CLIENT_TARGETS);
    assert!(c.targets_capped);
    assert_eq!(
        c.ports_sorted(),
        vec![(443, MAX_CLIENT_TARGETS as u64 + 10)]
    );

    for p in 0..10u16 {
        m.record_client(
            "10.0.0.2",
            HostOutcome::Bypass,
            0,
            (0, 0),
            None,
            Some(&format!("a.example:{}", 1000 + p)),
        );
    }
    let c2 = m
        .clients_sorted_by(ClientSort::Requests)
        .into_iter()
        .find(|(k, _)| k == "10.0.0.2")
        .expect("10.0.0.2 の行")
        .1;
    assert_eq!(c2.ports.len(), MAX_CLIENT_PORTS);
    assert_eq!(c2.ports_other, 2);
    assert_eq!(c2.nonstandard_ports, 10);
    assert_eq!(c2.distinct_targets(), 1);
}

/// 接続元ごとの上限で断った数は、全体の合計と個票の両方に残る (T14.13)。
#[test]
fn rejections_are_counted_for_the_total_and_for_the_client() {
    let m = Metrics::new();
    m.record_client_rejected("10.0.0.1");
    m.record_client_rejected("10.0.0.1");
    m.record_client_rejected("10.0.0.2");
    assert_eq!(m.rejected_per_client.load(Ordering::Relaxed), 3);

    // 要求を 1 本も通していない接続元でも `/clients` に出る (断られただけの相手)
    let all = m.clients_sorted_by(ClientSort::Requests);
    let one = all
        .iter()
        .find(|(k, _)| k == "10.0.0.1")
        .expect("10.0.0.1 の行")
        .1
        .to_json("10.0.0.1");
    assert!(one.contains("\"rejected\":2"), "{}", one);
    assert!(m.to_json().contains("\"rejected_per_client\":3"));
}

/// `?sort=` の鍵ごとに先頭が入れ替わること (T14.7)。
#[test]
fn clients_can_be_sorted_by_requests_recent_targets_or_literals() {
    // (1) 要求数と「最後に見た時刻」は向きが逆になるように読み戻す
    let m = Metrics::new();
    m.restore(
        vec![],
        vec![
            (
                "10.0.0.1".to_string(),
                HostStats {
                    requests: 10,
                    last_seen: 100,
                    ..HostStats::default()
                },
            ),
            (
                "10.0.0.2".to_string(),
                HostStats {
                    requests: 5,
                    last_seen: 900,
                    ..HostStats::default()
                },
            ),
        ],
    );
    let first = |s: ClientSort| m.clients_sorted_by(s).remove(0).0;
    assert_eq!(first(ClientSort::Requests), "10.0.0.1");
    assert_eq!(first(ClientSort::Recent), "10.0.0.2");
    // 読み戻した接続元は「いつから居るか」が分からない
    assert_eq!(m.clients_sorted_by(ClientSort::Requests)[0].1.first_seen, 0);

    // (2) 宛先の種類と IP リテラル宛ても向きを逆にする
    let m2 = Metrics::new();
    for i in 0..3 {
        m2.record_client(
            "10.0.0.3",
            HostOutcome::Bypass,
            0,
            (0, 0),
            None,
            Some(&format!("h{}.example:443", i)),
        );
    }
    for _ in 0..5 {
        m2.record_client(
            "10.0.0.4",
            HostOutcome::Bypass,
            0,
            (0, 0),
            None,
            Some("192.0.2.9:443"),
        );
    }
    let first2 = |s: ClientSort| m2.clients_sorted_by(s).remove(0).0;
    assert_eq!(first2(ClientSort::Requests), "10.0.0.4");
    assert_eq!(first2(ClientSort::Targets), "10.0.0.3");
    assert_eq!(first2(ClientSort::Literal), "10.0.0.4");
    // 知らない値は既定に倒す
    assert_eq!(ClientSort::from_param("nonsense"), ClientSort::Requests);
    assert_eq!(ClientSort::from_param("targets"), ClientSort::Targets);
    assert_eq!(ClientSort::from_param("literal").name(), "literal");
}

/// 宛先の鍵は呼び出し側でまちまちなので、ホストとポートの取り出しを 1 か所で見る。
#[test]
fn the_target_key_is_split_the_same_way_everywhere() {
    assert_eq!(
        target_parts("connect://example.com:443"),
        ("example.com", Some(443))
    );
    assert_eq!(
        target_parts("http://example.com:80"),
        ("example.com", Some(80))
    );
    assert_eq!(
        target_parts("http://example.com:80/a/b"),
        ("example.com", Some(80))
    );
    assert_eq!(target_parts("example.com"), ("example.com", None));
    assert_eq!(
        target_parts("[2001:db8::1]:8443"),
        ("2001:db8::1", Some(8443))
    );
    assert_eq!(target_parts("2001:db8::1"), ("2001:db8::1", None));
    // ポートを持たない宛先はポートを数えない (ホストの種類だけ)
    let m = Metrics::new();
    m.record_client(
        "10.0.0.1",
        HostOutcome::Blocked,
        0,
        (0, 0),
        None,
        Some("ads.example"),
    );
    let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
    assert_eq!(c.distinct_targets(), 1);
    assert!(c.ports.is_empty());
    assert_eq!(c.nonstandard_ports, 0);
}

#[test]
fn error_rate_counts_all_requests() {
    let m = Metrics::new();
    m.record_host("http://a:80", HostOutcome::Error, 0);
    m.record_host("http://a:80", HostOutcome::Hit, 0);
    m.record_host("http://a:80", HostOutcome::Hit, 0);
    m.record_host("http://a:80", HostOutcome::Hit, 0);
    let (_, s) = &m.hosts_sorted()[0];
    assert!((s.error_rate() - 0.25).abs() < 1e-9);
    assert_eq!(s.timed, 0);
}

/// T.28.45: 区間に「確立 (`dns + connect`) − オリジン側のカーネルの RTT」の窓と、
/// 名前解決に失敗した試みのミスを積む (`/slo` の判定の元)。
///
/// 見るのは: (a) 確立 6 + 20 = 26 ms・RTT 18.3 ms の CONNECT は 7.7 → **8 ms** (切り上げ)、
/// (b) RTT が RTT より短い確立 (負) は 0、(c) RTT が読めなかった (0) 本と forward と
/// 失敗した本は窓に入らない (`excess.count <= connect.count`)、(d) 原因 `dns` の失敗の
/// ミスだけが `dns_fail_misses` に入り、ほかの原因の失敗のミスは入らない。
#[test]
fn the_interval_keeps_the_rtt_excess_and_the_misses_of_failed_lookups() {
    let m = Metrics::new();
    let ok = |dns_ms, connect_ms, rtt_us| Detail {
        dns_ms,
        dns_misses: 1,
        connect_ms,
        origin_rtt_us: rtt_us,
        ..Detail::default()
    };
    let took = Some(Duration::from_millis(26));
    // (a)
    m.record_host_detail(
        "connect://a:443",
        HostOutcome::Bypass,
        0,
        took,
        ok(6, 20, 18_300),
    );
    // (b)
    m.record_host_detail(
        "connect://a:443",
        HostOutcome::Bypass,
        0,
        took,
        ok(0, 3, 9_000),
    );
    // (c) RTT が読めなかった / forward / 失敗
    m.record_host_detail(
        "connect://a:443",
        HostOutcome::Bypass,
        0,
        took,
        ok(0, 26, 0),
    );
    m.record_host_detail(
        "http://b:80",
        HostOutcome::Miss,
        0,
        took,
        Detail {
            first_byte_ms: Some(7),
            ..ok(0, 26, 18_000)
        },
    );
    // (d) 名前解決の失敗 (ミス 2 回) と、引けたあとの拒否 (ミス 1 回)
    m.record_host_detail(
        "connect://nx:25",
        HostOutcome::Error,
        0,
        Some(Duration::from_millis(40)),
        Detail {
            dns_ms: 40,
            dns_misses: 2,
            cause: Some(ErrCause::Dns),
            ..Detail::default()
        },
    );
    m.record_host_detail(
        "connect://rst:25",
        HostOutcome::Error,
        0,
        Some(Duration::from_millis(40)),
        Detail {
            cause: Some(ErrCause::Refused),
            ..ok(5, 35, 30_000)
        },
    );
    let iv = m.take_interval();
    assert_eq!(iv.connect.count, 3, "確立は今までどおり 3 本");
    assert_eq!(iv.excess.count, 2, "RTT が読めた成功の CONNECT だけ");
    assert_eq!((iv.excess.ms_sum, iv.excess.ms_max), (8, 8), "8 + 0");
    assert_eq!(iv.dns_misses, 1 + 1 + 1 + 1 + 2 + 1);
    assert_eq!(iv.dns_fail_misses, 2, "原因 `dns` の失敗のミスだけ");
    assert_eq!(m.totals().dns_fail_misses, 2, "通算にも入る");
}
