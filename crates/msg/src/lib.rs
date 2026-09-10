//! HTTP メッセージの表現。ヘッダーの解釈と組み立て、本文の枠 (Content-Length / chunked)、
//! クライアント側の読み取りバッファ、応答の先頭の読み取り。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod body;
pub mod clientio;
pub mod headers;
pub mod response;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    ascii, cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info,
    log_trace, log_warn, sync, via,
};
