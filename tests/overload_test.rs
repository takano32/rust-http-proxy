//! 同時接続の上限に当たったときの振る舞いの結合テスト (T13.2)。
//!
//! 上限に当たったら、まず**暇なトンネル**を最古から 1 本閉じて席を作る。閉じるものが
//! 無いときだけ 503 で、それでも自分宛て (`/status` など) は上限 + 4 本まで受ける。
//!
//! **ここでは `/status` を「待つため」に使わない。** `/status` 自体が 1 本の接続なので、
//! 上限に当たっている最中に取りに行くと、それがトンネルを 1 本閉じてしまう
//! (測る行為が状態を変える)。待つのは `start_test_proxy_with_metrics` が返す指標で行う。
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

mod common;
use common::*;

/// 上限 8 本のテスト用プロキシ。
fn proxy_with_limit(max_conns: usize) -> (u16, std::sync::Arc<rust_http_proxy::metrics::Metrics>) {
    let mut cfg = park_config();
    cfg.max_conns = max_conns;
    // 握ったままの keep-alive 接続が預かり所の期限で閉じないように長くする
    cfg.keepalive = Duration::from_secs(60);
    start_test_proxy_with_metrics(cfg)
}

/// CONNECT を張って `200` まで読む。
fn open_tunnel(proxy_port: u16, target_port: u16) -> TcpStream {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        target_port, target_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    stream
}

/// 読むだけで何も返さず、閉じもしないリスナー。
///
/// ここへ張ったトンネルでクライアントが送信側だけ閉じると、トンネルは**片方向だけ EOF**
/// になる。この形は預けられない (預かり所は両方向とも暇なものだけ預かる) ので、
/// 「全部のトンネルが忙しい = 閉じるものが無い」状態を待ち時間なしに作れる。
fn start_quiet_origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while !matches!(stream.read(&mut buf), Ok(0) | Err(_)) {}
                // EOF を見ても**閉じない**。記述子ごと手放してこのスレッドは終わる
                // (テストが終わるまで開いたまま = トンネルは片方向 EOF のまま生き続ける)
                std::mem::forget(stream);
            });
        }
    });
    port
}

/// 忙しい (預けられない) トンネルで上限をちょうど埋める。
fn fill_with_busy_tunnels(proxy_port: u16, origin_port: u16, n: usize) -> Vec<TcpStream> {
    (0..n)
        .map(|_| {
            let s = open_tunnel(proxy_port, origin_port);
            // 送信側を閉じると片方向だけ EOF = 預けられないトンネルになる
            s.shutdown(std::net::Shutdown::Write).unwrap();
            s
        })
        .collect()
}

/// 上限に当たったら、暇なトンネルの**最古の 1 本**を閉じて新しい CONNECT を受ける。
#[test]
fn test_integration_hitting_the_limit_closes_the_oldest_idle_tunnel() {
    let echo_port = start_echo_server();
    let (proxy_port, metrics) = proxy_with_limit(8);

    // 上限ちょうどまで暇なトンネルを張り、全部が預かり所に入るまで待つ
    let mut tunnels: Vec<TcpStream> = (0..8).map(|_| open_tunnel(proxy_port, echo_port)).collect();
    wait_until(
        || metrics.parked_tunnels.load(Ordering::Relaxed) == 8,
        "every idle tunnel should be parked",
    );

    // 9 本目。503 ではなく 200 で、席は最古の 1 本を閉じて作られる
    let mut ninth = open_tunnel(proxy_port, echo_port);
    assert_eq!(
        metrics.evicted_idle.load(Ordering::Relaxed),
        1,
        "exactly one idle tunnel is closed to make room"
    );
    assert_eq!(
        metrics.rejected_overload.load(Ordering::Relaxed),
        0,
        "nothing was refused"
    );
    assert!(
        metrics.active_connections.load(Ordering::Relaxed) <= 8,
        "the limit still holds: {} open",
        metrics.active_connections.load(Ordering::Relaxed)
    );

    // 閉じられたのは最古の 1 本 (握っている側で EOF が見える)
    let mut buf = [0u8; 1];
    assert_eq!(
        tunnels[0].read(&mut buf).unwrap(),
        0,
        "the oldest idle tunnel must be the one that was closed"
    );

    // 残りは生きていて、新しいトンネルも通る
    tunnels[1].write_all(b"ping").unwrap();
    let mut got = [0u8; 4];
    tunnels[1].read_exact(&mut got).unwrap();
    assert_eq!(&got, b"ping", "the other idle tunnels are untouched");
    ninth.write_all(b"pong").unwrap();
    ninth.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"pong", "the connection that took the seat works");
}

/// 同じ状態で `GET /status` を取ると、200 で `evicted_idle: 1` が出る。
#[test]
fn test_integration_status_gets_in_at_the_limit_and_reports_the_eviction() {
    let echo_port = start_echo_server();
    let (proxy_port, metrics) = proxy_with_limit(8);

    let _tunnels: Vec<TcpStream> = (0..8).map(|_| open_tunnel(proxy_port, echo_port)).collect();
    wait_until(
        || metrics.parked_tunnels.load(Ordering::Relaxed) == 8,
        "every idle tunnel should be parked",
    );

    // `/status` も 1 本の接続なので、これ自身が暇なトンネルを 1 本閉じて席を作る
    let json = status_json(proxy_port);
    assert!(json.starts_with("HTTP/1.1 200"), "{}", json);
    assert_eq!(status_number(&json, "evicted_idle"), 1, "{}", json);
    assert_eq!(status_number(&json, "parked_tunnels"), 7, "{}", json);
    assert_eq!(status_number(&json, "rejected_overload"), 0, "{}", json);
}

