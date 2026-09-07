//! ディスクの実測。実際にファイルを書いてみて、どれだけ入るかと空きの見え方を確かめる
//! (`df` の値は tmpfs や overlayfs では当てにならないため)。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod diskprobe;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, sync,
};

// 設定 (proxy-cachecfg) の名前をこのクレートの中でも使う。上の層へは流さない。
pub use proxy_cachecfg::config;
