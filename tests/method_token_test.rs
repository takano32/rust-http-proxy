//! 要求行のメソッドが token でなければ 400 で断る結合テスト (T34.142)。
//!
//! RFC 9112 §3 の `method = token`。今までは要求行の形 (3 要素・版・対象の字) しか見ず、
//! `G\u{a0}ET /x HTTP/1.1` のような要求をそのままオリジンへ送っていた。断るときの理由は
//! 割れない要求行の先頭が token でないときと同じ `method` (`/status` の
//! `rejected_requests.method`、`/errors` の `bad_request:method`)。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

use rust_http_proxy::metrics::BadRequestReason;

/// token でないメソッドは、オリジンへ届かずに 400 になり、`method` に 1 ずつ数える。
#[test]
fn test_integration_a_method_that_is_not_a_token_is_refused_before_the_origin() {
    let hits = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&hits), "");
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    for method in ["G\u{a0}ET", "GE@T", "GET\u{1}"] {
        let req = format!("{method} http://{host}/x HTTP/1.1\r\nHost: {host}\r\n\r\n");
        let resp = raw_request(proxy_port, req.as_bytes());
        assert!(resp.starts_with("HTTP/1.1 400 "), "{method:?}: {resp:?}");
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "オリジンへは 1 本も届かない"
    );

    let count = |r: BadRequestReason| metrics.rejected_requests[r as usize].load(Ordering::Relaxed);
    assert_eq!(count(BadRequestReason::Method), 3);
    assert_eq!(count(BadRequestReason::RequestLine), 0);
    let errors = endpoint_json(proxy_port, "/errors");
    assert!(
        errors.contains("\"cause\":\"bad_request:method\""),
        "{}",
        errors
    );

    // token の字だけのメソッドは、知らないものでもこれまでどおり通す
    let req = format!("X~CUSTOM! http://{host}/x HTTP/1.1\r\nHost: {host}\r\n\r\n");
    let resp = raw_request(proxy_port, req.as_bytes());
    assert!(resp.starts_with("HTTP/1.1 200 "), "{resp:?}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(count(BadRequestReason::Method), 3);
}

/// 要求行が割れないとき (`Malformed`) も、区切りは ASCII の空白だけで読む。
/// NBSP だけで区切った行は 1 語で、それが token でないので `method` (今までは `request_line`)。
#[test]
fn test_integration_a_line_split_only_by_nbsp_is_blamed_on_the_method() {
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let resp = raw_request(proxy_port, "GET\u{a0}/x\r\n\r\n".as_bytes());
    assert!(resp.starts_with("HTTP/1.1 400 "), "{resp:?}");
    let count = |r: BadRequestReason| metrics.rejected_requests[r as usize].load(Ordering::Relaxed);
    assert_eq!(count(BadRequestReason::Method), 1);
    assert_eq!(count(BadRequestReason::RequestLine), 0);
}
