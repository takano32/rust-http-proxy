//! 要求の受け入れの判定 (純粋な計算。ソケットも時計も触らない)。
//!
//! [`check`] が要求行とヘッダーの印から「断る理由」([`Rejection`]) を 1 つ決め、断らないなら
//! 要求行の 3 要素 ([`RequestLine`]) を返す。判定の順は今までの `handle_http_parsed` と同じで、
//! 複数の理由が重なる要求では先に見ていた方で断る。400 / 417 を書くのと数えるのは呼ぶ側
//! (`http` の本体) で、ここは判断だけなので単体で試せる。

use crate::body::Framing;
use crate::metrics::BadRequestReason;
use crate::request::RequestHeaders;

/// 要求行の 3 要素 (版が無ければ `HTTP/1.0` とみなす)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RequestLine<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub version: &'a str,
    /// `HTTP/1.1` を名乗ったか (keep-alive の既定と、1xx・TE の扱いが変わる)
    pub http11: bool,
}

/// 断る理由。それぞれの「なぜ」は [`check`] の判定のそばに書いてある。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Rejection<'a> {
    /// 折り返した (obs-fold) ヘッダー (T27.2)
    ObsFold,
    /// ヘッダーの形が不正 (T27.5)
    BadHeader,
    /// 要求行が空白で 2 つに割れない
    RequestLine,
    /// `Content-Length` の形が壊れている (T19.1)
    ContentLength,
    /// `Transfer-Encoding` の最後がちょうど 1 回の `chunked` でない、または HTTP/1.0 の要求の TE (T27.6)
    TransferEncoding { te_invalid: bool, version: &'a str },
    /// `Host` が 2 本以上 (T19.2)
    DupHost,
    /// `100-continue` 以外の `Expect` (T28.3)。これだけ 417
    ExpectUnknown,
    /// `Content-Length` と `Transfer-Encoding: chunked` が両方ある (T14.28)
    ChunkedAndLength,
    /// メソッドが token でない (T36.15。server の要求行の判定 (T34.142) と同じ)
    Method(&'a str),
}

impl Rejection<'_> {
    /// 返す状態コードと理由句。
    pub(super) fn status(&self) -> (u16, &'static str) {
        match self {
            Rejection::ExpectUnknown => (417, "Expectation Failed"),
            _ => (400, "Bad Request"),
        }
    }

    /// `rejected_requests` に数える理由。`Expect` の 417 は理由の表に無いので `None`
    /// (呼ぶ側が静的な絞りで WARN を出す)。
    pub(super) fn reason(&self, request_line: &str) -> Option<BadRequestReason> {
        Some(match self {
            Rejection::ObsFold => BadRequestReason::ObsFold,
            Rejection::BadHeader => BadRequestReason::BadHeader,
            Rejection::RequestLine => BadRequestReason::of_request_line(request_line),
            Rejection::ContentLength
            | Rejection::TransferEncoding { .. }
            | Rejection::ChunkedAndLength => BadRequestReason::BodyFraming,
            Rejection::DupHost => BadRequestReason::DupHost,
            Rejection::Method(_) => BadRequestReason::Method,
            Rejection::ExpectUnknown => return None,
        })
    }
}

