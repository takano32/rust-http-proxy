//! プロキシ自身のエンドポイント: `/dashboard` (コントロールパネル)、`/healthz` `/status`
//! (`?sort=requests|errors|dns|slow` で `hosts[]` の上位 50 の切り出しを変えられる。T13.3)、
//! `/history` (JSON、`res=5|60|3600`)、`/metrics` (Prometheus)、`/proxy.pac` (ブラウザの自動設定)、
//! `/purge` と `PURGE` メソッド、`/lookup`、`/blocklist` (判定と手動の上書き)。
//!
//! 自分宛てかどうかは**ポートだけ**で決める: 絶対形式 (`http://host:PORT/status`) は authority の、
//! オリジン形式 (`GET /status` + `Host:`) は `Host` のポート (無ければ 80) が自分の待ち受けポートと
//! 同じときだけ自分宛て。自分宛てで知らないパスは 404、`/` は 200 でこの一覧を返す
//! (自分へ転送してループしない。T12.3)。ポートの違うオリジン形式は今までどおり転送する。
//! 応答は常に `Connection: close`。認証は無いので、到達できる人は誰でも purge できる
//! (公開ポートなら ACL や到達制御で守ること)。

use std::io::{self, Write};

use crate::cache::{Cache, cache_key};
use crate::http::parse_origin;
use crate::log_info;
use crate::metrics::{self, Metrics};
use crate::persist;
use crate::prom;
use crate::reload;

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
    /// lite プロファイル (ダッシュボードを持たない)
    pub lite: bool,
    /// 動いているバイナリの版 (`/status` に出す。本体クレートの `VERSION`)
    pub version: &'a str,
    /// 上限といまのスレッド数を引く口 (`/status` と `/metrics` を組み立てるときだけ呼ぶ)。
    ///
    /// 値そのものではなく関数で受け取るのは、生きているスレッド数と待ち行列を数えるのに
    /// **全接続スレッドで共有している鍵**を取るため。`Endpoint` は要求ごとに組むので、
    /// ここで数えると熱い経路に乗ってしまう (この 2 つのパスに来たときだけ引く)
    pub concurrency: &'a dyn Fn() -> metrics::Concurrency,
}

mod blocklist;
mod pac;
mod recent;

const DASHBOARD_HTML: &str = include_str!("../web/dashboard.html");

/// 要求ターゲットを自分宛てのパスに直す。**どちらの形式もポートだけで判定する**:
/// 絶対形式は authority の、オリジン形式は `Host` ヘッダーのポート (無ければ 80) が
/// 自分の待ち受けポートと同じときだけ自分宛て。それ以外 (他所への転送) は `None`。
///
/// オリジン形式を無条件に自分宛てにしていた頃は、知らないパスが `Host` 宛ての転送に落ちて
/// **`Host` が自分自身ならループした** (1 要求で `max_conns` 本。T12.3 の前提 4)。
/// `Host` が無い HTTP/1.0 のオリジン形式は自分宛てにしない (今までどおり 400)。
fn local_path<'a>(target: &'a str, port: u16, host: Option<&str>) -> Option<&'a str> {
    if target.starts_with('/') {
        return (authority_port(host?)? == port).then_some(target);
    }
    let rest = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("HTTP://"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    (authority_port(authority)? == port).then_some(path)
}

/// `host[:port]` のポート (無ければ 80)。`[::1]` のようなブラケット付きも扱う。
fn authority_port(authority: &str) -> Option<u16> {
    if authority.is_empty() {
        return None;
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => p.parse::<u16>().ok(),
        _ => Some(80),
    }
}

/// 自分宛ての `/` に返す案内 (エンドポイントの一覧)。`--lite` では `/dashboard` を載せない。
fn endpoint_list(lite: bool) -> String {
    let dashboard = if lite {
        ""
    } else {
        "  /dashboard                                  control panel (graphs, per-host stats)\n"
    };
    format!(
        "rust-http-proxy - an HTTP/HTTPS(CONNECT) forward proxy.\n\n\
         This is the proxy itself, not a web site. Point your browser or client at\n\
         this address as an HTTP proxy (or use /proxy.pac below).\n\n\
         endpoints:\n\
         {}\
         \x20 /status[?sort=errors|dns|slow]              JSON: counters, hosts, cache, threads\n\
         \x20 /errors?n=100                               JSON: the last errors (who, when, why)\n\
         \x20 /connections                                JSON: the connections open right now\n\
         \x20 /healthz                                    same as /status\n\
         \x20 /history?res=5|60|3600                      JSON: time series\n\
         \x20 /metrics                                    Prometheus text format\n\
         \x20 /proxy.pac                                  browser auto-config script\n\
         \x20 /lookup?url=<url>                           cache entry state\n\
         \x20 /purge?url=<url> | /purge?all=1             drop cache entries (also: PURGE <url>)\n\
         \x20 /blocklist?host=&action=block|allow|clear   blocklist decision and overrides\n",
        dashboard
    )
}