/// 全部のトンネルが忙しい (中継中) ときは、今までどおり 503。
#[test]
fn test_integration_busy_tunnels_are_not_closed_and_the_client_gets_503() {
    let quiet_port = start_quiet_origin();
    let (proxy_port, metrics) = proxy_with_limit(8);

    let _busy = fill_with_busy_tunnels(proxy_port, quiet_port, 8);
    assert_eq!(metrics.active_connections.load(Ordering::Relaxed), 8);
    // 片方向だけ EOF のトンネルは預けられない (猶予 100ms を越えても預かり所は空のまま)
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        metrics.parked_tunnels.load(Ordering::Relaxed),
        0,
        "busy tunnels are not parked"
    );

    let resp = raw_request(
        proxy_port,
        format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            quiet_port, quiet_port
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 503"), "{}", resp);
    assert!(resp.contains("Retry-After: 1"), "{}", resp);
    assert_eq!(
        metrics.evicted_idle.load(Ordering::Relaxed),
        0,
        "nothing was closed"
    );
    assert_eq!(metrics.rejected_overload.load(Ordering::Relaxed), 1);
}

/// 暇な keep-alive 接続 (`Parked::Http`) は閉じない。
///
/// 次の要求を待っているだけなので、閉じると入れ違いで届いた要求を取りこぼす。
#[test]
fn test_integration_idle_keepalive_connections_are_never_evicted() {
    let (origin_port, _origin) = start_mock_origin();
    let (proxy_port, metrics) = proxy_with_limit(8);
    let host = format!("127.0.0.1:{}", origin_port);

    // 1 要求ずつ通してから握る (要求を 1 本も通していない接続は預けられない)
    let mut held = Vec::new();
    for i in 0..8 {
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let (head, _) = one_keepalive_request(&mut s, &host, &format!("/hold{}", i));
        assert!(head.starts_with("HTTP/1.1 200"), "{}: {}", i, head);
        held.push(s);
    }
    wait_until(
        || metrics.parked_connections.load(Ordering::Relaxed) == 8,
        "every idle keep-alive connection should be parked",
    );

    // 9 本目は 503 (閉じてよい暇なトンネルが無い)
    let resp = raw_request(
        proxy_port,
        format!(
            "GET http://{}/ninth HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, host
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 503"), "{}", resp);
    assert_eq!(
        metrics.evicted_idle.load(Ordering::Relaxed),
        0,
        "idle keep-alive connections must not be closed"
    );

    // 握っていた接続はどれも生きていて、次の要求が通る
    for (i, s) in held.iter_mut().enumerate() {
        let (head, _) = one_keepalive_request(s, &host, &format!("/again{}", i));
        assert!(head.starts_with("HTTP/1.1 200"), "{}: {}", i, head);
    }
}

/// 閉じるものが無くても、自分宛ての要求は上限の外 (4 本の枠) で受ける。
#[test]
fn test_integration_self_addressed_requests_are_served_over_the_limit() {
    let quiet_port = start_quiet_origin();
    let (proxy_port, metrics) = proxy_with_limit(8);

    let _busy = fill_with_busy_tunnels(proxy_port, quiet_port, 8);
    assert_eq!(metrics.active_connections.load(Ordering::Relaxed), 8);

    // 上限に当たっていて閉じるものも無いが、`/status` は取れる
    let json = status_json(proxy_port);
    assert!(json.starts_with("HTTP/1.1 200"), "{}", json);
    assert_eq!(status_number(&json, "evicted_idle"), 0, "{}", json);
    assert_eq!(
        status_number(&json, "active_connections"),
        9,
        "上限 8 本 + 自分 (上限の外の 1 本): {}",
        json
    );
    // 枠は接続が閉じたら返る (2 本目も取れる)
    let again = status_json(proxy_port);
    assert!(again.starts_with("HTTP/1.1 200"), "{}", again);
}

/// 上限 + 4 本を超えた自分宛ては、読まずに 503。
#[test]
fn test_integration_over_the_limit_plus_four_is_refused_without_reading() {
    let quiet_port = start_quiet_origin();
    let (proxy_port, metrics) = proxy_with_limit(8);

    let _busy = fill_with_busy_tunnels(proxy_port, quiet_port, 8);
    // 枠 (4 本) を握る。どれも要求を送らないので、受けたまま枠を占める
    let _slots: Vec<TcpStream> = (0..4)
        .map(|_| TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap())
        .collect();
    wait_until(
        || metrics.active_connections.load(Ordering::Relaxed) == 12,
        "the four over-the-limit slots should be taken",
    );

    // 5 本目。要求を送らなくても 503 が返る (= 読まずに断っている)
    let mut fifth = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    fifth
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut resp = String::new();
    fifth.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 503"), "{}", resp);
    assert_eq!(
        metrics.evicted_idle.load(Ordering::Relaxed),
        0,
        "nothing was closed"
    );
}
