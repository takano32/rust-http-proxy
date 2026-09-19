//! `PROXY_CONNECT_TIMEOUT_SECS` の結合テスト (T15.6 (1)(2))。
//!
//! 見るのは 1 つだけ: **CONNECT のオリジン接続に `config.connect_timeout` が効いていること**。
//! `config.timeout` (5 秒) と別の値 (1 秒) を入れて、黒穴への CONNECT が 5 秒ではなく
//! 1 秒あまりで 502 になり、`/errors` の原因が `timeout` になることを確かめる。
//!
//! 黒穴の作り方は `crates/net-conn/src/net.rs` の `blackhole_v4`
//! (`mod tests` の中の private なので `tests/` からは呼べない。中身を写す)。
//! `listen(fd, 0)` で受け入れ待ち行列を 1 本にして、詰め物 1 本で埋めると、
//! 以後の SYN は黙って捨てられる (`tcp_abort_on_overflow = 0` の既定)。

mod common;

#[cfg(target_os = "linux")]
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use common::*;

/// `127.0.0.1` の黒穴 (`listen(fd, 0)` + 詰め物 1 本)。返り値は生かしておくこと。
///
/// `libc` は足さない (§0) ので `listen(2)` だけ直に宣言する。`tests/` で
/// `unsafe extern "C"` を宣言する先例は `tests/common/mod.rs` の `connect_from`。
#[cfg(target_os = "linux")]
fn blackhole_v4() -> Option<(TcpListener, TcpStream, SocketAddr)> {
    use std::os::fd::AsRawFd;
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

/// `connect_timeout` を短くすると、CONNECT だけがその長さで諦める。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_connect_timeout_cuts_the_connect_short() {
    let Some((_hole, _filler, hole_addr)) = blackhole_v4() else {
        eprintln!("cannot build a v4 blackhole; skipping");
        return;
    };

    // `proxy_config()` の `timeout` は 5 秒 (`allow_local = true` なので loopback へ繋げる)。
    // CONNECT 専用の締め切りだけを 1 秒にするので、5 秒との差で「効いたか」が分かる
    let mut cfg = proxy_config();
    cfg.connect_timeout = Duration::from_secs(1);
    assert_eq!(cfg.timeout, Duration::from_secs(5), "前提: 共通の締め切り");
    let proxy_port = start_test_proxy(cfg);

    let target = hole_addr.to_string();
    let started = Instant::now();
    let out = raw_get(
        proxy_port,
        &format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", target, target),
    );
    let elapsed = started.elapsed();

    // CONNECT の接続失敗は 502 (504 はコードに無い)
    assert!(out.starts_with("HTTP/1.1 502"), "{}", out);
    assert!(
        elapsed < Duration::from_secs(3),
        "`connect_timeout` が効いていれば 1 秒あまりで諦める (かかった時間 {:?})",
        elapsed
    );
    assert!(
        elapsed >= Duration::from_millis(500),
        "黒穴なのに即座に返った ({:?})。この機械では待ち行列が埋まっていない",
        elapsed
    );

    // 原因の札は `timeout` (T15.0 (3) で `refused` に化けなくなった)
    let json = endpoint_json(proxy_port, "/errors");
    assert!(json.contains("\"kind\":\"connect\""), "{}", json);
    assert!(json.contains("\"cause\":\"timeout\""), "{}", json);
    assert!(json.contains("\"status\":502"), "{}", json);
    assert!(
        json.contains(&format!("\"target\":\"{}\"", target)),
        "宛先が無い: {}",
        json
    );
}

/// 既定の締め切り (規則どおりの実効値) のままでも普通の CONNECT は 200 で通る。
///
/// `proxy_config()` の `timeout` は 5 秒で、既定の 10 秒より短いので
/// `connect_timeout` も 5 秒になる (`min(10, PROXY_TIMEOUT_SECS)`。T15.6 (2))。
/// 見張っているのは `Config::new` が規則を通ること (下の `assert_eq!`) と、
/// **その値で普通の CONNECT が今までどおり 200 で通ること** (煙試験)。`crates/server/src/lib.rs` の 1 行
/// (`config.timeout` → `config.connect_timeout`) の見張りは 1 本目
/// (`…cuts_the_connect_short`) の方で、この 1 本はその 1 行を戻しても通る
/// (どちらも 5 秒なので区別できない)。規則の 5 通りは `crates/config` の単体テスト
/// (`connect_timeout_defaults_to_ten_seconds`)、`/config` の既定 10 秒は
/// `tests/config_test.rs` が実バイナリで見ている
/// (ここの `/config` は `Live` が無いと環境から組み直すため)。
#[test]
fn test_integration_connect_still_works_with_the_default_timeout() {
    let (origin_port, _origin) = start_mock_origin();
    let cfg = proxy_config();
    assert_eq!(
        cfg.connect_timeout,
        std::time::Duration::from_secs(5),
        "`PROXY_TIMEOUT_SECS` が 10 秒より短ければそちらに合わせる"
    );
    assert_eq!(cfg.timeout, std::time::Duration::from_secs(5));
    let proxy_port = start_test_proxy(cfg);

    let mut stream =
        std::net::TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).expect("proxy");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        origin_port, origin_port
    );
    std::io::Write::write_all(&mut stream, req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
}
