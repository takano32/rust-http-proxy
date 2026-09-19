//! 認証なしの HTTP/HTTPS(CONNECT) フォワードプロキシ (`rust_http_proxy`)。
//!
//! **中身は [`proxy_server`] にあり、ここはその名前をそのまま出し直すだけの facade**
//! (T15.12 段 6')。分けたのは、`serve` の総称の引数 (`config_of: impl Fn()`) の単相化が
//! bin の側に出て、最後のリンクの段が 106 MB まで膨らんでいたため
//! (実測 2026-09-19。bin を薄くして別の `rustc` に分けると、その段が小さくなる)。
//!
//! `rust_http_proxy::config` のような今までの綴りは、この再輸出でそのまま通る
//! (結合テスト `tests/` は全部この名前で書いてある)。

pub use proxy_server::*;
