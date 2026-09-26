//! 記録の個票 — 閉じた接続 ([`recent`])、1 つの接続元の要求の並び ([`trace`])、
//! 出来事の時系列 ([`events`])。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 180 MB)。**外部クレートは 1 つも使っていない。**

pub mod events;
pub mod recent;
pub mod trace;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_cache::cache;
#[cfg(target_os = "linux")]
pub use proxy_metrics_types::sys;
pub use proxy_metrics_types::{
    acl, ascii, cli, clock, dns, envfile, hostseries, httpdate, json, kernel, log, log_at,
    log_debug, log_error, log_info, log_trace, log_warn, metrics, net, prefault, quantiles,
    records, rrd, signal, sync, sysinfo,
};
pub use proxy_selfbench as selfbench;
