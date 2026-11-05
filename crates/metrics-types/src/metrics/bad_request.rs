//! 要求を読めずに断った理由 ([`BadRequestReason`]。T14.28)。**断る経路からだけ**通る。

/// 要求を読めずに断った理由 (T14.28 / T19.2 / T27.5 / T.29.10)。**10 種で固定** (足すときは末尾に)。
///
/// 公開ポートには走査 (scanner) の要求が来る。今までは 400 / 414 / 431 で閉じるだけで
/// **何が来たか**の数が無かったので、`/status` の `rejected_requests` (理由別) と
/// `/errors` の個票 (`cause` が `bad_request:<reason>`) に残す。
///
/// [`ErrCause`](super::ErrCause) にも [`BlockCause`](super::BlockCause) にも足さないのは
/// [`BlockCause`](super::BlockCause) と同じ理由
/// (`errors_by_cause` を伸ばすと `.rrd` の標本が領域に収まらず、版を上げて統計を捨てることになる)。
/// **数えるのは断る経路だけ**なので、通した要求には 1 命令も足していない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum BadRequestReason {
    /// 要求行が読めない (空白で 2 つに割れない / `MAX_LINE` を越えて 414 で閉じた)
    RequestLine = 0,
    /// ヘッダーが長すぎる (1 行が長い / 合計が `MAX_HEADER_BYTES` 超 / 行数が多すぎる。431)
    HeaderTooLarge = 1,
    /// メソッドが HTTP のメソッド (token。RFC 9110 §5.6.2) として読めない
    Method = 2,
    /// オリジン形式 (`/path`) なのに `Host` が無い
    NoHost = 3,
    /// 絶対 URI (`http://…`) やマッピング形式 (`/https/…`) にホストが無い
    BadUri = 4,
    /// 本文の枠が不正 (`Content-Length` と `Transfer-Encoding: chunked` が両方ある = 要求の密輸)
    BodyFraming = 5,
    /// 折り返した (obs-fold) ヘッダーがある (次の行が SP / HTAB で始まる。RFC 9112 §5.2)
    ObsFold = 6,
    /// `Host` が 2 本以上ある (RFC 9110 §7.2。どの `Host` を信じるかで前段と後段が食い違う。T19.2)
    DupHost = 7,
    /// ヘッダーの形が不正 (名前とコロンの間に空白がある = RFC 9112 §5.1、値に CR / LF / NUL がある =
    /// RFC 9110 §5.5)。どちらも前段と後段で読み方が割れる (要求の密輸)。T27.5
    BadHeader = 8,
    /// 要求行とヘッダーが期限 (`PROXY_HEADER_TIMEOUT_SECS`) までに揃わず 408 で閉じた (T.28.0)。
    /// T.29.10 までは debug ログだけで数が無く、`/recent` の理由も CONNECT の確立の締め切りと
    /// 同じ `error:timeout` だった
    HeaderTimeout = 9,
}

/// [`BadRequestReason`] の数 (`rejected_requests` の配列の長さ)。
pub const BAD_REQUEST_REASONS: usize = 10;

/// `/status` の鍵と `/metrics` のラベルに使う名前 ([`BadRequestReason`] と同じ順)。
pub const BAD_REQUEST_REASON_NAMES: [&str; BAD_REQUEST_REASONS] = [
    "request_line",
    "header_too_large",
    "method",
    "no_host",
    "bad_uri",
    "body_framing",
    "obs_fold",
    "dup_host",
    "bad_header",
    "header_timeout",
];

impl BadRequestReason {
    /// `/status` の鍵と `/metrics` のラベルに使う名前。
    pub fn name(self) -> &'static str {
        BAD_REQUEST_REASON_NAMES[self as usize]
    }

    /// `/errors` の `cause` に出す名前 (`bad_request:<reason>`)。
    pub fn cause_name(self) -> &'static str {
        match self {
            BadRequestReason::RequestLine => "bad_request:request_line",
            BadRequestReason::HeaderTooLarge => "bad_request:header_too_large",
            BadRequestReason::Method => "bad_request:method",
            BadRequestReason::NoHost => "bad_request:no_host",
            BadRequestReason::BadUri => "bad_request:bad_uri",
            BadRequestReason::BodyFraming => "bad_request:body_framing",
            BadRequestReason::ObsFold => "bad_request:obs_fold",
            BadRequestReason::DupHost => "bad_request:dup_host",
            BadRequestReason::BadHeader => "bad_request:bad_header",
            BadRequestReason::HeaderTimeout => "bad_request:header_timeout",
        }
    }

    /// 符号 (0〜9) から戻す。知らない値は [`BadRequestReason::RequestLine`]。
    pub fn from_index(v: usize) -> BadRequestReason {
        match v {
            1 => BadRequestReason::HeaderTooLarge,
            2 => BadRequestReason::Method,
            3 => BadRequestReason::NoHost,
            4 => BadRequestReason::BadUri,
            5 => BadRequestReason::BodyFraming,
            6 => BadRequestReason::ObsFold,
            7 => BadRequestReason::DupHost,
            8 => BadRequestReason::BadHeader,
            9 => BadRequestReason::HeaderTimeout,
            _ => BadRequestReason::RequestLine,
        }
    }

    /// 空白で 2 つに割れなかった要求行の理由を決める。
    ///
    /// **断る経路からしか呼ばない**ので、文字列を見ても熱い経路には乗らない
    /// ([`ErrCause::from_io`](super::ErrCause::from_io) と同じ作法)。メソッドらしきものが token として
    /// 読めなければ [`BadRequestReason::Method`] (走査が投げるゴミ)、
    /// 読めるなら要求行そのものの形が足りない ([`BadRequestReason::RequestLine`])。
    ///
    /// 区切りは要求行を割る側 (`proxy-server` の `check_request_line`) と同じ **ASCII の空白だけ**
    /// (T.34.142。今までは Unicode の空白で割っていたので、`G\u{a0}ET` を `G` と読んで
    /// `request_line` に数えていた)。
    pub fn of_request_line(line: &str) -> BadRequestReason {
        match line.split_ascii_whitespace().next() {
            Some(m) if !is_method_token(m) => BadRequestReason::Method,
            _ => BadRequestReason::RequestLine,
        }
    }

    /// 要求ターゲットが解けなかったときの理由を決める (**断る経路からだけ**)。
    ///
    /// `proxy-origin` の `parse_origin` / `target_host` が `Err` を返すのは 2 通りしか
    /// 無い: スキーム付き (`http://` `https://`) とマッピング形式 (`/http/` `/https/`) で
    /// ホストが空 = [`BadRequestReason::BadUri`]、オリジン形式 (`/path`) で `Host` が
    /// 無い = [`BadRequestReason::NoHost`]。メソッドが token として読めなければ
    /// そちらを先に返す (要求行ごとゴミなら理由は `method` の方が読む人に近い)。
    pub fn of_target(method: &str, target: &str) -> BadRequestReason {
        if !is_method_token(method) {
            return BadRequestReason::Method;
        }
        // 絶対 URI のスキームは大小を区別しない (`HTTP://` も。T.28.33)、マッピング形式はパスなので小文字だけ
        let absolute = ["http://", "https://"].iter().any(|p| {
            target
                .get(..p.len())
                .is_some_and(|s| s.eq_ignore_ascii_case(p))
        });
        if absolute || target.starts_with("/http/") || target.starts_with("/https/") {
            BadRequestReason::BadUri
        } else {
            BadRequestReason::NoHost
        }
    }
}

