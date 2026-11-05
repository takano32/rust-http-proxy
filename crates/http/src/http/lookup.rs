//! キャッシュ参照の段: 鍵と鮮度の判定、新鮮なら配信 (HIT / REFRESHING)、期限切れなら
//! 再検証に備えて保持、`only-if-cached`、入場制御と同時ミスの合流。

use super::respond::write_error;
use super::serve::serve_cached;
use super::{ClientWriter, Exchange, Stage, done, finished, refresh};
use crate::body;
use crate::cache::inflight::LeaderGuard;
use crate::cache::{
    CacheConfig, CacheKey, CacheSource, CachedResponse, FetchOutcome, FetchTicket,
    cache_key_variant, now_epoch,
};
use crate::clientio::ClientReader;
use crate::freshness;
use crate::log_debug;

/// キャッシュ参照の段が決めたこと。オリジンへ行く段 (fetch) と配信の段 (deliver) が使う。
pub(super) struct CacheState<'c> {
    pub key: CacheKey,
    /// 鍵を作ったときの epoch 秒 (キャッシュ無効時は 0)
    pub now: u64,
    pub cfg: &'c CacheConfig,
    pub cache_on: bool,
    /// キャッシュを参照してよい要求 (GET / HEAD、認証なし、`no-store` なし、本文なし)
    pub lookup_allowed: bool,
    /// 応答を保存してよい要求 (参照できる GET で `Range` が無く、入場制御も通った)
    pub store_allowed: bool,
    /// クライアントが再検証を求めた (`no-cache` / `max-age=0` / `max-age=N` / `min-fresh=N`)
    pub force_revalidate: bool,
    /// 期限切れの保存済みの表現 (オリジンの 304 で延命、障害なら stale として配る)
    pub stale: Option<(CachedResponse, CacheSource)>,
    /// クライアント自身の条件付き要求のときの、期限切れの保存済みの表現 (T29.6)
    pub client_cond_entry: Option<CachedResponse>,
    /// 同時ミスの合流で先頭になった印 (保存の成否を待っている要求へ知らせる)
    pub leader: Option<LeaderGuard<'c>>,
}

