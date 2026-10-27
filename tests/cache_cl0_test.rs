//! `Content-Length: 0` の GET もキャッシュを使う (T.29.5)。
//!
//! 0.29.35 までは長さ 0 の本文を「本文つきの GET」と見て、毎回オリジンへ行っていた
//! (`cache BYPASS`)。長さ 0 は本文が無いのと同じ。オリジンへは今までどおり `Content-Length: 0` を転送する。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

mod common;
use common::*;

fn origin(counter: Arc<AtomicUsize>, seen: Arc<Mutex<Vec<String>>>) -> u16 {
    start_origin(
        counter,
        Arc::new(move |req: &str, _| {
            seen.lock().unwrap().push(req.to_string());
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nCache-Control: max-age=600\r\n\
              Connection: close\r\n\r\nok"
                .to_vec()
        }),
    )
    .0
}

fn send(proxy_port: u16, method: &str, url: &str, host: &str) -> String {
    raw_get(
        proxy_port,
        &format!(
            "{method} {url} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    )
}

/// **受け入れ基準**: `Content-Length: 0` の GET は 2 回目が HIT。POST + CL: 0 は今までどおり毎回オリジンへ。
#[test]
fn test_integration_get_with_zero_length_body_uses_the_cache() {
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let port = origin(Arc::clone(&counter), Arc::clone(&seen));
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-cl0"));
    let host = format!("127.0.0.1:{}", port);
    let url = format!("http://{}/x", host);

    let first = send(proxy_port, "GET", &url, &host);
    assert!(first.ends_with("\r\n\r\nok"), "{}", first);
    let second = send(proxy_port, "GET", &url, &host);
    assert!(
        second.contains("X-Cache: HIT"),
        "2 回目が HIT でない: {}",
        second
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    // オリジンへは `Content-Length: 0` をそのまま送っている
    assert!(
        seen.lock().unwrap()[0].contains("\r\nContent-Length: 0\r\n"),
        "{:?}",
        seen.lock().unwrap()
    );

    // POST + CL: 0 は保存も参照もしない (毎回オリジンへ)
    let post_url = format!("http://{}/p", host);
    for _ in 0..2 {
        let resp = send(proxy_port, "POST", &post_url, &host);
        assert!(!resp.contains("X-Cache"), "{}", resp);
    }
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}
