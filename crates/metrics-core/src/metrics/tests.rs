use super::status::{canary_status_json, push_env};
use super::*;

/// canary を預かっていないときの `/status` の `canary` (T15.12)。
///
/// canary は 1 つ上の層 (`proxy-metrics-watch`) に居るので、**この binary では
/// 誰も預けない** = 既定の文字列が出る。`canary` が `off` のときと 1 バイト同じ
/// であることは向こう側のテスト
/// (`canary::tests::the_default_canary_status_is_byte_identical_to_off`) が縛る。
#[test]
fn the_status_canary_falls_back_to_the_off_shaped_default_when_nobody_deposited_it() {
    assert_eq!(canary_status_json(), CANARY_STATUS_UNSET);
    let mut out = String::new();
    push_env(&mut out, &StatusExtras::default(), "{}");
    assert!(
        out.contains(&format!("\"canary\":{},", CANARY_STATUS_UNSET)),
        "{}",
        out
    );
}

#[test]
fn test_metrics() {
    let metrics = Metrics::new();
    metrics.inc_requests();
    metrics.inc_active_conn();
    metrics.add_bytes(1024);

    let json = metrics.to_json();
    assert!(json.contains("\"status\":\"ok\""));
    assert!(json.contains("\"total_requests\":1"));
    assert!(json.contains("\"active_connections\":1"));
    assert!(json.contains("\"bytes_forwarded\":1024"));

    metrics.inc_cache_hit();
    metrics.inc_cache_miss();
    let json_c = metrics.to_json();
    assert!(json_c.contains("\"cache_hits\":1"));
    assert!(json_c.contains("\"cache_misses\":1"));
    assert!(json_c.contains("\"cache\":null"));

    metrics.dec_active_conn();
    let json2 = metrics.to_json();
    assert!(json2.contains("\"active_connections\":0"));
    // 上の層が渡さないときは 0 (= 無制限・数えていない) で出る
    assert!(json2.contains("\"max_conns\":0"));
    assert!(
        json2.contains("\"live_threads\":0,\"idle_threads\":0,\"queued_jobs\":0,\"max_threads\":0")
    );
}

/// 同時接続の**山** (`/history` の `active_peak`。T15.0 (10))。
///
/// 見るのは 4 つ: (a) 5 秒より短い山は瞬間値 (`active_connections`) からは
/// 見えないが山には残る、(b) 読むと **0 ではなく今の本数**に戻る
/// (張りっぱなしの接続が次の区間で消えないように)、(c) 山は下がらない、
/// (d) 読みと書き戻しの競合で山が今の本数より低くなっても、戻り値は下振れしない。
#[test]
fn the_active_peak_keeps_spikes_the_instant_value_misses() {
    let m = Metrics::new();
    // (a) 50 本張ってすぐ閉じる = 標本と標本の間で終わる山
    for _ in 0..50 {
        m.inc_active_conn();
    }
    for _ in 0..50 {
        m.dec_active_conn();
    }
    assert_eq!(
        m.active_connections.load(Ordering::Relaxed),
        0,
        "瞬間値はもう 0"
    );
    assert_eq!(m.take_active_peak(), 50, "山は残る");
    // (b) 読んだあとは「いまの本数」 (この時点では 0)
    assert_eq!(m.take_active_peak(), 0);

    // (c) 張ったままの 3 本は次の区間でも山として数える
    for _ in 0..3 {
        m.inc_active_conn();
    }
    assert_eq!(m.take_active_peak(), 3);
    assert_eq!(m.take_active_peak(), 3, "0 ではなくいまの本数に戻る");
    // 山より低い出入りでは動かない
    m.dec_active_conn();
    m.inc_active_conn();
    assert_eq!(m.take_active_peak(), 3);

    // (d) 「今の本数を読む」と「山を書き戻す」の間に 1 本増えた状態を手で作る
    // (`swap` が読んだときの本数で上書きするので、山が今の本数より低くなる)。
    // 戻り値は今の本数で下から押さえるので、張ったままの接続は消えない
    m.active_peak.store(1, Ordering::Relaxed);
    assert_eq!(m.active_connections.load(Ordering::Relaxed), 3);
    assert_eq!(
        m.take_active_peak(),
        3,
        "競合で山が低く書き戻されても、張っている本数より下には出ない"
    );
}

