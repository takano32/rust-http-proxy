//! キャッシュ済みレスポンスの表現: 先頭 (ステータス行 + ヘッダー) と、メモリまたはファイル上の本文。

use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, Read};
use std::sync::Arc;

use super::format::Meta;
use super::now_epoch;

/// ヘッダー部の上限 (これを超える場合は壊れているとみなす)。
const HEAD_MAX: usize = 1024 * 1024;

/// `Cursor` で読むための `Arc<Vec<u8>>` ラッパ。
struct ArcBytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for ArcBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// キャッシュ済みレスポンスの本文 (ヘッダー部の後ろ)。
pub enum Body {
    Memory { data: Arc<Vec<u8>>, offset: usize },
    File(BufReader<File>),
}

/// キャッシュ済みレスポンス。`head` はステータス行 + ヘッダー + 空行、`body` はその続き。
pub struct CachedResponse {
    pub head: Vec<u8>,
    pub body: Body,
    /// ワイヤバイト列全体の長さ (head + body)
    pub size: u64,
    pub meta: Meta,
}

impl CachedResponse {
    pub fn is_fresh(&self, now: u64) -> bool {
        self.meta.expires_at > now
    }

    pub fn age(&self) -> u64 {
        now_epoch().saturating_sub(self.meta.stored_at)
    }

    pub fn ttl_left(&self, now: u64) -> u64 {
        self.meta.expires_at.saturating_sub(now)
    }

    /// 本文の長さ (ヘッダー部を除く)。
    pub fn body_len(&self) -> u64 {
        self.size.saturating_sub(self.head.len() as u64)
    }

    /// 本文の `start` から `len` バイトを読むリーダー (Range 応答用)。
    pub fn into_body_range(self, start: u64, len: u64) -> Box<dyn Read + Send> {
        match self.body {
            Body::Memory { data, offset } => {
                let mut cur = Cursor::new(ArcBytes(data));
                cur.set_position(offset as u64 + start);
                Box::new(cur.take(len))
            }
            Body::File(mut reader) => {
                if start > 0 {
                    // 先頭から start バイト読み飛ばす (BufReader の位置を保つため seek は使わない)
                    let _ = io::copy(&mut (&mut reader).take(start), &mut io::sink());
                }
                Box::new(reader.take(len))
            }
        }
    }

    /// 本文を読むリーダー。
    pub fn into_body_reader(self) -> Box<dyn Read + Send> {
        match self.body {
            Body::Memory { data, offset } => {
                let mut cur = Cursor::new(ArcBytes(data));
                cur.set_position(offset as u64);
                Box::new(cur)
            }
            Body::File(reader) => Box::new(reader),
        }
    }

    /// ワイヤバイト列全体を読む (テスト・小さなエントリ用)。
    pub fn read_all(self) -> io::Result<Vec<u8>> {
        let mut out = self.head.clone();
        self.into_body_reader().read_to_end(&mut out)?;
        Ok(out)
    }
}

/// ワイヤバイト列の先頭からヘッダー部 (空行まで) を読む。
pub(super) fn read_head<R: BufRead>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(512);
    loop {
        let start = head.len();
        let n = reader.read_until(b'\n', &mut head)?;
        if n == 0 || head.len() > HEAD_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cached response has no header terminator",
            ));
        }
        let line = &head[start..];
        if line == b"\r\n" || line == b"\n" {
            return Ok(head);
        }
    }
}

/// 先頭部分 (空行まで) の長さ。CRLF でも素の LF でも受け付ける。
pub(super) fn head_len(wire: &[u8]) -> usize {
    let mut i = 0;
    while i < wire.len() {
        let nl = match wire[i..].iter().position(|&b| b == b'\n') {
            Some(p) => i + p,
            None => return wire.len(),
        };
        let line = &wire[i..nl];
        if line.is_empty() || line == b"\r" {
            return nl + 1;
        }
        i = nl + 1;
    }
    wire.len()
}
