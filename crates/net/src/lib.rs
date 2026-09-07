//! ソケットと HTTP の素の部品。名前解決・接続・TLS・ヘッダーと本文の表現まで。
//!
//! キャッシュも統計も中継の本体も知らない。依存するのは [`proxy_base`] だけ。

pub mod acl;
pub mod body;
pub mod clientio;
pub mod dns;
pub mod headers;
pub mod net;
pub mod origin;
pub mod pool;
pub mod request;
pub mod response;
pub mod tls;

// 下の層をこのクレートの名前空間にも出す (移設前と同じ `crate::sync` の書き方を通すため)。
#[cfg(target_os = "linux")]
pub use proxy_base::sys;
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, rrd, signal, sync, sysinfo, workers,
};

use pool::Pool;
use tls::TlsClient;

/// オリジンへ向かう側の共有状態 (接続プールと TLS クライアント)。
pub struct Upstream {
    pub pool: Pool,
    pub tls: Option<TlsClient>,
}