/// 版は上の層から渡ったものがそのまま `/status` に出ること (T12.6)。
#[test]
fn the_version_from_the_upper_layer_lands_in_the_status_json() {
    let m = Metrics::new();
    let json = m.to_json_with_cache(
        None,
        StatusExtras {
            version: "0.1.0+deadbee",
            ..StatusExtras::default()
        },
    );
    assert!(json.contains("\"version\":\"0.1.0+deadbee\""), "{}", json);
    // 渡されなければ "unknown" (git の無い環境でビルドしたときと同じ見え方)
    assert!(
        m.to_json().contains("\"version\":\"unknown\""),
        "{}",
        m.to_json()
    );
}

/// 上限といまのスレッド数は上の層から渡ったものがそのまま出ること (T10.7)。
#[test]
fn capacity_from_the_upper_layer_lands_in_the_status_json() {
    let m = Metrics::new();
    let json = m.to_json_with_cache(
        None,
        StatusExtras {
            concurrency: Concurrency {
                max_conns: 48,
                max_threads: 256,
                live_threads: 7,
                idle_threads: 3,
                queued_jobs: 2,
            },
            ..StatusExtras::default()
        },
    );
    assert!(json.contains("\"max_conns\":48"), "{}", json);
    assert!(
        json.contains(
            "\"live_threads\":7,\"idle_threads\":3,\"queued_jobs\":2,\"max_threads\":256"
        ),
        "{}",
        json
    );
}

#[test]
fn clients_and_blocked_are_counted() {
    let m = Metrics::new();
    m.record_client(
        "10.0.0.1",
        HostOutcome::Hit,
        100,
        // 向き別 (T14.26)。ここでは合計と辻褄が合う値にしてある
        (30, 70),
        Some(Duration::from_millis(30)),
        Some("http://a.example:80"),
    );
    m.record_client(
        "10.0.0.1",
        HostOutcome::Blocked,
        0,
        (0, 0),
        None,
        Some("ads.example:443"),
    );
    m.record_client("10.0.0.2", HostOutcome::Bypass, 5, (5, 0), None, None);
    m.record_host("blocked://ads.example", HostOutcome::Blocked, 0);
    let clients = m.clients_sorted();
    assert_eq!(clients[0].0, "10.0.0.1");
    assert_eq!(clients[0].1.requests, 2);
    assert_eq!(clients[0].1.blocked, 1);
    assert_eq!(clients[0].1.timed, 1);
    assert!(clients[0].1.last_seen > 0);
    let enc = clients[0].1.encode("10.0.0.1");
    assert!(enc.len() <= crate::rrd::STATS_RECORD - 4);
    let (name, back) = HostStats::decode(&enc).unwrap();
    assert_eq!(name, "10.0.0.1");
    assert_eq!(back, clients[0].1);
    let json = m.to_json();
    assert!(json.contains("\"clients\":[{\"client\":\"10.0.0.1\",\"requests\":2"));
    assert!(json.contains("\"blocked\":1"));
    assert!(json.contains("\"host\":\"blocked://ads.example\",\"requests\":1,\"hits\":0,\"misses\":0,\"bypass\":0,\"errors\":0,\"blocked\":1"));
}

