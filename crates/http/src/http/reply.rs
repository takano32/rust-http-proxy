//! 応答の先頭を見て決める段: 枠組み・`Connection: close`・`Age`・`Date` の 1 回の走査、
//! 壊れた `Content-Length` と頼んでいない 101 の 502、再検証の 304 (保存済みの延命と配信)、
//! オリジン障害時の stale、HTTP/1.0 へ送れない `Transfer-Encoding`。

use std::sync::atomic::Ordering;

use super::fetch::Fetched;
use super::lookup::CacheState;
use super::respond::write_error;
use super::serve::{can_serve_stale, serve_cached};
use super::{ClientWriter, Exchange, Stage, done, finished};
use crate::body::Framing;
use crate::cache::now_epoch;
use crate::freshness;
use crate::headers;
use crate::log_trace;
use crate::metrics::{ErrCause, Failure};
use crate::{log_debug, log_warn};

/// オリジンがこのステータスを返したら stale を配信する (RFC 5861 stale-if-error 相当)。
const STALE_ON_STATUS: &[u16] = &[500, 502, 503, 504];

/// 応答の先頭から分かったこと (配信の段が使う)。
pub(super) struct HeadFacts {
    /// 応答の本文の枠組み
    pub framing: Framing,
    /// 素通しの応答に付け直す `Age` (T.29.4)
    pub age_secs: Option<u64>,
    /// `Date` の行があるか (無ければ自分で付ける。T.29.9)
    pub has_date: bool,
    /// オリジンとの接続を使い回せるか (HTTP/1.1・閉じるまでの枠組みでない・`Connection: close` でない・
    /// 本文の途中で返した応答でない)
    pub origin_reusable: bool,
    /// 最後が chunked でない `Transfer-Encoding` の並び (T.28.1・T.28.35)。閉じるまでを本文にして
    /// 保存せず、TE の行はクライアントへ伝える
    pub unframed_te: Option<String>,
}

/// 304 が保存済みの表現を指しているか (RFC 9111 §4.3.4 の「更新する保存済みの応答を選ぶ」の簡易版。T.29.6)。
///
/// 304 に `ETag` があれば保存済みの `ETag` と一致すること (弱い印 `W/` も含めて文字どおり)。
/// 無ければ `Last-Modified` が一致すること。どちらも無い 304 は選ばない (別の表現かもしれない)。
fn not_modified_selects(cached: &freshness::CachedHead, pairs_304: &[(String, String)]) -> bool {
    let value = |name: &str| {
        pairs_304
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim_ascii())
    };
    if let Some(etag) = value("etag") {
        return cached.etag.as_deref().map(str::trim_ascii) == Some(etag);
    }
    match value("last-modified") {
        Some(lm) => cached.last_modified.as_deref().map(str::trim_ascii) == Some(lm),
        None => false,
    }
}

