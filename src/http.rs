//! HTTP (平文) リクエストの転送とキャッシュの適用。
//!
//! 流れ: リクエスト解析 → キャッシュ参照 (新鮮なら配信、期限切れでもバリデータ付きなら
//! 再検証用に保持) → オリジンへ転送 (プールの接続を再利用、必要なら条件付き) → 応答本文を
//! 解読しつつクライアントへ自前の枠組みで配信し、同時にストリーミングで保存。
//! 304 なら保存済みの表現を延命して配信し、オリジン障害時は stale を配信する。
//! `Range` / `HEAD` はキャッシュ済みの完全な表現から切り出して応答する。

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use crate::body::{self, BodyReader, Framing, RangeSpec};
use crate::cache::{Cache, CacheSource, CachedResponse, cache_key_variant, now_epoch};
use crate::freshness::{self, CachedHead};
use crate::headers;
use crate::log::{Access, access};
use crate::metrics::Metrics;
use crate::net;
use crate::origin::{self, OriginStream, Scheme};
use crate::pool::Pool;
use crate::tls::TlsClient;
use crate::{log_debug, log_trace, log_warn};

const COPY_BUF_SIZE: usize = 64 * 1024;
/// オリジンがこのステータスを返したら stale を配信する (RFC 5861 stale-if-error 相当)。
const STALE_ON_STATUS: &[u16] = &[500, 502, 503, 504];

/// (生バイト列, ステータスコード, 小文字化したヘッダー名と値の組)
pub type ResponseHead = (Vec<u8>, u16, Vec<(String, String)>);

/// 接続をまたいで共有する状態。
pub struct Shared<'a> {
    pub timeout: Duration,
    pub keepalive: Duration,
    pub conn_id: usize,
    pub metrics: &'a Metrics,
    pub cache: &'a Cache,
    pub pool: &'a Pool,
    /// HTTPS のオリジン用 (libssl が無ければ `None`)
    pub tls: Option<&'a TlsClient>,
}

/// 要求先 (オリジン)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub scheme: Scheme,
    /// ホスト (必要ならポート付き)、IPv6 リテラルは括弧付き
    pub host_port: String,
    pub path: String,
    /// `/https/host/path` 形式 (プロキシをオリジンとして叩く形) で頼まれたか。
    /// この場合、応答の Location も同じ形式に書き換えてクライアントをプロキシに留める
    pub mapped: bool,
}

impl Origin {
    pub fn server_addr(&self) -> String {
        net::with_default_port(&self.host_port, self.scheme.default_port())
    }

    /// キャッシュキーとログに使う正規化 URL。
    pub fn url(&self) -> String {
        format!("{}://{}{}", self.scheme, self.server_addr(), self.path)
    }

    /// 接続プールのキー。
    pub fn pool_key(&self) -> String {
        format!("{}://{}", self.scheme, self.server_addr())
    }

    pub fn host(&self) -> String {
        net::split_host_port(&self.host_port).0
    }
}

/// 要求行の target と Host ヘッダーからオリジンを決める。
/// 受け付ける形: `http://h/p`、`https://h/p` (絶対形式)、`/https/h/p`、`/http/h/p` (マッピング)、
/// `/p` (Host ヘッダー宛て)、`h` (ホストのみ)。
pub fn parse_origin(target: &str, host_header: Option<&str>) -> io::Result<Origin> {
    let split = |scheme: Scheme, rest: &str, mapped: bool| -> io::Result<Origin> {
        let (host_port, path) = match rest.find('/') {
            Some(pos) => (&rest[..pos], &rest[pos..]),
            None => (rest, "/"),
        };
        if host_port.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Missing host in request target",
            ));
        }
        Ok(Origin {
            scheme,
            host_port: host_port.to_string(),
            path: path.to_string(),
            mapped,
        })
    };
    if let Some(rest) = target.strip_prefix("http://") {
        return split(Scheme::Http, rest, false);
    }
    if let Some(rest) = target.strip_prefix("https://") {
        return split(Scheme::Https, rest, false);
    }
    if let Some(rest) = target.strip_prefix("/https/") {
        return split(Scheme::Https, rest, true);
    }
    if let Some(rest) = target.strip_prefix("/http/") {
        return split(Scheme::Http, rest, true);
    }
    if target.starts_with('/') {
        return match host_header {
            Some(h) if !h.is_empty() => Ok(Origin {
                scheme: Scheme::Http,
                host_port: h.to_string(),
                path: target.to_string(),
                mapped: false,
            }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Missing host in HTTP request",
            )),
        };
    }
    Ok(Origin {
        scheme: Scheme::Http,
        host_port: target.to_string(),
        path: "/".to_string(),
        mapped: false,
    })
}

