//! メッセージ本文の区切り方 ([`Framing`]) を、ヘッダーから決める。

/// メッセージ本文の区切り方 (RFC 9112 §6)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// 本文なし (1xx / 204 / 304 / HEAD への応答など)
    None,
    Length(u64),
    Chunked,
    /// 接続が閉じるまで
    Close,
}

/// 応答の先頭を 1 回なめて分かること ([`Framing::scan_response_head`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseScan<'a> {
    pub framing: Result<Framing, BadLength<'a>>,
    /// `Connection` が `close` を挙げているか
    pub close: bool,
    /// 最後の `Age` の行の値 (無ければ `None`。T29.4)
    pub age: Option<&'a str>,
    /// `Date` の行があるか (無ければ forward が付ける。T29.9)
    pub date: bool,
}

/// 応答の `Content-Length` が壊れていた (T19.5)。中身は問題の値 (ログに出すため)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadLength<'a>(pub &'a str);

/// `Transfer-Encoding` の並びを読んだ結果 (T28.1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Te {
    /// `Transfer-Encoding` が無い (空の値の行しか無い場合も含む)
    Absent,
    /// 最後の符号化が `chunked`
    Chunked,
    /// あるが、最後の符号化が `chunked` でない (`gzip`・`xchunked`・`chunked, gzip` など)
    Unchunked,
}

impl Te {
    /// 1 行ぶんの値を足す。複数の行は 1 つの並びとして読み、空の要素は読み飛ばす
    /// (要求側の T27.6 と同じ読み方)。決めるのは「並び全体の最後の符号化」。
    fn add(self, value: &str) -> Te {
        match value
            .rsplit(',')
            .map(str::trim_ascii)
            .find(|t| !t.is_empty())
        {
            Some(t) if t.eq_ignore_ascii_case("chunked") => Te::Chunked,
            Some(_) => Te::Unchunked,
            None => self,
        }
    }
}

impl Framing {
    /// レスポンスの枠組み。`head_only` は HEAD への応答など本文が来ないケース。
    pub fn of_response(status: u16, head_only: bool, headers: &[(String, String)]) -> Framing {
        if head_only || (100..200).contains(&status) || status == 204 || status == 304 {
            return Framing::None;
        }
        Self::of_headers(headers).unwrap_or(Framing::Close)
    }

    /// 応答の枠組みを**生の先頭バイト列から**決める ([`Framing::of_response`] と同じ規則)。
    /// 小文字の名前と値の組を作らずに済む。いまの熱い経路は [`Framing::scan_response_head`] を使うので、
    /// これは `of_response_head_tests` が規則を突き合わせる手本として試験のときだけ作る (T34.23)。
    #[cfg(test)]
    pub(crate) fn of_response_head(status: u16, head_only: bool, head: &[u8]) -> Framing {
        if head_only || (100..200).contains(&status) || status == 204 || status == 304 {
            return Framing::None;
        }
        let mut te = Te::Absent;
        let mut length = None;
        for (k, v) in crate::headers::response_lines(head) {
            if k.eq_ignore_ascii_case("transfer-encoding") {
                te = te.add(v);
            } else if k.eq_ignore_ascii_case("content-length") {
                length = v.trim_ascii().parse::<u64>().ok();
            }
        }
        match te {
            Te::Chunked => Framing::Chunked,
            // 最後が chunked でない TE は、CL があっても閉じるまでが本文 (RFC 9112 §6.3。T28.1)
            Te::Unchunked => Framing::Close,
            Te::Absent => length.map(Framing::Length).unwrap_or(Framing::Close),
        }
    }

    /// 応答の先頭に (空でない値の) `Transfer-Encoding` の行があるか (T28.1)。
    ///
    /// [`Framing::scan_response_head`] の枠組みが [`Framing::Close`] だったときだけ呼ぶ
    /// (「CL も TE も無い」のか「最後が chunked でない TE」なのかを見分ける。後者は
    /// 区切りがオリジンと食い違いうるので保存しない)。熱い経路 (CL / chunked) では呼ばれない。
    pub fn response_has_transfer_encoding(head: &[u8]) -> bool {
        crate::headers::response_lines(head).any(|(k, v)| {
            k.eq_ignore_ascii_case("transfer-encoding") && Te::Absent.add(v) != Te::Absent
        })
    }

