//! 接続元ごとの同時接続の上限 `PROXY_MAX_CONNS_PER_CLIENT` の結合テスト (T14.13)。
//!
//! 上限に当たった接続は、T13.2 の「上限 + 4 本」の枠で受けてから要求行を読み、**自分宛て
//! (内部エンドポイント) なら普通に応答、それ以外は 503 + `Retry-After: 1`** で閉じる。
//! だから「上限に当たっている接続元からでも `/status` は取れる」(監視が消えない) 一方で、
//! プロキシとしての要求は断られる。断った数は `/status` の `rejected_per_client` と
//! `/clients` のその接続元の `rejected`。
//!
//! **別の接続元は送信元アドレスで作る** (`connect_from([127,0,0,2], ..)`)。ループバックは
//! `127.0.0.0/8` が丸ごと自分のアドレスなので、1 台の機械で「2 人の利用者」が作れる。
//!
//! 待つのに `/status` を使わない場面がある: `/status` 自体が 1 本の接続なので、上限に
//! 当たっている最中に取りに行くとその行為が本数を動かす (T13.2 と同じ罠)。本数は
//! `metrics.conns.client_conns()` を直に読んで待つ。
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

mod common;
use common::*;

use rust_http_proxy::metrics::Metrics;

/// 接続元ごとの上限だけを変えたテスト用プロキシ。
fn proxy_with_client_limit(max_conns_per_client: usize) -> (u16, Arc<Metrics>) {
    let mut cfg = park_config();
    cfg.max_conns_per_client = max_conns_per_client;
    // 握ったままのトンネルが預かり所の期限で閉じないように長くする
    cfg.keepalive = Duration::from_secs(60);
    start_test_proxy_with_metrics(cfg)
}

fn connect_request(target_port: u16) -> String {
    format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        target_port, target_port
    )
}

/// `src` から CONNECT を 1 本張り、応答の先頭行までを返す (200 なら接続も返す)。
fn connect_via(src: [u8; 4], proxy_port: u16, target_port: u16) -> (String, TcpStream) {
    let mut s = connect_from(src, proxy_port);
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(connect_request(target_port).as_bytes())
        .unwrap();
    let head = read_connect_response(&mut s);
    (head, s)
}

