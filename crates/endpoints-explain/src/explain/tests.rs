use super::client::{ClientFacts, client_summary};
use super::host::{HostFacts, host_summary, key_host_port, merge_into, same_host};
use super::*;
use crate::metrics::{ClientStats, DNS_CAUSE_NAMES, ERR_CAUSE_NAMES, HostStats, LATENCY_BOUNDS_MS};

/// 鍵と個票の宛先から、ホストとポートを同じ形で取れること。
#[test]
fn the_key_and_the_target_split_the_same_way() {
    assert_eq!(key_host_port("connect://a.test:443"), ("a.test", Some(443)));
    assert_eq!(key_host_port("http://a.test:80/x"), ("a.test", Some(80)));
    assert_eq!(key_host_port("blocked://ads.test"), ("ads.test", None));
    assert_eq!(key_host_port("a.test:8080"), ("a.test", Some(8080)));
    assert_eq!(key_host_port("[::1]:443"), ("::1", Some(443)));
}

/// ポートを書かなければ全ポート、書けばそのポートだけ。大小は区別しない。
#[test]
fn a_name_without_a_port_gathers_every_port() {
    assert!(same_host("connect://a.test:443", "a.test", None));
    assert!(same_host("http://A.TEST:80", "a.test", None));
    assert!(same_host("connect://a.test:443", "a.test", Some(443)));
    assert!(!same_host("connect://a.test:443", "a.test", Some(8443)));
    // ポートを持たない鍵は「ポート指定あり」では混ぜない
    assert!(!same_host("blocked://a.test", "a.test", Some(443)));
    assert!(same_host("blocked://a.test", "a.test", None));
    assert!(!same_host("connect://b.test:443", "a.test", None));
}

/// 足し算で分位点と RTT の最小が壊れないこと (ポート違いをまとめる道)。
#[test]
fn merging_two_ports_keeps_the_quantiles_and_the_minimum_rtt() {
    let two = HostStats {
        requests: 2,
        timed: 2,
        duration_ms_sum: 20,
        duration_ms_max: 10,
        buckets: {
            let mut b = [0u64; LATENCY_BOUNDS_MS.len() + 1];
            b[0] = 2;
            b
        },
        ..HostStats::default()
    };
    let a = HostStats {
        rtt_us_min: 500,
        rtt_us_sum: 1000,
        rtt_samples: 2,
        ..two.clone()
    };
    let b = HostStats {
        rtt_us_min: 300,
        rtt_us_sum: 600,
        rtt_samples: 2,
        ..two
    };
    let mut acc = HostStats::default();
    merge_into(&mut acc, &a);
    merge_into(&mut acc, &b);
    assert_eq!(acc.requests, 4);
    assert_eq!(acc.timed, 4);
    assert_eq!(acc.buckets[0], 4);
    assert_eq!(acc.avg_ms(), 10.0);
    assert_eq!(acc.rtt_us_min, 300);
    assert_eq!(acc.rtt_samples, 4);
    // 片方だけ RTT を持っているときは、その値がそのまま最小になる
    let mut only = HostStats::default();
    merge_into(&mut only, &HostStats::default());
    merge_into(&mut only, &a);
    assert_eq!(only.rtt_us_min, 500);
}

