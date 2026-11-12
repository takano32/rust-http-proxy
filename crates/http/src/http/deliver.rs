//! 配信の段: 保存の方針を決め、クライアント向けの先頭を組み、本文を運びながら保存し、
//! オリジンの接続の後始末と状態の文字列・アクセスログまで。

use std::borrow::Cow;
#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
// BufRead は splice の枝でだけ使う (T36.31)
use std::io::{self, BufRead, Read, Write};

use super::buffers::{COPY_BUF_SIZE, CopyBuf};
use super::ctx::content_length_line;
use super::fetch::Fetched;
use super::lookup::CacheState;
use super::reply::HeadFacts;
use super::respond::write_error;
use super::serve::age_line;
use super::upstream::{OriginConn, is_eof_like};
use super::{ClientWriter, Exchange, Stage, done};
use crate::body::{self, BodyReader, Framing};
use crate::cache::{FetchOutcome, StoreSink};
use crate::freshness;
use crate::headers;
use crate::metrics::{ErrCause, Failure};
use crate::request::parse_origin;
use crate::{log_debug, log_warn};

/// 応答を配信しつつ保存し、後始末とアクセスログまで済ませる。戻り値はクライアント接続を次の要求に使えるか。
#[inline]
pub(super) fn deliver(
    x: &mut Exchange<'_>,
    client: &mut ClientWriter<'_>,
    cs: &mut CacheState<'_>,
    fetched: Fetched,
    facts: HeadFacts,
) -> Stage<bool> {
    let Fetched {
        server,
        head,
        status,
        request_body_bytes,
        ..
    } = fetched;
    // 応答の先頭を見た段 (`reply`) が 304 で接続を手放すときは `Done` で抜けているので、ここでは必ず持っている
    let mut server = server.expect("the origin connection is handed over to deliver");
    let HeadFacts {
        framing,
        age_secs,
        has_date,
        origin_reusable,
        unframed_te,
    } = facts;
    let key = cs.key;
    let (cache, conn_id) = (x.cache, x.ctx.conn_id);
    let (http11, head_only, url) = (x.http11, x.ctx.head_only, x.ctx.url);

    // ---- 配信しつつ保存 ----
    let policy = storage_plan(x, cs, status, &head, unframed_te.is_some());
    // 保存するのは元の Location のまま (配信時に必要なら書き換える)。
    // マッピング形式でなければ String を 1 本ずつ作らずに直接バイト列へ書く。
    // **保存しないなら組み立てもしない**: 素通しの経路では、この 1 本のために
    // 応答ヘッダーをもう一度なめて Vec を育て直していた (要求ごとに確保 5 回)
    let cached_head = policy.is_some().then(|| {
        let mut out = Vec::with_capacity(head.len() + 64);
        headers::write_response_head(&mut out, &head, None, &[], &[] as &[&str]);
        out
    });
    let sanitized = x.origin.mapped.then(|| {
        let mut h = headers::sanitize_response_head(&head);
        crate::request::map_locations(&mut h.lines);
        h
    });
    let client_framing = client_framing(framing, unframed_te.is_some(), http11);
    if client_framing == Framing::Close {
        x.ctx.keep_client = false;
    }
    let client_head = build_client_head(
        &head,
        sanitized.as_ref(),
        client_framing,
        unframed_te.as_deref(),
        head_only,
        http11,
        has_date,
        age_secs,
        x.ctx.connection_line(),
    );
    let expected = match framing {
        Framing::Length(n) => cached_head
            .as_ref()
            .map(|h| n.saturating_add(h.len() as u64)),
        _ => None,
    };
    let mut sink =
        policy.map(|p| cache.begin_store(key, url, p.ttl, p.age, p.validators, expected, conn_id));
    if let (Some(s), Some(h)) = (sink.as_mut(), cached_head.as_ref()) {
        s.write(h);
    }

    let (mut pre_buf, early) = read_small_body_first(
        x,
        client,
        &mut server,
        &mut sink,
        framing,
        client_framing,
        head_only,
    )?;

    client.write_all(&client_head)?;
    let mut body_bytes = early as u64;
    if let Some(buf) = pre_buf.as_ref()
        && early > 0
    {
        // 先に読み切った本文 (ヘッダーのすぐ後ろに置く)
        client.write_all(&buf[..early])?;
    }
    let (moved, clean) = relay_body(
        x,
        client,
        &mut server,
        &mut sink,
        &mut pre_buf,
        early,
        framing,
        client_framing,
        head_only,
    )?;
    body_bytes += moved;
    if clean && client_framing == Framing::Chunked {
        body::write_last_chunk(client)?;
    }
    client.flush()?;

    if clean && origin_reusable {
        x.shared
            .upstream
            .pool
            .put(x.ctx.pool_key, server, x.shared.timeout);
    }
    let total = client_head.len() as u64 + body_bytes;
    let keep = finish(
        x,
        cs,
        policy.as_ref(),
        &mut sink,
        clean,
        (status, total, request_body_bytes),
    );
    Ok(keep)
}