/// メソッドが HTTP の token (RFC 9110 §5.6.2) として読めるか。**断る経路からだけ**呼ぶ。
fn is_method_token(m: &str) -> bool {
    !m.is_empty()
        && m.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [BadRequestReason; BAD_REQUEST_REASONS] = [
        BadRequestReason::RequestLine,
        BadRequestReason::HeaderTooLarge,
        BadRequestReason::Method,
        BadRequestReason::NoHost,
        BadRequestReason::BadUri,
        BadRequestReason::BodyFraming,
        BadRequestReason::ObsFold,
        BadRequestReason::DupHost,
        BadRequestReason::BadHeader,
        BadRequestReason::HeaderTimeout,
    ];

    /// 符号 (`/errors` の個票と `.recent` に残る数) は 10 種とも往復し、知らない値は `request_line`。
    #[test]
    fn the_index_round_trips_and_an_unknown_value_falls_back_to_request_line() {
        for (i, r) in ALL.iter().enumerate() {
            assert_eq!(*r as usize, i);
            assert_eq!(BadRequestReason::from_index(i), *r);
        }
        assert_eq!(
            BadRequestReason::from_index(BAD_REQUEST_REASONS),
            BadRequestReason::RequestLine
        );
        assert_eq!(
            BadRequestReason::from_index(usize::MAX),
            BadRequestReason::RequestLine
        );
    }

    /// `/errors` の `cause` は `/status` の鍵の名前に `bad_request:` を付けたもの (綴りを 2 か所で持つので縛る)。
    #[test]
    fn the_cause_name_is_the_status_key_with_the_bad_request_prefix() {
        for r in ALL {
            assert_eq!(r.cause_name(), format!("bad_request:{}", r.name()), "{r:?}");
            assert_eq!(r.name(), BAD_REQUEST_REASON_NAMES[r as usize]);
        }
    }

    /// 要求行の 1 語目が token でなければ `method`、token なら形が足りない `request_line`。
    #[test]
    fn a_request_line_whose_first_word_is_not_a_token_is_blamed_on_the_method() {
        assert_eq!(
            BadRequestReason::of_request_line("GET"),
            BadRequestReason::RequestLine
        );
        assert_eq!(
            BadRequestReason::of_request_line(""),
            BadRequestReason::RequestLine
        );
        assert_eq!(
            BadRequestReason::of_request_line("M-CUSTOM.1 /"),
            BadRequestReason::RequestLine
        );
        assert_eq!(
            BadRequestReason::of_request_line("\x16\x03\x01 /"),
            BadRequestReason::Method,
            "TLS の ClientHello を素で投げた走査"
        );
        assert_eq!(
            BadRequestReason::of_request_line("GET/ HTTP/1.1"),
            BadRequestReason::Method,
            "`/` は token の字ではない"
        );
        assert_eq!(
            BadRequestReason::of_request_line("取得 / HTTP/1.1"),
            BadRequestReason::Method
        );
    }

    /// 区切りは ASCII の空白だけ (要求行を割る側と同じ。T.34.142)。NBSP はメソッドの中に残るので
    /// `G\u{a0}ET\u{a0}/x` は 1 語 = token でない = `method` (今までは `G` と読んで `request_line`)。
    #[test]
    fn a_request_line_is_split_on_ascii_whitespace_only() {
        assert_eq!(
            BadRequestReason::of_request_line("G\u{a0}ET\u{a0}/x\r\n"),
            BadRequestReason::Method
        );
        assert_eq!(
            BadRequestReason::of_request_line("GET\t/x\r\n"),
            BadRequestReason::RequestLine
        );
    }
}