/// 内部エンドポイントなら応答して `Ok(true)` を返す。そうでなければ何もせず `Ok(false)`。
pub fn handle(
    client: &mut impl Write,
    method: &str,
    target: &str,
    ep: &Endpoint<'_>,
) -> io::Result<bool> {
    let is_purge = method.eq_ignore_ascii_case("PURGE");
    let local = if is_purge {
        Some(target)
    } else {
        local_path(target, ep.port, ep.host)
    };
    let Some(local) = local else {
        return Ok(false);
    };
    // ここへ来た要求はどちらの形式でも自分宛て (`local_path` がポートで判定済み)。
    // この旗は「絶対形式か」= `/proxy.pac` が自分の名前をどちらから取るか、だけに使う
    let absolute_form = !target.starts_with('/');
    let (path, query) = match local.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (local, None),
    };
    let is_get = method.eq_ignore_ascii_case("GET");
    let (status, content_type, body) = if is_purge {
        purge_url(ep, target)
    } else if is_get && (path == "/dashboard" || path == "/dashboard/") {
        if ep.lite {
            (
                200,
                "text/plain; charset=utf-8",
                "lite mode: the dashboard is off (PROXY_PROFILE=lite)\n".to_string(),
            )
        } else {
            (200, "text/html; charset=utf-8", DASHBOARD_HTML.to_string())
        }
    } else if is_get && path == "/proxy.pac" {
        (
            200,
            "application/x-ns-proxy-autoconfig",
            pac::render(ep, target, absolute_form),
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
    } else if is_get && path == "/errors" {
        // 個票 (T13.4)。集計 (`/status`) では読めない「誰が・いつ・なぜ」を出す
        recent::errors(ep, query)
    } else if is_get && path == "/connections" {
        recent::connections(ep)
    } else if is_get && path == "/blocklist" {
        blocklist::handle(&parse_query(query.unwrap_or("")))
    } else if is_get && (path == "/healthz" || path == "/status") {
        // `?sort=requests|errors|dns|slow` は `hosts[]` の上位 50 を切り出す鍵だけを変える
        // (T13.3)。知らない値は既定に倒す。**`/healthz` は問い合わせを読まない**
        // (監視が叩く口の意味を変えない)
        let sort = if path == "/status" {
            parse_query(query.unwrap_or(""))
                .iter()
                .find(|(k, _)| k == "sort")
                .map_or(metrics::HostSort::Requests, |(_, v)| {
                    metrics::HostSort::from_param(v)
                })
        } else {
            metrics::HostSort::Requests
        };
        (
            200,
            "application/json",
            // `/status` の組み立てはここの仕事。指標は部品を並べるだけにしてある
            // (下の層が上の層を呼ぶと依存が輪になるため)
            ep.metrics.to_json_with_cache(
                Some(ep.cache),
                metrics::StatusExtras {
                    settings: &reload::status_json(),
                    blocklist: &crate::blocklist::status_json(),
                    state_file: &persist::status_json(),
                    version: ep.version,
                    concurrency: (ep.concurrency)(),
                    sort,
                },
            ),
        )
    } else if is_get && path == "/metrics" {
        (
            200,
            "text/plain; version=0.0.4; charset=utf-8",
            // 上限といまのスレッド数はここで 1 回だけ引く (`/status` と同じ形)
            prom::render(ep.metrics, Some(ep.cache), (ep.concurrency)()),
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
    } else if is_get && (path == "/" || path.is_empty()) {
        // ブラウザでプロキシの URL を開いた人への案内 (`--lite` でも出す)
        (200, "text/plain; charset=utf-8", endpoint_list(ep.lite))
    } else {
        // 自分宛てだが知らないパス: 自分へ転送するとループするので 404
        (
            404,
            "text/plain; charset=utf-8",
            format!("not found.\n\n{}", endpoint_list(ep.lite)),
        )
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
                    crate::json::escape(&canonical)
                ),
            )
        }
        Err(e) => (
            400,
            "application/json",
            format!("{{\"error\":\"{}\"}}", crate::json::escape(&e.to_string())),
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
        }
        None => (
            404,
            "application/json",
            format!(
                "{{\"found\":false,\"url\":\"{}\"}}",
                crate::json::escape(&canonical)
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
        assert_eq!(crate::json::escape("a\"b\\c"), "a\\\"b\\\\c");
    }
}