/// マッピング形式のクライアント向けに、絶対 URL の Location / Content-Location を `/https/h/p` 形式へ。
pub fn map_locations(lines: &mut [String]) {
    for line in lines.iter_mut() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let lower = name.trim().to_ascii_lowercase();
        if lower != "location" && lower != "content-location" {
            continue;
        }
        let v = value.trim();
        let mapped = if let Some(rest) = v.strip_prefix("https://") {
            Some(format!("/https/{}", rest))
        } else {
            v.strip_prefix("http://")
                .map(|rest| format!("/http/{}", rest))
        };
        if let Some(m) = mapped {
            *line = format!("{}: {}", name.trim(), m);
        }
    }
}

/// リクエストヘッダーのうち転送・キャッシュ判断に使うもの。
#[derive(Default)]
struct RequestHeaders {
    host: Option<String>,
    /// (小文字の名前, 値)
    pairs: Vec<(String, String)>,
    authorization: bool,
    cache_control: String,
    if_none_match: Option<String>,
    if_modified_since: Option<String>,
    if_range: Option<String>,
    range: Option<String>,
    accept_encoding: Option<String>,
    connection_close: bool,
    connection_keep_alive: bool,
}

fn parse_request_headers(raw_headers: &[String], conn_id: usize) -> RequestHeaders {
    let mut h = RequestHeaders::default();
    for line in raw_headers {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k_lower = k.trim().to_ascii_lowercase();
        let v_trim = v.trim();
        log_trace!(Some(conn_id), "req header  {}: {}", k.trim(), v_trim);
        match k_lower.as_str() {
            "host" => h.host = Some(v_trim.to_string()),
            "authorization" => h.authorization = true,
            "cache-control" | "pragma" => {
                if !h.cache_control.is_empty() {
                    h.cache_control.push(',');
                }
                h.cache_control.push_str(&v_trim.to_ascii_lowercase());
            }
            "if-none-match" => h.if_none_match = Some(v_trim.to_string()),
            "if-modified-since" => h.if_modified_since = Some(v_trim.to_string()),
            "if-range" => h.if_range = Some(v_trim.to_string()),
            "range" => h.range = Some(v_trim.to_string()),
            "accept-encoding" => h.accept_encoding = Some(v_trim.to_string()),
            "connection" | "proxy-connection" => {
                for token in v_trim.split(',') {
                    let t = token.trim();
                    if t.eq_ignore_ascii_case("close") {
                        h.connection_close = true;
                    } else if t.eq_ignore_ascii_case("keep-alive") {
                        h.connection_keep_alive = true;
                    }
                }
            }
            _ => {}
        }
        h.pairs.push((k_lower, v_trim.to_string()));
    }
    h
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
}

