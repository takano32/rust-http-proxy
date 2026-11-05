//! リクエストの解析: 使うヘッダーの抽出と、要求先 (オリジン) の決定。
//!
//! - `headers`: 要求ヘッダーのうち転送・キャッシュ判断に使うもの ([`RequestHeaders`]) を 1 回の走査で拾う
//! - `authority`: `host[:port]` の形の検査 (RFC 3986 §3.2・RFC 9112 §3.2。T.32.12)
//! - `target`: 要求行の対象と `Host` から要求先 ([`Origin`]) を決め、正規化 URL ([`Located`]) を組む

mod authority;
mod headers;
mod target;

#[cfg(test)]
mod content_length_tests;
#[cfg(test)]
mod target_host_tests;

pub use headers::{RequestHeaders, parse_request_headers, parse_request_headers_visit};
pub use target::{
    Located, Origin, has_empty_path, map_locations, parse_origin, strip_scheme, target_host,
    target_scheme,
};

// 試験が速い道と遅い道を突き合わせるために引く (本体の利用者は `target` の中だけ)
#[cfg(test)]
use authority::{check_authority, check_authority_slow};
