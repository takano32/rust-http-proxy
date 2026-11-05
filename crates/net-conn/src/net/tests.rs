//! `net` の、ソケットを使う試験 (待ち受け・Happy Eyeballs の候補の上限と締め切り・serve-stale の繋ぎ直し)。
//! IPv6 の全体の勝敗 (`Ipv6Policy`) を動かす試験は `ipv6_tests`、純粋な判断の試験はそれぞれの子
//! (`addrs` / `listen` / `ipv6` / `connect`) に置いてある。

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::happy_eyeballs::HE_LAUNCHED;
use super::*;

/// T32.14: 多アドレスのホストへの 1 接続で起こす試行 (スレッドと記述子) は [`MAX_CANDIDATES`] 件で
/// 頭打ち。拒否される loopback の番地 20 個 (拒否は即座に返るので、前は間隔を待たずに 20 本とも起こした)。
#[cfg(target_os = "linux")]
#[test]
fn happy_eyeballs_launches_at_most_max_candidates() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let addrs: Vec<SocketAddr> = (2..22u8)
        .map(|i| SocketAddr::from(([127, 0, 0, i], port)))
        .collect();
    HE_LAUNCHED.set(0);
    let err = connect_resolved("", addrs, Duration::from_secs(2)).expect_err("全部拒否");
    assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused, "{err}");
    assert_eq!(HE_LAUNCHED.get(), MAX_CANDIDATES);
}

#[test]
fn binds_dual_stack_or_falls_back() {
    assert!(ipv6_enabled(), "on by default");
    let v4 = bind_all_with(&[], 0, false, 0).expect("v4-only listener");
    assert_eq!(v4.len(), 1);
    assert!(v4[0].local_addr().unwrap().ip().is_ipv4());
    let listeners = bind_all(&[], 0, 0).expect("at least one listener");
    assert!(!listeners.is_empty() && listeners.len() <= 2);
    let port = listeners[0].local_addr().unwrap().port();
    assert!(
        listeners
            .iter()
            .all(|l| l.local_addr().unwrap().port() == port)
    );
    // 明示指定なら指定どおり
    let explicit = bind_all(&[IpAddr::V4(Ipv4Addr::LOCALHOST)], 0, 0).unwrap();
    assert_eq!(explicit.len(), 1);
    assert!(explicit[0].local_addr().unwrap().ip().is_loopback());
}

/// backlog の既定は `min(1024, somaxconn)` で、読めない機械でも 1〜1,024 に収まる (T14.47)。
#[test]
fn default_backlog_is_capped_at_1024() {
    let n = default_backlog();
    assert!((1..=DEFAULT_BACKLOG_CAP).contains(&n), "backlog {}", n);
    #[cfg(target_os = "linux")]
    if let Some(somaxconn) = std::fs::read_to_string("/proc/sys/net/core/somaxconn")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    {
        assert_eq!(
            n,
            somaxconn.min(DEFAULT_BACKLOG_CAP),
            "somaxconn と食い違う"
        );
    }
}

/// backlog を指定しても待ち受けは今までどおり使えること (T14.47)。
///
/// backlog そのもの (`ss -ltn` の `Send-Q`) は結合テスト `tests/backlog_test.rs` で見る。
/// ここで見るのは「`std` の `bind` を置き換えても待ち受けの姿が変わらない」こと:
/// 指定したアドレスに 1 本だけ立ち、accept でき、`0` と大きすぎる値でも失敗しないこと。
#[test]
fn binds_with_an_explicit_backlog() {
    let listeners = bind_all(&[IpAddr::V4(Ipv4Addr::LOCALHOST)], 0, 5).expect("listener");
    assert_eq!(listeners.len(), 1);
    let addr = listeners[0].local_addr().unwrap();
    let client = TcpStream::connect(addr).unwrap();
    let (accepted, _) = listeners[0].accept().unwrap();
    drop((client, accepted));

    // 0 は「既定に任せる」(呼ぶ側が解決していなくても落ちない)
    let zero = bind_all(&[IpAddr::V4(Ipv4Addr::LOCALHOST)], 0, 0).expect("listener");
    assert_eq!(zero.len(), 1);
    // 大きすぎる値はカーネルが somaxconn で頭打ちにするので、ここでは失敗しない
    let big = bind_all(&[IpAddr::V4(Ipv4Addr::LOCALHOST)], 0, 1 << 20).expect("listener");
    assert_eq!(big.len(), 1);
}

