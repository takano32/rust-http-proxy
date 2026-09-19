//! 名前解決 (Happy Eyeballs)、接続、アドレスの判定 (ACL とローカル宛ての拒否)。
//!
//! 名前解決は `proxy-net-dns`、接続と待ち受けは `proxy-net-conn` に分けてあり、
//! このクレートは判定 (`acl`) と、上の 11 クレートが使う名前空間の facade を持つ。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 180 MB)。**外部クレートは 1 つも使っていない。**

pub mod acl;

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
// `dns` と `net` は T15.12 で下のクレートへ移したが、綴りはここで維持している。
pub use proxy_net_conn::{
    ascii, cli, clock, dns, envfile, hostport, httpdate, json, log, log_at, log_debug, log_error,
    log_info, log_trace, log_warn, net, sync,
};
