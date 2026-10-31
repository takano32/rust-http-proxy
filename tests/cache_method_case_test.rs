//! メソッドの大小 (T.32.8): `get` の応答を GET の鍵に保存して、他の利用者の `GET` に配らない。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

/// 大小を区別するオリジン (Python の `http.server` と同じ): `GET` / `HEAD` には本物の 200、
/// それ以外 (`get`・`head`) には `max-age=600` 付きの 501。
fn case_sensitive_origin(counter: Arc<AtomicUsize>) -> u16 {
    start_origin(
        counter,
        Arc::new(|req, _| {
            if req.starts_with("GET ") {
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nCache-Control: max-age=600\r\nConnection: close\r\n\r\nreal".to_vec()
            } else if req.starts_with("HEAD ") {
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nCache-Control: max-age=600\r\nConnection: close\r\n\r\n".to_vec()
            } else {
                b"HTTP/1.1 501 Not Implemented\r\nContent-Length: 6\r\nCache-Control: max-age=600\r\nConnection: close\r\n\r\npoison".to_vec()
            }
        }),
    )
    .0
}

fn send(proxy_port: u16, method: &str, url: &str, host: &str) -> String {
    raw_get(
        proxy_port,
        &format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            method, url, host
        ),
    )
}

#[test]
fn test_integration_lowercase_get_does_not_poison_the_get_key() {
    let counter = Arc::new(AtomicUsize::new(0));
    let port = case_sensitive_origin(Arc::clone(&counter));
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-method-case"));
    let url = format!("http://127.0.0.1:{}/index.html", port);
    let host = format!("127.0.0.1:{}", port);

    // 攻撃者の `get` はオリジンへ `get` のまま届いて 501 (保存も参照もしない)
    let attacker = send(proxy_port, "get", &url, &host);
    assert!(attacker.starts_with("HTTP/1.1 501 "), "{}", attacker);
    assert!(!attacker.contains("X-Cache"), "{}", attacker);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    // 続く正しい `GET` は MISS でオリジンの本物の 200
    let victim = send(proxy_port, "GET", &url, &host);
    assert!(victim.starts_with("HTTP/1.1 200 "), "{}", victim);
    assert!(victim.ends_with("real"), "{}", victim);
    assert!(!victim.contains("X-Cache"), "{}", victim);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    // `GET` / `HEAD` の HIT は今のまま
    let hit = send(proxy_port, "GET", &url, &host);
    assert!(hit.contains("X-Cache: HIT"), "{}", hit);
    assert!(hit.ends_with("real"), "{}", hit);
    let head = send(proxy_port, "HEAD", &url, &host);
    assert!(head.contains("X-Cache: HIT"), "{}", head);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    // `get` は保存済みを参照しない (オリジンへ行く)。安全でないメソッドとして項目は無効化される
    let again = send(proxy_port, "get", &url, &host);
    assert!(again.starts_with("HTTP/1.1 501 "), "{}", again);
    assert!(!again.contains("X-Cache: HIT"), "{}", again);
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

/// `head` は HEAD 扱いにしない (本文を返すオリジンの応答を先頭だけで切らない)。
#[test]
fn test_integration_lowercase_head_is_not_treated_as_head() {
    let counter = Arc::new(AtomicUsize::new(0));
    let port = case_sensitive_origin(Arc::clone(&counter));
    let proxy_port = start_test_proxy(proxy_config());
    let url = format!("http://127.0.0.1:{}/h", port);
    let host = format!("127.0.0.1:{}", port);
    let r = send(proxy_port, "head", &url, &host);
    assert!(r.starts_with("HTTP/1.1 501 "), "{}", r);
    assert!(r.ends_with("\r\n\r\npoison"), "{}", r);
}
