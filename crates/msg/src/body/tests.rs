use super::*;
use std::io::{self, BufRead, BufReader, Read};

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

/// T32.15: データの後ろのごみ・`+` 付きの大きさは `InvalidData`。LF だけの行は今までどおり受ける。
#[test]
fn chunked_rejects_junk_after_data_and_signed_sizes() {
    for raw in [
        &b"2\r\nokXX\r\n0\r\n\r\n"[..],
        b"+2\r\nok\r\n0\r\n\r\n",
        b"-2\r\nok\r\n0\r\n\r\n",
        b"2\r\nok 0\r\n\r\n",
        b"\r\nok\r\n0\r\n\r\n",
    ] {
        let (out, _) = read_all(&mut BufReader::new(raw), Framing::Chunked);
        assert_eq!(
            out.unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "{:?}",
            String::from_utf8_lossy(raw)
        );
    }
    let (out, clean) = read_all(&mut BufReader::new(&b"2\nok\n0\n\n"[..]), Framing::Chunked);
    assert_eq!(out.unwrap(), b"ok");
    assert!(clean);
}

/// T29.1: chunk の大きさの行・データの後ろの行・trailer の行と合計に上限がある。
/// 上限を越えたら、そこまでしか読まずに `InvalidData` で止まる。
#[test]
fn chunked_framing_lines_are_bounded() {
    // 改行の無い大きさの行 (`f` の連打): 上限ぶん読んだところで止まる
    let raw = vec![b'f'; 1024 * 1024];
    let mut r = BufReader::new(&raw[..]);
    let (out, _) = read_all(&mut r, Framing::Chunked);
    assert_eq!(out.unwrap_err().kind(), io::ErrorKind::InvalidData);
    let mut rest = Vec::new();
    r.read_to_end(&mut rest).unwrap();
    assert!(
        raw.len() - rest.len() <= MAX_CHUNK_SIZE_LINE + 8 * 1024,
        "読みすぎ: {}",
        raw.len() - rest.len()
    );
    // 拡張が長いだけの大きさの行も同じ
    let mut raw = b"5;".to_vec();
    raw.extend(std::iter::repeat_n(b'a', MAX_CHUNK_SIZE_LINE));
    raw.extend_from_slice(b"\r\nhello\r\n0\r\n\r\n");
    let (out, _) = read_all(&mut BufReader::new(&raw[..]), Framing::Chunked);
    assert!(out.is_err());
    // データの後ろに CRLF の代わりに長い行
    let mut raw = b"5\r\nhello".to_vec();
    raw.extend(std::iter::repeat_n(b'x', MAX_CHUNK_SIZE_LINE + 1));
    let (out, _) = read_all(&mut BufReader::new(&raw[..]), Framing::Chunked);
    assert!(out.is_err());
    // trailer の 1 行が長い
    let mut raw = b"0\r\nX-T: ".to_vec();
    raw.extend(std::iter::repeat_n(b'x', MAX_TRAILER_LINE));
    raw.extend_from_slice(b"\r\n\r\n");
    let (out, _) = read_all(&mut BufReader::new(&raw[..]), Framing::Chunked);
    assert!(out.is_err());
    // trailer の 1 行は短いが、本数で合計を越える
    let mut raw = b"0\r\n".to_vec();
    while raw.len() < MAX_TRAILER_BYTES + 1024 {
        raw.extend_from_slice(b"X-T: 0123456789\r\n");
    }
    raw.extend_from_slice(b"\r\n");
    let (out, _) = read_all(&mut BufReader::new(&raw[..]), Framing::Chunked);
    assert!(out.is_err());
    // 上限の内側は今までどおり読める (拡張つき・trailer つき)
    let mut raw = b"5;".to_vec();
    raw.extend(std::iter::repeat_n(b'a', 1000));
    raw.extend_from_slice(b"\r\nhello\r\n0\r\nX-T: ");
    raw.extend(std::iter::repeat_n(b'y', 4000));
    raw.extend_from_slice(b"\r\n\r\n");
    let (out, clean) = read_all(&mut BufReader::new(&raw[..]), Framing::Chunked);
    assert_eq!(out.unwrap(), b"hello");
    assert!(clean);
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