/// 1 行の判定が段階を足し算の形で並べ、数字の欄と同じ値を持つこと。
#[test]
fn the_one_line_verdict_lines_up_the_stages() {
    let s = HostStats {
        requests: 10,
        timed: 10,
        duration_ms_sum: 2590,
        duration_ms_max: 300,
        buckets: {
            let mut b = [0u64; LATENCY_BOUNDS_MS.len() + 1];
            // 区間の最後 (上限なし) に入れて、分位点が最大値で頭打ちになる道を通す
            b[LATENCY_BOUNDS_MS.len()] = 10;
            b
        },
        dns_ms_sum: 90,
        dns_misses: 3,
        connect_ms_sum: 2500,
        rtt_us_sum: 2_500_000,
        rtt_us_min: 250_000,
        rtt_samples: 10,
        ..HostStats::default()
    };
    let (text, numbers) = host_summary(&s, 10, 0, None, 3, 0, true);
    assert!(text.contains("確立の平均 259.0 ms"), "{}", text);
    assert!(
        text.contains("名前解決 9.0 + 接続 250.0 + プロキシ側 0.0 ms"),
        "{}",
        text
    );
    assert!(text.contains("カーネルの RTT 250.0 ms"), "{}", text);
    assert!(text.contains("名前解決の表に行は無い"), "{}", text);
    assert!(numbers.contains("\"avg_ms\":259.0"), "{}", numbers);
    assert!(numbers.contains("\"connect_ms\":250.0"), "{}", numbers);
    assert!(numbers.contains("\"dns_ms\":9.0"), "{}", numbers);
    assert!(numbers.contains("\"rtt_ms\":250.0"), "{}", numbers);
    assert!(numbers.contains("\"dns_entry\":false"), "{}", numbers);
    // forward だけなら「応答」、混ざれば「所要」。引き算の残りの呼び名も変わる
    // (forward の平均はオリジンの応答と本文まで含むので「プロキシ側」とは呼べない)
    let (fwd, _) = host_summary(&s, 0, 10, None, 0, 0, true);
    assert!(fwd.contains("応答の平均"), "{}", fwd);
    assert!(
        fwd.contains("残り (プロキシ側の待ち + オリジンの応答と本文) 0.0 ms"),
        "{}",
        fwd
    );
    let (both, _) = host_summary(&s, 5, 5, None, 0, 0, true);
    assert!(both.contains("所要の平均"), "{}", both);
    // 知らない相手は「1 件も無い」と言う
    let (none, numbers) = host_summary(&HostStats::default(), 0, 0, None, 0, 0, false);
    assert!(none.contains("記録は 1 件も無い"), "{}", none);
    assert!(numbers.contains("\"requests\":0"), "{}", numbers);
}

/// ホストの 1 枚の**鍵の並び** (T.34.4 で材料と組み立てを分けたので、材料を手で作って縛る)。
///
/// 読む道具 (`scripts/` と `inspect.html`) は鍵の名前と順で引くので、ここが 1 バイト変わると
/// 読めなくなる。表もリングも読まず、`render` だけを通す。
#[test]
fn the_host_page_keeps_its_key_order() {
    let stats = HostStats {
        requests: 3,
        timed: 3,
        duration_ms_sum: 30,
        duration_ms_max: 20,
        connect_ms_sum: 15,
        ..HostStats::default()
    };
    let facts = HostFacts {
        query: "a.test:443".to_string(),
        want: "a.test".to_string(),
        want_port: Some(443),
        stats: stats.clone(),
        keys: vec![("connect://a.test:443".to_string(), 3)],
        keys_total: 1,
        connects: 3,
        forwards: 0,
        dns_row: None,
        recent: Vec::new(),
        recent_scanned: 7,
        errors: Vec::new(),
        series: "null".to_string(),
        anomalies: vec!["{\"kind\":\"anomaly\"}".to_string()],
        at: 1_700_000_000,
    };
    let out = facts.render();
    let head = format!(
        "{}\"kind\":\"host\",\"query\":\"a.test:443\",\"host\":\"a.test\",\"port\":443,\"known\":true,\"at\":1700000000,\"keys\":[{{\"key\":\"connect://a.test:443\",\"requests\":3}}],\"keys_total\":1,\"stats\":{{{}}},\"errors_by_cause\":{{",
        SCHEMA_HEAD,
        crate::metrics::stats_json(&stats, true)
    );
    assert!(out.starts_with(&head), "{}", out);
    // 原因の内訳は 8 つの名前のあとに DNS の 4 つ (T25.0) が続く
    let causes: String = ERR_CAUSE_NAMES
        .iter()
        .chain(DNS_CAUSE_NAMES.iter())
        .map(|n| format!("\"{}\":0", n))
        .collect::<Vec<_>>()
        .join(",");
    let middle = format!(
        "{}}},\"dns\":null,\"recent\":[],\"recent_matched\":0,\"recent_scanned\":7,\"errors\":[],\"errors_matched\":0,\"series\":null,\"anomalies\":[{{\"kind\":\"anomaly\"}}],\"numbers\":{{\"requests\":3,\"connect_requests\":3,\"forward_requests\":0,",
        causes
    );
    assert!(out.contains(&middle), "{}", out);
    assert!(
        out.contains("\"summary\":\"3 要求 (CONNECT 3 / forward 0)。確立の平均 10.0 ms = 名前解決 0.0 + 接続 5.0 + プロキシ側 5.0 ms。"),
        "{}",
        out
    );
    assert!(out.ends_with(",\"truncated\":false}"), "{}", out);
    assert!(out.len() <= MAX_BODY);

    // 知らない相手: ポート無しは `null`、`known` は偽、文章は「1 件も無い」
    let none = HostFacts {
        query: "nobody.test".to_string(),
        want: "nobody.test".to_string(),
        want_port: None,
        stats: HostStats::default(),
        keys: Vec::new(),
        keys_total: 0,
        connects: 0,
        forwards: 0,
        dns_row: None,
        recent: Vec::new(),
        recent_scanned: 0,
        errors: Vec::new(),
        series: "null".to_string(),
        anomalies: Vec::new(),
        at: 1,
    }
    .render();
    assert!(
        none.contains("\"host\":\"nobody.test\",\"port\":null,\"known\":false,\"at\":1,\"keys\":[],\"keys_total\":0,"),
        "{}",
        none
    );
    assert!(
        none.contains("\"anomalies\":[],\"numbers\":{\"requests\":0,"),
        "{}",
        none
    );
    assert!(none.contains("この相手の記録は 1 件も無い"), "{}", none);
}

