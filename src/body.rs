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

impl Framing {
    /// レスポンスの枠組み。`head_only` は HEAD への応答など本文が来ないケース。
    pub fn of_response(status: u16, head_only: bool, headers: &[(String, String)]) -> Framing {
        if head_only || (100..200).contains(&status) || status == 204 || status == 304 {
            return Framing::None;
        }
        Self::of_headers(headers).unwrap_or(Framing::Close)
    }

    /// リクエストの枠組み (枠が無ければ本文なし)。
    pub fn of_request(headers: &[(String, String)]) -> Framing {
        Self::of_headers(headers).unwrap_or(Framing::None)
    }

    fn of_headers(headers: &[(String, String)]) -> Option<Framing> {
        let mut length = None;
        for (k, v) in headers {
            match k.as_str() {
                "transfer-encoding" => {
                    if v.split(',')
                        .next_back()
                        .is_some_and(|t| t.trim().eq_ignore_ascii_case("chunked"))
                    {
                        return Some(Framing::Chunked);
                    }
                }
                "content-length" => length = v.trim().parse::<u64>().ok(),
                _ => {}
            }
        }
        length.map(Framing::Length)
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