impl Ctx<'_> {
    fn log(&self, status: &str, bytes: u64, cache: &str) {
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
type Upstream = BufReader<OriginStream>;

/// 1 リクエストを処理する。戻り値はクライアント接続を次の要求に使えるか。
pub fn handle_http_with_headers(
    client: &mut TcpStream,
    peer_addr: Option<SocketAddr>,
    request_line: &str,
    raw_headers: &[String],
    reader: &mut BufReader<TcpStream>,
    shared: &Shared<'_>,
) -> io::Result<bool> {
    let started = Instant::now();
    let conn_id = shared.conn_id;
    let cache = shared.cache;
    let metrics = shared.metrics;
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

    // ---- オリジンへ転送 ----
    let conditional_lines = stale
        .as_ref()
        .map(|(entry, _)| {
            freshness::conditional_headers(&freshness::parse_cached_head(&entry.head))
        })
        .unwrap_or_default();
    let mut request_head = format!("{} {} {}\r\n", method, origin.path, "HTTP/1.1").into_bytes();
    // Host はオリジンのものに差し替える (マッピング形式ではプロキシ宛ての Host が来る)
    request_head.extend_from_slice(format!("Host: {}\r\n", origin.host_port).as_bytes());
    for h in headers::sanitize_and_inject_headers(raw_headers, peer_addr) {
        if h.trim_start().to_ascii_lowercase().starts_with("host:") {
            continue;
        }
        log_trace!(Some(conn_id), "fwd header  {}", h.trim_end());
        request_head.extend_from_slice(h.as_bytes());
    }
    for line in &conditional_lines {
        log_trace!(Some(conn_id), "fwd header  {} (revalidation)", line);
        request_head.extend_from_slice(line.as_bytes());
        request_head.extend_from_slice(b"\r\n");
    }
    request_head.extend_from_slice(b"\r\n");

    // 本文の無い冪等な要求だけ、再利用した接続が死んでいたときに 1 回やり直す
    let retryable = req_framing == Framing::None && (is_get || head_only);
    let mut attempt = 0;
    let (mut server, head, status, resp_headers, request_body_bytes) = loop {
        attempt += 1;
        let (mut server, reused) = match acquire_origin(shared, &origin, &pool_key) {
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
            shared.pool.put(&pool_key, server);
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
            shared.pool.put(&pool_key, server);
        }
        match (policy, sink) {
            (Some(p), Some(s)) => {
                let out = s.finish();
                if out.memory || out.disk {
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

    let total = client_head.len() as u64 + body_bytes;
    metrics.add_bytes(total + request_body_bytes);
    ctx.log(&status.to_string(), total, &cache_state);
    Ok(ctx.keep_client)
}

/// プールにあれば再利用し、無ければ接続する (HTTPS なら TLS まで)。戻り値の bool は再利用したか。
fn acquire_origin(
    shared: &Shared<'_>,
    origin: &Origin,
    pool_key: &str,
) -> io::Result<(Upstream, bool)> {
    if let Some(server) = shared.pool.get(pool_key) {
        server.get_ref().set_timeouts(shared.timeout)?;
        log_debug!(
            Some(shared.conn_id),
            "reusing pooled connection to {}",
            pool_key
        );
        return Ok((server, true));
    }
    log_debug!(Some(shared.conn_id), "connecting to origin {}", pool_key);
    let stream = origin::connect(
        origin.scheme,
        &origin.server_addr(),
        &origin.host(),
        shared.timeout,
        shared.tls,
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

/// stale のまま配信してよいか (`must-revalidate` / `proxy-revalidate` なら不可)。
fn can_serve_stale(entry: &CachedResponse) -> bool {
    freshness::parse_cached_head(&entry.head).may_serve_stale()
}

/// `If-Range` が保存済みの表現に一致するか (無ければ一致扱い)。弱い ETag は使えない。
fn if_range_matches(if_range: Option<&str>, head: &CachedHead) -> bool {
    let Some(cond) = if_range else {
        return true;
    };
    let cond = cond.trim();
    if cond.starts_with('"') {
        head.etag.as_deref().is_some_and(|e| e == cond)
    } else if cond.starts_with("W/") {
        false
    } else {
        match (&head.last_modified, crate::httpdate::parse(cond)) {
            (Some(lm), Some(t)) => crate::httpdate::parse(lm) == Some(t),
            _ => false,
        }
    }
}

/// キャッシュ済みレスポンスを配信する。クライアントの条件付き要求には 304、`Range` には 206、
/// `HEAD` にはヘッダーだけを返す。戻り値はクライアント接続を維持できるか。
fn serve_cached(
    client: &mut TcpStream,
    entry: CachedResponse,
    source: CacheSource,
    label: &str,
    ttl_left: u64,
    ctx: &Ctx<'_>,
) -> io::Result<bool> {
    let age = entry.age();
    let cached_head = freshness::parse_cached_head(&entry.head);
    let conditional = ctx.req.if_none_match.is_some() || ctx.req.if_modified_since.is_some();
    if conditional
        && freshness::client_not_modified(
            &cached_head,
            ctx.req.if_none_match.as_deref(),
            ctx.req.if_modified_since.as_deref(),
        )
    {
        let written =
            write_not_modified(client, &cached_head, label, source, age, ctx.keep_client)?;
        ctx.metrics.inc_cache_hit();
        ctx.metrics.add_bytes(written);
        ctx.log(
            "304",
            written,
            &format!("{}({},304) age={}s", label, source.as_str(), age),
        );
        return Ok(ctx.keep_client);
    }

    let body_len = entry.body_len();
    let range = match (&ctx.req.range, cached_head.status == 200 && !ctx.head_only) {
        (Some(r), true) if if_range_matches(ctx.req.if_range.as_deref(), &cached_head) => {
            body::parse_range(r, body_len)
        }
        _ => RangeSpec::Ignore,
    };
    let served = Serve {
        label,
        source,
        age,
        keep_alive: ctx.keep_client,
        head_only: ctx.head_only,
        range,
        map_locations: ctx.mapped,
    };
    let (status, written) = write_cached_response(client, entry, &served)?;
    ctx.metrics.inc_cache_hit();
    ctx.metrics.add_bytes(written);
    let detail = match range {
        RangeSpec::Bytes { start, end } => format!(" range={}-{}", start, end),
        RangeSpec::Unsatisfiable => " range=unsatisfiable".to_string(),
        RangeSpec::Ignore => String::new(),
    };
    ctx.log(
        &status.to_string(),
        written,
        &format!(
            "{}({}) age={}s ttl_left={}s{}",
            label,
            source.as_str(),
            age,
            ttl_left,
            detail
        ),
    );
    Ok(ctx.keep_client)
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

fn x_cache_lines(label: &str, source: CacheSource, age: u64) -> [String; 2] {
    [
        format!(
            "X-Cache: {} from sorahost-http-proxy ({})",
            label,
            source.as_str()
        ),
        format!("Age: {}", age),
    ]
}

/// キャッシュ済みレスポンスの配信方法。
pub struct Serve<'a> {
    pub label: &'a str,
    pub source: CacheSource,
    pub age: u64,
    pub keep_alive: bool,
    pub head_only: bool,
    pub range: RangeSpec,
    /// マッピング形式のクライアント向けに Location を書き換える
    pub map_locations: bool,
}

/// キャッシュ済みレスポンスに枠組み (Content-Length) と `X-Cache` / `Age` を付けて書き出す。
/// 戻り値は (ステータス, 書いたバイト数)。
pub fn write_cached_response(
    client: &mut impl Write,
    entry: CachedResponse,
    serve: &Serve<'_>,
) -> io::Result<(u16, u64)> {
    let mut head = headers::sanitize_response_head(&entry.head);
    if serve.map_locations {
        map_locations(&mut head.lines);
    }
    let body_len = entry.body_len();
    let mut extra: Vec<String> = x_cache_lines(serve.label, serve.source, serve.age).to_vec();
    let status;
    let (start, len) = match serve.range {
        RangeSpec::Bytes { start, end } => {
            status = 206;
            head.status_line = "HTTP/1.1 206 Partial Content".to_string();
            extra.push(format!(
                "Content-Range: bytes {}-{}/{}",
                start, end, body_len
            ));
            (start, end - start + 1)
        }
        RangeSpec::Unsatisfiable => {
            status = 416;
            head.status_line = "HTTP/1.1 416 Range Not Satisfiable".to_string();
            head.lines
                .retain(|l| !l.to_ascii_lowercase().starts_with("content-type:"));
            extra.push(format!("Content-Range: bytes */{}", body_len));
            (0, 0)
        }
        RangeSpec::Ignore => {
            status = head
                .status_line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(200);
            (0, body_len)
        }
    };
    extra.push(format!("Content-Length: {}", len));
    extra.push(if serve.keep_alive {
        "Connection: keep-alive".to_string()
    } else {
        "Connection: close".to_string()
    });
    let bytes = head.assemble(&extra);
    client.write_all(&bytes)?;
    let mut written = bytes.len() as u64;
    if !serve.head_only && len > 0 {
        written += io::copy(&mut entry.into_body_range(start, len), client)?;
    }
    client.flush()?;
    Ok((status, written))
}

/// クライアントの条件付き要求に対する 304 応答。
fn write_not_modified(
    client: &mut impl Write,
    head: &CachedHead,
    label: &str,
    source: CacheSource,
    age: u64,
    keep_alive: bool,
) -> io::Result<u64> {
    let mut out = String::from("HTTP/1.1 304 Not Modified\r\n");
    for line in x_cache_lines(label, source, age) {
        out.push_str(&line);
        out.push_str("\r\n");
    }
    for line in freshness::not_modified_headers(head) {
        out.push_str(&line);
        out.push_str("\r\n");
    }
    out.push_str(if keep_alive {
        "Connection: keep-alive\r\n\r\n"
    } else {
        "Connection: close\r\n\r\n"
    });
    client.write_all(out.as_bytes())?;
    client.flush()?;
    Ok(out.len() as u64)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Body, Meta};
    use std::sync::Arc;

    #[test]
    fn test_parse_origin_forms() {
        let o = parse_origin("http://example.com/test?a=1", None).unwrap();
        assert_eq!(
            (o.scheme, o.host_port.as_str(), o.path.as_str(), o.mapped),
            (Scheme::Http, "example.com", "/test?a=1", false)
        );
        assert_eq!(o.server_addr(), "example.com:80");
        assert_eq!(o.url(), "http://example.com:80/test?a=1");

        let o = parse_origin("https://example.com", None).unwrap();
        assert_eq!((o.scheme, o.path.as_str()), (Scheme::Https, "/"));
        assert_eq!(o.server_addr(), "example.com:443");
        assert_eq!(o.pool_key(), "https://example.com:443");

        let o = parse_origin("/https/[2001:db8::1]:8443/v6", None).unwrap();
        assert_eq!(
            (o.scheme, o.host_port.as_str(), o.path.as_str(), o.mapped),
            (Scheme::Https, "[2001:db8::1]:8443", "/v6", true)
        );
        assert_eq!(o.host(), "2001:db8::1");
        assert_eq!(o.server_addr(), "[2001:db8::1]:8443");

        let o = parse_origin("/http/example.com", None).unwrap();
        assert_eq!(
            (o.scheme, o.path.as_str(), o.mapped),
            (Scheme::Http, "/", true)
        );

        let o = parse_origin("/index.html", Some("example.com")).unwrap();
        assert_eq!(
            (o.scheme, o.host_port.as_str(), o.path.as_str()),
            (Scheme::Http, "example.com", "/index.html")
        );
        assert!(parse_origin("/index.html", None).is_err());
        assert!(parse_origin("/https/", None).is_err());
        assert!(parse_origin("/https//path", None).is_err());
    }

    #[test]
    fn test_map_locations() {
        let mut lines = vec![
            "Location: https://example.com/next".to_string(),
            "Content-Location: http://example.com:8080/x".to_string(),
            "X-Other: https://keep.me/".to_string(),
            "Location: /relative".to_string(),
        ];
        map_locations(&mut lines);
        assert_eq!(lines[0], "Location: /https/example.com/next");
        assert_eq!(lines[1], "Content-Location: /http/example.com:8080/x");
        assert_eq!(lines[2], "X-Other: https://keep.me/");
        assert_eq!(lines[3], "Location: /relative");
    }

    #[test]
    fn test_read_response_head() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nCache-Control: max-age=60\r\n\r\nhello";
        let mut reader = BufReader::new(&raw[..]);
        let (head, status, headers) = read_response_head(&mut reader).unwrap();
        assert_eq!(status, 200);
        assert!(head.ends_with(b"\r\n\r\n"));
        assert_eq!(headers.len(), 2);
        assert_eq!(
            headers[1],
            ("cache-control".to_string(), "max-age=60".to_string())
        );

        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"hello");
    }

    fn cached(wire: &[u8]) -> CachedResponse {
        let offset = wire.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        CachedResponse {
            head: wire[..offset].to_vec(),
            size: wire.len() as u64,
            body: Body::Memory {
                data: Arc::new(wire.to_vec()),
                offset,
            },
            meta: Meta {
                stored_at: 0,
                expires_at: u64::MAX,
                validators: false,
            },
        }
    }

    fn serve(range: RangeSpec, head_only: bool) -> Serve<'static> {
        Serve {
            label: "HIT",
            source: CacheSource::Disk,
            age: 42,
            keep_alive: true,
            head_only,
            range,
            map_locations: false,
        }
    }

    #[test]
    fn test_write_cached_response_injects_framing_and_headers() {
        let entry =
            cached(b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nhi");
        let mut out = Vec::new();
        let (status, n) =
            write_cached_response(&mut out, entry, &serve(RangeSpec::Ignore, false)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(status, 200);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Cache: HIT from sorahost-http-proxy (disk)\r\nAge: 42\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nhi"), "{}", text);
        assert!(!text.contains("Connection: close"));
        assert_eq!(n, text.len() as u64);
    }

    #[test]
    fn test_write_cached_response_range_and_head() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n0123456789";
        let mut out = Vec::new();
        let (status, _) = write_cached_response(
            &mut out,
            cached(wire),
            &serve(RangeSpec::Bytes { start: 2, end: 5 }, false),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(status, 206);
        assert!(
            text.starts_with("HTTP/1.1 206 Partial Content\r\n"),
            "{}",
            text
        );
        assert!(text.contains("Content-Range: bytes 2-5/10\r\n"));
        assert!(text.contains("Content-Length: 4\r\n"));
        assert!(text.ends_with("\r\n\r\n2345"));

        let mut out = Vec::new();
        let (status, _) = write_cached_response(
            &mut out,
            cached(wire),
            &serve(RangeSpec::Unsatisfiable, false),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(status, 416);
        assert!(text.contains("Content-Range: bytes */10\r\n") && text.ends_with("\r\n\r\n"));

        let mut out = Vec::new();
        let (status, _) =
            write_cached_response(&mut out, cached(wire), &serve(RangeSpec::Ignore, true)).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(status, 200);
        assert!(
            text.contains("Content-Length: 10\r\n") && text.ends_with("\r\n\r\n"),
            "{}",
            text
        );
    }

    #[test]
    fn test_if_range_matching() {
        let head = freshness::parse_cached_head(
            b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\n\r\n",
        );
        assert!(if_range_matches(None, &head));
        assert!(if_range_matches(Some("\"v1\""), &head));
        assert!(!if_range_matches(Some("\"v2\""), &head));
        assert!(!if_range_matches(Some("W/\"v1\""), &head));
        assert!(if_range_matches(
            Some("Sun, 06 Nov 1994 08:49:37 GMT"),
            &head
        ));
        assert!(!if_range_matches(
            Some("Mon, 07 Nov 1994 08:49:37 GMT"),
            &head
        ));
    }

    #[test]
    fn test_write_not_modified_keeps_validators_only() {
        let head = freshness::parse_cached_head(
            b"HTTP/1.1 200 OK\r\nETag: \"x\"\r\nContent-Length: 2\r\nContent-Type: text/plain\r\n\r\n",
        );
        let mut out = Vec::new();
        write_not_modified(&mut out, &head, "HIT", CacheSource::Memory, 3, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 304 Not Modified\r\nX-Cache: HIT from sorahost-http-proxy (memory)\r\nAge: 3\r\n"));
        assert!(text.contains("ETag: \"x\"\r\n"));
        assert!(!text.contains("Content-Length"));
        assert!(text.ends_with("Connection: close\r\n\r\n"));
    }
}