#[cfg(test)]
mod local_path_tests {
    use super::{endpoint_list, local_path};

    #[test]
    fn origin_form_is_local_only_when_the_host_port_is_ours() {
        assert_eq!(
            local_path("/status", 8080, Some("127.0.0.1:8080")),
            Some("/status")
        );
        assert_eq!(
            local_path("/purge?all=1", 8080, Some("[::1]:8080")),
            Some("/purge?all=1")
        );
        assert_eq!(local_path("/", 80, Some("proxy.example.net")), Some("/"));
        // ポートが違えば今までどおり転送する (透過プロキシの使い方を壊さない)
        assert_eq!(local_path("/status", 8080, Some("example.com")), None);
        assert_eq!(local_path("/status", 8080, Some("example.com:80")), None);
        assert_eq!(local_path("/x", 8080, Some("127.0.0.1:9999")), None);
        // Host の無い HTTP/1.0 のオリジン形式は今までどおり (転送に落ちて 400)
        assert_eq!(local_path("/status", 8080, None), None);
        assert_eq!(local_path("/status", 8080, Some("")), None);
        assert_eq!(local_path("/status", 8080, Some("host:nope")), None);
    }

    #[test]
    fn absolute_form_is_local_only_on_our_port() {
        assert_eq!(
            local_path("http://tokyo.example.net:60624/status", 60624, None),
            Some("/status")
        );
        assert_eq!(local_path("http://[::1]:60624", 60624, None), Some("/"));
        assert_eq!(local_path("http://example.com/status", 60624, None), None);
        assert_eq!(
            local_path("http://example.com/status", 80, None),
            Some("/status")
        );
        assert_eq!(local_path("http://example.com:8080/x", 60624, None), None);
        assert_eq!(local_path("https://example.com:60624/x", 60624, None), None);
    }

    /// ダッシュボードに Phase 13 が見る図と KPI が載っていること (T12.4 (5)、T13.3)。
    ///
    /// ブラウザが無いので絵は確かめられない。**`/history` と `/status` の読み方が
    /// 合っているか**は `scripts/check-dashboard.js` (Node があるときだけ) が
    /// 実出力を通して見る。ここで見るのは「消えていないこと」だけ。
    #[test]
    fn the_dashboard_has_the_charts_phase_13_reads() {
        let html = super::DASHBOARD_HTML;
        for id in [
            "ch-conn",
            "ch-err",
            "ch-fd",
            "connp50",
            "version",
            "window",
            "dnsmiss",
            "connlimit",
            "bad",
        ] {
            assert!(html.contains(&format!("id=\"{}\"", id)), "{} が無い", id);
        }
        // 配列の配列を読む側 (キー名を戻す関数) と区間の分位点、`/status` を読む側 (T13.3)
        for f in [
            "function toSamples(",
            "function winQuantile(",
            "function mergeWindows(",
            "function dnsStats(",
            "function badHosts(",
            "function peak(",
        ] {
            assert!(html.contains(f), "{} が無い", f);
        }
        // ホスト別の表の列 (名前解決 / 接続、v4 / v6) と並びの選択肢 (T13.3 で 2 つ増えた)
        assert!(html.contains("名前解決 / 接続"), "{}", "内訳の列が無い");
        assert!(html.contains("v4 / v6"), "{}", "族の列が無い");
        for opt in ["\"p95\"", "\"errors\"", "\"dns\"", "\"slow\"", "\"bytes\""] {
            assert!(
                html.contains(&format!("<option value={}>", opt)),
                "並びの選択肢 {} が無い",
                opt
            );
        }
        // 「悪いホスト」は `?sort=` の 2 本を **30 秒に 1 回だけ** 取る (負荷を増やさない)
        assert!(
            html.contains("'/status?sort='+k"),
            "{}",
            "?sort= を取っていない"
        );
        assert!(
            html.contains("setInterval(pollBad,30000)"),
            "{}",
            "30 秒ごとになっていない"
        );
        // 外部ライブラリは読み込まない (依存なしの 1 ページ)
        assert!(!html.contains("<script src="), "外部 JS を読み込んでいる");
        assert!(
            !html.contains("<link rel=\"stylesheet\""),
            "外部 CSS を読み込んでいる"
        );
    }

    #[test]
    fn the_listing_hides_the_dashboard_in_lite_mode() {
        assert!(endpoint_list(false).contains("/dashboard"));
        assert!(!endpoint_list(true).contains("/dashboard"));
        for path in ["/status", "/metrics", "/proxy.pac", "/purge?url="] {
            assert!(endpoint_list(true).contains(path), "{}", path);
        }
    }
}
