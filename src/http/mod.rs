//! HTTP (平文) リクエストの転送とキャッシュの適用。
//!
//! 流れ: リクエスト解析 ([`request`]) → キャッシュ参照 (新鮮なら配信 [`serve`]、期限切れでも
//! バリデータ付きなら再検証用に保持) → オリジンへ転送 (プールの接続を再利用、必要なら条件付き)
//! → 応答本文を解読しつつクライアントへ自前の枠組みで配信し、同時にストリーミングで保存。
//! 304 なら保存済みの表現を延命して配信し、オリジン障害時は stale を配信する。

mod refresh;
mod request;
mod serve;
#[cfg(test)]
mod tests;

pub use request::{Origin, map_locations, parse_origin};
pub use serve::{Serve, write_cached_response};

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::Upstream;

use crate::body::{self, BodyReader, Framing};
use crate::cache::{
    Cache, CacheSource, CachedResponse, FetchOutcome, FetchTicket, cache_key_variant, now_epoch,
};
use crate::freshness;
use crate::headers;
use crate::log::{Access, access};
use crate::metrics::{HostOutcome, Metrics};
use crate::origin::{self, OriginStream};
use crate::{log_debug, log_trace, log_warn};

use request::{RequestHeaders, parse_request_headers};
use serve::{can_serve_stale, serve_cached};

pub(super) const COPY_BUF_SIZE: usize = 64 * 1024;
/// オリジンがこのステータスを返したら stale を配信する (RFC 5861 stale-if-error 相当)。
const STALE_ON_STATUS: &[u16] = &[500, 502, 503, 504];

/// (生バイト列, ステータスコード, 小文字化したヘッダー名と値の組)
pub type ResponseHead = (Vec<u8>, u16, Vec<(String, String)>);

/// 接続をまたいで共有する状態。
pub struct Shared {
    pub timeout: Duration,
    pub keepalive: Duration,
    pub conn_id: usize,
    pub metrics: Arc<Metrics>,
    pub cache: Arc<Cache>,
    /// 接続プールと TLS クライアント
    pub upstream: Arc<Upstream>,
}

/// アクセスログと配信に必要なリクエストの文脈。
struct Ctx<'a> {
    client_ip: &'a str,
    method: &'a str,
    url: &'a str,
    version: &'a str,
    started: Instant,
    conn_id: usize,
    metrics: &'a Metrics,
    req: &'a RequestHeaders,
    /// 応答後もクライアント接続を維持できるか
    keep_client: bool,
    head_only: bool,
    /// マッピング形式の要求 (Location を書き換える)
    mapped: bool,
    /// ホスト別統計のキー (`scheme://host:port`)
    pool_key: &'a str,
}

impl Ctx<'_> {
    fn log(&self, status: &str, bytes: u64, cache: &str) {
        self.metrics.record_host(
            self.pool_key,
            HostOutcome::from_access(cache, status.parse().unwrap_or(0)),
            bytes,
        );
        access(
            self.conn_id,
            &Access {
                client: self.client_ip,
                method: self.method,
                target: self.url,
                version: self.version,
                status,
                bytes,
                duration_ms: self.started.elapsed().as_secs_f64() * 1000.0,
                cache,
            },
        );
    }

    fn connection_line(&self) -> String {
        if self.keep_client {
            "Connection: keep-alive".to_string()
        } else {
            "Connection: close".to_string()
        }
    }
}

/// オリジンとの接続 (バッファ付き)。
type OriginConn = BufReader<OriginStream>;

