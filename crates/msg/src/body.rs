//! HTTP 本文の枠組み (framing): 解読と再エンコード、`Range` の解釈。
//!
//! オリジンからの応答は必ず解読 (chunked を外す) してから扱う。クライアントへは
//! 自前で枠を付け直し (Content-Length / 再 chunk / close)、キャッシュには生の本文だけを置く。

use std::io::{self, BufRead, Read, Write};

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

/// 応答の `Content-Length` が壊れていた (T19.5)。中身は問題の値 (ログに出すため)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadLength<'a>(pub &'a str);

/// `Content-Length` の値として受け付ける形か (RFC 9110 §8.6 の `1*DIGIT`、`u64` に収まること)。
///
/// 要求側 (`proxy-origin` の `request::content_length_value`) と同じ決まり:
/// `-1`・`+5`・`5, 5`・空・`0x5`・21 桁はどれも受け付けない。先頭の `0` は文法どおり許す。
fn content_length_value(v: &str) -> Option<u64> {
    // `str::parse::<u64>` は先頭の `+` を受け付けるので、数字だけであることを先に見る
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    v.parse::<u64>().ok()
}

/// `Transfer-Encoding` の並びを読んだ結果 (T.28.1)。
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
    /// (要求側の T.27.6 と同じ読み方)。決めるのは「並び全体の最後の符号化」。
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
    /// 小文字の名前と値の組を作らずに済むので、素通しの経路はこちらを使う。
    pub fn of_response_head(status: u16, head_only: bool, head: &[u8]) -> Framing {
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
            // 最後が chunked でない TE は、CL があっても閉じるまでが本文 (RFC 9112 §6.3。T.28.1)
            Te::Unchunked => Framing::Close,
            Te::Absent => length.map(Framing::Length).unwrap_or(Framing::Close),
        }
    }

    /// 応答の先頭に (空でない値の) `Transfer-Encoding` の行があるか (T.28.1)。
    ///
    /// [`Framing::of_response_head_checked`] が [`Framing::Close`] を返したときだけ呼ぶ
    /// (「CL も TE も無い」のか「最後が chunked でない TE」なのかを見分ける。後者は
    /// 区切りがオリジンと食い違いうるので保存しない)。熱い経路 (CL / chunked) では呼ばれない。
    pub fn response_has_transfer_encoding(head: &[u8]) -> bool {
        crate::headers::response_lines(head).any(|(k, v)| {
            k.eq_ignore_ascii_case("transfer-encoding") && Te::Absent.add(v) != Te::Absent
        })
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
    /// 本文を持つ応答なら CL を見ずに [`Framing::Close`] (RFC 9112 §6.3。T.28.1)。
    /// 以前は CL で区切って TE を落とし、オリジンの接続を使い回して保存もしていた
    /// (CL の後ろに続けたバイトが次の応答として読まれうる)。
    pub fn of_response_head_checked(
        status: u16,
        head_only: bool,
        head: &[u8],
    ) -> Result<Framing, BadLength<'_>> {
        let no_body = head_only || (100..200).contains(&status) || status == 204 || status == 304;
        let mut te = Te::Absent;
        for (k, v) in crate::headers::response_lines(head) {
            if k.eq_ignore_ascii_case("transfer-encoding") {
                te = te.add(v);
            }
        }
        match te {
            // chunked が来ていれば CL は見ない (RFC 9112 §6.3。今までの優先順位のまま)
            Te::Chunked => return Ok(Framing::Chunked),
            Te::Unchunked if !no_body => return Ok(Framing::Close),
            _ => {}
        }
        let mut length: Option<u64> = None;
        for (k, v) in crate::headers::response_lines(head) {
            if !k.eq_ignore_ascii_case("content-length") {
                continue;
            }
            let n = content_length_value(v).ok_or(BadLength(v))?;
            match length {
                Some(prev) if prev != n => return Err(BadLength(v)),
                _ => length = Some(n),
            }
        }
        if no_body {
            return Ok(Framing::None);
        }
        Ok(length.map(Framing::Length).unwrap_or(Framing::Close))
    }

    /// リクエストの枠組み (枠が無ければ本文なし)。
    ///
    /// 最後が chunked でない TE の要求は、ここへ来る前に 400 で断っている (T.27.6)。
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
            // 最後が chunked でない TE は CL を無視して閉じるまで (T.28.1)
            Te::Unchunked => Some(Framing::Close),
            Te::Absent => length.map(Framing::Length),
        }
    }

    pub fn has_body(self) -> bool {
        self != Framing::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    Size,
    Data(u64),
    DataCrlf,
    Trailers,
    Done,
}

