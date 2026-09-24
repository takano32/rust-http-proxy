//! `/profile` の結合テスト (T14.3 (4))。
//!
//! 見るのは「実際に通した要求の段階が、その形で `/profile` に出てくるか」だけ。
//! スレッドの標本 (`profile-sample` スレッド) はここでは起こさないので `sampler` は
//! `off` になる — その道も一緒に確かめる。

mod common;

use std::net::TcpStream;
use std::time::Duration;

use common::*;
use rust_http_proxy::profile;

/// 窓を 1 つ閉じる (本番の `profile-sample` スレッドが 5 秒ごとにするのと同じこと)。
/// ユーザー空間は CPU の 3 割にしておく (T16.0 の `cpu_user_us` が畳まれるのを見るため。
/// デプロイ先の実測もカーネル側が約 7 割)。
fn close_window(metrics: &rust_http_proxy::metrics::Metrics, t: u64, requests: u64, cpu_us: u64) {
    metrics.profile.push(profile::Sample {
        t,
        requests,
        cpu_us,
        cpu_user_us: cpu_us * 3 / 10,
        stages: metrics.take_stages(),
        ..profile::Sample::default()
    });
}

/// `/profile` の骨格 (キー・段階の名前・役割・状態・ロックの名前) が出ること。
#[test]
fn test_integration_profile_has_the_shape_the_dashboard_reads() {
    profile::set_enabled(true);
    let (port, _metrics) = start_test_proxy_with_metrics(proxy_config());
    let json = endpoint_json(port, "/profile");
    for key in [
        "\"interval_secs\":5",
        "\"sample_ms\":",
        "\"sampler\":\"off\"",
        "\"bounds_ms\":[1,2,5,10,25,50,100,250,500,1000,2500,5000]",
        // 新しい欄は**末尾**に足す (画面は位置で開くので、古い読み手はそのまま動く。T15.0 (5))
        // T16.0 は `user_us` (役割ごと) と `cpu_user_us` (プロセス全体) を末尾に足した
        "\"keys\":[\"t\",\"requests\",\"cpu_us\",\"connect\",\"forward\",\"threads\",\"locks\",\"queue\",\"threads_top\",\"run_delay_us\",\"user_us\",\"cpu_user_us\"]",
        "\"lock_names\":[\"stats\",\"dns\",\"park\",\"workers\"]",
        "\"locks_total\":[",
        "\"queue_total\":[",
        "\"recent\":{",
        "\"truncated\":false",
    ] {
        assert!(json.contains(key), "{} が無い: {}", key, json);
    }
    // 段階の名前 (CONNECT 7 段 / forward 6 段)
    assert!(
        json.contains(
            "\"stages\":{\"connect\":[\"queue\",\"client_read\",\"dns\",\"connect\",\"first_relay\",\"relay\",\"park\"],\"forward\":[\"queue\",\"client_read\",\"origin\",\"send\",\"ttfb\",\"body\"]}"
        ),
        "{}",
        json
    );
    // 役割 9 つと状態 22 枠
    assert!(json.contains("\"roles\":[\"accept\",\"conn\","), "{}", json);
    assert!(
        json.contains("\"states\":[\"running\",\"recvfrom\",\"sendto\",\"ppoll\","),
        "{}",
        json
    );
    assert!(json.contains("\"sleeping\",\"other\"]"), "{}", json);
    // まだ窓が閉じていないので標本は 0
    assert!(json.contains("\"samples\":[]"), "{}", json);
    assert!(json.contains("\"count\":0"), "{}", json);
}

