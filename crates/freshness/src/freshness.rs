//! RFC 9111 に基づくキャッシュ可否・鮮度・再検証の判断。
//!
//! - 保存可否と TTL: `Cache-Control` (s-maxage / max-age) → `Expires` → `Last-Modified` からの
//!   経験則 (経過時間の一定割合) → 既定 TTL の順
//! - `no-cache` や TTL 0 でもバリデータ (ETag / Last-Modified) があれば「常に再検証」で保存する
//! - `Vary` は `Accept-Encoding` のみ対応 (キャッシュキーに正規化して含める)。それ以外は保存しない
//! - クライアントの条件付き要求 (`If-None-Match` / `If-Modified-Since`) はキャッシュ側で 304 を返す

use std::time::Duration;

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
        match k.as_str() {
            "set-cookie" => return None,
            "vary" if !vary_is_supported(v) => return None,
            "cache-control" | "pragma" => {
                if !cache_control.is_empty() {
                    cache_control.push(',');
                }
                cache_control.push_str(&v.to_ascii_lowercase());
            }
            "expires" => expires = Some(v.as_str()),
            "date" => date = httpdate::parse(v),
            "last-modified" => last_modified = httpdate::parse(v),
            "etag" => etag = true,
            "age" => age_header = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    // 上流のキャッシュを経てきた分の経過時間 (RFC 9111 §4.2.3 の簡易版)
    let age = age_header.max(date.map_or(0, |d| now.saturating_sub(d)));
    if has_directive(&cache_control, "no-store") || has_directive(&cache_control, "private") {
        return None;
    }
    let validators = etag || last_modified.is_some();
    let must_revalidate = has_directive(&cache_control, "must-revalidate")
        || has_directive(&cache_control, "proxy-revalidate");

    let explicit = directive_value(&cache_control, "s-maxage")
        .or_else(|| directive_value(&cache_control, "max-age"))
        .or_else(|| {
            expires.map(|e| {
                // 解析できない Expires (例: "0") は「既に古い」の意味
                httpdate::parse(e).map_or(0, |exp| exp.saturating_sub(date.unwrap_or(now)))
            })
        });
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
fn heuristic_ttl(last_modified: Option<u64>, cfg: &CacheConfig, now: u64) -> Option<u64> {
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
    let mut hs: Vec<(String, String)> = headers_304.to_vec();
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

/// `Vary` が対応できる範囲か (`Accept-Encoding` のみ)。
pub fn vary_is_supported(v: &str) -> bool {
    v.split(',')
        .map(str::trim)
        .all(|t| t.is_empty() || t.eq_ignore_ascii_case("accept-encoding"))
}

/// キャッシュキーに含める `Accept-Encoding` の正規形 (小文字・q=0 除外・ソート・重複除去)。
pub fn accept_encoding_variant(value: Option<&str>) -> String {
    let mut tokens: Vec<String> = value
        .unwrap_or("")
        .split(',')
        .filter_map(|t| {
            let mut parts = t.split(';');
            let name = parts.next()?.trim().to_ascii_lowercase();
            if name.is_empty() {
                return None;
            }
            let zero_q = parts.any(|p| {
                let p = p.trim();
                p.strip_prefix("q=")
                    .is_some_and(|q| q.trim().parse::<f64>().is_ok_and(|q| q <= 0.0))
            });
            (!zero_q).then_some(name)
        })
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens.join(",")
}

/// `cache_control` に指示 `name` があるか (値付き指示も含む)。
pub fn has_directive(cache_control: &str, name: &str) -> bool {
    cache_control.split(',').any(|part| {
        let part = part.trim();
        part == name
            || part
                .strip_prefix(name)
                .is_some_and(|r| r.trim_start().starts_with('='))
    })
}

pub fn directive_value(cache_control: &str, name: &str) -> Option<u64> {
    for part in cache_control.split(',') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name)
            && let Some(v) = rest.trim_start().strip_prefix('=')
        {
            return v.trim().trim_matches('"').parse::<u64>().ok();
        }
    }
    None
}

/// キャッシュ済みレスポンスの先頭 (ステータス行 + ヘッダー) から再検証に必要な情報。
#[derive(Debug, Clone, Default)]
pub struct CachedHead {
    pub status: u16,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub must_revalidate: bool,
    /// `no-cache` / `s-maxage` 付き: 再検証なしに stale を配信してはいけない (RFC 9111 §4.2.4)
    pub no_stale: bool,
    /// 元の `Cache-Control` (小文字)、`Expires`、`Date`
    pub cache_control: Option<String>,
    pub expires: Option<String>,
    pub date: Option<String>,
    /// (小文字の名前, 元の名前, 値)
    pub headers: Vec<(String, String, String)>,
}

