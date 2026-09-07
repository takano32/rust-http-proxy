//! Linux のシステムコールを直接叩く薄い層 (`poll`/`epoll`/`splice`/`pipe2`/`recv`) と、シグナルの受け取り。
//! 外部クレートは使わず `unsafe extern "C"` で宣言する。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod signal;
#[cfg(target_os = "linux")]
pub mod sys;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, sync,
};
