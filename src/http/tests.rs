//! `http` の単体テスト。

use super::request::*;
use super::serve::*;
use super::*;
use crate::body::RangeSpec;
use crate::cache::{Body, Meta};
use crate::origin::Scheme;
use std::sync::Arc;

#[test]
fn test_parse_origin_forms() {
    let o = parse_origin("http://example.com/test?a=1", None).unwrap();
    assert_eq!(
        (o.scheme, o.host_port.as_str(), o.path.as_str(), o.mapped),
        (Scheme::Http, "example.com", "/test?a=1", false)
    );
    assert_eq!(o.server_addr(), "example.com:80");
    assert_eq!(o.url(), "http://example.com:80/test?a=1");

    let o = parse_origin("https://example.com", None).unwrap();
    assert_eq!((o.scheme, o.path.as_str()), (Scheme::Https, "/"));
    assert_eq!(o.server_addr(), "example.com:443");
    assert_eq!(o.pool_key(), "https://example.com:443");

    let o = parse_origin("/https/[2001:db8::1]:8443/v6", None).unwrap();
    assert_eq!(
        (o.scheme, o.host_port.as_str(), o.path.as_str(), o.mapped),
        (Scheme::Https, "[2001:db8::1]:8443", "/v6", true)
    );
    assert_eq!(o.host(), "2001:db8::1");
    assert_eq!(o.server_addr(), "[2001:db8::1]:8443");

    let o = parse_origin("/http/example.com", None).unwrap();
    assert_eq!(
        (o.scheme, o.path.as_str(), o.mapped),
        (Scheme::Http, "/", true)
    );

    let o = parse_origin("/index.html", Some("example.com")).unwrap();
    assert_eq!(
        (o.scheme, o.host_port.as_str(), o.path.as_str()),
        (Scheme::Http, "example.com", "/index.html")
    );
    assert!(parse_origin("/index.html", None).is_err());
    assert!(parse_origin("/https/", None).is_err());
    assert!(parse_origin("/https//path", None).is_err());
}

#[test]
fn test_map_locations() {
    let mut lines = vec![
        "Location: https://example.com/next".to_string(),
        "Content-Location: http://example.com:8080/x".to_string(),
        "X-Other: https://keep.me/".to_string(),
        "Location: /relative".to_string(),
    ];
    map_locations(&mut lines);
    assert_eq!(lines[0], "Location: /https/example.com/next");
    assert_eq!(lines[1], "Content-Location: /http/example.com:8080/x");
    assert_eq!(lines[2], "X-Other: https://keep.me/");
    assert_eq!(lines[3], "Location: /relative");
}

#[test]
fn test_read_response_head() {
    let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nCache-Control: max-age=60\r\n\r\nhello";
    let mut reader = BufReader::new(&raw[..]);
    let (head, status, headers) = read_response_head(&mut reader).unwrap();
    assert_eq!(status, 200);
    assert!(head.ends_with(b"\r\n\r\n"));
    assert_eq!(headers.len(), 2);
    assert_eq!(
        headers[1],
        ("cache-control".to_string(), "max-age=60".to_string())
    );

    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    assert_eq!(rest, b"hello");
}

fn cached(wire: &[u8]) -> CachedResponse {
    let offset = wire.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    CachedResponse {
        head: wire[..offset].to_vec(),
        size: wire.len() as u64,
        body: Body::Memory {
            data: Arc::new(wire.to_vec()),
            offset,
        },
        meta: Meta {
            stored_at: 0,
            expires_at: u64::MAX,
            validators: false,
        },
    }
}

fn serve(range: RangeSpec, head_only: bool) -> Serve<'static> {
    Serve {
        label: "HIT",
        source: CacheSource::Disk,
        age: 42,
        keep_alive: true,
        head_only,
        range,
        map_locations: false,
    }
}

#[test]
fn test_write_cached_response_injects_framing_and_headers() {
    let entry =
        cached(b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nhi");
    let mut out = Vec::new();
    let (status, n) =
        write_cached_response(&mut out, entry, &serve(RangeSpec::Ignore, false)).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(status, 200);
    assert!(text.starts_with("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nX-Cache: HIT from rust-http-proxy (disk)\r\nAge: 42\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nhi"), "{}", text);
    assert!(!text.contains("Connection: close"));
    assert_eq!(n, text.len() as u64);
}

#[test]
fn test_write_cached_response_range_and_head() {
    let wire = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n0123456789";
    let mut out = Vec::new();
    let (status, _) = write_cached_response(
        &mut out,
        cached(wire),
        &serve(RangeSpec::Bytes { start: 2, end: 5 }, false),
    )
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(status, 206);
    assert!(
        text.starts_with("HTTP/1.1 206 Partial Content\r\n"),
        "{}",
        text
    );
    assert!(text.contains("Content-Range: bytes 2-5/10\r\n"));
    assert!(text.contains("Content-Length: 4\r\n"));
    assert!(text.ends_with("\r\n\r\n2345"));

    let mut out = Vec::new();
    let (status, _) = write_cached_response(
        &mut out,
        cached(wire),
        &serve(RangeSpec::Unsatisfiable, false),
    )
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(status, 416);
    assert!(text.contains("Content-Range: bytes */10\r\n") && text.ends_with("\r\n\r\n"));

    let mut out = Vec::new();
    let (status, _) =
        write_cached_response(&mut out, cached(wire), &serve(RangeSpec::Ignore, true)).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(status, 200);
    assert!(
        text.contains("Content-Length: 10\r\n") && text.ends_with("\r\n\r\n"),
        "{}",
        text
    );
}

#[test]
fn test_if_range_matching() {
    let head = freshness::parse_cached_head(
        b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\n\r\n",
    );
    assert!(if_range_matches(None, &head));
    assert!(if_range_matches(Some("\"v1\""), &head));
    assert!(!if_range_matches(Some("\"v2\""), &head));
    assert!(!if_range_matches(Some("W/\"v1\""), &head));
    assert!(if_range_matches(
        Some("Sun, 06 Nov 1994 08:49:37 GMT"),
        &head
    ));
    assert!(!if_range_matches(
        Some("Mon, 07 Nov 1994 08:49:37 GMT"),
        &head
    ));
}

#[test]
fn test_write_not_modified_keeps_validators_only() {
    let head = freshness::parse_cached_head(
        b"HTTP/1.1 200 OK\r\nETag: \"x\"\r\nContent-Length: 2\r\nContent-Type: text/plain\r\n\r\n",
    );
    let mut out = Vec::new();
    write_not_modified(&mut out, &head, "HIT", CacheSource::Memory, 3, false).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with(
        "HTTP/1.1 304 Not Modified\r\nX-Cache: HIT from rust-http-proxy (memory)\r\nAge: 3\r\n"
    ));
    assert!(text.contains("ETag: \"x\"\r\n"));
    assert!(!text.contains("Content-Length"));
    assert!(text.ends_with("Connection: close\r\n\r\n"));
}
