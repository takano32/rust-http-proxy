//! キャッシュ済みレスポンスの配信: 条件付き要求への 304、Range の 206/416、HEAD、stale の判定。

use std::io::{self, Write};

use super::Ctx;
use crate::body::{self, RangeSpec};
use crate::cache::{CacheSource, CachedResponse};
use crate::freshness::{self, CachedHead};
use crate::headers;
use crate::log::{self, Level};
use crate::request::map_locations;

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
    client: &mut impl Write,
    entry: CachedResponse,
    source: CacheSource,
    label: &str,
    ttl_left: u64,
    ctx: &Ctx<'_>,
) -> io::Result<bool> {
    let age = entry.age();
    let conditional = ctx.req.if_none_match.is_some() || ctx.req.if_modified_since.is_some();
    // 条件付きでも Range でもない素の HIT では、保存したヘッダーを読み直す必要がない
    // (毎ヒットで全ヘッダー行を String に作り直していた)
    let cached_head =
        (conditional || ctx.req.range.is_some()).then(|| freshness::parse_cached_head(&entry.head));
    if let Some(cached_head) = cached_head.as_ref()
        && conditional
        && freshness::client_not_modified(
            cached_head,
            ctx.req.if_none_match.as_deref(),
            ctx.req.if_modified_since.as_deref(),
        )
    {
        let written = write_not_modified(client, cached_head, label, source, age, ctx.keep_client)?;
        ctx.metrics.inc_cache_hit();
        ctx.metrics.add_bytes(written);
        // アクセスログが出ないなら状態の文字列を組み立てない (`from_access` は先頭しか見ない)
        if log::enabled(Level::Info) {
            ctx.log(
                304,
                written,
                &format!("{}({},304) age={}s", label, source.as_str(), age),
            );
        } else {
            ctx.log(304, written, label);
        }
        return Ok(ctx.keep_client);
    }

    let body_len = entry.body_len();
    let head_is_200 = cached_head.as_ref().is_some_and(|h| h.status == 200);
    let range = match (&ctx.req.range, head_is_200 && !ctx.head_only) {
        (Some(r), true)
            if cached_head
                .as_ref()
                .is_some_and(|h| if_range_matches(ctx.req.if_range.as_deref(), h)) =>
        {
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
    // アクセスログが出ないなら 1 バイトも組み立てない。forward の経路が既に
    // 同じ扱い (`Cow::Borrowed("BYPASS")`) で、`HostOutcome::from_access` は
    // 先頭の label しか見ないので統計も変わらない
    if log::enabled(Level::Info) {
        let detail = match range {
            RangeSpec::Bytes { start, end } => format!(" range={}-{}", start, end),
            RangeSpec::Unsatisfiable => " range=unsatisfiable".to_string(),
            RangeSpec::Ignore => String::new(),
        };
        ctx.log(
            status,
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
    } else {
        ctx.log(status, written, label);
    }
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

/// 文字列を `buf` の `at` から書き、書き終わりの位置を返す (収まらなければ切る)。
fn put_str(buf: &mut [u8], at: usize, s: &str) -> usize {
    if at >= buf.len() {
        return at;
    }
    let end = (at + s.len()).min(buf.len());
    buf[at..end].copy_from_slice(&s.as_bytes()[..end - at]);
    end
}

/// 10 進数を `buf` の `at` から書き、書き終わりの位置を返す (確保しない)。
fn put_u64(buf: &mut [u8], at: usize, mut v: u64) -> usize {
    if at >= buf.len() {
        return at;
    }
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let n = digits.len() - i;
    let end = (at + n).min(buf.len());
    buf[at..end].copy_from_slice(&digits[i..i + (end - at)]);
    end
}

/// `X-Cache: <label> from rust-http-proxy (<source>)` を確保せずに組み立てる。
fn x_cache_line<'a>(buf: &'a mut [u8; 96], label: &str, source: CacheSource) -> &'a str {
    let mut i = put_str(buf, 0, "X-Cache: ");
    i = put_str(buf, i, label);
    i = put_str(buf, i, " from rust-http-proxy (");
    i = put_str(buf, i, source.as_str());
    i = put_str(buf, i, ")");
    std::str::from_utf8(&buf[..i]).unwrap_or("X-Cache: HIT from rust-http-proxy (memory)")
}

/// `Age: <n>` を確保せずに組み立てる。
fn age_line(buf: &mut [u8; 32], age: u64) -> &str {
    let mut i = put_str(buf, 0, "Age: ");
    i = put_u64(buf, i, age);
    std::str::from_utf8(&buf[..i]).unwrap_or("Age: 0")
}

/// `Content-Range: bytes <start>-<end>/<total>` (範囲なしなら `bytes */<total>`) を確保せずに組み立てる。
fn content_range_line(buf: &mut [u8; 64], range: Option<(u64, u64)>, total: u64) -> &str {
    let mut i = put_str(buf, 0, "Content-Range: bytes ");
    match range {
        Some((start, end)) => {
            i = put_u64(buf, i, start);
            i = put_str(buf, i, "-");
            i = put_u64(buf, i, end);
        }
        None => i = put_str(buf, i, "*"),
    }
    i = put_str(buf, i, "/");
    i = put_u64(buf, i, total);
    std::str::from_utf8(&buf[..i]).unwrap_or("Content-Range: bytes */0")
}

/// 保存済みの先頭のステータス行から状態コードを取る (`sanitize_response_head` を通さない)。
fn status_of(head: &[u8]) -> u16 {
    head.split(|b| *b == b'\n')
        .next()
        .and_then(|l| std::str::from_utf8(l).ok())
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(200)
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
    let body_len = entry.body_len();
    // `ResponseHead` (ヘッダー 1 本ごとの String) を組み立てるのは Location を書き換えるとき
    // だけにする。それ以外は生の先頭をそのまま `write_response_head` に渡せば 1 バイトも
    // 変わらないのに、素の HIT のたびに全ヘッダー行を作り直して捨てていた
    let mut head = serve
        .map_locations
        .then(|| headers::sanitize_response_head(&entry.head));
    if let Some(h) = head.as_mut() {
        map_locations(&mut h.lines);
    }
    // 足すヘッダー行は借用のまま持つ (`Vec<String>` と `format!` を要求ごとに作らない。
    // forward の経路 (`handle_http_with_headers`) が既に同じ形)
    let mut xc_buf = [0u8; 96];
    let mut age_buf = [0u8; 32];
    let mut cr_buf = [0u8; 64];
    let mut cl_buf = [0u8; 40];
    let mut extra: [&str; 5] = [""; 5];
    extra[0] = x_cache_line(&mut xc_buf, serve.label, serve.source);
    extra[1] = age_line(&mut age_buf, serve.age);
    let mut n_extra = 2usize;

    let status;
    // ステータス行を差し替えるのは 206 / 416 のときだけ (`None` なら保存済みのものを使う)
    let status_line: Option<&str>;
    let (start, len) = match serve.range {
        RangeSpec::Bytes { start, end } => {
            status = 206;
            status_line = Some("HTTP/1.1 206 Partial Content");
            extra[n_extra] = content_range_line(&mut cr_buf, Some((start, end)), body_len);
            n_extra += 1;
            (start, end - start + 1)
        }
        RangeSpec::Unsatisfiable => {
            status = 416;
            status_line = Some("HTTP/1.1 416 Range Not Satisfiable");
            if let Some(h) = head.as_mut() {
                h.lines
                    .retain(|l| !l.to_ascii_lowercase().starts_with("content-type:"));
            }
            extra[n_extra] = content_range_line(&mut cr_buf, None, body_len);
            n_extra += 1;
            (0, 0)
        }
        RangeSpec::Ignore => {
            status = status_of(&entry.head);
            status_line = None;
            (0, body_len)
        }
    };
    extra[n_extra] = super::content_length_line(&mut cl_buf, len);
    n_extra += 1;
    extra[n_extra] = if serve.keep_alive {
        "Connection: keep-alive"
    } else {
        "Connection: close"
    };
    n_extra += 1;
    let extra = &extra[..n_extra];

    // Location の書き換えが要らないときは String を 1 本ずつ作らずに直接書く
    let bytes = match head {
        Some(mut h) => {
            if let Some(sl) = status_line {
                h.status_line = sl.to_string();
            }
            h.assemble(extra)
        }
        None => {
            let mut out = Vec::with_capacity(entry.head.len() + 128);
            crate::headers::write_response_head(&mut out, &entry.head, status_line, extra);
            out
        }
    };
    client.write_all(&bytes)?;
    let mut written = bytes.len() as u64;
    if !serve.head_only && len > 0 {
        // メモリ層ならそのまま書く (io::copy はスタックの 8 KiB で写すため、
        // 大きいエントリでは中間バッファへのコピーが 1 回まるごと余計にかかる)
        if let Some((data, from, to)) = entry.memory_range(start, len) {
            client.write_all(&data[from..to])?;
            written += (to - from) as u64;
        } else {
            written += io::copy(&mut entry.into_body_range(start, len), client)?;
        }
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

#[cfg(test)]
mod extra_line_tests {
    use super::*;

    /// スタックで組み立てた行が、以前の `format!` 版と 1 バイトも違わないこと。
    #[test]
    fn stack_built_lines_match_the_previous_implementation() {
        for label in ["HIT", "REVALIDATED", "REFRESHING", "COALESCED", "STALE"] {
            for source in [CacheSource::Memory, CacheSource::Disk] {
                for age in [0u64, 1, 42, 4_294_967_296, u64::MAX] {
                    let old = x_cache_lines(label, source, age);
                    let mut b1 = [0u8; 96];
                    let mut b2 = [0u8; 32];
                    assert_eq!(x_cache_line(&mut b1, label, source), old[0]);
                    assert_eq!(age_line(&mut b2, age), old[1]);
                }
            }
        }
    }

    #[test]
    fn content_range_and_status_match_the_previous_implementation() {
        for (start, end, total) in [(0u64, 0u64, 1u64), (2, 5, 10), (0, u64::MAX - 1, u64::MAX)] {
            let mut b = [0u8; 64];
            assert_eq!(
                content_range_line(&mut b, Some((start, end)), total),
                format!("Content-Range: bytes {}-{}/{}", start, end, total)
            );
            let mut b = [0u8; 64];
            assert_eq!(
                content_range_line(&mut b, None, total),
                format!("Content-Range: bytes */{}", total)
            );
        }
        // ステータスは sanitize_response_head 経由と同じ値になること
        for head in [
            &b"HTTP/1.1 200 OK\r\n\r\n"[..],
            b"HTTP/1.0 404 Not Found\r\n\r\n",
            b"HTTP/1.1 204\r\n\r\n",
            b"garbage",
            b"HTTP/1.1",
            b"",
        ] {
            let old = headers::sanitize_response_head(head)
                .status_line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(200);
            assert_eq!(status_of(head), old, "head={:?}", head);
        }
    }
}