/// unsafe メソッドへの成功応答で、対象 URL と `Location` / `Content-Location` の先 (同じオリジンのもの) の
/// キャッシュを無効化する (RFC 9111 §4.4)。
#[inline]
fn invalidate_targets(x: &Exchange<'_>, head: &[u8]) {
    x.cache.invalidate(x.ctx.url, x.ctx.conn_id);
    for name in ["location", "content-location"] {
        if let Some(target) = headers::response_value(head, name)
            && let Ok(o) = parse_origin(target, None)
            && o.scheme == x.origin.scheme
            && o.server_addr() == x.server_addr
        {
            x.cache.invalidate(&o.url(), x.ctx.conn_id);
        }
    }
}

/// クライアント向けの枠組み: 長さが分かればそのまま、分からなければ HTTP/1.1 には再 chunk。
/// 最後が chunked でない TE の応答 (`unframed_te`) は、オリジンの TE の行を残して閉じるまでを本文にする
/// (T28.35。`gzip, chunked` に包み直してもよいが、バイト列をオリジンのまま渡す方を選んだ)
#[inline]
fn client_framing(framing: Framing, unframed_te: bool, http11: bool) -> Framing {
    match framing {
        Framing::None | Framing::Length(_) => framing,
        Framing::Close if unframed_te => Framing::Close,
        Framing::Chunked | Framing::Close => {
            if http11 {
                Framing::Chunked
            } else {
                Framing::Close
            }
        }
    }
}

/// クライアントへ返す応答の先頭を組む。オリジンの先頭から hop-by-hop と枠組みの名前を落とし、
/// 自分で決めた枠組み・`Date` (無ければ)・`Age`・`Via`・`Connection` の行を足す。
#[inline]
#[allow(clippy::too_many_arguments)]
fn build_client_head(
    head: &[u8],
    sanitized: Option<&headers::ResponseHead>,
    client_framing: Framing,
    unframed_te: Option<&str>,
    head_only: bool,
    http11: bool,
    has_date: bool,
    age_secs: Option<u64>,
    connection: &'static str,
) -> Vec<u8> {
    // 足すヘッダー行は借用のまま持つ (`Vec<String>` と `format!` を要求ごとに作らない)
    let mut cl_buf = [0u8; 40];
    // HEAD のときだけ確保する (それ以外は 1 バイトも作らない)
    let cl_owned;
    let mut age_buf = [0u8; 32];
    let mut date_buf = [0u8; 35];
    let mut extra: [&str; 5] = [""; 5];
    let mut n_extra = 0usize;
    match client_framing {
        Framing::Length(len) => {
            extra[0] = content_length_line(&mut cl_buf, len);
            n_extra = 1;
        }
        Framing::Chunked => {
            extra[0] = "Transfer-Encoding: chunked";
            n_extra = 1;
        }
        Framing::Close if let Some(te) = unframed_te => {
            extra[0] = te;
            n_extra = 1;
        }
        Framing::None if head_only => {
            // HEAD: GET と同じヘッダーを返す (本文は無い)。TE があれば GET と同じく、最後が chunked なら
            // `Transfer-Encoding: chunked`、そうでなければオリジンの並びを 1 行で (HTTP/1.1 のクライアントだけ。
            // T28.36)。TE が無ければ `Content-Length`
            let te = if http11 {
                Framing::response_transfer_encoding_line(head)
            } else {
                None
            };
            if let Some(te) = te {
                let chunked = te
                    .rsplit(',')
                    .next()
                    .is_some_and(|t| t.trim_ascii().eq_ignore_ascii_case("chunked"));
                cl_owned = if chunked {
                    "Transfer-Encoding: chunked".to_string()
                } else {
                    te
                };
                extra[0] = &cl_owned;
                n_extra = 1;
            } else if let Some(v) = headers::response_value(head, "content-length") {
                cl_owned = format!("Content-Length: {}", v);
                extra[0] = &cl_owned;
                n_extra = 1;
            }
        }
        _ => {}
    }
    // 応答にも自分の `Via` を足す (RFC 9110 §7.6.3。T28.10 (b))。保存する先頭 (`cached_head`) には入れない
    // `Date` の無い応答には自分で付ける (RFC 9110 §6.6.1: 時計を持つ送り手は付けなければならない。T29.9)。
    // 有無は枠組みと同じ 1 回の走査で見ているので、`Date` のある応答 (ほとんど全部) には何も足さない
    if !has_date {
        extra[n_extra] = crate::httpdate::date_line(&mut date_buf);
        n_extra += 1;
    }
    if let Some(age) = age_secs {
        extra[n_extra] = age_line(&mut age_buf, age);
        n_extra += 1;
    }
    extra[n_extra] = crate::via::field();
    n_extra += 1;
    extra[n_extra] = connection;
    n_extra += 1;
    let extra = &extra[..n_extra];
    match sanitized {
        Some(h) => h.assemble(extra),
        None => {
            let mut out = Vec::with_capacity(head.len() + 96);
            headers::write_response_head(&mut out, head, None, &[], extra);
            out
        }
    }
}

