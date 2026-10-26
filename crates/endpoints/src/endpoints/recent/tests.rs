use super::*;
use crate::metrics::{Detail, ErrCause, Metrics};
use crate::recent::ConnState;

/// 500 件の最悪 (宛先も接続元も上限いっぱい) でも 256 KiB に収まること。
#[test]
fn the_errors_response_stays_under_256_kib() {
    let m = Metrics::new();
    let long = "x".repeat(300);
    for i in 0..(MAX_ERRORS + 20) {
        // 宛先は 1 件ずつ変える (同じ失敗は 1 行にまとまるので、500 行にならない。T20.3)。
        // 頭の 3 桁が違うだけで、長さは今までどおり上限いっぱいに切られる
        m.record_error(
            true,
            &format!("{:03}{}", i, long),
            "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6",
            502,
            &Detail {
                cause: Some(ErrCause::Unreachable),
                dns_ms: u64::MAX,
                connect_ms: u64::MAX,
                ..Detail::default()
            },
        );
    }
    let ep_metrics = m;
    let (mut entries, total) = ep_metrics.errors.recent(MAX_ERRORS);
    // `repeats` も最悪の桁数にする (T20.3 で各行の末尾に足した)
    for e in &mut entries {
        e.repeats = u32::MAX;
    }
    assert_eq!(entries.len(), MAX_ERRORS);
    assert_eq!(total, MAX_ERRORS as u64 + 20);
    let mut body = String::from(SCHEMA_HEAD) + "\"errors\":";
    let (shown, cut) = array_within(&mut body, entries.iter().map(|e| e.to_json()));
    body.push('}');
    assert_eq!(shown, MAX_ERRORS, "500 件が全部入ること");
    assert!(!cut);
    assert!(body.len() <= MAX_BODY, "{} B", body.len());
    println!(
        "errors 500 件の応答: {} B (上限 {} B)",
        body.len(),
        MAX_BODY
    );
}

/// 原因の分からないエラーはリングに書かない。
#[test]
fn errors_without_a_cause_are_not_recorded() {
    let m = Metrics::new();
    m.record_error(
        false,
        "example.com:80",
        "127.0.0.1",
        502,
        &Detail::default(),
    );
    assert!(m.errors.is_empty());
}

/// 240 本 (デプロイ先の上限) でも、1,000 本でも 256 KiB に収まること。
///
/// **余りは 1 行あたり 5 B しか無い** (欄を足す前の実測 1,000 本で 256,918 B、
/// [`array_within`] の予算は 261,632 B)。T15.0 (4) で足した 5 つの欄が
/// 「既定のままなら 1 バイトも出さない」形なのはこのためで、ここでは**最悪の値が
/// 全部入った行を 10 本混ぜて**、それでも 1,000 本入ることを見る。
///
/// **このテストが担保しているのは「証拠つきの行が少数である 1,000 本」だけ**である
/// (デプロイ先の上限は 240 本で、そのうち欄を持つのは `relaying` のトンネルだけ)。
/// **全部の行が証拠つきなら 1,000 本は入らない** — どこで切れるかは
/// [`the_connections_response_cuts_when_every_row_carries_evidence`] が実測で縛る。
#[test]
fn the_connections_response_stays_under_256_kib() {
    use crate::recent::{ORIGIN_SIDE, pack_revents};

    /// 最悪の値を入れる行数 (残りは既定のまま = 新しい欄が 1 つも出ない)。
    const WORST: usize = 10;

    let m = Metrics::new();
    let long_host = format!("{}.example.net:65535", "sub.".repeat(30));
    let now = Instant::now();
    for i in 0..1000u64 {
        let slot = m
            .conns
            .register(i, "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6", now)
            .expect("登録できる");
        slot.begin_tunnel(&long_host);
        slot.set_bytes(u64::MAX);
        slot.set_state(ConnState::Parked);
        if (i as usize) < WORST {
            // T15.0 (4) の 5 つが全部、起こりえない桁で入っている行。
            // `0x03d` = `IN|OUT|ERR|HUP|NVAL` (旗の名前が最長になる組み合わせ)
            slot.set_tid(u32::MAX);
            slot.set_relaying(u64::MAX, u64::MAX, pack_revents(0x03d, 0x03d));
            slot.set_half_closed(ORIGIN_SIDE);
            slot.sweep_rate(999_999_999_999);
            slot.sweep_rate(999_999_999_999);
        }
    }
    let all = m.conns.snapshot();
    assert_eq!(all.len(), 1000);
    assert_eq!(all[0].id, 0, "古い順に並ぶ");
    let worst = all[0].to_json(now);
    let plain = all[WORST].to_json(now);
    println!(
        "connections の 1 行: 最悪 {} B / 既定 {} B",
        worst.len(),
        plain.len()
    );
    println!("  最悪の 1 行: {}", worst);
    assert!(worst.len() > plain.len(), "最悪の行に欄が出ていない");
    for n in [240usize, 1000] {
        let mut body = String::from(SCHEMA_HEAD) + "\"connections\":";
        let (shown, cut) = array_within(&mut body, all.iter().take(n).map(|c| c.to_json(now)));
        body.push('}');
        assert_eq!(shown, n, "{} 本が全部入ること", n);
        assert!(!cut);
        assert!(body.len() <= MAX_BODY, "{} 本で {} B", n, body.len());
        println!(
            "connections {} 本の応答: {} B (上限 {} B)",
            n,
            body.len(),
            MAX_BODY
        );
    }
}