/// 接続元の 1 枚の鍵の並びと、知らない接続元の形。
#[test]
fn the_client_page_keeps_its_key_order() {
    let none = ClientFacts {
        ip: "203.0.113.9".to_string(),
        row: None,
        recent: Vec::new(),
        errors: Vec::new(),
        anomalies: Vec::new(),
        at: 42,
    }
    .render();
    assert!(
        none.starts_with(&format!(
            "{}\"kind\":\"client\",\"query\":\"203.0.113.9\",\"client\":\"203.0.113.9\",\"known\":false,\"at\":42,\"stats\":null,\"recent\":[],\"recent_matched\":0,\"errors\":[],\"errors_matched\":0,\"anomalies\":[],\"numbers\":{{\"requests\":0,\"timed\":0,",
            SCHEMA_HEAD
        )),
        "{}",
        none
    );
    assert!(none.contains("この接続元の記録は 1 件も無い"), "{}", none);
    assert!(none.ends_with(",\"truncated\":false}"), "{}", none);

    // 行があれば `stats` にその行 (`/clients` と同じ形) が入り、`known` は真
    let mut c = ClientStats::restored(HostStats {
        requests: 4,
        ..HostStats::default()
    });
    c.note_rejected();
    let known = ClientFacts {
        ip: "10.0.0.1".to_string(),
        row: Some(("10.0.0.1".to_string(), c.clone())),
        recent: Vec::new(),
        errors: Vec::new(),
        anomalies: Vec::new(),
        at: 43,
    }
    .render();
    assert!(
        known.contains(&format!(
            "\"known\":true,\"at\":43,\"stats\":{},\"recent\":[]",
            c.to_json("10.0.0.1")
        )),
        "{}",
        known
    );
    let (text, numbers) = client_summary(Some(&c), 0, 0, true);
    assert!(text.starts_with("4 要求。"), "{}", text);
    assert!(text.contains("断った接続 1 本。"), "{}", text);
    assert!(numbers.contains("\"requests\":4,"), "{}", numbers);
    assert!(numbers.contains(",\"rejected\":1,"), "{}", numbers);
}

/// 配列は予算に収まる分だけ入れ、入らなければそこで止めて真を返す。名前は 80 バイトで切る。
#[test]
fn arrays_stop_at_the_budget_and_names_are_clipped() {
    let mut out = String::new();
    assert!(!array_within(&mut out, ["1".to_string(), "22".to_string()]));
    assert_eq!(out, "[1,22]");
    let mut out = String::new();
    assert!(!array_within(&mut out, Vec::<String>::new()));
    assert_eq!(out, "[]");
    // 1 要素が予算 (64 KiB − 2 KiB) を越えると 1 つも入らず、切ったことが返る
    let mut out = String::new();
    let big = "x".repeat(MAX_BODY);
    assert!(array_within(&mut out, [big]));
    assert_eq!(out, "[]");
    // 入るものを入れたあとで越える要素が来たらそこで止める
    let mut out = String::new();
    let piece = "y".repeat(30 * 1024);
    assert!(array_within(
        &mut out,
        [piece.clone(), piece.clone(), piece]
    ));
    assert!(out.contains("yyyy"));
    assert!(out.len() <= MAX_BODY - 2048);
    assert_eq!(out.len(), 2 * 30 * 1024 + 3, "2 つ入って 3 つ目で止まる");
    assert_eq!(clip("short"), "short");
    assert_eq!(clip(&"a".repeat(200)).len(), 80);
}
