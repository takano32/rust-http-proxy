//! 裏側での再検証 (stale-while-revalidate)。
//!
//! 期限切れ直後 (grace 内) の要求には保存済みの表現をすぐ返し、このモジュールが別スレッドで
//! オリジンへ条件付き要求を送る。304 なら延命、新しい表現なら保存し直し、保存できない応答なら
//! 古い表現を捨てる。同じキーの再検証は同時に 1 本だけ、全体でも上限を設ける。
//!
//! # 走らせる場所は接続スレッドの置き場 (T11.3)
//!
//! 再検証は**自分でスレッドを起こさない**。T10.5 が `Workers` に入れた「生きているスレッドの
//! 上限」(`PROXY_MAX_THREADS`) の外にいると、stale-while-revalidate が集中したときに
//! T10.5 が防いだのと同じスレッドの山が起きるため。
//!
//! **上限に達しているときは待ち行列に積まず捨てる** (`Workers::try_run`)。理由は 3 つ:
//!
//! - 再検証は「後でやればいい仕事」で、**捨てても正しさは崩れない**
//!   (その項目は次の要求で普通のミス = 同期の再検証として取り直されるだけ)。
//! - 待ち行列は新しい接続と共用なので、積むと**その接続の処理が再検証の後ろに並ぶ**。
//! - 積むと、順番が来るまでキャッシュ側の「再検証中」の印を握り続ける
//!   (同じキーの再検証も、全体の 32 本の枠も、そのぶん詰まる)。
//!
//! 捨てた回数は `/status` の `revalidations_dropped` と `/metrics` の
//! `cache_revalidations_dropped_total` に出る。
//!
//! この形は**自分で釣り合う**: 接続がスレッドを使い切っているときは再検証が全部捨てられ
//! (要求の処理が優先される)、空きがあるときだけ裏で走る。

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{Shared, acquire_origin, read_response_head, request_head};
use crate::Upstream;
use crate::body::{BodyReader, Framing};
use crate::cache::{Cache, CacheKey, now_epoch};
use crate::freshness;
use crate::headers;
use crate::log_debug;
use crate::metrics::Metrics;
use crate::request::Origin;

/// 「このキーは裏で再検証中」の印の番人。落ちると印が消える。
///
/// 仕事がワーカーに渡らずに落ちても、走っている途中でパニックしても、`Drop` が必ず
/// 1 回だけ消す (以前は仕事の最後に `end_revalidation` を呼ぶだけだったので、
/// パニックすると印が残り、そのキーは二度と裏で再検証されなかった)。
/// T9.6 の `OpenGuard` と同じ形で、`Workers` が失敗した仕事を呼び出し元へ返す性質に乗っている。
struct Revalidating {
    cache: Arc<Cache>,
    key: CacheKey,
}

impl Drop for Revalidating {
    fn drop(&mut self) {
        self.cache.end_revalidation(self.key);
    }
}

/// 裏で再検証を始める。既に同じキーが再検証中、上限に達している、または
/// 接続スレッドに空きが無ければ false (呼び出し元は普通の (同期の) 再検証に回る)。
pub fn spawn(
    shared: &Shared<'_>,
    origin: &Origin,
    key: CacheKey,
    url: &str,
    cached_head: Vec<u8>,
    accept_encoding: Option<String>,
) -> bool {
    if !shared.cache.begin_revalidation(key) {
        return false;
    }
    // 印はここから番人が持つ (以後どの道を通っても 1 回だけ消える)
    let ticket = Revalidating {
        cache: Arc::clone(&shared.cache),
        key,
    };
    let upstream = Arc::clone(&shared.upstream);
    let metrics = Arc::clone(&shared.metrics);
    let timeout = shared.timeout;
    let origin = origin.clone();
    let url = url.to_string();
    let conn_id = shared.conn_id;
    let job = Box::new(move || {
        let cache: &Cache = &ticket.cache;
        let outcome = revalidate(
            cache,
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
        // ここで ticket が落ち、「再検証中」の印が消える
    });
    match shared.workers.try_run(job) {
        Ok(()) => true,
        Err(_returned) => {
            // 生きているスレッドが上限。**待ち行列には積まない** (モジュール冒頭の理由)。
            // 戻ってきた仕事はここで落ち、番人が印を消す
            shared
                .cache
                .revalidations_dropped
                .fetch_add(1, Ordering::Relaxed);
            log_debug!(
                Some(conn_id),
                "background revalidation skipped (no worker thread available)"
            );
            false
        }
    }
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

    let mut extra: Vec<String> = vec!["Via: 1.1 rust-http-proxy\r\n".to_string()];
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
            upstream.pool.put(&pool_key, server, timeout);
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
    let stored_head = sanitized.assemble(&[] as &[&str]);
    let expected = match framing {
        Framing::Length(n) => Some(n.saturating_add(stored_head.len() as u64)),
        _ => None,
    };
    let mut sink = cache.begin_store(key, url, p.ttl, p.age, p.validators, expected, conn_id);
    sink.write(&stored_head);
    let mut buf = super::CopyBuf::take();
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
        upstream.pool.put(&pool_key, server, timeout);
    }
    Ok("replaced with a fresh copy")
}
