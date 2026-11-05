//! HTTP ヘッダーの解釈と組み立て。
//!
//! - `request`: クライアントから受けた要求ヘッダーを、オリジンへ転送する形に整える
//! - `response`: オリジンの応答の先頭 (状態行 + ヘッダー) を、クライアントへ返す形に整える
//! - `view`: 応答の先頭を生のバイト列のまま「名前, 値」で読む (確保しない)
//! - `not_modified`: 再検証の 304 で保存済みの先頭を書き直す
//!
//! hop-by-hop と枠組みの名前の表、`Connection:` で指名された名前の覚え方 ([`ConnectionNames`])、
//! 値の CR / NUL の置き換え ([`clean_value`]) はここに置き、要求の側も応答の側も同じ規則を使う。

mod not_modified;
mod request;
mod response;
mod view;

#[cfg(test)]
mod merge_not_modified_tests;
#[cfg(test)]
mod response_view_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod write_request_tests;
#[cfg(test)]
mod write_response_tests;

pub use not_modified::{merge_not_modified, with_set_cookies};
#[cfg(test)]
use request::sanitize_and_inject_headers;
pub use request::write_request_headers;
pub use response::{ResponseHead, line_named, sanitize_response_head, write_response_head};
pub use view::{response_lines, response_names, response_pairs, response_value};

use crate::ascii;

const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "upgrade",
    "proxy-connection",
];

pub(crate) fn is_hop_by_hop(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP_HEADERS.contains(&lower.as_str())
}

/// 小文字化した文字列を作らずに判定する版。
#[inline]
pub(crate) fn is_hop_by_hop_name(name: &str) -> bool {
    HOP_BY_HOP_HEADERS
        .iter()
        .any(|h| name.eq_ignore_ascii_case(h))
}

/// `Connection:` で指名される独自の hop-by-hop 名を、確保せずに覚えておける本数。
const MAX_CUSTOM_HOP: usize = 16;

/// レスポンスヘッダーのうち、プロキシが自分で決め直すもの (枠組み・経過時間)。
const FRAMING_HEADERS: &[&str] = &["transfer-encoding", "content-length", "age"];

/// `Connection:` で指名された独自の hop-by-hop 名 (RFC 9110 §7.6.1)。
///
/// 要求 ([`write_request_headers`]) も応答 ([`write_response_head`]) も、転送する前に `Connection:` の
/// 行をなめて名前を**借用のまま**固定枠 ([`MAX_CUSTOM_HOP`] 本) に覚える。枠を溢れたときだけ、
/// 問い合わせのたびに元の行を読み直す (`String` を 1 本も作らない)。
struct ConnectionNames<'a> {
    names: [&'a str; MAX_CUSTOM_HOP],
    n: usize,
    /// 枠に入り切らなかった名前があった (問い合わせは元の行をなめ直す)
    overflow: bool,
}

impl<'a> ConnectionNames<'a> {
    /// `lines` (`Name: value` の行。行末は付いていてもよい) の `Connection:` の行から集める。
    #[inline]
    fn collect(lines: impl Iterator<Item = &'a str>) -> Self {
        let mut me = ConnectionNames {
            names: [""; MAX_CUSTOM_HOP],
            n: 0,
            overflow: false,
        };
        for line in lines {
            let Some(v) = connection_value(line) else {
                continue;
            };
            for item in v.split(',') {
                let token = item.trim_ascii();
                if token.is_empty() {
                    continue;
                }
                if me.n < MAX_CUSTOM_HOP {
                    me.names[me.n] = token;
                    me.n += 1;
                } else {
                    me.overflow = true;
                }
            }
        }
        me
    }

    /// `name` が指名されているか (大小は無視)。枠を溢れていれば `lines` をもう一度なめる
    /// (`lines` は溢れたときだけ呼ばれる)。
    #[inline]
    fn contains<I>(&self, name: &str, lines: impl FnOnce() -> I) -> bool
    where
        I: Iterator<Item = &'a str>,
    {
        if self.names[..self.n]
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name))
        {
            return true;
        }
        self.overflow
            && lines().any(|line| {
                connection_value(line).is_some_and(|v| {
                    v.split(',')
                        .any(|t| t.trim_ascii().eq_ignore_ascii_case(name))
                })
            })
    }
}

/// `line` が `Connection:` の行ならその値 (コロンより後ろ。前後の空白は付いたまま)。
#[inline]
fn connection_value(line: &str) -> Option<&str> {
    let (k, v) = ascii::split_once(line, b':')?;
    k.trim_ascii()
        .eq_ignore_ascii_case("connection")
        .then_some(v)
}

/// 応答のヘッダー値の CR と NUL を SP に置き換える (T27.7。RFC 9110 §5.5 の「SP に置き換えて転送」の方)。
///
/// オリジンが `X-A: a\rSet-Cookie: evil=1` を返すと、CR を改行と読むクライアントには別のヘッダーに見える。
/// 応答は断る (502) と壊れたオリジンの頁が全部見えなくなるので、置き換えて通す。LF は行の区切りで
/// 先に割っているので値には残らない。**ふつうの値は確保も写しもしない** (借用のまま返す)。
fn clean_value(v: &str) -> std::borrow::Cow<'_, str> {
    if v.bytes().any(|b| b == b'\r' || b == 0) {
        std::borrow::Cow::Owned(v.replace(['\r', '\0'], " "))
    } else {
        std::borrow::Cow::Borrowed(v)
    }
}
