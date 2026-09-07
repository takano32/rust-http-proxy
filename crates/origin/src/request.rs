//! リクエストの解析: 使うヘッダーの抽出と、要求先 (オリジン) の決定。

use std::io;

use crate::log_trace;
use crate::net;
use crate::origin::Scheme;

/// リクエストヘッダーのうち転送・キャッシュ判断に使うもの。
#[derive(Default)]
pub struct RequestHeaders {
    pub host: Option<String>,
    /// (小文字の名前, 値)
    pub pairs: Vec<(String, String)>,
    pub authorization: bool,
    pub cache_control: String,
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<String>,
    pub if_range: Option<String>,
    pub range: Option<String>,
    pub accept_encoding: Option<String>,
    pub connection_close: bool,
    pub connection_keep_alive: bool,
}

pub fn parse_request_headers(raw_headers: &[String], conn_id: usize) -> RequestHeaders {
    let mut h = RequestHeaders::default();
    for line in raw_headers {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k_lower = k.trim().to_ascii_lowercase();
        let v_trim = v.trim();
        log_trace!(Some(conn_id), "req header  {}: {}", k.trim(), v_trim);
        match k_lower.as_str() {
            "host" => h.host = Some(v_trim.to_string()),
            "authorization" => h.authorization = true,
            "cache-control" | "pragma" => {
                if !h.cache_control.is_empty() {
                    h.cache_control.push(',');
                }
                h.cache_control.push_str(&v_trim.to_ascii_lowercase());
            }
            "if-none-match" => h.if_none_match = Some(v_trim.to_string()),
            "if-modified-since" => h.if_modified_since = Some(v_trim.to_string()),
            "if-range" => h.if_range = Some(v_trim.to_string()),
            "range" => h.range = Some(v_trim.to_string()),
            "accept-encoding" => h.accept_encoding = Some(v_trim.to_string()),
            "connection" | "proxy-connection" => {
                for token in v_trim.split(',') {
                    let t = token.trim();
                    if t.eq_ignore_ascii_case("close") {
                        h.connection_close = true;
                    } else if t.eq_ignore_ascii_case("keep-alive") {
                        h.connection_keep_alive = true;
                    }
                }
            }
            _ => {}
        }
        h.pairs.push((k_lower, v_trim.to_string()));
    }
    h
}

/// 要求先 (オリジン)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub scheme: Scheme,
    /// ホスト (必要ならポート付き)、IPv6 リテラルは括弧付き
    pub host_port: String,
    pub path: String,
    /// `/https/host/path` 形式 (プロキシをオリジンとして叩く形) で頼まれたか。
    /// この場合、応答の Location も同じ形式に書き換えてクライアントをプロキシに留める
    pub mapped: bool,
}

impl Origin {
    // 以下 3 つは [`Origin::locate`] に委譲する。同じ正規化を 2 か所に書くと、
    // 片方だけ直したときに黙って食い違う (例: Location 先のキャッシュ無効化は
    // 「locate() 由来の接続先」と「server_addr() の結果」を突き合わせている)。

    pub fn server_addr(&self) -> String {
        self.locate().server_addr().to_string()
    }

    /// キャッシュキーとログに使う正規化 URL。
    pub fn url(&self) -> String {
        self.locate().into_url()
    }

    /// 接続プールのキー。
    pub fn pool_key(&self) -> String {
        self.locate().pool_key().to_string()
    }

    pub fn host(&self) -> String {
        net::split_host_port(&self.host_port).0
    }

