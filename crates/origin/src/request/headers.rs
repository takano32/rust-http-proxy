//! 要求ヘッダーのうち転送・キャッシュ判断に使うものを、1 回の走査で拾う。

use crate::ascii;
use crate::log_debug;
use crate::log_trace;

/// リクエストヘッダーのうち転送・キャッシュ判断に使うもの。
#[derive(Default)]
pub struct RequestHeaders {
    pub host: Option<String>,
    /// `Host` が 2 本以上あった (T19.2)。RFC 9110 §7.2 は 400 を要求するので、
    /// これを立てて断る。**値は最初の 1 本**を残す (2 本目以降は確保しない)
    pub dup_host: bool,
    /// `Content-Length` の値 (重複が同値なら畳んだ 1 本。数でなければ `None`)
    pub content_length: Option<u64>,
    /// `Content-Length` の形が不正 (数字でない・`u64` に収まらない・重複で不一致)。
    ///
    /// 本文の切れ目が決まらないので、要求そのものを 400 で断る (T19.1)。
    /// **判定 (ここ) と転送 (生の行) が食い違わないようにする**ための持ち場で、
    /// 真なら [`RequestHeaders::content_length`] の値は使ってはいけない。
    pub content_length_invalid: bool,
    /// `Transfer-Encoding` の最後の指定が `chunked` で、`chunked` が 1 回だけか
    /// (このときだけ本文を chunked として読む。T27.6 で [`RequestHeaders::te_invalid`] と分けた)
    pub chunked: bool,
    /// `Transfer-Encoding` の行が 1 本でもあった (HTTP/1.0 の要求で立っていれば断る。T27.6)
    pub te_present: bool,
    /// `Transfer-Encoding` があるのに本文の切れ目が決まらない (T27.6): 最後の符号化が `chunked`
    /// でない (`xchunked`・`gzip`・`chunked, identity`) か、`chunked` が 2 回以上ある。
    /// RFC 9112 §6.3 は「要求の TE の最後が chunked でなければ 400 で断って閉じる」を求める。
    /// 今までは本文なしとしてヘッダーだけを転送し、本文はクライアントの次の要求として読まれた
    pub te_invalid: bool,
    pub authorization: bool,
    /// `Cookie` の行があった (T.32.35)。明示の鮮度も `public` も無い応答は保存しない
    pub cookie: bool,
    pub cache_control: String,
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<String>,
    pub if_range: Option<String>,
    pub range: Option<String>,
    pub accept_encoding: Option<String>,
    pub connection_close: bool,
    pub connection_keep_alive: bool,
    /// 折り返し (obs-fold) の継続行が混ざっていた (RFC 9112 §5.2)。
    /// 中継する側は 400 で断るか SP 1 個に置き換えるかを選べるが、置き換えると値が
    /// 黙って変わるので**断る**方を採る (T27.2)。断るかどうかは `http` 側が決める
    pub obs_fold: bool,
    /// ヘッダーの形が不正 (T27.5): 名前とコロンの間に空白がある (RFC 9112 §5.1 は 400 を求める)、
    /// または値に CR / LF / NUL がある (RFC 9110 §5.5。断るか SP に置き換える)。どちらも前段と
    /// 後段で読み方が割れる (`Content-Length : 5` を無視するオリジン、CR を改行と読むオリジン)。
    /// 要求は置き換えると値が黙って変わるので**断る**。断るのは `http` 側と CONNECT の入口。
    /// T.29.8 から、名前が token でない (空・空白・制御文字・`"` など) 行とコロンの無い行もここに入る
    pub bad_header: bool,
    /// `Expect: 100-continue` (大小無視) があった (T.28.3)。本文のある HTTP/1.1 の要求なら、
    /// `http` 側が本文を読む前に `100 Continue` を返す (`Expect` の行はオリジンへ転送しない)
    pub expect_continue: bool,
    /// `Expect` に `100-continue` 以外の期待があった (RFC 9110 §10.1.1。`http` 側が 417 で断る)
    pub expect_unknown: bool,
    /// `Upgrade` の行があった (T.29.9)。無いのにオリジンが 101 を返したら `http` 側が 502 にする
    pub upgrade: bool,
}

/// 空でない token か (ヘッダーの名前。T.29.8)。表は [`ascii::is_token`] (T.32.15 で応答の側と共有)
fn is_token(s: &str) -> bool {
    ascii::is_token(s)
}

/// `Transfer-Encoding` の並びの読み取り (RFC 9110 §5.3: 複数の行でも 1 つの並びとして読む)。
///
/// 覚えるのは「最後の符号化が `chunked` か」と「`chunked` を何回見たか」(T27.6)。
/// 本文を chunked として読めるのは、最後が `chunked` で、かつ `chunked` がちょうど 1 回のときだけ。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct TeTally {
    last_chunked: bool,
    chunked_count: u32,
}

