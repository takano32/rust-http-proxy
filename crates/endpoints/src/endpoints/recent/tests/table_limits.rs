//! 表の口 (`/dns` `/log` `/hosts` `/clients` `/hosts/series`) の応答が
//! [`MAX_BODY`] (256 KiB) に収まること (T34.86 で tests.rs から分けた)。

use super::*;

/// `/dns` は**どんな中身でも** 256 KiB に収まること。
///
/// 名前は 253 文字 (DNS の上限) まで、アドレスは 1 ホストに 8 本まで出すので、
/// 最悪の 1 行は 900 B 近くなる。件数の上限 (既定 300) だけでは足りないので
/// **バイト数でも打ち切る** — その打ち切りがちゃんと効くことをここで見る。
#[test]
fn the_dns_response_stays_under_256_kib() {
    use crate::dns::{MAX_ROW_ADDRS, TableRow};
    use std::net::IpAddr;
    let v6: IpAddr = "2001:db8:85a3:8d3:1319:8a2e:370:7348".parse().unwrap();
    let v4: IpAddr = "93.184.216.34".parse().unwrap();
    // (1) ありふれた行 (名前 15 文字、A と AAAA の 2 本)。既定の 300 行が収まること
    let plain = TableRow {
        host: "www.example.com".to_string(),
        addrs: vec![v4, v6],
        addr_count: 2,
        age_secs: 12,
        ttl_left: 48,
        idle_secs: 3,
        win_v6: Some(false),
        failed: None,
        refreshing: false,
        warm: true,
        next_refresh_secs: Some(33),
        misses: 4,
        refreshes: 2,
        // T14.37 で増えた欄 (引き直しで答えが変わった回数)。1 行の大きさに効く
        changes: 1,
        // T15.0 (7) で増えた欄 (warm のまま引かれた回数とミスの内訳)。同上
        warm_requests: 9,
        misses_by_kind: [2, 1, 0, 1],
    };
    let mut body = String::from(SCHEMA_HEAD) + "\"entries\":";
    let (shown, cut) = array_within(&mut body, vec![plain; 300].iter().map(|r| r.to_json()));
    body.push('}');
    assert_eq!(shown, 300);
    assert!(!cut);
    assert!(body.len() <= MAX_BODY, "{} B", body.len());
    println!(
        "dns 300 行 (ありふれた行): {} B (上限 {} B)",
        body.len(),
        MAX_BODY
    );

    // (2) 最悪の行を表いっぱい (4,096 = `MAX_ENTRIES`) 並べても上限を越えない
    let worst = TableRow {
        host: "a".repeat(253),
        addrs: vec![v6; MAX_ROW_ADDRS],
        addr_count: 32,
        age_secs: u64::MAX,
        ttl_left: u64::MAX,
        idle_secs: u64::MAX,
        win_v6: Some(true),
        failed: Some((u64::MAX, "e".repeat(300))),
        refreshing: true,
        warm: true,
        next_refresh_secs: Some(u64::MAX),
        misses: u64::MAX,
        refreshes: u64::MAX,
        changes: u64::MAX,
        warm_requests: u64::MAX,
        misses_by_kind: [u64::MAX; 4],
    };
    let mut body = String::from(SCHEMA_HEAD) + "\"entries\":";
    let (shown, cut) = array_within(&mut body, vec![worst; 4096].iter().map(|r| r.to_json()));
    body.push('}');
    assert!(cut, "バイト数で打ち切られること");
    assert!(body.len() <= MAX_BODY, "{} B", body.len());
    println!(
        "dns 4,096 行 (最悪の行): {} B / 出せたのは {} 行 (上限 {} B)",
        body.len(),
        shown,
        MAX_BODY
    );
}

