use super::probe::split_addrs;
use super::*;
use crate::metrics::{Detail, HostOutcome};
use std::net::{SocketAddr, TcpListener};

/// この単体テストたちは**同じプロセスの静的な状態**を触るので直列にする。
static SERIAL: Mutex<()> = Mutex::new(());

/// canary をクレートの上へ上げても `/status` の `canary` が 1 バイトも変わらないこと
/// (T15.12)。
///
/// 下の層 (`proxy-metrics-core`) は canary を預かっていないとき
/// [`crate::metrics::CANARY_STATUS_UNSET`] を出す。**それが `off` の
/// [`status_json`] と 1 バイト同じ**でなければ、預け忘れたときに `/status` の
/// `canary` が静かに変わってしまう。
#[test]
fn the_default_canary_status_is_byte_identical_to_off() {
    let _s = SERIAL.locked();
    reset();
    *MODE.locked() = Mode::Off;
    assert_eq!(status_json(), crate::metrics::CANARY_STATUS_UNSET);
    // 預けた口は下の層でも同じ文字列を組む (`configure` が預ける)
    configure("off", Duration::from_secs(SECS), true);
    assert_eq!(status_json(), crate::metrics::CANARY_STATUS_UNSET);
    reset();
}

#[test]
fn the_setting_is_read_as_auto_off_or_a_list_of_hosts() {
    assert_eq!(Mode::parse("auto"), Mode::Auto);
    assert_eq!(Mode::parse(" AUTO "), Mode::Auto);
    assert_eq!(Mode::parse(""), Mode::Auto);
    assert_eq!(Mode::parse("off"), Mode::Off);
    assert_eq!(Mode::parse("OFF"), Mode::Off);
    // ポートを省いたら 443、書いてあればそのまま。大小は潰す
    assert_eq!(
        Mode::parse("Example.com, b.example.net:8443"),
        Mode::Hosts(vec![
            "example.com:443".to_string(),
            "b.example.net:8443".to_string()
        ])
    );
    // 並べられるのは MAX_HOSTS まで
    let many = (0..20)
        .map(|i| format!("h{}.example.net", i))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        match Mode::parse(&many) {
            Mode::Hosts(h) => h.len(),
            _ => 0,
        },
        MAX_HOSTS
    );
    // 区切りだけ書いたら回さない
    assert_eq!(Mode::parse(",, ,"), Mode::Off);
}

#[test]
fn auto_picks_the_busiest_connect_target_seen_in_the_last_hour() {
    let _s = SERIAL.locked();
    let m = Metrics::new();
    // forward の宛先は選ばない (canary は CONNECT の確立を測るもの)
    m.record_host("http://a.example.net:80", HostOutcome::Miss, 10);
    m.record_host("http://a.example.net:80", HostOutcome::Miss, 10);
    m.record_host("http://a.example.net:80", HostOutcome::Miss, 10);
    m.record_host_detail(
        "connect://b.example.net:443",
        HostOutcome::Bypass,
        1,
        None,
        Detail::default(),
    );
    m.record_host_detail(
        "connect://c.example.net:443",
        HostOutcome::Bypass,
        1,
        None,
        Detail::default(),
    );
    m.record_host_detail(
        "connect://c.example.net:443",
        HostOutcome::Bypass,
        1,
        None,
        Detail::default(),
    );
    assert_eq!(
        auto_target(&m).as_deref(),
        Some("c.example.net:443"),
        "要求の多い CONNECT の宛先が選ばれる"
    );
    // 何も通っていないプロキシでは選ばない (= 何もしない)
    assert_eq!(auto_target(&Metrics::new()), None);
}

