//! オリジンへ転送する段: 接続を掴み、要求の先頭と本文を送り、応答の先頭を読む。
//! 再利用した接続が死んでいたときと 408 / 421 のやり直し、1xx の中継、失敗時の stale の配信もここ。

use std::io::{self, Write};
use std::time::Instant;

use super::lookup::CacheState;
use super::respond::{forward_interim, write_error};
use super::serve::{can_serve_stale, serve_cached};
use super::upstream::{
    OriginConn, acquire_origin, build_request_head, forward_request_body, is_client_body_error,
    is_stale_conn_error,
};
use super::{ClientWriter, Exchange, Stage, done, finished};
use crate::clientio::ClientReader;
use crate::freshness;
use crate::headers;
use crate::log_trace;
use crate::metrics::{BadRequestReason, Detail, Failure};
use crate::{log_debug, log_warn};

/// オリジンから応答の先頭を読み終えたところ。
pub(super) struct Fetched {
    /// オリジンとの接続。再検証の 304 で保存済みを配るとき (`reply`) に手放したあとだけ `None`
    pub server: Option<OriginConn>,
    /// 応答の先頭 (状態行 + ヘッダー + 空行) の生のバイト列
    pub head: Vec<u8>,
    pub status: u16,
    /// 転送した要求の本文のバイト数
    pub request_body_bytes: u64,
    /// オリジンが要求の本文を読み切らずに閉じた (T.29.7)。立っていれば応答は渡すが、
    /// クライアントの本文の残りが読めていないので、クライアントともオリジンとも続けない
    pub body_cut: bool,
}

/// 再検証の 304 が保存済みとは別の表現を指しているか (RFC 9111 §4.3.4。T.32.9)。
///
/// 304 の `ETag` が保存済みの `ETag` と違う (比べるのは弱い印 `W/` を外して)、または `ETag` で比べられず
/// `Last-Modified` が両方にあって違うときに真。validator の無い 304 と、比べる相手が保存済みに無いときは偽
/// (今までどおり延ばす)。`reply::not_modified_selects` より甘い: こちらは「延ばしてはいけない」の判定
pub(super) fn not_modified_conflicts(
    cached: &freshness::CachedHead,
    pairs_304: &[(String, String)],
) -> bool {
    let value = |name: &str| {
        pairs_304
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim_ascii())
    };
    let weak = |v: &str| v.strip_prefix("W/").unwrap_or(v).to_string();
    if let (Some(got), Some(have)) = (value("etag"), cached.etag.as_deref()) {
        return weak(got) != weak(have.trim_ascii());
    }
    match (value("last-modified"), cached.last_modified.as_deref()) {
        (Some(got), Some(have)) => got != have.trim_ascii(),
        _ => false,
    }
}

