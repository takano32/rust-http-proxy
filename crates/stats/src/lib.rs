//! 設定と計測の層 (設定の読み込みと再読込、ブロックリスト、指標、履歴、永続化)。
//!
//! 依存するのは [`proxy_base`] と [`proxy_cache`] だけ。中継の本体は知らない。

pub mod blocklist;
pub mod config;
pub mod history;
pub mod metrics;
pub mod persist;
pub mod prom;
pub mod reload;

// 下の層をこのクレートの名前空間にも出す (移設前と同じ `crate::sync` の書き方を通すため)。
#[cfg(target_os = "linux")]
pub use proxy_base::sys;
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, rrd, signal, sync, sysinfo, workers,
};
pub use proxy_cache::cache;
pub use proxy_net::{
    Upstream, acl, body, clientio, dns, headers, net, origin, pool, request, response, tls,
};
