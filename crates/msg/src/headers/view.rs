//! 応答の先頭 (生のバイト列) を「名前, 値」で読む。`Vec` も `String` も作らない。

use crate::ascii;

/// 応答の先頭 (生バイト列) からヘッダー行を「名前, 値」で返す。前後の空白は落としてある。
///
/// 状態行は飛ばし、最初の空行で終わる。`Vec` も `String` も作らないので、
/// 素通しの経路 (枠組みと `Connection` しか見ない) はこれだけで足りる。
///
/// 値に UTF-8 として読めないバイト (obs-text) がある行は、名前だけ読んで値を `"\u{FFFD}"` 1 字にする
/// (T.28.12)。以前は読めない行を空行と取り違えてそこで終わり、後ろの `Content-Length` などを
/// 見落としていた (応答の先頭を行ごとの `String` で読んでいた頃は、その前に 502 で落ちていた)。
/// 値を読めないものとして渡すので、`Content-Length` なら 502、`Transfer-Encoding` なら閉じるまでになる。
pub fn response_lines(head: &[u8]) -> impl Iterator<Item = (&str, &str)> {
    head.split(|b| *b == b'\n')
        .skip(1)
        .map(|line| line.trim_ascii())
        .take_while(|line| !line.is_empty())
        .filter_map(|line| match std::str::from_utf8(line) {
            Ok(s) => ascii::split_once(s, b':').map(|(k, v)| (k.trim_ascii(), v.trim_ascii())),
            Err(e) => {
                let valid = std::str::from_utf8(&line[..e.valid_up_to()]).unwrap_or("");
                ascii::split_once(valid, b':').map(|(k, _)| (k.trim_ascii(), "\u{FFFD}"))
            }
        })
}

/// 応答の先頭を「小文字の名前, 値」の組にする ([`response_lines`] を複製した形)。
/// キャッシュの判定 ([`crate::freshness`] 相当) のように組で受け取る API のために残してある。
pub fn response_pairs(head: &[u8]) -> Vec<(String, String)> {
    response_lines(head)
        .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
        .collect()
}

/// 応答の先頭から、名前が一致する最初のヘッダーの値を借用で返す。
pub fn response_value<'a>(head: &'a [u8], name: &str) -> Option<&'a str> {
    response_lines(head)
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

/// 応答の `name` ヘッダーが `token` を (カンマ区切りの中に) 挙げているか。
pub fn response_names(head: &[u8], name: &str, token: &str) -> bool {
    response_lines(head).any(|(k, v)| {
        k.eq_ignore_ascii_case(name)
            && v.split(',')
                .any(|t| t.trim_ascii().eq_ignore_ascii_case(token))
    })
}
