//! keep-alive とアイドル接続の預かり (epoll) の結合テスト。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

#[test]
fn test_integration_parked_idle_connection_serves_the_next_request() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let proxy_port = start_test_proxy(park_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, body) = one_keepalive_request(&mut stream, &host, "/p1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");

    // 猶予 (既定 3ms) を過ぎれば監視スレッドに預けられ、スレッドから外れる
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the idle connection should be parked",
    );
    let status = status_json(proxy_port);
    assert!(status.contains("\"parking\":true"), "{}", status);

    // 預けた接続に要求を送ると、監視スレッドが起こしてワーカーが処理する
    let (head, body) = one_keepalive_request(&mut stream, &host, "/p2");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert!(head.contains("Connection: keep-alive"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    // 2 本目のあとも預けられる (何度でも往復できる)
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the connection should be parked again",
    );
    let (head, _) = one_keepalive_request(&mut stream, &host, "/p3");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
}

#[test]
fn test_integration_keepalive_without_parking_still_works() {
    // PROXY_PARK_IDLE=off: 「1 接続 = 1 スレッドが専任」の元の動き
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = proxy_config();
    cfg.park_idle = false;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/n1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    // 猶予より十分長く空けても、預けないので接続はそのまま
    thread::sleep(Duration::from_millis(100));
    let status = status_json(proxy_port);
    assert!(status.contains("\"parked_connections\":0"), "{}", status);
    assert!(status.contains("\"parking\":false"), "{}", status);
    let (head, body) = one_keepalive_request(&mut stream, &host, "/n2");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_integration_park_waits_for_a_request_sent_in_pieces() {
    // 猶予は「次の要求がまだ来ていない」ことを読み取りタイムアウトで測る。
    // 要求を送っている最中の細切れ (猶予より長い間隔) で切ってはいけない
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.park_grace = Duration::from_millis(5);
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/s1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the idle connection should be parked",
    );

    // 猶予 (5ms) の何倍も空けながら、要求行の途中・ヘッダーの途中で区切って送る
    let req = format!(
        "GET http://{}/s2 HTTP/1.1\r\nHost: {}\r\nX-Slow: yes\r\n\r\n",
        host, host
    );
    let bytes = req.as_bytes();
    for chunk in [&bytes[..12], &bytes[12..30], &bytes[30..]] {
        stream.write_all(chunk).unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_millis(40));
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let (head, body) = read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_integration_park_with_no_grace_serves_every_request() {
    // 猶予 0 = 要求のたびに必ず預けて戻す。預ける経路を毎回通す設定 (CI 用)
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.park_grace = Duration::ZERO;
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    for i in 0..5 {
        let (head, body) = one_keepalive_request(&mut stream, &host, &format!("/g{}", i));
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
        assert_eq!(body, b"hello from mock origin");
    }
    assert_eq!(counter.load(Ordering::SeqCst), 5);
}

#[test]
fn test_integration_parked_connection_closes_at_keepalive_timeout() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.keepalive = Duration::from_millis(300);
    let proxy_port = start_test_proxy(cfg);
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/t1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);

    // 預けたまま keep-alive の期限が過ぎたら、監視スレッドが閉じる
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut buf = [0u8; 1];
    assert_eq!(stream.read(&mut buf).unwrap(), 0, "closed by the proxy");
    // /status を引く接続それ自体が active に入るので、預かり数の方で見る
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":0"),
        "the expired connection should be released",
    );
}

#[test]
fn test_integration_parked_connection_notices_the_client_going_away() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let proxy_port = start_test_proxy(park_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut stream, &host, "/c1");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":1"),
        "the idle connection should be parked",
    );

    // 預けている間にクライアントが閉じたら、持ち分ごと片付ける
    drop(stream);
    wait_until(
        || status_json(proxy_port).contains("\"parked_connections\":0"),
        "the closed connection should be released",
    );
}

#[test]
fn test_integration_keepalive_serves_multiple_requests_per_connection() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-keepalive"));
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    for i in 0..3 {
        let req = format!(
            "GET http://{}/ka{} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, i, host
        );
        stream.write_all(req.as_bytes()).unwrap();
        let (head, body) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
        assert!(head.contains("Connection: keep-alive"), "{}", head);
        assert_eq!(body, b"hello from mock origin");
    }
    // 同じ接続でキャッシュヒットも返る
    let req = format!("GET http://{}/ka0 HTTP/1.1\r\nHost: {}\r\n\r\n", host, host);
    stream.write_all(req.as_bytes()).unwrap();
    let (head, body) = read_response(&mut stream);
    assert!(head.contains("X-Cache: HIT"), "{}", head);
    assert_eq!(body, b"hello from mock origin");
    assert_eq!(counter.load(Ordering::SeqCst), 3);

    // Connection: close で終わる
    let req = format!(
        "GET http://{}/ka1 HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        host, host
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    let text = String::from_utf8_lossy(&rest);
    assert!(text.contains("Connection: close"), "{}", text);
    assert!(text.ends_with("hello from mock origin"));

    // HTTP/1.0 の要求は応答後に閉じられる
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let req = format!("GET http://{}/ka2 HTTP/1.0\r\nHost: {}\r\n\r\n", host, host);
    stream.write_all(req.as_bytes()).unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    assert!(String::from_utf8_lossy(&rest).contains("Connection: close"));
}

#[test]
fn test_integration_keepalive_requests_are_not_delayed_by_nagle() {
    // TCP_NODELAY が立っていないと、応答ヘッダーと本文を別々に write したときに
    // Nagle + delayed ACK で 1 要求あたり約 40 ms 止まる (5 要求で 200 ms 以上)。
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        start_keepalive_origin(Arc::clone(&connections), Arc::clone(&requests));
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    // 1 要求目は接続確立を含むので測定から外す
    let warmup = format!("GET http://{}/w HTTP/1.1\r\nHost: {}\r\n\r\n", host, host);
    stream.write_all(warmup.as_bytes()).unwrap();
    read_response(&mut stream);

    let started = std::time::Instant::now();
    for i in 0..5 {
        let req = format!(
            "GET http://{}/nagle{} HTTP/1.1\r\nHost: {}\r\n\r\n",
            host, i, host
        );
        stream.write_all(req.as_bytes()).unwrap();
        let (head, _body) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "5 keep-alive requests took {:?} (Nagle would need 200ms or more)",
        elapsed
    );
}