    /// 応答の `Transfer-Encoding` の並びを、クライアントへそのまま伝える 1 行
    /// (`Transfer-Encoding: gzip, br`。CRLF なし) にする (T28.35)。並びが空なら `None`。
    ///
    /// 最後が chunked でない TE の応答は閉じるまでを本文にして、符号化はほどかずに渡すので、
    /// TE の行も残さないとクライアントには符号化済みのバイトが TE なしに見える (RFC 9112 §6.1)。
    /// 複数の行は 1 つの並びとして 1 行に畳み、空の要素は読み飛ばす ([`Te`] と同じ読み方)。
    /// 値の CR / NUL は SP に置き換える (T27.7 の応答の値と同じ)。
    /// [`Framing::Close`] のときだけ呼ぶ (熱い経路では先頭をなめ直さない)。
    pub fn response_transfer_encoding_line(head: &[u8]) -> Option<String> {
        let mut line = String::new();
        for (k, v) in crate::headers::response_lines(head) {
            if !k.eq_ignore_ascii_case("transfer-encoding") {
                continue;
            }
            for t in v.split(',').map(str::trim_ascii).filter(|t| !t.is_empty()) {
                line.push_str(if line.is_empty() {
                    "Transfer-Encoding: "
                } else {
                    ", "
                });
                line.extend(
                    t.chars()
                        .map(|c| if c == '\r' || c == '\0' { ' ' } else { c }),
                );
            }
        }
        (!line.is_empty()).then_some(line)
    }

