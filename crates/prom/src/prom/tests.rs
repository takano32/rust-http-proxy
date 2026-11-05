//! `/metrics` の単体試験 (T34.27 で `prom.rs` から移した。名前はそのまま)。

use super::*;
use crate::metrics::HostOutcome;

#[test]
fn renders_counters_and_escaped_labels() {
    let m = Metrics::new();
    m.inc_requests();
    m.record_host("http://a\"b:80", HostOutcome::Hit, 10);
    let text = render(&m, None, Concurrency::default());
    assert!(text.contains("# TYPE sorahost_requests_total counter\nsorahost_requests_total 1\n"));
    assert!(
        text.contains("sorahost_host_hits_total{host=\"http://a\\\"b:80\"} 1\n"),
        "{}",
        text
    );
    assert!(
        !text.contains("sorahost_cache_hits_total"),
        "no cache section without a cache"
    );
    assert_eq!(escape("x\\y\n"), "x\\\\y\\n");
}

/// 上限といまのスレッド数が gauge として出ること (T11.5)。
/// `/status` に出しているのと同じ 5 つで、値は上の層から渡ったものがそのまま出る。
#[test]
fn renders_the_limits_and_the_thread_counts_as_gauges() {
    let m = Metrics::new();
    let text = render(
        &m,
        None,
        Concurrency {
            max_conns: 137,
            max_threads: 41,
            live_threads: 7,
            idle_threads: 3,
            queued_jobs: 2,
        },
    );
    for (name, value) in [
        ("max_connections", 137),
        ("max_threads", 41),
        ("live_threads", 7),
        ("idle_threads", 3),
        ("queued_jobs", 2),
    ] {
        assert!(
            text.contains(&format!("# TYPE sorahost_{} gauge\n", name)),
            "{} の TYPE が無い:\n{}",
            name,
            text
        );
        assert!(
            text.contains(&format!("sorahost_{} {}\n", name, value)),
            "{} の値が違う:\n{}",
            name,
            text
        );
    }
    // 渡されなければ 0 (= 無制限・数えていない)
    let zeroed = render(&m, None, Concurrency::default());
    assert!(
        zeroed.contains("sorahost_max_connections 0\n"),
        "{}",
        zeroed
    );
}

/// 接続スレッドの数が新旧両方の名前で出ること (T12.4 (4))。旧名は 1 版だけ残す。
#[test]
fn thread_gauges_are_rendered_under_both_the_old_and_the_new_name() {
    let m = Metrics::new();
    let text = render(
        &m,
        None,
        Concurrency {
            live_threads: 7,
            idle_threads: 3,
            ..Concurrency::default()
        },
    );
    for pat in [
        "sorahost_threads{state=\"live\"} 7\n",
        "sorahost_threads{state=\"idle\"} 3\n",
        "sorahost_live_threads 7\n",
        "sorahost_idle_threads 3\n",
    ] {
        assert!(text.contains(pat), "{} が無い:\n{}", pat, text);
    }
}

/// CONNECT 確立のヒストグラム・名前解決・エラーの原因が出ること (T12.4 (4))。
#[test]
fn renders_the_connect_histogram_the_dns_summary_and_the_error_causes() {
    use std::time::Duration;
    let m = Metrics::new();
    m.record_host_timed(
        "connect://a:443",
        HostOutcome::Bypass,
        0,
        Duration::from_millis(257),
    );
    // 繋がらなかった 1 本 (30 秒で `timeout`)。**確立のヒストグラムには入らない**
    // (T20.4。`errors_total` には残る)
    m.record_host_detail(
        "connect://b:443",
        HostOutcome::Error,
        0,
        Some(Duration::from_millis(30_000)),
        crate::metrics::Detail {
            cause: Some(crate::metrics::ErrCause::Timeout),
            ..crate::metrics::Detail::default()
        },
    );
    let text = render(&m, None, Concurrency::default());
    assert!(
        text.contains("# TYPE sorahost_connect_seconds histogram\n"),
        "{}",
        text
    );
    // 12 段 + `+Inf` で、累積になっていること
    assert!(
        text.contains("sorahost_connect_seconds_bucket{le=\"0.25\"} 0\n"),
        "{}",
        text
    );
    assert!(
        text.contains("sorahost_connect_seconds_bucket{le=\"0.5\"} 1\n"),
        "{}",
        text
    );
    assert!(
        text.contains("sorahost_connect_seconds_bucket{le=\"+Inf\"} 1\n"),
        "{}",
        text
    );
    assert!(
        text.contains("sorahost_connect_seconds_count 1\n"),
        "{}",
        text
    );
    assert!(
        text.contains("sorahost_connect_seconds_sum 0.257\n"),
        "{}",
        text
    );
    assert!(text.contains("sorahost_dns_seconds_count "), "{}", text);
    assert!(
        text.contains("sorahost_errors_total{cause=\"timeout\"} 1\n"),
        "{}",
        text
    );
    assert!(
        text.contains("sorahost_errors_total{cause=\"loop\"} 0\n"),
        "{}",
        text
    );
}

