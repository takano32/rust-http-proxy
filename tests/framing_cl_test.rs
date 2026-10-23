//! 要求の `Content-Length` の検査 (T19.1)。
//!
//! 直す前は、数字でない値 (`-1`・`5, 5`・`0x5`・空)・`u64` に収まらない桁数・重複で不一致が
//! **そのままオリジンへ転送**されていた (重複不一致ではプロキシが最後の値を信じて本文を待ち、
//! 2.35 秒待って応答なしで切断)。判定 (こちらが使う値) と転送 (生の行) が食い違っていたのが原因。
//!
//! いまは形が壊れていれば 400 で断り (無言で閉じない)、同値の重複は 1 本に畳んで転送する。
//! `Transfer-Encoding: chunked` と `Content-Length` の同時指定 (T14.28 の 400) は回帰しないこと。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod common;
use common::*;

use rust_http_proxy::metrics::BadRequestReason;

/// `recording_origin` の戻り値 (port, 受け取った要求, 本数, 番人の handle)。
type Recording = (
    u16,
    Arc<Mutex<Vec<String>>>,
    Arc<AtomicUsize>,
    thread::JoinHandle<()>,
);

/// 受け取った要求の全文を覚えておくオリジン (何が届いたかを見るため)。
fn recording_origin() -> Recording {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hits = Arc::new(AtomicUsize::new(0));
    let sink = Arc::clone(&seen);
    let handler: Arc<Handler> = Arc::new(move |req: &str, _n: usize| {
        sink.lock().unwrap().push(req.to_string());
        let body = "ok";
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    });
    let (port, handle) = start_origin(Arc::clone(&hits), handler);
    (port, seen, hits, handle)
}

/// 生のバイト列を 1 本の接続に投げて応答を読み切る (所要時間も返す)。
fn send_raw(port: u16, bytes: &[u8]) -> (String, Duration) {
    let started = Instant::now();
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _ = stream.write_all(bytes);
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    (
        String::from_utf8_lossy(&out).into_owned(),
        started.elapsed(),
    )
}

/// 受け取った要求の中の `Content-Length` の値 (名前の大小は無視する)。
fn content_lengths(req: &str) -> Vec<String> {
    req.split("\r\n")
        .filter_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().to_string())
        })
        .collect()
}

