//! 400 の理由別カウンタと個票の結合テスト (T14.28)。
//!
//! 公開ポートには走査 (scanner) の要求が来る。今までは 400 / 414 / 431 で閉じるだけで
//! **何が来たか**の数が無かったので、理由別に数えて (`/status` の `rejected_requests`、
//! `/metrics` の `sorahost_rejected_requests_total{reason=}`)、`/errors` に個票
//! (`cause` が `bad_request:<reason>`) を残す。
//!
//! **個票に要求行そのものは入れない** (個票の決まり: 接続元 IP・宛先・時刻・数字だけ)。
//! 壊れた要求行にも URL やヘッダーが載っているので、`target` は空のままにしてある。
//!
//! 数えるのは**断る経路だけ**なので、通した要求は 1 つも増やさない
//! (最後のテストで「成功の経路は 0 増」を実際に確かめている)。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

mod common;
use common::*;

use rust_http_proxy::metrics::{BAD_REQUEST_REASON_NAMES, BadRequestReason};

/// ヘッダーの合計の上限 (`MAX_HEADER_BYTES` = 128 KiB) を越える要求。
///
/// 1 行の上限 (`MAX_LINE` = 64 KiB) と行数の上限 (256 行) には当てずに、
/// **合計だけ**で越えさせる (受け入れ基準の「上限 + 1」の形)。
fn oversized_headers() -> Vec<u8> {
    let mut req = b"GET http://example.invalid/ HTTP/1.1\r\nHost: example.invalid\r\n".to_vec();
    // 45,011 B/行 × 3 行 = 135,033 B > 131,072 B。3 行目を読んだところで越える
    for i in 0..3 {
        req.extend_from_slice(format!("X-Pad-{}: ", i).as_bytes());
        req.extend(std::iter::repeat_n(b'a', 45_000));
        req.extend_from_slice(b"\r\n");
    }
    req.extend_from_slice(b"\r\n");
    req
}

/// 生のバイト列を 1 本の接続に投げて応答を読む (書き込みの失敗は無視する:
/// 長すぎるヘッダーは相手が途中で閉じることがある)。
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

/// `reason` の系列の値 (`/metrics` の `sorahost_rejected_requests_total{reason="…"}`)。
fn prom_value(text: &str, reason: &str) -> u64 {
    let needle = format!("sorahost_rejected_requests_total{{reason=\"{}\"}} ", reason);
    let line = text
        .lines()
        .find(|l| l.starts_with(&needle))
        .unwrap_or_else(|| panic!("{} の系列が無い", reason));
    line[needle.len()..].trim().parse().unwrap()
}