impl<'c> CacheState<'c> {
    /// 何も決めていない状態 (キャッシュ無効と同じ形)。[`consult`] が欄を埋める出力欄として使う。
    #[inline]
    pub(super) fn new(cfg: &'c CacheConfig) -> CacheState<'c> {
        CacheState {
            key: CacheKey(0),
            now: 0,
            cfg,
            cache_on: false,
            lookup_allowed: false,
            store_allowed: false,
            force_revalidate: false,
            stale: None,
            client_cond_entry: None,
            leader: None,
        }
    }
}

/// 要求の `max-age=N` / `min-fresh=N` を、保存済みの表現がそのままでは満たさないか (T28.10 (c))。
///
/// `max-age`: 保存からの経過 (`now - stored_at`) が N 秒を越える。`min-fresh`: 期限までが N 秒に足りない。
/// 値の読めない指示は見ない (今までどおり)。`max-age=0` は呼ぶ側が先に `no-cache` と同じに扱っている。
fn client_wants_fresher(cache_control: &str, entry: &CachedResponse, now: u64) -> bool {
    let age = now.saturating_sub(entry.meta.stored_at);
    freshness::directive_value(cache_control, "max-age").is_some_and(|n| age > n)
        || freshness::directive_value(cache_control, "min-fresh")
            .is_some_and(|n| entry.ttl_left(now) < n)
}

/// キャッシュを参照する。保存から返せたらそこで終わり ([`done`])、そうでなければオリジンへ行くための状態を
/// `cs` ([`CacheState::new`] で作った出力欄) に書く (大きな構造体を戻り値で運ぶ写しを避ける。T34.8)。
#[inline]
pub(super) fn consult<'c>(
    x: &mut Exchange<'c>,
    client: &mut ClientWriter<'_>,
    reader: &mut ClientReader<'_>,
    cs: &mut CacheState<'c>,
) -> Stage<()> {
    let cache = x.cache;
    let req = x.ctx.req;
    let conn_id = x.ctx.conn_id;
    let req_framing = x.req_framing;
    let cfg = cs.cfg;
    // キャッシュ無効時は鍵の生成も鮮度判定もしない (素通しの経路を最短にする)
    let cache_on = cache.enabled();
    let (key, client_no_store, mut force_revalidate, only_if_cached, now) = if cache_on {
        let variant = freshness::accept_encoding_variant(req.accept_encoding.as_deref());
        (
            cache_key_variant("GET", x.ctx.url, &variant),
            freshness::has_directive(&req.cache_control, "no-store"),
            freshness::has_directive(&req.cache_control, "no-cache")
                || freshness::directive_value(&req.cache_control, "max-age") == Some(0),
            // `only-if-cached` (RFC 9111 §5.2.1.7。T28.10 (c)): 保存から返せなければ 504
            freshness::has_directive(&req.cache_control, "only-if-cached"),
            now_epoch(),
        )
    } else {
        (CacheKey(0), false, false, false, 0)
    };
    // 本文付きの GET は本文でも意味が変わりうるのでキャッシュしない
    let lookup_allowed = cache_on
        && (x.is_get || x.ctx.head_only)
        && !req.authorization
        && !client_no_store
        && req_framing == body::Framing::None;
    let store_allowed = lookup_allowed && x.is_get && req.range.is_none();
    let client_conditional = req.if_none_match.is_some() || req.if_modified_since.is_some();

    // 期限切れの保存済みの表現 (`cs.stale`) と、クライアント自身の条件付き要求のときの期限切れの
    // 保存済みの表現 (`cs.client_cond_entry`。T29.6) は出力欄へ直に書く
    if lookup_allowed {
        if let Some((entry, source)) = cache.get(key, conn_id) {
            cache.remember_variant(x.ctx.url, key);
            // 要求の `max-age=N` (N > 0) と `min-fresh=N` (RFC 9111 §5.2.1.1・§5.2.1.3。T28.10 (c)):
            // 保存から N 秒より古い・期限まで N 秒ない表現は、そのままでは返さず再検証する
            // (`max-age=0` と同じ扱い)。今までは見ずに HIT にしていた。`Cache-Control` の無い要求は
            // 空文字の判定 1 回だけ
            if !force_revalidate
                && !req.cache_control.is_empty()
                && client_wants_fresher(&req.cache_control, &entry, now)
            {
                force_revalidate = true;
            }
            if entry.is_fresh(now) && !force_revalidate {
                body::drain(reader, req_framing)?;
                let ttl_left = entry.ttl_left(now);
                return finished(serve_cached(client, entry, source, "HIT", ttl_left, &x.ctx));
            }
            // クライアント自身の条件付き要求は、そのままオリジンに判断させる
            if !client_conditional {
                // 期限切れ直後 (grace 内) なら、すぐ返して裏で再検証する (stale-while-revalidate)
                if !force_revalidate && try_refreshing(x, key, now, &entry, cfg.grace.as_secs()) {
                    body::drain(reader, req_framing)?;
                    cache.note_stale_served();
                    return finished(serve_cached(client, entry, source, "REFRESHING", 0, &x.ctx));
                }
                cs.stale = Some((entry, source));
            } else if entry.meta.validators {
                // オリジンの 304 で保存済みの表現も延ばせるように持っておく (T29.6)。
                // `stale` には入れない (条件はクライアントのものをそのまま送り、stale も配らない)
                cs.client_cond_entry = Some(entry);
            }
        }
    } else if cache_on {
        log_debug!(
            Some(conn_id),
            "cache BYPASS (method={} auth={} cc='{}')",
            x.ctx.method,
            req.authorization,
            req.cache_control
        );
    }

    // `only-if-cached` で保存から返せなかった (無い・古い・保存できない要求): オリジンへは行かずに 504
    // (RFC 9111 §5.2.1.7。T28.10 (c))。今まではオリジンへ行っていた
    if only_if_cached {
        body::drain(reader, req_framing)?;
        log_debug!(
            Some(conn_id),
            "only-if-cached and no usable stored response: 504"
        );
        write_error(client, 504, "Gateway Timeout")?;
        x.ctx.log(504, 0, "MISS");
        return done(false);
    }

    // 保存されないと分かっている鍵 (T11.9) は 1 回だけ見て、入場制御と合流の両方に使う (T28.13 (c))。
    // その鍵は前に見ているので、入場制御に通しても「2 回目」で通る (門番の表を引かずに済ませる)
    let known_unstored = store_allowed && cs.stale.is_none() && !cache.may_coalesce(key, now);

    // ---- 入場制御: 層が埋まっているとき、初めて見た URL は保存しない (2 回目から) ----
    let store_allowed = store_allowed && (cs.stale.is_some() || known_unstored || cache.admit(key));

    // ---- 同時ミスの合流: 同じキーを誰かが取得中なら、その保存完了を待ってキャッシュから返す ----
    // **保存されないと分かっている URL では合流しない** (T11.9)。待っても保存されないので、
    // 起きてから結局自分でオリジンへ行くことになり、待ち損 (`futex` 1.76 回/要求) だけが残る。
    // 初めて見る URL では今までどおり合流する
    if store_allowed && cs.stale.is_none() && !known_unstored {
        coalesce(x, client, reader, cs, key)?;
    }
    cs.key = key;
    cs.now = now;
    cs.cache_on = cache_on;
    cs.lookup_allowed = lookup_allowed;
    cs.store_allowed = store_allowed;
    cs.force_revalidate = force_revalidate;
    Ok(())
}