/// 枠組みに従って本文を解読して返すリーダー。終端で `Ok(0)`。
/// 途中で接続が切れたら `UnexpectedEof` を返し、`finished_cleanly()` が false になる。
pub struct BodyReader<'a, R: BufRead> {
    inner: &'a mut R,
    framing: Framing,
    remaining: u64,
    chunk: ChunkState,
    done: bool,
    truncated: bool,
}

impl<'a, R: BufRead> BodyReader<'a, R> {
    pub fn new(inner: &'a mut R, framing: Framing) -> Self {
        let (remaining, done) = match framing {
            Framing::None => (0, true),
            Framing::Length(n) => (n, n == 0),
            Framing::Chunked | Framing::Close => (0, false),
        };
        Self {
            inner,
            framing,
            remaining,
            chunk: ChunkState::Size,
            done,
            truncated: false,
        }
    }

    pub fn finished_cleanly(&self) -> bool {
        self.done && !self.truncated
    }

    fn truncate(&mut self) -> io::Error {
        self.truncated = true;
        self.done = true;
        io::Error::new(io::ErrorKind::UnexpectedEof, "body ended early")
    }

    fn read_chunked(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.chunk {
                ChunkState::Size => {
                    let mut line = String::new();
                    if self.inner.read_line(&mut line)? == 0 {
                        return Err(self.truncate());
                    }
                    let size_str = line.trim().split(';').next().unwrap_or("").trim();
                    let size = u64::from_str_radix(size_str, 16).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "bad chunk size")
                    })?;
                    self.chunk = if size == 0 {
                        ChunkState::Trailers
                    } else {
                        ChunkState::Data(size)
                    };
                }
                ChunkState::Data(left) => {
                    let want = (left.min(buf.len() as u64)) as usize;
                    let n = self.inner.read(&mut buf[..want])?;
                    if n == 0 {
                        return Err(self.truncate());
                    }
                    let left = left - n as u64;
                    self.chunk = if left == 0 {
                        ChunkState::DataCrlf
                    } else {
                        ChunkState::Data(left)
                    };
                    return Ok(n);
                }
                ChunkState::DataCrlf => {
                    let mut crlf = String::new();
                    if self.inner.read_line(&mut crlf)? == 0 {
                        return Err(self.truncate());
                    }
                    self.chunk = ChunkState::Size;
                }
                ChunkState::Trailers => {
                    let mut line = String::new();
                    if self.inner.read_line(&mut line)? == 0 {
                        return Err(self.truncate());
                    }
                    if line.trim().is_empty() {
                        self.chunk = ChunkState::Done;
                        self.done = true;
                        return Ok(0);
                    }
                }
                ChunkState::Done => return Ok(0),
            }
        }
    }
}

impl<R: BufRead> Read for BodyReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        match self.framing {
            Framing::None => Ok(0),
            Framing::Length(_) => {
                let want = (self.remaining.min(buf.len() as u64)) as usize;
                let n = self.inner.read(&mut buf[..want])?;
                if n == 0 {
                    return Err(self.truncate());
                }
                self.remaining -= n as u64;
                if self.remaining == 0 {
                    self.done = true;
                }
                Ok(n)
            }
            Framing::Chunked => self.read_chunked(buf),
            Framing::Close => {
                let n = self.inner.read(buf)?;
                if n == 0 {
                    self.done = true;
                }
                Ok(n)
            }
        }
    }
}