/// 要求を受け入れるか。断るなら最初に当たった理由、通すなら要求行の 3 要素。
#[inline]
pub(super) fn check<'a>(
    req: &RequestHeaders,
    request_line: &'a str,
) -> Result<RequestLine<'a>, Rejection<'a>> {
    // RFC 9112 §5.2: 折り返した (obs-fold) ヘッダーは、中継する側が 400 で断るか
    // SP 1 個に置き換えるかを選べる。置き換えると値が黙って変わるので**断る** (T27.2)。
    // 継続行を捨てていた今までは、オリジンに `X-A: 1` だけが届いていた
    if req.obs_fold {
        return Err(Rejection::ObsFold);
    }
    // 名前とコロンの間の空白 (RFC 9112 §5.1) と値の CR / LF / NUL (RFC 9110 §5.5) も断る (T27.5)。
    // `Content-Length : 5` をこちらは長さとして使い、空白つきのまま転送していた。値の CR は
    // そのまま届き、CR を改行と読むオリジンには別のヘッダー (`Transfer-Encoding` …) に見えていた
    if req.bad_header {
        return Err(Rejection::BadHeader);
    }
    let mut parts = request_line.split_ascii_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        // 本体クレートが同じ判定を先にしているのでここへは普通は来ないが、数える口は
        // 400 を返す場所ごとに置く (T14.28)
        return Err(Rejection::RequestLine);
    };
    // `Content-Length` の形が壊れている要求は、本文の切れ目が決まらないので 400 で断る (T19.1)。
    // ここで断らないと、判定 (数字でない → 本文なし) と転送 (生の行をそのまま) が食い違い、
    // オリジンが本文を待ち続けるか、後段が別の長さで読む (要求の密輸)。**無言で閉じない**
    if req.content_length_invalid {
        return Err(Rejection::ContentLength);
    }
    let version = parts.next().unwrap_or("HTTP/1.0");
    let http11 = version == "HTTP/1.1";
    // `Transfer-Encoding` の最後がちょうど 1 回の `chunked` でない要求 (RFC 9112 §6.3) と、
    // HTTP/1.0 の要求の `Transfer-Encoding` (§6.1 は「枠組みが壊れているとして扱う」) は 400 (T27.6)。
    // 今までは `xchunked`・`gzip` を本文なしとしてヘッダーだけ転送し、本文はクライアントの
    // 次の要求として読まれていた (`xchunked` と `Content-Length` が両方あると CL で読んで両方を転送した)
    if req.te_invalid || (req.te_present && !http11) {
        return Err(Rejection::TransferEncoding {
            te_invalid: req.te_invalid,
            version,
        });
    }
    // `Host` が 2 本以上ある要求は RFC 9110 §7.2 が 400 を求めている (T19.2)。
    // どの `Host` を信じるかで前段と後段が食い違うと、宛先の付け替え (要求の密輸) になる。
    // **通る要求には 1 命令も足さない**: 印は 2 本目を見たときしか立たない
    if req.dup_host {
        return Err(Rejection::DupHost);
    }
    // `100-continue` 以外の `Expect` は満たせないので 417 (RFC 9110 §10.1.1。T28.3)。
    // 本文が続くかもしれないので閉じる。印は `Expect` の行があるときしか立たない
    if req.expect_unknown {
        return Err(Rejection::ExpectUnknown);
    }
    // `Content-Length` と `Transfer-Encoding: chunked` が両方ある要求は、前段と後段で
    // 本文の切れ目が食い違う (要求の密輸)。RFC 9112 §6.1 は「中継してはならない」と
    // 書いているので 400 で断る (T14.28)。**この比較が走るのは `chunked` が真のとき
    // だけ**なので、本文の無い要求と `Content-Length` だけの要求 (= 成功の経路) には
    // 分岐も比較も 1 つも増えない
    if req.chunked && req.content_length.is_some() {
        return Err(Rejection::ChunkedAndLength);
    }
    // メソッドは token だけ (RFC 9112 §3 の `method = token`。T36.15)。本体の入口 (server の
    // 要求行の判定。T34.142) が先に断るので、ここで当たるのは http を別の入口から呼んだときだけ。
    // 見るのは上の判定を全部通った要求だけなので、今まで断っていた要求の理由は変わらない
    if !proxy_base::ascii::is_token(method) {
        return Err(Rejection::Method(method));
    }
    Ok(RequestLine {
        method,
        target,
        version,
        http11,
    })
}

/// 要求の本文の枠組み ([`check`] を通った要求だけ)。
///
/// `Content-Length: 0` は本文が無いのと同じ (T29.5)。`Length(0)` のままだと「本文つきの GET」として
/// キャッシュを素通り (`cache BYPASS`) していた。行そのものは今までどおりオリジンへ転送する
/// (`raw_headers` から書くので、ここで畳んでも消えない)
#[inline]
pub(super) fn request_framing(req: &RequestHeaders) -> Framing {
    if req.chunked {
        Framing::Chunked
    } else {
        match req.content_length {
            None | Some(0) => Framing::None,
            Some(n) => Framing::Length(n),
        }
    }
}