impl TeTally {
    /// 1 行ぶんの値を足す。並びの空の要素は読み飛ばす
    #[inline]
    fn add(&mut self, value: &str) {
        for t in value
            .split(',')
            .map(str::trim_ascii)
            .filter(|t| !t.is_empty())
        {
            self.last_chunked = t.eq_ignore_ascii_case("chunked");
            self.chunked_count += u32::from(self.last_chunked);
        }
    }

    /// 最後の符号化が chunked で、chunked が 1 回だけか (RFC 9112 §6.1)
    #[inline]
    fn chunked(&self) -> bool {
        self.last_chunked && self.chunked_count == 1
    }
}

pub fn parse_request_headers(raw_headers: &[String], conn_id: usize) -> RequestHeaders {
    parse_request_headers_visit(raw_headers, conn_id, |_, _, _| {})
}

/// [`parse_request_headers`] と同じで、ふつうの行 (obs-fold の継続行を除く) を 1 本ずつ
/// `visit(行の添字, 名前, 値)` にも渡す (名前と値は前後の空白を落としたもの)。
///
/// 受け付けの層 (`serve_one`) が `Via` と `User-Agent` だけを別に見るためのもので、
/// ヘッダーをなめるのは**この 1 回だけ**になる (今までは `serve_one` が host / CL / TE / Via /
/// User-Agent を手でなめ、続いてここが全部をなめ直していた。T.28.15)。
pub fn parse_request_headers_visit(
    raw_headers: &[String],
    conn_id: usize,
    mut visit: impl FnMut(usize, &str, &str),
) -> RequestHeaders {
    let mut h = RequestHeaders::default();
    let mut te = TeTally::default();
    for (index, line) in raw_headers.iter().enumerate() {
        // 行の先頭が SP / HTAB なら直前の行の続き (obs-fold。RFC 9112 §5.2)。
        // 単独のヘッダーとして読むと継続行の値が黙って消える (オリジンには前の行だけが届く)
        // ので、ここでは旗を立てるだけにして呼び出し側に 400 を返させる
        if matches!(line.as_bytes().first(), Some(b' ' | b'\t')) {
            h.obs_fold = true;
            continue;
        }
        // コロンの無い行は不正 (T.29.8。今までは黙って捨て、オリジンへは転送していた)
        let Some((k, v)) = ascii::split_once(line, b':') else {
            h.bad_header = true;
            continue;
        };
        // 名前は空でない token だけ (RFC 9110 §5.1。T.29.8): 名前の後ろの空白 (`Content-Length : 5`。T27.5)、
        // 名前の中の空白・制御文字・`"`・`(` など、空の名前 (`: v`) はどれも不正。値の CR / LF / NUL (T27.5) も。
        // 行は行末の CRLF (か LF) を付けたまま来るので、行末の CR / LF を外した残りの途中にあるものだけを見る
        if !is_token(k)
            || v.trim_end_matches(['\r', '\n'])
                .bytes()
                .any(|b| matches!(b, b'\r' | b'\n' | 0))
        {
            h.bad_header = true;
        }
        // 名前は小文字化した `String` を作らずに比べる (要求ごとにヘッダーの本数だけ確保していた)
        let name = k.trim_ascii();
        let v_trim = v.trim_ascii();
        log_trace!(Some(conn_id), "req header  {}: {}", name, v_trim);
        visit(index, name, v_trim);
        h.note(name, v_trim, &mut te, conn_id);
    }
    if h.te_present {
        h.chunked = te.chunked();
        h.te_invalid = !h.chunked;
        if h.te_invalid {
            // debug: 呼ぶ側が 400 で断って WARN を絞って出す (T.28.9)
            log_debug!(
                Some(conn_id),
                "Transfer-Encoding does not end in a single chunked coding"
            );
        }
    }
    h
}