/// `/log` はどんな中身でも 256 KiB に収まること。
///
/// 1 行 256 B × 1,000 行 = 250 KiB に JSON の飾りが乗るので、**上限いっぱいの
/// 1,000 行は入りきらない**のが設計どおり (既定の 200 行は最悪でも入る)。
/// 入らない分はバイト数で打ち切って `"truncated":true` を出す。
#[test]
fn the_log_response_stays_under_256_kib() {
    let line = crate::log::Line {
        at: u64::MAX,
        level: crate::log::Level::Warn,
        conn: Some(usize::MAX),
        // 1 行は 256 B で切ってあるが、`\"` に化ける文字だけの最悪も見る
        msg: "\"".repeat(crate::log::MAX_LOG_LINE),
    };
    let lines = vec![line; crate::log::MAX_LOG_LINES];
    for (n, want_cut) in [(200usize, false), (crate::log::MAX_LOG_LINES, true)] {
        let mut body = String::from(SCHEMA_HEAD) + "\"lines\":";
        let (shown, cut) = array_within(&mut body, lines.iter().take(n).map(log_line_json));
        body.push('}');
        assert_eq!(cut, want_cut, "{} 行", n);
        assert!(body.len() <= MAX_BODY, "{} 行で {} B", n, body.len());
        println!(
            "log {} 行 (最悪の行) の応答: {} B / 出せたのは {} 行 (上限 {} B)",
            n,
            body.len(),
            shown,
            MAX_BODY
        );
    }
}

/// `/hosts` は 1,000 ホスト (`MAX_HOSTS` = `.rrd` に入る全部) でも 256 KiB 以下。
///
/// デプロイ先並みの値なら 1,000 件が丸ごと入る。桁を振り切った値 (転送 20 桁、
/// 応答 9,999 ms、原因が 8 種類とも埋まる) を 1,000 件並べると入りきらないので、
/// そこは**バイト数で打ち切る** — その打ち切りが効くことも一緒に見る。
#[test]
fn the_hosts_response_stays_under_256_kib() {
    use crate::metrics::{Detail, ErrCause, HostOutcome, HostSort, MAX_HOSTS};
    use std::time::Duration;

    let row = |m: &Metrics, host: &str, bytes: u64, ms: u64, detail: Detail| {
        m.record_host_detail(
            host,
            HostOutcome::Bypass,
            bytes,
            Some(Duration::from_millis(ms)),
            detail,
        );
    };

    // (1) デプロイ先並み (鍵 29 文字、転送 100 MB、確立 12 ms)
    let plain = Metrics::new();
    for i in 0..MAX_HOSTS {
        row(
            &plain,
            &format!("connect://host{:04}.example.net:443", i),
            98_765_432,
            12,
            Detail {
                dns_ms: 3,
                dns_misses: 1,
                connect_ms: 9,
                family_v6: Some(false),
                cause: None,
                first_byte_ms: None,
                ..Detail::default()
            },
        );
    }
    // (2) 桁を振り切った最悪
    let worst = Metrics::new();
    for i in 0..MAX_HOSTS {
        row(
            &worst,
            &format!("connect://host{:04}.cdn.example.net:443", i),
            u64::MAX / 2,
            9999,
            Detail {
                dns_ms: 123,
                dns_misses: 45,
                connect_ms: 678,
                family_v6: Some(true),
                cause: Some(ErrCause::Dns),
                first_byte_ms: None,
                ..Detail::default()
            },
        );
    }

    let build = |m: &Metrics, take: usize| {
        let all = m.hosts_sorted_by(HostSort::Requests);
        assert_eq!(all.len(), MAX_HOSTS);
        let mut body = String::from(SCHEMA_HEAD) + "\"hosts\":";
        let (shown, cut) = array_within(
            &mut body,
            all.iter().take(take).map(|(h, s)| {
                format!(
                    "{{\"host\":\"{}\",{}}}",
                    crate::json::escape(h),
                    crate::metrics::stats_json(s, true)
                )
            }),
        );
        body.push('}');
        (body.len(), shown, cut)
    };

    // 既定の 200 件は、桁を振り切った値でも丸ごと入る
    let (len, shown, cut) = build(&worst, 200);
    assert_eq!(shown, 200);
    assert!(!cut);
    assert!(len <= MAX_BODY, "{} B", len);
    println!("hosts 200 件 (最悪の値): {} B (上限 {} B)", len, MAX_BODY);

    // 1,000 件は 1 件 325 B (ありふれた値) なので 256 KiB には入りきらない。
    // **バイト数で打ち切って上限を守る**のが設計どおり (`"truncated":true` で分かる)
    for (label, m) in [("ありふれた値", &plain), ("最悪の値", &worst)] {
        let (len, shown, cut) = build(m, MAX_HOSTS);
        assert!(len <= MAX_BODY, "{} で {} B", label, len);
        assert!(cut, "{}: バイト数で打ち切られること", label);
        assert!(shown >= 500, "{}: {} 件しか出ていない", label, shown);
        println!(
            "hosts 1,000 件 ({}): {} B / 出せたのは {} 件 (上限 {} B)",
            label, len, shown, MAX_BODY
        );
    }
}

