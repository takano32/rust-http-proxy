//! ヘッダーの形が不正な要求を 400 で断ることの結合テスト (T27.5)。
//!
//! 2026-10-24 に 0.27.1 を叩くと、次の 3 つがどれもオリジンへ届いていた。
//!
//! - `Content-Length : 5` (名前とコロンの間に空白)。こちらは長さとして使い、**空白つきのまま**転送した。
//!   RFC 9112 §5.1 は 400 を求めている (空白つきの名前を無視するオリジンとは本文の切れ目が割れる)
//! - `X-A: a\rTransfer-Encoding: chunked` (値の途中に CR)。CR を改行と読むオリジンには
//!   `Transfer-Encoding` に見える。RFC 9110 §5.5 は断るか SP に置き換えるかを求めている
//! - `X-A: a\0b` (値に NUL)。同じく §5.5
//!
//! 要求は置き換えると値が黙って変わるので断る (obs-fold と同じ)。見るのは、3 つとも 400 で
//! オリジンに届かないこと、`rejected_requests.bad_header` と `/metrics` が増えること、値の中の
//! HTAB と、空白の無いふつうの行は今までどおり通ること、CONNECT の入口でも同じに断ること。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod common;
use common::*;

/// 生のバイト列を 1 本の接続に投げて応答を全部読む (書けなくても落とさない)。
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

/// 受け取った要求の全文を貯めるオリジン。
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

/// `/metrics` の `sorahost_rejected_requests_total{reason="…"}` の値。
fn prom_value(text: &str, reason: &str) -> u64 {
    let needle = format!("sorahost_rejected_requests_total{{reason=\"{}\"}} ", reason);
    let line = text
        .lines()
        .find(|l| l.starts_with(&needle))
        .unwrap_or_else(|| panic!("{} の系列が無い", reason));
    line[needle.len()..].trim().parse().unwrap()
}

#[test]
fn test_integration_malformed_header_forms_are_rejected() {
    let (origin_port, seen) = capture_origin();
    let (proxy_port, _metrics) = start_test_proxy_with_metrics(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let bad: [(&str, Vec<u8>); 3] = [
        (
            "名前とコロンの間の空白",
            format!(
                "POST http://{h}/a HTTP/1.1\r\nHost: {h}\r\nContent-Length : 5\r\n\
                 Connection: close\r\n\r\nhello",
                h = host
            )
            .into_bytes(),
        ),
        (
            "値の途中の CR",
            format!(
                "GET http://{h}/b HTTP/1.1\r\nHost: {h}\r\nX-A: a\rTransfer-Encoding: chunked\r\n\
                 Connection: close\r\n\r\n",
                h = host
            )
            .into_bytes(),
        ),
        (
            "値の NUL",
            format!(
                "GET http://{h}/c HTTP/1.1\r\nHost: {h}\r\nX-A: a\0b\r\nConnection: close\r\n\r\n",
                h = host
            )
            .into_bytes(),
        ),
    ];
    for (name, bytes) in &bad {
        let resp = send_raw(proxy_port, bytes);
        assert!(resp.starts_with("HTTP/1.1 400 "), "{}: {:?}", name, resp);
        // 400 を返してから閉じる (無言で切らない)
        assert!(resp.contains("Content-Length: 0"), "{}: {:?}", name, resp);
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        seen.lock().unwrap().is_empty(),
        "400 にしたのにオリジンへ届いた: {:?}",
        seen.lock().unwrap()
    );

    // 値の中の HTAB は正しい (RFC 9110 §5.5 の field-vchar の間に置ける) ので通る
    let ok = format!(
        "GET http://{h}/ok HTTP/1.1\r\nHost: {h}\r\nX-A: a\tb\r\nConnection: close\r\n\r\n",
        h = host
    );
    let resp = send_raw(proxy_port, ok.as_bytes());
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    wait_until(|| !seen.lock().unwrap().is_empty(), "オリジンへの到達");
    assert!(seen.lock().unwrap()[0].contains("X-A: a\tb\r\n"));

    // `/status` の理由別と個票、`/metrics`
    let status = status_json(proxy_port);
    assert!(
        status.contains("\"bad_header\":3,\"header_timeout\":0,\"total\":3}"),
        "{}",
        status
    );
    let errors = endpoint_json(proxy_port, "/errors");
    assert!(
        errors.contains("\"cause\":\"bad_request:bad_header\""),
        "{}",
        errors
    );
    let prom = endpoint_json(proxy_port, "/metrics");
    assert_eq!(prom_value(&prom, "bad_header"), 3);
    assert_eq!(prom_value(&prom, "body_framing"), 0);
}

/// CONNECT は `http::handle_http_with_headers` を通らないので、入口で同じ判定をする (T19.8 と同じ所)。
#[test]
fn test_integration_malformed_header_in_connect_is_rejected() {
    let (origin_port, seen) = capture_origin();
    let (proxy_port, _metrics) = start_test_proxy_with_metrics(proxy_config());
    let target = format!("127.0.0.1:{}", origin_port);
    for bytes in [
        format!("CONNECT {t} HTTP/1.1\r\nHost : {t}\r\n\r\n", t = target),
        format!(
            "CONNECT {t} HTTP/1.1\r\nHost: {t}\r\nX-A: a\rb\r\n\r\n",
            t = target
        ),
    ] {
        let resp = send_raw(proxy_port, bytes.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);
        assert!(!resp.contains(" 200 "), "トンネルを張った: {:?}", resp);
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(seen.lock().unwrap().is_empty());
    let status = status_json(proxy_port);
    assert!(
        status.contains("\"bad_header\":2,\"header_timeout\":0,\"total\":2}"),
        "{}",
        status
    );
}

/// 応答の値の CR と NUL は SP に置き換えて返す (T27.7)。応答は断ると頁が見えなくなるので通す。
#[test]
fn test_integration_cr_and_nul_in_response_values_become_spaces() {
    let (origin_port, _handle) = start_origin(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(|_req, _n| {
            b"HTTP/1.1 200 OK\r\nX-A: a\rSet-Cookie: evil=1\r\nX-B: a\0b\r\nContent-Length: 2\r\n\
              Connection: close\r\n\r\nok"
                .to_vec()
        }),
    );
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);
    let resp = send_raw(
        proxy_port,
        format!(
            "GET http://{h}/r HTTP/1.1\r\nHost: {h}\r\nConnection: close\r\n\r\n",
            h = host
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    assert!(
        resp.contains("\r\nX-A: a Set-Cookie: evil=1\r\n"),
        "{:?}",
        resp
    );
    assert!(resp.contains("\r\nX-B: a b\r\n"), "{:?}", resp);
    assert!(!resp.contains('\0'), "{:?}", resp);
    // CR は行の区切りの CRLF にしか残らない
    assert_eq!(
        resp.matches('\r').count(),
        resp.matches("\r\n").count(),
        "{:?}",
        resp
    );
    assert!(resp.ends_with("\r\n\r\nok"), "{:?}", resp);
}
