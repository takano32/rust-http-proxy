//! `Expect: 100-continue` に、本文を読む前に `100 Continue` を返す (T.28.3)。
//!
//! 0.27.8 までは 100 を自分でも返さず、オリジンの 100 も中継しなかった
//! (`crates/msg/src/response.rs` の `read_head_forwarding`)。100 を厳密に待つクライアントは
//! 締め切りまで本文を送らず、こちらは本文を待って止まっていた。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

mod common;
use common::*;

/// 受け取った要求 (ヘッダーと本文) をそのまま本文にして返すオリジン。
fn start_request_echo_origin(counter: Arc<AtomicUsize>) -> u16 {
    start_origin(
        counter,
        Arc::new(|req, _| {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                req.len(),
                req
            )
            .into_bytes()
        }),
    )
    .0
}

/// `\r\n\r\n` まで 1 バイトずつ読む (読めた所まで返す。締め切りで止まる)。
fn read_head(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => buf.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// **受け入れ基準 (T.28.3)**: ヘッダーだけ送った時点で `100 Continue` が届き、本文を送ると 200。
/// `Expect` の行はオリジンへ届かない。
#[test]
fn test_integration_expect_100_continue_gets_100_before_the_body() {
    let counter = Arc::new(AtomicUsize::new(0));
    let origin_port = start_request_echo_origin(Arc::clone(&counter));
    let proxy_port = start_test_proxy(proxy_config());

    for (name, head_extra, body) in [
        ("Content-Length", "Content-Length: 5\r\n", &b"hello"[..]),
        (
            "chunked",
            "Transfer-Encoding: chunked\r\n",
            &b"5\r\nhello\r\n0\r\n\r\n"[..],
        ),
    ] {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let req = format!(
            "POST http://127.0.0.1:{}/up HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Expect: 100-Continue\r\n{}Connection: close\r\n\r\n",
            origin_port, origin_port, head_extra
        );
        stream.write_all(req.as_bytes()).unwrap();
        // 本文はまだ送らない。100 が先に届く
        let interim = read_head(&mut stream);
        assert_eq!(interim, "HTTP/1.1 100 Continue\r\n\r\n", "{}", name);
        stream.write_all(body).unwrap();
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest);
        let rest = String::from_utf8_lossy(&rest).into_owned();
        assert!(rest.starts_with("HTTP/1.1 200 "), "{}: {:?}", name, rest);
        // 100 は 1 回だけ
        assert!(!rest.contains("100 Continue"), "{}: {:?}", name, rest);
        assert!(
            rest.contains("hello"),
            "{}: 本文が届かない: {:?}",
            name,
            rest
        );
        assert!(
            !rest.to_ascii_lowercase().contains("expect:"),
            "{}: Expect がオリジンへ届いた: {:?}",
            name,
            rest
        );
    }
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

/// `100-continue` 以外の `Expect` は 417 で、オリジンに届かない。
/// 本文の無い要求の `Expect: 100-continue` と HTTP/1.0 の要求には 100 を返さない。
#[test]
fn test_integration_other_expectations_are_417_and_no_body_gets_no_100() {
    let counter = Arc::new(AtomicUsize::new(0));
    let origin_port = start_request_echo_origin(Arc::clone(&counter));
    let proxy_port = start_test_proxy(proxy_config());
    let url = format!("http://127.0.0.1:{}/x", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let resp = raw_request(
        proxy_port,
        format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nExpect: fancy\r\nContent-Length: 5\r\n\r\nhello",
            url, host
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 417 "), "{:?}", resp);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "417 なのにオリジンへ届いた"
    );

    // 本文なし: 100 は返さずにそのまま 200 (Expect はオリジンへ届かない)
    let resp = get_via_proxy_with(proxy_port, &url, &host, "Expect: 100-continue\r\n");
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    assert!(!resp.contains("100 Continue"), "{:?}", resp);
    assert!(!resp.to_ascii_lowercase().contains("expect:"), "{:?}", resp);

    // HTTP/1.0 の要求の 100-continue は無視する (本文はそのまま送ってくる)
    let resp = raw_request(
        proxy_port,
        format!(
            "POST {} HTTP/1.0\r\nHost: {}\r\nExpect: 100-continue\r\nContent-Length: 5\r\n\r\nhello",
            url, host
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    assert!(!resp.contains("100 Continue"), "{:?}", resp);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}