impl RequestHeaders {
    /// ヘッダー 1 本 (名前と値は前後の空白を落としたもの) を見て、使う欄を埋める。
    /// 名前の比較は上から順に 1 回ずつで、どれにも当たらない行は何もしない。
    #[inline]
    fn note(&mut self, name: &str, v_trim: &str, te: &mut TeTally, conn_id: usize) {
        let is = |want: &str| name.eq_ignore_ascii_case(want);
        if is("host") {
            // 2 本目を見た時点で「重複」の印を立てる (要求の密輸と、どの Host を信じるかの
            // 食い違いを避ける)。RFC 9110 §7.2 は中継する側に 400 を求めている
            if self.host.is_some() {
                self.dup_host = true;
            } else {
                self.host = Some(v_trim.to_string());
            }
        } else if is("authorization") {
            self.authorization = true;
        } else if is("cookie") {
            self.cookie = true;
        } else if is("cache-control") || is("pragma") {
            if !self.cache_control.is_empty() {
                self.cache_control.push(',');
            }
            self.cache_control.push_str(&v_trim.to_ascii_lowercase());
        } else if is("if-none-match") {
            self.if_none_match = Some(v_trim.to_string());
        } else if is("if-modified-since") {
            self.if_modified_since = Some(v_trim.to_string());
        } else if is("if-range") {
            self.if_range = Some(v_trim.to_string());
        } else if is("range") {
            self.range = Some(v_trim.to_string());
        } else if is("accept-encoding") {
            self.accept_encoding = Some(v_trim.to_string());
        } else if is("connection") || is("proxy-connection") {
            for token in v_trim.split(',') {
                let t = token.trim_ascii();
                if t.eq_ignore_ascii_case("close") {
                    self.connection_close = true;
                } else if t.eq_ignore_ascii_case("keep-alive") {
                    self.connection_keep_alive = true;
                }
            }
        } else if is("content-length") {
            // RFC 9110 §8.6: `Content-Length = 1*DIGIT`。数字だけの 10 進で `u64` に収まること。
            // 重複は全部同じ値なら 1 本に畳んでよく、1 つでも違えば本文の切れ目が決まらないので
            // 要求を断る (T19.1)
            match (self.content_length, content_length_value(v_trim)) {
                // ここの行は debug (T.28.9)。印が立った要求は呼ぶ側が必ず 400 で断り、そちらが
                // 理由ごとに 1 分に 1 行の WARN を出す (1 本ごとに WARN だと数を相手が決められる)
                (_, None) => {
                    log_debug!(Some(conn_id), "invalid Content-Length: {:?}", v_trim);
                    self.content_length_invalid = true;
                }
                (None, Some(n)) => self.content_length = Some(n),
                (Some(prev), Some(n)) if prev == n => {}
                (Some(prev), Some(n)) => {
                    log_debug!(
                        Some(conn_id),
                        "conflicting Content-Length: {} and {}",
                        prev,
                        n
                    );
                    self.content_length_invalid = true;
                }
            }
        } else if is("upgrade") {
            self.upgrade = true;
        } else if is("expect") {
            // 並びの空の要素は読み飛ばす。`100-continue` 以外が 1 つでもあれば 417 (T.28.3)
            for t in v_trim
                .split(',')
                .map(str::trim_ascii)
                .filter(|t| !t.is_empty())
            {
                if t.eq_ignore_ascii_case("100-continue") {
                    self.expect_continue = true;
                } else {
                    self.expect_unknown = true;
                }
            }
        } else if is("transfer-encoding") {
            // 最後の符号化が chunked なら本文は chunked (RFC 9112 §6.1)。並びの空の要素は読み飛ばす
            self.te_present = true;
            te.add(v_trim);
        }
    }
}

/// `Content-Length` の値として受け付ける形か (RFC 9110 §8.6 の `1*DIGIT`、`u64` に収まること)。
///
/// `-1`・`+5`・`5, 5`・空・`0x5`・21 桁 (u64 に収まらない) はどれも受け付けない。
/// 先頭の `0` は文法どおり許す (`0007` は 7)。**確保しない**ので要求ごとの費用は数字の判定 1 回。
fn content_length_value(v: &str) -> Option<u64> {
    // `str::parse::<u64>` は先頭の `+` を受け付けるので、数字だけであることを先に見る
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    v.parse::<u64>().ok()
}

#[cfg(test)]
mod te_tally_tests {
    use super::*;

    /// 並びは複数の行をまたいで 1 つ。最後が `chunked` でちょうど 1 回のときだけ chunked。
    #[test]
    fn chunked_only_when_last_and_exactly_once() {
        let tally = |lines: &[&str]| {
            let mut t = TeTally::default();
            for l in lines {
                t.add(l);
            }
            t
        };
        assert!(tally(&["chunked"]).chunked());
        assert!(tally(&["gzip, Chunked"]).chunked());
        assert!(tally(&["gzip", "chunked"]).chunked());
        assert!(tally(&["chunked,", ""]).chunked());
        assert!(!tally(&["chunked, gzip"]).chunked());
        assert!(!tally(&["chunked, chunked"]).chunked());
        assert!(!tally(&["chunked", "chunked"]).chunked());
        assert!(!tally(&["chunked;x=1"]).chunked());
        assert!(!tally(&[""]).chunked());
        assert_eq!(
            tally(&["chunked", "chunked"]),
            TeTally {
                last_chunked: true,
                chunked_count: 2
            }
        );
    }
}