/// **全部の行が証拠つきだと 1,000 本は入らない** (単位 7 の `offset=` が入るまでの
/// 既知の穴。T15.0 (4))。
///
/// 上の [`the_connections_response_stays_under_256_kib`] は「1,000 本のうち証拠つきは
/// 少数」を前提にしていて、そこだけ読むと「1,000 本はいつでも担保されている」と
/// 誤読できる。ここでは逆に**全部の行に証拠を入れて**、入る本数を実測で縛る。
///
/// 実測 (2026-09-18):
///
/// | 行の中身 | 1 行 | 入る本数 |
/// |---|---|---|
/// | 既定 (新しい欄が 1 つも出ない) | 255 B | 1,000 本 (上のテスト) |
/// | 現実的な `relaying` の行 (`tid` と `revents` だけ) | 301 B | **860 本** |
/// | 最悪 (5 つとも起こりえない桁) | 442 B | **588 本** |
///
/// 切れても黙って落ちることは無く、`shown` が入った本数・`truncated` が `true` に
/// なる (口の側は `count` も返すので、読む側は「全部は見えていない」と分かる)。
#[test]
fn the_connections_response_cuts_when_every_row_carries_evidence() {
    use crate::recent::{ORIGIN_SIDE, pack_revents};

    /// 「`tid` と旗だけ」の現実的な行がこれより少なければ知らせる (回帰の下限)。
    const REALISTIC_MIN: usize = 800;
    /// 「5 つとも最悪」の行がこれより少なければ知らせる (回帰の下限)。
    const WORST_MIN: usize = 550;

    // `worst` = 5 つとも最悪、`false` = `tid` と `revents` だけの現実的な行
    let fill = |worst: bool| -> Vec<String> {
        let m = Metrics::new();
        let long_host = format!("{}.example.net:65535", "sub.".repeat(30));
        let now = Instant::now();
        for i in 0..1000u64 {
            let slot = m
                .conns
                .register(i, "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6", now)
                .expect("登録できる");
            slot.begin_tunnel(&long_host);
            slot.set_state(ConnState::Relaying);
            slot.set_tid(u32::MAX);
            if worst {
                // `0x03d` = `IN|OUT|ERR|HUP|NVAL` (旗の名前が最長になる組み合わせ)
                slot.set_relaying(u64::MAX, u64::MAX, pack_revents(0x03d, 0x03d));
                slot.set_half_closed(ORIGIN_SIDE);
                slot.sweep_rate(999_999_999_999);
                slot.sweep_rate(999_999_999_999);
            } else {
                // 生きているトンネルのありふれた形: 起床の旗はあるが空回りしていない
                slot.set_relaying(1 << 30, 0, pack_revents(0x001, 0x000));
            }
        }
        m.conns.snapshot().iter().map(|c| c.to_json(now)).collect()
    };

    for (what, rows, least) in [
        ("tid と旗だけ", fill(false), REALISTIC_MIN),
        ("5 つとも最悪", fill(true), WORST_MIN),
    ] {
        let mut body = String::from(SCHEMA_HEAD) + "\"connections\":";
        let (shown, cut) = array_within(&mut body, rows.iter().cloned());
        body.push('}');
        println!(
            "connections 1,000 本が全部「{}」: 1 行 {} B → 入るのは {} 本 ({} B / 上限 {} B)",
            what,
            rows[0].len(),
            shown,
            body.len(),
            MAX_BODY
        );
        assert!(body.len() <= MAX_BODY, "{}: {} B", what, body.len());
        assert!(
            shown < 1000 && cut,
            "{}: 1,000 本入ってしまった (欄が縮んだなら上の表と README を直す)",
            what
        );
        assert!(
            shown >= least,
            "{}: 入るのが {} 本まで減った (期待は {} 本以上)",
            what,
            shown,
            least
        );
    }
}