impl CachedHead {
    /// stale のまま配信してよいか。
    pub fn may_serve_stale(&self) -> bool {
        !self.must_revalidate && !self.no_stale
    }
}

pub fn parse_cached_head(head: &[u8]) -> CachedHead {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split('\n');
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut out = CachedHead {
        status,
        ..CachedHead::default()
    };
    let mut cache_control = String::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let name = k.trim().to_string();
        let lower = name.to_ascii_lowercase();
        let value = v.trim().to_string();
        match lower.as_str() {
            "etag" => out.etag = Some(value.clone()),
            "last-modified" => out.last_modified = Some(value.clone()),
            "expires" => out.expires = Some(value.clone()),
            "date" => out.date = Some(value.clone()),
            "cache-control" => {
                if !cache_control.is_empty() {
                    cache_control.push(',');
                }
                cache_control.push_str(&value.to_ascii_lowercase());
            }
            _ => {}
        }
        out.headers.push((lower, name, value));
    }
    out.must_revalidate = has_directive(&cache_control, "must-revalidate")
        || has_directive(&cache_control, "proxy-revalidate");
    out.no_stale =
        has_directive(&cache_control, "no-cache") || has_directive(&cache_control, "s-maxage");
    if !cache_control.is_empty() {
        out.cache_control = Some(cache_control);
    }
    out
}

/// 304 応答に載せるヘッダー行 (RFC 9110 §15.4.5)。
pub fn not_modified_headers(head: &CachedHead) -> Vec<String> {
    const KEEP: [&str; 7] = [
        "cache-control",
        "content-location",
        "date",
        "etag",
        "expires",
        "vary",
        "last-modified",
    ];
    head.headers
        .iter()
        .filter(|(lower, _, _)| KEEP.contains(&lower.as_str()))
        .map(|(_, name, value)| format!("{}: {}", name, value))
        .collect()
}

/// クライアントの条件付き要求に対し、キャッシュ済みの表現が変わっていないか。
pub fn client_not_modified(
    head: &CachedHead,
    if_none_match: Option<&str>,
    if_modified_since: Option<&str>,
) -> bool {
    if let Some(inm) = if_none_match {
        return match &head.etag {
            Some(etag) => etag_list_matches(inm, etag),
            None => inm.trim() == "*",
        };
    }
    if let (Some(ims), Some(lm)) = (if_modified_since, &head.last_modified)
        && let (Some(since), Some(modified)) = (httpdate::parse(ims), httpdate::parse(lm))
    {
        return modified <= since;
    }
    false
}

/// `If-None-Match` のリストに `etag` が (弱い比較で) 含まれるか。
pub fn etag_list_matches(list: &str, etag: &str) -> bool {
    let weak = |s: &str| s.trim().trim_start_matches("W/").to_string();
    list.trim() == "*" || list.split(',').any(|t| weak(t) == weak(etag))
}

