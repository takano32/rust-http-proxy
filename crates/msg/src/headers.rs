use std::net::SocketAddr;

use crate::ascii;
use crate::via;

const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "upgrade",
    "proxy-connection",
];

pub fn is_hop_by_hop(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP_HEADERS.contains(&lower.as_str())
}

/// 小文字化した文字列を作らずに判定する版。
#[inline]
pub fn is_hop_by_hop_name(name: &str) -> bool {
    HOP_BY_HOP_HEADERS
        .iter()
        .any(|h| name.eq_ignore_ascii_case(h))
}

/// `Connection:` で指名される独自の hop-by-hop 名を、確保せずに覚えておける本数。
const MAX_CUSTOM_HOP: usize = 16;

/// 転送するリクエストヘッダーを `out` へ直接書く ([`sanitize_and_inject_headers`] と同じ規則)。
///
/// `Vec<String>` を経由しないので、ヘッダー 1 本につき小文字化した名前と行の複製を作らない
/// (実測: ヘッダー 11 本の要求で確保が 137 → 62 回/要求)。`Host:` は呼び出し側がオリジンの
/// ものに差し替えるので、ここでは落とす。
pub fn write_request_headers(out: &mut Vec<u8>, headers: &[String], client_ip: Option<&str>) {
    // Connection: で指名された名前は借用のまま覚える (溢れたら元の行を読み直す)
    let mut custom: [&str; MAX_CUSTOM_HOP] = [""; MAX_CUSTOM_HOP];
    let mut n_custom = 0usize;
    let mut overflow = false;
    for line in headers {
        if let Some((k, v)) = ascii::split_once(line, b':')
            && k.trim_ascii().eq_ignore_ascii_case("connection")
        {
            for item in v.split(',') {
                let token = item.trim_ascii();
                if token.is_empty() {
                    continue;
                }
                if n_custom < MAX_CUSTOM_HOP {
                    custom[n_custom] = token;
                    n_custom += 1;
                } else {
                    overflow = true;
                }
            }
        }
    }
    let named_in_connection = |name: &str| -> bool {
        if custom[..n_custom]
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name))
        {
            return true;
        }
        overflow
            && headers.iter().any(|line| {
                ascii::split_once(line, b':').is_some_and(|(k, v)| {
                    k.trim_ascii().eq_ignore_ascii_case("connection")
                        && v.split(',')
                            .any(|t| t.trim_ascii().eq_ignore_ascii_case(name))
                })
            })
    };

    let mut x_forwarded_for: Option<&str> = None;
    let mut has_via = false;
    for line in headers {
        let Some((k, v)) = ascii::split_once(line, b':') else {
            continue;
        };
        let name = k.trim_ascii();
        // 枠組みのヘッダー (Content-Length / Transfer-Encoding) は Connection: で指名されても
        // 落とさない。落とすと「ヘッダーは枠組み無し・本文は送る」というずれが起き、
        // オリジンが本文を次の要求の先頭として読む (要求スマグリング)。
        // 本文を送るかどうかは Framing::of_request が生のヘッダーから決めるので、
        // ここだけで落とすと両者が食い違う
        let framing_header = FRAMING_HEADERS.iter().any(|f| name.eq_ignore_ascii_case(f));
        if name.eq_ignore_ascii_case("host")
            || is_hop_by_hop_name(name)
            || (!framing_header && named_in_connection(name))
        {
            continue;
        }
        if name.eq_ignore_ascii_case("x-forwarded-for") {
            x_forwarded_for = Some(v.trim_ascii());
            continue;
        }
        if name.eq_ignore_ascii_case("via") {
            has_via = true;
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(v.trim_ascii().as_bytes());
            out.extend_from_slice(b", ");
            out.extend_from_slice(via::token().as_bytes());
            out.extend_from_slice(b"\r\n");
            continue;
        }
        out.extend_from_slice(line.as_bytes());
    }

    if let Some(ip) = client_ip {
        out.extend_from_slice(b"X-Forwarded-For: ");
        if let Some(existing) = x_forwarded_for {
            out.extend_from_slice(existing.as_bytes());
            out.extend_from_slice(b", ");
        }
        out.extend_from_slice(ip.as_bytes());
        out.extend_from_slice(b"\r\n");
    } else if let Some(existing) = x_forwarded_for {
        out.extend_from_slice(b"X-Forwarded-For: ");
        out.extend_from_slice(existing.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    if !has_via {
        // 起動時に 1 回作った行をそのまま書く (要求ごとに String を作らない)
        out.extend_from_slice(via::line().as_bytes());
    }
}

/// レスポンスの先頭 (ステータス行 + ヘッダー) から hop-by-hop と枠組みのヘッダーを除いたもの。
/// ステータス行は自分のバージョン (HTTP/1.1) に揃える。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseHead {
    pub status_line: String,
    /// `Name: value` (CRLF 無し)
    pub lines: Vec<String>,
}

