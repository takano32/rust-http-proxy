//! `/proxy.pac`: ブラウザの自動設定スクリプト。

use super::Endpoint;

/// ブラウザ用の自動設定スクリプト (PAC)。自分自身・ローカル・`pac_direct` のホストは DIRECT、
/// それ以外はこのプロキシ経由 (落ちていれば DIRECT にフォールバック)。
pub(super) fn render(ep: &Endpoint<'_>, target: &str, self_addressed: bool) -> String {
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
        "// rust-http-proxy PAC (PROXY_PAC_DIRECT で除外ホストを追加)\nfunction FindProxyForURL(url, host) {{\n  host = host.toLowerCase();\n  if ({}) return \"DIRECT\";\n  return \"PROXY {}; DIRECT\";\n}}\n",
        direct.join("\n      || "),
        proxy
    )
}

fn js_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::metrics::Metrics;

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
        let script = render(&e, "/proxy.pac", false);
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
        let script = render(&e, "http://proxy.local:8080/proxy.pac", true);
        assert!(script.contains("PROXY proxy.local:8080; DIRECT"));
        let script = render(&e, "http://[::1]:8080/proxy.pac", true);
        assert!(script.contains("PROXY [::1]:8080; DIRECT"));
    }
}
