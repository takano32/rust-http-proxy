//! 接続 (Happy Eyeballs で A / AAAA を並行に試す) と待ち受けソケットの用意。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 180 MB)。**外部クレートは 1 つも使っていない。**

pub mod net;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_net_dns::{
    ascii, cli, clock, dns, envfile, hostport, httpdate, json, log, log_at, log_debug, log_error,
    log_info, log_trace, log_warn, sync,
};