/// `/recent` は 2,000 件のリングを丸ごと読んでも 256 KiB に収まること (T14.4)。
///
/// 1 件はありふれた値で 225 B なので **2,000 件 (440 KiB) は入りきらない**のが
/// 設計どおり。入らない分はバイト数で打ち切って `"truncated":true` を出す
/// (既定の 200 件は最悪の値でも入る)。ここは `/recent` そのものを通して見る。
#[test]
fn the_recent_response_stays_under_256_kib() {
    use crate::recent::{CloseReason, ConnTally, MAX_RECENT, SIDES, STAGES};

    let m = Metrics::new();
    let now = Instant::now();
    let long_host = format!("{}.example.net:65535", "sub.".repeat(30));
    for i in 0..(MAX_RECENT as u64) {
        let slot = m
            .conns
            .register(i, "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6", now)
            .expect("登録できる");
        slot.begin_tunnel(&long_host);
        slot.finish(
            CloseReason::Error(crate::metrics::ErrCause::Unreachable),
            ConnTally {
                up: u64::MAX,
                down: u64::MAX,
                status: 599,
                stage_ms: [u64::MAX; STAGES],
                rtt_us: [u32::MAX; SIDES],
                retrans: [u32::MAX; SIDES],
                syn_retrans: u8::MAX,
                stall_ms: [u32::MAX; SIDES],
            },
            u32::MAX,
        );
        m.record_closed(i);
    }
    assert_eq!(m.closed.len(), MAX_RECENT);
    assert!(m.conns.is_empty(), "抹消は接続の終了で 1 回ずつ");

    let cache = crate::cache::Cache::new(crate::cache::CacheConfig::disabled());
    let concurrency = || crate::metrics::Concurrency {
        max_conns: 0,
        max_threads: 0,
        live_threads: 0,
        idle_threads: 0,
        queued_jobs: 0,
    };
    let ep = Endpoint {
        metrics: &m,
        cache: &cache,
        conn_id: 1,
        port: 8080,
        host: None,
        client: None,
        pac_direct: &[],
        lite: false,
        readonly: false,
        version: "test",
        build: "test",
        concurrency: &concurrency,
        own_ip: &|_| false,
    };
    // **エンドポイントそのものを通す** (既定の 200 件は最悪の値でも入り、2,000 件は切れる)
    for (q, want_cut) in [("n=200", false), ("n=2000", true)] {
        let body = recent(&ep, Some(q)).2;
        assert!(body.len() <= MAX_BODY, "{} で {} B", q, body.len());
        assert!(
            body.contains(&format!("\"truncated\":{}", want_cut)),
            "{} の truncated が {} でない: …{}",
            q,
            want_cut,
            &body[body.len() - 220..]
        );
        assert!(body.contains("\"matched\":2000"), "{}", q);
        let shown = body.matches("\"id\":").count();
        assert_eq!(shown == 200, !want_cut, "{} で {} 件", q, shown);
        println!(
            "recent {} (最悪の値) の応答: {} B / 出せたのは {} 件 (上限 {} B)",
            q,
            body.len(),
            shown,
            MAX_BODY
        );
    }
}