/// `127.0.0.1` の黒穴だけ (`listen(fd, 0)` + 詰め物 1 本で受け入れ待ち行列を埋める)。
///
/// 手本は [`blackhole_v6_and_live_v4`] だが、こちらは **IPv6 の全体状態
/// (`attempts` など) に一切触らない**ので `IPV6_TEST_LOCK` も要らない。
#[cfg(target_os = "linux")]
fn blackhole_v4() -> Option<(TcpListener, TcpStream, SocketAddr)> {
    use std::os::fd::AsRawFd;
    // `libc` は使わない (§0)。`listen(2)` だけ直接宣言する
    unsafe extern "C" {
        fn listen(fd: i32, backlog: i32) -> i32;
    }
    let hole = TcpListener::bind("127.0.0.1:0").ok()?;
    if unsafe { listen(hole.as_raw_fd(), 0) } != 0 {
        return None;
    }
    let hole_addr = hole.local_addr().ok()?;
    // 1 本つないで待ち行列を埋める (accept しない)
    let filler = TcpStream::connect_timeout(&hole_addr, Duration::from_secs(1)).ok()?;
    Some((hole, filler, hole_addr))
}

/// 締め切りで抜けたときの札は `TimedOut` (T15.0 (3))。
///
/// 候補 2 つ = 「閉じたポート (即 `ECONNREFUSED`)」+「黒穴 (SYN を黙って捨てる)」。
/// 直す前は先に返った `ECONNREFUSED` が `last_err` に残り、**締め切りまで待った
/// 接続が `refused` として集計されていた** (デプロイ先の実例: `cause=refused` なのに
/// `connect` 29,992 ms = 締め切り 30 秒)。
#[cfg(target_os = "linux")]
#[test]
fn a_deadline_reports_timed_out_not_the_first_refusal() {
    let Some((_hole, _filler, hole_addr)) = blackhole_v4() else {
        eprintln!("cannot build a v4 blackhole; skipping");
        return;
    };
    // 閉じたポート = 即 `ECONNREFUSED` (loopback なので確実に返る)
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_addr = closed.local_addr().unwrap();
    drop(closed);

    let started = Instant::now();
    let err = connect_resolved(
        "t150-deadline.invalid",
        vec![closed_addr, hole_addr],
        // `STAGGER` (250 ms) より十分長い締め切り
        Duration::from_secs(1),
    )
    .expect_err("どの候補も確立しないはず");
    let took = started.elapsed();
    assert_eq!(
        err.kind(),
        io::ErrorKind::TimedOut,
        "締め切りで抜けたのに {:?} ({:?})",
        err.kind(),
        took
    );
    assert!(
        took >= Duration::from_secs(1),
        "締め切りより前に諦めている: {:?}",
        took
    );
}

#[test]
fn connect_reports_error_when_all_fail() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let err = connect(&format!("127.0.0.1:{}", port), Duration::from_secs(2)).unwrap_err();
    assert!(matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
    ));
}

/// `/status` の `dns.stale_retries` (T21.3)。
fn stale_retries() -> u64 {
    let json = crate::dns::status_json();
    let pat = "\"stale_retries\":";
    let at = json.find(pat).expect("stale_retries") + pat.len();
    json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}

