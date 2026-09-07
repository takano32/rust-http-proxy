//! キャッシュの土台。設定、容量の見積もり (メモリ・ディスク・cgroup・quota)、
//! 保存形式と鍵。
//!
//! 保存そのもの (LRU、エントリ、取り出し) は [`proxy_cache`](../proxy_cache/index.html)。
//! ここは `Cache` を知らないので、その手前で切ってある。

pub mod budget;
pub mod config;
pub mod diskprobe;
pub mod format;
pub mod key;
pub mod margin;
pub mod quota;

// 下の層をこのクレートの名前空間にも出す (移設前と同じ `crate::sync` の書き方を通すため)。
#[cfg(target_os = "linux")]
pub use proxy_base::sys;
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, rrd, signal, sync, sysinfo, workers,
};