/// 受け入れ基準の 3 つ (壊れた要求行・長すぎるヘッダー・`Host` 無しの HTTP/1.1) を送ると、
/// それぞれの理由が 1 ずつ増え、`/errors` に 3 件、`/metrics` に 3 本の系列が 1 になる。
#[test]
fn test_integration_bad_requests_are_counted_by_reason() {
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());

    // (1) 要求行が空白で 2 つに割れない → `request_line` (400)
    let resp = send_raw(proxy_port, b"GARBAGE\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);

    // (2) ヘッダーの合計が上限を越える → `header_too_large` (431)
    let resp = send_raw(proxy_port, &oversized_headers());
    assert!(resp.starts_with("HTTP/1.1 431 "), "{:?}", resp);

    // (3) オリジン形式なのに `Host` が無い → `no_host` (400)
    let resp = send_raw(proxy_port, b"GET /somewhere HTTP/1.1\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);

    // それぞれ 1 ずつ、ほかの 3 種は 0 のまま
    let count = |r: BadRequestReason| metrics.rejected_requests[r as usize].load(Ordering::Relaxed);
    assert_eq!(count(BadRequestReason::RequestLine), 1);
    assert_eq!(count(BadRequestReason::HeaderTooLarge), 1);
    assert_eq!(count(BadRequestReason::NoHost), 1);
    assert_eq!(count(BadRequestReason::Method), 0);
    assert_eq!(count(BadRequestReason::BadUri), 0);
    assert_eq!(count(BadRequestReason::BodyFraming), 0);

    // `/status` の欄 (理由別と合計)
    let status = status_json(proxy_port);
    assert!(
        status.contains(
            "\"rejected_requests\":{\"request_line\":1,\"header_too_large\":1,\"method\":0,\
             \"no_host\":1,\"bad_uri\":0,\"body_framing\":0,\"total\":3}"
        ),
        "{}",
        status
    );

    // `/errors` の個票 3 件 (`cause` は `bad_request:<reason>`、接続元は入るが**要求行は入らない**)
    let errors = endpoint_json(proxy_port, "/errors");
    for (cause, status_code) in [
        ("bad_request:request_line", 400),
        ("bad_request:header_too_large", 431),
        ("bad_request:no_host", 400),
    ] {
        let row = format!(
            "\"target\":\"\",\"cause\":\"{}\",\"dns_ms\":0,\"connect_ms\":0,\"status\":{},\"client\":\"127.0.0.1\"",
            cause, status_code
        );
        assert!(errors.contains(&row), "{} が無い: {}", cause, errors);
    }
    assert!(errors.contains("\"count\":3,"), "{}", errors);
    // 要求行 (とその中の URL) は 1 文字も残っていない
    assert!(!errors.contains("GARBAGE"), "{}", errors);
    assert!(!errors.contains("somewhere"), "{}", errors);
    assert!(!errors.contains("example.invalid"), "{}", errors);

    // `/metrics` の 6 本 (立った 3 本が 1、残りは 0)
    let prom = endpoint_json(proxy_port, "/metrics");
    assert!(
        prom.contains("# TYPE sorahost_rejected_requests_total counter\n"),
        "{}",
        prom
    );
    for name in BAD_REQUEST_REASON_NAMES {
        let want = u64::from(matches!(
            name,
            "request_line" | "header_too_large" | "no_host"
        ));
        assert_eq!(prom_value(&prom, name), want, "{}", name);
    }
}

/// 残りの 3 種 (`method` / `bad_uri` / `body_framing`) も、それぞれの形で 1 ずつ増える。
#[test]
fn test_integration_the_other_three_reasons() {
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());

    // メソッドが HTTP の token として読めない (走査が投げるゴミ) → `method`
    let resp = send_raw(proxy_port, b"\x01\x02\x03\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);

    // 絶対 URI にホストが無い → `bad_uri`
    let resp = send_raw(proxy_port, b"GET http:/// HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);

    // `Content-Length` と `Transfer-Encoding: chunked` が両方ある (要求の密輸) → `body_framing`
    let (origin_port, _origin) = start_mock_origin();
    let req = format!(
        "POST http://127.0.0.1:{}/x HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
         Content-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        origin_port, origin_port
    );
    let resp = send_raw(proxy_port, req.as_bytes());
    assert!(resp.starts_with("HTTP/1.1 400 "), "{:?}", resp);

    let count = |r: BadRequestReason| metrics.rejected_requests[r as usize].load(Ordering::Relaxed);
    assert_eq!(count(BadRequestReason::Method), 1);
    assert_eq!(count(BadRequestReason::BadUri), 1);
    assert_eq!(count(BadRequestReason::BodyFraming), 1);
    assert_eq!(count(BadRequestReason::RequestLine), 0);
    assert_eq!(count(BadRequestReason::NoHost), 0);

    let errors = endpoint_json(proxy_port, "/errors");
    for cause in [
        "bad_request:method",
        "bad_request:bad_uri",
        "bad_request:body_framing",
    ] {
        assert!(
            errors.contains(&format!("\"cause\":\"{}\"", cause)),
            "{} が無い: {}",
            cause,
            errors
        );
    }
}

/// **成功の経路は 0 増**: 通した要求 (転送と内部エンドポイント) では 1 つも増えない。
#[test]
fn test_integration_successful_requests_do_not_count() {
    let (origin_port, _origin) = start_mock_origin();
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);

    // 転送 (keep-alive で 3 要求)、CONNECT ではない普通の GET
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    for _ in 0..3 {
        let (head, _body) = one_keepalive_request(&mut stream, &host, "/hello");
        assert!(head.starts_with("HTTP/1.1 200 "), "{}", head);
    }
    // 本文のある要求 (`Content-Length` だけ) も枠は正しいので通る
    let resp = send_raw(
        proxy_port,
        format!(
            "POST http://{}/x HTTP/1.1\r\nHost: {}\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            host, host
        )
        .as_bytes(),
    );
    assert!(resp.starts_with("HTTP/1.1 200 "), "{:?}", resp);

    // 内部エンドポイント (`/status` は自分宛て) も数えない
    let status = status_json(proxy_port);
    assert!(status.contains("\"status\":\"ok\""), "{}", status);

    let total: u64 = metrics
        .rejected_requests
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .sum();
    assert_eq!(total, 0, "成功の経路で数えてはいけない: {}", status);
    assert!(
        status.contains(
            "\"rejected_requests\":{\"request_line\":0,\"header_too_large\":0,\"method\":0,\
             \"no_host\":0,\"bad_uri\":0,\"body_framing\":0,\"total\":0}"
        ),
        "`/status` の合計も 0: {}",
        status
    );
}
