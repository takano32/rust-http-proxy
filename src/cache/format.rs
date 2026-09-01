//! ディスク上のエントリ形式。
//!
//! ```text
//! SHPC2\n
//! <URL>\n
//! <stored_at> <expires_at> <flags> <payload_len>\n
//! <レスポンスの先頭 (ステータス行 + ヘッダー) + 解読済み本文>
//! ```
//!
//! `flags` は `v` (ETag / Last-Modified を持ち再検証できる) か `-`。`payload_len` はヘッダー行の
//! 後ろに続くバイト数を 20 桁ゼロ埋めで書いたもので、ストリーミング書き込みでは最初に 0 を置き、
//! 確定時に書き戻す (0 = 未確定)。読むときに実際のサイズと照合し、途中で切れたファイルを弾く。
//! 先頭部分には Content-Length / Transfer-Encoding / Connection を含めない (配信時に付け直す)。
//! 旧形式 (`SHPC1`: ワイヤ形式そのまま) は互換性が無いので起動時に捨てる。

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

pub const MAGIC: &str = "SHPC2";

/// ヘッダーを読むときに読み込む最大バイト数 (URL は通常これより十分短い)。
pub const HEADER_MAX: usize = 16 * 1024;
/// `payload_len` の桁数 (u64 の最大値が収まる)。
const LEN_DIGITS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub stored_at: u64,
    pub expires_at: u64,
    /// 再検証用のバリデータ (ETag / Last-Modified) を持つか
    pub validators: bool,
}

/// 解析したヘッダー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub meta: Meta,
    /// ペイロード (ヘッダー行の後ろ) の開始オフセット
    pub offset: usize,
    /// 記録されたペイロード長。0 (未確定) なら `None`
    pub payload_len: Option<u64>,
}

