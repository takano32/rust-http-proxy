//! 不正な `Host` と authority を 400 にする (RFC 9112 §3.2。T.32.12)。
//!
//! 0.30.4 までは空と `@` しか見ず、`Host: a b`・`a, b`・`h:99999` などを名前解決まで行って 502 nxdomain
//! (80 番宛ての origin エラー) にし、`/errors`・`/hosts`・`/dns` に `http://a, b:80` などが並んでいた。
//! `http://h:+22501/` は Rust の `parse` が `+` を受けて通り、オリジンへ `Host: h:+22501` が届いていた。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

use rust_http_proxy::metrics::BadRequestReason;

#[test]
fn test_integration_invalid_host_and_authority_are_400() {
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let origin_form = [
        "example.invalid x",
        "a, b",
        "a/b",
        "a b:443",
        "exa mple.com",
        "127.0.0.1:99999",
    ];
    for host in origin_form {
        let resp = raw_request(
            proxy_port,
            format!(
                "GET /x HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                host
            )
            .as_bytes(),
        );
        assert!(
            resp.starts_with("HTTP/1.1 400 "),
            "Host: {:?}: {}",
            host,
            resp
        );
    }
    // 絶対形式のポートの `+` (オリジンへ `Host: 127.0.0.1:+22501` が届いていた)
    let resp = raw_request(
        proxy_port,
        b"GET http://127.0.0.1:+22501/ HTTP/1.1\r\nHost: 127.0.0.1:22501\r\nConnection: close\r\n\r\n",
    );
    assert!(resp.starts_with("HTTP/1.1 400 "), "{}", resp);

    let count = |r: BadRequestReason| metrics.rejected_requests[r as usize].load(Ordering::Relaxed);
    assert_eq!(count(BadRequestReason::NoHost), 6);
    assert_eq!(count(BadRequestReason::BadUri), 1);
    // 名前解決にも宛先の表にも行かない
    let hosts = endpoint_json(proxy_port, "/hosts");
    let dns = endpoint_json(proxy_port, "/dns");
    for name in ["invalid x", "a, b", "mple", "99999", "+22501"] {
        assert!(!hosts.contains(name), "/hosts に {:?}: {}", name, hosts);
        assert!(!dns.contains(name), "/dns に {:?}: {}", name, dns);
    }
    let errors = endpoint_json(proxy_port, "/errors");
    assert!(!errors.contains("nxdomain"), "{}", errors);
}

/// 正当な形 (大文字のホスト・空のポート・`[::1]:p`) は今までどおり通る。
#[test]
fn test_integration_valid_host_forms_pass() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let proxy_port = start_test_proxy(proxy_config());
    for host in [format!("LOCALHOST:{}", port), format!("127.0.0.1:{}", port)] {
        let resp = raw_request(
            proxy_port,
            format!(
                "GET /x HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                host
            )
            .as_bytes(),
        );
        assert!(
            resp.starts_with("HTTP/1.1 200 "),
            "Host: {}: {}",
            host,
            resp
        );
    }
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    // `[::1]:p` (IPv6 の待ち受けが作れる機械だけ)
    if let Ok(listener) = std::net::TcpListener::bind("[::1]:0") {
        let v6 = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for mut s in listener.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nv6",
                );
            }
        });
        let resp = raw_request(
            proxy_port,
            format!(
                "GET http://[::1]:{}/x HTTP/1.1\r\nHost: [::1]:{}\r\nConnection: close\r\n\r\n",
                v6, v6
            )
            .as_bytes(),
        );
        assert!(resp.starts_with("HTTP/1.1 200 "), "{}", resp);
    }
}
