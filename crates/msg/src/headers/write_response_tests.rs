use super::*;

fn old_way(
    head: &[u8],
    status_line: Option<&str>,
    drop_names: &[&str],
    extra: &[String],
) -> Vec<u8> {
    let mut h = sanitize_response_head(head);
    if let Some(sl) = status_line {
        h.status_line = sl.to_string();
    }
    h.lines.retain(|l| !line_named(l, drop_names));
    h.assemble(extra)
}

#[test]
fn matches_the_previous_implementation_byte_for_byte() {
    let cases: Vec<&[u8]> = vec![
        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\n",
        b"HTTP/1.0 404 Not Found\r\nServer: x\r\nConnection: close\r\n\r\n",
        // Connection で独自の hop-by-hop を指名
        b"HTTP/1.1 200 OK\r\nConnection: X-Odd, Upgrade\r\nX-Odd: 1\r\nUpgrade: h2c\r\nA: b\r\n\r\n",
        // 枠組みのヘッダーは落ちる
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nAge: 5\r\nETag: \"x\"\r\n\r\n",
        // 空白の混ざり方と大文字小文字
        b"HTTP/1.1 301 Moved Permanently\r\n  \r\nlOcAtIoN:   /elsewhere  \r\nx:y\r\n\r\n",
        // 区切りの無い行
        b"HTTP/1.1 200 OK\r\nNoColon\r\nA: b\r\n\r\n",
        // CR 無し
        b"HTTP/1.1 204 No Content\nA: b\n\n",
    ];
    let extras = [
        Vec::new(),
        vec![
            "Content-Length: 5".to_string(),
            "Connection: keep-alive".to_string(),
        ],
    ];
    for head in cases {
        for extra in &extras {
            for sl in [None, Some("HTTP/1.1 206 Partial Content")] {
                // 落とすヘッダーの指名あり / なし の両方で、組を作る版と一致すること
                for drop_names in [&[] as &[&str], &["content-type"], &["A", "x"]] {
                    let mut got = Vec::new();
                    write_response_head(&mut got, head, sl, drop_names, extra);
                    assert_eq!(
                        String::from_utf8_lossy(&got),
                        String::from_utf8_lossy(&old_way(head, sl, drop_names, extra)),
                        "head={:?} sl={:?} drop={:?} extra={:?}",
                        String::from_utf8_lossy(head),
                        sl,
                        drop_names,
                        extra
                    );
                }
            }
        }
    }
}

/// `Connection:` の指名が固定枠 (16) を超える応答でも、組を作る版と同じ行が落ちること
/// (溢れたぶんは元の行を読み直す)。48 本を超える行も同じに書ける (固定枠の外へ溢れる)。
#[test]
fn many_connection_names_and_many_lines_match_the_previous_implementation() {
    let names: Vec<String> = (0..20).map(|i| format!("X-H{}", i)).collect();
    let mut head = format!("HTTP/1.1 200 OK\r\nConnection: {}\r\n", names.join(", ")).into_bytes();
    for n in &names {
        head.extend_from_slice(format!("{}: v\r\n", n).as_bytes());
    }
    for i in 0..60 {
        head.extend_from_slice(format!("X-Keep-{}: {}\r\n", i, i).as_bytes());
    }
    head.extend_from_slice(b"\r\n");
    let mut got = Vec::new();
    write_response_head::<&str>(&mut got, &head, None, &[], &[]);
    let text = String::from_utf8_lossy(&got);
    for n in &names {
        assert!(!text.contains(n.as_str()), "{} が残っている", n);
    }
    assert!(text.contains("X-Keep-59: 59\r\n"));
    assert_eq!(got, old_way(&head, None, &[], &[]));
}
