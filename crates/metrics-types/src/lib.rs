//! 計測の土台 — 型と定数 ([`metrics`])、分位点 ([`quantiles`])、ホスト別の時系列
//! ([`hostseries`])、カーネルと cgroup の統計 ([`kernel`])。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 180 MB)。**外部クレートは 1 つも使っていない。**
//! 元は 1 つの `proxy-metrics` だったものを T14.55 で 5 つに割った
//! (`types` → `recent` → `core` → `watch` → `proxy-metrics`)。

pub mod hostseries;
pub mod kernel;
pub mod metrics;
pub mod quantiles;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    ascii, cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info,
    log_trace, log_warn, prefault, records, sync,
};
pub use proxy_net::{acl, dns, net};
pub use proxy_rrd::rrd;
#[cfg(target_os = "linux")]
pub use proxy_sysinfo::sys;
pub use proxy_sysinfo::{signal, sysinfo};