#[test]
fn a_probe_measures_a_real_connect_and_a_refused_one() {
    let _s = SERIAL.locked();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ok = probe(&format!("127.0.0.1:{}", port));
    assert_eq!(ok.error, None, "{:?}", ok);
    assert_eq!(ok.host, format!("127.0.0.1:{}", port));
    assert!(ok.at > 1_700_000_000, "{:?}", ok);
    // IP リテラルは名前解決が要らない (表も通らない) ので、かかっても数 ms。
    // **`== 0` では見ない** (T18.1): 測っているのは壁時計で、混んだ機械ではスレッドが
    // 1 回止められただけで 0.5 ms を超えて 1 に丸め上がり、稀に落ちた。1 回の値は
    // 止められた時間をそのまま含むので、**3 回測った最小**が 2 ms 以下かで見る
    let dns_ms = [ok.dns_ms]
        .into_iter()
        .chain((0..2).map(|_| probe(&format!("127.0.0.1:{}", port)).dns_ms))
        .min()
        .unwrap();
    assert!(
        dns_ms <= 2,
        "IP リテラルの名前解決: {} ms ({:?})",
        dns_ms,
        ok
    );

    drop(listener);
    let dead = probe(&format!("127.0.0.1:{}", port));
    assert!(dead.error.is_some(), "{:?}", dead);
}

#[test]
fn the_windows_keep_one_row_per_slot_and_the_json_has_its_own_array() {
    let _s = SERIAL.locked();
    reset();
    let sample = |at: u64, dns: u64, conn: u64| Probe {
        at,
        host: "a.example.net:443".to_string(),
        dns_ms: dns,
        connect_ms: conn,
        // 5 列目 (T14.37)。偶数の周だけ IPv6 側が繋がった、という形にする
        ipv6_connect_ms: at.is_multiple_of(2).then_some(conn + 1),
        error: None,
        cause: None,
    };
    // 同じ 5 秒の窓に 2 回入ったら新しい方だけが残り、1 分の窓もひとつ
    push_row(&sample(1_000_000, 3, 8));
    push_row(&sample(1_000_002, 4, 9));
    // 次の 5 秒の窓 (1 分の窓は同じ)
    push_row(&sample(1_000_007, 5, 10));
    let mut fine = String::new();
    push_history_json(&mut fine, 0);
    assert!(
        fine.starts_with(",\"canary\":{\"keys\":[\"t\",\"canary_dns_ms\",\"canary_connect_ms\",\"canary_host\",\"canary_ipv6_connect_ms\"],\"samples\":[["),
        "{}",
        fine
    );
    assert!(
        fine.contains("[1000000,4,9,\"a.example.net:443\",10],[1000005,5,10,"),
        "5 秒の窓は 2 行で、同じ窓は上書き: {}",
        fine
    );
    // 繋がらなかった IPv6 側は `null` (T14.37)
    assert!(
        fine.ends_with("[1000005,5,10,\"a.example.net:443\",null]]}"),
        "{}",
        fine
    );
    let mut minute = String::new();
    push_history_json(&mut minute, 1);
    assert_eq!(
        minute.matches("a.example.net").count(),
        1,
        "1 分の窓は 1 行: {}",
        minute
    );
    assert!(
        minute.contains("[999960,5,10,\"a.example.net:443\",null]"),
        "{}",
        minute
    );
    // 1 時間の解像度は窓を持たない (空の配列)
    let mut hour = String::new();
    push_history_json(&mut hour, 2);
    assert!(hour.ends_with("\"samples\":[]}"), "{}", hour);
    reset();
}

