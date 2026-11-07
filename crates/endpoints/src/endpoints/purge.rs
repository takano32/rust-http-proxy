//! キャッシュを引く・消す口: `/purge` と `PURGE` メソッド (全バリアントを消す)、`/lookup` (項目の状態)。

use super::respond::Reply;
use super::{Endpoint, param, parse_query};
use crate::cache::cache_key;
use crate::http::parse_origin;
use crate::metrics;

/// `/purge?url=<url>` か `/purge?all=1` (T14.18 の読み取り専用で断るのは `super::handle` の側)。
pub(super) fn purge(ep: &Endpoint<'_>, query: Option<&str>) -> Reply {
    let params = parse_query(query.unwrap_or(""));
    if params.iter().any(|(k, v)| k == "all" && v != "0") {
        let n = ep.cache.clear_all();
        return (
            200,
            "application/json",
            format!("{}\"purged\":{},\"all\":true}}", metrics::SCHEMA_HEAD, n),
        )
            .into();
    }
    match param(&params, "url") {
        Some(url) => purge_url(ep, url),
        None => Reply::json_error(400, "use /purge?url=<url> or /purge?all=1"),
    }
}

/// URL を正規化して全バリアントを消す (`/purge?url=` と `PURGE <url>` の両方がここに来る)。
pub(super) fn purge_url(ep: &Endpoint<'_>, url: &str) -> Reply {
    match parse_origin(url, None) {
        Ok(origin) => {
            let canonical = origin.url();
            let n = ep.cache.invalidate(&canonical, ep.conn_id);
            (
                200,
                "application/json",
                format!(
                    "{}\"purged\":{},\"url\":\"{}\"}}",
                    metrics::SCHEMA_HEAD,
                    n,
                    crate::json::escape(&canonical)
                ),
            )
                .into()
        }
        Err(e) => Reply::json_error(400, &crate::json::escape(&e.to_string())),
    }
}

/// `/lookup?url=<url>` — 引数が要る口は案内を返す (`/explain` と同じ方針)。
pub(super) fn lookup(ep: &Endpoint<'_>, query: Option<&str>) -> Reply {
    let params = parse_query(query.unwrap_or(""));
    match param(&params, "url") {
        Some(url) => lookup_url(ep, url),
        None => Reply::json_error(400, "use /lookup?url=<url>"),
    }
}

/// エントリの状態を返す (LRU には触らない)。バリアント無し (Accept-Encoding 無し) のキーを見る。
fn lookup_url(ep: &Endpoint<'_>, url: &str) -> Reply {
    let Ok(origin) = parse_origin(url, None) else {
        return Reply::json_error(400, "invalid url");
    };
    let canonical = origin.url();
    let key = cache_key("GET", &canonical);
    match ep.cache.peek(key) {
        Some(info) => {
            // 鮮度は注入された時計で (キャッシュ自身と同じ「いま」。T35.1)
            let now = ep.cache.now();
            (
                200,
                "application/json",
                format!(
                    "{}\"found\":true,\"url\":\"{}\",\"memory\":{},\"disk\":{},\"size\":{},\"stored_at\":{},\"expires_at\":{},\"fresh\":{},\"ttl_left\":{},\"validators\":{}}}",
                    metrics::SCHEMA_HEAD,
                    crate::json::escape(&canonical),
                    info.memory,
                    info.disk,
                    info.size,
                    info.meta.stored_at,
                    info.meta.expires_at,
                    info.meta.expires_at > now,
                    info.meta.expires_at.saturating_sub(now),
                    info.meta.validators
                ),
            )
                .into()
        }
        None => (
            404,
            "application/json",
            format!(
                "{}\"found\":false,\"url\":\"{}\"}}",
                metrics::SCHEMA_HEAD,
                crate::json::escape(&canonical)
            ),
        )
            .into(),
    }
}
