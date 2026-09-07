//! CONNECT トンネルの結合テスト。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

mod common;
use common::*;

#[test]
fn test_integration_connect_tunnel_forwards_prefix_and_both_directions() {
    let echo_port = start_echo_server();
    let proxy_port = start_test_proxy(proxy_config());

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    // 要求と、その直後に続くバイト (TLS ClientHello 相当) を 1 回で送る。
    // プロキシは先読みしてしまった分をトンネルの先頭で送り直さなければならない
    let req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\nCLIENT-HELLO",
        echo_port, echo_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);

    let mut got = [0u8; 12];
    stream.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"CLIENT-HELLO", "prefix must reach the origin first");

    // 双方向に流れる (splice の経路)
    let payload = vec![b'z'; 1 << 20];
    let mut sender = stream.try_clone().unwrap();
    let sent = payload.clone();
    let writer = thread::spawn(move || {
        sender.write_all(&sent).unwrap();
        sender.shutdown(std::net::Shutdown::Write).unwrap();
    });
    let mut back = Vec::new();
    stream.read_to_end(&mut back).unwrap();
    writer.join().unwrap();
    assert_eq!(back.len(), payload.len());
    assert_eq!(back, payload);
}

#[test]
fn test_integration_idle_tunnel_is_closed_after_the_idle_timeout() {
    let echo_port = start_echo_server();
    let mut cfg = proxy_config();
    cfg.tunnel_idle = Duration::from_secs(1);
    let proxy_port = start_test_proxy(cfg);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        echo_port, echo_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    assert!(read_connect_response(&mut stream).starts_with("HTTP/1.1 200"));

    // 無通信のまま放っておくと約 1 秒で閉じられる
    let started = std::time::Instant::now();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    let elapsed = started.elapsed();
    assert!(rest.is_empty(), "no data was sent");
    assert!(
        elapsed >= Duration::from_millis(700) && elapsed < Duration::from_secs(5),
        "closed after {:?}",
        elapsed
    );
}

#[test]
fn test_integration_connect_port_restriction() {
    let echo_port = start_echo_server();
    let mut cfg = proxy_config();
    // 443 だけ許す設定なので、テスト用オリジンのポートは弾かれる
    cfg.connect_ports = rust_http_proxy::acl::PortSet::parse("443");
    let proxy_port = start_test_proxy(cfg);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        echo_port, echo_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 403 Forbidden"), "{}", resp);
}
