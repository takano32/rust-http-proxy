//! RFC から外れていた小さい振る舞いの結合テスト (T27.8)。2026-10-24 に 0.27.1 を叩いて見つけた 6 つ。
//!
//! (a) `Max-Forwards: 0` の TRACE / OPTIONS は自分で応え、0 でなければ 1 減らして転送する (RFC 9110 §7.6.2)
//! (b) URI の userinfo (`http://user:pw@host/`) は 400 (§4.2.4。今までは `user:pw@host` を引きに行って 502)
//! (c) `#fragment` はオリジンへ送らない (RFC 9112 §3.2)
//! (d) 版が `HTTP/1.x` でない要求 (`HTTP/9.9`・HTTP/2 の preface) は 505
//! (e) CONNECT の宛先にポートが無い・0 なら 400 (今までは 502)
//! (f) chunk の大きさが読めない本文は 400 (クライアントの誤り。今までは 502)

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod common;
use common::*;

fn send_raw(proxy_port: u16, bytes: &[u8]) -> String {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _ = stream.write_all(bytes);
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

fn capture_origin() -> (u16, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let (port, _handle) = start_origin(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(move |req, _n| {
            sink.lock().unwrap().push(req.to_string());
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec()
        }),
    );
    (port, seen)
}

fn settle_and_take(seen: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    std::thread::sleep(Duration::from_millis(200));
    std::mem::take(&mut *seen.lock().unwrap())
}

#[test]
fn test_integration_max_forwards_on_trace_and_options() {
    let (origin_port, seen) = capture_origin();
    let proxy_port = start_test_proxy(proxy_config());
    let h = format!("127.0.0.1:{}", origin_port);

    // 0 の OPTIONS: 自分で 200、本文なし、オリジンへは届かない
    let resp = send_raw(
        proxy_port,
        format!("OPTIONS http://{h}/a HTTP/1.1\r\nHost: {h}\r\nMax-Forwards: 0\r\n\r\n").as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    assert!(resp.contains("Content-Length: 0\r\n"), "{:?}", resp);
    assert!(settle_and_take(&seen).is_empty());

    // 0 の TRACE: 受け取った要求を message/http で返す。Cookie と Authorization は返さない
    let resp = send_raw(
        proxy_port,
        format!(
            "TRACE http://{h}/t HTTP/1.1\r\nHost: {h}\r\nMax-Forwards: 0\r\nX-Test: 1\r\n\
             Cookie: secret=1\r\nAuthorization: Basic eDp5\r\n\r\n"
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    assert!(
        resp.contains("Content-Type: message/http\r\n"),
        "{:?}",
        resp
    );
    let body = resp.split_once("\r\n\r\n").map_or("", |x| x.1);
    assert!(
        body.starts_with(&format!("TRACE http://{h}/t HTTP/1.1\r\n")),
        "{:?}",
        body
    );
    assert!(body.contains("X-Test: 1\r\n"), "{:?}", body);
    assert!(
        !body.contains("secret") && !body.contains("eDp5"),
        "{:?}",
        body
    );
    assert!(settle_and_take(&seen).is_empty());

    // 0 でなければ 1 減らして転送する
    let resp = send_raw(
        proxy_port,
        format!(
            "OPTIONS http://{h}/a HTTP/1.1\r\nHost: {h}\r\nMax-Forwards: 3\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    wait_until(|| !seen.lock().unwrap().is_empty(), "オリジンへの到達");
    let got = settle_and_take(&seen);
    assert!(got[0].contains("Max-Forwards: 2\r\n"), "{:?}", got);
    assert!(!got[0].contains("Max-Forwards: 3"), "{:?}", got);
}

#[test]
fn test_integration_userinfo_and_fragment() {
    let (origin_port, seen) = capture_origin();
    let proxy_port = start_test_proxy(proxy_config());
    let h = format!("127.0.0.1:{}", origin_port);

    let resp = send_raw(
        proxy_port,
        format!("GET http://user:pw@{h}/a HTTP/1.1\r\nHost: {h}\r\n\r\n").as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);
    assert!(settle_and_take(&seen).is_empty());

    let resp = send_raw(
        proxy_port,
        format!("GET http://{h}/a?q=1#frag HTTP/1.1\r\nHost: {h}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    wait_until(|| !seen.lock().unwrap().is_empty(), "オリジンへの到達");
    let got = settle_and_take(&seen);
    assert!(got[0].starts_with("GET /a?q=1 HTTP/1.1\r\n"), "{:?}", got);
}

#[test]
fn test_integration_versions_connect_ports_and_bad_chunks() {
    let (origin_port, seen) = capture_origin();
    let proxy_port = start_test_proxy(proxy_config());
    let h = format!("127.0.0.1:{}", origin_port);

    for req in [
        format!("GET http://{h}/a HTTP/9.9\r\nHost: {h}\r\n\r\n"),
        "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_string(),
    ] {
        let resp = send_raw(proxy_port, req.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 505 "), "{:?}: {:?}", req, resp);
    }
    // `HTTP/1.0` と `HTTP/1.1` は今までどおり
    let resp = send_raw(
        proxy_port,
        format!("GET http://{h}/a HTTP/1.0\r\nHost: {h}\r\n\r\n").as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    settle_and_take(&seen);

    for target in ["127.0.0.1", "127.0.0.1:0", "127.0.0.1:abc", ":443"] {
        let resp = send_raw(
            proxy_port,
            format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes(),
        );
        assert!(resp.starts_with("HTTP/1.1 400 "), "{}: {:?}", target, resp);
    }

    for size in ["0x5", "FFFFFFFFFFFFFFFFFF"] {
        let resp = send_raw(
            proxy_port,
            format!(
                "POST http://{h}/c HTTP/1.1\r\nHost: {h}\r\nTransfer-Encoding: chunked\r\n\r\n\
                 {size}\r\nhello\r\n0\r\n\r\n"
            )
            .as_bytes(),
        );
        assert!(resp.starts_with("HTTP/1.1 400 "), "{}: {:?}", size, resp);
    }
}

/// T.29.8: 要求行はちょうど 3 要素・対象は VCHAR・版は `HTTP/d.d`、ヘッダー名は空でない token。
/// 外れたら 400 で、オリジンへは 1 本も届かない (今までは 505 か、そのまま転送していた)。
/// 正しい形 (区切りの空白が 2 個・`HTTP/1.0`・token の記号を使った名前) は今までどおり通る。
#[test]
fn test_integration_request_line_and_header_name_grammar() {
    let (origin_port, seen) = capture_origin();
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let h = format!("127.0.0.1:{}", origin_port);
    let get = |line_tail: &str, headers: &str| {
        format!("GET http://{h}/a{line_tail}\r\nHost: {h}\r\n{headers}\r\n").into_bytes()
    };

    let bad: Vec<(&str, Vec<u8>)> = vec![
        ("4 要素", get(" b HTTP/1.1", "")),
        ("後ろに junk", get(" HTTP/1.1 junk", "")),
        ("版が無い", get("", "")),
        ("版の後ろに字", get(" HTTP/1.1x", "")),
        ("対象に DEL", get("\x7f HTTP/1.1", "")),
        ("対象に NUL", get("\x00b HTTP/1.1", "")),
        ("対象に制御文字", get("\x01 HTTP/1.1", "")),
        ("名前の中の空白", get(" HTTP/1.1", "X A: 1\r\n")),
        ("名前の \"", get(" HTTP/1.1", "X\"A: 1\r\n")),
        ("名前の制御文字", get(" HTTP/1.1", "X\x01A: 1\r\n")),
        ("空の名前", get(" HTTP/1.1", ": v\r\n")),
        ("コロンの無い行", get(" HTTP/1.1", "Junk\r\n")),
    ];
    for (name, req) in &bad {
        let resp = send_raw(proxy_port, req);
        assert!(resp.starts_with("HTTP/1.1 400 "), "{}: {:?}", name, resp);
    }
    assert!(
        settle_and_take(&seen).is_empty(),
        "断った要求がオリジンへ届いた"
    );
    // 断った理由に乗る (要求行は `request_line`、ヘッダーは `bad_header`)
    use rust_http_proxy::metrics::BadRequestReason;
    use std::sync::atomic::Ordering;
    let count = |r: BadRequestReason| metrics.rejected_requests[r as usize].load(Ordering::Relaxed);
    assert_eq!(count(BadRequestReason::RequestLine), 7);
    assert_eq!(count(BadRequestReason::BadHeader), 5);

    // 正しい形は通る
    for req in [
        format!("GET  http://{h}/a  HTTP/1.1\r\nHost: {h}\r\n\r\n"),
        format!("GET http://{h}/a HTTP/1.0\r\nHost: {h}\r\n\r\n"),
        format!("GET http://{h}/a?q=%7F~ HTTP/1.1\r\nHost: {h}\r\nX-A.b_c!#$%&'*+^`|~: 1\r\n\r\n"),
    ] {
        let resp = send_raw(proxy_port, req.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}: {:?}", req, resp);
    }
    assert_eq!(settle_and_take(&seen).len(), 3);
}
