//! プロキシ自身のエンドポイント: `/dashboard` (コントロールパネル)、`/healthz` `/status`
//! `/history` (JSON、`res=5|60|3600`)、`/metrics` (Prometheus)、`/proxy.pac` (ブラウザの自動設定)、
//! `/purge` と `PURGE` メソッド、`/lookup`、`/blocklist` (判定と手動の上書き)。
//!
//! これらのパスはオリジン形式の要求 (`GET /status` + `Host:`) より優先する。ブラウザがこの
//! プロキシ自身を経由して `http://host:PORT/status` のように絶対形式で要求してきた場合も、
//! ポートが自分の待ち受けポートなら自分宛てとみなす (自分へ転送してループしない)。
//! 応答は常に `Connection: close`。認証は無いので、到達できる人は誰でも purge できる
//! (公開ポートなら ACL や到達制御で守ること)。

use std::io::{self, Write};
use std::net::TcpStream;

use crate::cache::{Cache, cache_key};
use crate::http::parse_origin;
use crate::log_info;
use crate::metrics::Metrics;
use crate::prom;

pub struct Endpoint<'a> {
    pub metrics: &'a Metrics,
    pub cache: &'a Cache,
    pub conn_id: usize,
    /// 自分の待ち受けポート (絶対形式の自分宛て判定に使う)
    pub port: u16,
    /// 要求の `Host` ヘッダー (`/proxy.pac` が自分の名前を知るため)
    pub host: Option<&'a str>,
    /// `/proxy.pac` で DIRECT にするホストのパターン
    pub pac_direct: &'a [String],
}

const DASHBOARD_HTML: &str = include_str!("web/dashboard.html");

/// 要求ターゲットを自分宛てのパスに直す。オリジン形式はそのまま、絶対形式は自分のポート宛て
/// のときだけパスに落とす。それ以外 (他所への転送) は `None`。
fn local_path(target: &str, port: u16) -> Option<&str> {
    if target.starts_with('/') {
        return Some(target);
    }
    let rest = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("HTTP://"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let authority_port = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => p.parse::<u16>().ok()?,
        _ => 80,
    };
    (authority_port == port).then_some(path)
}

/// 内部エンドポイントなら応答して `Ok(true)` を返す。そうでなければ何もせず `Ok(false)`。
pub fn handle(
    client: &mut TcpStream,
    method: &str,
    target: &str,
    ep: &Endpoint<'_>,
) -> io::Result<bool> {
    let is_purge = method.eq_ignore_ascii_case("PURGE");
    let local = if is_purge {
        Some(target)
    } else {
        local_path(target, ep.port)
    };
    let Some(local) = local else {
        return Ok(false);
    };
    let self_addressed = !target.starts_with('/');
    let (path, query) = match local.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (local, None),
    };
    let is_get = method.eq_ignore_ascii_case("GET");
    let (status, content_type, body) = if is_purge {
        purge_url(ep, target)
    } else if is_get && (path == "/dashboard" || path == "/dashboard/") {
        (200, "text/html; charset=utf-8", DASHBOARD_HTML.to_string())
    } else if is_get && path == "/proxy.pac" {
        (
            200,
            "application/x-ns-proxy-autoconfig",
            pac(ep, target, self_addressed),
        )
    } else if is_get && path == "/history" {
        let params = parse_query(query.unwrap_or(""));
        let res = params
            .iter()
            .find(|(k, _)| k == "res")
            .and_then(|(_, v)| v.parse::<u64>().ok())
            .map(crate::history::History::index_for)
            .unwrap_or(0);
        (200, "application/json", ep.metrics.history.to_json_res(res))
    } else if is_get && path == "/blocklist" {
        blocklist_op(&parse_query(query.unwrap_or("")))
    } else if is_get && (path == "/healthz" || path == "/status") {
        (
            200,
            "application/json",
            ep.metrics.to_json_with_cache(Some(ep.cache)),
        )
    } else if is_get && path == "/metrics" {
        (
            200,
            "text/plain; version=0.0.4; charset=utf-8",
            prom::render(ep.metrics, Some(ep.cache)),
        )
    } else if is_get && path == "/purge" {
        let params = parse_query(query.unwrap_or(""));
        if params.iter().any(|(k, v)| k == "all" && v != "0") {
            let n = ep.cache.clear_all();
            (
                200,
                "application/json",
                format!("{{\"purged\":{},\"all\":true}}", n),
            )
        } else if let Some((_, url)) = params.iter().find(|(k, _)| k == "url") {
            purge_url(ep, url)
        } else {
            (
                400,
                "application/json",
                "{\"error\":\"use /purge?url=<url> or /purge?all=1\"}".to_string(),
            )
        }
    } else if is_get && path == "/lookup" {
        let params = parse_query(query.unwrap_or(""));
        match params.iter().find(|(k, _)| k == "url") {
            Some((_, url)) => lookup(ep, url),
            None => (
                400,
                "application/json",
                "{\"error\":\"use /lookup?url=<url>\"}".to_string(),
            ),
        }
    } else if self_addressed {
        // 自分宛てだが知らないパス: 自分へ転送するとループするので 404
        (
            404,
            "text/plain; charset=utf-8",
            "not found. endpoints: /dashboard /status /history?res=5|60|3600 /metrics /proxy.pac /lookup?url= /purge?url=|all=1 /blocklist?host=&action=block|allow|clear\n"
                .to_string(),
        )
    } else {
        return Ok(false);
    };
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        content_type,
        body.len(),
        body
    );
    client.write_all(response.as_bytes())?;
    client.flush()?;
    log_info!(
        Some(ep.conn_id),
        "{} {} -> {} (internal endpoint)",
        method,
        target,
        status
    );
    Ok(true)
}

