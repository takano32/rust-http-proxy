use std::net::SocketAddr;

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
    pub fn assemble(&self, extra: &[String]) -> Vec<u8> {
        let mut out = String::with_capacity(256);
        out.push_str(&self.status_line);
        out.push_str("\r\n");
        for line in self.lines.iter().chain(extra.iter()) {
            out.push_str(line);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        out.into_bytes()
    }
}

/// レスポンスヘッダーのうち、プロキシが自分で決め直すもの (枠組み・経過時間)。
const FRAMING_HEADERS: &[&str] = &["transfer-encoding", "content-length", "age"];

pub fn sanitize_response_head(head: &[u8]) -> ResponseHead {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split('\n').map(|l| l.trim_end_matches('\r'));
    let status_line = lines.next().unwrap_or("").trim();
    let mut parts = status_line.splitn(2, ' ');
    let _version = parts.next();
    let rest = parts.next().unwrap_or("200 OK").trim();
    let mut out = ResponseHead {
        status_line: format!("HTTP/1.1 {}", rest),
        lines: Vec::new(),
    };
    let raw: Vec<&str> = lines.filter(|l| !l.trim().is_empty()).collect();
    let mut custom_hop: Vec<String> = Vec::new();
    for line in &raw {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("connection")
        {
            custom_hop.extend(v.split(',').map(|t| t.trim().to_ascii_lowercase()));
        }
    }
    for line in raw {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let lower = k.trim().to_ascii_lowercase();
        if is_hop_by_hop(&lower)
            || custom_hop.contains(&lower)
            || FRAMING_HEADERS.contains(&lower.as_str())
        {
            continue;
        }
        out.lines.push(format!("{}: {}", k.trim(), v.trim()));
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
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("connection")
        {
            for item in v.split(',') {
                custom_hop_by_hop.push(item.trim().to_ascii_lowercase());
            }
        }
    }

    // Second pass: filter and collect
    for line in headers {
        if let Some((k, v)) = line.split_once(':') {
            let k_trim = k.trim();
            let k_lower = k_trim.to_ascii_lowercase();

            if is_hop_by_hop(&k_lower) || custom_hop_by_hop.contains(&k_lower) {
                continue;
            }

            if k_lower == "x-forwarded-for" {
                x_forwarded_for = Some(v.trim().to_string());
                continue;
            }

            if k_lower == "via" {
                has_via = true;
                let new_via = format!("{}: {}, 1.1 sorahost-http-proxy\r\n", k_trim, v.trim());
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
        out.push("Via: 1.1 sorahost-http-proxy\r\n".to_string());
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
        assert!(
            cleaned
                .iter()
                .any(|h| h.starts_with("Via: 1.1 sorahost-http-proxy"))
        );

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
