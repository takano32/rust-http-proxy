//! 1 相手の説明 `/explain?host=<name>` / `?client=<ip>` (T14.36)。
//!
//! T15.12 段 1 (ii) で `proxy-endpoints` から下ろした (`explain.rs` は 1 行も変えていない。
//! `super::{Endpoint, parse_query}` が `crate::{…}` になっただけ)。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 120 MB)。**外部クレートは 1 つも使っていない。**

pub mod explain;

pub use explain::{MAX_BODY, explain};
pub use proxy_endpoints_core::{Endpoint, parse_query};

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, records, sync,
};
#[cfg(target_os = "linux")]
pub use proxy_cache::sys;
pub use proxy_cache::{cache, signal, sysinfo};
pub use proxy_http::{
    Upstream, anomaly, body, clientio, daily, events, freshness, headers, history, hostseries,
    http, kernel, metrics, origin, persist, pool, profile, recent, request, response, rrd, slo,
    snapshots, tls, trace,
};
pub use proxy_net::{acl, dns, net};