/// 転送した要求が forward の 6 段に、CONNECT が 7 段に 1 件ずつ乗ること。
#[test]
fn test_integration_profile_counts_the_stages_of_real_requests() {
    profile::set_enabled(true);
    let (origin_port, _origin) = start_mock_origin();
    let echo_port = start_echo_server();
    let (port, metrics) = start_test_proxy_with_metrics(proxy_config());

    // 転送 1 要求
    let body = get_via_proxy(
        port,
        &format!("http://127.0.0.1:{}/hello", origin_port),
        &format!("127.0.0.1:{}", origin_port),
    );
    assert!(body.contains("HTTP/1.1 200"), "{}", body);

    // CONNECT 1 本 (開いて 1 バイト通してから閉じる)
    let target = format!("127.0.0.1:{}", echo_port);
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let head = format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", target, target);
    use std::io::{Read as _, Write as _};
    s.write_all(head.as_bytes()).unwrap();
    assert!(read_connect_response(&mut s).starts_with("HTTP/1.1 200"));
    s.write_all(b"x").unwrap();
    let mut one = [0u8; 1];
    s.read_exact(&mut one).unwrap();
    drop(s);
    // トンネルの統計は接続が閉じてから出る
    wait_until(
        || status_json(port).contains("connect://"),
        "CONNECT の統計が出る",
    );

    close_window(&metrics, 1_800_000_000, 2, 12_345);
    let json = endpoint_json(port, "/profile");
    let sample = json
        .split("\"samples\":[")
        .nth(1)
        .and_then(|s| s.split("],\"locks_total\"").next())
        .expect("標本があること")
        .to_string();
    assert!(sample.starts_with("[1800000000,2,12345,["), "{}", sample);
    // CONNECT の 7 段と forward の 6 段が、どちらも 1 件ずつ観測されていること
    // (件数 0 の段階は `0` 1 文字で書くので、`0` が並んでいたら観測されていない)
    assert!(
        !sample.contains("[0,0,0,0,0,0,0]"),
        "CONNECT の段階が 1 つも観測されていない: {}",
        sample
    );
    assert!(
        !sample.contains("[0,0,0,0,0,0]"),
        "forward の段階が 1 つも観測されていない: {}",
        sample
    );
    assert!(json.contains("\"count\":1"), "{}", json);
    assert!(json.contains("\"shown\":1"), "{}", json);
    // 直近の要約に **CPU/要求** が出る (窓の CPU ÷ 窓の要求数)
    assert!(
        json.contains("\"cpu_per_request_us\":6172.50"),
        "CPU/要求 が合わない: {}",
        json
    );
}

/// `?res=60` は 1 分の窓を返す (知らない値は 5 秒に倒す)。
#[test]
fn test_integration_profile_resolution() {
    profile::set_enabled(true);
    let (port, _metrics) = start_test_proxy_with_metrics(proxy_config());
    assert!(
        endpoint_json(port, "/profile?res=60").contains("\"interval_secs\":60"),
        "res=60 が効いていない"
    );
    assert!(
        endpoint_json(port, "/profile?res=3600").contains("\"interval_secs\":5"),
        "知らない解像度は 5 秒に倒す"
    );
}

