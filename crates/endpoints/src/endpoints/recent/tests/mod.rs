//! recent の単体試験 (T34.86 で 1 つの tests.rs から関心ごとに割った)。
//!
//! 口を通す試験は、どれも [`with_ep`] で組んだ同じ `Endpoint` を使う
//! (キャッシュは無効、上限とスレッド数は 0)。

use super::*;
use crate::metrics::{Detail, ErrCause, Metrics};
use crate::recent::ConnState;

mod filters;
mod limits;
mod paging;
mod params;
mod table_limits;

/// 試験用の `Endpoint` を組んで `f` に渡す (キャッシュは無効、`/status` の上限は全部 0)。
///
/// `recent/snapshot.rs` の試験もこれを使う。
pub(super) fn with_ep<R>(m: &Metrics, f: impl FnOnce(&Endpoint<'_>) -> R) -> R {
    let cache = crate::cache::Cache::new(crate::cache::CacheConfig::disabled());
    let concurrency = || crate::metrics::Concurrency {
        max_conns: 0,
        max_threads: 0,
        live_threads: 0,
        idle_threads: 0,
        queued_jobs: 0,
    };
    let ep = Endpoint {
        metrics: m,
        cache: &cache,
        conn_id: 1,
        port: 8080,
        host: None,
        client: None,
        pac_direct: &[],
        lite: false,
        readonly: false,
        version: "test",
        build: "test",
        concurrency: &concurrency,
    };
    f(&ep)
}
