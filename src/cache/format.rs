//! ディスク上のエントリ形式。
//!
//! ```text
//! SHPC1\n
//! <URL>\n
//! <stored_at> <expires_at> [flags]\n
//! <レスポンスのワイヤバイト列 (ステータス行 + ヘッダー + ボディ)>
//! ```
//!
//! `flags` は省略可能で、`v` が含まれていれば ETag / Last-Modified を持ち再検証できる。
//! 起動時の走査はファイルの mtime (= expires_at) だけを見るので、ヘッダーは
//! ヒット時・期限切れの確認時にしか読まない。

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

pub const MAGIC: &str = "SHPC1";

/// ヘッダーを読むときに読み込む最大バイト数 (URL は通常これより十分短い)。
const HEADER_MAX: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub stored_at: u64,
    pub expires_at: u64,
    /// 再検証用のバリデータ (ETag / Last-Modified) を持つか
    pub validators: bool,
}

/// エントリ 1 件分のヘッダー行を組み立てる。
pub fn header(url: &str, meta: &Meta) -> Vec<u8> {
    let mut out = Vec::with_capacity(url.len() + 64);
    out.extend_from_slice(MAGIC.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(url.replace(['\n', '\r'], "").as_bytes());
    out.push(b'\n');
    let flags = if meta.validators { " v" } else { "" };
    out.extend_from_slice(format!("{} {}{}\n", meta.stored_at, meta.expires_at, flags).as_bytes());
    out
}

/// ヘッダー + ボディを一続きに組み立てる (小さなエントリ・テスト用)。
pub fn encode(url: &str, data: &[u8], meta: &Meta) -> Vec<u8> {
    let mut blob = header(url, meta);
    blob.extend_from_slice(data);
    blob
}

/// ヘッダーを解析し、(メタ情報, ボディ開始オフセット) を返す。
pub fn parse_header(data: &[u8]) -> Option<(Meta, usize)> {
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
    Some((
        Meta {
            stored_at,
            expires_at,
            validators,
        },
        offset,
    ))
}

/// ヘッダーだけを読む (ファイル全体は読まない)。
pub fn read_meta(path: &Path) -> Option<Meta> {
    let mut file = File::open(path).ok()?;
    read_header(&mut file).ok().flatten().map(|(meta, _)| meta)
}

/// 開いたファイルからヘッダーを読み、ファイル位置をボディの先頭に合わせる。
/// 形式が不正なら `Ok(None)`。戻り値は (メタ情報, ボディ開始オフセット)。
pub fn read_header(file: &mut File) -> io::Result<Option<(Meta, u64)>> {
    let mut buf = Vec::with_capacity(512);
    file.by_ref()
        .take(HEADER_MAX as u64)
        .read_to_end(&mut buf)?;
    let Some((meta, offset)) = parse_header(&buf) else {
        return Ok(None);
    };
    file.seek(SeekFrom::Start(offset as u64))?;
    Ok(Some((meta, offset as u64)))
}

/// ファイル全体を読み、ヘッダーを取り除いたボディを返す。形式が不正なら `Ok(None)`。
pub fn read_entry(path: &Path) -> io::Result<Option<(Meta, Vec<u8>)>> {
    let mut data = std::fs::read(path)?;
    Ok(parse_header(&data).map(|(meta, offset)| {
        data.drain(..offset);
        (meta, data)
    }))
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
        let (m, off) = parse_header(&blob).unwrap();
        assert_eq!(m, meta(100, 200, true));
        assert_eq!(&blob[off..], b"body");
        // flags 無し (旧形式) も読める
        let (m, _) = parse_header(b"SHPC1\nurl\n1 2\nx").unwrap();
        assert_eq!(m, meta(1, 2, false));
        assert!(parse_header(b"NOPE\nx\n1 2\n").is_none());
        assert!(parse_header(b"SHPC1\nurl\n").is_none());
    }

    #[test]
    fn url_newlines_are_stripped() {
        let blob = encode("http://x/\r\ninjected", b"", &meta(1, 2, false));
        let (_, off) = parse_header(&blob).unwrap();
        assert_eq!(&blob[..off], b"SHPC1\nhttp://x/injected\n1 2\n");
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
        let (m, offset) = read_header(&mut f).unwrap().unwrap();
        assert_eq!(m.expires_at, 6);
        let mut rest = Vec::new();
        f.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, body);
        assert_eq!(
            offset as usize + body.len(),
            std::fs::metadata(&path).unwrap().len() as usize
        );

        let (m, got) = read_entry(&path).unwrap().unwrap();
        assert_eq!(m.expires_at, 6);
        assert_eq!(got, body);
        std::fs::write(&path, b"garbage").unwrap();
        assert!(read_meta(&path).is_none());
        assert!(read_entry(&path).unwrap().is_none());
        let _ = std::fs::remove_file(&path);
    }
}
