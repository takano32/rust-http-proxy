//! キャッシュ済みレスポンスの先頭 ([`CachedHead`]) と、それを使う条件付き要求の判断:
//! クライアントの `If-None-Match` / `If-Modified-Since` に 304 を返すか ([`client_not_modified`])、
//! 304 に載せるヘッダー ([`not_modified_headers`])、再検証の条件付きヘッダー ([`conditional_headers`])。

use super::directives::has_directive;
use crate::httpdate;

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