/// 期限切れの表現を返して裏で再検証してよい猶予 (秒。0 なら返さない)。
///
/// 既定の猶予 (`default_grace`、`PROXY_CACHE_GRACE_SECS`) は新鮮だった期間のある表現だけに使い、
/// max-age=0 (毎回再検証) の表現はオリジンが `stale-while-revalidate` を明示したときだけ対象にする。
/// 両方あれば大きい方 (T34.20 で `consult` から出した。純粋な計算)。
#[inline]
fn grace_for(entry: &CachedResponse, cache_control: Option<&str>, default_grace: u64) -> u64 {
    let default_grace = if entry.meta.expires_at > entry.meta.stored_at {
        default_grace
    } else {
        0
    };
    default_grace.max(
        cache_control
            .and_then(|cc| freshness::directive_value(cc, "stale-while-revalidate"))
            .unwrap_or(0),
    )
}

/// stale-while-revalidate: 期限切れの表現 (`entry`) が猶予の内で、validator があり、配ってよい表現なら、
/// 裏の再検証を起こす。起こせたら真 (呼ぶ側がこの表現を `REFRESHING` で返す)。`default_grace` は
/// 既定の猶予 (秒。[`grace_for`])。
fn try_refreshing(
    x: &Exchange<'_>,
    key: CacheKey,
    now: u64,
    entry: &CachedResponse,
    default_grace: u64,
) -> bool {
    let head = freshness::parse_cached_head(&entry.head);
    let grace = grace_for(entry, head.cache_control.as_deref(), default_grace);
    let stale_for = now.saturating_sub(entry.meta.expires_at);
    grace > 0
        && entry.meta.validators
        && head.may_serve_stale()
        && stale_for <= grace
        && refresh::spawn(
            x.shared,
            &x.origin,
            key,
            x.ctx.url,
            entry.head.clone(),
            x.ctx.req.accept_encoding.clone(),
        )
}

