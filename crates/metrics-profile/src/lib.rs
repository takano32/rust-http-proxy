//! 時系列の窓の土台 — 記録の間隔と 1 区間の応答時間 ([`window`])、要求の段階ごとの待ちと
//! スレッドの標本 ([`profile`])。
//!
//! `proxy-metrics-window` から割った (T17.11。いちばん重いクレートを 100 MB 未満の関門に
//! 収めるため。中身は 1 バイトも変えていない)。2 つとも履歴 (`history`)・転送 (`transfer`)・
//! 接続元の個票 (`clients`) を呼ばないので、下の層に置ける。上の層が今までの名前
//! (`proxy_metrics_window::profile` など) で出し直す。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 120 MB)。**外部クレートは 1 つも使っていない。**

pub mod profile;
pub mod window;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
#[cfg(target_os = "linux")]
pub use proxy_metrics_recent::sys;
pub use proxy_metrics_recent::{
    acl, ascii, cache, cli, clock, dns, envfile, events, hostseries, httpdate, json, kernel, log,
    log_at, log_debug, log_error, log_info, log_trace, log_warn, metrics, net, prefault, quantiles,
    recent, records, rrd, selfbench, signal, sync, sysinfo, trace,
};
