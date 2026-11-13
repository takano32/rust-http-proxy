//! 枠組みに従って本文を解読して読む ([`BodyReader`])。読み捨てと chunk の書き出しもここ。

use std::io::{self, BufRead, Read, Write};

use super::Framing;

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
    /// ここまでに読んだ trailer の合計 (T29.1)
    trailer_bytes: usize,
}

/// chunk の大きさの行 (拡張込み) とデータの後ろの CRLF の行の上限 (T29.1)。
/// 16 進の大きさは 16 桁で足りるので、拡張に十分な幅を残して 4 KiB。
pub const MAX_CHUNK_SIZE_LINE: usize = 4 * 1024;
/// trailer の 1 行と合計の上限 (T29.1)。
pub const MAX_TRAILER_LINE: usize = 8 * 1024;
pub const MAX_TRAILER_BYTES: usize = 64 * 1024;

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
            trailer_bytes: 0,
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

    /// 枠の行 (chunk の大きさの行・データの後ろの CRLF・trailer の行) を 1 本、`limit` バイトまで読む
    /// (T29.1)。行が無いまま閉じたら `Ok(None)`、`limit` までに改行が来なければ `InvalidData`。
    ///
    /// 今までは `read_line` で上限が無く、改行を送らずに `f` を流し続けるだけで 1 本の要求が
    /// 何百 MiB も抱えた (クライアントの chunked の POST で +793 MiB、オリジンの chunk 行で +702 MiB)。
    fn frame_line(&mut self, limit: usize) -> io::Result<Option<Vec<u8>>> {
        let mut line = Vec::new();
        let n = (&mut *self.inner)
            .take(limit as u64)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok(None);
        }
        // 改行の前に閉じた行は、今までの `read_line` と同じくここまでを 1 行として読む
        if n == limit && line.last() != Some(&b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunked framing line too long",
            ));
        }
        Ok(Some(line))
    }

    fn read_chunked(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.chunk {
                ChunkState::Size => {
                    let Some(line) = self.frame_line(MAX_CHUNK_SIZE_LINE)? else {
                        return Err(self.truncate());
                    };
                    let size = chunk_size(&line)?;
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
                    let Some(line) = self.frame_line(MAX_CHUNK_SIZE_LINE)? else {
                        return Err(self.truncate());
                    };
                    // データの後ろは空行 (CRLF。LF だけも受ける) でなければならない (RFC 9112 §7.1。
                    // T32.15)。今までは `2\r\nokXX\r\n` の `XX` を黙って読み捨てていた
                    if !matches!(line.as_slice(), b"\r\n" | b"\n") {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "chunk data not followed by CRLF",
                        ));
                    }
                    self.chunk = ChunkState::Size;
                }
                ChunkState::Trailers => {
                    let Some(line) = self.frame_line(MAX_TRAILER_LINE)? else {
                        return Err(self.truncate());
                    };
                    // trailer は読み捨てるが、行の数だけ足していけるので合計にも上限を置く (T29.1)
                    self.trailer_bytes += line.len();
                    if self.trailer_bytes > MAX_TRAILER_BYTES {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "chunked trailer section too large",
                        ));
                    }
                    if line.trim_ascii().is_empty() {
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

/// chunk の大きさの行 (`<16 進>[;拡張]\r\n`) を読む。
///
/// 大きさは 16 進の数字だけ (RFC 9112 §7.1 の `1*HEXDIG`。T32.15)。`from_str_radix` は
/// 先頭の `+` を受けるので (`+2`)、先に字を見る。読めない行は `InvalidData`。
fn chunk_size(line: &[u8]) -> io::Result<u64> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "bad chunk size");
    let line = std::str::from_utf8(line).map_err(|_| bad())?;
    let size_str = line.trim().split(';').next().unwrap_or("").trim();
    if !size_str.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    u64::from_str_radix(size_str, 16).map_err(|_| bad())
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
    // 本文が無いなら読まない (`io::copy` は 8 KiB の置き場を毎回ゼロ埋めする。T28.12)
    if framing == Framing::None {
        return Ok(0);
    }
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

#[cfg(test)]
mod chunk_size_tests {
    use super::*;

    /// chunk の大きさの行の読み方: 16 進の数字だけ、拡張は捨てる、`+`・ごみ・空は `InvalidData`。
    #[test]
    fn hex_size_with_optional_extension() {
        assert_eq!(chunk_size(b"4\r\n").unwrap(), 4);
        assert_eq!(chunk_size(b"FF;ext=1\r\n").unwrap(), 255);
        assert_eq!(chunk_size(b" 0a \n").unwrap(), 10);
        assert_eq!(chunk_size(b"0\r\n").unwrap(), 0);
        for bad in [
            &b"+2\r\n"[..],
            b"-2\r\n",
            b"zz\r\n",
            b"\r\n",
            b"1 2\r\n",
            b"\xff\r\n",
            b"10000000000000000\r\n",
        ] {
            assert_eq!(
                chunk_size(bad).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
