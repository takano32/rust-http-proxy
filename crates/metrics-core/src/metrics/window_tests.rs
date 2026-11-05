//! 区間の合計 (`Interval`) と窓の試験: `wait`・失敗を窓に入れないこと・RTT の差・443/80 以外の試み・
//! 443/80 宛ての名前解決のミス (T.34.33 で `latency_tests.rs` から移した)。

use super::host_table::web_port;
use super::*;

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
    // 失敗した試みが払った ms も別に持つ (T.32.16。`dns_slow` が引く)
    assert_eq!(iv.dns_ms_sum, 6 + 40 + 5);
    assert_eq!(iv.dns_fail_ms_sum, 40, "原因 `dns` の失敗の ms だけ");
}

/// T.30.3: 区間に「443・80 以外の宛先への試み」とそのエラーを積む (`/slo` の `error_rate` が引く)。
///
/// 見るのは: (a) `:443`・`:80` の成功とエラーは入らない、(b) `:993` の時間切れ・`:8080` の転送の成功
/// (`:80` で終わらない)・`:5228` の確立の成功は入る、(c) 原因 `dns` の失敗は試みに数えない (`/slo` が
/// 先に除く)、(d) 窓に入らない成功 (応答時間なし・`blocked://`) は試みに数えない。
#[test]
fn the_interval_counts_the_attempts_to_ports_other_than_443_and_80() {
    let m = Metrics::new();
    let took = Some(Duration::from_millis(5));
    let err = |cause| Detail {
        cause: Some(cause),
        ..Detail::default()
    };
    let ok = Detail::default;
    // (a)
    m.record_host_detail("connect://a:443", HostOutcome::Bypass, 0, took, ok());
    m.record_host_detail("http://b:80", HostOutcome::Miss, 0, took, ok());
    m.record_host_detail(
        "connect://a:443",
        HostOutcome::Error,
        0,
        took,
        err(ErrCause::Refused),
    );
    // (b)
    for _ in 0..3 {
        m.record_host_detail(
            "connect://mail:993",
            HostOutcome::Error,
            0,
            took,
            err(ErrCause::Timeout),
        );
    }
    m.record_host_detail("http://c:8080", HostOutcome::Miss, 0, took, ok());
    m.record_host_detail("connect://[::1]:5228", HostOutcome::Bypass, 0, took, ok());
    // (c)
    m.record_host_detail(
        "connect://nx:995",
        HostOutcome::Error,
        0,
        took,
        err(ErrCause::Dns),
    );
    // (d)
    m.record_host("http://d:8080", HostOutcome::Hit, 0);
    m.record_host("blocked://e", HostOutcome::Blocked, 0);
    let iv = m.take_interval();
    assert_eq!((iv.errors, iv.connect.count, iv.forward.count), (5, 2, 2));
    assert_eq!(
        iv.other_port_attempts,
        3 + 1 + 1,
        "`:993` 3・`:8080` 1・`:5228` 1"
    );
    assert_eq!(iv.other_port_errors, 3, "`:993` の時間切れだけ");
    assert_eq!(m.totals().other_port_attempts, 5, "通算にも入る");
    // 鍵の末尾の比べ方 (解かない)
    for (key, web) in [
        ("connect://a:443", true),
        ("http://b:80", true),
        ("connect://[::1]:443", true),
        ("http://c:8080", false),
        ("http://c:180", false),
        ("connect://d:1443", false),
        ("connect://e:993", false),
        ("loop://f", false),
        ("", false),
    ] {
        assert_eq!(web_port(key), web, "{}", key);
    }
}

/// T.33.3: 区間に「443・80 宛ての、名前解決に成功したミス」の回数と ms を積む (`dns_slow` が読む)。
///
/// 見るのは: (a) `:443` の確立と `:80` の転送 (エラーで終わっても、名前は引けた) は入る、(b) `:993`・`:8080` は
/// 入らない (`dns_misses` には入ったまま)、(c) 原因 `dns` の失敗は 443 宛てでも入らない、(d) ミスの無い要求は
/// 何も足さない、(e) 通算にも入る。
#[test]
fn the_interval_counts_the_resolved_dns_misses_for_443_and_80() {
    let m = Metrics::new();
    let took = Some(Duration::from_millis(5));
    let miss = |n, ms| Detail {
        dns_misses: n,
        dns_ms: ms,
        ..Detail::default()
    };
    // (a)
    m.record_host_detail(
        "connect://a:443",
        HostOutcome::Bypass,
        0,
        took,
        miss(1, 300),
    );
    m.record_host_detail("http://b:80", HostOutcome::Miss, 0, took, miss(2, 120));
    m.record_host_detail(
        "connect://c:443",
        HostOutcome::Error,
        0,
        took,
        Detail {
            cause: Some(ErrCause::Timeout),
            ..miss(1, 50)
        },
    );
    // (b)
    m.record_host_detail(
        "connect://imap:993",
        HostOutcome::Bypass,
        0,
        took,
        miss(1, 400),
    );
    m.record_host_detail("http://d:8080", HostOutcome::Miss, 0, took, miss(1, 70));
    // (c)
    m.record_host_detail(
        "connect://nx:443",
        HostOutcome::Error,
        0,
        took,
        Detail {
            cause: Some(ErrCause::Dns),
            ..miss(2, 2_000)
        },
    );
    // (d)
    m.record_host_detail("connect://a:443", HostOutcome::Bypass, 0, took, miss(0, 0));
    let iv = m.take_interval();
    assert_eq!((iv.dns_misses, iv.dns_ms_sum), (8, 2_940));
    assert_eq!((iv.dns_fail_misses, iv.dns_fail_ms_sum), (2, 2_000));
    assert_eq!(
        (iv.web_dns_misses, iv.web_dns_ms_sum),
        (1 + 2 + 1, 300 + 120 + 50),
        "443・80 宛ての、名前解決に成功したミスだけ"
    );
    // (e)
    let all = m.totals();
    assert_eq!((all.web_dns_misses, all.web_dns_ms_sum), (4, 470));
}
