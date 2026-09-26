//! 窓を読んで判定するもの — SLO ([`slo`])、日次の要約 ([`daily`])、
//! 日次の雪像 ([`snapshots`])、異常の検知 ([`anomaly`])、
//! 利用者が居ない時間帯の様子見 ([`canary`]。T15.12 で下の層から上げた)。
//! SLO・日次の要約・日次の雪像の 3 つは T17.11 で下の層 (`proxy-metrics-slo`) へ割り、
//! ここでは今までの名前で出し直すだけ。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 120 MB)。**外部クレートは 1 つも使っていない。**

pub mod anomaly;
pub mod canary;

// 割った先を今までの名前で出し直す (`proxy_metrics_watch::slo` のような書き方をそのまま通す)。
pub use proxy_metrics_slo::{daily, slo, snapshots};

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
#[cfg(target_os = "linux")]
pub use proxy_metrics_slo::sys;
pub use proxy_metrics_slo::{
    acl, ascii, cache, canaryhist, cli, clients, clock, dns, envfile, events, history, hostseries,
    httpdate, json, kernel, log, log_at, log_debug, log_error, log_info, log_trace, log_warn,
    metrics, net, profile, quantiles, recent, records, rrd, selfbench, signal, sync, sysinfo,
    trace, transfer, window,
};
