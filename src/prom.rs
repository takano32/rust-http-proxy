//! `/metrics` 用の Prometheus テキスト形式 (text/plain; version=0.0.4) の生成。

use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use crate::cache::Cache;
use crate::metrics::Metrics;

/// ラベル値のエスケープ (RFC: `\`、`"`、改行)。
fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn line(out: &mut String, name: &str, labels: &str, value: impl std::fmt::Display) {
    if labels.is_empty() {
        let _ = writeln!(out, "sorahost_{} {}", name, value);
    } else {
        let _ = writeln!(out, "sorahost_{}{{{}}} {}", name, labels, value);
    }
}

fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP sorahost_{} {}", name, help);
    let _ = writeln!(out, "# TYPE sorahost_{} {}", name, kind);
}

/// メトリクス一式を描く。`cache` が `None` ならキャッシュ関連は出さない。
pub fn render(m: &Metrics, cache: Option<&Cache>) -> String {
    let mut out = String::with_capacity(4096);
    header(
        &mut out,
        "uptime_seconds",
        "gauge",
        "Seconds since the proxy started",
    );
    line(
        &mut out,
        "uptime_seconds",
        "",
        m.start_time.elapsed().as_secs(),
    );
    header(&mut out, "requests_total", "counter", "Requests received");
    line(
        &mut out,
        "requests_total",
        "",
        m.total_requests.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "active_connections",
        "gauge",
        "Open client connections",
    );
    line(
        &mut out,
        "active_connections",
        "",
        m.active_connections.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "bytes_forwarded_total",
        "counter",
        "Bytes sent to clients and origins",
    );
    line(
        &mut out,
        "bytes_forwarded_total",
        "",
        m.bytes_forwarded.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "origin_connections_total",
        "counter",
        "Origin connections by state",
    );
    line(
        &mut out,
        "origin_connections_total",
        "state=\"new\"",
        m.origin_new.load(Ordering::Relaxed),
    );
    line(
        &mut out,
        "origin_connections_total",
        "state=\"reused\"",
        m.origin_reused.load(Ordering::Relaxed),
    );

    header(
        &mut out,
        "host_requests_total",
        "counter",
        "Requests per origin host",
    );
    header(
        &mut out,
        "host_hits_total",
        "counter",
        "Cache hits per origin host",
    );
    header(
        &mut out,
        "host_misses_total",
        "counter",
        "Cache misses per origin host",
    );
    header(
        &mut out,
        "host_bypass_total",
        "counter",
        "Uncacheable requests per origin host",
    );
    header(
        &mut out,
        "host_errors_total",
        "counter",
        "5xx/502 responses per origin host",
    );
    header(
        &mut out,
        "host_bytes_total",
        "counter",
        "Bytes per origin host",
    );
    for (host, s) in m.hosts_sorted().into_iter().take(100) {
        let l = format!("host=\"{}\"", escape(&host));
        line(&mut out, "host_requests_total", &l, s.requests);
        line(&mut out, "host_hits_total", &l, s.hits);
        line(&mut out, "host_misses_total", &l, s.misses);
        line(&mut out, "host_bypass_total", &l, s.bypass);
        line(&mut out, "host_errors_total", &l, s.errors);
        line(&mut out, "host_bytes_total", &l, s.bytes);
    }

    let Some(c) = cache else {
        return out;
    };
    let (mem_bytes, mem_entries) = c.mem_usage();
    let (disk_bytes, disk_entries) = c.disk_usage();
    header(
        &mut out,
        "cache_hits_total",
        "counter",
        "Cache hits by tier",
    );
    line(
        &mut out,
        "cache_hits_total",
        "tier=\"memory\"",
        c.hits_mem.load(Ordering::Relaxed),
    );
    line(
        &mut out,
        "cache_hits_total",
        "tier=\"disk\"",
        c.hits_disk.load(Ordering::Relaxed),
    );
    header(&mut out, "cache_misses_total", "counter", "Cache misses");
    line(
        &mut out,
        "cache_misses_total",
        "",
        c.misses.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_stores_total",
        "counter",
        "Responses stored",
    );
    line(
        &mut out,
        "cache_stores_total",
        "",
        c.stores.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_revalidations_total",
        "counter",
        "Successful revalidations (304)",
    );
    line(
        &mut out,
        "cache_revalidations_total",
        "",
        c.revalidations.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_background_revalidations_total",
        "counter",
        "Revalidations completed in the background",
    );
    line(
        &mut out,
        "cache_background_revalidations_total",
        "",
        c.background_revalidations.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_stale_served_total",
        "counter",
        "Expired entries served (grace, origin failure, slow origin)",
    );
    line(
        &mut out,
        "cache_stale_served_total",
        "",
        c.stale_served.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_revalidating",
        "gauge",
        "Background revalidations in flight",
    );
    line(&mut out, "cache_revalidating", "", c.revalidating_count());
    header(
        &mut out,
        "cache_evictions_total",
        "counter",
        "Entries evicted",
    );
    line(
        &mut out,
        "cache_evictions_total",
        "",
        c.evictions.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_bytes_served_total",
        "counter",
        "Bytes served from cache",
    );
    line(
        &mut out,
        "cache_bytes_served_total",
        "",
        c.bytes_served.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_used_bytes",
        "gauge",
        "Bytes held by cache entries",
    );
    line(&mut out, "cache_used_bytes", "tier=\"memory\"", mem_bytes);
    line(&mut out, "cache_used_bytes", "tier=\"disk\"", disk_bytes);
    header(
        &mut out,
        "cache_limit_bytes",
        "gauge",
        "Current cache budget",
    );
    line(
        &mut out,
        "cache_limit_bytes",
        "tier=\"memory\"",
        c.mem_capacity(),
    );
    line(
        &mut out,
        "cache_limit_bytes",
        "tier=\"disk\"",
        c.disk_capacity(),
    );
    header(
        &mut out,
        "cache_reserved_bytes",
        "gauge",
        "Ballast reserved ahead of use",
    );
    line(
        &mut out,
        "cache_reserved_bytes",
        "tier=\"memory\"",
        c.mem_reserved(),
    );
    line(
        &mut out,
        "cache_reserved_bytes",
        "tier=\"disk\"",
        c.disk_reserved(),
    );
    header(&mut out, "cache_entries", "gauge", "Entries per tier");
    line(&mut out, "cache_entries", "tier=\"memory\"", mem_entries);
    line(&mut out, "cache_entries", "tier=\"disk\"", disk_entries);
    let snap = c.snapshot();
    header(
        &mut out,
        "cache_keep_free_bytes",
        "gauge",
        "Dynamic safety margin",
    );
    line(
        &mut out,
        "cache_keep_free_bytes",
        "tier=\"memory\"",
        snap.mem_keep_free,
    );
    line(
        &mut out,
        "cache_keep_free_bytes",
        "tier=\"disk\"",
        snap.disk_keep_free,
    );
    if let Some(mem) = &snap.mem {
        header(
            &mut out,
            "system_memory_bytes",
            "gauge",
            "System memory as seen by the proxy",
        );
        line(&mut out, "system_memory_bytes", "kind=\"total\"", mem.total);
        line(
            &mut out,
            "system_memory_bytes",
            "kind=\"available\"",
            mem.available,
        );
        line(
            &mut out,
            "system_memory_bytes",
            "kind=\"active_file\"",
            mem.active_file,
        );
        if let (Some(l), Some(u)) = (mem.cgroup_limit(), mem.cgroup_usage()) {
            line(&mut out, "system_memory_bytes", "kind=\"cgroup_limit\"", l);
            line(&mut out, "system_memory_bytes", "kind=\"cgroup_usage\"", u);
        }
        header(
            &mut out,
            "system_memory_pressure",
            "gauge",
            "1 when PSI indicates memory pressure",
        );
        line(
            &mut out,
            "system_memory_pressure",
            "",
            u8::from(mem.under_pressure()),
        );
    }
    if let Some(fs) = snap.fs {
        header(
            &mut out,
            "system_disk_bytes",
            "gauge",
            "Disk (or quota) as seen by the proxy",
        );
        line(&mut out, "system_disk_bytes", "kind=\"total\"", fs.total);
        line(
            &mut out,
            "system_disk_bytes",
            "kind=\"available\"",
            fs.available,
        );
    }
    if let Some(rss) = snap.rss {
        header(
            &mut out,
            "process_rss_bytes",
            "gauge",
            "Resident set size of the proxy",
        );
        line(&mut out, "process_rss_bytes", "", rss);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::HostOutcome;

    #[test]
    fn renders_counters_and_escaped_labels() {
        let m = Metrics::new();
        m.inc_requests();
        m.record_host("http://a\"b:80", HostOutcome::Hit, 10);
        let text = render(&m, None);
        assert!(
            text.contains("# TYPE sorahost_requests_total counter\nsorahost_requests_total 1\n")
        );
        assert!(
            text.contains("sorahost_host_hits_total{host=\"http://a\\\"b:80\"} 1\n"),
            "{}",
            text
        );
        assert!(
            !text.contains("sorahost_cache_hits_total"),
            "no cache section without a cache"
        );
        assert_eq!(escape("x\\y\n"), "x\\\\y\\n");
    }
}
