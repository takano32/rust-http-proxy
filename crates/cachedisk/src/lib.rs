//! キャッシュのディスク側。ファイルの形式、起動時の走査、書き出し。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod disk;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, sync,
};
#[cfg(target_os = "linux")]
pub use proxy_sysinfo::sys;
pub use proxy_sysinfo::{signal, sysinfo};

// 土台 (proxy-store) とメモリ側の名前をこのクレートの中でも使う。上の層へは流さない。
pub use proxy_base::clock::now_epoch;
pub use proxy_cachecfg::config;
pub use proxy_cachekey::{format, key};
pub use proxy_cachemem::{lru, memory};
