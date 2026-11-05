//! 要求行の対象と `Host` から要求先 ([`Origin`]) を決め、正規化 URL ([`Located`]) を組む。

use std::io;

use super::authority::{authority_end, check_authority};
use crate::net;
use crate::origin::Scheme;

/// ポート番号を確保せずに 10 進の文字列にする (`u16::to_string` は `String` を 1 本作る)。
fn port_digits(buf: &mut [u8; 5], mut port: u16) -> &str {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (port % 10) as u8;
        port /= 10;
        if port == 0 {
            break;
        }
    }
    // 数字しか書いていないので UTF-8 として必ず妥当
    std::str::from_utf8(&buf[i..]).unwrap_or("0")
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
        let mut digits = [0u8; 5];
        url.push_str(port_digits(
            &mut digits,
            port.unwrap_or_else(|| self.scheme.default_port()),
        ));
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

/// 要求の対象から `#fragment` を落とす (RFC 9112 §3.2 の要求の対象に fragment は入らない。T27.8)。
/// オリジンへは fragment の手前までを送る (今までは `/a#frag` をそのまま送っていた)
fn without_fragment(path: &str) -> &str {
    match path.find('#') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => path,
    }
}

/// 絶対 URI の `http://` / `https://` を**大小を無視して**外し、スキームと `//` の後ろを返す。
///
/// スキームは大小を区別しない (RFC 9110 §4.2.3) のに小文字だけを見ていたので、
/// `HTTP://h/p` はスキームの無い形として丸ごとを authority に読み、`[HTTP://h/p]:80` を
/// 引きに行って 502 になっていた (T.28.33)。マッピング形式 (`/https/`) はパスなので今までどおり小文字だけ。
pub fn strip_scheme(target: &str) -> Option<(Scheme, &str)> {
    let strip = |prefix: &str| -> Option<&str> {
        let head = target.get(..prefix.len())?;
        head.eq_ignore_ascii_case(prefix)
            .then(|| &target[prefix.len()..])
    };
    if let Some(rest) = strip("http://") {
        return Some((Scheme::Http, rest));
    }
    strip("https://").map(|rest| (Scheme::Https, rest))
}