#[test]
fn the_status_json_has_the_last_result_and_a_failure_lands_in_the_error_ring() {
    let _s = SERIAL.locked();
    reset();
    let m = Metrics::new();
    // まだ 1 回も回っていない
    let empty = status_json();
    assert!(empty.contains("\"mode\":\"auto\""), "{}", empty);
    assert!(empty.contains("\"runs\":0"), "{}", empty);
    assert!(empty.contains("\"at\":0"), "{}", empty);

    record(
        &Probe {
            at: 1_789_000_000,
            host: "a.example.net:443".to_string(),
            dns_ms: 7,
            connect_ms: 9,
            ipv6_connect_ms: Some(11),
            error: None,
            cause: None,
        },
        &m,
    );
    let json = status_json();
    assert!(json.contains("\"runs\":1"), "{}", json);
    assert!(json.contains("\"failures\":0"), "{}", json);
    assert!(json.contains("\"host\":\"a.example.net:443\""), "{}", json);
    assert!(
        json.contains("\"dns_ms\":7,\"connect_ms\":9,\"ipv6_connect_ms\":11"),
        "{}",
        json
    );
    assert!(json.contains("\"error\":null"), "{}", json);
    assert!(m.errors.is_empty(), "成功は個票に残さない");

    record(
        &Probe {
            at: 1_789_000_060,
            host: "a.example.net:443".to_string(),
            dns_ms: 12,
            connect_ms: 0,
            ipv6_connect_ms: None,
            error: Some("Connection refused (os error 111)".to_string()),
            cause: Some(ErrCause::Refused),
        },
        &m,
    );
    let json = status_json();
    assert!(json.contains("\"failures\":1"), "{}", json);
    assert!(json.contains("\"error\":\"Connection"), "{}", json);
    // 繋がらなかった IPv6 側は `null` (T14.37)
    assert!(json.contains("\"ipv6_connect_ms\":null"), "{}", json);
    let (entries, total) = m.errors.recent(10);
    assert_eq!(total, 1);
    assert_eq!(entries.len(), 1);
    let one = entries[0].to_json();
    assert!(one.contains("\"kind\":\"canary\""), "{}", one);
    assert!(one.contains("\"target\":\"a.example.net:443\""), "{}", one);
    assert!(one.contains("\"dns_ms\":12"), "{}", one);
    // 集計 (`/status` の errors_by_cause) には足さない
    assert_eq!(m.totals().errors_by_cause.iter().sum::<u64>(), 0);
    assert!(one.contains("\"cause\":\"refused\""), "{}", one);
    reset();
}

#[test]
fn off_means_no_target_at_all() {
    let _s = SERIAL.locked();
    reset();
    let m = Metrics::new();
    m.record_host_detail(
        "connect://a.example.net:443",
        HostOutcome::Bypass,
        1,
        None,
        Detail::default(),
    );
    configure("off", Duration::from_secs(1), true);
    assert!(targets(&m).is_empty());
    configure("auto", Duration::from_secs(1), true);
    assert_eq!(targets(&m), vec!["a.example.net:443".to_string()]);
    configure(
        "x.example.net,y.example.net:8443",
        Duration::from_secs(1),
        true,
    );
    assert_eq!(
        targets(&m),
        vec![
            "x.example.net:443".to_string(),
            "y.example.net:8443".to_string()
        ]
    );
    // 周期は最小 1 秒。IPv6 側は `PROXY_CANARY_IPV6` でその場で止まる (T14.37)
    configure("off", Duration::from_secs(0), false);
    assert_eq!(period(), Duration::from_secs(1));
    assert!(!ipv6_enabled(), "off なら IPv6 側は測らない");
    reset();
}