/// 本文を読み捨てる (転送しないリクエスト本文など)。
pub fn drain<R: BufRead>(inner: &mut R, framing: Framing) -> io::Result<u64> {
    let mut reader = BodyReader::new(inner, framing);
    io::copy(&mut reader, &mut io::sink())
}

/// chunked 形式で 1 チャンク書く。
pub fn write_chunk(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    write!(w, "{:x}\r\n", data.len())?;
    w.write_all(data)?;
    w.write_all(b"\r\n")
}

/// chunked 形式の終端。
pub fn write_last_chunk(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"0\r\n\r\n")
}

/// `Range` 要求の解釈結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// 両端を含むバイト範囲
    Bytes { start: u64, end: u64 },
    /// 416 を返す
    Unsatisfiable,
    /// 対応しない形 (複数範囲など) → 200 で全体を返す
    Ignore,
}

/// 単一範囲の `Range: bytes=...` を `total` バイトの表現に当てはめる (RFC 9110 §14)。
pub fn parse_range(header: &str, total: u64) -> RangeSpec {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeSpec::Ignore;
    };
    if spec.contains(',') {
        return RangeSpec::Ignore;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return RangeSpec::Ignore;
    };
    let (a, b) = (a.trim(), b.trim());
    let parsed = if a.is_empty() {
        // 末尾 n バイト
        match b.parse::<u64>() {
            Ok(0) | Err(_) => {
                return if b.is_empty() {
                    RangeSpec::Ignore
                } else {
                    RangeSpec::Unsatisfiable
                };
            }
            Ok(n) => (total.saturating_sub(n), total.saturating_sub(1)),
        }
    } else {
        let Ok(start) = a.parse::<u64>() else {
            return RangeSpec::Ignore;
        };
        let end = if b.is_empty() {
            total.saturating_sub(1)
        } else {
            match b.parse::<u64>() {
                Ok(e) if e >= start => e.min(total.saturating_sub(1)),
                _ => return RangeSpec::Ignore,
            }
        };
        (start, end)
    };
    if total == 0 || parsed.0 >= total {
        return RangeSpec::Unsatisfiable;
    }
    RangeSpec::Bytes {
        start: parsed.0,
        end: parsed.1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn hdrs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn read_all<R: BufRead>(r: &mut R, f: Framing) -> (io::Result<Vec<u8>>, bool) {
        let mut body = BodyReader::new(r, f);
        let mut out = Vec::new();
        let res = body.read_to_end(&mut out).map(|_| out);
        (res, body.finished_cleanly())
    }

    #[test]
    fn framing_selection() {
        assert_eq!(
            Framing::of_response(200, false, &hdrs(&[("content-length", "5")])),
            Framing::Length(5)
        );
        assert_eq!(
            Framing::of_response(200, false, &hdrs(&[("transfer-encoding", "gzip, chunked")])),
            Framing::Chunked
        );
        assert_eq!(Framing::of_response(200, false, &hdrs(&[])), Framing::Close);
        assert_eq!(
            Framing::of_response(204, false, &hdrs(&[("content-length", "5")])),
            Framing::None
        );
        assert_eq!(Framing::of_response(304, false, &hdrs(&[])), Framing::None);
        assert_eq!(
            Framing::of_response(200, true, &hdrs(&[("content-length", "5")])),
            Framing::None
        );
        assert_eq!(Framing::of_request(&hdrs(&[])), Framing::None);
        assert_eq!(
            Framing::of_request(&hdrs(&[("content-length", "3")])),
            Framing::Length(3)
        );
        assert!(Framing::Chunked.has_body() && !Framing::None.has_body());
    }

    #[test]
    fn decodes_chunked_with_extensions_and_trailers() {
        let raw = b"4;ext=1\r\nWiki\r\n5\r\npedia\r\n0\r\nX-Trailer: a\r\n\r\nNEXT";
        let mut r = BufReader::new(&raw[..]);
        let (out, clean) = read_all(&mut r, Framing::Chunked);
        assert_eq!(out.unwrap(), b"Wikipedia");
        assert!(clean);
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"NEXT", "bytes after the body stay in the stream");
    }

    #[test]
    fn detects_truncation() {
        let mut r = BufReader::new(&b"hel"[..]);
        let (out, clean) = read_all(&mut r, Framing::Length(5));
        assert_eq!(out.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert!(!clean);
        let mut r = BufReader::new(&b"5\r\nhel"[..]);
        let (out, clean) = read_all(&mut r, Framing::Chunked);
        assert!(out.is_err() && !clean);
        let mut r = BufReader::new(&b"zz\r\n"[..]);
        assert!(read_all(&mut r, Framing::Chunked).0.is_err());
    }

    #[test]
    fn length_and_close_and_none() {
        let mut r = BufReader::new(&b"hello world"[..]);
        let (out, clean) = read_all(&mut r, Framing::Length(5));
        assert_eq!(out.unwrap(), b"hello");
        assert!(clean);
        let mut r = BufReader::new(&b"until eof"[..]);
        let (out, clean) = read_all(&mut r, Framing::Close);
        assert_eq!(out.unwrap(), b"until eof");
        assert!(clean);
        let mut r = BufReader::new(&b"ignored"[..]);
        let (out, clean) = read_all(&mut r, Framing::None);
        assert!(out.unwrap().is_empty() && clean);
        let mut r = BufReader::new(&b"abcdef"[..]);
        assert_eq!(drain(&mut r, Framing::Length(4)).unwrap(), 4);
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"ef");
    }

    #[test]
    fn writes_chunks() {
        let mut out = Vec::new();
        write_chunk(&mut out, b"Wiki").unwrap();
        write_chunk(&mut out, b"").unwrap();
        write_last_chunk(&mut out).unwrap();
        assert_eq!(out, b"4\r\nWiki\r\n0\r\n\r\n");
    }

    #[test]
    fn parses_ranges() {
        assert_eq!(
            parse_range("bytes=0-4", 10),
            RangeSpec::Bytes { start: 0, end: 4 }
        );
        assert_eq!(
            parse_range("bytes=5-", 10),
            RangeSpec::Bytes { start: 5, end: 9 }
        );
        assert_eq!(
            parse_range("bytes=-3", 10),
            RangeSpec::Bytes { start: 7, end: 9 }
        );
        assert_eq!(
            parse_range("bytes=-30", 10),
            RangeSpec::Bytes { start: 0, end: 9 }
        );
        assert_eq!(
            parse_range("bytes=2-100", 10),
            RangeSpec::Bytes { start: 2, end: 9 }
        );
        assert_eq!(parse_range("bytes=10-", 10), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 10), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,3-4", 10), RangeSpec::Ignore);
        assert_eq!(parse_range("bytes=5-2", 10), RangeSpec::Ignore);
        assert_eq!(parse_range("items=0-1", 10), RangeSpec::Ignore);
        assert_eq!(parse_range("bytes=x-1", 10), RangeSpec::Ignore);
    }
}