/// 1 リクエストを処理する。戻り値はクライアント接続を次の要求に使えるか。
pub fn handle_http_with_headers(
    client: &mut TcpStream,
    peer_addr: Option<SocketAddr>,
    request_line: &str,
    raw_headers: &[String],
    reader: &mut BufReader<TcpStream>,
    shared: &Shared,
) -> io::Result<bool> {
    let started = Instant::now();
    let conn_id = shared.conn_id;
    let cache: &Cache = &shared.cache;
    let metrics: &Metrics = &shared.metrics;
    let req = parse_request_headers(raw_headers, conn_id);

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        log_warn!(
            Some(conn_id),
            "malformed request line: {:?}",
            request_line.trim()
        );
        return Ok(false);
    }
    let method = parts[0];
    let target = parts[1];
    let version = if parts.len() > 2 {
        parts[2]
    } else {
        "HTTP/1.0"
    };
    let http11 = version.eq_ignore_ascii_case("HTTP/1.1");
    let keep_client = !shared.keepalive.is_zero()
        && if http11 {
            !req.connection_close
        } else {
            req.connection_keep_alive
        };
    let req_framing = Framing::of_request(&req.pairs);
    let is_get = method.eq_ignore_ascii_case("GET");
    let head_only = method.eq_ignore_ascii_case("HEAD");

    let origin = match parse_origin(target, req.host.as_deref()) {
        Ok(o) => o,
        Err(e) => {
            log_warn!(Some(conn_id), "400 Bad Request: {}", e);
            let _ = client.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return Ok(false);
        }
    };
    let server_addr = origin.server_addr();
    let url = origin.url();
    let pool_key = origin.pool_key();
    let client_ip = peer_addr
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "-".to_string());
    log_debug!(Some(conn_id), "start {} {} {}", method, url, version);
    let mut ctx = Ctx {
        client_ip: &client_ip,
        method,
        url: &url,
        version,
        started,
        conn_id,
        metrics,
        req: &req,
        keep_client,
        head_only,
        mapped: origin.mapped,
        pool_key: &pool_key,
    };

    // ---- キャッシュ参照 ----
    let cfg = cache.config();
    let variant = freshness::accept_encoding_variant(req.accept_encoding.as_deref());
    let key = cache_key_variant("GET", &url, &variant);
    let client_no_store = freshness::has_directive(&req.cache_control, "no-store");
    let force_revalidate = freshness::has_directive(&req.cache_control, "no-cache")
        || freshness::directive_value(&req.cache_control, "max-age") == Some(0);
    // 本文付きの GET は本文でも意味が変わりうるのでキャッシュしない
    let lookup_allowed = cache.enabled()
        && (is_get || head_only)
        && !req.authorization
        && !client_no_store
        && req_framing == Framing::None;
    let store_allowed = lookup_allowed && is_get && req.range.is_none();
    let client_conditional = req.if_none_match.is_some() || req.if_modified_since.is_some();
    let now = now_epoch();

    let mut stale: Option<(CachedResponse, CacheSource)> = None;
    if lookup_allowed {
        if let Some((entry, source)) = cache.get(key, conn_id) {
            cache.remember_variant(&url, key);
            if entry.is_fresh(now) && !force_revalidate {
                body::drain(reader, req_framing)?;
                let ttl_left = entry.ttl_left(now);
                return serve_cached(client, entry, source, "HIT", ttl_left, &ctx);
            }
            // クライアント自身の条件付き要求は、そのままオリジンに判断させる
            if !client_conditional {
                // 期限切れ直後 (grace 内) なら、すぐ返して裏で再検証する (stale-while-revalidate)。
                // 既定の grace は新鮮だった期間のある表現だけに使い、max-age=0 (毎回再検証) の表現は
                // オリジンが stale-while-revalidate を明示したときだけ対象にする
                let head = freshness::parse_cached_head(&entry.head);
                let default_grace = if entry.meta.expires_at > entry.meta.stored_at {
                    cfg.grace.as_secs()
                } else {
                    0
                };
                let grace = default_grace.max(
                    head.cache_control
                        .as_deref()
                        .and_then(|cc| freshness::directive_value(cc, "stale-while-revalidate"))
                        .unwrap_or(0),
                );
                let stale_for = now.saturating_sub(entry.meta.expires_at);
                if !force_revalidate
                    && grace > 0
                    && entry.meta.validators
                    && head.may_serve_stale()
                    && stale_for <= grace
                    && refresh::spawn(
                        shared,
                        &origin,
                        key,
                        &url,
                        entry.head.clone(),
                        req.accept_encoding.clone(),
                    )
                {
                    body::drain(reader, req_framing)?;
                    cache.stale_served.fetch_add(1, Ordering::Relaxed);
                    return serve_cached(client, entry, source, "REFRESHING", 0, &ctx);
                }
                stale = Some((entry, source));
            }
        }
    } else if cache.enabled() {
        log_debug!(
            Some(conn_id),
            "cache BYPASS (method={} auth={} cc='{}')",
            method,
            req.authorization,
            req.cache_control
        );
    }

    // ---- 同時ミスの合流: 同じキーを誰かが取得中なら、その保存完了を待ってキャッシュから返す ----
    let mut leader = None;
    if store_allowed && stale.is_none() {
        match cache.begin_fetch(key) {
            FetchTicket::Leader(guard) => leader = Some(guard),
            FetchTicket::Follower(inflight) => {
                log_debug!(
                    Some(conn_id),
                    "cache WAIT: another request is fetching key={}",
                    key
                );
                if inflight.wait(shared.timeout) == Some(FetchOutcome::Stored)
                    && let Some((entry, source)) = cache.get(key, conn_id)
                    && entry.is_fresh(now_epoch())
                {
                    body::drain(reader, req_framing)?;
                    cache.coalesced.fetch_add(1, Ordering::Relaxed);
                    let ttl_left = entry.ttl_left(now_epoch());
                    return serve_cached(client, entry, source, "COALESCED", ttl_left, &ctx);
                }
                // 保存されなかった・間に合わなかった: 自分で取りに行く
            }
        }
    }

    // ---- オリジンへ転送 ----
    let conditional_lines = stale
        .as_ref()
        .map(|(entry, _)| {
            freshness::conditional_headers(&freshness::parse_cached_head(&entry.head))
        })
        .unwrap_or_default();
    let forwarded: Vec<String> = headers::sanitize_and_inject_headers(raw_headers, peer_addr)
        .into_iter()
        .filter(|h| !h.trim_start().to_ascii_lowercase().starts_with("host:"))
        .collect();
    for h in &forwarded {
        log_trace!(Some(conn_id), "fwd header  {}", h.trim_end());
    }
    for line in &conditional_lines {
        log_trace!(Some(conn_id), "fwd header  {} (revalidation)", line);
    }
    let request_head = request_head(method, &origin, &forwarded, &conditional_lines);
    // 期限切れの表現が手元にあるなら、オリジンを長く待たずに stale を返す
    let origin_timeout = if stale.is_some() {
        shared.timeout.min(cfg.stale_wait)
    } else {
        shared.timeout
    };

    // 本文の無い冪等な要求だけ、再利用した接続が死んでいたときに 1 回やり直す
    let retryable = req_framing == Framing::None && (is_get || head_only);
    let mut attempt = 0;
    let (mut server, head, status, resp_headers, request_body_bytes) = loop {
        attempt += 1;
        let (mut server, reused) = match acquire_origin(
            &shared.upstream,
            origin_timeout,
            conn_id,
            &origin,
            &pool_key,
        ) {
            Ok(v) => v,
            Err(e) => {
                log_warn!(
                    Some(conn_id),
                    "502 Bad Gateway: connect {} failed: {}",
                    server_addr,
                    e
                );
                if let Some((entry, source)) = stale.take()
                    && !force_revalidate
                    && can_serve_stale(&entry)
                {
                    body::drain(reader, req_framing)?;
                    cache.stale_served.fetch_add(1, Ordering::Relaxed);
                    return serve_cached(client, entry, source, "STALE", 0, &ctx);
                }
                write_error(client, 502, "Bad Gateway")?;
                return Ok(false);
            }
        };
        metrics.inc_origin_conn(reused);
        let sent = server
            .get_mut()
            .write_all(&request_head)
            .and_then(|_| forward_request_body(reader, server.get_mut(), req_framing))
            .and_then(|n| server.get_mut().flush().map(|_| n));
        let result = sent.and_then(|n| read_response_head(&mut server).map(|h| (h, n)));
        match result {
            Ok(((head, status, resp_headers), n)) => {
                if origin_timeout != shared.timeout {
                    let _ = server.get_ref().set_timeouts(shared.timeout);
                }
                break (server, head, status, resp_headers, n);
            }
            Err(e) if reused && retryable && attempt == 1 && is_eof_like(&e) => {
                log_debug!(
                    Some(conn_id),
                    "pooled connection to {} was stale ({}), retrying",
                    pool_key,
                    e
                );
                continue;
            }
            Err(e) => {
                log_warn!(Some(conn_id), "failed to read origin response: {}", e);
                if let Some((entry, source)) = stale.take()
                    && !force_revalidate
                    && can_serve_stale(&entry)
                {
                    cache.stale_served.fetch_add(1, Ordering::Relaxed);
                    return serve_cached(client, entry, source, "STALE", 0, &ctx);
                }
                write_error(client, 502, "Bad Gateway")?;
                return Ok(false);
            }
        }
    };
    if request_body_bytes > 0 {
        log_debug!(
            Some(conn_id),
            "forwarded request body {}B",
            request_body_bytes
        );
    }
    for (k, v) in &resp_headers {
        log_trace!(Some(conn_id), "res header  {}: {}", k, v);
    }

    let framing = Framing::of_response(status, head_only, &resp_headers);
    let origin_reusable = head.starts_with(b"HTTP/1.1")
        && framing != Framing::Close
        && !resp_headers.iter().any(|(k, v)| {
            k == "connection" && v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close"))
        });

    // 304: 保存済みの表現がまだ有効。延命して配信する
    if status == 304
        && let Some((entry, source)) = stale.take()
    {
        let cached_head = freshness::parse_cached_head(&entry.head);
        let p = freshness::revalidated_policy(&resp_headers, &cached_head, cfg, now);
        cache.refresh(key, p.ttl, p.age, conn_id);
        if origin_reusable {
            shared.upstream.pool.put(&pool_key, server);
        }
        let ttl_left = p.ttl.as_secs().saturating_sub(p.age);
        return serve_cached(client, entry, source, "REVALIDATED", ttl_left, &ctx);
    }
    // オリジン障害: stale を配信 (禁止されていなければ)。この接続は再利用しない
    if STALE_ON_STATUS.contains(&status)
        && !force_revalidate
        && let Some((entry, source)) = stale.take()
        && can_serve_stale(&entry)
    {
        log_debug!(
            Some(conn_id),
            "origin returned {}: serving stale entry",
            status
        );
        cache.stale_served.fetch_add(1, Ordering::Relaxed);
        return serve_cached(client, entry, source, "STALE", 0, &ctx);
    }
    if lookup_allowed {
        metrics.inc_cache_miss();
    }

    // ---- 配信しつつ保存 ----
    let policy = if store_allowed {
        freshness::response_policy(status, &resp_headers, cfg, now)
    } else {
        None
    };
    if policy.is_none() && stale.is_some() && (200..400).contains(&status) {
        // 新しい表現が届いたのに保存できないので、古い表現も捨てる (4xx/5xx では残す)
        cache.remove(key);
    }
    // unsafe メソッドへの成功応答は対象 URL (と Location 先) のキャッシュを無効化する (RFC 9111 §4.4)
    if !is_get && !head_only && (200..400).contains(&status) && cache.enabled() {
        cache.invalidate(&url, conn_id);
        for name in ["location", "content-location"] {
            if let Some((_, target)) = resp_headers.iter().find(|(k, _)| k == name)
                && let Ok(o) = parse_origin(target, None)
                && o.scheme == origin.scheme
                && o.server_addr() == server_addr
            {
                cache.invalidate(&o.url(), conn_id);
            }
        }
    }
    let mut sanitized = headers::sanitize_response_head(&head);
    // 保存するのは元の Location のまま (配信時に必要なら書き換える)
    let cached_head = sanitized.assemble(&[]);
    if origin.mapped {
        map_locations(&mut sanitized.lines);
    }
    // クライアント向けの枠組み: 長さが分かればそのまま、分からなければ HTTP/1.1 には再 chunk
    let client_framing = match framing {
        Framing::None | Framing::Length(_) => framing,
        Framing::Chunked | Framing::Close => {
            if http11 {
                Framing::Chunked
            } else {
                Framing::Close
            }
        }
    };
    if client_framing == Framing::Close {
        ctx.keep_client = false;
    }
    let mut extra = Vec::new();
    match client_framing {
        Framing::Length(n) => extra.push(format!("Content-Length: {}", n)),
        Framing::Chunked => extra.push("Transfer-Encoding: chunked".to_string()),
        Framing::None if head_only => {
            // HEAD: GET と同じヘッダーを返す (本文は無い)
            if let Some((_, v)) = resp_headers.iter().find(|(k, _)| k == "content-length") {
                extra.push(format!("Content-Length: {}", v));
            }
        }
        _ => {}
    }
    extra.push(ctx.connection_line());
    let client_head = sanitized.assemble(&extra);
    let expected = match framing {
        Framing::Length(n) => Some(n.saturating_add(cached_head.len() as u64)),
        _ => None,
    };
    let mut sink =
        policy.map(|p| cache.begin_store(key, &url, p.ttl, p.age, p.validators, expected, conn_id));
    if let Some(s) = sink.as_mut() {
        s.write(&cached_head);
    }

    client.write_all(&client_head)?;
    let mut body_bytes = 0u64;
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    let mut clean = true;
    {
        let mut body = BodyReader::new(&mut server, framing);
        loop {
            let n = match body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(ref e) if is_eof_like(e) => {
                    log_debug!(Some(conn_id), "origin body ended early: {}", e);
                    clean = false;
                    break;
                }
                Err(e) => {
                    if let Some(s) = sink.take() {
                        s.abort();
                    }
                    return Err(e);
                }
            };
            if client_framing == Framing::Chunked {
                body::write_chunk(client, &buf[..n])?;
            } else {
                client.write_all(&buf[..n])?;
            }
            body_bytes += n as u64;
            if let Some(s) = sink.as_mut() {
                s.write(&buf[..n]);
            }
        }
        clean = clean && body.finished_cleanly();
    }
    if clean && client_framing == Framing::Chunked {
        body::write_last_chunk(client)?;
    }
    client.flush()?;

    let cache_state = if clean {
        if origin_reusable {
            shared.upstream.pool.put(&pool_key, server);
        }
        match (policy, sink) {
            (Some(p), Some(s)) => {
                let out = s.finish();
                if out.memory || out.disk {
                    if let Some(guard) = leader.take() {
                        guard.complete(FetchOutcome::Stored);
                    }
                    format!("MISS stored ttl={}s", p.ttl.as_secs())
                } else {
                    "MISS".to_string()
                }
            }
            _ if !lookup_allowed => "BYPASS".to_string(),
            _ => "MISS".to_string(),
        }
    } else {
        // 途中で切れた本文はクライアントにもそれと分かる形 (終端チャンク無し / 短い本文) で伝わる
        if let Some(s) = sink {
            s.abort();
        }
        ctx.keep_client = false;
        "MISS truncated".to_string()
    };

    // 保存されなかった場合は待っている要求に自分で取りに行かせる (Drop でも通知される)
    if let Some(guard) = leader.take() {
        guard.complete(FetchOutcome::NotStored);
    }
    let total = client_head.len() as u64 + body_bytes;
    metrics.add_bytes(total + request_body_bytes);
    ctx.log(&status.to_string(), total, &cache_state);
    Ok(ctx.keep_client)
}