/// `/events` の形と、`?n=` `?since=` の絞り (T14.11)。
///
/// リングは静的に 1 本なので、ここでは 1 つのテストにまとめて順に見る。
#[test]
fn the_events_endpoint_filters_by_n_and_since() {
    use crate::events::{EventKind, MAX_EVENTS};
    let m = crate::metrics::Metrics::new();
    let cache = crate::cache::Cache::new(crate::cache::CacheConfig::disabled());
    let concurrency = || crate::metrics::Concurrency {
        max_conns: 0,
        max_threads: 0,
        live_threads: 0,
        idle_threads: 0,
        queued_jobs: 0,
    };
    let ep = Endpoint {
        metrics: &m,
        cache: &cache,
        conn_id: 1,
        port: 8080,
        host: None,
        client: None,
        pac_direct: &[],
        lite: false,
        readonly: false,
        version: "test",
        build: "test",
        concurrency: &concurrency,
        own_ip: &|_| false,
    };
    crate::events::clear();
    crate::events::push(EventKind::Start, "version 0.0.0 on port 8080");
    crate::events::push(EventKind::Reload, "PROXY_TIMEOUT_SECS 30 \u{2192} 10");
    let body = events(&ep, None).2;
    assert!(body.starts_with("{\"schema\":1,\"events\":["), "{}", body);
    assert!(body.contains("\"kind\":\"reload\""), "{}", body);
    assert!(
        body.contains("PROXY_TIMEOUT_SECS 30 \u{2192} 10"),
        "前後の値が出る: {}",
        body
    );
    assert!(body.contains("\"count\":2"), "{}", body);
    assert!(body.contains("\"recorded\":2"), "{}", body);
    assert!(body.contains("\"capacity\":512"), "{}", body);
    assert!(body.contains("\"truncated\":false"), "{}", body);
    // 12 種の名前が全部出る (README の一覧と合っているか)
    for kind in [
        "start",
        "reload",
        "blocklist",
        "ipv6",
        "pressure",
        "ballast",
        "state_file",
        "evict",
        "emfile",
        "shutdown",
        "anomaly",
        "new_client",
    ] {
        assert!(body.contains(&format!("\"{}\"", kind)), "{} が無い", kind);
    }
    // 新しい順
    let first = body.find("reload").unwrap();
    let second = body.find("\"kind\":\"start\"").unwrap();
    assert!(first < second, "新しい順でない: {}", body);
    // `?n=1` で 1 件
    let one = events(&ep, Some("n=1")).2;
    assert!(one.contains("\"count\":1"), "{}", one);
    assert!(!one.contains("\"kind\":\"start\""), "{}", one);
    // `?since=` は「その時刻以降」。先の時刻なら 0 件
    let none = events(&ep, Some("since=9999999999")).2;
    assert!(none.starts_with("{\"schema\":1,\"events\":[]"), "{}", none);
    assert!(none.contains("\"since\":9999999999"), "{}", none);
    assert!(none.contains("\"recorded\":2"), "通算は残る: {}", none);
    // 満杯 (512 件) の最悪でも 256 KiB に収まる
    crate::events::clear();
    for i in 0..MAX_EVENTS + 10 {
        crate::events::push(
            EventKind::Blocklist,
            &format!("{}{}", "\u{2192}".repeat(50), i),
        );
    }
    let body = events(&ep, Some("n=512")).2;
    assert!(body.contains("\"count\":512"), "{}", body);
    assert!(body.contains("\"truncated\":false"), "{}", body);
    assert!(body.len() <= MAX_BODY, "{} B", body.len());
    println!(
        "events 512 件の応答: {} B (上限 {} B)",
        body.len(),
        MAX_BODY
    );
    crate::events::clear();
}