/// 要求行の target と Host ヘッダーからオリジンを決める。
/// 受け付ける形: `http://h/p`、`https://h/p` (絶対形式)、`/https/h/p`、`/http/h/p` (マッピング)、
/// `/p` (Host ヘッダー宛て)、`h` (ホストのみ)。
pub fn parse_origin(target: &str, host_header: Option<&str>) -> io::Result<Origin> {
    let split = |scheme: Scheme, rest: &str, mapped: bool| -> io::Result<Origin> {
        let end = authority_end(rest);
        let host_port = check_authority(&rest[..end])?;
        // `http://h?q` の path は `/?q` (RFC 9112 §3.2.2: path が空なら `/` を補う)
        let path = match &rest[end..] {
            "" => "/".to_string(),
            p if p.starts_with('/') => without_fragment(p).to_string(),
            p => match without_fragment(p) {
                "/" => "/".to_string(),
                q => format!("/{}", q),
            },
        };
        Ok(Origin {
            scheme,
            host_port: host_port.to_string(),
            path,
            mapped,
        })
    };
    if let Some((scheme, rest)) = strip_scheme(target) {
        return split(scheme, rest, false);
    }
    if let Some(rest) = target.strip_prefix("/https/") {
        return split(Scheme::Https, rest, true);
    }
    if let Some(rest) = target.strip_prefix("/http/") {
        return split(Scheme::Http, rest, true);
    }
    // `*` (asterisk-form。`OPTIONS *` だけ。RFC 9112 §3.2.4) は `Host` のサーバー全体を指す (T.29.9)。
    // 今までは `*` をホスト名として引きに行って 502 だった。OPTIONS 以外の `*` は受け付けの層が 400 で断る
    if target.starts_with('/') || target == "*" {
        return match host_header {
            // `Host` も同じ形で確かめる (RFC 9112 §3.2 は不正な `Host` に 400 を求める。T.32.12)。
            // 絶対形式の要求の `Host` (宛先には使わない) は入口の [`target_host`] だけが見る (要求ごとに 2 回なめない)
            Some(h) if !h.is_empty() => Ok(Origin {
                scheme: Scheme::Http,
                host_port: check_authority(h)?.to_string(),
                path: without_fragment(target).to_string(),
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
        host_port: check_authority(target)?.to_string(),
        path: "/".to_string(),
        mapped: false,
    })
}

/// 絶対形式 (`http://h:P`) かマッピング形式 (`/http/h:P`) で、authority の後ろが空か (T.29.9)。
///
/// `OPTIONS` でこの形なら、オリジンへは `OPTIONS *` を送る (RFC 9112 §3.2.4。今までは `/` を補っていた)。
pub fn has_empty_path(target: &str) -> bool {
    let rest = strip_scheme(target)
        .map(|(_, rest)| rest)
        .or_else(|| target.strip_prefix("/https/"))
        .or_else(|| target.strip_prefix("/http/"));
    rest.is_some_and(|r| r.len() == authority_end(r))
}

/// 要求先のホスト (`host[:port]`) だけを**借用で**取り出す ([`parse_origin`] と同じ規則)。
///
/// ACL / ブロックリストの判定は `host_port` しか見ないので、そのためだけに
/// [`parse_origin`] を呼ぶと要求ごとに `String` を 2 本 (`host_port` と `path`) 作ることになる。
/// 返す文字列は必ず `target` か `host_header` の部分文字列なので確保が要らない。
pub fn target_host<'a>(target: &'a str, host_header: Option<&'a str>) -> io::Result<&'a str> {
    let host_of =
        |rest: &'a str| -> io::Result<&'a str> { check_authority(&rest[..authority_end(rest)]) };
    // `Host` も同じ形で確かめる (T.32.12)。ここは受け付けの入口なので、絶対形式の要求でも見る
    // (宛先は要求の対象で決めるが、不正な値は不正。空の `Host` は形としては通す)
    let host_header = match host_header {
        Some(h) if !h.is_empty() => Some(check_authority(h)?),
        other => other,
    };
    if let Some((_, rest)) = strip_scheme(target) {
        return host_of(rest);
    }
    for prefix in ["/https/", "/http/"] {
        if let Some(rest) = target.strip_prefix(prefix) {
            return host_of(rest);
        }
    }
    if target.starts_with('/') || target == "*" {
        return match host_header {
            Some(h) if !h.is_empty() => Ok(h),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Missing host in HTTP request",
            )),
        };
    }
    // スキームも `/` も無い形 (`h` や `h:443`) は、丸ごとがホストになる (形は確かめる。T.32.12)
    check_authority(target)
}

/// 要求ターゲットのスキームだけを**確保せずに**取り出す ([`parse_origin`] と同じ規則)。
///
/// ポートを書かない要求 (`http://example.com/`) の宛先を `pool_key` と同じ `host:port` に
/// そろえるために使う (T14.48)。既定のポートはスキームで決まる ([`Scheme::default_port`]) ので、
/// そのためだけに [`parse_origin`] を呼んで `String` を 2 本作らずに済ませる。
pub fn target_scheme(target: &str) -> Scheme {
    if let Some((scheme, _)) = strip_scheme(target) {
        scheme
    } else if target.starts_with("/https/") {
        Scheme::Https
    } else {
        // `http://` も `/http/` も、オリジン形式 (`/p`) もホストだけの形 (`h`) も http
        Scheme::Http
    }
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

#[cfg(test)]
mod port_digits_tests {
    use super::*;

    /// 確保しない 10 進が `to_string` と同じこと。
    #[test]
    fn matches_to_string() {
        for port in [0u16, 1, 9, 10, 80, 443, 8080, 65535] {
            let mut buf = [0u8; 5];
            assert_eq!(port_digits(&mut buf, port), port.to_string());
        }
    }

    /// `#fragment` は落とし、先頭が `#` なら `/`。
    #[test]
    fn fragment_is_dropped() {
        assert_eq!(without_fragment("/a#frag"), "/a");
        assert_eq!(without_fragment("#frag"), "/");
        assert_eq!(without_fragment("/a?q"), "/a?q");
    }
}
