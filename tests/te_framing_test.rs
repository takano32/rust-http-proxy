//! `Transfer-Encoding` の形が本文の切れ目を決めない要求を 400 で断ることの結合テスト (T27.6)。
//!
//! 2026-10-24 に 0.27.1 を叩くと、`Transfer-Encoding: xchunked` / `gzip` / `chunked, identity` は
//! **本文なしとしてヘッダーだけ転送**され、本文はクライアントの次の要求として読まれた (応答が 2 つ)。
//! `xchunked` と `Content-Length: 5` が両方ある要求は CL で読み、TE も CL も転送した。`chunked` を
//! 緩く読むオリジンとは切れ目が割れる (要求の密輸)。RFC 9112 §6.3 は「要求の TE の最後が chunked で
//! なければ 400 で断って閉じる」、§6.1 は「HTTP/1.0 の TE は枠組みが壊れているとして扱う」。
//!
//! 見るのは、断る 6 形が 400 でオリジンに届かず `body_framing` に数えられること、
//! `gzip, chunked` (最後が chunked) と大文字の `Chunked` は今までどおり chunked として届くこと。

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

fn post(host: &str, version: &str, headers: &str, body: &str) -> Vec<u8> {
    format!(
        "POST http://{h}/te {v}\r\nHost: {h}\r\n{headers}Connection: close\r\n\r\n{body}",
        h = host,
        v = version
    )
    .into_bytes()
}

const CHUNKED_BODY: &str = "5\r\nhello\r\n0\r\n\r\n";

#[test]
fn test_integration_transfer_encoding_that_does_not_frame_is_rejected() {
    let (origin_port, seen) = capture_origin();
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let bad = [
        (
            "xchunked",
            post(
                &host,
                "HTTP/1.1",
                "Transfer-Encoding: xchunked\r\n",
                CHUNKED_BODY,
            ),
        ),
        (
            "gzip だけ",
            post(&host, "HTTP/1.1", "Transfer-Encoding: gzip\r\n", "hello"),
        ),
        (
            "chunked, identity",
            post(
                &host,
                "HTTP/1.1",
                "Transfer-Encoding: chunked, identity\r\n",
                CHUNKED_BODY,
            ),
        ),
        (
            "chunked を 2 行",
            post(
                &host,
                "HTTP/1.1",
                "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
                CHUNKED_BODY,
            ),
        ),
        (
            "xchunked と Content-Length",
            post(
                &host,
                "HTTP/1.1",
                "Content-Length: 5\r\nTransfer-Encoding: xchunked\r\n",
                "hello",
            ),
        ),
        (
            "HTTP/1.0 の chunked",
            post(
                &host,
                "HTTP/1.0",
                "Transfer-Encoding: chunked\r\n",
                CHUNKED_BODY,
            ),
        ),
    ];
    for (name, bytes) in &bad {
        let resp = send_raw(proxy_port, bytes);
        assert!(resp.starts_with("HTTP/1.1 400 "), "{}: {:?}", name, resp);
        // 応答は 1 つだけ (本文が次の要求として読まれていない)
        assert_eq!(resp.matches("HTTP/1.1 ").count(), 1, "{}: {:?}", name, resp);
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        seen.lock().unwrap().is_empty(),
        "400 にしたのにオリジンへ届いた: {:?}",
        seen.lock().unwrap()
    );
    let framing = metrics.rejected_requests
        [rust_http_proxy::metrics::BadRequestReason::BodyFraming as usize]
        .load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(framing, bad.len() as u64);

    // 最後が chunked なら今までどおり chunked として届く (符号化の名前の大小は問わない)
    for te in ["gzip, chunked", "Chunked"] {
        seen.lock().unwrap().clear();
        let resp = send_raw(
            proxy_port,
            &post(
                &host,
                "HTTP/1.1",
                &format!("Transfer-Encoding: {}\r\n", te),
                CHUNKED_BODY,
            ),
        );
        assert!(resp.starts_with("HTTP/1.1 200 "), "{}: {:?}", te, resp);
        wait_until(|| !seen.lock().unwrap().is_empty(), "オリジンへの到達");
        let got = seen.lock().unwrap()[0].clone();
        assert!(got.ends_with(CHUNKED_BODY), "{}: {:?}", te, got);
    }
}
