//! `Host` が 2 本以上ある要求を 400 で断ることの結合テスト (T19.2)。
//!
//! RFC 9110 §7.2 は、`Host` を 2 本以上受けた中継する側に 400 を求めている。ここまでは
//! `parse_request_headers` が**最後の 1 本を黙って上書き**していたので、
//! `Host: 127.0.0.1:P` と `Host: evil.example` を並べた要求が 200 で通り、オリジンには
//! 要求先から作り直した `Host` が 1 本だけ届いていた (前段と後段で「どの `Host` を信じるか」が
//! 食い違う = 宛先の付け替えになる)。
//!
//! ここで見るのは 3 つ:
//!
//! 1. 2 本並べた要求は **400** で、オリジンには**届かない**
//! 2. 1 本だけの要求は今までどおり **200** で、オリジンが受け取る `Host` は**1 本だけ**
//! 3. `/status` の `rejected_requests.dup_host` と `/metrics` の
//!    `sorahost_rejected_requests_total{reason="dup_host"}` が 1 増える

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

/// 受け取った要求の全文を貯めるオリジン (`start_origin` のハンドラに渡す)。
fn capture_origin() -> (u16, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let (port, _handle) = start_origin(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(move |req, _n| {
            sink.lock().unwrap().push(req.to_string());
            let body = "hello from mock origin";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    );
    (port, seen)
}

/// 要求の全文から `Host:` の行だけを取り出す (名前の大小は無視する)。
fn host_lines(req: &str) -> Vec<String> {
    req.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("host")
                .then(|| v.trim().to_string())
        })
        .collect()
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

/// `Host` 2 本 → 400 でオリジンへ届かない。1 本 → 今までどおり 200 で `Host` は 1 本。
#[test]
fn test_integration_duplicate_host_is_rejected() {
    let (origin_port, seen) = capture_origin();
    let (proxy_port, _metrics) = start_test_proxy_with_metrics(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    // (1) `Host` を 2 本並べる。要求先 (target) は 2 本目とは別の宛先を指す
    let dup = format!(
        "GET http://{}/dup HTTP/1.1\r\nHost: {}\r\nHost: evil.example\r\n\
         Connection: close\r\n\r\n",
        host, host
    );
    let resp = send_raw(proxy_port, dup.as_bytes());
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);
    // 400 を返してから閉じる (無言で切らない)
    assert!(resp.contains("Content-Length: 0"), "{:?}", resp);
    // オリジンには 1 バイトも届いていない
    std::thread::sleep(Duration::from_millis(200));
    let before = seen.lock().unwrap().clone();
    assert!(
        before.is_empty(),
        "400 にしたのにオリジンへ届いた: {:?}",
        before
    );

    // (2) `Host` が 1 本の要求は今までどおり通る
    let single = format!(
        "GET http://{}/once HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        host, host
    );
    let resp = send_raw(proxy_port, single.as_bytes());
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);
    wait_until(|| !seen.lock().unwrap().is_empty(), "オリジンへの到達");
    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "オリジンに届いた要求: {:?}", got);
    let hosts = host_lines(&got[0]);
    assert_eq!(
        hosts,
        vec![host.clone()],
        "オリジンが受け取った Host: {:?}",
        hosts
    );

    // (3) `/status` の理由別 (`dup_host` だけ 1、ほかは 0)
    let status = status_json(proxy_port);
    assert!(
        status.contains(
            "\"rejected_requests\":{\"request_line\":0,\"header_too_large\":0,\"method\":0,\
             \"no_host\":0,\"bad_uri\":0,\"body_framing\":0,\"obs_fold\":0,\"dup_host\":1,\"bad_header\":0,\"header_timeout\":0,\
             \"total\":1}"
        ),
        "{}",
        status
    );

    // `/errors` の個票にも `bad_request:dup_host` が 1 件 (要求行も Host の値も残さない)
    let errors = endpoint_json(proxy_port, "/errors");
    assert!(
        errors.contains("\"cause\":\"bad_request:dup_host\""),
        "{}",
        errors
    );
    assert!(!errors.contains("evil.example"), "{}", errors);

    // `/metrics` も同じ理由の系列が 1 本
    let prom = endpoint_json(proxy_port, "/metrics");
    assert_eq!(prom_value(&prom, "dup_host"), 1);
    assert_eq!(prom_value(&prom, "no_host"), 0);
    assert_eq!(prom_value(&prom, "body_framing"), 0);
}