/// オリジンへの要求の先頭 (要求行 + Host + 転送するヘッダー + 条件付きヘッダー + 空行)。
/// Host はオリジンのものに差し替える (マッピング形式ではプロキシ宛ての Host が来る)。
pub(super) fn request_head(
    method: &str,
    origin: &Origin,
    forwarded: &[String],
    conditional: &[String],
) -> Vec<u8> {
    let mut head = format!("{} {} HTTP/1.1\r\n", method, origin.path).into_bytes();
    head.extend_from_slice(format!("Host: {}\r\n", origin.host_port).as_bytes());
    for h in forwarded {
        head.extend_from_slice(h.as_bytes());
    }
    for line in conditional {
        head.extend_from_slice(line.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    head
}

/// プールにあれば再利用し、無ければ接続する (HTTPS なら TLS まで)。戻り値の bool は再利用したか。
/// `timeout` は接続と最初の応答を待つ時間 (stale があるときは短くする)。
pub(super) fn acquire_origin(
    upstream: &Upstream,
    timeout: Duration,
    conn_id: usize,
    origin: &Origin,
    pool_key: &str,
) -> io::Result<(OriginConn, bool)> {
    if let Some(server) = upstream.pool.get(pool_key) {
        server.get_ref().set_timeouts(timeout)?;
        log_debug!(Some(conn_id), "reusing pooled connection to {}", pool_key);
        return Ok((server, true));
    }
    log_debug!(Some(conn_id), "connecting to origin {}", pool_key);
    let stream = origin::connect(
        origin.scheme,
        &origin.server_addr(),
        &origin.host(),
        timeout,
        upstream.tls.as_ref(),
    )?;
    Ok((BufReader::with_capacity(COPY_BUF_SIZE, stream), false))
}

/// クライアントのリクエスト本文をオリジンへ同じ枠組みで転送する。戻り値は本文のバイト数。
fn forward_request_body(
    reader: &mut BufReader<TcpStream>,
    server: &mut OriginStream,
    framing: Framing,
) -> io::Result<u64> {
    match framing {
        Framing::None | Framing::Close => Ok(0),
        Framing::Length(_) => {
            let mut body = BodyReader::new(reader, framing);
            io::copy(&mut body, server)
        }
        Framing::Chunked => {
            let mut body = BodyReader::new(reader, framing);
            let mut buf = vec![0u8; COPY_BUF_SIZE];
            let mut total = 0u64;
            loop {
                let n = body.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                body::write_chunk(server, &buf[..n])?;
                total += n as u64;
            }
            body::write_last_chunk(server)?;
            Ok(total)
        }
    }
}

fn write_error(client: &mut TcpStream, status: u16, reason: &str) -> io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        status, reason
    );
    client.write_all(resp.as_bytes())?;
    client.flush()
}

fn is_eof_like(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
    )
}

/// ステータス行とヘッダー部を読み切り、[`ResponseHead`] を返す。
pub fn read_response_head<R: BufRead>(reader: &mut R) -> io::Result<ResponseHead> {
    let mut head = Vec::with_capacity(1024);
    let mut status_line = String::new();
    if reader.read_line(&mut status_line)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "origin closed before sending a status line",
        ));
    }
    head.extend_from_slice(status_line.as_bytes());

    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        head.extend_from_slice(line.as_bytes());
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    Ok((head, status, headers))
}