impl ResponseHead {
    /// 追加のヘッダー行を足して、空行まで含めたバイト列にする。
    pub fn assemble<S: AsRef<str>>(&self, extra: &[S]) -> Vec<u8> {
        let mut out = String::with_capacity(256);
        out.push_str(&self.status_line);
        out.push_str("\r\n");
        for line in self.lines.iter() {
            out.push_str(line);
            out.push_str("\r\n");
        }
        for line in extra {
            out.push_str(line.as_ref());
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out.into_bytes()
    }
}

/// 応答ヘッダー 1 行の名前が `names` のどれかと一致するか (大小は無視する)。
///
/// 落とすヘッダーの指名に使う。行に `:` が無ければ名前は空扱い (どれにも一致しない)。
pub fn line_named(line: &str, names: &[&str]) -> bool {
    if names.is_empty() {
        return false;
    }
    let name = ascii::split_once(line, b':')
        .map(|(k, _)| k.trim_ascii())
        .unwrap_or("");
    names.iter().any(|n| name.eq_ignore_ascii_case(n))
}

/// 応答の先頭を `Vec<String>` を経由せずに `out` へ書く。
///
/// [`sanitize_response_head`] + [`ResponseHead::assemble`] と同じ結果を、ヘッダー 1 本ごとの
/// String を作らずに得る。`Location` の書き換えが要るとき (マッピング形式) は使えないので、
/// そのときは従来どおり [`sanitize_response_head`] を使う。
/// `status_line` を渡すとステータス行を差し替える (206 / 416 用)。
/// `drop_names` に挙げた名前のヘッダーは落とす (416 の `Content-Type` 用。空なら何もしない)。
pub fn write_response_head<S: AsRef<str>>(
    out: &mut Vec<u8>,
    head: &[u8],
    status_line: Option<&str>,
    drop_names: &[&str],
    extra: &[S],
) {
    let text = String::from_utf8_lossy(head);
    let first = text.split('\n').next().unwrap_or("").trim_ascii();
    match status_line {
        Some(sl) => out.extend_from_slice(sl.as_bytes()),
        None => {
            let rest = ascii::split_once(first, b' ')
                .map(|x| x.1)
                .unwrap_or("200 OK")
                .trim_ascii();
            out.extend_from_slice(b"HTTP/1.1 ");
            out.extend_from_slice(rest.as_bytes());
        }
    }
    out.extend_from_slice(b"\r\n");

    // ヘッダー行は 2 回なめるが、`Vec<&str>` に集めない (要求ごとの確保を 1 つ減らす)
    let raw = || {
        text.split('\n')
            .skip(1)
            // 行末の空白 (CR を含む) は値の側で必ず落とすので、ここで落としても結果は変わらない
            .map(|l| l.trim_ascii_end())
            .filter(|l| !l.is_empty())
    };
    let mut custom: [&str; MAX_CUSTOM_HOP] = [""; MAX_CUSTOM_HOP];
    let mut n_custom = 0usize;
    let mut overflow = false;
    for line in raw() {
        if let Some((k, v)) = ascii::split_once(line, b':')
            && k.trim_ascii().eq_ignore_ascii_case("connection")
        {
            for item in v.split(',') {
                let token = item.trim_ascii();
                if token.is_empty() {
                    continue;
                }
                if n_custom < MAX_CUSTOM_HOP {
                    custom[n_custom] = token;
                    n_custom += 1;
                } else {
                    overflow = true;
                }
            }
        }
    }
    let named_in_connection = |name: &str| -> bool {
        if custom[..n_custom]
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name))
        {
            return true;
        }
        overflow
            && raw().any(|line| {
                ascii::split_once(line, b':').is_some_and(|(k, v)| {
                    k.trim_ascii().eq_ignore_ascii_case("connection")
                        && v.split(',')
                            .any(|t| t.trim_ascii().eq_ignore_ascii_case(name))
                })
            })
    };

    for line in raw() {
        let Some((k, v)) = ascii::split_once(line, b':') else {
            continue;
        };
        let name = k.trim_ascii();
        if is_hop_by_hop_name(name)
            || named_in_connection(name)
            || drop_names.iter().any(|d| name.eq_ignore_ascii_case(d))
            || FRAMING_HEADERS.iter().any(|f| name.eq_ignore_ascii_case(f))
        {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.trim_ascii().as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    for line in extra {
        out.extend_from_slice(line.as_ref().as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
}

/// 応答の先頭 (生バイト列) からヘッダー行を「名前, 値」で返す。前後の空白は落としてある。
///
/// 状態行は飛ばし、最初の空行で終わる。`Vec` も `String` も作らないので、
/// 素通しの経路 (枠組みと `Connection` しか見ない) はこれだけで足りる。
pub fn response_lines(head: &[u8]) -> impl Iterator<Item = (&str, &str)> {
    head.split(|b| *b == b'\n')
        .skip(1)
        .map(|line| std::str::from_utf8(line).unwrap_or("").trim_ascii())
        .take_while(|line| !line.is_empty())
        .filter_map(|line| ascii::split_once(line, b':'))
        .map(|(k, v)| (k.trim_ascii(), v.trim_ascii()))
}

/// 応答の先頭を「小文字の名前, 値」の組にする ([`response_lines`] を複製した形)。
/// キャッシュの判定 ([`crate::freshness`] 相当) のように組で受け取る API のために残してある。
pub fn response_pairs(head: &[u8]) -> Vec<(String, String)> {
    response_lines(head)
        .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
        .collect()
}

/// 応答の先頭から、名前が一致する最初のヘッダーの値を借用で返す。
pub fn response_value<'a>(head: &'a [u8], name: &str) -> Option<&'a str> {
    response_lines(head)
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

/// 応答の `name` ヘッダーが `token` を (カンマ区切りの中に) 挙げているか。
pub fn response_names(head: &[u8], name: &str, token: &str) -> bool {
    response_lines(head).any(|(k, v)| {
        k.eq_ignore_ascii_case(name)
            && v.split(',')
                .any(|t| t.trim_ascii().eq_ignore_ascii_case(token))
    })
}

/// レスポンスヘッダーのうち、プロキシが自分で決め直すもの (枠組み・経過時間)。
const FRAMING_HEADERS: &[&str] = &["transfer-encoding", "content-length", "age"];

pub fn sanitize_response_head(head: &[u8]) -> ResponseHead {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split('\n').map(|l| l.trim_ascii_end());
    let status_line = lines.next().unwrap_or("").trim_ascii();
    let mut parts = status_line.splitn(2, ' ');
    let _version = parts.next();
    let rest = parts.next().unwrap_or("200 OK").trim_ascii();
    let mut out = ResponseHead {
        status_line: format!("HTTP/1.1 {}", rest),
        lines: Vec::new(),
    };
    let raw: Vec<&str> = lines.filter(|l| !l.is_empty()).collect();
    let mut custom_hop: Vec<String> = Vec::new();
    for line in &raw {
        if let Some((k, v)) = ascii::split_once(line, b':')
            && k.trim_ascii().eq_ignore_ascii_case("connection")
        {
            custom_hop.extend(v.split(',').map(|t| t.trim_ascii().to_ascii_lowercase()));
        }
    }
    for line in raw {
        let Some((k, v)) = ascii::split_once(line, b':') else {
            continue;
        };
        let lower = k.trim_ascii().to_ascii_lowercase();
        if is_hop_by_hop(&lower)
            || custom_hop.contains(&lower)
            || FRAMING_HEADERS.contains(&lower.as_str())
        {
            continue;
        }
        out.lines
            .push(format!("{}: {}", k.trim_ascii(), v.trim_ascii()));
    }
    out
}

pub fn sanitize_and_inject_headers(
    headers: &[String],
    client_addr: Option<SocketAddr>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut custom_hop_by_hop = Vec::new();
    let mut x_forwarded_for: Option<String> = None;
    let mut has_via = false;

    // First pass: check Connection header for custom hop-by-hop header names
    for line in headers {
        if let Some((k, v)) = ascii::split_once(line, b':')
            && k.trim_ascii().eq_ignore_ascii_case("connection")
        {
            for item in v.split(',') {
                custom_hop_by_hop.push(item.trim_ascii().to_ascii_lowercase());
            }
        }
    }

    // Second pass: filter and collect
    for line in headers {
        if let Some((k, v)) = ascii::split_once(line, b':') {
            let k_trim = k.trim_ascii();
            let k_lower = k_trim.to_ascii_lowercase();

            // 枠組みのヘッダーは Connection: で指名されても落とさない
            // (要求スマグリング対策。write_request_headers 側と同じ規則)
            let framing_header = FRAMING_HEADERS.contains(&k_lower.as_str());
            if is_hop_by_hop(&k_lower) || (!framing_header && custom_hop_by_hop.contains(&k_lower))
            {
                continue;
            }

            if k_lower == "x-forwarded-for" {
                x_forwarded_for = Some(v.trim_ascii().to_string());
                continue;
            }

            if k_lower == "via" {
                has_via = true;
                let new_via = format!("{}: {}, {}\r\n", k_trim, v.trim_ascii(), via::token());
                out.push(new_via);
                continue;
            }

            out.push(line.clone());
        }
    }

    // Add X-Forwarded-For
    if let Some(addr) = client_addr {
        let ip_str = addr.ip().to_string();
        let xff_val = match x_forwarded_for {
            Some(existing) => format!("{}, {}", existing, ip_str),
            None => ip_str,
        };
        out.push(format!("X-Forwarded-For: {}\r\n", xff_val));
    } else if let Some(existing) = x_forwarded_for {
        out.push(format!("X-Forwarded-For: {}\r\n", existing));
    }

    // Add Via if not already updated
    if !has_via {
        out.push(via::line().to_string());
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_head_is_sanitized_and_reassembled() {
        let head = b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\nConnection: close, X-Custom\r\nX-Custom: 1\r\nKeep-Alive: timeout=5\r\nAge: 12\r\nContent-Type: text/plain\nETag: \"a\"\n\n";
        let h = sanitize_response_head(head);
        assert_eq!(h.status_line, "HTTP/1.1 200 OK");
        assert_eq!(
            h.lines,
            vec![
                "Content-Type: text/plain".to_string(),
                "ETag: \"a\"".to_string()
            ]
        );
        let bytes = h.assemble(&["Content-Length: 2".to_string()]);
        assert_eq!(
            bytes,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nETag: \"a\"\r\nContent-Length: 2\r\n\r\n"
        );
        assert_eq!(
            sanitize_response_head(b"garbage").status_line,
            "HTTP/1.1 200 OK"
        );
    }

    #[test]
    fn test_hop_by_hop_removal() {
        let raw = vec![
            "Host: example.com\r\n".to_string(),
            "Proxy-Connection: keep-alive\r\n".to_string(),
            "Keep-Alive: timeout=5\r\n".to_string(),
            "Connection: close, X-Foo\r\n".to_string(),
            "X-Foo: bar\r\n".to_string(),
            "User-Agent: curl/7.88.1\r\n".to_string(),
        ];
        let addr = "192.168.1.100:54321".parse().unwrap();
        let cleaned = sanitize_and_inject_headers(&raw, Some(addr));

        assert!(cleaned.iter().any(|h| h.starts_with("Host: example.com")));
        assert!(
            cleaned
                .iter()
                .any(|h| h.starts_with("User-Agent: curl/7.88.1"))
        );
        assert!(
            cleaned
                .iter()
                .any(|h| h.starts_with("X-Forwarded-For: 192.168.1.100"))
        );
        assert!(cleaned.iter().any(|h| h == &via::line().to_string()));

        assert!(
            !cleaned
                .iter()
                .any(|h| h.to_ascii_lowercase().starts_with("proxy-connection"))
        );
        assert!(
            !cleaned
                .iter()
                .any(|h| h.to_ascii_lowercase().starts_with("keep-alive"))
        );
        assert!(
            !cleaned
                .iter()
                .any(|h| h.to_ascii_lowercase().starts_with("x-foo"))
        );
    }
}

#[cfg(test)]
mod write_request_tests {
    use super::*;

    /// 従来の `sanitize_and_inject_headers` + Host 除去 + 連結と、1 バイトも違わないこと。
    fn old_way(headers: &[String], addr: Option<SocketAddr>) -> Vec<u8> {
        let mut out = Vec::new();
        for h in sanitize_and_inject_headers(headers, addr)
            .into_iter()
            .filter(|h| !h.trim_start().to_ascii_lowercase().starts_with("host:"))
        {
            out.extend_from_slice(h.as_bytes());
        }
        out
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|l| format!("{}\r\n", l)).collect()
    }

    #[test]
    fn matches_the_previous_implementation_byte_for_byte() {
        let addr: SocketAddr = "203.0.113.7:1234".parse().unwrap();
        let cases: Vec<Vec<String>> = vec![
            lines(&["Host: example.com", "Accept: */*"]),
            lines(&["Host: example.com", "Connection: keep-alive", "Accept: */*"]),
            // Connection で独自の hop-by-hop を指名する
            lines(&[
                "Connection: X-Private, Upgrade",
                "X-Private: secret",
                "Upgrade: websocket",
                "Accept: */*",
            ]),
            // 既存の Via と X-Forwarded-For を引き継ぐ
            lines(&["Via: 1.0 upstream", "X-Forwarded-For: 198.51.100.1", "A: b"]),
            // hop-by-hop 一式
            lines(&[
                "Keep-Alive: timeout=5",
                "Proxy-Authorization: Basic x",
                "TE: trailers",
                "Transfer-Encoding: chunked",
                "Cookie: a=b",
            ]),
            // 大文字小文字の混在と空白
            lines(&[
                "hOsT:  example.com ",
                "cOnNeCtIoN: X-Odd",
                "X-ODD: 1",
                "Z:  v  ",
            ]),
            // 区切りの無い行 (無視される)
            vec!["NoColon\r\n".to_string(), "A: b\r\n".to_string()],
            Vec::new(),
        ];
        for headers in &cases {
            for client in [Some(addr), None] {
                let ip = client.map(|a: SocketAddr| a.ip().to_string());
                let mut got = Vec::new();
                write_request_headers(&mut got, headers, ip.as_deref());
                assert_eq!(
                    String::from_utf8_lossy(&got),
                    String::from_utf8_lossy(&old_way(headers, client)),
                    "headers={:?} client={:?}",
                    headers,
                    client
                );
            }
        }
    }

    #[test]
    fn falls_back_when_connection_names_more_than_the_fixed_slots() {
        // 固定枠 (16) を超える指名でも、全部 hop-by-hop として落とすこと
        let mut names: Vec<String> = (0..20).map(|i| format!("X-H{}", i)).collect();
        let mut headers = vec![format!("Connection: {}\r\n", names.join(", "))];
        for n in &names {
            headers.push(format!("{}: v\r\n", n));
        }
        headers.push("Accept: */*\r\n".to_string());
        let mut got = Vec::new();
        write_request_headers(&mut got, &headers, None);
        let text = String::from_utf8_lossy(&got);
        names.retain(|n| text.contains(n.as_str()));
        assert!(names.is_empty(), "指名された名前が残っている: {:?}", names);
        assert!(text.contains("Accept: */*"));
        assert_eq!(String::from_utf8_lossy(&old_way(&headers, None)), text);
    }
}

#[cfg(test)]
mod write_response_tests {
    use super::*;

    fn old_way(
        head: &[u8],
        status_line: Option<&str>,
        drop_names: &[&str],
        extra: &[String],
    ) -> Vec<u8> {
        let mut h = sanitize_response_head(head);
        if let Some(sl) = status_line {
            h.status_line = sl.to_string();
        }
        h.lines.retain(|l| !line_named(l, drop_names));
        h.assemble(extra)
    }

    #[test]
    fn matches_the_previous_implementation_byte_for_byte() {
        let cases: Vec<&[u8]> = vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\n",
            b"HTTP/1.0 404 Not Found\r\nServer: x\r\nConnection: close\r\n\r\n",
            // Connection で独自の hop-by-hop を指名
            b"HTTP/1.1 200 OK\r\nConnection: X-Odd, Upgrade\r\nX-Odd: 1\r\nUpgrade: h2c\r\nA: b\r\n\r\n",
            // 枠組みのヘッダーは落ちる
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nAge: 5\r\nETag: \"x\"\r\n\r\n",
            // 空白の混ざり方と大文字小文字
            b"HTTP/1.1 301 Moved Permanently\r\n  \r\nlOcAtIoN:   /elsewhere  \r\nx:y\r\n\r\n",
            // 区切りの無い行
            b"HTTP/1.1 200 OK\r\nNoColon\r\nA: b\r\n\r\n",
            // CR 無し
            b"HTTP/1.1 204 No Content\nA: b\n\n",
        ];
        let extras = [
            Vec::new(),
            vec![
                "Content-Length: 5".to_string(),
                "Connection: keep-alive".to_string(),
            ],
        ];
        for head in cases {
            for extra in &extras {
                for sl in [None, Some("HTTP/1.1 206 Partial Content")] {
                    // 落とすヘッダーの指名あり / なし の両方で、組を作る版と一致すること
                    for drop_names in [&[] as &[&str], &["content-type"], &["A", "x"]] {
                        let mut got = Vec::new();
                        write_response_head(&mut got, head, sl, drop_names, extra);
                        assert_eq!(
                            String::from_utf8_lossy(&got),
                            String::from_utf8_lossy(&old_way(head, sl, drop_names, extra)),
                            "head={:?} sl={:?} drop={:?} extra={:?}",
                            String::from_utf8_lossy(head),
                            sl,
                            drop_names,
                            extra
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod response_view_tests {
    use super::*;

    /// 生の先頭を 1 行ずつ読む版が、以前の「読みながら組を作る」実装と同じ結果になること。
    fn old_way(head: &[u8]) -> Vec<(String, String)> {
        let text = String::from_utf8_lossy(head);
        let mut out = Vec::new();
        for line in text.split_inclusive('\n').skip(1) {
            if line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                out.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        out
    }

    #[test]
    fn response_pairs_matches_the_previous_implementation() {
        let cases: Vec<&[u8]> = vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\n",
            b"HTTP/1.0 404 Not Found\r\nServer: x\r\nConnection: close\r\n\r\n",
            b"HTTP/1.1 200 OK\nA:b\nNoColon\nC:  d  \n\n",
            b"HTTP/1.1 204 No Content\r\n\r\n",
            // 空行のあとに何かあっても読まない
            b"HTTP/1.1 200 OK\r\nA: b\r\n\r\nbody",
            // 終端の空行が無い (相手が途中で閉じた)
            b"HTTP/1.1 200 OK\r\nA: b\r\n",
        ];
        for head in cases {
            assert_eq!(
                response_pairs(head),
                old_way(head),
                "head={:?}",
                String::from_utf8_lossy(head)
            );
        }
    }

    #[test]
    fn response_value_and_names() {
        let head =
            b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: keep-alive, X-Odd\r\n\r\n";
        assert_eq!(response_value(head, "content-length"), Some("12"));
        assert_eq!(response_value(head, "CONTENT-LENGTH"), Some("12"));
        assert_eq!(response_value(head, "missing"), None);
        assert!(response_names(head, "connection", "x-odd"));
        assert!(!response_names(head, "connection", "close"));
    }
}