/// 応答のあともクライアント接続を保つか。
///
/// この接続で処理する最後の要求 (`last`。T14.2) と keep-alive 0 秒の設定では保たない。
/// HTTP/1.1 は `Connection: close` が無ければ保ち、HTTP/1.0 は `keep-alive` を求めたときだけ保つ。
#[inline]
pub(super) fn keep_client(
    req: &RequestHeaders,
    http11: bool,
    last: bool,
    keepalive_off: bool,
) -> bool {
    !last
        && !keepalive_off
        && if http11 {
            !req.connection_close
        } else {
            req.connection_keep_alive
        }
}

/// `Expect: 100-continue` の本文つきの要求に、本文を読む前に `100 Continue` を 1 回返すか
/// (RFC 9110 §10.1.1。T28.3)。今までは自分でも返さず、オリジンの 100 も中継しないので、
/// 100 を厳密に待つクライアントは締め切りまで本文を送らず、こちらは本文を待って止まっていた。
/// HTTP/1.0 の要求の 100-continue は無視する (§10.1.1)
#[inline]
pub(super) fn wants_continue(req: &RequestHeaders, http11: bool, framing: Framing) -> bool {
    req.expect_continue && http11 && matches!(framing, Framing::Chunked | Framing::Length(1..))
}

/// `Max-Forwards` の行 (RFC 9110 §7.6.2。TRACE と OPTIONS だけが見る。T27.8)。
///
/// 戻り値は (行の添字, 読めた値)。無ければ `None`、値が読めないときは `Some((i, None))`
/// (呼ぶ側は今までどおり触らない)。
pub(super) fn max_forwards(raw_headers: &[String]) -> Option<(usize, Option<u64>)> {
    raw_headers.iter().enumerate().find_map(|(i, l)| {
        let (k, v) = proxy_base::ascii::split_once(l, b':')?;
        k.trim_ascii()
            .eq_ignore_ascii_case("max-forwards")
            .then(|| (i, v.trim_ascii().parse::<u64>().ok()))
    })
}

