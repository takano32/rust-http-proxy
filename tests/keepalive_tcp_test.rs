//! **TCP の** keepalive (`PROXY_TCP_KEEPALIVE`。T14.52) で、消えたクライアントを検知する設定が
//! accept したソケットに当たることの結合テスト。HTTP の keep-alive (`PROXY_KEEPALIVE_SECS`) とは別のもの。
//!
//! 初めは `tests/keepalive_test.rs` の末尾にあった (T.34.119 で割った)。

use std::net::TcpStream;

mod common;
use common::*;

// ------------------------------------------------------------------
// 消えたクライアントの検知 (`PROXY_TCP_KEEPALIVE`。T14.52)

// ------------------------------------------------------------------

/// `sockaddr_in` と `getsockname`/`getpeername`。プロキシ側の記述子を探すために使う
/// (外部クレートは足さない方針なので、宣言は `tests/common` の `ffi` に 1 つだけ置く。T.34.104)。
#[cfg(target_os = "linux")]
use common::ffi::{SockAddrIn, getpeername, getsockname};

/// その記述子の (自分のポート, 相手のポート)。IPv4 の繋がったソケットでなければ `None`。
#[cfg(target_os = "linux")]
fn port_pair(fd: i32) -> Option<(u16, u16)> {
    let mut local = SockAddrIn::default();
    let mut peer = SockAddrIn::default();
    let mut n = std::mem::size_of::<SockAddrIn>() as u32;
    // SAFETY: どちらも `sockaddr_in` 1 つぶんの領域とその長さを対で渡している
    // (IPv6 なら切り詰められるが、族を見て弾く)。
    if unsafe { getsockname(fd, &mut local, &mut n) } != 0 {
        return None;
    }
    let mut n = std::mem::size_of::<SockAddrIn>() as u32;
    if unsafe { getpeername(fd, &mut peer, &mut n) } != 0 {
        return None;
    }
    (local.family == 2).then(|| (u16::from_be(local.port), u16::from_be(peer.port)))
}

/// この接続の**プロキシ側** (accept した方) の記述子を探す。
///
/// 結合テストのプロキシは同じプロセスの中で動いているので、`/proc/self/fd` を走査して
/// 「自分のポート = 待ち受け、相手のポート = クライアント」のソケットを 1 つ見つければ
/// それが accept された側 (クライアント側はちょうど逆なので取り違えない)。
#[cfg(target_os = "linux")]
fn proxy_side_fd(proxy_port: u16, client_port: u16) -> Option<i32> {
    for entry in std::fs::read_dir("/proc/self/fd").ok()? {
        let Ok(entry) = entry else { continue };
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if port_pair(fd) == Some((proxy_port, client_port)) {
            return Some(fd);
        }
    }
    None
}

/// 要求を 1 本通してから、プロキシ側の記述子に当たっている keepalive を読み戻す。
#[cfg(target_os = "linux")]
fn keepalive_after_one_request(
    cfg: rust_http_proxy::config::Config,
) -> rust_http_proxy::sys::Keepalive {
    let (origin_port, _origin) = start_mock_origin();
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let client_port = stream.local_addr().unwrap().port();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/p1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);

    let fd = proxy_side_fd(proxy_port, client_port)
        .expect("accept された側の記述子が自分のプロセスに見つからない");
    let k = rust_http_proxy::sys::keepalive_of(fd).expect("getsockopt");
    drop(stream);
    k
}

/// 既定 (`on`) では accept した接続に 60 / 10 / 3 が当たっていること (T14.52)。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_tcp_keepalive_is_set_on_the_accepted_socket() {
    let k = keepalive_after_one_request(proxy_config());
    assert_eq!(
        k,
        rust_http_proxy::sys::Keepalive {
            on: true,
            idle_secs: 60,
            intvl_secs: 10,
            count: 3,
        },
        "既定の PROXY_TCP_KEEPALIVE が当たっていない"
    );
}

/// テスト用に短くした指定 (`on:1:1:2`) がそのままカーネルへ届くこと (T14.52)。
///
/// この設定なら消えた相手は約 3 秒で `ETIMEDOUT` になる (既定は約 90 秒)。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_tcp_keepalive_takes_the_three_numbers() {
    let mut cfg = proxy_config();
    cfg.tcp_keepalive =
        rust_http_proxy::config::TcpKeepalive::parse("on:1:1:2").expect("on:1:1:2 が読めない");
    assert_eq!(cfg.tcp_keepalive.map(|k| k.dead_after_secs()), Some(3));
    let k = keepalive_after_one_request(cfg);
    assert_eq!(
        k,
        rust_http_proxy::sys::Keepalive {
            on: true,
            idle_secs: 1,
            intvl_secs: 1,
            count: 2,
        }
    );
}

/// `off` なら `setsockopt` を 1 回も呼ばない (ソケットは素のまま。T14.52)。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_tcp_keepalive_off_leaves_the_socket_alone() {
    let mut cfg = proxy_config();
    cfg.tcp_keepalive = None;
    let k = keepalive_after_one_request(cfg);
    assert!(!k.on, "off なのに SO_KEEPALIVE が立っている: {:?}", k);
}
