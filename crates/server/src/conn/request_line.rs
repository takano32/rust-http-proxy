//! 要求行の文法 ([`check_request_line`])。ソケットも統計も見ない純粋な判定で、
//! 断るときの理由・状態・WARN の文面は [`RequestLineError`] から呼ぶ側が組む (T.34.11)。

/// 文法を通った要求行 (どれも要求行の借用)。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct RequestLine<'a> {
    pub(super) method: &'a str,
    pub(super) target: &'a str,
    /// `HTTP/1.x` (形も版も確かめてある)
    pub(super) version: &'a str,
}

/// 要求行を断る理由 (判定の順に並べてある)。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RequestLineError<'a> {
    /// メソッドか対象が無い (空白だけの行も含む)。400
    Malformed,
    /// ちょうど 3 要素でない・版が `HTTP/d.d` の形でない・対象に VCHAR 以外がある。400
    Grammar,
    /// 版が `HTTP/1.x` でない (`HTTP/9.9`・HTTP/2 の preface `PRI * HTTP/2.0`)。505
    Version(&'a str),
    /// `*` (asterisk-form) を `OPTIONS` 以外のメソッドで使った。400。中身はメソッド
    Asterisk(&'a str),
    /// メソッドが token (RFC 9110 §5.6.2・RFC 9112 §3) でない (`G\u{a0}ET`・`GE@T`)。400。
    /// 中身はメソッド (T.34.142)
    Method(&'a str),
}

impl RequestLineError<'_> {
    /// 返す状態コード。
    pub(super) fn status(&self) -> u16 {
        match self {
            RequestLineError::Version(_) => 505,
            _ => 400,
        }
    }
}

/// 要求行の文法を確かめる (RFC 9112 §3)。
#[inline]
pub(super) fn check_request_line(line: &str) -> Result<RequestLine<'_>, RequestLineError<'_>> {
    let mut parts = line.split_ascii_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(RequestLineError::Malformed);
    };
    // 要求行の文法 (RFC 9112 §3。T.29.8): **ちょうど 3 要素** (メソッド・対象・版) で、対象は VCHAR だけ、
    // 版は `HTTP/d.d` の形。外れたら 400。今までは `GET http://h/a b HTTP/1.1` の `b` を版と読んで 505、
    // `… HTTP/1.1 junk` と版の無い行 (HTTP/0.9 の形) を 1.0 として受け付け、対象の DEL / NUL / 制御文字を
    // そのまま転送していた。区切りの空白の数は今までどおり問わない
    let version = parts.next();
    if parts.next().is_some()
        || !version.is_some_and(version_syntax_ok)
        || !target.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(RequestLineError::Grammar);
    }
    // 上で `version` が `Some` であることは確かめてある
    let version = version.unwrap_or_default();
    // 版が `HTTP/1.x` でない要求 (`HTTP/9.9`・HTTP/2 の preface `PRI * HTTP/2.0`) は 505 (T27.8)。
    // 今までは `HTTP/9.9` を 1.1 として転送し、preface は `*` を宛先として引きに行って 502 だった
    if !version.get(..7).is_some_and(|v| v == "HTTP/1.") {
        return Err(RequestLineError::Version(version));
    }
    // `*` (asterisk-form) は `OPTIONS` だけ (RFC 9112 §3.2.4。T.29.9)。今までは `*` を宛先として引いて 502
    if target == "*" && method != "OPTIONS" {
        return Err(RequestLineError::Asterisk(method));
    }
    // メソッドは token だけ (RFC 9112 §3 の `method = token`。T.34.142)。今までは形しか見ず、
    // `G\u{a0}ET /x HTTP/1.1` (区切りは ASCII の空白だけなので NBSP はメソッドの中に残る) や
    // `GE@T` をそのままオリジンへ送っていた。見るのは上の判定を全部通った行だけなので、
    // 今まで断っていた行の理由は変わらない
    if !proxy_base::ascii::is_token(method) {
        return Err(RequestLineError::Method(method));
    }
    Ok(RequestLine {
        method,
        target,
        version,
    })
}

