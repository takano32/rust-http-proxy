//! 応答キャッシュの層 (保存・取り出し・鮮度判定・容量の見積もり)。
//!
//! 依存するのは [`proxy_base`] だけ。統計や中継の本体は知らない。

pub mod cache;

pub use proxy_store::{budget, config as cache_config, diskprobe, format, key, margin, quota};

// 下の層をこのクレートの名前空間にも出す。移設前と同じ `crate::sync` のような
// 書き方がそのまま通るようにするため (実体は proxy-base の 1 つだけ)。
#[cfg(target_os = "linux")]
pub use proxy_base::sys;
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, rrd, signal, sync, sysinfo, workers,
};
pub use proxy_net::{
    Upstream, acl, body, clientio, dns, headers, net, origin, pool, request, response, tls,
};