/// `?sort=` の並びと `?since=` / `?client=` の絞り (T14.4)。
#[test]
fn recent_sorts_and_filters() {
    let m = Metrics::new();
    let now = Instant::now();
    for (i, (client, connect_ms, bytes)) in [
        ("10.0.0.1", 5u64, 100u64),
        ("10.0.0.2", 90, 10),
        ("10.0.0.1", 1, 5000),
    ]
    .into_iter()
    .enumerate()
    {
        let slot = m.conns.register(i as u64, client, now).expect("登録できる");
        slot.begin_tunnel("example.net:443");
        slot.finish(
            crate::recent::CloseReason::ClientEof,
            crate::recent::ConnTally {
                up: 0,
                down: bytes,
                status: 0,
                stage_ms: [0, connect_ms, 0, 0, 0, 0],
                ..Default::default()
            },
            0,
        );
        m.record_closed(i as u64);
    }
    let concurrency = || crate::metrics::Concurrency {
        max_conns: 0,
        max_threads: 0,
        live_threads: 0,
        idle_threads: 0,
        queued_jobs: 0,
    };
    let cache = crate::cache::Cache::new(crate::cache::CacheConfig::disabled());
    let ep = Endpoint {
        metrics: &m,
        cache: &cache,
        conn_id: 1,
        port: 8080,
        host: None,
        client: None,
        pac_direct: &[],
        lite: false,
        readonly: false,
        version: "test",
        build: "test",
        concurrency: &concurrency,
        own_ip: &|_| false,
    };
    // 既定は閉じた新しい順
    let body = recent(&ep, None).2;
    assert!(body.contains("\"count\":3"), "{}", body);
    assert!(body.contains("\"sort\":\"time\""), "{}", body);
    let ids = |b: &str| {
        b.match_indices("\"id\":")
            .map(|(i, _)| b[i + 5..].split(',').next().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&body), ["2", "1", "0"]);
    // 確立の遅い順 / 転送の多い順
    assert_eq!(ids(&recent(&ep, Some("sort=slow")).2), ["1", "0", "2"]);
    assert_eq!(ids(&recent(&ep, Some("sort=bytes")).2), ["2", "0", "1"]);
    // 接続元で絞る
    let mine = recent(&ep, Some("client=10.0.0.1")).2;
    assert_eq!(ids(&mine), ["2", "0"]);
    assert!(mine.contains("\"client\":\"10.0.0.1\""), "{}", mine);
    // 未来の時刻で絞れば 0 件 (`recorded` は減らない)
    let none = recent(&ep, Some("since=9999999999")).2;
    assert!(none.contains("\"recent\":[]"), "{}", none);
    assert!(none.contains("\"recorded\":3"), "{}", none);
    // `?n=` で件数を絞る
    assert_eq!(ids(&recent(&ep, Some("n=1")).2).len(), 1);
}

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

