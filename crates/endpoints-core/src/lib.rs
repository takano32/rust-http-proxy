//! エンドポイントの共通の土台 (要求 1 本ぶんの文脈と、問い合わせの読み方)。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 180 MB)。**外部クレートは 1 つも使っていない。**

pub mod core;

pub use core::{Endpoint, has_flag, next_offset, offset_param, parse_query, percent_decode};

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, records, sync,
};
#[cfg(target_os = "linux")]
pub use proxy_cache::sys;
pub use proxy_cache::{cache, signal, sysinfo};
pub use proxy_metrics::metrics;