/// T14.37: canary の IPv6 側は **AAAA が有るときだけ** 1 本繋いで ms を出し、
/// 繋がらなければ (AAAA が無い / 断られた / `PROXY_CANARY_IPV6=off`) `null`。
#[test]
fn the_ipv6_side_is_one_extra_connection_and_null_when_it_cannot_connect() {
    let _s = SERIAL.locked();
    reset();
    // (1) AAAA があって生きていれば ms が出る
    let Ok(v6) = TcpListener::bind("[::1]:0") else {
        eprintln!("飛ばした: この機械の lo は ::1 を持たない (IPv6 の無い sandbox。T.32.24)");
        return;
    };
    let v6_port = v6.local_addr().unwrap().port();
    let got = probe(&format!("[::1]:{}", v6_port));
    assert!(got.error.is_none(), "{:?}", got);
    assert!(got.ipv6_connect_ms.is_some(), "生きていれば ms: {:?}", got);

    // (2) AAAA が無い名前 (A だけ) は `null` — 余計な 1 本も出さない
    let v4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let v4_port = v4.local_addr().unwrap().port();
    let got = probe(&format!("127.0.0.1:{}", v4_port));
    assert!(got.error.is_none(), "{:?}", got);
    assert_eq!(got.ipv6_connect_ms, None, "A だけの宛先: {:?}", got);

    // (3) AAAA はあるが繋がらない (閉じたポート = 黒穴と同じ「失敗」) なら `null`
    drop(v6);
    let got = probe(&format!("[::1]:{}", v6_port));
    assert_eq!(got.ipv6_connect_ms, None, "繋がらなければ null: {:?}", got);

    // (4) `PROXY_CANARY_IPV6=off` なら生きていても測らない
    let v6 = TcpListener::bind("[::1]:0").unwrap();
    let v6_port = v6.local_addr().unwrap().port();
    configure("auto", Duration::from_secs(60), false);
    let got = probe(&format!("[::1]:{}", v6_port));
    assert!(got.error.is_none(), "{:?}", got);
    assert_eq!(got.ipv6_connect_ms, None, "off: {:?}", got);
    reset();
}

/// `/status` の `ipv6` から数の欄を 1 つ読む (T18.1 のテスト用)。
fn ipv6_field(key: &str) -> u64 {
    let json = crate::net::ipv6_status_json();
    let pat = format!("\"{}\":", key);
    let rest = &json[json
        .find(&pat)
        .unwrap_or_else(|| panic!("{}: {}", key, json))
        + pat.len()..];
    rest[..rest.find([',', '}']).unwrap()].parse().unwrap()
}

/// T18.1: IPv6 側を**実際に試した**周は `ipv6_runs` が増える (繋がっても繋がらなくても)。
/// その周は下の層の印が新しくなるので、利用者の経路の探り (`ipv6.request_probes`) は増えない。
#[test]
fn a_round_that_tries_ipv6_counts_ipv6_runs_and_no_request_probe() {
    let _s = SERIAL.locked();
    let _ipv6 = crate::net::IPV6_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    reset();
    let probes = ipv6_field("request_probes");
    // 繋がる 1 本
    let Ok(v6) = TcpListener::bind("[::1]:0") else {
        eprintln!("飛ばした: この機械の lo は ::1 を持たない (IPv6 の無い sandbox。T.32.24)");
        return;
    };
    let v6_port = v6.local_addr().unwrap().port();
    let got = probe(&format!("[::1]:{}", v6_port));
    assert!(got.ipv6_connect_ms.is_some(), "{:?}", got);
    assert!(
        status_json().ends_with(",\"error\":null,\"ipv6_runs\":1,\"ipv6_skipped\":0}"),
        "まだ `record` していないので最後の結果は空: {}",
        status_json()
    );
    // 繋がらない 1 本 (閉じたポート = 黒穴と同じ「失敗」) も「試した」に数える。
    // `ipv6_connect_ms` はどちらの「繋がらない」でも `null` で、ここでしか見分けられない
    drop(v6);
    let got = probe(&format!("[::1]:{}", v6_port));
    assert_eq!(got.ipv6_connect_ms, None, "{:?}", got);
    assert_eq!(
        (
            IPV6_RUNS.load(Ordering::Relaxed),
            IPV6_SKIPPED.load(Ordering::Relaxed)
        ),
        (2, 0)
    );
    // 試した周は印が新しい = 探りは canary のもの。利用者の経路の探りは数えられていない
    let status = crate::net::ipv6_status_json();
    assert!(status.contains("\"probe_by\":\"canary\""), "{}", status);
    assert_eq!(ipv6_field("request_probes"), probes, "{}", status);
    reset();
}

