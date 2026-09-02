//! 裏側での再検証 (stale-while-revalidate)。
//!
//! 期限切れ直後 (grace 内) の要求には保存済みの表現をすぐ返し、このモジュールが別スレッドで
//! オリジンへ条件付き要求を送る。304 なら延命、新しい表現なら保存し直し、保存できない応答なら
//! 古い表現を捨てる。同じキーの再検証は同時に 1 本だけ、全体でも上限を設ける。

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use super::request::Origin;
use super::{COPY_BUF_SIZE, Shared, acquire_origin, read_response_head, request_head};
use crate::Upstream;
use crate::body::{BodyReader, Framing};
use crate::cache::{Cache, CacheKey, now_epoch};
use crate::freshness;
use crate::headers;
use crate::log_debug;
use crate::metrics::Metrics;

/// 裏で再検証を始める。既に同じキーが再検証中、または上限に達していれば false。
pub fn spawn(
    shared: &Shared,
    origin: &Origin,
    key: CacheKey,
    url: &str,
    cached_head: Vec<u8>,
    accept_encoding: Option<String>,
) -> bool {
    if !shared.cache.begin_revalidation(key) {
        return false;
    }
    let cache = Arc::clone(&shared.cache);
    let upstream = Arc::clone(&shared.upstream);
    let metrics = Arc::clone(&shared.metrics);
    let timeout = shared.timeout;
    let origin = origin.clone();
    let url = url.to_string();
    let conn_id = shared.conn_id;
    let spawned = thread::Builder::new()
        .name("revalidate".into())
        .spawn(move || {
            let outcome = revalidate(
                &cache,
                &upstream,
                &metrics,
                timeout,
                &origin,
                key,
                &url,
                &cached_head,
                accept_encoding.as_deref(),
                conn_id,
            );
            match outcome {
                Ok(what) => log_debug!(
                    Some(conn_id),
                    "background revalidation of {} -> {}",
                    url,
                    what
                ),
                Err(e) => log_debug!(
                    Some(conn_id),
                    "background revalidation of {} failed: {} (stale entry kept)",
                    url,
                    e
                ),
            }
            cache.end_revalidation(key);
        })
        .is_ok();
    if !spawned {
        shared.cache.end_revalidation(key);
    }
    spawned
}

#[allow(clippy::too_many_arguments)]
fn revalidate(
    cache: &Cache,
    upstream: &Upstream,
    metrics: &Metrics,
    timeout: Duration,
    origin: &Origin,
    key: CacheKey,
    url: &str,
    cached_head: &[u8],
    accept_encoding: Option<&str>,
    conn_id: usize,
) -> io::Result<&'static str> {
    let cached = freshness::parse_cached_head(cached_head);
    let pool_key = origin.pool_key();
    let (mut server, reused) = acquire_origin(upstream, timeout, conn_id, origin, &pool_key)?;
    metrics.inc_origin_conn(reused);

    let mut extra: Vec<String> = vec!["Via: 1.1 sorahost-http-proxy\r\n".to_string()];
    if let Some(ae) = accept_encoding {
        extra.push(format!("Accept-Encoding: {}\r\n", ae));
    }
    let head = request_head(
        "GET",
        origin,
        &extra,
        &freshness::conditional_headers(&cached),
    );
    server.get_mut().write_all(&head)?;
    server.get_mut().flush()?;

    let (rhead, status, rheaders) = read_response_head(&mut server)?;
    let framing = Framing::of_response(status, false, &rheaders);
    let reusable = rhead.starts_with(b"HTTP/1.1")
        && framing != Framing::Close
        && !rheaders.iter().any(|(k, v)| {
            k == "connection" && v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close"))
        });
    let now = now_epoch();
    let cfg = cache.config();

    if status == 304 {
        let p = freshness::revalidated_policy(&rheaders, &cached, cfg, now);
        cache.refresh(key, p.ttl, p.age, conn_id);
        cache
            .background_revalidations
            .fetch_add(1, Ordering::Relaxed);
        if reusable {
            upstream.pool.put(&pool_key, server);
        }
        return Ok("304, refreshed");
    }
    if !(200..400).contains(&status) {
        // オリジン側の問題: 古い表現は残す
        return Ok("origin error, stale kept");
    }
    let Some(p) = freshness::response_policy(status, &rheaders, cfg, now) else {
        cache.remove(key);
        return Ok("not cacheable any more, removed");
    };
    let sanitized = headers::sanitize_response_head(&rhead);
    let stored_head = sanitized.assemble(&[]);
    let expected = match framing {
        Framing::Length(n) => Some(n.saturating_add(stored_head.len() as u64)),
        _ => None,
    };
    let mut sink = cache.begin_store(key, url, p.ttl, p.age, p.validators, expected, conn_id);
    sink.write(&stored_head);
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    let clean = {
        let mut body = BodyReader::new(&mut server, framing);
        loop {
            match body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => sink.write(&buf[..n]),
                Err(e) => {
                    sink.abort();
                    return Err(e);
                }
            }
        }
        body.finished_cleanly()
    };
    if !clean {
        sink.abort();
        return Ok("truncated, stale kept");
    }
    sink.finish();
    cache
        .background_revalidations
        .fetch_add(1, Ordering::Relaxed);
    if reusable {
        upstream.pool.put(&pool_key, server);
    }
    Ok("replaced with a fresh copy")
}
