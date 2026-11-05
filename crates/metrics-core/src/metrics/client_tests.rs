//! 接続元の個票 (`/clients`) の試験 (T34.33 で `latency_tests.rs` から移した)。

use super::*;

/// 要求 1 本で壁時計を読むのは `Metrics::record` の 1 回だけ (T34.146)。接続元の個票は
/// ホスト別の記録が返した時刻をそのまま使い、自分では読み直さない
/// (今までは `ClientStats::count` がもう 1 回読んでいた)。
#[test]
fn the_client_row_takes_the_clock_the_host_row_read() {
    let m = Metrics::new();
    // 渡した時刻がそのまま `last_seen` になる (時計を読み直すなら今の時刻になる)
    m.record_client_at(
        "10.0.0.1",
        HostOutcome::Bypass,
        1,
        (0, 1),
        None,
        Some("http://example.com:80"),
        1_000,
    );
    let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
    assert_eq!(c.stats().last_seen, 1_000);
    assert_eq!(c.stats().requests, 1);
    // ホスト別の記録は読んだ時刻を返し、それがホストの行と接続元の行の両方に入る
    let now = m.record_host_detail(
        "http://example.com:80",
        HostOutcome::Bypass,
        1,
        None,
        Detail::default(),
    );
    assert!(now > 1_700_000_000);
    m.record_client_at(
        "10.0.0.1",
        HostOutcome::Bypass,
        1,
        (0, 1),
        None,
        Some("http://example.com:80"),
        now,
    );
    let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
    assert_eq!(c.stats().last_seen, now);
    let hosts = m.hosts_sorted();
    let host = hosts
        .iter()
        .find(|(k, _)| k == "http://example.com:80")
        .expect("ホストの行");
    assert_eq!(host.1.last_seen, now);
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
    assert_eq!(c.stats().requests, 3);
    assert_eq!(c.agents(), vec!["t147/1.0".to_string()]);
    assert_eq!(c.agent(), Some("t147/1.0"));
    assert_eq!(c.agents_dropped(), 0);
    // 宛先は example.com と 192.0.2.7 の 2 種 (ポートが違っても同じホスト)
    assert_eq!(c.distinct_targets(), 2);
    assert!(!c.targets_capped());
    assert_eq!(c.literal_targets(), 1);
    assert_eq!(c.nonstandard_ports(), 1);
    // 同数のポートは番号の小さい順 (順序は 1 つに決まる)
    assert_eq!(c.ports_sorted(), vec![(80, 1), (443, 1), (8443, 1)]);
    assert_eq!(c.ports_other(), 0);
    assert!(c.first_seen() > 1_700_000_000);
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
    assert_eq!(c.agents().len(), MAX_CLIENT_AGENTS);
    assert_eq!(c.agents_dropped(), 2);
    // 覚えた中で最後に見たものが `/status` に出る 1 つ
    assert_eq!(c.agent(), Some("ua/3"));
    m.record_client_agent("10.0.0.1", "ua/0");
    let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
    assert_eq!(c.agent(), Some("ua/0"));
    assert_eq!(c.agents().len(), MAX_CLIENT_AGENTS, "並びは変えない");

    // 同じ長い `User-Agent` は 1 種のまま (切った形で見比べるため)
    let long = "x".repeat(300);
    let m2 = Metrics::new();
    m2.record_client_agent("10.0.0.2", &long);
    m2.record_client_agent("10.0.0.2", &long);
    m2.record_client("10.0.0.2", HostOutcome::Bypass, 0, (0, 0), None, None);
    let c2 = m2.clients_sorted_by(ClientSort::Requests).remove(0).1;
    assert_eq!(c2.agents().len(), 1);
    assert_eq!(c2.agents_dropped(), 0);
    assert!(
        c2.agents()[0].len() <= MAX_AGENT_BYTES,
        "{}",
        c2.agents()[0].len()
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
    assert!(c.targets_capped());
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
    assert_eq!(c2.ports().len(), MAX_CLIENT_PORTS);
    assert_eq!(c2.ports_other(), 2);
    assert_eq!(c2.nonstandard_ports(), 10);
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
    assert_eq!(
        m.clients_sorted_by(ClientSort::Requests)[0].1.first_seen(),
        0
    );

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
    assert!(c.ports().is_empty());
    assert_eq!(c.nonstandard_ports(), 0);
}