    /// [`Framing::of_response_head`] に `Content-Length` の検査を足した版 (T19.5)。
    ///
    /// 重複した `Content-Length` の値が食い違う / 数字でない / 負 / `u64` に収まらない桁数なら
    /// `Err`。そのまま通すと「名乗った長さと実際に送るバイトが食い違う」応答になるので、
    /// 呼ぶ側は 502 に差し替える。値が全部同じなら 1 本に畳む (転送する先頭は
    /// [`crate::headers::write_response_head`] が元の CL を落として組み直す)。
    ///
    /// **本文を持たない応答 (1xx / 204 / 304 / HEAD) でも CL は検査する**: 枠組みは
    /// 今までどおり [`Framing::None`] だが、HEAD はオリジンの `Content-Length` をそのまま
    /// 転送するので、壊れた値を素通ししない。
    ///
    /// **最後の符号化が chunked でない `Transfer-Encoding`** (`gzip`・`xchunked` など) は、
    /// 本文を持つ応答なら CL を見ずに [`Framing::Close`] (RFC 9112 §6.3。T28.1)。
    /// 以前は CL で区切って TE を落とし、オリジンの接続を使い回して保存もしていた
    /// (CL の後ろに続けたバイトが次の応答として読まれうる)。
    ///
    /// 試験のときだけ作る (クレートの外にもクレートの中にも、試験のほかの利用者はいない。T34.23)。
    #[cfg(test)]
    pub(crate) fn of_response_head_checked(
        status: u16,
        head_only: bool,
        head: &[u8],
    ) -> Result<Framing, BadLength<'_>> {
        Self::of_response_head_checked_with_close(status, head_only, head).0
    }

    /// [`Framing::of_response_head_checked`] と「`Connection` が `close` を挙げているか」を
    /// **1 回の走査**で返す (T28.12。forward の経路は枠組み・CL の検査・`Connection` で 3 回なめていた)。
    /// 試験のときだけ作る (T34.23)。
    #[cfg(test)]
    pub(crate) fn of_response_head_checked_with_close(
        status: u16,
        head_only: bool,
        head: &[u8],
    ) -> (Result<Framing, BadLength<'_>>, bool) {
        let scan = Self::scan_response_head(status, head_only, head);
        (scan.framing, scan.close)
    }

    /// `of_response_head_checked_with_close` (試験の手本) に `Age` の値も足したもの (T29.4)。
    /// forward の経路は応答の先頭をこの 1 回だけなめる。
    pub fn scan_response_head(status: u16, head_only: bool, head: &[u8]) -> ResponseScan<'_> {
        let no_body = head_only || (100..200).contains(&status) || status == 204 || status == 304;
        let mut te = Te::Absent;
        let mut length: Option<u64> = None;
        let mut bad: Option<BadLength<'_>> = None;
        let mut close = false;
        let mut age = None;
        let mut date = false;
        for (k, v) in crate::headers::response_lines(head) {
            // `Age` (3 字) と `Date` (4 字) は、長さで外れる行は比べずに済む
            if k.len() == 3 && k.eq_ignore_ascii_case("age") {
                age = Some(v);
                continue;
            }
            if k.len() == 4 && k.eq_ignore_ascii_case("date") {
                date = true;
                continue;
            }
            if k.eq_ignore_ascii_case("transfer-encoding") {
                te = te.add(v);
            } else if k.eq_ignore_ascii_case("content-length") {
                // 最初の不正な値で止める (重複して不一致・数字でない・負・桁あふれ)
                if bad.is_none() {
                    match (length, crate::ascii::content_length_value(v)) {
                        (_, None) => bad = Some(BadLength(v)),
                        (Some(prev), Some(n)) if prev != n => bad = Some(BadLength(v)),
                        (_, Some(n)) => length = Some(n),
                    }
                }
            } else if k.eq_ignore_ascii_case("connection")
                && v.split(',')
                    .any(|t| t.trim_ascii().eq_ignore_ascii_case("close"))
            {
                close = true;
            }
        }
        let framing = match te {
            // chunked が来ていれば CL は見ない (RFC 9112 §6.3。今までの優先順位のまま)
            Te::Chunked => Ok(Framing::Chunked),
            // 最後が chunked でない TE は CL を見ずに閉じるまで (T28.1)
            Te::Unchunked if !no_body => Ok(Framing::Close),
            _ => match bad {
                Some(b) => Err(b),
                None if no_body => Ok(Framing::None),
                None => Ok(length.map(Framing::Length).unwrap_or(Framing::Close)),
            },
        };
        ResponseScan {
            framing,
            close,
            age,
            date,
        }
    }

    /// リクエストの枠組み (枠が無ければ本文なし)。
    ///
    /// 最後が chunked でない TE の要求は、ここへ来る前に 400 で断っている (T27.6)。
    pub fn of_request(headers: &[(String, String)]) -> Framing {
        Self::of_headers(headers).unwrap_or(Framing::None)
    }

    fn of_headers(headers: &[(String, String)]) -> Option<Framing> {
        let mut te = Te::Absent;
        let mut length = None;
        for (k, v) in headers {
            match k.as_str() {
                "transfer-encoding" => te = te.add(v),
                "content-length" => length = v.trim().parse::<u64>().ok(),
                _ => {}
            }
        }
        match te {
            Te::Chunked => Some(Framing::Chunked),
            // 最後が chunked でない TE は CL を無視して閉じるまで (T28.1)
            Te::Unchunked => Some(Framing::Close),
            Te::Absent => length.map(Framing::Length),
        }
    }

    pub fn has_body(self) -> bool {
        self != Framing::None
    }
}

#[cfg(test)]
mod te_tests {
    use super::*;

    /// `Transfer-Encoding` の並びは「全体の最後の符号化」で決まり、空の要素と空の行は読み飛ばす。
    #[test]
    fn the_last_coding_decides() {
        assert_eq!(Te::Absent.add("chunked"), Te::Chunked);
        assert_eq!(Te::Absent.add("gzip, chunked"), Te::Chunked);
        assert_eq!(Te::Absent.add("chunked, gzip"), Te::Unchunked);
        assert_eq!(Te::Absent.add("CHUNKED ,"), Te::Chunked);
        assert_eq!(Te::Absent.add(""), Te::Absent);
        assert_eq!(Te::Absent.add(" , "), Te::Absent);
        // 複数の行は 1 つの並び: 後の行が勝つ。空の行は前の答えを変えない
        assert_eq!(Te::Chunked.add("gzip"), Te::Unchunked);
        assert_eq!(Te::Unchunked.add("chunked"), Te::Chunked);
        assert_eq!(Te::Chunked.add(""), Te::Chunked);
    }
}