/// DNS の失敗の内訳が `errors_total` の**うしろ**に出ること (T26.1)。
///
/// - 先頭 8 つの並びは今までどおりで、内訳は `cause="other"` の後ろ
/// - 値はホスト別の内訳の**合計** (2 ホストに 1 件ずつ入れて 2 になる)
/// - **0 のときは行ごと出さない** (既存の `host_blocked_total` と同じ約束)
#[test]
fn renders_the_dns_breakdown_after_the_eight_causes() {
    use crate::metrics::{Detail, DnsCause, ErrCause};
    let m = Metrics::new();
    for (host, dns) in [
        ("connect://a:443", DnsCause::Nxdomain),
        ("connect://b:443", DnsCause::Nxdomain),
    ] {
        m.record_host_detail(
            host,
            HostOutcome::Error,
            0,
            Some(std::time::Duration::from_millis(1)),
            Detail {
                cause: Some(ErrCause::Dns),
                dns_cause: Some(dns),
                ..Detail::default()
            },
        );
    }
    let text = render(&m, None, Concurrency::default());
    // `dns` は合計のまま 2、内訳がその後ろに 2
    assert!(
        text.contains("sorahost_errors_total{cause=\"dns\"} 2\n"),
        "{}",
        text
    );
    assert!(
        text.contains("sorahost_errors_total{cause=\"nxdomain\"} 2\n"),
        "{}",
        text
    );
    assert!(
        !text.contains("cause=\"dns_temp\""),
        "0 の内訳は行ごと出さない: {}",
        text
    );
    // 並び: 8 つの最後 (`other`) → `nxdomain` → `dns_temp` (出るとき)
    let other = text
        .lines()
        .position(|l| l == "sorahost_errors_total{cause=\"other\"} 0")
        .expect("`other` が無い");
    let nx = text
        .lines()
        .position(|l| l == "sorahost_errors_total{cause=\"nxdomain\"} 2")
        .expect("`nxdomain` が無い");
    assert_eq!(nx, other + 1, "内訳は 8 つの**すぐ後ろ**:\n{}", text);
    // `/status` の側 (ホスト別の `errors_by_cause` の 9 番目) と同じ値
    let hosts = m.hosts_sorted();
    assert_eq!(hosts.len(), 2);
    let per_host: u64 = hosts.iter().map(|(_, s)| s.dns_by_cause[0]).sum();
    assert_eq!(per_host, 2, "ホスト別は 1 件ずつ:\n{}", text);
    assert_eq!(m.dns_cause_totals(), [2, 0]);

    // 続けて `dns_temp` も 1 件 → 2 本そろって 8 つの後ろに並ぶ
    m.record_host_detail(
        "connect://c:443",
        HostOutcome::Error,
        0,
        Some(std::time::Duration::from_millis(1)),
        Detail {
            cause: Some(ErrCause::Dns),
            dns_cause: Some(DnsCause::DnsTemp),
            ..Detail::default()
        },
    );
    let text = render(&m, None, Concurrency::default());
    let nx = text
        .lines()
        .position(|l| l == "sorahost_errors_total{cause=\"nxdomain\"} 2")
        .expect("`nxdomain` が無い");
    assert!(
        text.lines()
            .nth(nx + 1)
            .is_some_and(|l| l == "sorahost_errors_total{cause=\"dns_temp\"} 1"),
        "`nxdomain` の次が `dns_temp` でない:\n{}",
        text
    );
    assert_eq!(m.dns_cause_totals(), [2, 1]);
}

