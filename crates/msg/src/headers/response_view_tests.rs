use super::*;

/// 生の先頭を 1 行ずつ読む版が、以前の「読みながら組を作る」実装と同じ結果になること。
fn old_way(head: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(head);
    let mut out = Vec::new();
    for line in text.split_inclusive('\n').skip(1) {
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            out.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    out
}

#[test]
fn response_pairs_matches_the_previous_implementation() {
    let cases: Vec<&[u8]> = vec![
        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\n",
        b"HTTP/1.0 404 Not Found\r\nServer: x\r\nConnection: close\r\n\r\n",
        b"HTTP/1.1 200 OK\nA:b\nNoColon\nC:  d  \n\n",
        b"HTTP/1.1 204 No Content\r\n\r\n",
        // 空行のあとに何かあっても読まない
        b"HTTP/1.1 200 OK\r\nA: b\r\n\r\nbody",
        // 終端の空行が無い (相手が途中で閉じた)
        b"HTTP/1.1 200 OK\r\nA: b\r\n",
    ];
    for head in cases {
        assert_eq!(
            response_pairs(head),
            old_way(head),
            "head={:?}",
            String::from_utf8_lossy(head)
        );
    }
}

#[test]
fn response_value_and_names() {
    let head = b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: keep-alive, X-Odd\r\n\r\n";
    assert_eq!(response_value(head, "content-length"), Some("12"));
    assert_eq!(response_value(head, "CONTENT-LENGTH"), Some("12"));
    assert_eq!(response_value(head, "missing"), None);
    assert!(response_names(head, "connection", "x-odd"));
    assert!(!response_names(head, "connection", "close"));
}
