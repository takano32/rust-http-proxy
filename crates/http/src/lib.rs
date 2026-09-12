//! 中継の本体。1 つの HTTP 要求をオリジンへ運び、応答をクライアントへ返す。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod http;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, sync, via,
};
pub use proxy_cache::{cache, sysinfo};
pub use proxy_freshness::freshness;
pub use proxy_metrics::{history, metrics, persist, recent, rrd};
pub use proxy_msg::{body, clientio, headers, response};
pub use proxy_net::{acl, dns, net};
pub use proxy_origin::{Upstream, origin, pool, request, tls};
pub use proxy_sys::signal;
// 裏側の再検証 (`http::refresh`) を接続スレッドと同じ置き場で走らせるため (T11.3)。
// `proxy-workers` が依存しているのは `proxy-base` だけなので、ここから使っても輪にならない
// (輪になるなら T10.7 の `Concurrency` と同じく上の層から関数を渡す形にしていた)
#[cfg(target_os = "linux")]
pub use proxy_sys::sys;
pub use proxy_workers::workers;