/// 形の壊れた `Content-Length` は 400 で断り、オリジンへは 1 バイトも届かない。
///
/// **無言の切断ではない**ことも見る (400 の応答が返り、待たされない)。
/// 直す前は「重複不一致」が 2.35 秒待って応答なし、「21 桁」が本文なしとして 200 で転送、
/// `-1` と `5, 5` はそのまま転送されていた。
#[test]
fn test_integration_invalid_content_length_is_rejected() {
    let (origin_port, seen, hits, _origin) = recording_origin();
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    // (見出し, ヘッダー行, 本文)
    let cases: &[(&str, &[&str], &[u8])] = &[
        (
            "重複で不一致",
            &["Content-Length: 5", "Content-Length: 6"],
            b"hello",
        ),
        ("21 桁", &["Content-Length: 99999999999999999999"], b""),
        ("カンマ", &["Content-Length: 5, 5"], b""),
        ("負", &["Content-Length: -1"], b""),
        ("プラス", &["Content-Length: +5"], b""),
        ("空", &["Content-Length:"], b""),
        ("16 進", &["Content-Length: 0x5"], b""),
        (
            "壊れた値と正しい値の重複",
            &["Content-Length: x", "Content-Length: 5"],
            b"hello",
        ),
    ];

    for (name, lines, body) in cases {
        let mut req = format!("POST http://{}/x HTTP/1.1\r\nHost: {}\r\n", host, host);
        for l in *lines {
            req.push_str(l);
            req.push_str("\r\n");
        }
        req.push_str("Connection: close\r\n\r\n");
        let mut payload = req.into_bytes();
        payload.extend_from_slice(body);

        let (resp, elapsed) = send_raw(proxy_port, &payload);
        assert!(
            resp.starts_with("HTTP/1.1 400 "),
            "{}: 400 で断っていない: {:?}",
            name,
            resp
        );
        // 無言で閉じる経路 (2.35 秒待って切断) に戻っていないこと
        assert!(
            elapsed < Duration::from_secs(1),
            "{}: 400 を返すまで {:?} 待った",
            name,
            elapsed
        );
    }

    // オリジンへは 1 本も行っていない (転送そのものをしない)
    assert_eq!(hits.load(Ordering::SeqCst), 0, "オリジンへ届いている");
    assert!(
        seen.lock().unwrap().is_empty(),
        "{:?}",
        seen.lock().unwrap()
    );

    // `/status` の `body_framing` に数える
    let count = |r: BadRequestReason| metrics.rejected_requests[r as usize].load(Ordering::Relaxed);
    assert_eq!(count(BadRequestReason::BodyFraming), cases.len() as u64);

    // 正常な要求はこの後も通る (断る判定が広がっていない)
    let (resp, _) = send_raw(
        proxy_port,
        format!(
            "POST http://{}/x HTTP/1.1\r\nHost: {}\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            host, host
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// 同値の重複は 1 本に畳んで転送し、200 で通る (本文もそのまま届く)。
#[test]
fn test_integration_equal_duplicate_content_length_is_collapsed() {
    let (origin_port, seen, hits, _origin) = recording_origin();
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let (resp, _) = send_raw(
        proxy_port,
        format!(
            "POST http://{0}/x HTTP/1.1\r\nHost: {0}\r\nContent-Length: 5\r\n\
             Content-Length: 5\r\nConnection: close\r\n\r\nhello",
            host
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let at_origin = content_lengths(&seen[0]);
    assert_eq!(
        at_origin,
        vec!["5".to_string()],
        "オリジンが受け取った CL: {:?}\n{}",
        at_origin,
        seen[0]
    );
    // 本文も 1 回だけ届いている (二重に送っていない)
    assert!(seen[0].ends_with("hello"), "{:?}", seen[0]);
}

/// `Transfer-Encoding: chunked` と `Content-Length` の同時指定は今までどおり 400 (T14.28)。
/// CL の側の検査を足したことで、この 400 が消えたり二重に数えたりしないこと。
#[test]
fn test_integration_chunked_with_content_length_is_still_rejected() {
    let (origin_port, seen, hits, _origin) = recording_origin();
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let (resp, _) = send_raw(
        proxy_port,
        format!(
            "POST http://{0}/x HTTP/1.1\r\nHost: {0}\r\nContent-Length: 5\r\n\
             Transfer-Encoding: chunked\r\nConnection: close\r\n\r\nhello",
            host
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    assert!(seen.lock().unwrap().is_empty());
    let count = |r: BadRequestReason| metrics.rejected_requests[r as usize].load(Ordering::Relaxed);
    assert_eq!(count(BadRequestReason::BodyFraming), 1);

    // chunked だけの要求は今までどおり通る (枠の判定を壊していない)
    let (resp, _) = send_raw(
        proxy_port,
        format!(
            "POST http://{0}/x HTTP/1.1\r\nHost: {0}\r\nTransfer-Encoding: chunked\r\n\
             Connection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
            host
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);

    // keep-alive で 2 本続けても、同値の重複は畳まれて通る (畳んだことで枠がずれない)
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    for _ in 0..2 {
        let req = format!(
            "POST http://{0}/x HTTP/1.1\r\nHost: {0}\r\nContent-Length: 2\r\n\
             Content-Length: 2\r\n\r\nhi",
            host
        );
        stream.write_all(req.as_bytes()).unwrap();
        let (head, body) = read_response(&mut stream);
        assert!(head.starts_with("HTTP/1.1 200 "), "{:?}", head);
        assert_eq!(body, b"ok");
    }
}