/// 上限まで埋めても `/status` 64 KiB / `/metrics` 400 KiB に収まること (T12.4 (3))。
#[test]
fn the_endpoints_stay_within_their_size_budget_when_the_tables_are_full() {
    let m = Metrics::new();
    for i in 0..crate::metrics::MAX_HOSTS {
        // 長めのホスト名 + 全部の区間に件数が入っている最悪の形
        let host = format!("connect://very-long-host-name-{:04}.example.com:443", i);
        for ms in [1, 5, 30, 120, 400, 900, 3000, 12_000] {
            m.record_host_timed(
                &host,
                HostOutcome::Bypass,
                1 << 30,
                std::time::Duration::from_millis(ms),
            );
        }
    }
    for i in 0..crate::metrics::MAX_CLIENTS {
        m.record_client(
            &format!("2001:db8:1234:5678:9abc:def0:1234:{:04x}", i),
            HostOutcome::Bypass,
            1 << 30,
            (1 << 20, 1 << 30),
            Some(std::time::Duration::from_millis(400)),
            Some("very-long-host-name.example.com:443"),
        );
    }
    let status = m.to_json();
    assert!(status.len() <= 64 * 1024, "/status が {} B", status.len());
    let before = render(&m, None, Concurrency::default()).len();
    // 段階の 13 本が全部の区間に埋まった最悪 (T14.19)
    let mut stages = crate::profile::Stages::default();
    for ms in [
        0, 1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 30_000,
    ] {
        let d = crate::metrics::Detail {
            dns_ms: ms,
            connect_ms: ms,
            first_byte_ms: Some(ms),
            stages: crate::metrics::StageMs {
                queue: ms as u32,
                client_read: ms as u32,
                first_relay: ms as u32,
                relay: ms as u32,
                park: ms as u32,
                send: ms as u32,
                body: ms as u32,
            },
            ..crate::metrics::Detail::default()
        };
        stages.observe_connect(&d);
        stages.observe_forward(&d);
    }
    m.profile.push(crate::profile::Sample {
        t: 1,
        stages,
        ..crate::profile::Sample::default()
    });
    let text = render(&m, None, Concurrency::default());
    assert!(text.len() <= 400 * 1024, "/metrics が {} B", text.len());
    // T14.19 の増分 (段階 + keep-warm + ロック) は 20 KiB を超えない
    assert!(
        text.len() - before <= 20 * 1024,
        "段階の増分が {} B (表が満杯のとき /metrics は {} B)",
        text.len() - before,
        text.len()
    );
}

/// 段階の窓を 1 標本ぶん積む (T14.19 のテスト用。CONNECT 1 本 + forward 1 要求)。
fn push_one_sample(m: &Metrics) {
    use crate::metrics::StageMs;
    let mut stages = crate::profile::Stages::default();
    stages.observe_connect(&crate::metrics::Detail {
        dns_ms: 30,
        connect_ms: 4,
        stages: StageMs {
            // `queue` と `park` は 0 ms = 速い道 (読むときに足し戻される)
            client_read: 1,
            first_relay: 120,
            relay: 900,
            ..StageMs::default()
        },
        ..crate::metrics::Detail::default()
    });
    stages.observe_forward(&crate::metrics::Detail {
        first_byte_ms: Some(60),
        stages: StageMs {
            queue: 2,
            client_read: 1,
            body: 7,
            ..StageMs::default()
        },
        ..crate::metrics::Detail::default()
    });
    m.profile.push(crate::profile::Sample {
        t: 1,
        stages,
        ..crate::profile::Sample::default()
    });
}