/// オリジンへ要求を送り、応答の先頭を読む。
///
/// `send_continue` は `100 Continue` を返してから本文を読むか ([`super::admission::wants_continue`])。
/// 繋げない・読めないときは stale を配るか 502 / 504 を書いて終わる ([`done`])。
#[inline]
pub(super) fn fetch(
    x: &mut Exchange<'_>,
    client: &mut ClientWriter<'_>,
    reader: &mut ClientReader<'_>,
    cs: &mut CacheState<'_>,
    mut send_continue: bool,
) -> Stage<Fetched> {
    let shared = x.shared;
    let cache = x.cache;
    let metrics = x.ctx.metrics;
    let conn_id = x.ctx.conn_id;
    let (method, http11, req_framing) = (x.ctx.method, x.http11, x.req_framing);
    let pool_key = x.ctx.pool_key;
    let conditional_lines = cs
        .stale
        .as_ref()
        .map(|(entry, _)| {
            freshness::conditional_headers(&freshness::parse_cached_head(&entry.head))
        })
        .unwrap_or_default();
    // 転送するヘッダーは Vec<String> を経由せず、そのままバイト列に書く
    let mut request_head = build_request_head(
        method,
        &x.origin,
        x.raw_headers,
        x.client_ip,
        &conditional_lines,
    );
    log_trace!(
        Some(conn_id),
        "origin request head:\n{}",
        String::from_utf8_lossy(&request_head).trim_end()
    );
    // 期限切れの表現が手元にあるなら、オリジンを長く待たずに stale を返す
    // (`shared.timeout` の 0 は「無期限」なので、素の `min` だと 0 秒で諦める方に
    // 化ける。`timeout::shorter` は無期限の側を必ず負けさせる。T10.6)
    let mut origin_timeout = if cs.stale.is_some() {
        proxy_base::timeout::shorter(shared.timeout, cs.cfg.stale_wait)
    } else {
        shared.timeout
    };

    // 本文の無い冪等な要求だけ、再利用した接続が死んでいたときに 1 回やり直す
    let retryable = req_framing == crate::body::Framing::None && (x.is_get || x.ctx.head_only);
    // 1xx を中継したか。中継したらクライアントはもう応答の先頭を受け取っているので、
    // プールの接続を掴み直して同じ 1xx をもう一度出さない (やり直さない)
    let mut interim_sent = false;
    // オリジンが要求の本文を読み切らずに閉じたか (T.29.7)。立っていれば応答は渡すが、
    // クライアントの本文の残りが読めていないので、クライアントともオリジンとも続けない
    let mut body_cut = false;
    let mut attempt = 0;
    loop {
        attempt += 1;
        // 接続にかかった時間は「オリジンを掴むまで − 名前解決」で出す。時計を読むのは
        // ここで 1 回だけで、プールから再利用できたときは 2 回目を読まない (熱い経路)
        let acquire_started = Instant::now();
        // この時計より**前に**払われた名前解決 (入口の ACL) を控える (T15.0 (1))。
        // thread-local の読み 1 回で、時計もシステムコールも増えない
        let dns_before = crate::dns::peek_resolve_cost().0;
        let (mut server, reused) = match acquire_origin(
            &shared.upstream,
            origin_timeout,
            shared.connect_timeout,
            conn_id,
            &x.origin,
            pool_key,
            shared.resolved,
        ) {
            Ok(v) => v,
            Err(e) => {
                return connect_failed(x, client, reader, cs, e, acquire_started, dns_before);
            }
        };
        if !reused {
            x.ctx.detail = Detail::of_connect(
                acquire_started.elapsed(),
                None,
                x.ctx.detail.stages,
                dns_before,
            );
        }
        metrics.inc_origin_conn(reused);
        let sent = send_request(
            x,
            client,
            reader,
            &mut server,
            &request_head,
            &mut send_continue,
            &mut body_cut,
        )?;
        // 要求をオリジンへ送り終えた境目 (T14.3 (1))。ここが forward に足す唯一の時計
        let sent_at = crate::profile::mark();
        // 1xx (100 と 101 を除く) はヘッダーだけクライアントへ中継し、最終応答を読み続ける (T19.6)。
        // HTTP/1.0 のクライアントへは送らない (RFC 9110 §15.2 の MUST NOT。1.0 のクライアントは
        // 103 を最終応答と読む。T.29.3): 読み捨てて最終応答だけを返す
        let result = sent.and_then(|n| {
            crate::response::read_head_forwarding(&mut server, |head, status| {
                if !http11 {
                    log_trace!(
                        Some(conn_id),
                        "dropping interim {} response for an HTTP/1.0 client",
                        status
                    );
                    return Ok(());
                }
                interim_sent = true;
                forward_interim(client, head, status, conn_id)
            })
            .map(|h| (h, n))
        });
        // やり直してよいのは、再利用した接続で、冪等で、1xx を中継しておらず、1 回目のときだけ
        let again = reused && retryable && !interim_sent && attempt == 1;
        let verdict = judge(result, again, |head| {
            cs.stale.as_ref().is_some_and(|(entry, _)| {
                not_modified_conflicts(
                    &freshness::parse_cached_head(&entry.head),
                    &headers::response_pairs(head),
                )
            })
        });
        match verdict {
            Attempt::RetryStatus(status) => {
                log_debug!(
                    Some(conn_id),
                    "pooled connection to {} returned {}, retrying on a fresh one",
                    pool_key,
                    status
                );
                continue;
            }
            // 304 が保存済みとは別の表現を指した: 項目を消し、条件なしで 1 回だけ取り直す。
            // この接続は使い回さない (304 の枠組みを読む前なので)。`stale` を外すので 2 回目は起きない
            Attempt::Refetch => {
                log_debug!(
                    Some(conn_id),
                    "304 from {} names another representation: dropped the stored entry, refetching",
                    pool_key
                );
                cache.remove(cs.key);
                cs.stale = None;
                drop(server);
                request_head =
                    build_request_head(method, &x.origin, x.raw_headers, x.client_ip, &[]);
                origin_timeout = shared.timeout;
                attempt = 0;
                continue;
            }
            Attempt::Use { head, status, n } => {
                if origin_timeout != shared.timeout {
                    let _ = server.get_ref().set_timeouts(shared.timeout);
                }
                // 「forward の初バイト」= オリジンの応答ヘッダーを読み終えたところ
                // (本文の転送は相手と回線の都合なので、待ちの物差しにはこちらを使う)
                x.ctx.detail.first_byte_ms =
                    Some(x.ctx.started.elapsed().as_millis().min(u64::MAX as u128) as u64);
                // 要求を送るのにかかった時間 = 「送り終えるまで」− 名前解決 − 接続 (T14.3 (1))
                if let Some(at) = sent_at {
                    let acquire_and_send =
                        crate::profile::ms_u32(at.saturating_duration_since(acquire_started));
                    let origin_ms = (x.ctx.detail.dns_ms + x.ctx.detail.connect_ms) as u32;
                    x.ctx.detail.stages.send = acquire_and_send.saturating_sub(origin_ms);
                }
                return Ok(Fetched {
                    server: Some(server),
                    head,
                    status,
                    request_body_bytes: n,
                    body_cut,
                });
            }
            Attempt::RetryStale(e) => {
                log_debug!(
                    Some(conn_id),
                    "pooled connection to {} was stale ({}), retrying",
                    pool_key,
                    e
                );
                continue;
            }
            // クライアントの本文が壊れている: 400 で返して閉じる。オリジンへの接続はここで捨てる
            // (プールに戻さない)
            Attempt::ClientBody(e) => {
                if let Some(more) = metrics.record_bad_request(
                    BadRequestReason::BodyFraming,
                    x.client_ip.unwrap_or("-"),
                    400,
                ) {
                    log_warn!(Some(conn_id), "400 Bad Request: {}{}", e, more);
                }
                let _ = write_error(client, 400, "Bad Request");
                return done(false);
            }
            Attempt::Fail(e) => {
                // オリジンの読み書きはブロッキング + `SO_RCVTIMEO` / `SO_SNDTIMEO` なので、
                // Linux では締め切りが切れると `WouldBlock` (EAGAIN) が返る。そのまま出すと
                // "Resource temporarily unavailable (os error 11)" になって締め切りと読めないので、
                // 言い換える (原因の数え方は `ErrCause::from_io` が `timeout` に寄せる。T.28.5)
                // 応答を読む途中の失敗も同じ分け方 (T25.0)。DNS はここまで来ないが、
                // `cause` と `dns_cause` を 1 か所で決めておくと組み合わせがずれない
                let failure = Failure::from_io(&e);
                // WARN は理由ごとに 1 分に 1 行 (T.29.15)
                if let Some(more) = shared.metrics.origin_warn(failure) {
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) {
                        log_warn!(
                            Some(conn_id),
                            "origin response timed out after {} s{}",
                            origin_timeout.as_secs_f64(),
                            more
                        );
                    } else {
                        log_warn!(
                            Some(conn_id),
                            "failed to read origin response: {}{}",
                            e,
                            more
                        );
                    }
                }
                x.ctx.detail.cause = Some(failure.cause);
                x.ctx.detail.dns_cause = failure.dns;
                if let Some((entry, source)) = cs.stale.take()
                    && !cs.force_revalidate
                    && can_serve_stale(&entry)
                {
                    cache.note_stale_served();
                    return finished(serve_cached(client, entry, source, "STALE", 0, &x.ctx));
                }
                write_error(client, 502, "Bad Gateway")?;
                x.ctx.log_failure(502);
                return done(false);
            }
        }
    }
}

