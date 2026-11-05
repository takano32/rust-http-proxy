//! 口 1 つが返すもの ([`Reply`]) と、それを 1 本の HTTP 応答のバイト列に組むところ。
//! ソケットは持たない (書くのは `super::handle`) ので、応答のバイト列をそのまま試験で縛れる。

use crate::metrics;

/// 口 1 つが返すもの (状態・`Content-Type`・本文)。
pub(super) struct Reply {
    pub(super) status: u16,
    pub(super) content_type: &'static str,
    pub(super) body: String,
    /// 405 に添える使えるメソッド (RFC 9110 §15.5.6。T28.7 の 2 つの枝だけが入れる。
    /// 読み取り専用で断った書き換え (T14.18) の 405 は今までどおり何も足さない)
    pub(super) allow: &'static str,
}

impl From<(u16, &'static str, String)> for Reply {
    fn from((status, content_type, body): (u16, &'static str, String)) -> Reply {
        Reply {
            status,
            content_type,
            body,
            allow: "",
        }
    }
}

impl Reply {
    /// `{"schema":N,"error":"<message>"}` の JSON (`message` は逃がし済みであること)。
    pub(super) fn json_error(status: u16, message: &str) -> Reply {
        (
            status,
            "application/json",
            format!("{}\"error\":\"{}\"}}", metrics::SCHEMA_HEAD, message),
        )
            .into()
    }

    /// `Allow:` の行を添える (405 の 2 つの枝。T28.7)。
    pub(super) fn with_allow(mut self, allow: &'static str) -> Reply {
        self.allow = allow;
        self
    }
}

/// 状態コードの理由句。
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        // `/healthz` の **`fatal` な**検査が 1 つでも偽 (T14.12 / T15.0 (6))、
        // または重い口が 1 本走っている (T14.51)
        503 => "Service Unavailable",
        _ => "OK",
    }
}

/// 応答 1 本のバイト列。常に `Cache-Control: no-store` と `Connection: close`。
///
/// `heavy_busy` は重い口を断った 503 (T14.51)、`head` は HEAD (GET と同じヘッダーで本文だけ書かない)。
pub(super) fn render(reply: &Reply, heavy_busy: bool, head: bool) -> String {
    // 204 は `Content-Type` も `Content-Length` も送らない (RFC 9110 §8.6・§15.3.5)
    let framing = if reply.status == 204 {
        String::new()
    } else {
        format!(
            "Content-Type: {}\r\nContent-Length: {}\r\n",
            reply.content_type,
            reply.body.len()
        )
    };
    format!(
        "HTTP/1.1 {} {}\r\n{}{}{}Cache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        reply.status,
        reason_phrase(reply.status),
        framing,
        reply.allow,
        // 重い口を断ったときだけ「1 秒後に」 (T14.51)。`/healthz` の 503 には付けない
        // (あちらは「治るまで待て」ではなく「この検査が偽」なので、目安の秒が出せない)
        if heavy_busy { "Retry-After: 1\r\n" } else { "" },
        // HEAD は GET と同じヘッダー (`Content-Length` も GET の本文の長さ) で本文だけ書かない
        if head { "" } else { reply.body.as_str() }
    )
}

#[cfg(test)]
mod tests {
    use super::{Reply, reason_phrase, render};
    use crate::metrics::SCHEMA_HEAD;

    fn reply(status: u16, content_type: &'static str, body: &str) -> Reply {
        (status, content_type, body.to_string()).into()
    }

    /// 応答の形: 状態行・`Content-Type`・`Content-Length`・`no-store`・`close`・空行・本文。
    #[test]
    fn a_get_response_has_the_fixed_header_order() {
        let out = render(&reply(200, "application/json", "{\"a\":1}"), false, false);
        assert_eq!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\
             Cache-Control: no-store\r\nConnection: close\r\n\r\n{\"a\":1}"
        );
        let out = render(
            &reply(404, "text/plain; charset=utf-8", "not found.\n"),
            false,
            false,
        );
        assert!(out.starts_with("HTTP/1.1 404 Not Found\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 11\r\n"));
        assert!(out.ends_with("\r\n\r\nnot found.\n"));
    }

    /// HEAD は GET と同じヘッダー (`Content-Length` も GET の本文の長さ) で本文だけ書かない (T28.7)。
    #[test]
    fn head_keeps_the_headers_and_drops_the_body() {
        let r = reply(200, "text/html; charset=utf-8", "<html>page</html>");
        let get = render(&r, false, false);
        let head = render(&r, false, true);
        assert!(head.contains("Content-Length: 17\r\n"));
        assert!(head.ends_with("Connection: close\r\n\r\n"));
        assert_eq!(get, format!("{}<html>page</html>", head));
    }

    /// 204 は `Content-Type` も `Content-Length` も送らない (RFC 9110 §8.6・§15.3.5)。
    #[test]
    fn no_content_has_no_framing_headers() {
        let out = render(&reply(204, "", ""), false, false);
        assert_eq!(
            out,
            "HTTP/1.1 204 No Content\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
        );
    }

    /// 重い口を断った 503 だけ `Retry-After: 1` が付く (T14.51)。`/healthz` の 503 には付かない。
    #[test]
    fn only_the_busy_503_carries_retry_after() {
        let busy = Reply::json_error(503, "busy");
        let out = render(&busy, true, false);
        assert_eq!(
            out,
            format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Retry-After: 1\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}\"error\":\"busy\"}}",
                busy.body.len(),
                SCHEMA_HEAD
            )
        );
        let unhealthy = render(&reply(503, "application/json", "{}"), false, false);
        assert!(!unhealthy.contains("Retry-After"));
    }

    /// 405 の `Allow:` は `Content-Length` の次・`Cache-Control` の前 (T28.7)。
    #[test]
    fn the_allow_header_sits_between_the_framing_and_cache_control() {
        let r = Reply::json_error(405, "method not allowed").with_allow("Allow: GET, HEAD\r\n");
        let out = render(&r, false, false);
        assert!(
            out.contains("\r\nContent-Length: ")
                && out.contains("\r\nAllow: GET, HEAD\r\nCache-Control: no-store\r\n"),
            "{}",
            out
        );
        assert!(out.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));
        // 読み取り専用で断った 405 (T14.18) は何も足さない
        let plain = render(&Reply::json_error(405, "read-only"), false, false);
        assert!(!plain.contains("Allow:"));
    }

    /// 理由句の表 (知らない状態は `OK` のまま = 今までどおり)。
    #[test]
    fn reason_phrases_match_the_status_codes() {
        assert_eq!(reason_phrase(200), "OK");
        assert_eq!(reason_phrase(204), "No Content");
        assert_eq!(reason_phrase(400), "Bad Request");
        assert_eq!(reason_phrase(404), "Not Found");
        assert_eq!(reason_phrase(405), "Method Not Allowed");
        assert_eq!(reason_phrase(503), "Service Unavailable");
        assert_eq!(reason_phrase(418), "OK");
    }

    /// `json_error` は先頭に版 (T14.49) を置き、`{"schema":N,"error":"…"}` の形。
    #[test]
    fn json_errors_start_with_the_schema() {
        let r = Reply::json_error(400, "use /lookup?url=<url>");
        assert_eq!(
            (r.status, r.content_type, r.allow),
            (400, "application/json", "")
        );
        assert_eq!(
            r.body,
            format!("{}\"error\":\"use /lookup?url=<url>\"}}", SCHEMA_HEAD)
        );
    }
}
