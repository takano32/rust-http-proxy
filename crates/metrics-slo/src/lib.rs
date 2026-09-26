//! 窓を読んで要約するもの — SLO ([`slo`])、日次の要約 ([`daily`])、日次の雪像 ([`snapshots`])。
//! 異常の検知と canary は上の層 (`proxy-metrics-watch`) に残してある。
//!
//! `proxy-metrics-watch` から割った (T17.11。いちばん重いクレートを 100 MB 未満の関門に
//! 収めるため。中身は 1 バイトも変えていない)。3 つとも `anomaly` / `canary` を呼ばないので、
//! 下の層に置ける。上の層が今までの名前 (`proxy_metrics_watch::slo` など) で出し直す。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 180 MB)。**外部クレートは 1 つも使っていない。**

pub mod daily;
pub mod slo;
pub mod snapshots;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
#[cfg(target_os = "linux")]
pub use proxy_metrics_core::sys;
pub use proxy_metrics_core::{
    acl, ascii, cache, canaryhist, cli, clients, clock, dns, envfile, events, history, hostseries,
    httpdate, json, kernel, log, log_at, log_debug, log_error, log_info, log_trace, log_warn,
    metrics, net, profile, quantiles, recent, records, rrd, selfbench, signal, sync, sysinfo,
    trace, transfer, window,
};