/// 小さな本文は、ヘッダーを出す前に読み切る (T19.5)。戻り値は (読んだ置き場, 読めたバイト数)。
///
/// オリジンが名乗った `Content-Length` より短い本文で閉じたとき、**ヘッダーをまだ
/// 出していなければ 502 に差し替えられる**。出したあとでは差し替えられない (接続を
/// 閉じるだけ) ので、手元のバッファ ([`CopyBuf`]、32 KiB) に収まる本文だけ先に読む。
///
/// 読む量も書く量も今までと同じで、変わるのはヘッダーを出す順番だけ
/// (正常な応答のバイト列は 1 バイトも変わらない)。**代償は初バイト**: この大きさの
/// 本文では、ヘッダーがクライアントに届くのが本文を読み終えた後ろになる。上限を
/// 中継バッファと同じ 32 KiB にしてあるのは、この待ちを「ふつうの小さな応答が
/// 1 パケットで届く範囲」に収めるため (それを超える本文は今までどおり流しながら運ぶ)
#[inline]
fn read_small_body_first(
    x: &mut Exchange<'_>,
    client: &mut ClientWriter<'_>,
    server: &mut OriginConn,
    sink: &mut Option<StoreSink<'_>>,
    framing: Framing,
    client_framing: Framing,
    head_only: bool,
) -> Stage<(Option<CopyBuf>, usize)> {
    let conn_id = x.ctx.conn_id;
    let mut pre_buf: Option<CopyBuf> = None;
    let mut early = 0usize;
    if !head_only
        && client_framing == framing
        && let Framing::Length(total) = framing
        && total as usize <= COPY_BUF_SIZE
    {
        let buf = pre_buf.get_or_insert_with(CopyBuf::take);
        let mut body = BodyReader::new(&mut *server, framing);
        while early < total as usize {
            match body.read(&mut buf[early..total as usize]) {
                Ok(0) => break,
                Ok(n) => early += n,
                Err(ref e) if is_eof_like(e) => {
                    log_debug!(Some(conn_id), "origin body ended early: {}", e);
                    break;
                }
                Err(e) => {
                    if let Some(s) = sink.take() {
                        s.abort();
                    }
                    return Err(e.into());
                }
            }
        }
        if early < total as usize {
            // 名乗った長さに足りないままオリジンが閉じた。**まだ 1 バイトも出していない**ので、
            // 中途半端な `Content-Length` を残さず 502 に差し替える (この接続は再利用しない)
            if let Some(s) = sink.take() {
                s.abort();
            }
            // 原因は既存の 8 つ (EOF で切られた = `reset`)
            x.ctx.detail.cause = Some(ErrCause::Reset);
            if let Some(more) = x.ctx.metrics.origin_warn(Failure::of(ErrCause::Reset)) {
                log_warn!(
                    Some(conn_id),
                    "502 Bad Gateway: origin body ended at {}B of {}B{}",
                    early,
                    total,
                    more
                );
            }
            write_error(client, 502, "Bad Gateway")?;
            x.ctx.log_failure(502);
            return done(false);
        }
        if let Some(s) = sink.as_mut() {
            s.write(&buf[..early]);
        }
    }
    Ok((pre_buf, early))
}

