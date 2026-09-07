//! 計測とブロックリストと設定の再読込。
//!
//! この 6 つは互いを参照しているので 1 つのクレートにまとめてある
//! (指標の JSON にブロックリストと再読込の状態が入り、ブロックリストの上書きは
//! 指標と同じ状態ファイルに載る)。割るには依存の反転が要るので、そこまではしない。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod blocklist;
pub mod history;
pub mod metrics;
pub mod persist;
pub mod reload;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, sync,
};
pub use proxy_cache::cache;
pub use proxy_config::config;
pub use proxy_msg::{body, clientio, headers, response};
pub use proxy_net::{acl, dns, net};
pub use proxy_origin::{Upstream, origin, pool, request, tls};
pub use proxy_rrd::rrd;
#[cfg(target_os = "linux")]
pub use proxy_sysinfo::sys;
pub use proxy_sysinfo::{signal, sysinfo};
