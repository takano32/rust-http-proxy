//! 預かり所の監視スレッド (`idle-watch`) は、預かりが 0 の間は眠ったまま起きない (T.29.23)。
//!
//! **このファイルのテストは 1 本だけ**にしてある: 同じテストバイナリで別のテストが
//! 自分のプロキシ (と自分の `idle-watch`) を立てると、どのスレッドが起きたのか分からなくなる。

#![cfg(target_os = "linux")]

use std::io::Read;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::thread;
use std::time::{Duration, Instant};

mod common;
use common::*;

/// このプロセスの `idle-watch` スレッドが自分から眠った回数 (`voluntary_ctxt_switches`)。
/// `epoll_wait` から起きて眠り直すたびに 1 増える。
///
/// スレッドの名前は**生まれたスレッドが自分で付ける**ので、立ち上げの直後は見えないことがある
/// (見えるまで待つ)。
fn idle_watch_wakeups() -> u64 {
    let mut found = Vec::new();
    for _ in 0..500 {
        found = idle_watch_counts();
        if found.len() == 1 {
            return found[0];
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("idle-watch はこのプロセスに 1 本のはず: {:?}", found);
}

fn idle_watch_counts() -> Vec<u64> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc/self/task").unwrap() {
        let dir = entry.unwrap().path();
        let comm = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
        if comm.trim() != "idle-watch" {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(dir.join("status")) else {
            continue;
        };
        if let Some(n) = status
            .lines()
            .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            found.push(n);
        }
    }
    found
}

fn parked(proxy_port: u16) -> u64 {
    status_number(&status_json(proxy_port), "parked_connections")
}

#[test]
fn test_integration_idle_watch_sleeps_while_nothing_is_parked() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.keepalive = Duration::from_secs(30);
    let (proxy_port, live) = start_test_proxy_with_live_config(cfg.clone());
    let host = format!("127.0.0.1:{}", origin_port);
    // 立ち上がって 1 本目の `/status` (Connection: close なので預けない) が返るまで待つ
    wait_until(|| parked(proxy_port) == 0, "the proxy should start");

    // (1) 預かりが 0 の間は起きない (以前は 1 秒ごとに起きていた: 2.5 秒で 2〜3 回)
    let before = idle_watch_wakeups();
    thread::sleep(Duration::from_millis(2500));
    let woke = idle_watch_wakeups() - before;
    assert_eq!(woke, 0, "預かりが 0 なのに idle-watch が {} 回起きた", woke);

    // (2) 眠っている所へ預けると呼び鈴で起き、期限 (30 秒) を待つ側に回る。
    //     そのあと keepalive を 300 ms に縮めて 2 本目を預けると、**先頭より早い期限**
    //     なのでもう一度起こされ、2 本目は 30 秒ではなく 300 ms で閉じる
    let mut a = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut a, &host, "/a");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    wait_until(
        || parked(proxy_port) == 1,
        "the first connection should be parked",
    );
    let mut short = cfg.clone();
    short.keepalive = Duration::from_millis(300);
    *live.write().unwrap() = Arc::new(short);
    let mut b = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut b, &host, "/b");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    let t0 = Instant::now();
    b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = [0u8; 1];
    let got = b.read(&mut buf);
    let took = t0.elapsed();
    assert!(
        matches!(got, Ok(0)),
        "早い期限で預けたのに閉じられない ({:?} / {:?})",
        got,
        took
    );
    assert!(
        took < Duration::from_secs(5),
        "早い期限で預けたのに監視スレッドが起きなかった ({:?})",
        took
    );
    assert_eq!(parked(proxy_port), 1, "1 本目 (期限 30 秒) は預かったまま");

    // (3) 1 本目を閉じて預かりが 0 に戻れば、また眠ったまま
    drop(a);
    wait_until(
        || parked(proxy_port) == 0,
        "the first connection should be released",
    );
    let before = idle_watch_wakeups();
    thread::sleep(Duration::from_millis(2500));
    let woke = idle_watch_wakeups() - before;
    assert_eq!(
        woke, 0,
        "預かりが 0 に戻ったのに idle-watch が {} 回起きた",
        woke
    );
}
