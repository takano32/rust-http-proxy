//! `/status` 用の JSON フラグメント生成。

use std::fmt::Display;
use std::sync::atomic::Ordering;

use super::Cache;
use super::config::{DiskQuota, Limit};

/// 値があればその表記、無ければ `null` (`/status` の数と [`BudgetEviction`] の欄が使う 1 つ。
/// 前は `opt`・`opt_f`・`BudgetEviction::to_json` の閉包の 3 つあった。T34.98)。
fn opt(v: Option<impl Display>) -> String {
    v.map_or_else(|| "null".to_string(), |x| x.to_string())
}

/// 割合を小数 1 桁で (`opt` に渡す前の形)。
fn one_decimal(x: f64) -> String {
    format!("{:.1}", x)
}

/// プローブが予算で追い出した 1 回の様子 (T32.7。`/status` の `cache.last_budget_eviction`)。
///
/// 10-31 02:34 の tokyo で、大きな転送 (32.6・375.5・253.8 MB) の間にメモリのキャッシュが
/// `evictions` 20 で 13.87 MB → 0 になり、1 分ごとの `mem_limit` は 189 → 126 → 53 → 164 MB と
/// 動いていたが、どの 5 秒で何が上限を下げたかは残っていなかった。追い出したその 5 秒の
/// 上限と、上限を決めた材料 (余白・cgroup の使用量・空き・活性なファイルの頁) を残す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetEviction {
    /// いつ (epoch 秒)
    pub at: u64,
    /// 追い出した件数 (メモリの層)
    pub entries: u64,
    /// 追い出す前と後のメモリの層の使用量 (バイト)
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// そのときの上限 (バイト)
    pub limit_bytes: u64,
    /// 上限を決めた材料 (プローブのその回の値。読めなければ `None`)
    pub keep_free_bytes: u64,
    pub cgroup_keep_free_bytes: u64,
    pub mem_available_bytes: Option<u64>,
    pub mem_active_file_bytes: Option<u64>,
    pub cgroup_usage_bytes: Option<u64>,
    pub cgroup_limit_bytes: Option<u64>,
}

impl BudgetEviction {
    /// `/status` の 1 要素 (鍵の並びは固定。`null` の書き方は `/status` の [`opt`] と 1 つ。T34.98)。
    pub fn to_json(self) -> String {
        format!(
            concat!(
                "{{\"at\":{},\"entries\":{},\"bytes_before\":{},\"bytes_after\":{},",
                "\"limit_bytes\":{},\"keep_free_bytes\":{},\"cgroup_keep_free_bytes\":{},",
                "\"mem_available_bytes\":{},\"mem_active_file_bytes\":{},",
                "\"cgroup_usage_bytes\":{},\"cgroup_limit_bytes\":{}}}"
            ),
            self.at,
            self.entries,
            self.bytes_before,
            self.bytes_after,
            self.limit_bytes,
            self.keep_free_bytes,
            self.cgroup_keep_free_bytes,
            opt(self.mem_available_bytes),
            opt(self.mem_active_file_bytes),
            opt(self.cgroup_usage_bytes),
            opt(self.cgroup_limit_bytes),
        )
    }
}

fn mode(limit: Limit) -> (&'static str, String) {
    match limit {
        Limit::Auto { percent } => ("auto", percent.to_string()),
        Limit::Fixed(_) => ("fixed", "null".to_string()),
    }
}

