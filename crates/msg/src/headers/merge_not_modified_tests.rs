use super::*;

/// 304 の名前は全部差し替え、無い名前と状態行は残す。枠組み・hop-by-hop・`Age`・
/// `Content-Encoding` は写さない (T28.2)。
#[test]
fn replaces_named_fields_and_keeps_the_rest() {
    let stored = b"HTTP/1.1 200 OK\r\nCache-Control: max-age=1\r\nX-Ver: 1\r\nX-Ver: 1b\r\n\
                   Content-Type: text/plain\r\nContent-Encoding: gzip\r\nETag: \"v1\"\r\n\r\n";
    let not_modified = b"HTTP/1.1 304 Not Modified\r\ncache-control: max-age=600\r\nX-Ver: 2\r\n\
                         ETag: \"v1\"\r\nContent-Length: 99\r\nTransfer-Encoding: chunked\r\n\
                         Connection: close, X-Hop\r\nX-Hop: 1\r\nKeep-Alive: timeout=5\r\nAge: 7\r\n\
                         Content-Encoding: br\r\nX-Bad: a\rb\r\n\r\n";
    let merged = merge_not_modified(stored, not_modified).expect("写すものがある");
    let text = String::from_utf8(merged).unwrap();
    assert_eq!(
        text,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Encoding: gzip\r\n\
         cache-control: max-age=600\r\nX-Ver: 2\r\nETag: \"v1\"\r\nX-Bad: a b\r\n\r\n"
    );
}

/// 304 の `Set-Cookie` / `Set-Cookie2` は保存する先頭へ写さず、保存済みに残っていた写しも
/// 落とす (T29.0)。配る写しには [`with_set_cookies`] で 304 の分だけを足す。
#[test]
fn set_cookie_is_not_merged_but_handed_to_the_requester() {
    let stored = b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nSet-Cookie: old=leak\r\n\r\n";
    let not_modified = b"HTTP/1.1 304 Not Modified\r\nSet-Cookie: s=A\r\nset-cookie2: t=A\r\n\
                         ETag: \"v1\"\r\n\r\n";
    let merged = merge_not_modified(stored, not_modified).expect("ETag を写す");
    assert_eq!(merged, b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\n\r\n".to_vec());
    let served = with_set_cookies(&merged, not_modified).expect("Set-Cookie がある");
    assert_eq!(
        String::from_utf8(served).unwrap(),
        "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nSet-Cookie: s=A\r\nset-cookie2: t=A\r\n\r\n"
    );
    assert!(with_set_cookies(&merged, b"HTTP/1.1 304 Not Modified\r\nETag: x\r\n\r\n").is_none());
}

/// 写すものが無い 304 (枠組みと hop-by-hop だけ) は `None` (呼ぶ側は期限だけ延ばす)。
#[test]
fn nothing_to_update_is_none() {
    let stored = b"HTTP/1.1 200 OK\r\nX-A: 1\r\n\r\n";
    assert!(
        merge_not_modified(
            stored,
            b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        )
        .is_none()
    );
}
