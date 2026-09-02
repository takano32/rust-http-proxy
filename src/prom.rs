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
    header(
        &mut out,
        "dns_lookups_total",
        "counter",
        "Name resolutions by result (hit = served from the DNS cache)",
    );
    let [hits, misses, stale, negative] = crate::dns::counters();
    line(&mut out, "dns_lookups_total", "result=\"hit\"", hits);
    line(&mut out, "dns_lookups_total", "result=\"miss\"", misses);
    line(&mut out, "dns_lookups_total", "result=\"stale\"", stale);
    line(
        &mut out,
        "dns_lookups_total",
        "result=\"negative\"",
        negative,
    );
    header(
        &mut out,
        "blocklist_entries",
        "gauge",
        "Domains in the blocklist",
    );
    line(&mut out, "blocklist_entries", "", crate::blocklist::len());
    header(
        &mut out,
        "blocklist_blocked_total",
        "counter",
        "Requests refused because the host is on the blocklist",
    );
    line(
        &mut out,
        "blocklist_blocked_total",
        "",
        crate::blocklist::blocked_total(),
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
    let hosts: Vec<_> = m.hosts_sorted().into_iter().take(100).collect();
    for (host, s) in &hosts {
        let l = format!("host=\"{}\"", escape(host));
        line(&mut out, "host_requests_total", &l, s.requests);
        line(&mut out, "host_hits_total", &l, s.hits);
        line(&mut out, "host_misses_total", &l, s.misses);
        line(&mut out, "host_bypass_total", &l, s.bypass);
        line(&mut out, "host_errors_total", &l, s.errors);
        line(&mut out, "host_bytes_total", &l, s.bytes);
    }
    header(
        &mut out,
        "host_blocked_total",
        "counter",
        "Requests refused by the ACL or blocklist per host",
    );
    for (host, s) in &hosts {
        if s.blocked > 0 {
            line(
                &mut out,
                "host_blocked_total",
                &format!("host=\"{}\"", escape(host)),
                s.blocked,
            );
        }
    }
    header(
        &mut out,
        "client_requests_total",
        "counter",
        "Requests per client address",
    );
    header(
        &mut out,
        "client_bytes_total",
        "counter",
        "Bytes per client address",
    );
    header(
        &mut out,
        "client_blocked_total",
        "counter",
        "Refused requests per client address",
    );
    let clients: Vec<_> = m.clients_sorted().into_iter().take(100).collect();
    for (client, c) in &clients {
        let l = format!("client=\"{}\"", escape(client));
        line(&mut out, "client_requests_total", &l, c.requests);
        line(&mut out, "client_bytes_total", &l, c.bytes);
        line(&mut out, "client_blocked_total", &l, c.blocked);
    }
    header(
        &mut out,
        "host_request_duration_seconds",
        "histogram",
        "Response time per origin host (CONNECT: time to establish the tunnel)",
    );
    for (host, s) in &hosts {
        if s.timed == 0 {
            continue;
        }
        let h = escape(host);
        let mut cum = 0u64;
        for (i, n) in s.buckets.iter().enumerate() {
            cum += n;
            let le = match crate::metrics::LATENCY_BOUNDS_MS.get(i) {
                Some(b) => format!("{}", *b as f64 / 1000.0),
                None => "+Inf".to_string(),
            };
            let _ = writeln!(
                out,
                "sorahost_host_request_duration_seconds_bucket{{host=\"{}\",le=\"{}\"}} {}",
                h, le, cum
            );
        }
        let _ = writeln!(
            out,
            "sorahost_host_request_duration_seconds_sum{{host=\"{}\"}} {}",
            h,
            s.duration_ms_sum as f64 / 1000.0
        );
        let _ = writeln!(
            out,
            "sorahost_host_request_duration_seconds_count{{host=\"{}\"}} {}",
            h, s.timed
        );
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
        "cache_coalesced_total",
        "counter",
        "Requests that waited for an in-flight fetch instead of contacting the origin",
    );
    line(
        &mut out,
        "cache_coalesced_total",
        "",
        c.coalesced.load(Ordering::Relaxed),
    );
    header(
        &mut out,
        "cache_inflight",
        "gauge",
        "Origin fetches in flight (coalescing table)",
    );
    line(&mut out, "cache_inflight", "", c.inflight_count());
    header(
        &mut out,
        "cache_admission_rejected_total",
        "counter",
        "Responses not stored because the URL was seen for the first time while the cache was full",
    );
    line(
        &mut out,
        "cache_admission_rejected_total",
        "",
        c.admission_rejected.load(Ordering::Relaxed),
    );
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
