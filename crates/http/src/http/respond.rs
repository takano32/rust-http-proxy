//! プロキシが自分で書く応答: 誤り (4xx / 5xx)、`Max-Forwards: 0` への答え、1xx の中継。

use std::io::{self, Write};

use crate::headers;
use crate::log_trace;

/// 自前の誤りの応答。T29.9 から `Date` も付ける (RFC 9110 §6.6.1)。
pub(super) fn write_error(client: &mut impl Write, status: u16, reason: &str) -> io::Result<()> {
    let mut head = [0u8; crate::httpdate::ERROR_HEAD_MAX];
    client.write_all(crate::httpdate::error_head(status, reason, &mut head))?;
    client.flush()
}

/// `Max-Forwards: 0` の TRACE / OPTIONS に自分で応える (RFC 9110 §7.6.2・§9.3.7・§9.3.8。T27.8)。
///
/// OPTIONS は `200` で本文なし。TRACE は `200` + `Content-Type: message/http` で、受け取った要求行と
/// ヘッダーをそのまま返す。ただし `Cookie`・`Authorization`・`Proxy-Authorization` の行は返さない
/// (§9.3.8 は「機微な値を含む行を除いてよい」。TRACE で認証情報を読み出す手口を塞ぐ)。返したら閉じる
pub(super) fn write_max_forwards_zero(
    client: &mut impl Write,
    trace: bool,
    request_line: &str,
    raw_headers: &[String],
) -> io::Result<()> {
    let body = if trace {
        trace_body(request_line, raw_headers)
    } else {
        Vec::new()
    };
    let mut date = [0u8; 35];
    let head = format!(
        "HTTP/1.1 200 OK\r\n{}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
        crate::httpdate::date_line(&mut date),
        if trace {
            "Content-Type: message/http\r\n"
        } else {
            ""
        },
        body.len()
    );
    client.write_all(head.as_bytes())?;
    client.write_all(&body)?;
    client.flush()
}

/// TRACE の本文: 受け取った要求行とヘッダー (機微な行を除く) をそのまま。
fn trace_body(request_line: &str, raw_headers: &[String]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(request_line.trim_end().as_bytes());
    body.extend_from_slice(b"\r\n");
    for line in raw_headers {
        let name = proxy_base::ascii::split_once(line, b':').map_or("", |(k, _)| k.trim_ascii());
        if ["cookie", "authorization", "proxy-authorization"]
            .iter()
            .any(|n| name.eq_ignore_ascii_case(n))
        {
            continue;
        }
        body.extend_from_slice(line.trim_end().as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"\r\n");
    body
}

/// 中間応答 (1xx) をクライアントへ中継する。**ヘッダー部だけで、本文は持たない** (T19.6)。
///
/// RFC 9110 §15.2 は、自分が起こした要求でない限り 1xx を中継することを求めている。
/// 整形は最終応答と同じ [`headers::write_response_head`] を通す — hop-by-hop
/// (`Connection` が名指ししたものを含む) と枠組みのヘッダーを落とし、obs-fold は SP 1 個に
/// 畳む。足す行は無い (`Content-Length` / `Connection` は 1xx に付けない)。
///
/// **キャッシュには載せない**し、`/recent` とホスト別の統計にも数えない (数えるのは
/// 最終応答の 1 本だけ)。
pub(super) fn forward_interim(
    client: &mut impl Write,
    head: &[u8],
    status: u16,
    conn_id: usize,
) -> io::Result<()> {
    let mut out = Vec::with_capacity(head.len() + 16);
    headers::write_response_head(&mut out, head, None, &[], &[] as &[&str]);
    client.write_all(&out)?;
    // 最終応答を読み終えるまで抱えない: クライアントは 103 を見てから先の資源を取りに行く
    client.flush()?;
    log_trace!(
        Some(conn_id),
        "forwarded interim {} response ({}B)",
        status,
        out.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TRACE の本文は要求行とヘッダーをそのまま返すが、`Cookie` / `Authorization` /
    /// `Proxy-Authorization` の行は返さない (§9.3.8)。
    #[test]
    fn trace_body_echoes_the_request_without_credentials() {
        let raw: Vec<String> = [
            "Host: h\r\n",
            "Cookie: a=b\r\n",
            "authorization: Basic x\r\n",
            "Proxy-Authorization: Basic y\r\n",
            "Max-Forwards: 0\r\n",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let body = trace_body("TRACE http://h/ HTTP/1.1\r\n", &raw);
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "TRACE http://h/ HTTP/1.1\r\nHost: h\r\nMax-Forwards: 0\r\n\r\n"
        );
    }

    /// OPTIONS は本文なしの 200、TRACE は `message/http` の本文つき。どちらも `Connection: close`。
    #[test]
    fn max_forwards_zero_answers_differ_by_method() {
        let raw = vec!["Host: h\r\n".to_string()];
        let mut out = Vec::new();
        write_max_forwards_zero(&mut out, false, "OPTIONS http://h/ HTTP/1.1\r\n", &raw).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\nDate: "), "{}", text);
        assert!(
            text.ends_with("Content-Length: 0\r\nConnection: close\r\n\r\n"),
            "{}",
            text
        );
        assert!(!text.contains("Content-Type"));
        let mut out = Vec::new();
        write_max_forwards_zero(&mut out, true, "TRACE http://h/ HTTP/1.1\r\n", &raw).unwrap();
        let text = String::from_utf8(out).unwrap();
        let body = "TRACE http://h/ HTTP/1.1\r\nHost: h\r\n\r\n";
        assert!(text.contains("Content-Type: message/http\r\n"), "{}", text);
        assert!(
            text.contains(&format!(
                "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )),
            "{}",
            text
        );
    }

    /// 自前の誤りの応答の形 (T29.9 から `Date` つき)。
    #[test]
    fn error_responses_carry_date_and_close() {
        let mut out = Vec::new();
        write_error(&mut out, 502, "Bad Gateway").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.starts_with("HTTP/1.1 502 Bad Gateway\r\nDate: "),
            "{}",
            text
        );
        assert!(
            text.ends_with("\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
            "{}",
            text
        );
    }
}
