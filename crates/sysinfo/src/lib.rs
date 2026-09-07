//! 機械の観測 (メモリ、ディスク、cgroup の上限、`inotify` でのファイル監視)。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod sysinfo;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
#[cfg(target_os = "linux")]
pub use proxy_sys::sys;
pub use proxy_sys::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, signal, sync,
};
