//! `PROXY_CACHE_ENABLED=off` ではバリアントの表 (URL → 保存したバリアントの鍵) が伸びないこと (T34.158)。
//!
//! `Cache::begin_store` は `enabled` を見る前に `remember_variant` を呼ぶ。呼ぶ側
//! (`http/deliver.rs` の保存・`http/lookup.rs` のヒット・`http/refresh.rs` の裏の再検証) が
//! `enabled` を見ていなければ、保存しないのに表だけが伸びる。本物の配線で要求を流して確かめる。
//! 表の行の数は `/status` に出ないので、`Cache` を自分で持って `serve` に渡す。

mod common;

use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::thread;
use std::time::Duration;

use common::*;
use rust_http_proxy::Upstream;
use rust_http_proxy::cache::{Cache, CacheConfig, MIB};
use rust_http_proxy::metrics::Metrics;
use rust_http_proxy::pool::Pool;

/// `cache` を持たせたプロキシを起こし、待ち受けポートを返す。
fn start_proxy(cache: Arc<Cache>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let cfg = Arc::new(proxy_config());
    let metrics = Arc::new(Metrics::new());
    let pool = Arc::new(Upstream {
        pool: Pool::new(cfg.pool_per_host, Duration::from_secs(30)),
        tls: None,
    });
    let workers = Arc::new(rust_http_proxy::workers::Workers::new(cfg.max_threads));
    thread::spawn(move || {
        rust_http_proxy::serve(
            listener,
            || Arc::clone(&cfg),
            rust_http_proxy::Limiter::new(),
            workers,
            metrics,
            cache,
            pool,
            None,
        )
    });
    port
}

/// 保存できる (`max-age=600`、ETag 付き) 応答と、毎回の再検証が要る (`max-age=0`) 応答を返すオリジン。
fn start_variant_origin() -> u16 {
    let (port, _) = start_origin(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(|req, _n| {
            let cc = if req.contains("/stale") {
                "max-age=0"
            } else {
                "max-age=600"
            };
            let body = "variant body";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCache-Control: {}\r\nETag: \"v1\"\r\n\
                 Vary: Accept-Encoding\r\nConnection: close\r\n\r\n{}",
                body.len(),
                cc,
                body
            )
            .into_bytes()
        }),
    );
    port
}

/// バリアントの付く要求 (`Accept-Encoding` 付き) を、保存・ヒット・再検証の形で流す。
fn drive(proxy: u16, origin: u16) {
    let host = format!("127.0.0.1:{}", origin);
    for path in ["/a", "/b", "/stale"] {
        let url = format!("http://{}{}", host, path);
        for enc in ["gzip", "br", "gzip, deflate"] {
            for _ in 0..2 {
                let resp = get_via_proxy_with(
                    proxy,
                    &url,
                    &host,
                    &format!("Accept-Encoding: {}\r\n", enc),
                );
                assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);
            }
        }
    }
}

#[test]
fn the_variant_table_does_not_grow_while_the_cache_is_off() {
    let cache = Arc::new(Cache::new(CacheConfig::disabled()));
    let proxy = start_proxy(Arc::clone(&cache));
    drive(proxy, start_variant_origin());
    assert_eq!(
        cache.variants_len(),
        0,
        "キャッシュを切っているのに表が伸びた"
    );
    assert_eq!(cache.counters().stores(), 0);
}

/// 上の試験が「伸びれば気づける」ことの裏付け: 同じ要求をキャッシュを入れて流すと表は伸びる。
#[test]
fn the_same_requests_grow_the_table_when_the_cache_is_on() {
    let dir = std::env::temp_dir().join(format!("shp-test-variants-on-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cache = Arc::new(Cache::new(CacheConfig {
        default_ttl: Duration::from_secs(60),
        ..CacheConfig::fixed(16 * MIB, 64 * MIB, dir.clone())
    }));
    let proxy = start_proxy(Arc::clone(&cache));
    drive(proxy, start_variant_origin());
    assert!(cache.variants_len() > 0, "キャッシュを入れても表が伸びない");
    drop(cache);
    let _ = std::fs::remove_dir_all(&dir);
}