/// `/blocklist?host=<h>` で判定、`&action=block|allow|clear[&ttl_secs=N]` で手動の上書き。
/// 引数なしなら一覧の状態と上書きの一覧。
fn blocklist_op(params: &[(String, String)]) -> (u16, &'static str, String) {
    use crate::blocklist;
    let get = |k: &str| params.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
    let Some(host) = get("host").map(str::trim).filter(|h| !h.is_empty()) else {
        return (200, "application/json", blocklist::status_json());
    };
    let host = crate::net::split_host_port(host).0.to_ascii_lowercase();
    if host.is_empty() || !host.contains('.') {
        return (
            400,
            "application/json",
            "{\"error\":\"host must be a domain name\"}".to_string(),
        );
    }
    let ttl = get("ttl_secs")
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(86400));
    let action = get("action").unwrap_or("");
    let changed = match action {
        "block" => Some(blocklist::set_override(&host, true, ttl).json()),
        "allow" => Some(blocklist::set_override(&host, false, ttl).json()),
        "clear" => Some(blocklist::clear_override(&host).to_string()),
        "" => None,
        _ => {
            return (
                400,
                "application/json",
                "{\"error\":\"action must be block, allow or clear\"}".to_string(),
            );
        }
    };
    let v = blocklist::check(&host);
    (
        200,
        "application/json",
        format!(
            "{{\"host\":\"{}\",\"blocked\":{},\"verdict\":\"{}\",\"action\":{},\"overrides\":[{}]}}",
            json_escape(&host),
            v.blocked(),
            v.as_str(),
            changed
                .map(|c| format!("{{\"{}\":{}}}", action, c))
                .unwrap_or_else(|| "null".to_string()),
            blocklist::overrides()
                .iter()
                .map(blocklist::Override::json)
                .collect::<Vec<_>>()
                .join(",")
        ),
    )
}

/// ブラウザ用の自動設定スクリプト (PAC)。自分自身・ローカル・`pac_direct` のホストは DIRECT、
/// それ以外はこのプロキシ経由 (落ちていれば DIRECT にフォールバック)。
fn pac(ep: &Endpoint<'_>, target: &str, self_addressed: bool) -> String {
    // 自分の名前: 絶対形式ならその authority、そうでなければ Host ヘッダー
    let authority = if self_addressed {
        target
            .split_once("://")
            .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
            .unwrap_or("")
    } else {
        ep.host.unwrap_or("")
    };
    let (self_host, _) = crate::net::split_host_port(authority);
    let self_host = self_host
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    let proxy = if self_host.is_empty() {
        format!("127.0.0.1:{}", ep.port)
    } else if self_host.contains(':') {
        format!("[{}]:{}", self_host, ep.port)
    } else {
        format!("{}:{}", self_host, ep.port)
    };
    let mut direct: Vec<String> = vec![
        "host === \"localhost\"".into(),
        "host === \"127.0.0.1\"".into(),
        "host === \"::1\"".into(),
        "isPlainHostName(host)".into(),
    ];
    if !self_host.is_empty() {
        direct.push(format!("host === \"{}\"", js_escape(&self_host)));
    }
    for pat in ep.pac_direct {
        let p = js_escape(pat);
        if let Some(bare) = pat.strip_prefix("*.") {
            direct.push(format!(
                "host === \"{}\" || shExpMatch(host, \"{}\")",
                js_escape(bare),
                p
            ));
        } else if pat.contains('*') {
            direct.push(format!("shExpMatch(host, \"{}\")", p));
        } else {
            direct.push(format!("host === \"{}\"", p));
        }
    }
    format!(
        "// sorahost-http-proxy PAC (PROXY_PAC_DIRECT で除外ホストを追加)\nfunction FindProxyForURL(url, host) {{\n  host = host.toLowerCase();\n  if ({}) return \"DIRECT\";\n  return \"PROXY {}; DIRECT\";\n}}\n",
        direct.join("\n      || "),
        proxy
    )
}

fn js_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// URL を正規化して全バリアントを消す。
fn purge_url(ep: &Endpoint<'_>, url: &str) -> (u16, &'static str, String) {
    match parse_origin(url, None) {
        Ok(origin) => {
            let canonical = origin.url();
            let n = ep.cache.invalidate(&canonical, ep.conn_id);
            (
                200,
                "application/json",
                format!(
                    "{{\"purged\":{},\"url\":\"{}\"}}",
                    n,
                    json_escape(&canonical)
                ),
            )
        }
        Err(e) => (
            400,
            "application/json",
            format!("{{\"error\":\"{}\"}}", json_escape(&e.to_string())),
        ),
    }
}

