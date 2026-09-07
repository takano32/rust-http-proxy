//! CONNECT のトンネル。Linux では `splice(2)` でカーネル内をコピーする。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod tunnel;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, sync,
};
pub use proxy_net::{acl, dns, net};
pub use proxy_stats::{
    Upstream, blocklist, body, cache, clientio, config, headers, history, metrics, origin, persist,
    pool, reload, request, response, rrd, sysinfo, tls,
};
pub use proxy_sys::signal;
#[cfg(target_os = "linux")]
pub use proxy_sys::sys;
