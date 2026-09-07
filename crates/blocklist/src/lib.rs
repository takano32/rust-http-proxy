//! ドメインのブロックリスト。ファイルと URL から取り込み、手動の上書きは指標と同じ
//! 状態ファイルに載せる。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod blocklist;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, sync,
};
pub use proxy_config::{cache, config, sysinfo};
pub use proxy_metrics::{history, metrics, persist};
pub use proxy_msg::{body, clientio, headers, response};
pub use proxy_net::{acl, dns, net};
#[cfg(target_os = "linux")]
pub use proxy_origin::sys;
pub use proxy_origin::{Upstream, origin, pool, request, signal, tls};
pub use proxy_rrd::rrd;
