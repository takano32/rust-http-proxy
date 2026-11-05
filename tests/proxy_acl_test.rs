//! 宛て先と接続元の制限 (ACL・メタデータと私設アドレスの既定の拒否・接続の上限) の結合テスト。
//!
//! 初めは `tests/proxy_test.rs` にあった (T34.116 で割った)。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use rust_http_proxy::acl::AllowLocal;
use rust_http_proxy::config::Config;

mod common;
use common::*;

#[test]
fn test_integration_acl_denied() {
    let (origin_port, _origin_handle) = start_mock_origin();
    let config = Config::new(
        "0",
        None,
        Some("blocked.com, 127.0.0.1"),
        Duration::from_secs(5),
    )
    .unwrap();
    let proxy_port = start_test_proxy(config);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "GET http://127.0.0.1:{}/test HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        origin_port, origin_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 403 Forbidden"));
}

#[test]
fn test_integration_connection_limit_returns_503() {
    // 上限 8 で起動し、9 本目が 503 になること
    let (origin_port, _origin) = start_mock_origin();
    let mut cfg = proxy_config();
    cfg.max_conns = 8;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    // 上限ぶん keep-alive で握ったままにする (1 要求ずつ流して接続を確立させる)
    let mut held = Vec::new();
    for i in 0..8 {
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        let req = format!(
            "GET http://{}/hold{} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, i, host
        );
        s.write_all(req.as_bytes()).unwrap();
        read_response(&mut s);
        held.push(s);
    }

    let mut extra = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let mut resp = String::new();
    extra.read_to_string(&mut resp).unwrap();
    assert!(
        resp.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{}",
        resp
    );
    assert!(resp.contains("Retry-After: 1"), "{}", resp);

    // 1 本閉じれば次は通る
    held.pop();
    wait_until_for(
        || {
            let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
            let req = format!(
                "GET http://{}/after HTTP/1.1\r\nHost: {}\r\n\r\n",
                host, host
            );
            s.write_all(req.as_bytes()).unwrap();
            let (head, _) = read_response(&mut s);
            head.starts_with("HTTP/1.1 200")
        },
        "a slot should have been freed",
        Duration::from_millis(20),
        Limit::Tries(50),
    );
}

#[test]
fn test_integration_metadata_address_is_forbidden_by_default() {
    // 既定 (PROXY_ALLOW_LOCAL 無し) ではクラウドのメタデータ宛ては 403
    let mut cfg = proxy_config();
    cfg.allow_local = AllowLocal::Off;
    let proxy_port = start_test_proxy(cfg);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .write_all(
            b"GET http://169.254.169.254/latest/meta-data/ HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n",
        )
        .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 403 Forbidden"), "{}", resp);
}

/// 私設アドレスの宛て先 (T28.41 の受け入れ基準で挙げた 5 つ)。forward は `http://<この>/`、
/// CONNECT は `<この>:443` で送る。v4-mapped の書き方 (`[::ffff:10.0.0.1]`) も同じに扱う
const PRIVATE_TARGETS: [&str; 6] = [
    "10.0.0.1",
    "192.168.1.1",
    "172.16.0.1",
    "100.64.0.1",
    "[fd00::1]",
    "[::ffff:10.0.0.1]",
];

/// プロキシ自身が断ったときの応答 (`crates/server/src/lib.rs` の `FORBIDDEN_RESPONSE`)。
/// 宛て先の先 (この試験を回す網の私設アドレスに何かが居ることがある) が返す 403 と分けるため、
/// 状態行だけでなく全部を突き合わせる
const OWN_403: &[u8] = b"HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain; charset=utf-8\r\n\
Content-Length: 13\r\nConnection: close\r\n\r\n403 Forbidden";

/// `target` 宛てに forward 1 本 (`connect` なら CONNECT 1 本) を送って、**プロキシ自身が 403 で
/// 断ったか**を返す。通した要求は宛て先へ繋ぎに行く (届かなければ 502/504、届けばその応答) ので、
/// 最初の 2 秒ぶんだけ読んで比べる
fn refused(proxy_port: u16, target: &str, connect: bool) -> bool {
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let req = if connect {
        format!(
            "CONNECT {t}:443 HTTP/1.1\r\nHost: {t}:443\r\n\r\n",
            t = target
        )
    } else {
        format!(
            "GET http://{t}/ HTTP/1.1\r\nHost: {t}\r\nConnection: close\r\n\r\n",
            t = target
        )
    };
    stream.write_all(req.as_bytes()).unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    while got.len() < OWN_403.len() {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    got == OWN_403
}

/// T28.41: 既定 (`PROXY_ALLOW_LOCAL` 無し) では私設アドレス宛ても 403 (CONNECT も forward も)。
/// `private` なら私設アドレスは通り、ループバック・リンクローカルは 403 のまま。`on` は全部通す。
#[test]
fn test_integration_private_addresses_are_forbidden_by_default() {
    // 通した要求は宛て先へ繋ぎに行くので、届かない相手で待たないように締め切りを短くする
    let config = |allow| {
        let mut cfg = proxy_config();
        cfg.allow_local = allow;
        cfg.timeout = Duration::from_secs(1);
        cfg.connect_timeout = Duration::from_secs(1);
        cfg
    };
    // ループバックには本物のオリジンを置く (`on` で本当に通ることまで見る)
    let (origin_port, _origin) = start_mock_origin();
    let loopback = format!("127.0.0.1:{}", origin_port);
    // 1 本ずつ別のスレッドで送る (届かない宛て先の締め切りを並べて待たないように)
    let refusals = |proxy_port: u16, targets: Vec<String>| -> Vec<(String, bool, bool)> {
        let handles: Vec<_> = targets
            .into_iter()
            .flat_map(|t| [(t.clone(), false), (t, true)])
            .map(|(t, connect)| {
                thread::spawn(move || {
                    let r = refused(proxy_port, &t, connect);
                    (t, connect, r)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    };
    let private: Vec<String> = PRIVATE_TARGETS.iter().map(|t| t.to_string()).collect();
    let local = vec![
        "127.0.0.1".to_string(),
        "169.254.169.254".to_string(),
        "[::1]".to_string(),
    ];

    // 既定 (off): 私設アドレスもループバックも断る
    let port = start_test_proxy(config(AllowLocal::Off));
    for (t, connect, r) in refusals(port, private.iter().chain(&local).cloned().collect()) {
        assert!(r, "off: {} (connect={}) は 403", t, connect);
    }
    // private: 私設アドレスは通し、ループバック・リンクローカルは断ったまま
    let port = start_test_proxy(config(AllowLocal::Private));
    for (t, connect, r) in refusals(port, private.clone()) {
        assert!(!r, "private: {} (connect={}) は通る", t, connect);
    }
    for (t, connect, r) in refusals(port, local.clone()) {
        assert!(r, "private: {} (connect={}) は 403 のまま", t, connect);
    }
    // on: 全部通す (ループバックのオリジンには本当に届く)
    let port = start_test_proxy(config(AllowLocal::On));
    for (t, connect, r) in refusals(port, private.iter().chain(&local).cloned().collect()) {
        assert!(!r, "on: {} (connect={}) は通る", t, connect);
    }
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET http://{l}/ HTTP/1.1\r\nHost: {l}\r\nConnection: close\r\n\r\n",
        l = loopback
    )
    .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{}", resp);
}
