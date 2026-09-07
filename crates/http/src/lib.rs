//! 中継の本体。1 つの HTTP 要求をオリジンへ運び、応答を返すところと、
//! CONNECT のトンネル。
//!
//! 接続の受け付けや keep-alive の管理は上の `rust_http_proxy` にある。

pub mod freshness;
pub mod http;
pub mod tunnel;

// 下の層をこのクレートの名前空間にも出す (移設前と同じ `crate::cache` の書き方を通すため)。
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
pub use proxy_stats::{blocklist, config, history, metrics, persist, prom, reload};
