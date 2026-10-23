//! CONNECT にも要求の枠組みの検査を効かせる (T27.0)。
//!
//! CONNECT は `http::handle_http_with_headers` を通らずトンネルの経路に入るので、
//! T19 で入れた検査 (`Host` の重複・obs-fold・本文の枠組み) が**全部素通り**していた。
//! `0.25.3` の実機では、3 通りとも **200 Connection Established** でトンネルが張られ、
//! `Content-Length: 5` の本文 `hello` が**トンネルの最初のデータとして宛先に届いていた**
//! (エコーのオリジンが `[helloPING]` を返す = 要求の密輸の形)。
//!
//! ここで見るのは 4 つ:
//!
//! 1. `Host` 2 本 / obs-fold / `Content-Length` 付き / `Transfer-Encoding` 付きの CONNECT は
//!    **400** で、宛先には**接続もしない** (受け取ったバイトは 0、受理した本数も 0)
//! 2. 正常な CONNECT は今までどおり **200** で、ハンドシェイク後のバイトは素通し (エコー)
//! 3. `/status` の `rejected_requests` に理由別 (`dup_host` / `obs_fold` / `body_framing`) に乗る
//! 4. `/metrics` の `sorahost_rejected_requests_total{reason="…"}` も同じ数になる

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod common;
use common::*;

/// 受け取ったバイトを接続ごとに記録して、そのまま返すオリジン (エコー)。
///
/// 記録は**接続が閉じたとき**に 1 本ずつ積む (空でも積む)。「接続はしたが 1 バイトも
/// 来ていない」と「接続すらしていない」を区別できるようにするため。
fn start_recording_echo() -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let conns: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&conns);
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let sink = Arc::clone(&sink);
            thread::spawn(move || {
                let mut got = Vec::new();
                let mut buf = [0u8; 64 * 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            got.extend_from_slice(&buf[..n]);
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = stream.shutdown(std::net::Shutdown::Write);
                sink.lock().unwrap().push(got);
            });
        }
    });
    (port, conns)
}