/// 応答の先頭を見て、配信に要ることを決める。304 で保存済みを配る・stale を配る・502 にするときは
/// ここで終わる ([`done`])。
#[inline]
pub(super) fn examine_head(
    x: &mut Exchange<'_>,
    client: &mut ClientWriter<'_>,
    cs: &mut CacheState<'_>,
    fetched: &mut Fetched,
) -> Stage<HeadFacts> {
    let (status, request_body_bytes, body_cut) =
        (fetched.status, fetched.request_body_bytes, fetched.body_cut);
    let head: &[u8] = &fetched.head;
    let shared = x.shared;
    let cache = x.cache;
    let metrics = x.metrics;
    let conn_id = x.conn_id;
    let (http11, head_only, pool_key, url) = (x.http11, x.head_only, x.pool_key, x.url);
    let (key, now, cfg) = (cs.key, cs.now, cs.cfg);
    // 本文の途中でオリジンが返した応答 (T.29.7): クライアントの本文の残りは読んでいないので、
    // 応答に `Connection: close` を付けて閉じる (次の要求の頭に本文の続きが来てしまう)
    if body_cut {
        x.ctx.keep_client = false;
    }
    if request_body_bytes > 0 {
        log_debug!(
            Some(conn_id),
            "forwarded request body {}B",
            request_body_bytes
        );
    }
    // trace が切れていれば応答の先頭をもう 1 回なめない (T.28.12)
    if crate::log::enabled(crate::log::Level::Trace) {
        for (k, v) in headers::response_lines(head) {
            log_trace!(Some(conn_id), "res header  {}: {}", k, v);
        }
    }

    // 枠組みも `Connection: close` も生の先頭から直接見る (組を作らない)。
    // ついでに `Content-Length` を検査する (T19.5)。重複して不一致・数字でない・負・
    // u64 に収まらない桁数は、そのまま通すと「名乗った長さと実際に送るバイトが食い違う」
    // 応答になるので、オリジンの応答として扱わず 502 に差し替える (この接続は再利用しない)
    // 枠組みと `Connection: close` を 1 回の走査で (T.28.12)
    // `Age` もこの 1 回で拾う (T.29.4。下の `age_secs`)
    let scan = Framing::scan_response_head(status, head_only, head);
    let (framing, conn_close, scan_date) = (scan.framing, scan.close, scan.date);
    // 素通しの応答に付け直す `Age` (T.29.4)。`Age` は end-to-end (RFC 9110 §7.6・9111 §4.2.3) なのに、
    // 枠組みの名前と一緒に [`headers::write_response_head`] が無条件に落としていた (HIT にだけ付いた)。
    // 値は保存するときの `policy.age` と同じ `max(受けた Age, now − Date)`。**オリジンが `Age` を
    // 付けてきた応答だけ**に付ける (`Date` だけの応答に足すと、時計のずれがそのまま `Age` に出る)。
    // 読めない値は今までどおり落とす。`Date` を読むのは `Age` のある応答だけ
    // 304 (クライアントの条件付き要求の中継) と 101 は対象外
    let age_secs = scan
        .age
        .filter(|_| status >= 200 && status != 304)
        .and_then(|v| v.trim_ascii().parse::<u64>().ok())
        .map(|age| {
            let date = headers::response_value(head, "date").and_then(crate::httpdate::parse);
            age.max(date.map_or(0, |d| now_epoch().saturating_sub(d)))
        });
    let framing = match framing {
        Ok(framing) => framing,
        Err(bad) => {
            if let Some(more) = metrics.origin_warn(Failure::of(ErrCause::Other)) {
                log_warn!(
                    Some(conn_id),
                    "502 Bad Gateway: bad Content-Length {:?} from {}{}",
                    bad.0,
                    pool_key,
                    more
                );
            }
            // 失敗の数え方 (`/status` の `errors`・`errors_by_cause`) に乗せる。
            // 原因は既存の 8 つのうち「上のどれでもない」を使う (配列は伸ばさない)
            x.ctx.detail.cause = Some(ErrCause::Other);
            write_error(client, 502, "Bad Gateway")?;
            x.ctx.log_failure(502);
            return done(false);
        }
    };
    // 頼んでいない 101 (RFC 9110 §15.2.2: クライアントが `Upgrade` で挙げていないプロトコルへ切り替えては
    // ならない。T.29.9)。今まではそのまま返していた。この接続はもう HTTP ではないので使い回さない
    if status == 101 && !x.req.upgrade {
        log_warn!(
            Some(conn_id),
            "502 Bad Gateway: unsolicited 101 Switching Protocols from {}",
            pool_key
        );
        x.ctx.detail.cause = Some(ErrCause::Other);
        write_error(client, 502, "Bad Gateway")?;
        x.ctx.log_failure(502);
        return done(false);
    }
    let origin_reusable =
        head.starts_with(b"HTTP/1.1") && framing != Framing::Close && !conn_close && !body_cut;
    // 最後が chunked でない `Transfer-Encoding` の応答 (T.28.1)。閉じるまでを本文にして返すが、
    // 区切りがオリジンと食い違っているかもしれないので保存しない (接続は上で使い回さない)。
    // 見分けるのは閉じるまでの応答のときだけ (CL / chunked の熱い経路では先頭をなめ直さない)
    //
    // 符号化はほどかずに渡すので、TE の行もクライアントへ伝える (T.28.35。配信の段の `client_framing`)
    let unframed_te = (framing == Framing::Close)
        .then(|| Framing::response_transfer_encoding_line(head))
        .flatten();
    if unframed_te.is_some() {
        log_debug!(
            Some(conn_id),
            "response Transfer-Encoding does not end in chunked: reading until close, not caching"
        );
    }

    // クライアントの条件付き要求に対するオリジンの 304 (T.29.6): 304 はそのまま中継するが、
    // 304 の validator が保存済みの表現と一致すれば、保存済みの表現も延ばして先頭を書き直す
    // (RFC 9111 §4.3.4)。今までは延ばさなかったので、続く素の GET が古い先頭のまま
    // `REFRESHING` でオリジンへ行っていた。validator が違う 304 では触らない
    if status == 304
        && let Some(entry) = cs.client_cond_entry.take()
    {
        let cached_head = freshness::parse_cached_head(&entry.head);
        let pairs_304 = headers::response_pairs(head);
        if not_modified_selects(&cached_head, &pairs_304) {
            if freshness::not_modified_forbids_storing(&pairs_304) {
                cache.remove(key);
            } else {
                let p = freshness::revalidated_policy(&pairs_304, &cached_head, cfg, now);
                match headers::merge_not_modified(&entry.head, head) {
                    Some(new_head) => {
                        cache.refresh_with_head(key, url, &new_head, p.ttl, p.age, conn_id);
                    }
                    None => {
                        cache.refresh(key, p.ttl, p.age, conn_id);
                    }
                }
            }
            log_debug!(
                Some(conn_id),
                "client-conditional 304 from {}: refreshed the stored entry",
                pool_key
            );
        }
    }

    // 304: 保存済みの表現がまだ有効。延命して配信する
    if status == 304
        && let Some((entry, source)) = cs.stale.take()
    {
        let cached_head = freshness::parse_cached_head(&entry.head);
        let pairs_304 = headers::response_pairs(head);
        let mut entry = entry;
        // 304 が保存を禁じた (`no-store`・`private`・`Vary: Cookie` など): 延命せず項目を消し、
        // この要求にだけ 304 のヘッダーを載せた写しを配る (T.29.0)。次の要求はオリジンへ行く
        if freshness::not_modified_forbids_storing(&pairs_304) {
            cache.remove(key);
            if let Some(new_head) = headers::merge_not_modified(&entry.head, head) {
                entry.replace_head(new_head);
            }
            if let Some(served) = headers::with_set_cookies(&entry.head, head) {
                entry.replace_head(served);
            }
            entry.meta.stored_at = now_epoch();
            entry.meta.expires_at = entry.meta.stored_at;
            if origin_reusable && let Some(server) = fetched.server.take() {
                shared.upstream.pool.put(pool_key, server, shared.timeout);
            }
            return finished(serve_cached(
                client,
                entry,
                source,
                "REVALIDATED",
                0,
                &x.ctx,
            ));
        }
        let p = freshness::revalidated_policy(&pairs_304, &cached_head, cfg, now);
        // 304 のヘッダーで保存済みの先頭を書き直して置き直し、配るのも新しい先頭にする
        // (RFC 9111 §3.2・§4.3.4。T.28.2)。今までは期限だけ延ばし、配る先頭は古いままだった
        let expires_at = match headers::merge_not_modified(&entry.head, head) {
            Some(new_head) => {
                let expires_at =
                    cache.refresh_with_head(key, url, &new_head, p.ttl, p.age, conn_id);
                entry.replace_head(new_head);
                expires_at
            }
            None => cache.refresh(key, p.ttl, p.age, conn_id),
        };
        // 配る `Age` も置き直した時刻から数える (`refresh` と同じく `now - 304 の Age`。T.28.34)。
        // 今までは手元の写しの古い `stored_at` のままで、保存から N 秒たって再検証した直後の応答が
        // `Age: N` になっていた (次の HIT からは新しい値)
        entry.meta.stored_at = now_epoch().saturating_sub(p.age);
        entry.meta.expires_at = expires_at;
        // 304 の `Set-Cookie` は保存した先頭へは写さず (T.29.0)、再検証したこの要求にだけ渡す
        if let Some(served) = headers::with_set_cookies(&entry.head, head) {
            entry.replace_head(served);
        }
        if origin_reusable && let Some(server) = fetched.server.take() {
            shared.upstream.pool.put(pool_key, server, shared.timeout);
        }
        let ttl_left = p.ttl.as_secs().saturating_sub(p.age);
        return finished(serve_cached(
            client,
            entry,
            source,
            "REVALIDATED",
            ttl_left,
            &x.ctx,
        ));
    }
    // オリジン障害: stale を配信 (禁止されていなければ)。この接続は再利用しない
    if STALE_ON_STATUS.contains(&status)
        && !cs.force_revalidate
        && let Some((entry, source)) = cs.stale.take()
        && can_serve_stale(&entry)
    {
        log_debug!(
            Some(conn_id),
            "origin returned {}: serving stale entry",
            status
        );
        cache.stale_served.fetch_add(1, Ordering::Relaxed);
        return finished(serve_cached(client, entry, source, "STALE", 0, &x.ctx));
    }
    if cs.lookup_allowed {
        metrics.inc_cache_miss();
    }

    // 最後が chunked でない TE を HTTP/1.0 のクライアントへは送れない (RFC 9112 §6.1: TE を送ってよいのは
    // HTTP/1.1 以上を名乗った要求への応答だけ)。符号化をほどけないので、TE を落として符号化済みの
    // バイトを本文として渡す (T.28.35 より前の振る舞い) 代わりに 502 にする (この接続は再利用しない)
    if !http11 && let Some(te) = unframed_te.as_deref() {
        if let Some(more) = metrics.origin_warn(Failure::of(ErrCause::Other)) {
            log_warn!(
                Some(conn_id),
                "502 Bad Gateway: {:?} from {} cannot be relayed to an HTTP/1.0 client{}",
                te,
                pool_key,
                more
            );
        }
        x.ctx.detail.cause = Some(ErrCause::Other);
        write_error(client, 502, "Bad Gateway")?;
        x.ctx.log_failure(502);
        return done(false);
    }
    Ok(HeadFacts {
        framing,
        age_secs,
        has_date: scan_date,
        origin_reusable,
        unframed_te,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// 304 が保存済みの表現を指しているか (T.29.6): `ETag` があれば文字どおり一致、無ければ `Last-Modified`、
    /// どちらも無ければ選ばない。
    #[test]
    fn not_modified_selects_by_etag_then_last_modified() {
        let cached = freshness::parse_cached_head(
            b"HTTP/1.1 200 OK\r\nETag: \"e\"\r\nLast-Modified: Mon, 01 Jan 2024 00:00:00 GMT\r\n\r\n",
        );
        assert!(not_modified_selects(&cached, &pairs(&[("etag", "\"e\"")])));
        assert!(
            !not_modified_selects(&cached, &pairs(&[("etag", "W/\"e\"")])),
            "弱い印は文字どおり比べる"
        );
        assert!(!not_modified_selects(&cached, &pairs(&[("etag", "\"x\"")])));
        assert!(not_modified_selects(
            &cached,
            &pairs(&[("last-modified", "Mon, 01 Jan 2024 00:00:00 GMT")])
        ));
        assert!(!not_modified_selects(
            &cached,
            &pairs(&[("last-modified", "Tue, 02 Jan 2024 00:00:00 GMT")])
        ));
        assert!(!not_modified_selects(&cached, &pairs(&[])));
        // `ETag` があれば `Last-Modified` は見ない (違う ETag なら一致する LM があっても選ばない)
        assert!(!not_modified_selects(
            &cached,
            &pairs(&[
                ("etag", "\"x\""),
                ("last-modified", "Mon, 01 Jan 2024 00:00:00 GMT")
            ])
        ));
    }
}
