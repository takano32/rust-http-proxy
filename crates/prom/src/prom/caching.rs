//! `/metrics` のうちキャッシュとシステムの値。
//!
//! T.34.27 で `prom.rs` の `render` から割った。行を動かしただけで、出す順も文面も変えていない。

use super::*;

/// キャッシュの数え物 (`cache_*`)。`system_metrics` が使う写しを 1 回だけ取って返す。
pub(super) fn cache_metrics(out: &mut String, c: &Cache) -> proxy_cache::budget::Snapshot {
    let (mem_bytes, mem_entries) = c.mem_usage();
    let (disk_bytes, disk_entries) = c.disk_usage();
    header(out, "cache_hits_total", "counter", "Cache hits by tier");
    line(
        out,
        "cache_hits_total",
        "tier=\"memory\"",
        c.hits_mem.load(Ordering::Relaxed),
    );
    line(
        out,
        "cache_hits_total",
        "tier=\"disk\"",
        c.hits_disk.load(Ordering::Relaxed),
    );
    header(out, "cache_misses_total", "counter", "Cache misses");
    line(
        out,
        "cache_misses_total",
        "",
        c.misses.load(Ordering::Relaxed),
    );
    header(out, "cache_stores_total", "counter", "Responses stored");
    line(
        out,
        "cache_stores_total",
        "",
        c.stores.load(Ordering::Relaxed),
    );
    header(
        out,
        "cache_revalidations_total",
        "counter",
        "Successful revalidations (304)",
    );
    line(
        out,
        "cache_revalidations_total",
        "",
        c.revalidations.load(Ordering::Relaxed),
    );
    header(
        out,
        "cache_background_revalidations_total",
        "counter",
        "Revalidations completed in the background",
    );
    line(
        out,
        "cache_background_revalidations_total",
        "",
        c.background_revalidations.load(Ordering::Relaxed),
    );
    header(
        out,
        "cache_revalidations_dropped_total",
        "counter",
        "Background revalidations skipped because every worker thread was busy",
    );
    line(
        out,
        "cache_revalidations_dropped_total",
        "",
        c.revalidations_dropped.load(Ordering::Relaxed),
    );
    header(
        out,
        "cache_stale_served_total",
        "counter",
        "Expired entries served (grace, origin failure, slow origin)",
    );
    line(
        out,
        "cache_stale_served_total",
        "",
        c.stale_served.load(Ordering::Relaxed),
    );
    header(
        out,
        "cache_revalidating",
        "gauge",
        "Background revalidations in flight",
    );
    line(out, "cache_revalidating", "", c.revalidating_count());
    header(
        out,
        "cache_coalesced_total",
        "counter",
        "Requests that waited for an in-flight fetch instead of contacting the origin",
    );
    line(
        out,
        "cache_coalesced_total",
        "",
        c.coalesced.load(Ordering::Relaxed),
    );
    header(
        out,
        "cache_not_stored_rotations_total",
        "counter",
        "Rotations of the memory of keys a leader did not store (two rotations forget a key)",
    );
    line(
        out,
        "cache_not_stored_rotations_total",
        "",
        c.not_stored_rotations(),
    );
    header(
        out,
        "cache_inflight",
        "gauge",
        "Origin fetches in flight (coalescing table)",
    );
    line(out, "cache_inflight", "", c.inflight_count());
    header(
        out,
        "cache_admission_rejected_total",
        "counter",
        "Responses not stored because the URL was seen for the first time while the cache was full",
    );
    line(
        out,
        "cache_admission_rejected_total",
        "",
        c.admission_rejected.load(Ordering::Relaxed),
    );
    header(out, "cache_evictions_total", "counter", "Entries evicted");
    line(
        out,
        "cache_evictions_total",
        "",
        c.evictions.load(Ordering::Relaxed),
    );
    header(
        out,
        "cache_bytes_served_total",
        "counter",
        "Bytes served from cache",
    );
    line(
        out,
        "cache_bytes_served_total",
        "",
        c.bytes_served.load(Ordering::Relaxed),
    );
    header(
        out,
        "cache_used_bytes",
        "gauge",
        "Bytes held by cache entries",
    );
    line(out, "cache_used_bytes", "tier=\"memory\"", mem_bytes);
    line(out, "cache_used_bytes", "tier=\"disk\"", disk_bytes);
    header(out, "cache_limit_bytes", "gauge", "Current cache budget");
    line(
        out,
        "cache_limit_bytes",
        "tier=\"memory\"",
        c.mem_capacity(),
    );
    line(out, "cache_limit_bytes", "tier=\"disk\"", c.disk_capacity());
    header(
        out,
        "cache_reserved_bytes",
        "gauge",
        "Ballast reserved ahead of use",
    );
    line(
        out,
        "cache_reserved_bytes",
        "tier=\"memory\"",
        c.mem_reserved(),
    );
    line(
        out,
        "cache_reserved_bytes",
        "tier=\"disk\"",
        c.disk_reserved(),
    );
    header(out, "cache_entries", "gauge", "Entries per tier");
    line(out, "cache_entries", "tier=\"memory\"", mem_entries);
    line(out, "cache_entries", "tier=\"disk\"", disk_entries);
    let snap = c.snapshot();
    header(
        out,
        "cache_keep_free_bytes",
        "gauge",
        "Dynamic safety margin",
    );
    line(
        out,
        "cache_keep_free_bytes",
        "tier=\"memory\"",
        snap.mem_keep_free,
    );
    line(
        out,
        "cache_keep_free_bytes",
        "tier=\"disk\"",
        snap.disk_keep_free,
    );
    snap
}

/// プローブが見たシステムの値 (`system_memory_*`・`system_disk_bytes`・`process_rss_bytes`)。
pub(super) fn system_metrics(out: &mut String, snap: &proxy_cache::budget::Snapshot) {
    if let Some(mem) = &snap.mem {
        header(
            out,
            "system_memory_bytes",
            "gauge",
            "System memory as seen by the proxy",
        );
        line(out, "system_memory_bytes", "kind=\"total\"", mem.total);
        line(
            out,
            "system_memory_bytes",
            "kind=\"available\"",
            mem.available,
        );
        line(
            out,
            "system_memory_bytes",
            "kind=\"active_file\"",
            mem.active_file,
        );
        if let (Some(l), Some(u)) = (mem.cgroup_limit(), mem.cgroup_usage()) {
            line(out, "system_memory_bytes", "kind=\"cgroup_limit\"", l);
            line(out, "system_memory_bytes", "kind=\"cgroup_usage\"", u);
        }
        header(
            out,
            "system_memory_pressure",
            "gauge",
            "1 when PSI indicates memory pressure",
        );
        line(
            out,
            "system_memory_pressure",
            "",
            u8::from(mem.under_pressure()),
        );
    }
    if let Some(fs) = &snap.fs {
        header(
            out,
            "system_disk_bytes",
            "gauge",
            "Disk (or quota) as seen by the proxy",
        );
        line(out, "system_disk_bytes", "kind=\"total\"", fs.total);
        line(out, "system_disk_bytes", "kind=\"available\"", fs.available);
    }
    if let Some(rss) = snap.rss {
        header(
            out,
            "process_rss_bytes",
            "gauge",
            "Resident set size of the proxy",
        );
        line(out, "process_rss_bytes", "", rss);
    }
}