/// 段階・keep-warm・ロックの取り合いが `/metrics` に出ること (T14.19)。
/// 段階は **12 段 + `+Inf` の累積**、値は秒、累計は起動から。
#[test]
fn renders_the_stage_histograms_the_warm_gauge_and_the_lock_counters() {
    let m = Metrics::new();
    push_one_sample(&m);
    let text = render(&m, None, Concurrency::default());
    assert_eq!(
        text.matches("# TYPE sorahost_stage_seconds histogram\n")
            .count(),
        1,
        "HELP / TYPE は族に 1 組だけ:\n{}",
        text
    );
    for pat in [
        // 0 ms の観測は `≤ 1 ms` の区間に足し戻る (`queue` / `park`)
        "sorahost_stage_seconds_bucket{kind=\"connect\",stage=\"queue\",le=\"0.001\"} 1\n",
        "sorahost_stage_seconds_sum{kind=\"connect\",stage=\"queue\"} 0\n",
        "sorahost_stage_seconds_count{kind=\"connect\",stage=\"queue\"} 1\n",
        // 名前解決 30 ms → `≤ 25 ms` には入らず `≤ 50 ms` に入る
        "sorahost_stage_seconds_bucket{kind=\"connect\",stage=\"dns\",le=\"0.025\"} 0\n",
        "sorahost_stage_seconds_bucket{kind=\"connect\",stage=\"dns\",le=\"0.05\"} 1\n",
        "sorahost_stage_seconds_bucket{kind=\"connect\",stage=\"dns\",le=\"+Inf\"} 1\n",
        "sorahost_stage_seconds_sum{kind=\"connect\",stage=\"dns\"} 0.03\n",
        "sorahost_stage_seconds_count{kind=\"connect\",stage=\"dns\"} 1\n",
        // トンネル越しの TLS 握手 120 ms と中継 900 ms
        "sorahost_stage_seconds_sum{kind=\"connect\",stage=\"first_relay\"} 0.12\n",
        "sorahost_stage_seconds_sum{kind=\"connect\",stage=\"relay\"} 0.9\n",
        // forward の初バイト 60 ms は `≤ 100 ms`
        "sorahost_stage_seconds_bucket{kind=\"forward\",stage=\"ttfb\",le=\"0.05\"} 0\n",
        "sorahost_stage_seconds_bucket{kind=\"forward\",stage=\"ttfb\",le=\"0.1\"} 1\n",
        "sorahost_stage_seconds_sum{kind=\"forward\",stage=\"body\"} 0.007\n",
        // keep-warm な名前の数 (T14.1) と ロックの取り合い (T14.3 (3))
        "# TYPE sorahost_warm_names gauge\nsorahost_warm_names ",
        "# TYPE sorahost_lock_contention_total counter\n",
        "sorahost_lock_contention_total{lock=\"stats\"} ",
        "sorahost_lock_contention_total{lock=\"workers\"} ",
    ] {
        assert!(text.contains(pat), "{} が無い:\n{}", pat, text);
    }
    // 13 段 (CONNECT 7 + forward 6) × (12 段 + `+Inf`) と `_sum` / `_count`
    let stages = crate::profile::CONNECT_STAGES.len() + crate::profile::FORWARD_STAGES.len();
    assert_eq!(stages, 13);
    assert_eq!(
        text.matches("sorahost_stage_seconds_bucket{").count(),
        stages * (crate::history::WINDOW_BOUNDS_MS.len() + 1)
    );
    assert_eq!(text.matches("sorahost_stage_seconds_sum{").count(), stages);
    assert_eq!(
        text.matches("sorahost_stage_seconds_count{").count(),
        stages
    );
    assert_eq!(
        text.matches("sorahost_lock_contention_total{").count(),
        crate::sync::LOCK_NAMES.len()
    );
}