/// 生のバイト列を投げて、相手が閉じるまで読む (400 で閉じる経路用)。
fn send_raw_to_end(proxy_port: u16, bytes: &[u8]) -> String {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _ = stream.write_all(bytes);
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// `/status` の `rejected_requests` から理由別の値だけを取り出す。
fn rejected(status: &str, reason: &str) -> u64 {
    let start = status
        .find("\"rejected_requests\":{")
        .unwrap_or_else(|| panic!("rejected_requests が無い: {}", status));
    let rest = &status[start..];
    let obj = &rest[..rest.find('}').unwrap()];
    let needle = format!("\"{}\":", reason);
    let at = obj
        .find(&needle)
        .unwrap_or_else(|| panic!("{} が無い: {}", reason, obj));
    let tail = &obj[at + needle.len()..];
    tail.chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} の値が読めない: {}", reason, obj))
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

/// 枠組みの壊れた CONNECT は 400 で断り、宛先には接続もしない。
#[test]
fn test_integration_connect_with_broken_framing_is_rejected() {
    let (origin_port, conns) = start_recording_echo();
    let (proxy_port, _metrics) = start_test_proxy_with_metrics(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    // (1) `Host` が 2 本。CONNECT の宛先は要求行の `host:port` で決まるが、
    //     RFC 9110 §7.2 は中継する側に 400 を求めている
    let dup = format!(
        "CONNECT {h} HTTP/1.1\r\nHost: {h}\r\nHost: evil.example\r\n\r\n",
        h = host
    );
    let resp = send_raw_to_end(proxy_port, dup.as_bytes());
    assert!(
        resp.starts_with("HTTP/1.1 400 "),
        "Host 2 本の CONNECT が 400 でない: {:?}",
        resp
    );
    // 400 を返してから閉じる (無言で切らない)
    assert!(resp.contains("Content-Length: 0"), "{:?}", resp);

    // (2) obs-fold (`X-A: 1` の次の行が ` 2`)
    let fold = format!(
        "CONNECT {h} HTTP/1.1\r\nHost: {h}\r\nX-A: 1\r\n 2\r\n\r\n",
        h = host
    );
    let resp = send_raw_to_end(proxy_port, fold.as_bytes());
    assert!(
        resp.starts_with("HTTP/1.1 400 "),
        "obs-fold の CONNECT が 400 でない: {:?}",
        resp
    );

    // (3) `Content-Length: 5` + 本文 `hello`。**本文のバイトがトンネルへ流れてはいけない**
    let with_body = format!(
        "CONNECT {h} HTTP/1.1\r\nHost: {h}\r\nContent-Length: 5\r\n\r\nhello",
        h = host
    );
    let resp = send_raw_to_end(proxy_port, with_body.as_bytes());
    assert!(
        resp.starts_with("HTTP/1.1 400 "),
        "本文付きの CONNECT が 400 でない: {:?}",
        resp
    );

    // (4) `Transfer-Encoding: chunked` 付き (本文の切れ目を相手に決めさせない)
    let chunked = format!(
        "CONNECT {h} HTTP/1.1\r\nHost: {h}\r\nTransfer-Encoding: chunked\r\n\r\n",
        h = host
    );
    let resp = send_raw_to_end(proxy_port, chunked.as_bytes());
    assert!(
        resp.starts_with("HTTP/1.1 400 "),
        "Transfer-Encoding 付きの CONNECT が 400 でない: {:?}",
        resp
    );

    // 宛先には**接続もしない** (受理した本数 0、受け取ったバイト 0)。少し待ってから見る
    thread::sleep(Duration::from_millis(200));
    let got = conns.lock().unwrap().clone();
    assert!(
        got.is_empty(),
        "400 にしたのに宛先へ届いた (接続 {} 本): {:?}",
        got.len(),
        got
    );

    // `/status` に理由別に乗る
    let status = status_json(proxy_port);
    assert_eq!(rejected(&status, "dup_host"), 1, "{}", status);
    assert_eq!(rejected(&status, "obs_fold"), 1, "{}", status);
    assert_eq!(rejected(&status, "body_framing"), 2, "{}", status);
    assert_eq!(rejected(&status, "request_line"), 0, "{}", status);
    assert_eq!(rejected(&status, "total"), 4, "{}", status);

    // `/metrics` も同じ理由の系列が同じ数
    let prom = endpoint_json(proxy_port, "/metrics");
    assert_eq!(prom_value(&prom, "dup_host"), 1);
    assert_eq!(prom_value(&prom, "obs_fold"), 1);
    assert_eq!(prom_value(&prom, "body_framing"), 2);
}

/// 正常な CONNECT は今までどおり 200 で、そのあとのバイトは素通し。
#[test]
fn test_integration_normal_connect_still_relays_after_handshake() {
    let (origin_port, conns) = start_recording_echo();
    let (proxy_port, _metrics) = start_test_proxy_with_metrics(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    let req = format!("CONNECT {h} HTTP/1.1\r\nHost: {h}\r\n\r\n", h = host);
    stream.write_all(req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 "), "{:?}", head);

    // ハンドシェイクのあとのバイトはそのままトンネルを通る
    stream.write_all(b"PING").unwrap();
    let mut back = [0u8; 4];
    stream.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"PING", "エコーが返らない: {:?}", back);
    drop(stream);

    wait_until(|| !conns.lock().unwrap().is_empty(), "宛先への到達");
    let got = conns.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "宛先の接続数: {:?}", got);
    assert_eq!(
        got[0],
        b"PING",
        "宛先が受け取ったバイトが違う: {:?}",
        String::from_utf8_lossy(&got[0])
    );

    // 正常な CONNECT は 400 の数に乗らない
    let status = status_json(proxy_port);
    assert_eq!(rejected(&status, "total"), 0, "{}", status);
}