/// 127.0.0.1 だけで待ち受け、受けた本数を数える相手。127.0.0.2 の同じポートへは
/// `ECONNREFUSED` が返る (Linux のループバックは 127/8 を全部受けるが、待ち受けは 127.0.0.1 だけ)。
#[cfg(target_os = "linux")]
fn counting_listener() -> (u16, std::sync::Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = std::sync::Arc::new(AtomicU64::new(0));
    let seen = accepted.clone();
    thread::spawn(move || {
        for s in listener.incoming() {
            if s.is_ok() {
                seen.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    (port, accepted)
}

/// T21.3: serve-stale の古い答え (127.0.0.2) で `refused` になったら、引き直した答え
/// (表の新しい 127.0.0.1) で 1 回だけ繋ぎ直して成功する。
#[cfg(target_os = "linux")]
#[test]
fn a_stale_answer_that_is_refused_is_relooked_up_and_retried() {
    let _dns = crate::dns::RESOLVE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::dns::set_ttl(Duration::from_secs(60));
    let (port, accepted) = counting_listener();
    let host = "t213-moved.invalid";
    // 裏の引き直しが済んだ姿 = 表には新しい答え
    crate::dns::put_answer_for_test(host, &[IpAddr::from([127, 0, 0, 1])], Duration::ZERO);
    let r =
        crate::dns::Resolved::new(host, vec![IpAddr::from([127, 0, 0, 2])], None).with_stale(true);
    let t0 = stale_retries();
    let stream = connect_with(&format!("{host}:{port}"), Some(&r), Duration::from_secs(5))
        .expect("引き直した答えで繋がる");
    assert_eq!(
        stream.peer_addr().unwrap().ip(),
        IpAddr::from([127, 0, 0, 1])
    );
    assert_eq!(stale_retries() - t0, 1, "引き直しは 1 回");
    wait_until_accepted(&accepted, 1);
    crate::dns::clear();
}

/// T21.3: 引き直しても番地が同じなら繋ぎ直さず、元の `refused` を返す。
#[cfg(target_os = "linux")]
#[test]
fn a_stale_answer_is_not_retried_when_the_address_did_not_change() {
    let _dns = crate::dns::RESOLVE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::dns::set_ttl(Duration::from_secs(60));
    let (port, accepted) = counting_listener();
    let host = "t213-same.invalid";
    let gone = IpAddr::from([127, 0, 0, 2]);
    crate::dns::put_answer_for_test(host, &[gone], Duration::ZERO);
    let r = crate::dns::Resolved::new(host, vec![gone], None).with_stale(true);
    let t0 = stale_retries();
    let err =
        connect_with(&format!("{host}:{port}"), Some(&r), Duration::from_secs(5)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    assert_eq!(stale_retries() - t0, 1, "引き直しはした");
    assert_eq!(accepted.load(Ordering::Relaxed), 0);
    crate::dns::clear();
}

/// T21.3: 古い答えでない (印が立っていない) 失敗は引き直さない (今までどおり)。
#[cfg(target_os = "linux")]
#[test]
fn a_fresh_answer_that_is_refused_is_not_relooked_up() {
    let _dns = crate::dns::RESOLVE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::dns::set_ttl(Duration::from_secs(60));
    let (port, _accepted) = counting_listener();
    let host = "t213-fresh.invalid";
    crate::dns::put_answer_for_test(host, &[IpAddr::from([127, 0, 0, 1])], Duration::ZERO);
    let r = crate::dns::Resolved::new(host, vec![IpAddr::from([127, 0, 0, 2])], None);
    let t0 = stale_retries();
    let err =
        connect_with(&format!("{host}:{port}"), Some(&r), Duration::from_secs(5)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    assert_eq!(stale_retries(), t0, "引き直さない");
    crate::dns::clear();
}

#[cfg(target_os = "linux")]
fn wait_until_accepted(accepted: &AtomicU64, want: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while accepted.load(Ordering::Relaxed) < want {
        assert!(
            Instant::now() < deadline,
            "accepted = {}",
            accepted.load(Ordering::Relaxed)
        );
        thread::sleep(Duration::from_millis(10));
    }
}