/// カーネルの RTT の 4 欄が `.rrd` の余白に入り、**古いファイルは 0 で読み戻る**こと (T14.5)。
#[test]
fn the_rtt_columns_fit_in_the_slot_and_old_records_read_back_as_zero() {
    let m = Metrics::new();
    m.record_host("connect://a:443", HostOutcome::Bypass, 10);
    m.record_client("10.0.0.1", HostOutcome::Bypass, 10, (0, 0), None, None);
    // 標本が無い間は「無い」(`/hosts` では `null`)
    let none = m.hosts_sorted()[0].1.clone();
    assert_eq!(none.rtt_samples, 0);
    assert_eq!(none.rtt_avg_ms(), None);
    assert!(stats_json(&none, false).contains("\"rtt_ms\":null,\"retrans\":0"));

    // 接続の終わりに 2 本ぶん (30.1 ms と 20.0 ms)
    m.record_host_rtt("connect://a:443", 30_100, 2);
    m.record_host_rtt("connect://a:443", 20_000, 0);
    // 知らないホストは作らない (数えられていない相手の行が生えない)
    m.record_host_rtt("connect://never-seen:443", 1_000, 0);
    // 読めなかった側 (0) は標本にしない
    m.record_client_rtt("10.0.0.1", 0, 0);
    m.record_client_rtt("10.0.0.1", 48_300, 0);
    assert_eq!(m.hosts_sorted().len(), 1, "行は増えない");

    let s = m.hosts_sorted()[0].1.clone();
    assert_eq!(s.rtt_samples, 2);
    assert_eq!(s.rtt_us_sum, 50_100);
    assert_eq!(s.rtt_us_min, 20_000);
    assert_eq!(s.retrans, 2);
    assert_eq!(s.rtt_avg_ms(), Some(25.05));
    assert_eq!(s.rtt_min_ms(), Some(20.0));
    assert!(
        stats_json(&s, false)
            .contains("\"rtt_ms\":{\"avg\":25.050,\"min\":20.000,\"samples\":2},\"retrans\":2")
    );
    // 全体の合計 (`/metrics`)。添字は [クライアント側, オリジン側]
    let totals = m.rtt_totals();
    assert_eq!(totals[crate::recent::CLIENT_SIDE], (48_300, 1));
    assert_eq!(totals[crate::recent::ORIGIN_SIDE], (50_100, 2));

    // `.rrd` の 1 スロット: 名前 128 B + 57 項目 × 8 B = 584 B (T25.0 の 2 欄まで)。
    // 版 3 (T14.14) でペイロードが 636 B になったので**余白は 52 B**
    // (T14.26 の時点では 68 B)。欄を足すとここが減る: 48 B を割ったら
    // 「予備を使い切りかけている」ので、版を上げる算段をすること
    let enc = s.encode("connect://a:443");
    assert_eq!(enc.len(), 128 + 57 * 8);
    let spare = crate::rrd::STATS_RECORD - 4 - enc.len();
    assert!(
        (16..=52).contains(&spare),
        "スロットの余白が {} B (版 3 で足した 64 B を使い切りかけている)",
        spare
    );
    assert_eq!(HostStats::decode(&enc).unwrap().1, s);

    // T14.5 より前に書かれたレコード (末尾 8 欄が無い) は 0 で読み戻る
    let old = &enc[..enc.len() - 64];
    let (name, back) = HostStats::decode(old).unwrap();
    assert_eq!(name, "connect://a:443");
    assert_eq!(
        (
            back.rtt_us_sum,
            back.rtt_us_min,
            back.rtt_samples,
            back.retrans
        ),
        (0, 0, 0, 0)
    );
    assert_eq!(back.requests, s.requests, "前の欄はそのまま読める");
}