/// 1 回の試み (要求を送って応答の先頭を読んだ) の結果をどう扱うか ([`judge`] が決める。T.34.17)。
#[derive(Debug)]
enum Attempt {
    /// 再利用した接続が 408 / 421 を返した: 新しい接続でやり直す
    RetryStatus(u16),
    /// 再検証の 304 が保存済みとは別の表現を指した: 保存済みを捨てて条件なしで取り直す
    Refetch,
    /// 応答の先頭を読めた: 次の段へ渡す
    Use { head: Vec<u8>, status: u16, n: u64 },
    /// 再利用した接続が死んでいた: 新しい接続でやり直す
    RetryStale(io::Error),
    /// クライアントの本文が壊れている (400)
    ClientBody(io::Error),
    /// オリジンとの読み書きに失敗した (stale か 502)
    Fail(io::Error),
}

/// 1 回の試みの結果を [`Attempt`] に分ける (ソケットに触らない判定だけ)。
///
/// `again` はやり直してよい試みか (再利用した接続・冪等・1xx を中継していない・1 回目)。
/// `conflicts` は 304 の先頭が保存済みとは別の表現を指すか (304 のときだけ呼ぶ)。
#[inline]
fn judge(
    result: io::Result<((Vec<u8>, u16), u64)>,
    again: bool,
    conflicts: impl FnOnce(&[u8]) -> bool,
) -> Attempt {
    match result {
        // 408 (Request Timeout) と 421 (Misdirected Request) は、再利用した接続に
        // 積まれていた応答か、この接続では処理できない印。冪等ならやり直す (RFC 9110 §15.5.20)
        Ok(((_, status), _)) if again && (status == 408 || status == 421) => {
            Attempt::RetryStatus(status)
        }
        // 再検証の 304 の validator が保存済みと「あって違う」(RFC 9111 §4.3.4。T.32.9): その 304 で
        // 保存済みを延ばすと、古い本文に新しい `ETag` を付けて配り続ける (下流は新しい `ETag` で
        // 304 をもらい続ける)。本文の無い GET だけ
        Ok(((head, 304), _)) if conflicts(&head) => Attempt::Refetch,
        Ok(((head, status), n)) => Attempt::Use { head, status, n },
        Err(e) if again && is_stale_conn_error(&e) => Attempt::RetryStale(e),
        // クライアントの本文が壊れている (chunk の大きさが読めない など)。オリジンの誤りではない
        // ので 502 ではなく 400 で返して閉じる (T27.8)
        Err(e) if is_client_body_error(&e) => Attempt::ClientBody(e),
        Err(e) => Attempt::Fail(e),
    }
}