/// `src` から自分宛ての GET を 1 本投げ、応答全部を返す。
fn get_via(src: [u8; 4], proxy_port: u16, path: &str) -> String {
    let mut s = connect_from(src, proxy_port);
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(
        format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            path, proxy_port
        )
        .as_bytes(),
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

/// 上限 2 本: 3 本目の CONNECT は 503、別の接続元は 200、1 本閉じれば通る。
#[test]
fn test_integration_the_third_connection_from_one_client_gets_503() {
    let echo_port = start_echo_server();
    let (proxy_port, metrics) = proxy_with_client_limit(2);

    // 同じ接続元から 2 本 (上限ちょうど)
    let (head1, t1) = connect_via([127, 0, 0, 1], proxy_port, echo_port);
    let (head2, _t2) = connect_via([127, 0, 0, 1], proxy_port, echo_port);
    assert!(head1.starts_with("HTTP/1.1 200"), "{}", head1);
    assert!(head2.starts_with("HTTP/1.1 200"), "{}", head2);
    wait_until(
        || metrics.conns.client_conns("127.0.0.1") == 2,
        "two live connections from 127.0.0.1",
    );

    // 3 本目は 503 + Retry-After: 1
    let (third, _s) = connect_via([127, 0, 0, 1], proxy_port, echo_port);
    assert!(third.starts_with("HTTP/1.1 503"), "{}", third);
    assert!(third.contains("Retry-After: 1"), "{}", third);
    assert_eq!(metrics.rejected_per_client.load(Ordering::Relaxed), 1);
    assert_eq!(
        metrics.rejected_overload.load(Ordering::Relaxed),
        0,
        "同時接続の上限 (T13.2) とは別の数え物"
    );

    // 別の接続元は上限と無関係に通る
    let (other, _o) = connect_via([127, 0, 0, 2], proxy_port, echo_port);
    assert!(other.starts_with("HTTP/1.1 200"), "{}", other);

    // 上限に当たっている接続元からでも自分宛ては取れる (枠で受けて要求を読むため)。
    // **数え物は動かない** (断ったのは CONNECT の 1 本だけ)
    let status = get_via([127, 0, 0, 1], proxy_port, "/status");
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{}", status);
    assert_eq!(
        status_number(&status, "rejected_per_client"),
        1,
        "{}",
        status
    );

    // `/clients` のその接続元の行に `rejected`
    let clients = endpoint_json(proxy_port, "/clients");
    let row = clients
        .split("{\"client\":")
        .find(|r| r.starts_with("\"127.0.0.1\""))
        .unwrap_or_else(|| panic!("127.0.0.1 の行が無い: {}", clients));
    assert!(row.contains("\"rejected\":1"), "{}", row);

    // `/metrics` にも同じ数が出る
    let prom = get_via([127, 0, 0, 2], proxy_port, "/metrics");
    assert!(
        prom.contains("sorahost_rejected_per_client_total 1"),
        "/metrics に出ていない: {}",
        &prom[..prom.len().min(400)]
    );

    // 1 本閉じれば 3 本目が通る
    drop(t1);
    wait_until(
        || metrics.conns.client_conns("127.0.0.1") == 1,
        "one connection left after closing a tunnel",
    );
    let (again, mut s) = connect_via([127, 0, 0, 1], proxy_port, echo_port);
    assert!(again.starts_with("HTTP/1.1 200"), "{}", again);
    s.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping", "通ったトンネルは中継できる");
    assert_eq!(metrics.rejected_per_client.load(Ordering::Relaxed), 1);
}

/// 既定 (`0`) では今までどおり: 何本つないでも断らず、数える表も持たない。
#[test]
fn test_integration_the_per_client_limit_is_off_by_default() {
    let echo_port = start_echo_server();
    let (proxy_port, metrics) = proxy_with_client_limit(0);

    let mut held = Vec::new();
    for i in 0..5 {
        let (head, s) = connect_via([127, 0, 0, 1], proxy_port, echo_port);
        assert!(head.starts_with("HTTP/1.1 200"), "{}: {}", i, head);
        held.push(s);
    }
    assert_eq!(metrics.rejected_per_client.load(Ordering::Relaxed), 0);
    assert!(
        !metrics.conns.counting_clients(),
        "上限が無いときは接続元ごとの本数を数えない (費用 0)"
    );
    let status = get_via([127, 0, 0, 1], proxy_port, "/status");
    assert_eq!(
        status_number(&status, "rejected_per_client"),
        0,
        "{}",
        status
    );
}

/// 実バイナリ + `.env` の配線と、**`--lite` でも数えること** (T14.13)。
///
/// `--lite` は `/connections` の枠を作らないが、接続元ごとの本数だけは数えるので上限は効く。
/// 上限に当たっている接続元からでも `/status` が取れる (自分宛ては数えない) ことも、
/// ここで実バイナリのまま確かめる。
#[test]
fn test_integration_the_env_var_works_in_the_lite_profile() {
    let echo_port = start_echo_server();
    let dir = std::env::temp_dir().join(format!("rhp-t1413-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\n\
         PROXY_BIND=127.0.0.1\n\
         PROXY_PROFILE=lite\n\
         PROXY_LOG_LEVEL=info\n\
         PROXY_ALLOW_LOCAL=on\n\
         PROXY_MAX_CONNS_PER_CLIENT=2\n",
    )
    .unwrap();
    let proxy = ProxyProcess::start(&dir);

    let (h1, _t1) = connect_via([127, 0, 0, 1], proxy.port, echo_port);
    let (h2, _t2) = connect_via([127, 0, 0, 1], proxy.port, echo_port);
    assert!(h1.starts_with("HTTP/1.1 200"), "{}", h1);
    assert!(h2.starts_with("HTTP/1.1 200"), "{}", h2);
    let (third, _s) = connect_via([127, 0, 0, 1], proxy.port, echo_port);
    assert!(third.starts_with("HTTP/1.1 503"), "{}", third);
    assert!(third.contains("Retry-After: 1"), "{}", third);

    // 別の接続元は通る
    let (other, _o) = connect_via([127, 0, 0, 2], proxy.port, echo_port);
    assert!(other.starts_with("HTTP/1.1 200"), "{}", other);

    // 上限に当たっている接続元からでも `/status` は取れる
    let status = get_via([127, 0, 0, 1], proxy.port, "/status");
    assert!(status.starts_with("HTTP/1.1 200 OK"), "{}", status);
    assert_eq!(
        status_number(&status, "rejected_per_client"),
        1,
        "{}",
        status
    );

    drop(proxy);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `.env` の再読込のように**あとから**上限を入れても、生きている接続から数え直す。
#[test]
fn test_integration_the_limit_can_be_turned_on_and_off_while_running() {
    let echo_port = start_echo_server();
    let mut cfg = park_config();
    cfg.keepalive = Duration::from_secs(60);
    let (proxy_port, live) = start_test_proxy_with_live_config(cfg.clone());

    // 上限なしで 3 本
    let mut held = Vec::new();
    for _ in 0..3 {
        let (head, s) = connect_via([127, 0, 0, 1], proxy_port, echo_port);
        assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
        held.push(s);
    }

    // ここで上限 2 本を入れる (次に受ける接続から効く)
    cfg.max_conns_per_client = 2;
    *live.write().unwrap() = Arc::new(cfg.clone());
    let (over, _s) = connect_via([127, 0, 0, 1], proxy_port, echo_port);
    assert!(
        over.starts_with("HTTP/1.1 503"),
        "既に 3 本居るので断られる: {}",
        over
    );
    // 別の接続元は 0 本から数え始める
    let (other, _o) = connect_via([127, 0, 0, 2], proxy_port, echo_port);
    assert!(other.starts_with("HTTP/1.1 200"), "{}", other);

    // 0 に戻すと今までどおり通る
    cfg.max_conns_per_client = 0;
    *live.write().unwrap() = Arc::new(cfg);
    let (back, _b) = connect_via([127, 0, 0, 1], proxy_port, echo_port);
    assert!(back.starts_with("HTTP/1.1 200"), "{}", back);
}
