//! ディスク上のエントリ形式。
//!
//! ```text
//! SHPC1\n
//! <URL>\n
//! <stored_at> <expires_at>\n
//! <レスポンスのワイヤバイト列>
//! ```
//!
//! 起動時の走査はファイルの mtime (= expires_at) だけを見るので、ヘッダーは
//! ヒット時と mtime が信用できないときにしか読まない。

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

pub const MAGIC: &str = "SHPC1";

/// ヘッダーを読むときに読み込む最大バイト数 (URL は通常これより十分短い)。
const HEADER_MAX: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub stored_at: u64,
    pub expires_at: u64,
}

/// エントリ 1 件分のファイル内容を組み立てる。
pub fn encode(url: &str, data: &[u8], stored_at: u64, expires_at: u64) -> Vec<u8> {
    let mut blob = Vec::with_capacity(data.len() + url.len() + 64);
    blob.extend_from_slice(MAGIC.as_bytes());
    blob.push(b'\n');
    blob.extend_from_slice(url.replace(['\n', '\r'], "").as_bytes());
    blob.push(b'\n');
    blob.extend_from_slice(format!("{} {}\n", stored_at, expires_at).as_bytes());
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
    let mut nums = lines[2].split_whitespace();
    let stored_at = nums.next()?.parse().ok()?;
    let expires_at = nums.next()?.parse().ok()?;
    Some((
        Meta {
            stored_at,
            expires_at,
        },
        offset,
    ))
}

/// ヘッダーだけを読む (ファイル全体は読まない)。
pub fn read_meta(path: &Path) -> Option<Meta> {
    let mut file = File::open(path).ok()?;
    let mut buf = Vec::with_capacity(512);
    file.by_ref()
        .take(HEADER_MAX as u64)
        .read_to_end(&mut buf)
        .ok()?;
    parse_header(&buf).map(|(meta, _)| meta)
}

/// ファイル全体を読み、ヘッダーを取り除いたボディを返す。形式が不正なら `Ok(None)`。
pub fn read_entry(path: &Path) -> io::Result<Option<(Meta, Vec<u8>)>> {
    let mut data = fs::read(path)?;
    Ok(parse_header(&data).map(|(meta, offset)| {
        data.drain(..offset);
        (meta, data)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_parse_round_trip() {
        let blob = encode("http://example.com/a?b=1", b"body", 100, 200);
        let (meta, off) = parse_header(&blob).unwrap();
        assert_eq!(
            meta,
            Meta {
                stored_at: 100,
                expires_at: 200
            }
        );
        assert_eq!(&blob[off..], b"body");
        assert!(parse_header(b"NOPE\nx\n1 2\n").is_none());
        assert!(parse_header(b"SHPC1\nurl\n").is_none());
    }

    #[test]
    fn url_newlines_are_stripped() {
        let blob = encode("http://x/\r\ninjected", b"", 1, 2);
        let (_, off) = parse_header(&blob).unwrap();
        assert_eq!(&blob[..off], b"SHPC1\nhttp://x/injected\n1 2\n");
    }

    #[test]
    fn read_meta_and_entry_from_file() {
        let path = std::env::temp_dir().join("shp-test-format.cache");
        let body = vec![b'z'; 100_000];
        fs::write(&path, encode("http://example.com/big", &body, 5, 6)).unwrap();
        assert_eq!(
            read_meta(&path),
            Some(Meta {
                stored_at: 5,
                expires_at: 6
            })
        );
        let (meta, got) = read_entry(&path).unwrap().unwrap();
        assert_eq!(meta.expires_at, 6);
        assert_eq!(got, body);
        fs::write(&path, b"garbage").unwrap();
        assert!(read_meta(&path).is_none());
        assert!(read_entry(&path).unwrap().is_none());
        let _ = fs::remove_file(&path);
    }
}