/// エントリのヘッダー行を組み立てる。`payload_len` は後で書き戻せるよう固定桁。
pub fn header(url: &str, meta: &Meta, payload_len: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(url.len() + 64);
    out.extend_from_slice(MAGIC.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(url.replace(['\n', '\r'], "").as_bytes());
    out.push(b'\n');
    out.extend_from_slice(
        format!(
            "{} {} {} {:0width$}\n",
            meta.stored_at,
            meta.expires_at,
            if meta.validators { 'v' } else { '-' },
            payload_len,
            width = LEN_DIGITS
        )
        .as_bytes(),
    );
    out
}

/// ヘッダー内の `payload_len` フィールドの位置 (書き戻し用)。
pub fn len_field_offset(header_len: usize) -> u64 {
    (header_len - LEN_DIGITS - 1) as u64
}

/// 20 桁ゼロ埋めの長さ表記。
pub fn len_field(payload_len: u64) -> String {
    format!("{:0width$}", payload_len, width = LEN_DIGITS)
}

/// ヘッダー + ペイロードを一続きに組み立てる (小さなエントリ・テスト用)。
pub fn encode(url: &str, data: &[u8], meta: &Meta) -> Vec<u8> {
    let mut blob = header(url, meta, data.len() as u64);
    blob.extend_from_slice(data);
    blob
}

/// ヘッダーを解析する。
pub fn parse_header(data: &[u8]) -> Option<Header> {
    let mut offset = 0usize;
    let mut lines = Vec::with_capacity(3);
    for _ in 0..3 {
        let nl = data[offset..].iter().position(|&b| b == b'\n')? + offset;
        lines.push(std::str::from_utf8(&data[offset..nl]).ok()?);
        offset = nl + 1;
    }
    if lines[0] != MAGIC {
        return None;
    }
    let mut fields = lines[2].split_whitespace();
    let stored_at = fields.next()?.parse().ok()?;
    let expires_at = fields.next()?.parse().ok()?;
    let validators = fields.next().is_some_and(|f| f.contains('v'));
    let payload_len = fields
        .next()
        .and_then(|f| f.parse::<u64>().ok())
        .filter(|&n| n > 0);
    Some(Header {
        meta: Meta {
            stored_at,
            expires_at,
            validators,
        },
        offset,
        payload_len,
    })
}

/// ヘッダーだけを読む (ファイル全体は読まない)。
pub fn read_meta(path: &Path) -> Option<Meta> {
    let mut file = File::open(path).ok()?;
    read_header(&mut file).ok().flatten().map(|h| h.meta)
}

/// 開いたファイルからヘッダーを読み、ファイル位置をペイロードの先頭に合わせる。
/// 形式が不正なら `Ok(None)`。
pub fn read_header(file: &mut File) -> io::Result<Option<Header>> {
    let mut buf = Vec::with_capacity(512);
    file.by_ref()
        .take(HEADER_MAX as u64)
        .read_to_end(&mut buf)?;
    let Some(h) = parse_header(&buf) else {
        return Ok(None);
    };
    file.seek(SeekFrom::Start(h.offset as u64))?;
    Ok(Some(h))
}

/// ファイル全体を読み、ヘッダーを取り除いたペイロードを返す。形式が不正・長さ不一致なら `Ok(None)`。
pub fn read_entry(path: &Path) -> io::Result<Option<(Meta, Vec<u8>)>> {
    let mut data = std::fs::read(path)?;
    let Some(h) = parse_header(&data) else {
        return Ok(None);
    };
    data.drain(..h.offset);
    if h.payload_len.is_some_and(|n| n != data.len() as u64) {
        return Ok(None);
    }
    Ok(Some((h.meta, data)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(stored_at: u64, expires_at: u64, validators: bool) -> Meta {
        Meta {
            stored_at,
            expires_at,
            validators,
        }
    }

    #[test]
    fn encode_parse_round_trip() {
        let blob = encode("http://example.com/a?b=1", b"body", &meta(100, 200, true));
        let h = parse_header(&blob).unwrap();
        assert_eq!(h.meta, meta(100, 200, true));
        assert_eq!(h.payload_len, Some(4));
        assert_eq!(&blob[h.offset..], b"body");
        // 長さフィールドの位置と書き戻し
        let hdr = header("u", &meta(1, 2, false), 0);
        let off = len_field_offset(hdr.len()) as usize;
        assert_eq!(&hdr[off..off + 20], b"00000000000000000000");
        assert_eq!(parse_header(&hdr).unwrap().payload_len, None, "0 = 未確定");
        assert_eq!(len_field(42).len(), 20);
        // flags 無し / 長さ無しも読める
        let h = parse_header(b"SHPC2\nurl\n1 2\nx").unwrap();
        assert_eq!(h.meta, meta(1, 2, false));
        assert!(parse_header(b"NOPE\nx\n1 2\n").is_none());
        assert!(parse_header(b"SHPC2\nurl\n").is_none());
        // 旧形式 (ワイヤ形式そのまま) は互換性が無いので読まない
        assert!(parse_header(b"SHPC1\nurl\n1 2\nx").is_none());
    }

    #[test]
    fn url_newlines_are_stripped() {
        let blob = encode("http://x/\r\ninjected", b"", &meta(1, 2, false));
        let h = parse_header(&blob).unwrap();
        assert_eq!(
            &blob[..h.offset],
            b"SHPC2\nhttp://x/injected\n1 2 - 00000000000000000000\n"
        );
    }

    #[test]
    fn read_meta_header_and_entry_from_file() {
        let path = std::env::temp_dir().join("shp-test-format.cache");
        let body = vec![b'z'; 100_000];
        std::fs::write(
            &path,
            encode("http://example.com/big", &body, &meta(5, 6, true)),
        )
        .unwrap();
        assert_eq!(read_meta(&path), Some(meta(5, 6, true)));

        let mut f = File::open(&path).unwrap();
        let h = read_header(&mut f).unwrap().unwrap();
        assert_eq!(h.meta.expires_at, 6);
        assert_eq!(h.payload_len, Some(100_000));
        let mut rest = Vec::new();
        f.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, body);

        let (m, got) = read_entry(&path).unwrap().unwrap();
        assert_eq!(m.expires_at, 6);
        assert_eq!(got, body);

        // 途中で切れたファイルは長さ不一致で弾く
        let mut truncated = encode("http://example.com/big", &body, &meta(5, 6, true));
        truncated.truncate(truncated.len() - 10);
        std::fs::write(&path, &truncated).unwrap();
        assert!(read_entry(&path).unwrap().is_none());

        std::fs::write(&path, b"garbage").unwrap();
        assert!(read_meta(&path).is_none());
        assert!(read_entry(&path).unwrap().is_none());
        let _ = std::fs::remove_file(&path);
    }
}