/// オリジンを掴めなかった (名前解決・接続の失敗)。stale を配るか、502 / 504 を書いて終わる。
#[inline]
fn connect_failed<T>(
    x: &mut Exchange<'_>,
    client: &mut ClientWriter<'_>,
    reader: &mut ClientReader<'_>,
    cs: &mut CacheState<'_>,
    e: io::Error,
    acquire_started: Instant,
    dns_before: u64,
) -> Stage<T> {
    let (shared, cache, conn_id) = (x.shared, x.cache, x.ctx.conn_id);
    let (req_framing, server_addr) = (x.req_framing, x.server_addr);
    // 名前解決が締め切りを過ぎた (または上限で断った) ときは 504 (T.32.11)
    let (code, reason) = if crate::dns::is_timeout(&e) {
        (504, "Gateway Timeout")
    } else {
        (502, "Bad Gateway")
    };
    // 理由ごとに 1 分に 1 行 (T.29.15。本数は `/errors` と `/hosts` が持つ)
    if let Some(more) = shared.metrics.origin_warn(Failure::from_io(&e)) {
        log_warn!(
            Some(conn_id),
            "{} {}: connect {} failed: {}{}",
            code,
            reason,
            server_addr,
            e,
            more
        );
    }
    // 入口で測った `client_read` は `ctx.detail` にしか無いので、
    // `shared.stages` ではなくこちらを引き継ぐ (T17.4)
    x.ctx.detail = Detail::of_connect(
        acquire_started.elapsed(),
        Some(Failure::from_io(&e)),
        x.ctx.detail.stages,
        dns_before,
    );
    if let Some((entry, source)) = cs.stale.take()
        && !cs.force_revalidate
        && can_serve_stale(&entry)
    {
        crate::body::drain(reader, req_framing)?;
        cache.note_stale_served();
        return finished(serve_cached(client, entry, source, "STALE", 0, &x.ctx));
    }
    write_error(client, code, reason)?;
    // 原因つきで 1 件数える (デプロイ先の「エラー 12 件、原因は不明」を無くす)
    x.ctx.log_failure(code);
    done(false)
}