/// `/bursts` は 50 枚の最悪 (接続元も宛先も上限いっぱい) でも 256 KiB に収まること (T14.6)。
#[test]
fn the_bursts_response_stays_under_256_kib() {
    use crate::recent::{BurstShot, MAX_BURSTS, MAX_SHOT_CLIENTS, MAX_SHOT_TARGETS};

    let m = Metrics::new();
    let now = Instant::now();
    let long_host = format!("{}.example.net:65535", "sub.".repeat(30));
    for i in 0..(MAX_SHOT_CLIENTS + MAX_SHOT_TARGETS + 8) as u64 {
        let slot = m
            .conns
            .register(
                i,
                &format!("2001:0db8:0000:0000:0000:ff00:0042:{:04x}%enp0s31f6", i),
                now,
            )
            .expect("登録できる");
        slot.begin_tunnel(&format!("{}{}", i, long_host));
    }
    let rows = m.conns.snapshot();
    for i in 0..(MAX_BURSTS as u64 + 5) {
        m.bursts.push(BurstShot::take(
            &rows,
            i + 1,
            usize::MAX,
            usize::MAX,
            4096,
            2048,
            u64::MAX,
            u64::MAX,
        ));
    }
    assert_eq!(m.bursts.len(), MAX_BURSTS);

    let cache = crate::cache::Cache::new(crate::cache::CacheConfig::disabled());
    let concurrency = || crate::metrics::Concurrency {
        max_conns: 0,
        max_threads: 0,
        live_threads: 0,
        idle_threads: 0,
        queued_jobs: 0,
    };
    let ep = Endpoint {
        metrics: &m,
        cache: &cache,
        conn_id: 1,
        port: 8080,
        host: None,
        client: None,
        pac_direct: &[],
        lite: false,
        readonly: false,
        version: "test",
        build: "test",
        concurrency: &concurrency,
        own_ip: &|_| false,
    };
    let body = bursts(&ep, None).2;
    assert!(body.len() <= MAX_BODY, "{} B", body.len());
    assert!(
        body.contains("\"count\":50"),
        "50 枚が全部入ること: {}",
        &body[body.len() - 300..]
    );
    assert!(
        body.contains("\"truncated\":false"),
        "{}",
        &body[body.len() - 300..]
    );
    assert!(
        body.contains("\"recorded\":55"),
        "{}",
        &body[body.len() - 300..]
    );
    println!(
        "bursts 50 枚 (最悪の 1 枚): {} B / 1 枚 {} B (上限 {} B)",
        body.len(),
        body.len() / MAX_BURSTS,
        MAX_BODY
    );
    // `?n=` で絞れ、1 枚も無ければ空の一覧
    assert_eq!(bursts(&ep, Some("n=3")).2.matches("\"seq\":").count(), 3);
    let empty = Metrics::new();
    let ep2 = Endpoint {
        metrics: &empty,
        ..ep
    };
    let none = bursts(&ep2, None).2;
    assert!(none.contains("\"bursts\":[]"), "{}", none);
    assert!(none.contains("\"recorded\":0"), "{}", none);
    assert!(none.contains("\"armed\":true"), "{}", none);
    assert!(none.contains("\"pending\":false"), "{}", none);
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

#[test]
fn numeric_parameters_fall_back_to_the_default() {
    assert_eq!(num_param(None, "n", 100, 500), 100);
    assert_eq!(num_param(Some("n=7"), "n", 100, 500), 7);
    assert_eq!(num_param(Some("n=9999"), "n", 100, 500), 500);
    assert_eq!(num_param(Some("n=0"), "n", 100, 500), 1);
    assert_eq!(num_param(Some("n=abc"), "n", 100, 500), 100);
    assert_eq!(str_param(Some("sort=host"), "sort"), "host");
    assert_eq!(str_param(Some("a=1&sort=misses"), "sort"), "misses");
    assert_eq!(str_param(None, "sort"), "");
}

/// `?offset=` は**下限 0** (`num_param` は `.clamp(1, max)` なので 0 を通せない。T15.0 (11))。
#[test]
fn the_offset_parameter_starts_at_zero() {
    assert_eq!(offset_param(None, 2000), 0);
    assert_eq!(offset_param(Some("offset=0"), 2000), 0);
    assert_eq!(offset_param(Some("offset=1"), 2000), 1);
    assert_eq!(offset_param(Some("n=200&offset=1500"), 2000), 1500);
    // 端は上だけ止める (`n=` と同じ方針)
    assert_eq!(offset_param(Some("offset=99999"), 2000), 2000);
    assert_eq!(offset_param(Some("offset=abc"), 2000), 0);
    assert_eq!(offset_param(Some("offset="), 2000), 0);
    // `?n=` の方は今までどおり 1 が下限
    assert_eq!(num_param(Some("offset=0"), "n", 200, 2000), 200);
}

/// `next_offset` は「続きがあれば次の `offset`、無ければ `null`」(T15.0 (11))。
#[test]
fn the_next_offset_is_null_at_the_end() {
    assert_eq!(next_offset(0, 500, 2000), "500");
    assert_eq!(next_offset(1500, 500, 2000), "null");
    assert_eq!(next_offset(0, 0, 0), "null");
    // バイト数で切れた頁でも続きの位置は出す (切れた所から次を引ける)
    assert_eq!(next_offset(0, 615, 2000), "615");
    // 並びの外を指した頁は空で終わり
    assert_eq!(next_offset(2000, 0, 2000), "null");
}

/// `/recent?offset=` の 4 頁で **2,000 本が重複なく揃う** (T15.0 (11))。
///
/// 1 枚では [`MAX_BODY`] に 615 本しか入らない (1 件 424 B × 2,000 = 828 KiB) ので、
/// 雪像を 1 枚で読むには頁が要る。ここで縛るのは「4 頁の `id` の集合が
/// 0..2,000 と完全に一致する」= **落ちない・重ならない**こと。
#[test]
fn the_recent_endpoint_pages_through_the_whole_ring_with_offset() {
    use crate::recent::{CloseReason, ConnTally, MAX_RECENT};

    let m = Metrics::new();
    let now = Instant::now();
    for i in 0..(MAX_RECENT as u64) {
        let slot = m
            .conns
            .register(i, "198.51.100.7", now)
            .expect("登録できる");
        slot.begin_tunnel("host.example.net:443");
        slot.finish(CloseReason::ClientEof, ConnTally::default(), 1);
        m.record_closed(i);
    }
    assert_eq!(m.closed.len(), MAX_RECENT);

    let cache = crate::cache::Cache::new(crate::cache::CacheConfig::disabled());
    let concurrency = || crate::metrics::Concurrency {
        max_conns: 0,
        max_threads: 0,
        live_threads: 0,
        idle_threads: 0,
        queued_jobs: 0,
    };
    let ep = Endpoint {
        metrics: &m,
        cache: &cache,
        conn_id: 1,
        port: 8080,
        host: None,
        client: None,
        pac_direct: &[],
        lite: false,
        readonly: false,
        version: "test",
        build: "test",
        concurrency: &concurrency,
        own_ip: &|_| false,
    };

    let page = 500;
    let mut seen: Vec<u64> = Vec::with_capacity(MAX_RECENT);
    let mut offset = 0usize;
    for p in 0..4 {
        let body = recent(&ep, Some(&format!("n={}&offset={}", page, offset))).2;
        assert!(body.len() <= MAX_BODY, "{} 頁目で {} B", p + 1, body.len());
        assert!(
            body.contains("\"truncated\":false"),
            "{} 頁目がバイト数で切れた ({} B)",
            p + 1,
            body.len()
        );
        assert!(
            body.contains(&format!("\"offset\":{}", offset)),
            "{}",
            p + 1
        );
        let ids: Vec<u64> = body
            .match_indices("\"id\":")
            .map(|(at, pat)| {
                body[at + pat.len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .expect("id は数")
            })
            .collect();
        assert_eq!(ids.len(), page, "{} 頁目の件数", p + 1);
        seen.extend(ids);
        // 最後の頁だけ `next_offset` が `null`
        let want_next = if p == 3 {
            "\"next_offset\":null".to_string()
        } else {
            format!("\"next_offset\":{}", offset + page)
        };
        assert!(body.contains(&want_next), "{} 頁目: {}", p + 1, want_next);
        offset += page;
    }
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), MAX_RECENT, "重複か欠けがある");
    assert_eq!(seen[0], 0);
    assert_eq!(seen[MAX_RECENT - 1], MAX_RECENT as u64 - 1);

    // 並びの外を指したら空の頁 (エラーにはしない)
    let past = recent(&ep, Some("n=200&offset=2000")).2;
    assert!(past.contains("\"recent\":[]"), "{}", past);
    assert!(past.contains("\"next_offset\":null"), "{}", past);
    assert!(past.contains("\"matched\":2000"), "{}", past);
}

