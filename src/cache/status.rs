//! `/status` 用の JSON フラグメント生成。

use std::sync::atomic::Ordering;

use super::Cache;
use super::config::{DiskQuota, Limit};

fn opt(v: Option<u64>) -> String {
    v.map_or_else(|| "null".to_string(), |x| x.to_string())
}

fn opt_f(v: Option<f64>) -> String {
    v.map_or_else(|| "null".to_string(), |x| format!("{:.1}", x))
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
        let hits_mem = self.hits_mem.load(Ordering::Relaxed);
        let hits_disk = self.hits_disk.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
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
                "\"revalidations\":{},\"bytes_served\":{},\"reserve\":{},",
                "\"memory\":{{\"used_bytes\":{},\"limit_bytes\":{},\"entries\":{},",
                "\"mode\":\"{}\",\"target_percent\":{},\"reserved_bytes\":{},",
                "\"keep_free_bytes\":{},\"cgroup_keep_free_bytes\":{}}},",
                "\"disk\":{{\"used_bytes\":{},\"limit_bytes\":{},\"entries\":{},\"dir\":\"{}\",",
                "\"mode\":\"{}\",\"target_percent\":{},\"reserved_bytes\":{},\"quota_bytes\":{},",
                "\"quota_mode\":\"{}\",\"keep_free_bytes\":{}}},",
                "\"system\":{{\"probed_at\":{},\"process_rss_bytes\":{},",
                "\"mem_total_bytes\":{},\"mem_available_bytes\":{},\"mem_used_percent\":{},",
                "\"mem_active_file_bytes\":{},",
                "\"cgroup_limit_bytes\":{},\"cgroup_usage_bytes\":{},\"mem_pressure\":{},",
                "\"disk_total_bytes\":{},\"disk_available_bytes\":{},\"disk_used_percent\":{}}}}}"
            ),
            self.cfg.enabled,
            hits_mem + hits_disk,
            hits_mem,
            hits_disk,
            misses,
            hit_ratio,
            self.stores.load(Ordering::Relaxed),
            self.evictions.load(Ordering::Relaxed),
            self.revalidations.load(Ordering::Relaxed),
            self.bytes_served.load(Ordering::Relaxed),
            self.cfg.reserve,
            mem_bytes,
            self.mem_capacity(),
            mem_entries,
            mem_mode,
            mem_pct,
            self.mem_reserved(),
            snap.mem_keep_free,
            snap.cgroup_keep_free,
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
            snap.taken_at,
            opt(snap.rss),
            opt(mem.map(|m| m.total)),
            opt(mem.map(|m| m.available)),
            opt_f(mem.map(|m| m.used_percent())),
            opt(mem.map(|m| m.active_file)),
            opt(mem.and_then(|m| m.cgroup_limit())),
            opt(mem.and_then(|m| m.cgroup_usage())),
            mem.is_some_and(|m| m.under_pressure()),
            opt(fs.map(|f| f.total)),
            opt(fs.map(|f| f.available)),
            opt_f(fs.map(|f| f.used_percent())),
        )
    }
}