/// `/clients` は 1,000 接続元 (`MAX_CLIENTS` = 表に入る全部) でも 256 KiB 以下。
///
/// 1 行の最悪は `User-Agent` 4 種 × 128 B が効いて 1 KB 近くになるので、
/// **既定の 200 件は丸ごと入り、1,000 件はバイト数で打ち切る**のが設計どおり。
#[test]
fn the_clients_response_stays_under_256_kib() {
    use crate::metrics::{ClientSort, HostOutcome, MAX_CLIENTS};
    use std::time::Duration;

    let m = Metrics::new();
    for i in 0..MAX_CLIENTS {
        let ip = format!("2001:db8:1234:5678:9abc:def0:1234:{:04x}", i);
        for a in 0..4 {
            m.record_client_agent(&ip, &format!("{}/{}", "x".repeat(200), a));
        }
        for p in 0..8u16 {
            m.record_client(
                &ip,
                HostOutcome::Bypass,
                // 8 本ぶんの合計が桁を振り切る手前まで (1 行の JSON を最悪にする)
                u64::MAX / 16,
                // 向き別も同じだけ振り切らせる (T14.26 の 2 欄ぶん行が太る最悪)
                (u64::MAX / 16, u64::MAX / 16),
                Some(Duration::from_millis(1234)),
                Some(&format!(
                    "very-long-host-name-{}.example.net:{}",
                    p,
                    1000 + p
                )),
            );
        }
    }
    let all = m.clients_sorted_by(ClientSort::Requests);
    assert_eq!(all.len(), MAX_CLIENTS);
    for (n, want_cut) in [(200usize, false), (MAX_CLIENTS, true)] {
        let mut body = String::from(SCHEMA_HEAD) + "\"clients\":";
        let (shown, cut) = array_within(&mut body, all.iter().take(n).map(|(c, s)| s.to_json(c)));
        body.push('}');
        assert!(body.len() <= MAX_BODY, "{} 件で {} B", n, body.len());
        assert_eq!(cut, want_cut, "{} 件", n);
        println!(
            "clients {} 件 (最悪の行): {} B / 出せたのは {} 件 (上限 {} B)",
            n,
            body.len(),
            shown,
            MAX_BODY
        );
    }
}

/// `/hosts/series` は 16 ホスト × 288 標本でも 256 KiB 以下 (T14.22)。
///
/// デプロイ先並みの値 (5 分に 300 本) なら 16 本が丸ごと入る。どの欄も 20 桁まで
/// 振り切った最悪は入りきらないので、そこは**バイト数で打ち切る** (`/hosts` と同じ)。
#[test]
fn the_host_series_response_stays_under_256_kib() {
    use crate::hostseries::{FIELDS, SAMPLES, SLOTS, Series};

    let plain: Vec<Series> = (0..SLOTS)
        .map(|i| Series {
            host: format!("connect://host{:02}.example.net:443", i),
            hour_requests: 3_600,
            // 5 分に 300 本、確立の合計 3,600 ms、最大 250 ms、名前解決 900 ms、エラー 2 件
            rows: vec![[300, 3_600, 250, 900, 2]; SAMPLES],
        })
        .collect();
    let worst: Vec<Series> = (0..SLOTS)
        .map(|i| Series {
            host: format!("connect://host{:02}.cdn.example.net:443", i),
            hour_requests: u64::MAX,
            rows: vec![[u64::MAX; FIELDS]; SAMPLES],
        })
        .collect();
    for (name, series, want_cut) in [("deployed-like", plain, false), ("worst", worst, true)] {
        let mut body = String::from(SCHEMA_HEAD) + "\"series\":";
        let (shown, cut) = array_within(&mut body, series.iter().map(series_json));
        body.push('}');
        assert_eq!(cut, want_cut, "{}", name);
        assert!(body.len() <= MAX_BODY, "{}: {} B", name, body.len());
        println!(
            "hosts/series {} ({} ホスト × {} 標本): {} B / 出せたのは {} 本 (上限 {} B)",
            name,
            SLOTS,
            SAMPLES,
            body.len(),
            shown,
            MAX_BODY
        );
    }
}
