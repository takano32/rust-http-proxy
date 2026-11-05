use super::*;
use crate::via;
use std::net::SocketAddr;

/// `Connection:` の指名の覚え方: 固定枠に収まる名前も溢れた名前も、大小を問わず引ける。
#[test]
fn connection_names_are_found_with_and_without_overflow() {
    let lines = [
        "Connection: X-A, keep-alive".to_string(),
        "X-A: 1".to_string(),
    ];
    let custom = ConnectionNames::collect(lines.iter().map(String::as_str));
    assert!(custom.contains("x-a", || lines.iter().map(String::as_str)));
    assert!(custom.contains("KEEP-ALIVE", || lines.iter().map(String::as_str)));
    assert!(!custom.contains("x-b", || lines.iter().map(String::as_str)));
    assert!(!custom.overflow);
    // 枠 (16) を超えた名前は、問い合わせのときに元の行を読み直して見つける
    let names: Vec<String> = (0..20).map(|i| format!("X-H{}", i)).collect();
    let lines = [format!("Connection: {}\r\n", names.join(", "))];
    let custom = ConnectionNames::collect(lines.iter().map(String::as_str));
    assert!(custom.overflow);
    for n in &names {
        assert!(
            custom.contains(n, || lines.iter().map(String::as_str)),
            "{}",
            n
        );
    }
    assert!(!custom.contains("X-H20", || lines.iter().map(String::as_str)));
    // 空の要素と `Connection` でない行は数えない
    let lines = [
        "Connection: , ,\r\n".to_string(),
        "Keep-Alive: X-A".to_string(),
    ];
    let custom = ConnectionNames::collect(lines.iter().map(String::as_str));
    assert_eq!(custom.n, 0);
    assert!(!custom.contains("x-a", || lines.iter().map(String::as_str)));
}

/// T28.12: 値に UTF-8 として読めないバイトがある行で終わらず、後ろの行も読む。
/// 読めない値は U+FFFD 1 字 (CL なら壊れた値として扱われる)。
#[test]
fn response_lines_read_past_obs_text() {
    let head = b"HTTP/1.1 200 OK\r\nX-Name: caf\xe9\r\nContent-Length: 5\r\n\xff\r\nContent-Length: 5\xe9\r\n\r\n";
    let lines: Vec<(&str, &str)> = response_lines(head).collect();
    assert_eq!(
        lines,
        vec![
            ("X-Name", "\u{FFFD}"),
            ("Content-Length", "5"),
            ("Content-Length", "\u{FFFD}"),
        ]
    );
}

/// T27.7: 応答の値の CR と NUL は SP になる。ふつうの行は 1 バイトも変わらない。
#[test]
fn cr_and_nul_in_a_response_value_become_spaces() {
    let head =
        b"HTTP/1.1 200 OK\r\nX-A: a\rSet-Cookie: evil=1\r\nX-B: a\0b\r\nX-C: plain\tvalue\r\n\r\n";
    let mut out = Vec::new();
    write_response_head(&mut out, head, None, &[], &[] as &[&str]);
    assert_eq!(
        out,
        b"HTTP/1.1 200 OK\r\nX-A: a Set-Cookie: evil=1\r\nX-B: a b\r\nX-C: plain\tvalue\r\n\r\n"
            .to_vec()
    );
    let h = sanitize_response_head(head);
    assert_eq!(
        h.lines,
        vec!["X-A: a Set-Cookie: evil=1", "X-B: a b", "X-C: plain\tvalue"]
    );
    assert!(matches!(
        clean_value("plain"),
        std::borrow::Cow::Borrowed(_)
    ));
}

/// T32.15 (1): 要求の行末は LF だけでも CRLF にそろえて書く。
#[test]
fn request_lines_are_written_with_crlf() {
    let raw = vec![
        "X-A: 1\n".to_string(),
        "X-B: 2\r\n".to_string(),
        "X-C: 3".to_string(),
    ];
    let mut out = Vec::new();
    write_request_headers(&mut out, &raw, None);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.starts_with("X-A: 1\r\nX-B: 2\r\nX-C: 3\r\n"),
        "{:?}",
        text
    );
    assert_eq!(
        text.matches('\n').count(),
        text.matches("\r\n").count(),
        "{:?}",
        text
    );
}