/// T18.1: IPv6 側を**試さなかった**周は `ipv6_skipped` が増え、`ipv6_runs` は増えない
/// (AAAA の無い宛先、`PROXY_CANARY_IPV6=off`、名前が引けない)。
#[test]
fn a_round_without_aaaa_counts_ipv6_skipped() {
    let _s = SERIAL.locked();
    reset();
    // (1) AAAA の無い宛先 (A だけ)
    let v4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let v4_port = v4.local_addr().unwrap().port();
    let got = probe(&format!("127.0.0.1:{}", v4_port));
    assert!(got.error.is_none(), "{:?}", got);
    assert_eq!(got.ipv6_connect_ms, None, "{:?}", got);
    let count = || {
        (
            IPV6_RUNS.load(Ordering::Relaxed),
            IPV6_SKIPPED.load(Ordering::Relaxed),
        )
    };
    assert_eq!(count(), (0, 1), "AAAA の無い周");

    // (2) AAAA があって生きていても `PROXY_CANARY_IPV6=off` なら試さない
    let Ok(v6) = TcpListener::bind("[::1]:0") else {
        eprintln!("飛ばした: この機械の lo は ::1 を持たない (IPv6 の無い sandbox。T.32.24)");
        return;
    };
    let v6_port = v6.local_addr().unwrap().port();
    configure("auto", Duration::from_secs(SECS), false);
    let got = probe(&format!("[::1]:{}", v6_port));
    assert!(got.error.is_none(), "{:?}", got);
    assert_eq!(count(), (0, 2), "PROXY_CANARY_IPV6=off の周");
    configure("auto", Duration::from_secs(SECS), true);

    // (3) 名前が引けなかった周 (ホストが空 = リゾルバへ出ずに失敗する) も試さない
    let got = probe(":443");
    assert!(got.error.is_some(), "{:?}", got);
    assert_eq!(count(), (0, 3), "名前が引けない周");

    // `/status` の `canary` の末尾に出る (鍵の順は今までのまま)
    record(&got, &Metrics::new());
    let json = status_json();
    assert!(
        json.starts_with("{\"mode\":\"auto\",\"secs\":60,\"runs\":1,\"failures\":1,\"at\":"),
        "{}",
        json
    );
    assert!(
        json.ends_with(",\"ipv6_runs\":0,\"ipv6_skipped\":3}"),
        "{}",
        json
    );
    reset();
}

/// T17.7: canary の IPv6 側の 1 本は、試したことを下の層に渡す
/// (`/status` の `ipv6.probe_by` が `"canary"` になり、繋がれば `v4_first` が解ける)。
/// **勝敗の数 (`attempts` / `wins` / `losses`) は 1 つも動かさない**。
#[test]
fn the_canary_ipv6_side_takes_over_the_ipv6_probe() {
    let _s = SERIAL.locked();
    let _ipv6 = crate::net::IPV6_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    reset();
    let before = crate::net::ipv6_counters();
    let Ok(v6) = TcpListener::bind("[::1]:0") else {
        eprintln!("飛ばした: この機械の lo は ::1 を持たない (IPv6 の無い sandbox。T.32.24)");
        return;
    };
    let v6_port = v6.local_addr().unwrap().port();
    let got = probe(&format!("[::1]:{}", v6_port));
    assert!(got.ipv6_connect_ms.is_some(), "{:?}", got);
    let status = crate::net::ipv6_status_json();
    assert!(status.contains("\"probe_by\":\"canary\""), "{}", status);
    assert!(!crate::net::ipv6_v4_first(), "{}", status);
    // canary 本体の 1 本は IP リテラル 1 つ (Happy Eyeballs を通らない) なので、
    // 勝敗の数が動いたら IPv6 側の 1 本が数えたことになる
    assert_eq!(crate::net::ipv6_counters(), before, "{}", status);
    reset();
}

