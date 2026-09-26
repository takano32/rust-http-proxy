//! 計測の本体 — [`metrics::Metrics`] と `/status` の組み立て。
//!
//! 利用者が居ない時間帯の様子見 (`canary`) は 1 つ上の層 (`proxy-metrics-watch`) に居る
//! (T15.12 で上げた)。`/status` の `canary` は、上の層が
//! [`metrics::set_canary_status`] で預けた口を呼んで組む。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 120 MB)。**外部クレートは 1 つも使っていない。**

pub mod metrics;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
// `metrics` だけはこのクレートにも同じ名前のモジュールがあるので、そちらが中で
// `pub use proxy_metrics_window::metrics::*;` をして出し直している。
#[cfg(target_os = "linux")]
pub use proxy_metrics_window::sys;
pub use proxy_metrics_window::{
    acl, ascii, cache, canaryhist, cli, clients, clock, dns, envfile, events, history, hostseries,
    httpdate, json, kernel, log, log_at, log_debug, log_error, log_info, log_trace, log_warn, net,
    profile, quantiles, recent, records, rrd, selfbench, signal, sync, sysinfo, trace, transfer,
    window,
};