#[cfg(test)]
mod of_response_head_tests {
    use super::*;

    /// 検査つきの版 (T19.5) が、壊れた `Content-Length` を 1 つも通さないこと。
    #[test]
    fn checked_rejects_every_broken_content_length() {
        let none = Framing::None;
        let len = |n| Ok(Framing::Length(n));
        // 正常な形は今までどおり (重複が同値なら 1 本に畳む)
        for (head, want) in [
            (&b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"[..], len(5)),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n",
                len(5),
            ),
            (b"HTTP/1.1 200 OK\r\nContent-Length: 0007\r\n\r\n", len(7)),
            (b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", len(0)),
            (b"HTTP/1.1 200 OK\r\n\r\n", Ok(Framing::Close)),
            // chunked が来ていれば CL は見ない (RFC 9112 §6.3)
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n",
                Ok(Framing::Chunked),
            ),
            // 最後が chunked でない TE は CL を捨てて閉じるまで (T.28.1)。CL が壊れていても見ない
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: 5\r\n\r\n",
                Ok(Framing::Close),
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: xchunked\r\n\r\n",
                Ok(Framing::Close),
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, gzip\r\nContent-Length: 5\r\n\r\n",
                Ok(Framing::Close),
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: cheese\r\nContent-Length: 5\r\n\r\n",
                Ok(Framing::Close),
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: abc\r\n\r\n",
                Ok(Framing::Close),
            ),
            // 複数の行は 1 つの並び: 最後の行が chunked なら chunked
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n",
                Ok(Framing::Chunked),
            ),
            // 空の値の TE は無いのと同じ (今までどおり CL で区切る)
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: \r\nContent-Length: 5\r\n\r\n",
                len(5),
            ),
        ] {
            assert_eq!(
                Framing::of_response_head_checked(200, false, head),
                want,
                "{:?}",
                String::from_utf8_lossy(head)
            );
        }
        // 壊れた形はすべて弾く (重複不一致・数字でない・負・桁あふれ・カンマ)
        for head in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n"[..],
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nContent-Length: 5\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: +5\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 5, 5\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: abc\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 0x5\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: \r\n\r\n",
            // 20 桁でも u64 に収まらなければ弾く / 21 桁も弾く
            b"HTTP/1.1 200 OK\r\nContent-Length: 99999999999999999999\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 999999999999999999999\r\n\r\n",
        ] {
            assert!(
                Framing::of_response_head_checked(200, false, head).is_err(),
                "{:?} を通した",
                String::from_utf8_lossy(head)
            );
        }
        // 本文を持たない応答でも CL は検査する (HEAD はオリジンの CL をそのまま転送する)。
        // 値が正常なら枠組みは今までどおり `None`
        for status in [101u16, 204, 304] {
            assert_eq!(
                Framing::of_response_head_checked(
                    status,
                    false,
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"
                ),
                Ok(none),
                "status={}",
                status
            );
            assert!(
                Framing::of_response_head_checked(
                    status,
                    false,
                    b"HTTP/1.1 204 No Content\r\nContent-Length: -1\r\n\r\n"
                )
                .is_err(),
                "status={} で壊れた CL を通した",
                status
            );
        }
        assert_eq!(
            Framing::of_response_head_checked(
                200,
                true,
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"
            ),
            Ok(none),
            "HEAD"
        );
        assert!(
            Framing::of_response_head_checked(
                200,
                true,
                b"HTTP/1.1 200 OK\r\nContent-Length: abc\r\n\r\n"
            )
            .is_err(),
            "HEAD で壊れた CL を通した"
        );
        // 本文を持たない応答は TE があっても `None` (CL の検査も今までどおり)
        assert_eq!(
            Framing::of_response_head_checked(
                200,
                true,
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: 5\r\n\r\n"
            ),
            Ok(none),
            "HEAD + TE"
        );
        // 見分けの口: CL だけ・TE なしの Close では偽、最後が chunked でない TE では真
        assert!(!Framing::response_has_transfer_encoding(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n"
        ));
        assert!(!Framing::response_has_transfer_encoding(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: \r\n\r\n"
        ));
        assert!(Framing::response_has_transfer_encoding(
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: cheese\r\n\r\n"
        ));
    }

    /// 生の先頭から決める版が、組を作ってから決める版と同じ結果になること。
    #[test]
    fn matches_the_pairs_version() {
        let heads: Vec<&[u8]> = vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, gzip\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: nope\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            b"HTTP/1.1 200 OK\r\n\r\n",
            // T.28.1: 最後が chunked でない TE と CL
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: 5\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: gzip\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n",
        ];
        for head in heads {
            let pairs = crate::headers::response_pairs(head);
            for status in [200u16, 204, 304, 101, 500] {
                for head_only in [false, true] {
                    assert_eq!(
                        Framing::of_response_head(status, head_only, head),
                        Framing::of_response(status, head_only, &pairs),
                        "head={:?} status={} head_only={}",
                        String::from_utf8_lossy(head),
                        status,
                        head_only
                    );
                }
            }
        }
    }
}
