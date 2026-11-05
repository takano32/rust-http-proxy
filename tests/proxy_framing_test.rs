//! 壊れた要求と枠組み (落ちない・ヘッダーの合計の上限・`Connection` で指名した枠組みのヘッダー) の結合テスト。
//!
//! 初めは `tests/proxy_test.rs` にあった (T34.116 で割った)。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod common;
use common::*;

#[test]
fn test_integration_malformed_requests_do_not_panic() {
    let proxy_port = start_test_proxy(proxy_config());
    let status =
        |resp: &str| -> Option<u16> { resp.split_whitespace().nth(1).and_then(|s| s.parse().ok()) };

    // 要求行が壊れている: 応答なしで閉じるか 400
    for bad in [
        &b"GARBAGE\r\n\r\n"[..],
        &b"GET\r\n\r\n"[..],
        &b"\x00\x01\x02\r\n\r\n"[..],
        &b"GET http://127.0.0.1:9/ HTTP/1.1\r\nHost\r\n\r\n"[..], // 区切りのないヘッダー
    ] {
        let resp = raw_request(proxy_port, bad);
        assert!(
            resp.is_empty() || matches!(status(&resp), Some(400..=599)),
            "unexpected response: {:?}",
            resp
        );
    }

    // 巨大な要求行 -> 414
    let mut long = b"GET http://example.invalid/".to_vec();
    long.extend(std::iter::repeat_n(b'a', 70 * 1024));
    long.extend_from_slice(b" HTTP/1.1\r\n\r\n");
    assert_eq!(status(&raw_request(proxy_port, &long)), Some(414));

    // ヘッダー行が多すぎる -> 431
    let mut many = b"GET http://example.invalid/ HTTP/1.1\r\nHost: example.invalid\r\n".to_vec();
    for i in 0..300 {
        many.extend_from_slice(format!("X-Pad-{}: 1\r\n", i).as_bytes());
    }
    many.extend_from_slice(b"\r\n");
    assert_eq!(status(&raw_request(proxy_port, &many)), Some(431));

    // ヘッダー 1 行が長すぎる -> 431
    let mut long_header = b"GET http://example.invalid/ HTTP/1.1\r\nX-Long: ".to_vec();
    long_header.extend(std::iter::repeat_n(b'b', 70 * 1024));
    long_header.extend_from_slice(b"\r\n\r\n");
    assert_eq!(status(&raw_request(proxy_port, &long_header)), Some(431));

    // CR 無しの行 (LF だけ) でも解釈できる
    let resp = raw_request(
        proxy_port,
        b"GET http://127.0.0.1:9/ HTTP/1.1\nHost: 127.0.0.1:9\n\n",
    );
    assert!(
        resp.is_empty() || matches!(status(&resp), Some(400..=599)),
        "{:?}",
        resp
    );

    // CONNECT の宛先にパスが付いている / スキームが付いている
    for target in [
        "http://example.invalid:443/path",
        "example.invalid:443/path",
    ] {
        let req = format!(
            "CONNECT {} HTTP/1.1\r\nHost: example.invalid\r\n\r\n",
            target
        );
        let resp = raw_request(proxy_port, req.as_bytes());
        assert!(
            resp.is_empty() || matches!(status(&resp), Some(400..=599)),
            "{:?}",
            resp
        );
    }

    // IPv6 リテラル (到達しないので 502 か 403。パニックしないことが要点)
    let resp = raw_request(
        proxy_port,
        b"GET http://[2001:db8::1]:8080/ HTTP/1.1\r\nHost: [2001:db8::1]:8080\r\n\r\n",
    );
    assert!(
        resp.is_empty() || matches!(status(&resp), Some(400..=599)),
        "{:?}",
        resp
    );

    // 最後に正常な要求が通ること (プロキシが生きている)
    let (origin_port, _origin) = start_mock_origin();
    let host = format!("127.0.0.1:{}", origin_port);
    let ok = get_via_proxy(proxy_port, &format!("http://{}/alive", host), &host);
    assert!(ok.contains("200 OK"), "{}", ok);
}

#[test]
fn test_integration_total_header_size_is_capped() {
    // 1 行の上限だけだと、正常な形の要求でも 64 KiB × 256 行 = 16 MiB を送れてしまい、
    // 認証なしの開放プロキシでは数接続でメモリを食い潰せる (実測で RSS 15 → 272 MiB)。
    // 合計にも上限があること、上限内の大きめのヘッダーは通ることを固定する
    let (origin_port, _origin) = start_mock_origin();
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let request_with_headers = |kib: usize| -> String {
        let mut req = format!("GET http://{}/pad HTTP/1.1\r\nHost: {}\r\n", host, host);
        // 1 行 8 KiB のヘッダーを並べる
        for i in 0..(kib / 8) {
            req.push_str(&format!(
                "X-Pad-{:03}: {}\r\n",
                i,
                "a".repeat(8 * 1024 - 15)
            ));
        }
        req.push_str("\r\n");
        req
    };

    // 上限内 (64 KiB) は通る
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(request_with_headers(64).as_bytes())
        .unwrap();
    let (head, _) = read_response(&mut stream);
    assert!(
        head.starts_with("HTTP/1.1 200 OK"),
        "上限内は通る: {}",
        head
    );

    // 上限超え (256 KiB) は 431
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(request_with_headers(256).as_bytes())
        .unwrap();
    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    assert!(
        resp.starts_with("HTTP/1.1 431"),
        "合計の上限を超えたら 431: {}",
        &resp[..resp.len().min(80)]
    );
}

#[test]
fn test_integration_connection_named_framing_headers_do_not_smuggle() {
    // `Connection: Content-Length` のように枠組みのヘッダーを hop-by-hop として指名されると、
    // 「ヘッダーからは Content-Length を落とすが本文は送る」というずれが起きうる。
    // オリジンはその本文を次の要求の先頭として読むので、共有のオリジン接続に別要求を
    // 注入できてしまう (要求スマグリング)。ヘッダーと本文が必ず整合することを固定する。
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_port = listener.local_addr().unwrap().port();
    {
        let received = Arc::clone(&received);
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let received = Arc::clone(&received);
                thread::spawn(move || {
                    stream
                        .set_read_timeout(Some(Duration::from_millis(500)))
                        .unwrap();
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() >= 10 {
                                    break;
                                }
                            }
                        }
                    }
                    received
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&buf).into_owned());
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                });
            }
        });
    }
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    for named in ["Content-Length", "Transfer-Encoding"] {
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let req = format!(
            "POST http://{}/x HTTP/1.1\r\nHost: {}\r\nConnection: {}\r\n\
             Content-Length: 10\r\n\r\n0123456789",
            host, host, named
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        let _ = s.read_to_string(&mut resp);

        // 応答を読み終えた = オリジンは応答を書く前に積み終えている (待たずに見てよい。T34.114)
        let seen = received.lock().unwrap();
        let last = seen.last().cloned().unwrap_or_default();
        let (head, body) = last.split_once("\r\n\r\n").unwrap_or((last.as_str(), ""));
        let has_framing = head
            .lines()
            .any(|l| l.to_ascii_lowercase().starts_with("content-length:"));
        assert!(
            has_framing || body.is_empty(),
            "Connection: {} で枠組みが落ちたのに本文が届いている (要求スマグリング):\n{}",
            named,
            last
        );
    }
}