/// 引けた答えの分け方 (T.34.45): AAAA の先頭は族で絞る前に控え、IPv6 を切ってあれば A だけ試す。
#[test]
fn split_addrs_keeps_the_first_aaaa_and_filters_by_family() {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let a1 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    let a2 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));
    let b1 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
    let b2 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2));
    let sa = |ip: IpAddr| SocketAddr::new(ip, 443);
    // AAAA が無い: IPv6 側の相手は無く、A は答えの順のまま
    assert_eq!(
        split_addrs(vec![a1, a2], 443, true),
        (None, vec![sa(a1), sa(a2)])
    );
    // v6 の先頭 (答えの中で最初の AAAA) を控え、本体は答えの順のまま全部
    assert_eq!(
        split_addrs(vec![a1, b2, b1, a2], 443, true),
        (Some(sa(b2)), vec![sa(a1), sa(b2), sa(b1), sa(a2)])
    );
    // `ipv6 = false`: 本体は A だけ。IPv6 側の相手は控えたまま (試すかは `probe_ipv6` が決める)
    assert_eq!(
        split_addrs(vec![b1, a1], 443, false),
        (Some(sa(b1)), vec![sa(a1)])
    );
    // AAAA しか無い名前を `ipv6 = false` で引いたら本体は空
    assert_eq!(split_addrs(vec![b1], 443, false), (Some(sa(b1)), vec![]));
}

/// `auto` は直近 3,600 秒の CONNECT の宛先だけを選ぶ (時刻を注入する。T.34.45)。
#[test]
fn auto_target_at_skips_targets_older_than_an_hour_and_non_connect_hosts() {
    let m = Metrics::new();
    // forward の宛先は要求が多くても選ばない
    for _ in 0..3 {
        m.record_host("http://a.example.net:80", HostOutcome::Miss, 10);
    }
    m.record_host_detail(
        "connect://b.example.net:443",
        HostOutcome::Bypass,
        1,
        None,
        Detail::default(),
    );
    let seen = m
        .hosts_sorted_by(crate::metrics::HostSort::Requests)
        .into_iter()
        .find(|(h, _)| h.starts_with("connect://"))
        .map(|(_, s)| s.last_seen)
        .unwrap();
    assert_eq!(
        auto_target_at(&m, seen + AUTO_WINDOW_SECS).as_deref(),
        Some("b.example.net:443"),
        "ちょうど 3,600 秒前までは選ぶ"
    );
    assert_eq!(
        auto_target_at(&m, seen + AUTO_WINDOW_SECS + 1),
        None,
        "3,600 秒より古い宛先は選ばない (forward の宛先にも落ちない)"
    );
}

/// `/status` の `canary` の 1 回ぶん (T.34.45): 成功は `error` が `null`、失敗は逃がした文字列。
#[test]
fn push_json_writes_null_or_the_escaped_error() {
    let ok = Probe {
        at: 1_789_000_000,
        host: "a.example.net:443".to_string(),
        dns_ms: 7,
        connect_ms: 9,
        ipv6_connect_ms: Some(11),
        error: None,
        cause: None,
    };
    let mut out = String::new();
    ok.push_json(&mut out);
    assert_eq!(
        out,
        "\"at\":1789000000,\"host\":\"a.example.net:443\",\"dns_ms\":7,\"connect_ms\":9,\"ipv6_connect_ms\":11,\"error\":null"
    );
    let failed = Probe {
        ipv6_connect_ms: None,
        error: Some("bad \"quote\"".to_string()),
        cause: Some(ErrCause::Refused),
        ..ok
    };
    let mut out = String::new();
    failed.push_json(&mut out);
    assert_eq!(
        out,
        "\"at\":1789000000,\"host\":\"a.example.net:443\",\"dns_ms\":7,\"connect_ms\":9,\"ipv6_connect_ms\":null,\"error\":\"bad \\\"quote\\\"\""
    );
}