/// 向き別のバイトが `.rrd` の残りの余白に入り、**古いファイルは 0 で読み戻る**こと (T14.26)。
///
/// `bytes` (合計) は今までどおりで、新しいのは「その内訳」だけ。
#[test]
fn the_direction_columns_fit_in_the_slot_and_old_records_read_back_as_zero() {
    let m = Metrics::new();
    // CONNECT 1 本: 1 KiB 上げて 2 KiB 下ろした (合計 3 KiB)
    m.record_host_detail(
        "connect://a:443",
        HostOutcome::Bypass,
        3072,
        Some(Duration::from_millis(12)),
        Detail {
            bytes_in: 1024,
            bytes_out: 2048,
            ..Detail::default()
        },
    );
    m.record_client(
        "10.0.0.1",
        HostOutcome::Bypass,
        3072,
        (1024, 2048),
        None,
        None,
    );
    // もう 1 本 (足し込まれること)
    m.record_host_detail(
        "connect://a:443",
        HostOutcome::Bypass,
        30,
        None,
        Detail {
            bytes_in: 10,
            bytes_out: 20,
            ..Detail::default()
        },
    );

    let s = m.hosts_sorted()[0].1.clone();
    assert_eq!(s.bytes, 3102, "合計は今までどおり");
    assert_eq!(s.bytes_in, 1034);
    assert_eq!(s.bytes_out, 2068);
    assert_eq!(
        s.bytes_in + s.bytes_out,
        s.bytes,
        "CONNECT は合計と一致する"
    );
    assert!(
        stats_json(&s, false).contains("\"bytes\":3102,\"bytes_in\":1034,\"bytes_out\":2068"),
        "{}",
        stats_json(&s, false)
    );
    // 接続元別にも同じ 2 欄が出る (`/status` の `clients[]` と `/clients`)
    let c = m.clients_sorted_by(ClientSort::Requests).remove(0).1;
    assert_eq!((c.stats.bytes_in, c.stats.bytes_out), (1024, 2048));
    assert!(
        c.to_json("10.0.0.1")
            .contains("\"bytes_in\":1024,\"bytes_out\":2048"),
        "{}",
        c.to_json("10.0.0.1")
    );

    // `.rrd` の 1 スロット: 名前 128 B + 57 項目 × 8 B = 584 B。T14.14 の版 3 (640 B) で
    // 予備 68 B になり、T25.0 の 2 欄 (DNS の内訳) で 52 B になった
    let enc = s.encode("connect://a:443");
    assert_eq!(enc.len(), 128 + 57 * 8);
    assert_eq!(crate::rrd::STATS_RECORD - 4 - enc.len(), 52, "残りの余白");
    assert_eq!(HostStats::decode(&enc).unwrap().1, s);

    // T14.26 より前に書かれたレコード (末尾 4 欄 = 向き別のバイトと T25.0 の
    // DNS の内訳が無い) は 0 で読み戻り、それより前の欄 (T14.5 の RTT も含めて)
    // はそのまま読める
    let (name, back) = HostStats::decode(&enc[..enc.len() - 32]).unwrap();
    assert_eq!(name, "connect://a:443");
    assert_eq!((back.bytes_in, back.bytes_out), (0, 0));
    assert_eq!(back.dns_by_cause, [0, 0], "古い行は内訳が分からない");
    assert_eq!(back.bytes, s.bytes, "合計は昔のファイルにも入っている");
    assert_eq!(back.requests, s.requests);
}

#[test]
fn test_host_stats() {
    let m = Metrics::new();
    m.record_host(
        "http://a:80",
        HostOutcome::from_access("HIT(memory) age=1s", 200),
        10,
    );
    m.record_host(
        "http://a:80",
        HostOutcome::from_access("MISS stored ttl=1s", 200),
        20,
    );
    m.record_host("http://b:80", HostOutcome::from_access("BYPASS", 200), 5);
    m.record_host("http://b:80", HostOutcome::from_access("MISS", 502), 0);
    let hosts = m.hosts_sorted();
    assert_eq!(hosts[0].0, "http://a:80");
    assert!(hosts[0].1.last_seen > 0);
    assert_eq!(
        HostStats {
            last_seen: 0,
            ..hosts[0].1.clone()
        },
        HostStats {
            requests: 2,
            hits: 1,
            misses: 1,
            bypass: 0,
            errors: 0,
            bytes: 30,
            ..Default::default()
        }
    );
    assert_eq!(hosts[1].1.errors, 1);
    assert_eq!(hosts[1].1.bypass, 1);
    let json = m.to_json();
    assert!(
        json.contains("\"hosts\":[{\"host\":\"http://a:80\",\"requests\":2"),
        "{}",
        json
    );
    for i in 0..(MAX_HOSTS + 5) {
        m.record_host(&format!("http://h{}:80", i), HostOutcome::Hit, 1);
    }
    let hosts = m.hosts_sorted();
    assert!(hosts.len() <= MAX_HOSTS + 1);
    assert!(hosts.iter().any(|(h, _)| h == "other"));
}
