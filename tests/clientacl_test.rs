//! `PROXY_ALLOW_CLIENTS` (接続元の ACL) の結合テスト (T14.18)。
//!
//! 許可リストに無い接続元は **accept した直後に、要求を読まずに**閉じられる。CONNECT も
//! `GET /status` も同じで、**内部エンドポイントも閉じる** (公開ポートで個票を見せないため)。
//!
//! 数える先の `rejected_client_acl` は `/status` に出るが、断られている間はその `/status`
//! 自体も読めない。そこで `.env` の再読込で `127.0.0.0/8` を足してから読む
//! (再読込を待つのは**起動ログの行**で。`/status` を叩いて待つと、その空振り 1 本ずつが
//! 数えたい `rejected_client_acl` を動かしてしまう)。

mod common;
use common::*;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 1 本つないで要求を投げ、返ってきたバイト列を返す (断られていれば空)。
///
/// 断られた接続はこちらが書く前に閉じていることがあるので、書き込みの失敗も
/// 「閉じられた」として扱う (RST になるか FIN になるかは間合い次第)。
fn try_request(port: u16, req: &str) -> String {
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let _ = s.write_all(req.as_bytes());
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

fn status_request(port: u16) -> String {
    format!(
        "GET /status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        port
    )
}

fn connect_request(target_port: u16) -> String {
    format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        target_port, target_port
    )
}

#[test]
fn test_integration_allow_clients_closes_strangers_and_counts_them() {
    let echo_port = start_echo_server();
    let dir = std::env::temp_dir().join(format!("rhp-t1418-acl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let write_env = |clients: &str| {
        std::fs::write(
            dir.join(".env"),
            format!(
                "SERVER_PORT=0\n\
                 PROXY_BIND=127.0.0.1\n\
                 PROXY_PROFILE=lite\n\
                 PROXY_LOG_LEVEL=info\n\
                 PROXY_ALLOW_LOCAL=on\n\
                 PROXY_ALLOW_CLIENTS={}\n",
                clients
            ),
        )
        .unwrap();
    };

    // `127.0.0.1` はこの一覧に無い
    write_env("10.0.0.0/8");
    let proxy = ProxyProcess::start(&dir);

    // CONNECT も `/status` も、応答を 1 バイトも返さずに閉じられる
    let resp = try_request(proxy.port, &connect_request(echo_port));
    assert!(resp.is_empty(), "CONNECT に応答が返った: {:?}", resp);
    let resp = try_request(proxy.port, &status_request(proxy.port));
    assert!(resp.is_empty(), "/status に応答が返った: {:?}", resp);

    // `127.0.0.0/8` を足す (`.env` の再読込。待つのは起動ログの行)
    write_env("10.0.0.0/8,127.0.0.0/8");
    let line = proxy.wait_for_log("PROXY_ALLOW_CLIENTS");
    assert!(line.contains("settings reloaded"), "{}", line);

    // 通るようになり、断った 2 本が数えられている
    let resp = try_request(proxy.port, &status_request(proxy.port));
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{}", resp);
    assert_eq!(
        status_number(&resp, "rejected_client_acl"),
        2,
        "断ったのは CONNECT と /status の 2 本: {}",
        resp
    );
    assert_eq!(
        status_number(&resp, "rejected_overload"),
        0,
        "上限の 503 とは別の数え物: {}",
        resp
    );

    // `/metrics` にも同じ数が出る
    let resp = try_request(
        proxy.port,
        &format!(
            "GET /metrics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            proxy.port
        ),
    );
    assert!(
        resp.contains("sorahost_rejected_client_acl_total 2"),
        "/metrics に出ていない: {}",
        &resp[..resp.len().min(400)]
    );

    // CONNECT も通り、トンネルとして使える
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(connect_request(echo_port).as_bytes()).unwrap();
    let head = read_connect_response(&mut s);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    s.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");

    drop(proxy);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 一覧が空 (既定) なら誰でも通ること、書き損じだけの一覧では絞らないこと。
///
/// こちらは設定を直に差し替えるだけの、プロセス内のテスト (`.env` を経由しない)。
#[test]
fn test_integration_allow_clients_is_off_by_default() {
    use rust_http_proxy::acl::ClientAcl;

    let mut cfg = proxy_config();
    assert!(cfg.allow_clients.is_empty(), "既定は空 = 全許可");
    let port = start_test_proxy(cfg.clone());
    let resp = try_request(port, &status_request(port));
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{}", resp);
    assert_eq!(status_number(&resp, "rejected_client_acl"), 0, "{}", resp);

    // 書式が違う項目しか無ければ空のまま = 全許可 (絞る手段が 1 つも無い)
    cfg.allow_clients = ClientAcl::parse("junk, 10.0.0.0/nope");
    assert!(cfg.allow_clients.is_empty());
    let port = start_test_proxy(cfg);
    let resp = try_request(port, &status_request(port));
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{}", resp);
}
