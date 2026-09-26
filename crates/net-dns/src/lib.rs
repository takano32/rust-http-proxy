//! 名前解決 (`getaddrinfo` の答えのキャッシュ、Happy Eyeballs のための族の記憶、
//! よく使う名前の先読み)。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 120 MB)。**外部クレートは 1 つも使っていない。**

pub mod dns;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    ascii, cli, clock, envfile, hostport, httpdate, json, log, log_at, log_debug, log_error,
    log_info, log_trace, log_warn, sync,
};
