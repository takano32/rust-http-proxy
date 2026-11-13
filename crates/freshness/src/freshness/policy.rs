//! 保存可否と TTL の判断 ([`Policy`]): オリジンの応答 ([`response_policy_lines`]) と、
//! 再検証の 304 ([`revalidated_policy`] / [`not_modified_forbids_storing`])。

use std::time::Duration;

use super::cached_head::CachedHead;
use super::directives::{directive_value, has_directive, vary_is_supported};
use crate::cache::CacheConfig;
use crate::httpdate;

/// キャッシュ対象となりうるレスポンスステータス (RFC 9111 4.2.2 heuristically cacheable)。
pub const CACHEABLE_STATUS: &[u16] = &[200, 203, 204, 300, 301, 308, 404, 405, 410, 414, 501];

/// レスポンスを保存する方針。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// 新鮮とみなす時間。0 なら保存はするが毎回再検証する
    pub ttl: Duration,
    /// ETag / Last-Modified を持ち再検証できる
    pub validators: bool,
    /// stale のまま配信してはいけない (オリジン障害時も 5xx を返す)
    pub must_revalidate: bool,
    /// 受信時点で既に経過している時間 (`Age` ヘッダーと `Date` からの経過の大きい方)
    pub age: u64,
}

/// レスポンスを保存してよいか。`headers` の名前は小文字化されていること。
pub fn response_policy(
    status: u16,
    headers: &[(String, String)],
    cfg: &CacheConfig,
    now: u64,
) -> Option<Policy> {
    response_policy_lines(
        status,
        headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        false,
        cfg,
        now,
    )
}

/// [`response_policy`] を借用の「名前, 値」の並びで (名前の大小は問わない)。
///
/// forward の経路は応答の先頭を `response_lines` のまま渡す (小文字の `String` の組を
/// 要求ごとに作らない。T28.13)。保存しない応答 (`no-store` など) でも毎回通る判定なので、
/// 確保を減らした分がそのまま効く。
///
/// `request_cookie` は要求に `Cookie` があったか (T32.35)。真なら、応答に明示の鮮度 (`s-maxage`・
/// `max-age`・`Expires`) も `public` も無いとき保存しない。経験則と既定 TTL は「してよい」(RFC 9111 §4.2.2)
/// だが、共有のキャッシュでは Cookie ごとに違う本文 (`you are alice`) を他の利用者に配る形になるため
pub fn response_policy_lines<'a>(
    status: u16,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    request_cookie: bool,
    cfg: &CacheConfig,
    now: u64,
) -> Option<Policy> {
    if !CACHEABLE_STATUS.contains(&status) {
        return None;
    }
    let mut cache_control = String::new();
    let mut expires = None;
    let mut date = None;
    let mut last_modified = None;
    let mut etag = false;
    let mut age_header = 0u64;
    for (k, v) in headers {
        let is = |name: &str| k.eq_ignore_ascii_case(name);
        if is("set-cookie") {
            return None;
        } else if is("vary") {
            if !vary_is_supported(v) {
                return None;
            }
        } else if is("cache-control") || is("pragma") {
            if !cache_control.is_empty() {
                cache_control.push(',');
            }
            cache_control.push_str(v);
        } else if is("expires") {
            expires = Some(v);
        } else if is("date") {
            date = httpdate::parse(v);
        } else if is("last-modified") {
            last_modified = httpdate::parse(v);
        } else if is("etag") {
            etag = true;
        } else if is("age") {
            age_header = v.trim().parse().unwrap_or(0);
        }
    }
    cache_control.make_ascii_lowercase();
    // 上流のキャッシュを経てきた分の経過時間 (RFC 9111 §4.2.3 の簡易版)
    let age = age_header.max(date.map_or(0, |d| now.saturating_sub(d)));
    if has_directive(&cache_control, "no-store") || has_directive(&cache_control, "private") {
        return None;
    }
    let validators = etag || last_modified.is_some();
    let must_revalidate = has_directive(&cache_control, "must-revalidate")
        || has_directive(&cache_control, "proxy-revalidate");

    // 指示はあるのに値が読めない (`max-age=abc`・`max-age=-1`・値なし) ときは「既に古い」= 0
    // (RFC 9111 §4.2.1 は無効な鮮度情報を古いとみなすよう勧める。T28.10)。今までは
    // 無いのと同じに扱って Expires / 経験則 / 既定 TTL (300 秒) で新鮮にしていた
    let directive = |name: &str| {
        directive_value(&cache_control, name)
            .or_else(|| has_directive(&cache_control, name).then_some(0))
    };
    let explicit = directive("s-maxage")
        .or_else(|| directive("max-age"))
        .or_else(|| {
            expires.map(|e| {
                // 解析できない Expires (例: "0") は「既に古い」の意味
                httpdate::parse(e).map_or(0, |exp| exp.saturating_sub(date.unwrap_or(now)))
            })
        });
    if request_cookie && explicit.is_none() && !has_directive(&cache_control, "public") {
        return None;
    }
    let ttl = if has_directive(&cache_control, "no-cache") {
        0
    } else if let Some(t) = explicit {
        t
    } else {
        let implied = heuristic_ttl(last_modified, cfg, now).unwrap_or(cfg.default_ttl.as_secs());
        // 否定応答 (404 / 410 など) は明示が無ければ短く持つ (すぐ復活することが多い)
        if status >= 400 {
            implied.min(cfg.negative_ttl.as_secs())
        } else {
            implied
        }
    };
    if ttl == 0 && !validators {
        return None;
    }
    Some(Policy {
        ttl: Duration::from_secs(ttl),
        validators,
        must_revalidate,
        age,
    })
}

