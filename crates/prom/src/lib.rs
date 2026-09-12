//! Prometheus 形式の出力。指標とキャッシュとブロックリストを読むだけの消費側。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod prom;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, sync,
};
pub use proxy_blocklist::{
    Upstream, blocklist, body, clientio, config, headers, origin, pool, request, response, tls,
};
#[cfg(target_os = "linux")]
pub use proxy_cache::sys;
pub use proxy_cache::{cache, signal, sysinfo};
pub use proxy_metrics::{history, metrics, persist, recent, rrd};
pub use proxy_net::{acl, dns, net};