    /// 正規化 URL を 1 本だけ組み立てる。接続プールのキーと接続先はその部分文字列として借りる。
    /// (要求ごとに `with_default_port` を 3 回、`format!` を 2 回やり直すのをやめる)
    pub fn locate(&self) -> Located {
        let addr_start = self.scheme.as_str().len() + 3; // "://"
        let mut url =
            String::with_capacity(addr_start + self.host_port.len() + 6 + self.path.len());
        url.push_str(self.scheme.as_str());
        url.push_str("://");
        let (host, port) = net::split_host_port_ref(&self.host_port);
        if host.contains(':') && !host.starts_with('[') {
            url.push('[');
            url.push_str(host);
            url.push(']');
        } else {
            url.push_str(host);
        }
        url.push(':');
        url.push_str(
            &port
                .unwrap_or_else(|| self.scheme.default_port())
                .to_string(),
        );
        let origin_end = url.len();
        url.push_str(&self.path);
        Located {
            url,
            origin_end,
            addr_start,
        }
    }
}

/// `scheme://host:port/path` を 1 本持ち、プールキーと接続先を部分文字列で返す。
pub struct Located {
    url: String,
    /// `scheme://host:port` が終わる位置
    origin_end: usize,
    /// `host:port` が始まる位置
    addr_start: usize,
}

impl Located {
    /// キャッシュキーとログに使う正規化 URL。
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 接続プールのキー (`scheme://host:port`)。
    pub fn pool_key(&self) -> &str {
        &self.url[..self.origin_end]
    }

    /// 接続先 (`host:port`)。
    pub fn server_addr(&self) -> &str {
        &self.url[self.addr_start..self.origin_end]
    }

    /// 組み立てた URL をそのまま受け取る (複製しない)。
    pub fn into_url(self) -> String {
        self.url
    }
}

/// 要求行の target と Host ヘッダーからオリジンを決める。
/// 受け付ける形: `http://h/p`、`https://h/p` (絶対形式)、`/https/h/p`、`/http/h/p` (マッピング)、
/// `/p` (Host ヘッダー宛て)、`h` (ホストのみ)。
pub fn parse_origin(target: &str, host_header: Option<&str>) -> io::Result<Origin> {
    let split = |scheme: Scheme, rest: &str, mapped: bool| -> io::Result<Origin> {
        let (host_port, path) = match rest.find('/') {
            Some(pos) => (&rest[..pos], &rest[pos..]),
            None => (rest, "/"),
        };
        if host_port.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Missing host in request target",
            ));
        }
        Ok(Origin {
            scheme,
            host_port: host_port.to_string(),
            path: path.to_string(),
            mapped,
        })
    };
    if let Some(rest) = target.strip_prefix("http://") {
        return split(Scheme::Http, rest, false);
    }
    if let Some(rest) = target.strip_prefix("https://") {
        return split(Scheme::Https, rest, false);
    }
    if let Some(rest) = target.strip_prefix("/https/") {
        return split(Scheme::Https, rest, true);
    }
    if let Some(rest) = target.strip_prefix("/http/") {
        return split(Scheme::Http, rest, true);
    }
    if target.starts_with('/') {
        return match host_header {
            Some(h) if !h.is_empty() => Ok(Origin {
                scheme: Scheme::Http,
                host_port: h.to_string(),
                path: target.to_string(),
                mapped: false,
            }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Missing host in HTTP request",
            )),
        };
    }
    Ok(Origin {
        scheme: Scheme::Http,
        host_port: target.to_string(),
        path: "/".to_string(),
        mapped: false,
    })
}

/// マッピング形式のクライアント向けに、絶対 URL の Location / Content-Location を `/https/h/p` 形式へ。
pub fn map_locations(lines: &mut [String]) {
    for line in lines.iter_mut() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let lower = name.trim().to_ascii_lowercase();
        if lower != "location" && lower != "content-location" {
            continue;
        }
        let v = value.trim();
        let mapped = if let Some(rest) = v.strip_prefix("https://") {
            Some(format!("/https/{}", rest))
        } else {
            v.strip_prefix("http://")
                .map(|rest| format!("/http/{}", rest))
        };
        if let Some(m) = mapped {
            *line = format!("{}: {}", name.trim(), m);
        }
    }
}