/// 要求の先頭と本文をオリジンへ送る (`100 Continue` を返すのもここ)。戻り値はオリジンへの送りの結果
/// (本文のバイト数)。クライアントへ `100 Continue` を書けなければ `Done` で終わる。
///
/// オリジンが本文を読み切らずに閉じたときは `body_cut` を立て、送れたところまでを `Ok` にする。
#[inline]
fn send_request(
    x: &Exchange<'_>,
    client: &mut ClientWriter<'_>,
    reader: &mut ClientReader<'_>,
    server: &mut OriginConn,
    request_head: &[u8],
    send_continue: &mut bool,
    body_cut: &mut bool,
) -> Stage<io::Result<u64>> {
    let (shared, conn_id) = (x.shared, x.ctx.conn_id);
    let (req_framing, pool_key) = (x.req_framing, x.ctx.pool_key);
    let head_sent = server.get_mut().write_all(request_head);
    // 本文のある要求はやり直さない (`retryable` が偽) ので、100 は高々 1 回。
    // クライアントへ書けなければここで終わる (オリジンの誤りと混ぜない)
    if head_sent.is_ok() && *send_continue {
        *send_continue = false;
        client.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        client.flush()?;
    }
    let mut body_sent = 0u64;
    let sent = head_sent
        .and_then(|_| {
            // 本文を送ってこないクライアントを見張る (T.29.34。見張りは本文のある要求だけが作る)
            forward_request_body(
                reader,
                server.get_mut(),
                req_framing,
                &mut body_sent,
                shared.min_transfer,
                conn_id,
            )
        })
        .and_then(|n| server.get_mut().flush().map(|_| n));
    // 本文を送る途中でオリジンが閉じた (`EPIPE` / `ECONNRESET`): 先に応答 (413 など) を返して
    // 閉じたのかもしれないので、読み残しの応答を読みに行く (RFC 9112 §9.5。T.29.7)。
    // 読めなければ今までどおり 502。本文の無い要求は対象外 (死んだ接続のやり直しに回す)。
    // 正常な要求にはシステムコールを 1 つも足さない (失敗したときだけ通る)
    Ok(match sent {
        Err(e)
            if req_framing.has_body()
                && !is_client_body_error(&e)
                && matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) =>
        {
            log_debug!(
                Some(conn_id),
                "origin {} stopped reading the request body after {}B ({}); reading its response",
                pool_key,
                body_sent,
                e
            );
            *body_cut = true;
            Ok(body_sent)
        }
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::upstream::client_body_error;

    fn got(status: u16) -> io::Result<((Vec<u8>, u16), u64)> {
        Ok((
            (format!("HTTP/1.1 {status} X\r\n\r\n").into_bytes(), status),
            7,
        ))
    }

    fn never(_: &[u8]) -> bool {
        panic!("304 でないのに保存済みと比べた")
    }

    /// 408 / 421 はやり直してよい試みのときだけやり直し、そうでなければそのまま渡す (T.34.17)。
    #[test]
    fn judge_retries_408_and_421_only_when_the_attempt_may_be_repeated() {
        for status in [408, 421] {
            assert!(matches!(
                judge(got(status), true, never),
                Attempt::RetryStatus(s) if s == status
            ));
            assert!(matches!(
                judge(got(status), false, never),
                Attempt::Use { status: s, n: 7, .. } if s == status
            ));
        }
        assert!(matches!(
            judge(got(200), true, never),
            Attempt::Use { status: 200, .. }
        ));
    }

    /// 304 だけを保存済みと比べ、別の表現を指していれば取り直す。
    #[test]
    fn judge_refetches_only_a_304_that_names_another_representation() {
        assert!(matches!(judge(got(304), true, |_| true), Attempt::Refetch));
        assert!(matches!(judge(got(304), false, |_| true), Attempt::Refetch));
        assert!(matches!(
            judge(got(304), true, |_| false),
            Attempt::Use { status: 304, .. }
        ));
        let mut seen = Vec::new();
        judge(got(304), true, |h| {
            seen.extend_from_slice(h);
            false
        });
        assert_eq!(seen, b"HTTP/1.1 304 X\r\n\r\n");
    }

    /// 死んだ接続の印はやり直してよいときだけやり直し、クライアントの本文の誤りは 400 に回す。
    #[test]
    fn judge_sorts_the_errors() {
        let reset = || Err(io::Error::from(io::ErrorKind::ConnectionReset));
        assert!(matches!(
            judge(reset(), true, never),
            Attempt::RetryStale(_)
        ));
        assert!(matches!(judge(reset(), false, never), Attempt::Fail(_)));
        let timeout = || Err(io::Error::from(io::ErrorKind::WouldBlock));
        assert!(matches!(judge(timeout(), true, never), Attempt::Fail(_)));
        // 本文の誤りでも、やり直してよい試みなら死んだ接続の印 (種類) の方を先に見る (今までの順)。
        // 本文のある要求はやり直さないので、実際には重ならない
        let body = |kind| Err(client_body_error(io::Error::from(kind)));
        assert!(matches!(
            judge(body(io::ErrorKind::InvalidData), false, never),
            Attempt::ClientBody(_)
        ));
        assert!(matches!(
            judge(body(io::ErrorKind::InvalidData), true, never),
            Attempt::RetryStale(_)
        ));
        assert!(matches!(
            judge(body(io::ErrorKind::Other), true, never),
            Attempt::ClientBody(_)
        ));
    }
}