/// Last-Modified からの経験則 TTL (RFC 9111 4.2.2)。経過時間の `heuristic_percent` %。
pub(super) fn heuristic_ttl(
    last_modified: Option<u64>,
    cfg: &CacheConfig,
    now: u64,
) -> Option<u64> {
    let lm = last_modified?;
    if cfg.heuristic_percent == 0 {
        return None;
    }
    let age = now.saturating_sub(lm);
    let ttl = (age as u128 * cfg.heuristic_percent as u128 / 100) as u64;
    let floor = cfg.default_ttl.as_secs();
    let ceiling = cfg.heuristic_max.as_secs().max(floor);
    Some(ttl.clamp(floor, ceiling))
}

/// 304 を受けたときの新しい方針。304 側のヘッダーを優先し、無いものは保存済みの表現の
/// Cache-Control / Expires / Date / Last-Modified / ETag で補う (RFC 9111 §4.3.4)。
/// 保存済みが `no-cache` / `max-age=0` なら、304 が明示的に延ばさない限り TTL 0 のまま。
pub fn revalidated_policy(
    headers_304: &[(String, String)],
    cached: &CachedHead,
    cfg: &CacheConfig,
    now: u64,
) -> Policy {
    // 304 の `Set-Cookie` は保存する先頭へ写さない (T29.0) ので、保存の可否と TTL にも数えない
    // (数えると `response_policy` が `None` になり、延ばすはずの TTL が 0 になる)
    let mut hs: Vec<(String, String)> = headers_304
        .iter()
        .filter(|(k, _)| {
            !k.eq_ignore_ascii_case("set-cookie") && !k.eq_ignore_ascii_case("set-cookie2")
        })
        .cloned()
        .collect();
    let has = |hs: &[(String, String)], name: &str| hs.iter().any(|(k, _)| k == name);
    let fill = |hs: &mut Vec<(String, String)>, name: &str, value: Option<&String>| {
        if !has(hs, name)
            && let Some(v) = value
        {
            hs.push((name.to_string(), v.clone()));
        }
    };
    // 304 に Cache-Control / Expires のどちらも無ければ保存済みの鮮度指示を引き継ぐ
    if !has(&hs, "cache-control") && !has(&hs, "expires") {
        fill(&mut hs, "cache-control", cached.cache_control.as_ref());
        fill(&mut hs, "expires", cached.expires.as_ref());
        fill(&mut hs, "date", cached.date.as_ref());
    }
    fill(&mut hs, "last-modified", cached.last_modified.as_ref());
    fill(&mut hs, "etag", cached.etag.as_ref());
    response_policy(200, &hs, cfg, now).unwrap_or(Policy {
        ttl: Duration::ZERO,
        validators: cached.etag.is_some() || cached.last_modified.is_some(),
        must_revalidate: cached.must_revalidate,
        age: 0,
    })
}

/// 再検証の 304 が「この表現を保存するな」と言っているか (T29.0)。`Cache-Control` / `Pragma` の
/// `no-store`・`private`、対応できない `Vary` (`Vary: Cookie` など)。`Set-Cookie` は見ない
/// (保存する先頭へは写さないので、保存を止める理由にならない)。真なら延命せず項目を消す。
/// 0.28.48 までは写したまま `REVALIDATED` / `HIT` で配り続けていた (TTL が 0 になるだけ)。
pub fn not_modified_forbids_storing(headers_304: &[(String, String)]) -> bool {
    headers_304.iter().any(|(k, v)| {
        let is = |name: &str| k.eq_ignore_ascii_case(name);
        if is("vary") {
            !vary_is_supported(v)
        } else if is("cache-control") || is("pragma") {
            let v = v.to_ascii_lowercase();
            has_directive(&v, "no-store") || has_directive(&v, "private")
        } else {
            false
        }
    })
}