/// 再検証用の条件付きヘッダー行。
pub fn conditional_headers(head: &CachedHead) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(etag) = &head.etag {
        out.push(format!("If-None-Match: {}", etag));
    }
    if let Some(lm) = &head.last_modified {
        out.push(format!("If-Modified-Since: {}", lm));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CacheConfig {
        CacheConfig::disabled()
    }

    fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const NOW: u64 = 1_788_288_860;

    #[test]
    fn default_ttl_without_any_hint() {
        let p = response_policy(200, &hdrs(&[("content-type", "text/html")]), &cfg(), NOW).unwrap();
        assert_eq!(p.ttl, Duration::from_secs(300));
        assert!(!p.validators && !p.must_revalidate);
    }

    #[test]
    fn max_age_s_maxage_and_expires() {
        let c = cfg();
        let p = response_policy(
            200,
            &hdrs(&[("cache-control", "public, max-age=120")]),
            &c,
            NOW,
        );
        assert_eq!(p.unwrap().ttl, Duration::from_secs(120));
        let p = response_policy(
            200,
            &hdrs(&[("cache-control", "max-age=120, s-maxage=600")]),
            &c,
            NOW,
        );
        assert_eq!(p.unwrap().ttl, Duration::from_secs(600));
        let p = response_policy(
            200,
            &hdrs(&[
                ("date", "Mon, 01 Sep 2025 00:00:00 GMT"),
                ("expires", "Mon, 01 Sep 2025 01:00:00 GMT"),
            ]),
            &c,
            NOW,
        );
        assert_eq!(p.unwrap().ttl, Duration::from_secs(3600));
        // 解析できない Expires は「既に古い」→ バリデータが無ければ保存しない
        assert!(response_policy(200, &hdrs(&[("expires", "0")]), &c, NOW).is_none());
    }

    #[test]
    fn negative_responses_get_a_short_implicit_ttl() {
        let c = cfg();
        let p = response_policy(404, &hdrs(&[]), &c, NOW).unwrap();
        assert_eq!(p.ttl, c.negative_ttl, "default TTL is capped for 404");
        let p = response_policy(410, &hdrs(&[("cache-control", "max-age=3600")]), &c, NOW).unwrap();
        assert_eq!(p.ttl.as_secs(), 3600, "explicit max-age wins");
        let p = response_policy(200, &hdrs(&[]), &c, NOW).unwrap();
        assert_eq!(p.ttl, c.default_ttl, "success keeps the default TTL");
    }

    #[test]
    fn heuristic_from_last_modified_is_clamped() {
        let c = cfg();
        // 100 日前に更新 → 10% = 10 日 だが上限 7 日
        let lm = NOW - 100 * 86_400;
        let (y, mo, d, h, mi, s) = crate::log::civil_from_epoch(lm);
        let lm_str = format!(
            "Mon, {:02} {} {} {:02}:{:02}:{:02} GMT",
            d,
            [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"
            ][mo as usize - 1],
            y,
            h,
            mi,
            s
        );
        let p = response_policy(200, &hdrs(&[("last-modified", &lm_str)]), &c, NOW).unwrap();
        assert_eq!(p.ttl, Duration::from_secs(7 * 86_400));
        assert!(p.validators);
        // 直近の更新なら既定 TTL が下限
        let p = response_policy(
            200,
            &hdrs(&[("last-modified", "Wed, 01 Jan 2025 00:00:00 GMT")]),
            &c,
            1_735_689_600 + 60,
        )
        .unwrap();
        assert_eq!(p.ttl, Duration::from_secs(300));
    }

    #[test]
    fn not_cacheable_cases() {
        let c = cfg();
        let n = |h: &[(&str, &str)]| response_policy(200, &hdrs(h), &c, NOW);
        assert!(n(&[("cache-control", "no-store")]).is_none());
        assert!(n(&[("cache-control", "private")]).is_none());
        assert!(n(&[("cache-control", "max-age=0")]).is_none());
        assert!(n(&[("cache-control", "no-cache")]).is_none());
        assert!(n(&[("set-cookie", "a=b")]).is_none());
        assert!(n(&[("vary", "*")]).is_none());
        assert!(n(&[("vary", "Cookie")]).is_none());
        assert!(n(&[("vary", "Accept-Encoding")]).is_some());
        assert!(response_policy(500, &hdrs(&[]), &c, NOW).is_none());
        assert!(response_policy(302, &hdrs(&[]), &c, NOW).is_none());
    }

    #[test]
    fn zero_ttl_with_validators_is_stored_for_revalidation() {
        let c = cfg();
        let p = response_policy(
            200,
            &hdrs(&[
                ("cache-control", "no-cache, must-revalidate"),
                ("etag", "\"abc\""),
            ]),
            &c,
            NOW,
        )
        .unwrap();
        assert_eq!(p.ttl, Duration::ZERO);
        assert!(p.validators && p.must_revalidate);
        let p = response_policy(
            200,
            &hdrs(&[("cache-control", "max-age=0"), ("etag", "x")]),
            &c,
            NOW,
        );
        assert_eq!(p.unwrap().ttl, Duration::ZERO);
    }

    #[test]
    fn accept_encoding_is_normalized() {
        assert_eq!(accept_encoding_variant(None), "");
        assert_eq!(
            accept_encoding_variant(Some("gzip, deflate, br")),
            "br,deflate,gzip"
        );
        assert_eq!(
            accept_encoding_variant(Some("BR, gzip;q=0.5, identity;q=0")),
            "br,gzip"
        );
        assert_eq!(accept_encoding_variant(Some("gzip, gzip")), "gzip");
    }

    #[test]
    fn cached_head_and_conditionals() {
        let head = b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\nCache-Control: max-age=60, must-revalidate\r\nContent-Length: 2\r\n\r\n";
        let h = parse_cached_head(head);
        assert_eq!(h.status, 200);
        assert_eq!(h.etag.as_deref(), Some("\"v1\""));
        assert!(h.must_revalidate);
        assert!(client_not_modified(&h, Some("\"v1\""), None));
        assert!(client_not_modified(&h, Some("W/\"v1\", \"other\""), None));
        assert!(client_not_modified(&h, Some("*"), None));
        assert!(!client_not_modified(&h, Some("\"v2\""), None));
        assert!(client_not_modified(
            &h,
            None,
            Some("Mon, 07 Nov 1994 00:00:00 GMT")
        ));
        assert!(!client_not_modified(
            &h,
            None,
            Some("Sat, 05 Nov 1994 00:00:00 GMT")
        ));
        // If-None-Match があれば If-Modified-Since は無視
        assert!(!client_not_modified(
            &h,
            Some("\"v2\""),
            Some("Mon, 07 Nov 1994 00:00:00 GMT")
        ));
        let cond = conditional_headers(&h);
        assert_eq!(cond[0], "If-None-Match: \"v1\"");
        assert!(cond[1].starts_with("If-Modified-Since: Sun"));
        let nm = not_modified_headers(&h);
        assert!(nm.iter().any(|l| l.starts_with("ETag:")));
        assert!(!nm.iter().any(|l| l.starts_with("Content-Length")));
    }

    #[test]
    fn revalidated_policy_prefers_304_headers_then_stored_directives() {
        let c = cfg();
        let head = parse_cached_head(
            b"HTTP/1.1 200 OK\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\n\r\n",
        );
        assert_eq!(
            revalidated_policy(&hdrs(&[("cache-control", "max-age=42")]), &head, &c, NOW).ttl,
            Duration::from_secs(42)
        );
        // 304 にヒント無し → 古い Last-Modified からの経験則 (上限 7 日)
        assert_eq!(
            revalidated_policy(&hdrs(&[]), &head, &c, NOW).ttl,
            Duration::from_secs(7 * 86_400)
        );
        let plain = parse_cached_head(b"HTTP/1.1 200 OK\r\nETag: \"e\"\r\n\r\n");
        assert_eq!(
            revalidated_policy(&hdrs(&[]), &plain, &c, NOW).ttl,
            Duration::from_secs(300)
        );
        // 保存済みが no-cache なら、304 が何も言わない限り TTL 0 のまま
        let nc =
            parse_cached_head(b"HTTP/1.1 200 OK\r\nETag: \"e\"\r\nCache-Control: no-cache\r\n\r\n");
        assert_eq!(
            revalidated_policy(&hdrs(&[]), &nc, &c, NOW).ttl,
            Duration::ZERO
        );
        assert!(!nc.may_serve_stale());
        assert_eq!(
            revalidated_policy(&hdrs(&[("cache-control", "max-age=10")]), &nc, &c, NOW).ttl,
            Duration::from_secs(10)
        );
    }

    #[test]
    fn age_and_date_reduce_remaining_freshness() {
        let c = cfg();
        let p = response_policy(
            200,
            &hdrs(&[("cache-control", "max-age=60"), ("age", "55")]),
            &c,
            NOW,
        )
        .unwrap();
        assert_eq!(p.ttl, Duration::from_secs(60));
        assert_eq!(p.age, 55);
        let (y, mo, d, h, mi, s) = crate::log::civil_from_epoch(NOW - 600);
        let date = format!(
            "Mon, {:02} {} {} {:02}:{:02}:{:02} GMT",
            d,
            [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"
            ][mo as usize - 1],
            y,
            h,
            mi,
            s
        );
        let p = response_policy(
            200,
            &hdrs(&[("cache-control", "max-age=900"), ("date", &date)]),
            &c,
            NOW,
        )
        .unwrap();
        assert_eq!(p.age, 600);
        let sm = parse_cached_head(
            b"HTTP/1.1 200 OK\r\nCache-Control: s-maxage=60\r\nETag: \"x\"\r\n\r\n",
        );
        assert!(!sm.may_serve_stale(), "s-maxage forbids stale");
        // 素の LF で終わるヘッダーも解析できる
        let lf = parse_cached_head(b"HTTP/1.1 200 OK\nETag: \"lf\"\n\n");
        assert_eq!(lf.etag.as_deref(), Some("\"lf\""));
    }
}