/// `Max-Forwards: N` (N > 0) の行を 1 減らした写し (そのほかの行はそのまま)。
pub(super) fn decrement_max_forwards(raw_headers: &[String], index: usize, n: u64) -> Vec<String> {
    let mut v = raw_headers.to_vec();
    v[index] = format!("Max-Forwards: {}\r\n", n - 1);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::parse_request_headers;

    fn parse(lines: &[&str]) -> RequestHeaders {
        let raw: Vec<String> = lines.iter().map(|l| format!("{}\r\n", l)).collect();
        parse_request_headers(&raw, 0)
    }

    /// ふつうの要求は通り、要求行の 3 要素と版の判定が取れる。版が無ければ HTTP/1.0。
    #[test]
    fn a_plain_request_is_admitted() {
        let req = parse(&["Host: h", "Accept: */*"]);
        let line = check(&req, "GET http://h/p HTTP/1.1\r\n").unwrap();
        assert_eq!(
            line,
            RequestLine {
                method: "GET",
                target: "http://h/p",
                version: "HTTP/1.1",
                http11: true
            }
        );
        let line = check(&req, "GET /p\r\n").unwrap();
        assert_eq!((line.version, line.http11), ("HTTP/1.0", false));
        let line = check(&req, "GET /p HTTP/1.0").unwrap();
        assert_eq!((line.version, line.http11), ("HTTP/1.0", false));
    }

    /// 断る理由 1 つずつと、その状態コード・数える理由。
    #[test]
    fn each_rejection_has_its_status_and_reason() {
        let cases: Vec<(
            RequestHeaders,
            &str,
            Rejection<'static>,
            u16,
            Option<BadRequestReason>,
        )> = vec![
            (
                parse(&["Host: h", " folded"]),
                "GET /p HTTP/1.1",
                Rejection::ObsFold,
                400,
                Some(BadRequestReason::ObsFold),
            ),
            (
                parse(&["Host : h"]),
                "GET /p HTTP/1.1",
                Rejection::BadHeader,
                400,
                Some(BadRequestReason::BadHeader),
            ),
            (
                parse(&["Host: h"]),
                "GET",
                Rejection::RequestLine,
                400,
                Some(BadRequestReason::RequestLine),
            ),
            (
                parse(&["Host: h", "Content-Length: x"]),
                "POST /p HTTP/1.1",
                Rejection::ContentLength,
                400,
                Some(BadRequestReason::BodyFraming),
            ),
            (
                parse(&["Host: h", "Transfer-Encoding: gzip"]),
                "POST /p HTTP/1.1",
                Rejection::TransferEncoding {
                    te_invalid: true,
                    version: "HTTP/1.1",
                },
                400,
                Some(BadRequestReason::BodyFraming),
            ),
            (
                parse(&["Host: h", "Transfer-Encoding: chunked"]),
                "POST /p HTTP/1.0",
                Rejection::TransferEncoding {
                    te_invalid: false,
                    version: "HTTP/1.0",
                },
                400,
                Some(BadRequestReason::BodyFraming),
            ),
            (
                parse(&["Host: a", "Host: b"]),
                "GET /p HTTP/1.1",
                Rejection::DupHost,
                400,
                Some(BadRequestReason::DupHost),
            ),
            (
                parse(&["Host: h", "Expect: fancy"]),
                "POST /p HTTP/1.1",
                Rejection::ExpectUnknown,
                417,
                None,
            ),
            (
                parse(&["Host: h", "Transfer-Encoding: chunked", "Content-Length: 5"]),
                "POST /p HTTP/1.1",
                Rejection::ChunkedAndLength,
                400,
                Some(BadRequestReason::BodyFraming),
            ),
            (
                parse(&["Host: h"]),
                "GE@T /p HTTP/1.1",
                Rejection::Method("GE@T"),
                400,
                Some(BadRequestReason::Method),
            ),
        ];
        for (req, line, want, status, reason) in cases {
            let got = check(&req, line).unwrap_err();
            assert_eq!(got, want, "{}", line);
            assert_eq!(got.status().0, status, "{:?}", want);
            assert_eq!(got.reason(line), reason, "{:?}", want);
        }
        // 要求行がゴミならメソッドの理由 (T14.28)
        assert_eq!(
            Rejection::RequestLine.reason("\u{1}\u{2}"),
            Some(BadRequestReason::Method)
        );
    }

    /// 理由が重なる要求は、今までの順で先に見ていた方で断る。
    #[test]
    fn the_first_reason_in_the_old_order_wins() {
        // obs-fold は壊れたヘッダーより先、壊れた CL は壊れた要求行より後
        let req = parse(&["Host : h", " folded", "Content-Length: x"]);
        assert_eq!(check(&req, "GET").unwrap_err(), Rejection::ObsFold);
        let req = parse(&["Host : h", "Content-Length: x"]);
        assert_eq!(check(&req, "GET").unwrap_err(), Rejection::BadHeader);
        let req = parse(&["Host: h", "Content-Length: x"]);
        assert_eq!(check(&req, "GET").unwrap_err(), Rejection::RequestLine);
        // 壊れた CL は TE の誤りより先、TE は 2 本目の Host より先、Host は Expect より先
        let req = parse(&[
            "Host: a",
            "Host: b",
            "Content-Length: x",
            "Transfer-Encoding: gzip",
        ]);
        assert_eq!(
            check(&req, "POST /p HTTP/1.1").unwrap_err(),
            Rejection::ContentLength
        );
        let req = parse(&["Host: a", "Host: b", "Transfer-Encoding: gzip", "Expect: x"]);
        assert!(matches!(
            check(&req, "POST /p HTTP/1.1").unwrap_err(),
            Rejection::TransferEncoding {
                te_invalid: true,
                ..
            }
        ));
        let req = parse(&["Host: a", "Host: b", "Expect: x"]);
        assert_eq!(
            check(&req, "POST /p HTTP/1.1").unwrap_err(),
            Rejection::DupHost
        );
        let req = parse(&[
            "Host: a",
            "Expect: x",
            "Transfer-Encoding: chunked",
            "Content-Length: 5",
        ]);
        assert_eq!(
            check(&req, "POST /p HTTP/1.1").unwrap_err(),
            Rejection::ExpectUnknown
        );
        // token でないメソッドは最後 (ほかの理由がある要求はそちらで断る。T36.15)
        let req = parse(&["Host: h", "Content-Length: x"]);
        assert_eq!(
            check(&req, "GE@T /p HTTP/1.1").unwrap_err(),
            Rejection::ContentLength
        );
    }

    /// token でないメソッドは断る (T36.15。server が手前で断るので、http を別の入口から
    /// 呼んだときだけ当たる)。NBSP は ASCII の空白ではないのでメソッドの中に残る。
    #[test]
    fn a_method_that_is_not_a_token_is_rejected() {
        let req = parse(&["Host: h"]);
        for (line, method) in [
            ("GE@T /p HTTP/1.1", "GE@T"),
            ("G\u{a0}ET /p HTTP/1.1", "G\u{a0}ET"),
            ("GET\u{1} /p HTTP/1.1", "GET\u{1}"),
            ("G(ET) /p HTTP/1.1", "G(ET)"),
        ] {
            assert_eq!(
                check(&req, line).unwrap_err(),
                Rejection::Method(method),
                "{line:?}"
            );
        }
        // token の記号は通す
        for line in ["M-SEARCH * HTTP/1.1", "X_Y.Z~!#$%&'*+^`| /p HTTP/1.1"] {
            assert!(check(&req, line).is_ok(), "{line:?}");
        }
    }

    /// 本文の枠組み: chunked が勝ち、`Content-Length: 0` は本文なし (T29.5)。
    #[test]
    fn framing_of_the_request_body() {
        assert_eq!(request_framing(&parse(&["Host: h"])), Framing::None);
        assert_eq!(
            request_framing(&parse(&["Content-Length: 0"])),
            Framing::None
        );
        assert_eq!(
            request_framing(&parse(&["Content-Length: 5"])),
            Framing::Length(5)
        );
        assert_eq!(
            request_framing(&parse(&["Transfer-Encoding: chunked"])),
            Framing::Chunked
        );
    }

    /// keep-alive: 最後の要求と keep-alive 0 秒では保たない。1.1 は close が無ければ、1.0 は keep-alive を求めたときだけ。
    #[test]
    fn keep_client_follows_the_version_and_the_limits() {
        let plain = parse(&["Host: h"]);
        assert!(keep_client(&plain, true, false, false));
        assert!(!keep_client(&plain, false, false, false));
        assert!(!keep_client(&plain, true, true, false));
        assert!(!keep_client(&plain, true, false, true));
        let close = parse(&["Connection: close"]);
        assert!(!keep_client(&close, true, false, false));
        let ka = parse(&["Connection: keep-alive"]);
        assert!(keep_client(&ka, false, false, false));
        let pka = parse(&["Proxy-Connection: Keep-Alive"]);
        assert!(keep_client(&pka, false, false, false));
    }

    /// `100 Continue` を返すのは、HTTP/1.1 の `Expect: 100-continue` で本文が 1 バイト以上あるときだけ。
    #[test]
    fn continue_only_for_http11_with_a_body() {
        let req = parse(&["Expect: 100-continue"]);
        assert!(wants_continue(&req, true, Framing::Length(1)));
        assert!(wants_continue(&req, true, Framing::Chunked));
        assert!(!wants_continue(&req, true, Framing::Length(0)));
        assert!(!wants_continue(&req, true, Framing::None));
        assert!(!wants_continue(&req, false, Framing::Length(1)));
        assert!(!wants_continue(
            &parse(&["Host: h"]),
            true,
            Framing::Length(1)
        ));
    }

    /// `Max-Forwards` の行を探し、1 減らした写しを作る (ほかの行はそのまま)。
    #[test]
    fn max_forwards_is_found_and_decremented() {
        let raw: Vec<String> = ["Host: h\r\n", "max-forwards:  2 \r\n", "A: b\r\n"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(max_forwards(&raw), Some((1, Some(2))));
        let v = decrement_max_forwards(&raw, 1, 2);
        assert_eq!(v, vec!["Host: h\r\n", "Max-Forwards: 1\r\n", "A: b\r\n"]);
        assert_eq!(max_forwards(&["Host: h\r\n".to_string()]), None);
        assert_eq!(
            max_forwards(&["Max-Forwards: x\r\n".to_string()]),
            Some((0, None))
        );
        assert_eq!(
            max_forwards(&["Max-Forwards: 0\r\n".to_string()]),
            Some((0, Some(0)))
        );
    }
}