/// T32.15 (2): 名前が token でない応答の行 (`X A: b`・`: v`) は落とす (両方の書き出し口で)。
#[test]
fn response_lines_with_a_non_token_name_are_dropped() {
    let head = b"HTTP/1.1 200 OK\r\nX A: b\r\n: v\r\nX\"Q: 1\r\nX-Ok: y\r\nX-Sp : z\r\n\r\n";
    let mut out = Vec::new();
    write_response_head::<&str>(&mut out, head, None, &[], &[]);
    assert_eq!(
        String::from_utf8_lossy(&out),
        "HTTP/1.1 200 OK\r\nX-Ok: y\r\nX-Sp: z\r\n\r\n"
    );
    let h = sanitize_response_head(head);
    assert_eq!(h.lines, vec!["X-Ok: y".to_string(), "X-Sp: z".to_string()]);
}

#[test]
fn write_response_head_unfolds_obs_fold_too() {
    // 素通しの速い経路 (write_response_head) でも値を捨てない (T19.7)
    let head = b"HTTP/1.1 200 OK\r\nX-A: 1\r\n 2\r\nContent-Length: 2\r\n\r\n";
    let mut out = Vec::new();
    write_response_head::<&str>(&mut out, head, None, &[], &[]);
    assert_eq!(
        String::from_utf8_lossy(&out),
        "HTTP/1.1 200 OK\r\nX-A: 1 2\r\n\r\n"
    );
}

#[test]
fn obs_fold_in_a_response_is_unfolded_with_one_space() {
    // RFC 9112 §5.2: 中継する側は obs-fold を SP 1 個に置き換えてよい。
    // 値を捨てないこと (T19.7)
    let head = b"HTTP/1.1 200 OK\r\nX-A: 1\r\n 2\r\n\t3\r\nContent-Length: 2\r\n\r\n";
    let h = sanitize_response_head(head);
    assert_eq!(h.lines, vec!["X-A: 1 2 3".to_string()]);
}

#[test]
fn obs_fold_after_a_dropped_header_is_dropped_too() {
    // 直前の行を落とした (hop-by-hop など) ときは、その続きも一緒に落とす。
    // 別の行に混ざると値が変わる
    let head = b"HTTP/1.1 200 OK\r\nKeep-Alive: timeout=5\r\n  nasty\r\nX-B: y\r\n\r\n";
    let h = sanitize_response_head(head);
    assert_eq!(h.lines, vec!["X-B: y".to_string()]);
}

#[test]
fn response_head_is_sanitized_and_reassembled() {
    let head = b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\nConnection: close, X-Custom\r\nX-Custom: 1\r\nKeep-Alive: timeout=5\r\nAge: 12\r\nContent-Type: text/plain\nETag: \"a\"\n\n";
    let h = sanitize_response_head(head);
    assert_eq!(h.status_line, "HTTP/1.1 200 OK");
    assert_eq!(
        h.lines,
        vec![
            "Content-Type: text/plain".to_string(),
            "ETag: \"a\"".to_string()
        ]
    );
    let bytes = h.assemble(&["Content-Length: 2".to_string()]);
    assert_eq!(
        bytes,
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nETag: \"a\"\r\nContent-Length: 2\r\n\r\n"
    );
    assert_eq!(
        sanitize_response_head(b"garbage").status_line,
        "HTTP/1.1 200 OK"
    );
}

#[test]
fn test_hop_by_hop_removal() {
    let raw = vec![
        "Host: example.com\r\n".to_string(),
        "Proxy-Connection: keep-alive\r\n".to_string(),
        "Keep-Alive: timeout=5\r\n".to_string(),
        "Connection: close, X-Foo\r\n".to_string(),
        "X-Foo: bar\r\n".to_string(),
        "User-Agent: curl/7.88.1\r\n".to_string(),
    ];
    let addr: SocketAddr = "192.168.1.100:54321".parse().unwrap();
    let cleaned = sanitize_and_inject_headers(&raw, Some(addr));

    assert!(cleaned.iter().any(|h| h.starts_with("Host: example.com")));
    assert!(
        cleaned
            .iter()
            .any(|h| h.starts_with("User-Agent: curl/7.88.1"))
    );
    assert!(
        cleaned
            .iter()
            .any(|h| h.starts_with("X-Forwarded-For: 192.168.1.100"))
    );
    assert!(cleaned.iter().any(|h| h == &via::line().to_string()));

    assert!(
        !cleaned
            .iter()
            .any(|h| h.to_ascii_lowercase().starts_with("proxy-connection"))
    );
    assert!(
        !cleaned
            .iter()
            .any(|h| h.to_ascii_lowercase().starts_with("keep-alive"))
    );
    assert!(
        !cleaned
            .iter()
            .any(|h| h.to_ascii_lowercase().starts_with("x-foo"))
    );
}