/// エントリの状態を返す (LRU には触らない)。バリアント無し (Accept-Encoding 無し) のキーを見る。
fn lookup(ep: &Endpoint<'_>, url: &str) -> (u16, &'static str, String) {
    let Ok(origin) = parse_origin(url, None) else {
        return (
            400,
            "application/json",
            "{\"error\":\"invalid url\"}".to_string(),
        );
    };
    let canonical = origin.url();
    let key = cache_key("GET", &canonical);
    match ep.cache.peek(key) {
        Some(info) => {
            let now = crate::cache::now_epoch();
            (
                200,
                "application/json",
                format!(
                    "{{\"found\":true,\"url\":\"{}\",\"memory\":{},\"disk\":{},\"size\":{},\"stored_at\":{},\"expires_at\":{},\"fresh\":{},\"ttl_left\":{},\"validators\":{}}}",
                    json_escape(&canonical),
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
        }
        None => (
            404,
            "application/json",
            format!(
                "{{\"found\":false,\"url\":\"{}\"}}",
                json_escape(&canonical)
            ),
        ),
    }
}

/// `a=b&c=d` を (キー, パーセントデコード済みの値) に分ける。
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect()
}

/// `%XX` を戻す (`+` はそのまま: URL の中の `+` を壊さない)。
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (
                hex(bytes[i + 1]),
                hex(bytes.get(i + 2).copied().unwrap_or(0)),
            )
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_queries() {
        assert_eq!(percent_decode("a%20b%2Fc+d"), "a b/c+d");
        assert_eq!(percent_decode("bad%zz%4"), "bad%zz%4");
        let q = parse_query("url=http%3A%2F%2Fexample.com%2Fx%3Fy%3D1&all=1&flag");
        assert_eq!(
            q[0],
            ("url".to_string(), "http://example.com/x?y=1".to_string())
        );
        assert_eq!(q[1], ("all".to_string(), "1".to_string()));
        assert_eq!(q[2], ("flag".to_string(), String::new()));
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
    }
}

#[cfg(test)]
mod local_path_tests {
    use super::local_path;

    #[test]
    fn origin_form_is_always_local() {
        assert_eq!(local_path("/status", 8080), Some("/status"));
        assert_eq!(local_path("/purge?all=1", 8080), Some("/purge?all=1"));
    }

    #[test]
    fn absolute_form_is_local_only_on_our_port() {
        assert_eq!(
            local_path("http://tokyo.example.net:60624/status", 60624),
            Some("/status")
        );
        assert_eq!(local_path("http://[::1]:60624", 60624), Some("/"));
        assert_eq!(local_path("http://example.com/status", 60624), None);
        assert_eq!(local_path("http://example.com/status", 80), Some("/status"));
        assert_eq!(local_path("http://example.com:8080/x", 60624), None);
        assert_eq!(local_path("https://example.com:60624/x", 60624), None);
    }
}

#[cfg(test)]
mod pac_tests {
    use super::*;

    fn ep<'a>(
        host: Option<&'a str>,
        direct: &'a [String],
    ) -> (Metrics, Cache, u16, Option<&'a str>, &'a [String]) {
        (
            Metrics::new(),
            Cache::new(crate::cache::CacheConfig::disabled()),
            8080,
            host,
            direct,
        )
    }

    #[test]
    fn pac_uses_host_header_and_direct_list() {
        let direct = vec!["*.example.com".to_string(), "intra".to_string()];
        let (m, c, port, host, d) = ep(Some("tokyo.sorahost.net:60624"), &direct);
        let e = Endpoint {
            metrics: &m,
            cache: &c,
            conn_id: 1,
            port,
            host,
            pac_direct: d,
        };
        let script = pac(&e, "/proxy.pac", false);
        assert!(script.contains("function FindProxyForURL(url, host)"));
        assert!(script.contains("return \"PROXY tokyo.sorahost.net:8080; DIRECT\""));
        assert!(script.contains("host === \"tokyo.sorahost.net\""));
        assert!(script.contains("host === \"example.com\" || shExpMatch(host, \"*.example.com\")"));
        assert!(script.contains("host === \"intra\""));
        assert!(script.contains("isPlainHostName(host)"));
    }

    #[test]
    fn pac_prefers_absolute_form_authority() {
        let (m, c, port, host, d) = ep(Some("other:1"), &[]);
        let e = Endpoint {
            metrics: &m,
            cache: &c,
            conn_id: 1,
            port,
            host,
            pac_direct: d,
        };
        let script = pac(&e, "http://proxy.local:8080/proxy.pac", true);
        assert!(script.contains("PROXY proxy.local:8080; DIRECT"));
        let script = pac(&e, "http://[::1]:8080/proxy.pac", true);
        assert!(script.contains("PROXY [::1]:8080; DIRECT"));
    }
}
