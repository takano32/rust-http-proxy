//! オリジンの応答の読みの締め切りを `timeout` に数える (T.28.5)。
//!
//! オリジンの読みはブロッキング + `SO_RCVTIMEO` で、Linux では締め切りが切れると
//! `WouldBlock` (EAGAIN = os error 11) が返る。0.27.8 までは `ErrCause::from_io` が
//! `TimedOut` しか見ていなかったので `errors_by_cause` の `other` に数え、ログは
//! `failed to read origin response: Resource temporarily unavailable (os error 11)` だった。

use std::io::Read;
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

mod common;
use common::*;

/// 要求を読んだあと `silent` だけ黙り、何も返さずに閉じるオリジン。
fn start_silent_origin(silent: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                thread::sleep(silent);
            });
        }
    });
    port
}

/// **受け入れ基準 (T.28.5)**: 2 秒の締め切りに対し 3 秒黙るオリジンは 502 で、
/// `errors_by_cause` の `timeout` に入り、ログに "timed out" が出る。
#[cfg(target_os = "linux")]
#[test]
fn test_integration_silent_origin_counts_as_timeout() {
    let origin_port = start_silent_origin(Duration::from_secs(3));
    let mut cfg = proxy_config();
    cfg.timeout = Duration::from_secs(2);
    let proxy_port = start_test_proxy(cfg);

    let started = Instant::now();
    let resp = get_via_proxy(
        proxy_port,
        &format!("http://127.0.0.1:{}/slow", origin_port),
        &format!("127.0.0.1:{}", origin_port),
    );
    let took = started.elapsed();
    assert!(resp.starts_with("HTTP/1.1 502 "), "{:?}", resp);
    // 締め切り (2 秒) で切れる。オリジンが閉じる 3 秒までは待たない
    assert!(
        took >= Duration::from_millis(1900) && took < Duration::from_millis(2900),
        "{:?}",
        took
    );

    let prom = endpoint_json(proxy_port, "/metrics");
    assert!(
        prom.contains(r#"sorahost_errors_total{cause="timeout"} 1"#),
        "{}",
        prom
    );
    assert!(
        !prom.contains(r#"sorahost_errors_total{cause="other"} 1"#),
        "{}",
        prom
    );
    let errors = endpoint_json(proxy_port, "/errors?n=10");
    assert!(errors.contains("\"cause\":\"timeout\""), "{}", errors);

    let log = endpoint_json(proxy_port, "/log");
    assert!(
        log.contains("origin response timed out after 2 s"),
        "締め切りの行が無い: {}",
        log
    );
    assert!(!log.contains("os error 11"), "{}", log);
}
