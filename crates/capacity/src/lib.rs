//! キャッシュに使ってよい量の見積もり。空きメモリ・空きディスク・cgroup の上限・
//! コンテナの quota を見て、余白を残しながら予算を決める。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod budget;
pub mod diskprobe;
pub mod margin;
pub mod quota;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, sync,
};
#[cfg(target_os = "linux")]
pub use proxy_sysinfo::sys;
pub use proxy_sysinfo::{signal, sysinfo};

// 設定 (proxy-cachecfg) の名前をこのクレートの中でも使う。上の層へは流さない
// (proxy-config の `config` と名前がぶつかるため)。
pub use proxy_cachecfg::config;