/// 版が `HTTP/d.d` の形か (RFC 9112 §2.3。T.29.8)。`HTTP` は大小を区別する (`HTTP-name = %s"HTTP"`。
/// T.32.15 (6)。T.29.8 では問わずに `http/1.1` を通していた)。
/// 形が合っていて `1.x` でないもの (`HTTP/2.0`・`HTTP/9.9`) は呼ぶ側が 505 にする。
#[inline]
fn version_syntax_ok(v: &str) -> bool {
    let b = v.as_bytes();
    b.len() == 8
        && &b[..5] == b"HTTP/"
        && b[5].is_ascii_digit()
        && b[6] == b'.'
        && b[7].is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_syntax() {
        for ok in ["HTTP/1.1", "HTTP/1.0", "HTTP/9.9", "HTTP/2.0"] {
            assert!(version_syntax_ok(ok), "{}", ok);
        }
        for bad in [
            "HTTP/1.1junk",
            "HTTP/1",
            "HTTP/11.1",
            "b",
            "",
            "HTTP/1.x",
            "XTTP/1.1",
            "http/1.1",
            "Http/1.0",
        ] {
            assert!(!version_syntax_ok(bad), "{}", bad);
        }
    }

    #[test]
    fn a_well_formed_line_is_split_into_three() {
        let ok = check_request_line("GET http://h/a HTTP/1.1\r\n");
        assert_eq!(
            ok,
            Ok(RequestLine {
                method: "GET",
                target: "http://h/a",
                version: "HTTP/1.1"
            })
        );
        // 区切りの空白の数は問わない。`OPTIONS *` は通す
        assert_eq!(
            check_request_line("OPTIONS  *   HTTP/1.0\r\n").map(|l| (l.method, l.target)),
            Ok(("OPTIONS", "*"))
        );
    }

    #[test]
    fn a_line_without_a_method_or_target_is_malformed() {
        for line in ["GET\r\n", "   \r\n", ""] {
            assert_eq!(
                check_request_line(line),
                Err(RequestLineError::Malformed),
                "{line:?}"
            );
        }
    }

    #[test]
    fn extra_words_a_missing_version_and_control_bytes_break_the_grammar() {
        for line in [
            // 4 要素 (対象に空白が混ざった形)
            "GET http://h/a b HTTP/1.1\r\n",
            "GET / HTTP/1.1 junk\r\n",
            // 版の無い行 (HTTP/0.9 の形)
            "GET /\r\n",
            // 版の形が違う (`HTTP` は大小を区別する)
            "GET / http/1.1\r\n",
            // 対象の DEL・制御文字
            "GET /a\u{7f} HTTP/1.1\r\n",
            "GET /a\u{1}b HTTP/1.1\r\n",
        ] {
            let e = check_request_line(line).unwrap_err();
            assert_eq!(e, RequestLineError::Grammar, "{line:?}");
            assert_eq!(e.status(), 400);
        }
    }

    #[test]
    fn a_version_other_than_1_x_is_505() {
        let e = check_request_line("PRI * HTTP/2.0\r\n").unwrap_err();
        assert_eq!(e, RequestLineError::Version("HTTP/2.0"));
        assert_eq!(e.status(), 505);
        assert_eq!(
            check_request_line("GET / HTTP/9.9\r\n"),
            Err(RequestLineError::Version("HTTP/9.9"))
        );
    }

    #[test]
    fn the_asterisk_form_is_only_for_options() {
        let e = check_request_line("GET * HTTP/1.1\r\n").unwrap_err();
        assert_eq!(e, RequestLineError::Asterisk("GET"));
        assert_eq!(e.status(), 400);
    }

    /// T.34.142: token でないメソッドは 400 (`method`)。token の字だけのメソッドは知らなくても通す。
    #[test]
    fn a_method_that_is_not_a_token_is_refused() {
        for (line, method) in [
            ("G\u{a0}ET /x HTTP/1.1\r\n", "G\u{a0}ET"),
            ("GE@T http://h/ HTTP/1.1\r\n", "GE@T"),
            ("GET\u{1} / HTTP/1.1\r\n", "GET\u{1}"),
        ] {
            let e = check_request_line(line).unwrap_err();
            assert_eq!(e, RequestLineError::Method(method), "{line:?}");
            assert_eq!(e.status(), 400);
        }
        assert!(check_request_line("X~CUSTOM! /x HTTP/1.1\r\n").is_ok());
        // 今まで断っていた行は、今までの理由のまま (メソッドの判定はいちばん後)
        assert_eq!(
            check_request_line("GE@T * HTTP/1.1\r\n"),
            Err(RequestLineError::Asterisk("GE@T"))
        );
        assert_eq!(
            check_request_line("GE@T / HTTP/2.0\r\n"),
            Err(RequestLineError::Version("HTTP/2.0"))
        );
    }

    #[test]
    fn the_checks_run_in_the_order_of_the_old_code() {
        // 文法の誤りは版より先 (4 要素で版が 2.0 でも 400)、版は `*` より先 (505)
        assert_eq!(
            check_request_line("GET * HTTP/2.0 x\r\n"),
            Err(RequestLineError::Grammar)
        );
        assert_eq!(
            check_request_line("GET * HTTP/2.0\r\n"),
            Err(RequestLineError::Version("HTTP/2.0"))
        );
    }
}