/// 本文を運ぶ (先に読み切った `early` バイトの続きから)。戻り値は (運んだバイト数, 最後まで届いたか)。
///
/// 書き換えも保存もしない大きな本文は `splice(2)` でカーネル内を運び、それ以外は中継バッファで
/// 読んで書く (chunked には再 chunk)。保存する本文は同じバイトを保存の受け口にも流す。
#[inline]
#[allow(clippy::too_many_arguments)]
fn relay_body(
    x: &mut Exchange<'_>,
    client: &mut ClientWriter<'_>,
    server: &mut OriginConn,
    sink: &mut Option<StoreSink<'_>>,
    pre_buf: &mut Option<CopyBuf>,
    early: usize,
    framing: Framing,
    client_framing: Framing,
    head_only: bool,
) -> Stage<(u64, bool)> {
    let conn_id = x.ctx.conn_id;
    let mut body_bytes = 0u64;
    let mut clean = true;
    // ---- 書き換えも保存もしない本文は splice(2) でカーネル内を運ぶ ----
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut spliced = false;
    #[cfg(target_os = "linux")]
    if early == 0
        && sink.is_none()
        && !head_only
        && client_framing == framing
        && let Framing::Length(total) = framing
        && let crate::origin::OriginStream::Plain(origin_tcp) = server.get_ref()
        && total.saturating_sub(server.buffer().len() as u64)
            >= super::passthrough::SPLICE_MIN_BYTES
    {
        use std::os::fd::AsRawFd;
        let (moved, ok) = splice_body(x, client, server, origin_tcp.as_raw_fd(), total)?;
        body_bytes += moved;
        clean = ok;
        spliced = true;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = head_only;

    if !spliced && early == 0 {
        let buf: &mut [u8] = pre_buf.get_or_insert_with(CopyBuf::take);
        let mut body = BodyReader::new(&mut *server, framing);
        loop {
            let n = match body.read(&mut *buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(ref e) if is_eof_like(e) => {
                    log_debug!(Some(conn_id), "origin body ended early: {}", e);
                    clean = false;
                    break;
                }
                // chunk の枠が壊れている・行が長すぎる (T29.1)。先頭はもう出ているので、途中で切れた
                // 本文と同じに扱う (終端の chunk を出さずに閉じる)。本数はオリジンが決めるので 1 分に 1 行
                Err(ref e) if e.kind() == io::ErrorKind::InvalidData => {
                    static FRAMING_WARN: crate::log::Throttle = crate::log::Throttle::new();
                    if let Some(more) = FRAMING_WARN.pass(crate::log::WARN_EVERY_SECS) {
                        log_warn!(
                            Some(conn_id),
                            "origin body from {} cut: {}{}",
                            x.ctx.pool_key,
                            e,
                            more
                        );
                    }
                    clean = false;
                    break;
                }
                Err(e) => {
                    if let Some(s) = sink.take() {
                        s.abort();
                    }
                    return Err(e.into());
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
    Ok((body_bytes, clean))
}

/// 保存の方針を決める (保存してよい要求で、閉じるまでの TE でない応答だけ)。保存しないと決まった
/// 新しい表現 (2xx / 3xx) が届いたら古い表現も捨て、unsafe メソッドへの成功応答では対象のキャッシュを
/// 無効化する (T34.19)。
#[inline]
fn storage_plan(
    x: &Exchange<'_>,
    cs: &CacheState<'_>,
    status: u16,
    head: &[u8],
    unframed_te: bool,
) -> Option<freshness::Policy> {
    let (cache, head_only) = (x.cache, x.ctx.head_only);
    let (key, now, cfg, cache_on) = (cs.key, cs.now, cs.cfg, cs.cache_on);
    let policy = if cs.store_allowed && !unframed_te {
        freshness::response_policy_lines(
            status,
            headers::response_lines(head),
            x.ctx.req.cookie,
            cfg,
            now,
        )
    } else {
        None
    };
    if policy.is_none() && cs.stale.is_some() && (200..400).contains(&status) && status != 206 {
        // 新しい表現が届いたのに保存できないので、古い表現も捨てる (4xx/5xx では残す)。
        // `Range` 要求への 206 は全体の置き換えではないので残す (T34.143。捨てると次の素の GET が
        // 再検証で延ばせずに MISS になる)
        cache.remove(key);
    }
    // unsafe メソッドへの成功応答は対象 URL (と Location 先) のキャッシュを無効化する (RFC 9111 §4.4)
    if !x.is_get && !head_only && (200..400).contains(&status) && cache_on {
        invalidate_targets(x, head);
    }
    policy
}

/// 本文を運び終えたあとの始末: 保存を締め (または捨て)、合流を待つ要求へ結果を知らせ、バイトを数えて
/// アクセスログを書く (使い回せる接続をプールへ戻すのは呼ぶ側で、この前)。戻り値はクライアント接続を
/// 次の要求に使えるか。
///
/// `(status, total, request_body_bytes)` の `total` はクライアントへ書いた先頭と本文のバイト数。
#[inline]
fn finish(
    x: &mut Exchange<'_>,
    cs: &mut CacheState<'_>,
    policy: Option<&freshness::Policy>,
    sink: &mut Option<StoreSink<'_>>,
    clean: bool,
    (status, total, request_body_bytes): (u16, u64, u64),
) -> bool {
    let (cache, metrics) = (x.cache, x.ctx.metrics);
    let (key, now, lookup_allowed) = (cs.key, cs.now, cs.lookup_allowed);
    let cache_state: Cow<'static, str> = if clean {
        match (policy, sink.take()) {
            (Some(p), Some(s)) => {
                let out = s.finish();
                if out.memory || out.disk {
                    if let Some(guard) = cs.leader.take() {
                        guard.complete(FetchOutcome::Stored);
                    }
                    // 保存できたときだけ組み立てる (素通しの経路では確保しない)
                    Cow::Owned(format!("MISS stored ttl={}s", p.ttl.as_secs()))
                } else {
                    Cow::Borrowed("MISS")
                }
            }
            _ if !lookup_allowed => Cow::Borrowed("BYPASS"),
            _ => Cow::Borrowed("MISS"),
        }
    } else {
        // 途中で切れた本文はクライアントにもそれと分かる形 (終端チャンク無し / 短い本文) で伝わる
        if let Some(s) = sink.take() {
            s.abort();
        }
        x.ctx.keep_client = false;
        Cow::Borrowed("MISS truncated")
    };

    // 保存されなかった場合は待っている要求に自分で取りに行かせる (Drop でも通知される)
    if let Some(guard) = cs.leader.take() {
        // この鍵は保存されないと分かったので覚えておく (次の同時ミスは合流させない。T11.9)。
        // 途中で切れた転送は一時的な失敗なので覚えない (次はうまくいくかもしれない)
        if clean {
            cache.remember_not_stored(key, now);
        }
        guard.complete(FetchOutcome::NotStored);
    }
    // 起動時の自己ベンチ (T14.43) が自分で打った要求のバイトは合計に足さない
    // (`/status` の `bytes_forwarded` と `/history` の `bytes` に 20 MB の山が立つため)。
    // 費用は自己ベンチが回っていないときの原子の読み 1 回
    if !proxy_metrics::selfbench::is_target(x.ctx.pool_key) {
        metrics.add_bytes(total + request_body_bytes);
    }
    // 個票の「上り」は要求の本文ぶん (ヘッダーぶんは本体クレートが足す。T14.4)
    x.ctx.up_bytes = request_body_bytes;
    x.ctx.log(status, total, &cache_state);
    x.ctx.keep_client
}

/// 書き換えも保存もしない大きな本文を `splice(2)` でカーネル内を運ぶ (Linux だけ)。
/// 戻り値は (運んだバイト数, 最後まで届いたか)。
#[cfg(target_os = "linux")]
#[inline]
fn splice_body(
    x: &Exchange<'_>,
    client: &mut ClientWriter<'_>,
    server: &mut OriginConn,
    origin_fd: std::os::fd::RawFd,
    total: u64,
) -> Stage<(u64, bool)> {
    use std::os::fd::AsRawFd;
    let conn_id = x.ctx.conn_id;
    let mut moved_prefix = 0u64;
    let client_fd = client.get_ref().get_ref().as_raw_fd();
    // 応答ヘッダーを読むときに一緒に読んでしまった本文の先頭は自分で書く
    // (ヘッダーと同じ 1 回の sendto に載る)
    let buffered = server.buffer().len().min(total as usize);
    if buffered > 0 {
        let prefix = server.buffer()[..buffered].to_vec();
        client.write_all(&prefix)?;
        server.consume(buffered);
        moved_prefix = buffered as u64;
    }
    client.flush()?;
    // 最小の速さを見るときだけ、運ぶ間クライアントを待たない形にする (T29.34)
    let nonblocking = client.get_ref().is_on();
    if nonblocking {
        client.get_ref().get_ref().set_nonblocking(true)?;
    }
    let relayed = super::passthrough::relay(
        origin_fd,
        client_fd,
        total - buffered as u64,
        client.get_mut().guard(),
        x.shared.timeout,
    );
    if nonblocking {
        client.get_ref().get_ref().set_nonblocking(false)?;
    }
    let (moved, ok) = relayed?;
    crate::log_trace!(Some(conn_id), "spliced {}B of body to the client", moved);
    Ok((moved_prefix + moved, ok))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// クライアント向けの枠組み: 長さが分かればそのまま、分からなければ 1.1 には再 chunk、1.0 には閉じるまで。
    /// 最後が chunked でない TE の応答は版に関わらず閉じるまで (TE の行は残す。T28.35)。
    #[test]
    fn client_framing_follows_the_version() {
        for f in [Framing::None, Framing::Length(5)] {
            assert_eq!(client_framing(f, false, true), f);
            assert_eq!(client_framing(f, false, false), f);
        }
        assert_eq!(
            client_framing(Framing::Chunked, false, true),
            Framing::Chunked
        );
        assert_eq!(
            client_framing(Framing::Chunked, false, false),
            Framing::Close
        );
        assert_eq!(
            client_framing(Framing::Close, false, true),
            Framing::Chunked
        );
        assert_eq!(client_framing(Framing::Close, false, false), Framing::Close);
        assert_eq!(client_framing(Framing::Close, true, true), Framing::Close);
        assert_eq!(client_framing(Framing::Close, true, false), Framing::Close);
    }

    /// クライアント向けの先頭: 枠組みの行 → (`Date`) → (`Age`) → `Via` → `Connection` の順に足し、
    /// オリジンの枠組みと hop-by-hop の名前は落ちる。HEAD は TE か `Content-Length` をそのまま伝える。
    #[test]
    fn client_head_adds_the_framing_date_age_via_and_connection() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nKeep-Alive: timeout=5\r\n\r\n";
        let out = build_client_head(
            head,
            None,
            Framing::Length(5),
            None,
            false,
            true,
            false,
            Some(3),
            "Connection: keep-alive",
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\nDate: "
            ),
            "{}",
            text
        );
        assert!(
            text.ends_with(&format!(
                "\r\nAge: 3\r\nVia: {}\r\nConnection: keep-alive\r\n\r\n",
                crate::via::token()
            )),
            "{}",
            text
        );
        assert!(!text.contains("Keep-Alive"), "{}", text);
        // `Date` があれば足さない。chunked は `Transfer-Encoding: chunked`、TE の並びはそのまま
        let dated = b"HTTP/1.1 200 OK\r\nDate: Mon, 01 Jan 2024 00:00:00 GMT\r\n\r\n";
        let out = build_client_head(
            dated,
            None,
            Framing::Chunked,
            None,
            false,
            true,
            true,
            None,
            "Connection: close",
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "HTTP/1.1 200 OK\r\nDate: Mon, 01 Jan 2024 00:00:00 GMT\r\nTransfer-Encoding: chunked\r\nVia: {}\r\nConnection: close\r\n\r\n",
                crate::via::token()
            )
        );
        let out = build_client_head(
            dated,
            None,
            Framing::Close,
            Some("Transfer-Encoding: gzip"),
            false,
            true,
            true,
            None,
            "Connection: close",
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("\r\nTransfer-Encoding: gzip\r\nVia: "),
            "TE の並びをそのまま"
        );
        // HEAD: TE があれば (1.1 だけ) それを、無ければ `Content-Length` をオリジンの値のまま
        let head_te = b"HTTP/1.1 200 OK\r\nDate: x\r\nTransfer-Encoding: gzip, chunked\r\n\r\n";
        let out = build_client_head(
            head_te,
            None,
            Framing::None,
            None,
            true,
            true,
            true,
            None,
            "Connection: close",
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("\r\nTransfer-Encoding: chunked\r\n")
        );
        let out = build_client_head(
            head_te,
            None,
            Framing::None,
            None,
            true,
            false,
            true,
            None,
            "Connection: close",
        );
        assert!(
            !String::from_utf8(out)
                .unwrap()
                .contains("Transfer-Encoding"),
            "1.0 には TE を伝えない"
        );
        let head_cl = b"HTTP/1.1 200 OK\r\nDate: x\r\nContent-Length: 0012\r\n\r\n";
        let out = build_client_head(
            head_cl,
            None,
            Framing::None,
            None,
            true,
            true,
            true,
            None,
            "Connection: close",
        );
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("\r\nContent-Length: 0012\r\n"),
            "値はそのまま"
        );
    }
}