/// 同時ミスの合流: 先頭になれたら印 (`cs.leader`) を持ってオリジンへ行き、誰かが取得中なら保存の
/// 完了を待ってキャッシュから返す (`COALESCED`)。保存されなかった・間に合わなかったら自分で取りに行く。
#[inline]
fn coalesce<'c>(
    x: &mut Exchange<'c>,
    client: &mut ClientWriter<'_>,
    reader: &mut ClientReader<'_>,
    cs: &mut CacheState<'c>,
    key: CacheKey,
) -> Stage<()> {
    let (cache, conn_id, req_framing) = (x.cache, x.ctx.conn_id, x.req_framing);
    match cache.begin_fetch(key) {
        FetchTicket::Leader(guard) => cs.leader = Some(guard),
        FetchTicket::Follower(inflight) => {
            log_debug!(
                Some(conn_id),
                "cache WAIT: another request is fetching key={}",
                key
            );
            // `shared.timeout` が 0 (無期限) のときはここだけ「待たない」意味に
            // なるが、待たなければ自分で取りに行くだけなので安全側。
            // 先頭のスレッドを無期限に待つ形にはしない (相手が固まると連鎖する)
            if inflight.wait(x.shared.timeout) == Some(FetchOutcome::Stored)
                && let Some((entry, source)) = cache.get(key, conn_id)
                && entry.is_fresh(now_epoch())
            {
                body::drain(reader, req_framing)?;
                cache.note_coalesced();
                let ttl_left = entry.ttl_left(now_epoch());
                return finished(serve_cached(
                    client,
                    entry,
                    source,
                    "COALESCED",
                    ttl_left,
                    &x.ctx,
                ));
            }
            // 保存されなかった・間に合わなかった: 自分で取りに行く
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Body, Meta};
    use std::sync::Arc;

    fn entry(stored_at: u64, expires_at: u64) -> CachedResponse {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        CachedResponse {
            head: wire[..wire.len() - 2].to_vec(),
            size: wire.len() as u64,
            body: Body::Memory {
                data: Arc::new(wire.to_vec()),
                offset: wire.len() - 2,
            },
            meta: Meta {
                stored_at,
                expires_at,
                validators: false,
            },
        }
    }

    /// 要求の `max-age=N` は保存からの経過で、`min-fresh=N` は期限までの残りで見る (T28.10 (c))。
    #[test]
    fn max_age_and_min_fresh_of_the_request() {
        // 保存から 10 秒、期限まで 50 秒
        let e = entry(1_000, 1_060);
        let now = 1_010;
        assert!(!client_wants_fresher("max-age=10", &e, now));
        assert!(client_wants_fresher("max-age=9", &e, now));
        assert!(!client_wants_fresher("min-fresh=50", &e, now));
        assert!(client_wants_fresher("min-fresh=51", &e, now));
        assert!(
            !client_wants_fresher("no-cache", &e, now),
            "値の無い指示は見ない"
        );
        assert!(
            !client_wants_fresher("max-age=x", &e, now),
            "読めない値は見ない"
        );
    }

    /// 既定の猶予は新鮮だった期間のある表現だけ、`stale-while-revalidate` は明示されれば max-age=0 の
    /// 表現にも効き、両方あれば大きい方 (T34.20)。
    #[test]
    fn grace_for_takes_the_default_only_for_entries_that_were_fresh() {
        let fresh_once = entry(1_000, 1_060);
        let always_stale = entry(1_000, 1_000);
        assert_eq!(grace_for(&fresh_once, None, 30), 30);
        assert_eq!(
            grace_for(&always_stale, None, 30),
            0,
            "max-age=0 には既定を使わない"
        );
        assert_eq!(
            grace_for(
                &always_stale,
                Some("max-age=0, stale-while-revalidate=5"),
                30
            ),
            5
        );
        assert_eq!(
            grace_for(&fresh_once, Some("stale-while-revalidate=90"), 30),
            90
        );
        assert_eq!(
            grace_for(&fresh_once, Some("stale-while-revalidate=10"), 30),
            30
        );
        assert_eq!(
            grace_for(&fresh_once, Some("stale-while-revalidate=x"), 30),
            30,
            "読めない値は見ない"
        );
        assert_eq!(grace_for(&fresh_once, Some("max-age=60"), 0), 0);
    }
}
