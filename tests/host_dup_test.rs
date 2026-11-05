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

use std::time::Duration;

mod common;
use common::*;

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

/// 要求を貯めるオリジンの応答 (`start_mock_origin` と同じ本文。T.34.110 で定数にした)。
const HELLO: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 22\r\nConnection: close\r\n\r\nhello from mock origin";

/// `Host` 2 本 → 400 でオリジンへ届かない。1 本 → 今までどおり 200 で `Host` は 1 本。
#[test]
fn test_integration_duplicate_host_is_rejected() {
    let (origin_port, seen) = capture_origin_replying(HELLO);
    let (proxy_port, _metrics) = start_test_proxy_with_metrics(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    // (1) `Host` を 2 本並べる。要求先 (target) は 2 本目とは別の宛先を指す
    let dup = format!(
        "GET http://{}/dup HTTP/1.1\r\nHost: {}\r\nHost: evil.example\r\n\
         Connection: close\r\n\r\n",
        host, host
    );
    let resp = raw_request_with(proxy_port, dup.as_bytes(), Duration::from_secs(5), false);
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);
    // 400 を返してから閉じる (無言で切らない)
    assert!(resp.contains("Content-Length: 0"), "{:?}", resp);
    // オリジンには 1 バイトも届いていない (印の要求が届くまで待ってから見る。T.34.114)
    settle_with_mark(proxy_port, origin_port, &seen);
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
    let resp = raw_request_with(proxy_port, single.as_bytes(), Duration::from_secs(5), false);
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
