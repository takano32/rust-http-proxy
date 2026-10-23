//! obs-fold (折り返したヘッダー) を 400 で断る (T27.2)。
//!
//! RFC 9112 §5.2 は、折り返したヘッダー (行頭が SP / HTAB の継続行) を中継する側に
//! 「400 で断る」か「SP 1 個に置き換える」かのどちらかを求めている。置き換えると
//! オリジンが受け取る値が黙って変わるので、こちらは**断る**方を採る。
//!
//! `0.26.0` までは継続行を `:` の無い行として読み飛ばしていたため、要求は 200 で通り、
//! オリジンには `X-A: 1` だけが届いていた (` 2` が黙って消える)。ここでは
//! **状態コードと、オリジンが実際に受け取ったヘッダーの両方**を固定する。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod common;
use common::*;

use rust_http_proxy::metrics::BadRequestReason;

/// 受け取った要求を 1 本ずつ記録するオリジン。
fn start_recording_origin(received: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let received = Arc::clone(&received);
            thread::spawn(move || {
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                received
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).into_owned());
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            });
        }
    });
    port
}

/// 生のバイト列を 1 本の接続に投げて、閉じるまで応答を読む。
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

/// 空白始まりとタブ始まりの継続行は、どちらも 400 で断り、オリジンへは 1 バイトも送らない。
#[test]
fn test_integration_obsolete_line_folding_is_refused() {
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let origin_port = start_recording_origin(Arc::clone(&received));
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    // (1) SP で折り返した `X-A: 1` + ` 2`
    let resp = send_raw(
        proxy_port,
        format!(
            "GET http://{}/x HTTP/1.1\r\nHost: {}\r\nX-A: 1\r\n 2\r\nConnection: close\r\n\r\n",
            host, host
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);

    // (2) HTAB で折り返した `X-A: 1` + `\t2`
    let resp = send_raw(
        proxy_port,
        format!(
            "GET http://{}/x HTTP/1.1\r\nHost: {}\r\nX-A: 1\r\n\t2\r\nConnection: close\r\n\r\n",
            host, host
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);

    // オリジンには 1 本も繋いでいない (継続行を捨てて 200 で通していた今までとの違い)
    thread::sleep(Duration::from_millis(200));
    let seen = received.lock().unwrap();
    assert!(seen.is_empty(), "オリジンに届いてはいけない: {:?}", *seen);

    // 理由は `obs_fold` で 2 件。`/status` と `/metrics` にも出る
    let count = |r: BadRequestReason| metrics.rejected_requests[r as usize].load(Ordering::Relaxed);
    assert_eq!(count(BadRequestReason::ObsFold), 2);
    assert_eq!(count(BadRequestReason::BodyFraming), 0);
    let status = status_json(proxy_port);
    assert!(status.contains("\"obs_fold\":2"), "{}", status);
    let prom = endpoint_json(proxy_port, "/metrics");
    assert!(
        prom.contains("sorahost_rejected_requests_total{reason=\"obs_fold\"} 2"),
        "{}",
        prom
    );
}

/// 折り返していないヘッダーは今までどおり 200 で通り、オリジンにそのまま届く。
#[test]
fn test_integration_plain_headers_still_pass() {
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let origin_port = start_recording_origin(Arc::clone(&received));
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let resp = send_raw(
        proxy_port,
        format!(
            "GET http://{}/x HTTP/1.1\r\nHost: {}\r\nX-A: 1\r\nX-B: 2\r\nConnection: close\r\n\r\n",
            host, host
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);

    thread::sleep(Duration::from_millis(200));
    let seen = received.lock().unwrap();
    assert_eq!(seen.len(), 1, "{:?}", *seen);
    let head = seen[0].split("\r\n\r\n").next().unwrap_or("");
    assert!(head.contains("\r\nX-A: 1\r\n"), "{:?}", head);
    assert!(head.contains("\r\nX-B: 2\r\n"), "{:?}", head);
}
