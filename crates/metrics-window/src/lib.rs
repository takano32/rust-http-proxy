//! 時系列の窓 — 記録の間隔と 1 区間の応答時間 ([`window`])、要求の段階ごとの待ちと
//! スレッドの標本 ([`profile`])、転送の速さと半閉じ ([`transfer`])。
//! 接続元の個票 ([`clients`]) と履歴 ([`history`]) も、`Metrics` が抱える側なので
//! ここに置いてある。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 180 MB)。**外部クレートは 1 つも使っていない。**

pub mod canaryhist;
pub mod clients;
pub mod history;
pub mod profile;
pub mod transfer;
pub mod window;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
#[cfg(target_os = "linux")]
pub use proxy_metrics_recent::sys;
pub use proxy_metrics_recent::{
    acl, ascii, cache, cli, clock, dns, envfile, events, hostseries, httpdate, json, kernel, log,
    log_at, log_debug, log_error, log_info, log_trace, log_warn, metrics, net, prefault, quantiles,
    recent, records, rrd, selfbench, signal, sync, sysinfo, trace,
};
