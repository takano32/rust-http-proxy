//! キャッシュ済みレスポンスの配信: 条件付き要求への 304、Range の 206/416、HEAD、stale の判定。

use std::io::{self, Write};
use std::net::TcpStream;

use super::Ctx;
use super::request::map_locations;
use crate::body::{self, RangeSpec};
use crate::cache::{CacheSource, CachedResponse};
use crate::freshness::{self, CachedHead};
use crate::headers;

/// stale のまま配信してよいか (`must-revalidate` / `proxy-revalidate` なら不可)。
pub(super) fn can_serve_stale(entry: &CachedResponse) -> bool {
    freshness::parse_cached_head(&entry.head).may_serve_stale()
}

/// `If-Range` が保存済みの表現に一致するか (無ければ一致扱い)。弱い ETag は使えない。
pub(super) fn if_range_matches(if_range: Option<&str>, head: &CachedHead) -> bool {
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
pub(super) fn serve_cached(
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

fn x_cache_lines(label: &str, source: CacheSource, age: u64) -> [String; 2] {
    [
        format!(
            "X-Cache: {} from rust-http-proxy ({})",
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
pub(super) fn write_not_modified(
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
