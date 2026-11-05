//! 再検証の 304 で届いたヘッダーで、保存済みの応答の先頭を書き直す (RFC 9111 §3.2・§4.3.4。T28.2)。

use super::{clean_value, response_lines, write_response_head};

/// 304 から保存済みの先頭へ写さない名前 (T28.2)。枠組み・hop-by-hop・`Connection` で指名された
/// 名前・`Age` ([`write_response_head`] が落とすもの) に加えて、保存した本文の形を決めている
/// `Content-Encoding` と `Content-Range` (304 が違う値を返しても本文は変わらないので、写すと食い違う)、
/// それに `Set-Cookie` / `Set-Cookie2` (再検証した 1 人の利用者あての値。写すと次の `REVALIDATED` と
/// `HIT` で他の利用者へ配ってしまう。T29.0。その人へは [`with_set_cookies`] で渡す)。
const NOT_UPDATED_BY_304: &[&str] = &[
    "content-encoding",
    "content-range",
    "set-cookie",
    "set-cookie2",
];

/// `head` (状態行 + ヘッダー + 空行) の末尾に、`not_modified` (オリジンの 304 の先頭) の
/// `Set-Cookie` / `Set-Cookie2` の行を足したもの (T29.0)。再検証した要求のクライアントへ配る
/// 写しだけに使い、保存する先頭には入れない。足す行が無ければ `None`。
pub fn with_set_cookies(head: &[u8], not_modified: &[u8]) -> Option<Vec<u8>> {
    let is_cookie =
        |k: &str| k.eq_ignore_ascii_case("set-cookie") || k.eq_ignore_ascii_case("set-cookie2");
    response_lines(not_modified).find(|(k, _)| is_cookie(k))?;
    let body_start = head
        .strip_suffix(b"\r\n\r\n")
        .map_or(head.len(), |h| h.len() + 2);
    let mut out = Vec::with_capacity(head.len() + 128);
    out.extend_from_slice(&head[..body_start]);
    for (k, v) in response_lines(not_modified).filter(|(k, _)| is_cookie(k)) {
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(clean_value(v).as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    Some(out)
}

/// 再検証の 304 で届いたヘッダーで、保存済みの応答の先頭を書き直す (RFC 9111 §3.2・§4.3.4。T28.2)。
///
/// `stored` は保存済みの先頭 (状態行 + ヘッダー + 空行)、`not_modified` はオリジンの 304 の先頭。
/// 304 にある名前 (写さない名前を除く) は、保存済みの同じ名前の行を**全部**落として 304 の行に
/// 置き換える (`Cache-Control`・`Expires`・`Date`・`ETag`・`Last-Modified`・`Vary`・独自の名前も)。
/// 状態行と、304 に無い名前の行はそのまま。304 の値は [`write_response_head`] と同じく
/// 整える (obs-fold を畳む・CR / NUL を SP に)。写すものが 1 つも無ければ `None`。
pub fn merge_not_modified(stored: &[u8], not_modified: &[u8]) -> Option<Vec<u8>> {
    // 304 を応答の書き出しと同じ規則で整える (hop-by-hop・`Connection` の指名・枠組み・`Age` が落ちる)
    let mut fresh = Vec::with_capacity(not_modified.len());
    write_response_head(
        &mut fresh,
        not_modified,
        None,
        NOT_UPDATED_BY_304,
        &[] as &[&str],
    );
    let updated = |name: &str| response_lines(&fresh).any(|(k, _)| k.eq_ignore_ascii_case(name));
    response_lines(&fresh).next()?;
    let mut out = Vec::with_capacity(stored.len() + fresh.len());
    // 状態行は保存済みのもの (304 の状態行は写さない)
    let status_end = stored
        .iter()
        .position(|&b| b == b'\n')
        .map_or(stored.len(), |i| i + 1);
    out.extend_from_slice(&stored[..status_end]);
    for (k, v) in response_lines(stored) {
        // 0.28.48 までの 304 が写した `Set-Cookie` も、次の再検証で落とす (T29.0)。保存は
        // `Set-Cookie` のある応答をもともと取らないので、保存済みの先頭にあるのはその写しだけ
        if updated(k)
            || k.eq_ignore_ascii_case("set-cookie")
            || k.eq_ignore_ascii_case("set-cookie2")
        {
            continue;
        }
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    for (k, v) in response_lines(&fresh) {
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    Some(out)
}
