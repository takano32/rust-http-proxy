//! リクエストの解析: 使うヘッダーの抽出と、要求先 (オリジン) の決定。

use std::io;

use crate::log_trace;
use crate::net;
use crate::origin::Scheme;

/// リクエストヘッダーのうち転送・キャッシュ判断に使うもの。
#[derive(Default)]
pub(super) struct RequestHeaders {
    pub(super) host: Option<String>,
    /// (小文字の名前, 値)
    pub(super) pairs: Vec<(String, String)>,
    pub(super) authorization: bool,
    pub(super) cache_control: String,
    pub(super) if_none_match: Option<String>,
    pub(super) if_modified_since: Option<String>,
    pub(super) if_range: Option<String>,
    pub(super) range: Option<String>,
    pub(super) accept_encoding: Option<String>,
    pub(super) connection_close: bool,
    pub(super) connection_keep_alive: bool,
}

pub(super) fn parse_request_headers(raw_headers: &[String], conn_id: usize) -> RequestHeaders {
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
    pub fn server_addr(&self) -> String {
        net::with_default_port(&self.host_port, self.scheme.default_port())
    }

    /// キャッシュキーとログに使う正規化 URL。
    pub fn url(&self) -> String {
        format!("{}://{}{}", self.scheme, self.server_addr(), self.path)
    }

    /// 接続プールのキー。
    pub fn pool_key(&self) -> String {
        format!("{}://{}", self.scheme, self.server_addr())
    }

    pub fn host(&self) -> String {
        net::split_host_port(&self.host_port).0
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
