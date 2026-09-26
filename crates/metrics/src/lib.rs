//! 計測。ホスト別・接続元別の統計、時系列の履歴、状態ファイルへの読み書き。
//!
//! **このクレートは状態ファイル ([`persist`] / [`persist_recent`]) と、割った 4 つの
//! クレートの出し直しだけ**を持つ。呼ぶ側は今までどおり `proxy_metrics::recent::…` の
//! ように引ける (T14.55 で `rustc` のメモリを下げるために割った。下から順に
//! `proxy-metrics-types` → `proxy-metrics-recent` → `proxy-metrics-core` →
//! `proxy-metrics-watch` → ここ)。
//!
//! 層ごとにクレートを分けてあるのは、`rustc` がクレート単位で全部を一度に抱えるため
//! (動作環境のメモリ上限は 120 MB)。**外部クレートは 1 つも使っていない。**

pub mod persist;
pub mod persist_recent;
mod tick;

/// 時系列の履歴 — 窓そのものは下の層 (`proxy-metrics-core`)、5 秒ごとに標本を取る
/// 記録スレッド ([`spawn`] / [`spawn_every`]) だけがここ (T14.55)。
pub mod history {
    pub use crate::tick::{spawn, spawn_every, take_sample};
    pub use proxy_metrics_watch::history::*;
}

// 割った先を今までの名前で出し直す (`proxy_metrics::anomaly` のような書き方をそのまま通す)。
pub use proxy_metrics_watch::{
    anomaly, canary, canaryhist, clients, daily, events, hostseries, kernel, metrics, profile,
    quantiles, recent, slo, snapshots, trace, transfer, window,
};

// 下の層をこのクレートの名前空間にも出す (`crate::sync` のような書き方をそのまま通すため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, records, sync,
};
#[cfg(target_os = "linux")]
pub use proxy_cache::sys;
pub use proxy_cache::{cache, signal, sysinfo};
pub use proxy_net::{acl, dns, net};
// 起動時の自己ベンチ (T14.43)。`/status` の `self_bench` はここが覚えている結果を読むだけ
pub use proxy_rrd::rrd;
pub use proxy_selfbench as selfbench;
