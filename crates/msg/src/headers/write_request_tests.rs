use super::*;
use std::net::SocketAddr;

/// 従来の `sanitize_and_inject_headers` + Host 除去 + 連結と、1 バイトも違わないこと。
fn old_way(headers: &[String], addr: Option<SocketAddr>) -> Vec<u8> {
    let mut out = Vec::new();
    for h in sanitize_and_inject_headers(headers, addr)
        .into_iter()
        .filter(|h| !h.trim_start().to_ascii_lowercase().starts_with("host:"))
    {
        out.extend_from_slice(h.as_bytes());
    }
    out
}

fn lines(v: &[&str]) -> Vec<String> {
    v.iter().map(|l| format!("{}\r\n", l)).collect()
}

#[test]
fn matches_the_previous_implementation_byte_for_byte() {
    let addr: SocketAddr = "203.0.113.7:1234".parse().unwrap();
    let cases: Vec<Vec<String>> = vec![
        lines(&["Host: example.com", "Accept: */*"]),
        lines(&["Host: example.com", "Connection: keep-alive", "Accept: */*"]),
        // Connection で独自の hop-by-hop を指名する
        lines(&[
            "Connection: X-Private, Upgrade",
            "X-Private: secret",
            "Upgrade: websocket",
            "Accept: */*",
        ]),
        // 既存の Via と X-Forwarded-For を引き継ぐ
        lines(&["Via: 1.0 upstream", "X-Forwarded-For: 198.51.100.1", "A: b"]),
        // hop-by-hop 一式
        lines(&[
            "Keep-Alive: timeout=5",
            "Proxy-Authorization: Basic x",
            "TE: trailers",
            "Transfer-Encoding: chunked",
            "Cookie: a=b",
        ]),
        // 大文字小文字の混在と空白
        lines(&[
            "hOsT:  example.com ",
            "cOnNeCtIoN: X-Odd",
            "X-ODD: 1",
            "Z:  v  ",
        ]),
        // 区切りの無い行 (無視される)
        vec!["NoColon\r\n".to_string(), "A: b\r\n".to_string()],
        // `Expect` は転送しない (T.28.3。どちらの実装も落とす)
        lines(&["eXpEcT: 100-continue", "Content-Length: 5", "A: b"]),
        Vec::new(),
    ];
    for headers in &cases {
        for client in [Some(addr), None] {
            let ip = client.map(|a: SocketAddr| a.ip().to_string());
            let mut got = Vec::new();
            write_request_headers(&mut got, headers, ip.as_deref());
            assert_eq!(
                String::from_utf8_lossy(&got),
                String::from_utf8_lossy(&old_way(headers, client)),
                "headers={:?} client={:?}",
                headers,
                client
            );
        }
    }
}

/// 同値の `Content-Length` は 1 本に畳んで転送する (T19.1)。
/// 2 本書くとオリジンによっては 400 になり、判定 (1 本ぶんの本文) と食い違う。
#[test]
fn forwards_only_one_content_length() {
    let headers = lines(&[
        "Host: example.com",
        "Content-Length: 5",
        "Accept: */*",
        "Content-Length: 5",
    ]);
    let mut got = Vec::new();
    write_request_headers(&mut got, &headers, None);
    let text = String::from_utf8_lossy(&got);
    assert_eq!(
        text.matches("Content-Length:").count(),
        1,
        "CL が 1 本でない: {:?}",
        text
    );
    assert_eq!(
        text.matches("content-length:").count(),
        0,
        "小文字の綴りが残っている: {:?}",
        text
    );
    // 残すのは最初の 1 本 (値は書き換えない)
    assert!(text.contains("Content-Length: 5\r\n"), "{:?}", text);
    // 他のヘッダーは落とさない
    assert!(text.contains("Accept: */*"), "{:?}", text);
    // 1 本だけの要求は今までどおり
    let headers = lines(&["Content-Length: 5"]);
    let mut got = Vec::new();
    write_request_headers(&mut got, &headers, None);
    assert!(String::from_utf8_lossy(&got).contains("Content-Length: 5"));
}

#[test]
fn falls_back_when_connection_names_more_than_the_fixed_slots() {
    // 固定枠 (16) を超える指名でも、全部 hop-by-hop として落とすこと
    let mut names: Vec<String> = (0..20).map(|i| format!("X-H{}", i)).collect();
    let mut headers = vec![format!("Connection: {}\r\n", names.join(", "))];
    for n in &names {
        headers.push(format!("{}: v\r\n", n));
    }
    headers.push("Accept: */*\r\n".to_string());
    let mut got = Vec::new();
    write_request_headers(&mut got, &headers, None);
    let text = String::from_utf8_lossy(&got);
    names.retain(|n| text.contains(n.as_str()));
    assert!(names.is_empty(), "指名された名前が残っている: {:?}", names);
    assert!(text.contains("Accept: */*"));
    assert_eq!(String::from_utf8_lossy(&old_way(&headers, None)), text);
}