/// **1 本も観測していないうちは段階の系列を 1 本も出さない** (T14.19)。
/// `--lite` は段階の時計を読まないので、ずっとこの形 (`kernel` と同じ判断)。
#[test]
fn the_stage_series_are_absent_until_the_first_observation() {
    let m = Metrics::new();
    let text = render(&m, None, Concurrency::default());
    assert!(!text.contains("sorahost_stage_seconds"), "{}", text);
    // 段階に依らないもの (keep-warm とロック) は `--lite` でも出る
    assert!(text.contains("sorahost_warm_names "), "{}", text);
    assert!(
        text.contains("sorahost_lock_contention_total{lock=\"dns\"} "),
        "{}",
        text
    );
}

/// 「合流を飛ばす鍵」の記憶を入れ替えた回数が `/metrics` にも出ること (T12.6)。
#[test]
fn renders_the_not_stored_rotations_counter() {
    let m = Metrics::new();
    let cache = Cache::new(crate::cache::CacheConfig::disabled());
    let text = render(&m, Some(&cache), Concurrency::default());
    assert!(
        text.contains(
            "# TYPE sorahost_cache_not_stored_rotations_total counter\nsorahost_cache_not_stored_rotations_total 0\n"
        ),
        "{}",
        text
    );
}

/// 版とビルドの種類が `sorahost_build_info{version,build} 1` の 1 行で出ること (T29.19)。
#[test]
fn renders_the_build_info_with_two_labels() {
    let mut out = String::new();
    build_info(&mut out, "0.29.35+b2b26c1", "release");
    assert!(
        out.contains("# TYPE sorahost_build_info gauge\n"),
        "{}",
        out
    );
    assert!(
        out.contains("sorahost_build_info{version=\"0.29.35+b2b26c1\",build=\"release\"} 1\n"),
        "{}",
        out
    );
    let mut quoted = String::new();
    build_info(&mut quoted, "a\"b", "c");
    assert!(quoted.contains("version=\"a\\\"b\""), "{}", quoted);
}

/// 起動の id と heap の基準が `/metrics` に出ること (T25.3)。
///
/// **出せないときは行ごと出さない** (`sorahost_kernel_*` と同じ判断)。0 や空の `id` を
/// 出すと「起動が変わっていない」「増えが 0」と読めてしまう。
#[test]
fn renders_the_boot_id_and_the_heap_baseline() {
    // info 型: 値は常に 1、ラベルは `id` の 1 つだけ (系列は増えない)
    let mut out = String::new();
    boot_id_info(&mut out, Some("0f1e2d3c-aaaa-bbbb-cccc-1234567890ab"));
    assert!(
        out.contains("# TYPE sorahost_boot_id_info gauge\n"),
        "{}",
        out
    );
    assert!(
        out.contains("sorahost_boot_id_info{id=\"0f1e2d3c-aaaa-bbbb-cccc-1234567890ab\"} 1\n"),
        "{}",
        out
    );
    // ラベルは `escape` を通す (id に `"` が混ざっても形が壊れない)
    let mut quoted = String::new();
    boot_id_info(&mut quoted, Some("a\"b"));
    assert!(
        quoted.contains("sorahost_boot_id_info{id=\"a\\\"b\"} 1\n"),
        "{}",
        quoted
    );
    let mut none = String::new();
    boot_id_info(&mut none, None);
    assert_eq!(none, "", "読めない環境では系列ごと出さない");

    // 基準: バイトと、それを取った epoch 秒
    let mut heap = String::new();
    heap_baseline(&mut heap, Some((1_767_225_600, 15_572_256)));
    assert!(
        heap.contains(
            "# TYPE sorahost_heap_baseline_bytes gauge\nsorahost_heap_baseline_bytes 15572256\n"
        ),
        "{}",
        heap
    );
    assert!(
        heap.contains(
            "# TYPE sorahost_heap_baseline_at_seconds gauge\nsorahost_heap_baseline_at_seconds 1767225600\n"
        ),
        "{}",
        heap
    );
    let mut no_base = String::new();
    heap_baseline(&mut no_base, None);
    assert_eq!(no_base, "", "まだ 60 秒たっていない起動では出さない");
}