/// `/hosts?offset=` の 2 頁で **1,000 ホストが重複なく揃う** (T15.0 (11))。
///
/// 1 枚では 639 件で切れていた (1,000 件 × 408 B = 398 KiB)。2 枚の雪像の差分から
/// 8 ホストが黙って落ちたのがこの欄を足した理由なので、**落ちないこと**を縛る。
#[test]
fn the_hosts_endpoint_pages_through_every_host_with_offset() {
    use crate::metrics::{HostOutcome, MAX_HOSTS};
    use std::time::Duration;

    let m = Metrics::new();
    for i in 0..MAX_HOSTS {
        m.record_host_timed(
            &format!("connect://host{:04}.example.net:443", i),
            HostOutcome::Bypass,
            1_234,
            Duration::from_millis(12),
        );
    }
    let cache = crate::cache::Cache::new(crate::cache::CacheConfig::disabled());
    let concurrency = || crate::metrics::Concurrency {
        max_conns: 0,
        max_threads: 0,
        live_threads: 0,
        idle_threads: 0,
        queued_jobs: 0,
    };
    let ep = Endpoint {
        metrics: &m,
        cache: &cache,
        conn_id: 1,
        port: 8080,
        host: None,
        client: None,
        pac_direct: &[],
        lite: false,
        readonly: false,
        version: "test",
        build: "test",
        concurrency: &concurrency,
        own_ip: &|_| false,
    };

    let page = 500;
    let mut seen: Vec<String> = Vec::with_capacity(MAX_HOSTS);
    for (p, offset) in [0usize, page].into_iter().enumerate() {
        let body = hosts(
            &ep,
            Some(&format!("sort=requests&limit={}&offset={}", page, offset)),
        )
        .2;
        assert!(body.len() <= MAX_BODY, "{} 頁目で {} B", p + 1, body.len());
        assert!(
            body.contains("\"truncated\":false"),
            "{} 頁目がバイト数で切れた ({} B)",
            p + 1,
            body.len()
        );
        assert!(body.contains("\"count\":1000"), "{}", body.len());
        assert!(
            body.contains(&format!("\"offset\":{}", offset)),
            "{}",
            p + 1
        );
        for (at, pat) in body.match_indices("{\"host\":\"") {
            let rest = &body[at + pat.len()..];
            let end = rest.find('"').expect("閉じ引用符");
            seen.push(rest[..end].to_string());
        }
        let want_next = if p == 0 {
            format!("\"next_offset\":{}", page)
        } else {
            "\"next_offset\":null".to_string()
        };
        assert!(body.contains(&want_next), "{} 頁目: {}", p + 1, want_next);
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), MAX_HOSTS, "重複か欠けがある");
}