/// 窓が埋まっても応答は 256 KiB 以下で、入り切らない古い標本は落ちること (T14.3 (4))。
///
/// 全部の段階と全部の役割が埋まった「いちばん大きい標本」を上限まで積む
/// (デプロイ先の静かな窓は `0` 1 文字なのでずっと小さい)。
#[test]
fn test_integration_profile_stays_under_256_kib() {
    profile::set_enabled(true);
    let (port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let mut stages = profile::Stages::default();
    let d = rust_http_proxy::metrics::Detail {
        dns_ms: 7,
        connect_ms: 9,
        first_byte_ms: Some(20),
        stages: rust_http_proxy::metrics::StageMs {
            queue: 1,
            client_read: 2,
            first_relay: 30,
            relay: 4000,
            park: 90_000,
            send: 3,
            body: 40,
        },
        ..rust_http_proxy::metrics::Detail::default()
    };
    // どの段階も 12 段ぜんぶに値が入るようにする (区間の配列がいちばん長くなる)
    for i in 0..13u64 {
        let mut d = d;
        d.dns_ms = i * 400;
        stages.observe_connect(&d);
        stages.observe_forward(&d);
    }
    let mut threads = profile::Threads::default();
    for (i, t) in threads.iter_mut().enumerate() {
        t.cpu_us = 1_234_567 + i as u64;
        t.user_us = 1_234_000 + i as u64;
        t.samples = 5;
        for (k, c) in t.states.iter_mut().enumerate() {
            *c = 10_000 + k as u32;
        }
    }
    // 上位のスレッドも 8 本ぜんぶ埋める (名前は 15 文字 = `/proc` の `comm` の上限)
    let mut threads_top = [profile::TopThread::default(); profile::TOP_THREADS];
    for (i, t) in threads_top.iter_mut().enumerate() {
        t.tid = 4_000_000 + i as u32;
        t.role = (i % profile::ROLES.len()) as u8;
        t.cpu_us = 1_234_567 + i as u64;
        t.running = 5;
        t.comm = *b"conn-123456789\0\0";
    }
    for i in 0..profile::RESOLUTIONS[0].1 as u64 {
        metrics.profile.push(profile::Sample {
            t: 1_800_000_000 + i * 5,
            requests: 123_456,
            cpu_us: 4_567_890,
            stages,
            threads,
            locks: [11, 22, 33, 44],
            queue_waited: 5,
            queue_ms_sum: 500,
            queue_ms_max: 250,
            threads_top,
            run_delay_us: Some([9_876_543; profile::ROLES.len()]),
            cpu_user_us: 3_456_789,
        });
    }
    let json = endpoint_json(port, "/profile");
    assert!(
        json.len() <= 256 * 1024,
        "応答が 256 KiB を超えた: {} B",
        json.len()
    );
    assert!(json.contains("\"truncated\":true"), "打ち切りの印が無い");
    assert!(
        json.contains(&format!("\"count\":{}", profile::RESOLUTIONS[0].1)),
        "全体の件数が出ていない"
    );
    // 残るのは**新しい方**
    let last = 1_800_000_000 + (profile::RESOLUTIONS[0].1 as u64 - 1) * 5;
    assert!(
        json.contains(&format!("[{},123456,", last)),
        "新しい標本が無い"
    );
    assert!(
        !json.contains("[1800000000,123456,"),
        "いちばん古い標本が残っている (新しい方から詰めるはず)"
    );
}

/// `--lite` では `{"profile":"off"}` だけ (段階の時計も読んでいない)。
#[test]
fn test_integration_profile_is_off_in_lite_mode() {
    let mut lite = proxy_config();
    lite.lite = true;
    let port = start_test_proxy(lite);
    let json = endpoint_json(port, "/profile");
    // T14.49 で先頭に `"schema":1,` が付いたので、`profile` の値だけを見る
    assert!(json.contains("\"profile\":\"off\""), "{}", json);
    // 案内 (`/`) には出す (`--lite` でも口はある)
    assert!(endpoint_json(port, "/").contains("/profile"));
}

/// `?n=` と `?offset=` で **720 標本ぜんぶが 1 枚ずつ読める** (T15.0 (11))。
///
/// `/profile?res=5` は 720 標本のうち 456 しか返らない (1 標本 3,136 B で 256 KiB に
/// 入り切らない) ので、雪像 1 枚で全部読むには頁が要る。ここで縛るのは
/// 「頁を継ぐと落ちも重なりもしない」ことと、`next_offset` が最後だけ `null` になること。
#[test]
fn test_integration_profile_pages_the_samples_with_n_and_offset() {
    profile::set_enabled(true);
    let (port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let total = profile::RESOLUTIONS[0].1;
    for i in 0..total as u64 {
        close_window(&metrics, 1_800_000_000 + i * 5, 100 + i, 1_000 + i);
    }

    // `?n=` だけ: 新しい方から 10 本 (打ち切りではない = 続きは `next_offset`)
    let head = endpoint_json(port, "/profile?n=10");
    assert!(head.contains("\"shown\":10"), "{}", tail(&head));
    assert!(head.contains("\"truncated\":false"), "{}", tail(&head));
    assert!(head.contains("\"offset\":0"), "{}", tail(&head));
    assert!(head.contains("\"next_offset\":10"), "{}", tail(&head));
    let newest = 1_800_000_000 + (total as u64 - 1) * 5;
    assert!(head.contains(&format!("[{},{},", newest, 100 + total as u64 - 1)));

    // 頁を継ぐと 720 本が重複なく揃う (1 頁 180 本 × 4)
    let page = 180;
    let mut seen = std::collections::BTreeSet::new();
    let mut dup = 0usize;
    let mut offset = 0usize;
    for p in 0..(total / page) {
        let json = endpoint_json(port, &format!("/profile?n={}&offset={}", page, offset));
        assert!(
            json.contains("\"truncated\":false"),
            "{} 頁目がバイト数で切れた ({} B)",
            p + 1,
            json.len()
        );
        assert!(
            json.contains(&format!("\"offset\":{}", offset)),
            "{}",
            p + 1
        );
        for i in 0..total as u64 {
            if json.contains(&format!("[{},{},", 1_800_000_000 + i * 5, 100 + i)) && !seen.insert(i)
            {
                dup += 1;
            }
        }
        offset += page;
        let want = match offset < total {
            true => format!("\"next_offset\":{}", offset),
            false => "\"next_offset\":null".to_string(),
        };
        assert!(json.contains(&want), "{} 頁目に {} が無い", p + 1, want);
    }
    assert_eq!(dup, 0, "同じ標本が 2 つの頁に出た");
    assert_eq!(seen.len(), total, "欠けがある");

    // 環の外を指したら空の頁 (エラーにはしない)
    let past = endpoint_json(port, &format!("/profile?offset={}", total));
    assert!(past.contains("\"samples\":[]"), "{}", tail(&past));
    assert!(past.contains("\"next_offset\":null"), "{}", tail(&past));
    assert!(
        past.contains(&format!("\"count\":{}", total)),
        "{}",
        tail(&past)
    );
}

/// `?summary=1` は標本を返さず、5 分 / 1 時間 / 全部 の 3 段だけ (T15.0 (11))。
#[test]
fn test_integration_profile_summary_folds_three_spans() {
    profile::set_enabled(true);
    let (port, metrics) = start_test_proxy_with_metrics(proxy_config());
    // 5 秒 × 720 = 1 時間ぶん。1 標本 要求 10 件・CPU 1,000 us = 100 us/要求
    let total = profile::RESOLUTIONS[0].1;
    for i in 0..total as u64 {
        close_window(&metrics, 1_800_000_000 + i * 5, 10, 1_000);
    }
    let json = endpoint_json(port, "/profile?summary=1");
    assert!(json.contains("\"summary\":true"), "{}", json);
    // **標本は 1 本も返さない** (雪像に入り切らない部を出さないための口。
    // 段の中の `"samples":60` は「畳んだ標本の数」なので、配列の方だけを見る)
    assert!(!json.contains("\"samples\":["), "{}", json);
    assert!(!json.contains("\"keys\""), "{}", json);
    assert!(json.contains("\"interval_secs\":5"), "{}", json);
    assert!(json.contains(&format!("\"count\":{}", total)), "{}", json);
    assert!(
        json.contains(&format!("\"capacity\":{}", total)),
        "{}",
        json
    );
    // 5 分 = 60 標本、1 時間 = 720 標本、全部 = 環に残っている全部
    assert!(
        json.contains("{\"name\":\"5m\",\"secs\":300,\"samples\":60,\"requests\":600,\"cpu_us\":60000,\"cpu_per_request_us\":100.00,\"cpu_user_us\":18000}"),
        "{}",
        json
    );
    assert!(
        json.contains("{\"name\":\"1h\",\"secs\":3600,\"samples\":720,\"requests\":7200,\"cpu_us\":720000,\"cpu_per_request_us\":100.00,\"cpu_user_us\":216000}"),
        "{}",
        json
    );
    assert!(
        json.contains("{\"name\":\"all\",\"secs\":3600,\"samples\":720,"),
        "{}",
        json
    );

    // 窓が 1 つも閉じていなければ 0 本 (`cpu_per_request_us` は `null`)
    let (empty_port, _m) = start_test_proxy_with_metrics(proxy_config());
    let empty = endpoint_json(empty_port, "/profile?summary=1");
    assert!(empty.contains("\"count\":0"), "{}", empty);
    assert!(empty.contains("\"cpu_per_request_us\":null"), "{}", empty);
    // `--lite` は今までどおり `{"profile":"off"}` (要約でも同じ)
    let mut lite = proxy_config();
    lite.lite = true;
    let lite_port = start_test_proxy(lite);
    assert!(
        endpoint_json(lite_port, "/profile?summary=1").contains("\"profile\":\"off\""),
        "lite"
    );
}

/// 応答の末尾だけ (assert のメッセージに 256 KiB を貼らない)。
fn tail(json: &str) -> &str {
    &json[json.len().saturating_sub(240)..]
}
