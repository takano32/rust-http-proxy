//! キャッシュのメモリ側。LRU、エントリの表現、受け入れ判定、進行中の取得の合流、
//! 「保存されない」と分かっている鍵の記憶。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 200 MB)。**外部クレートは 1 つも使っていない。**

pub mod admission;
pub mod entry;
pub mod inflight;
pub mod lru;
pub mod memory;
pub mod notstored;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, sync,
};

// 土台 (proxy-store) の名前をこのクレートの中でも使う。上の層へは流さない
// (proxy-config の `config` と名前がぶつかるため)。
pub use proxy_base::clock::now_epoch;
pub use proxy_cachekey::{format, key};