impl Cache {
    pub fn to_json(&self) -> String {
        let (mem_bytes, mem_entries) = self.mem_usage();
        let (disk_bytes, disk_entries) = self.disk_usage();
        let hits_mem = self.counters.hits_mem.load(Ordering::Relaxed);
        let hits_disk = self.counters.hits_disk.load(Ordering::Relaxed);
        let misses = self.counters.misses.load(Ordering::Relaxed);
        let lookups = hits_mem + hits_disk + misses;
        let hit_ratio = if lookups == 0 {
            0.0
        } else {
            (hits_mem + hits_disk) as f64 / lookups as f64
        };
        let (mem_mode, mem_pct) = mode(self.cfg.mem_limit);
        let (disk_mode, disk_pct) = mode(self.cfg.disk_limit);
        let snap = self.snapshot();
        let mem = snap.mem.as_ref();
        let fs = snap.fs;

        format!(
            concat!(
                "{{\"enabled\":{},\"hits\":{},\"hits_memory\":{},\"hits_disk\":{},",
                "\"misses\":{},\"hit_ratio\":{:.4},\"stores\":{},\"evictions\":{},",
                "\"revalidations\":{},\"background_revalidations\":{},",
                "\"revalidations_dropped\":{},\"revalidating\":{},",
                "\"stale_served\":{},\"coalesced\":{},\"not_stored_rotations\":{},",
                "\"inflight\":{},\"admission_rejected\":{},\"bytes_served\":{},\"reserve\":\"{}\",",
                "\"memory\":{{\"used_bytes\":{},\"limit_bytes\":{},\"entries\":{},",
                "\"mode\":\"{}\",\"target_percent\":{},\"reserved_bytes\":{},",
                "\"keep_free_bytes\":{},\"cgroup_keep_free_bytes\":{},\"alloc_bytes\":{}}},",
                "\"disk\":{{\"used_bytes\":{},\"limit_bytes\":{},\"entries\":{},\"dir\":\"{}\",",
                "\"mode\":\"{}\",\"target_percent\":{},\"reserved_bytes\":{},\"quota_bytes\":{},",
                "\"quota_mode\":\"{}\",\"keep_free_bytes\":{},\"entry_limit_bytes\":{}}},",
                "\"system\":{{\"probed_at\":{},\"process_rss_bytes\":{},",
                "\"mem_total_bytes\":{},\"mem_available_bytes\":{},\"mem_used_percent\":{},",
                "\"mem_active_file_bytes\":{},",
                "\"cgroup_limit_bytes\":{},\"cgroup_usage_bytes\":{},\"mem_pressure\":{},",
                "\"disk_total_bytes\":{},\"disk_available_bytes\":{},\"disk_used_percent\":{}}},",
                "\"budget_evictions\":{},\"last_budget_eviction\":{}}}"
            ),
            self.cfg.enabled,
            hits_mem + hits_disk,
            hits_mem,
            hits_disk,
            misses,
            hit_ratio,
            self.counters.stores.load(Ordering::Relaxed),
            self.counters.evictions.load(Ordering::Relaxed),
            self.counters.revalidations.load(Ordering::Relaxed),
            self.counters
                .background_revalidations
                .load(Ordering::Relaxed),
            self.counters.revalidations_dropped.load(Ordering::Relaxed),
            self.revalidating_count(),
            self.counters.stale_served.load(Ordering::Relaxed),
            self.counters.coalesced.load(Ordering::Relaxed),
            self.not_stored_rotations(),
            self.inflight_count(),
            self.counters.admission_rejected.load(Ordering::Relaxed),
            self.counters.bytes_served.load(Ordering::Relaxed),
            self.cfg.reserve,
            mem_bytes,
            self.mem_capacity(),
            mem_entries,
            mem_mode,
            mem_pct,
            self.mem_reserved(),
            snap.mem_keep_free,
            snap.cgroup_keep_free,
            // 本文の確保の大きさの合計 (T35.11。判定表 phase31 の heap の行が読む。**部の末尾に足した**)
            self.mem_alloc_bytes(),
            disk_bytes,
            self.disk_capacity(),
            disk_entries,
            self.cfg
                .dir
                .display()
                .to_string()
                .replace('\\', "/")
                .replace('"', "'"),
            disk_mode,
            disk_pct,
            self.disk_reserved(),
            opt(match self.disk_quota() {
                DiskQuota::Fixed(q) => Some(q),
                DiskQuota::Auto => snap.fs.map(|f| f.total),
                _ => None,
            }),
            self.disk_quota().as_str(),
            snap.disk_keep_free,
            self.disk.entry_capacity(),
            snap.taken_at,
            opt(snap.rss),
            opt(mem.map(|m| m.total)),
            opt(mem.map(|m| m.available)),
            opt(mem.map(|m| one_decimal(m.used_percent()))),
            opt(mem.map(|m| m.active_file)),
            opt(mem.and_then(|m| m.cgroup_limit())),
            opt(mem.and_then(|m| m.cgroup_usage())),
            mem.is_some_and(|m| m.under_pressure()),
            opt(fs.map(|f| f.total)),
            opt(fs.map(|f| f.available)),
            opt(fs.map(|f| one_decimal(f.used_percent()))),
            // 予算で追い出した回数と最後の 1 回 (T32.7)。**末尾に足した**
            self.budget_evictions(),
            self.last_budget_eviction()
                .map_or_else(|| "null".to_string(), |e| e.to_json()),
        )
    }
}
