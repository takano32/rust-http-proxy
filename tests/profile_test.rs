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
fn close_window(metrics: &rust_http_proxy::metrics::Metrics, t: u64, requests: u64, cpu_us: u64) {
    metrics.profile.push(profile::Sample {
        t,
        requests,
        cpu_us,
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
        "\"keys\":[\"t\",\"requests\",\"cpu_us\",\"connect\",\"forward\",\"threads\",\"locks\",\"queue\"]",
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
        t.samples = 5;
        for (k, c) in t.states.iter_mut().enumerate() {
            *c = 10_000 + k as u32;
        }
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
