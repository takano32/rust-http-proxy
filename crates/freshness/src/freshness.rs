//! RFC 9111 に基づくキャッシュ可否・鮮度・再検証の判断。
//!
//! - 保存可否と TTL: `Cache-Control` (s-maxage / max-age) → `Expires` → `Last-Modified` からの
//!   経験則 (経過時間の一定割合) → 既定 TTL の順
//! - `no-cache` や TTL 0 でもバリデータ (ETag / Last-Modified) があれば「常に再検証」で保存する
//! - `Vary` は `Accept-Encoding` のみ対応 (キャッシュキーに正規化して含める)。それ以外は保存しない
//! - クライアントの条件付き要求 (`If-None-Match` / `If-Modified-Since`) はキャッシュ側で 304 を返す
//!
//! 中の割り方 (T34.4): 指示の読み方と `Vary` / `Accept-Encoding` は [`directives`]、保存可否と TTL
//! の判断は [`policy`]、保存済みの先頭と条件付き要求は [`cached_head`]。どれも入力だけを見る純粋な
//! 関数で、名前は今までどおり `freshness::…` で引ける (ここで出し直している)。熱い経路から呼ばれる
//! `response_policy_lines`・`accept_encoding_variant`・`has_directive`・`directive_value` は
//! 中身を 1 行も変えずに移した。

mod cached_head;
mod directives;
mod policy;
#[cfg(test)]
mod tests;

pub use cached_head::{
    CachedHead, client_not_modified, conditional_headers, etag_list_matches, not_modified_headers,
    parse_cached_head,
};
pub use directives::{accept_encoding_variant, directive_value, has_directive, vary_is_supported};
pub use policy::{
    CACHEABLE_STATUS, Policy, not_modified_forbids_storing, response_policy, response_policy_lines,
    revalidated_policy,
};
