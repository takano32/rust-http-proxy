//! `/metrics` 用の Prometheus テキスト形式 (text/plain; version=0.0.4) の生成。

use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use crate::cache::Cache;
use crate::metrics::{Concurrency, Metrics};

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
///
/// `conc` (上限といまのスレッドの数) を**呼び出し側から受け取る**のは、`/status` の
/// [`crate::metrics::StatusExtras`] と同じ理由: 数えるには `Config` と `Workers` が要り、
/// ここ (指標を読むだけの層) から呼ぶと依存が輪になる。数えるのに全接続スレッドで
/// 共有している鍵を取るので、**`/metrics` を組み立てるときだけ**引くこと (熱い経路に乗せない)。
pub fn render(m: &Metrics, cache: Option<&Cache>, conc: Concurrency) -> String {
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
        "Name resolutions by result (hit = served from the DNS cache, refresh = re-resolved in the background)",
    );
    let [hits, misses, stale, negative, refresh] = crate::dns::counters();
    line(&mut out, "dns_lookups_total", "result=\"hit\"", hits);
    line(&mut out, "dns_lookups_total", "result=\"miss\"", misses);
    line(&mut out, "dns_lookups_total", "result=\"stale\"", stale);
    line(
        &mut out,
        "dns_lookups_total",
        "result=\"negative\"",
        negative,
    );
    // 期限前に裏で引き直した回数 (T13.1)。利用者は待っていないので `miss` とは別系列
    line(&mut out, "dns_lookups_total", "result=\"refresh\"", refresh);
    // Happy Eyeballs の族ごとの勝敗 (T12.1)。`v4_first` は `/status` に出す
    header(
        &mut out,
        "ipv6_attempts_total",
        "counter",
        "Connections where an IPv6 candidate was tried (Happy Eyeballs)",
    );
    let [attempts, wins, losses] = crate::net::ipv6_counters();
    line(&mut out, "ipv6_attempts_total", "", attempts);
    header(
        &mut out,
        "ipv6_wins_total",
        "counter",
        "Connections established by the IPv6 candidate",
    );
    line(&mut out, "ipv6_wins_total", "", wins);
    header(
        &mut out,
        "ipv6_losses_total",
        "counter",
        "Connections where IPv6 was tried but IPv4 established first",
    );
    line(&mut out, "ipv6_losses_total", "", losses);
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
    // 上限といまのスレッドの数 (`/status` に出しているのと同じ値。T11.5)。
    // `auto` で決まった上限を、起動ログを見なくても監視側から確かめられるようにするためのもの
    header(
        &mut out,
        "max_connections",
        "gauge",
        "Client connection limit (PROXY_MAX_CONNS after auto sizing; 0 = unlimited)",
    );
    line(&mut out, "max_connections", "", conc.max_conns);
    header(
        &mut out,
        "max_threads",
        "gauge",
        "Connection thread limit (PROXY_MAX_THREADS after auto sizing; 0 = unlimited)",
    );
    line(&mut out, "max_threads", "", conc.max_threads);
    // 接続スレッドの数はラベル付きの 1 系列にそろえた (T11.5 の候補 → T12.4 (4))。
    // **旧名 (`sorahost_live_threads` / `sorahost_idle_threads`) は 1 版だけ両方出す**
    // ので、監視側は次の版までに `sorahost_threads{state=...}` へ移せる
    header(
        &mut out,
        "threads",
        "gauge",
        "Connection threads by state (replaces sorahost_live_threads / sorahost_idle_threads)",
    );
    line(&mut out, "threads", "state=\"live\"", conc.live_threads);
    line(&mut out, "threads", "state=\"idle\"", conc.idle_threads);
    header(
        &mut out,
        "live_threads",
        "gauge",
        "Connection threads alive (deprecated: use sorahost_threads{state=\"live\"})",
    );
    line(&mut out, "live_threads", "", conc.live_threads);
    header(
        &mut out,
        "idle_threads",
        "gauge",
        "Connection threads waiting for work (deprecated: use sorahost_threads{state=\"idle\"})",
    );
    line(&mut out, "idle_threads", "", conc.idle_threads);
    // プロセス全体の数え物 (接続スレッドとは別。`/proc` を読むのはこのパスだけ。T12.4 (4))
    if let Some(n) = crate::sysinfo::process_threads() {
        header(
            &mut out,
            "process_threads",
            "gauge",
            "Threads in the process (including the watcher, history and connect-attempt threads)",
        );
        line(&mut out, "process_threads", "", n);
    }
    if let Some((fds, max_fds)) = crate::sysinfo::process_fds() {
        header(&mut out, "fds", "gauge", "Open file descriptors");
        line(&mut out, "fds", "", fds);
        header(
            &mut out,
            "max_fds",
            "gauge",
            "File descriptor limit (RLIMIT_NOFILE soft)",
        );
        line(&mut out, "max_fds", "", max_fds);
    }
    header(
        &mut out,
        "queued_jobs",
        "gauge",
        "Jobs waiting because the connection thread limit was reached",
    );
    line(&mut out, "queued_jobs", "", conc.queued_jobs);
    header(
        &mut out,
        "rejected_overload_total",
        "counter",
        "Connections refused with 503 because PROXY_MAX_CONNS was reached",
    );
    line(
        &mut out,
        "rejected_overload_total",
        "",
        m.rejected_overload.load(Ordering::Relaxed),
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

    // CONNECT 確立の全体のヒストグラム (Phase 13 が最初に見る数字。T12.4 (4))。
    // ホスト別の `sorahost_host_request_duration_seconds` と違い、**区間は 12 段**で
    // 履歴 (`/history`) と同じ刻み。合計は起動からの累計
    let totals = m.totals();
    header(
        &mut out,
        "connect_seconds",
        "histogram",
        "Time to establish a CONNECT tunnel (name resolution + TCP)",
    );
    let mut cum = 0u64;
    for (i, n) in totals.connect.buckets.iter().enumerate() {
        cum += n;
        let le = match crate::history::WINDOW_BOUNDS_MS.get(i) {
            Some(b) => format!("{}", *b as f64 / 1000.0),
            None => "+Inf".to_string(),
        };
        let _ = writeln!(
            out,
            "sorahost_connect_seconds_bucket{{le=\"{}\"}} {}",
            le, cum
        );
    }
    let _ = writeln!(
        out,
        "sorahost_connect_seconds_sum {}",
        totals.connect.ms_sum as f64 / 1000.0
    );
    let _ = writeln!(
        out,
        "sorahost_connect_seconds_count {}",
        totals.connect.count
    );
    // 名前解決のミス 1 回の値段 (デプロイ先ではミス率 26%。Phase 13 の候補 1 の分子)
    let (dns_us, dns_misses) = crate::dns::resolve_cost_total();
    header(
        &mut out,
        "dns_seconds",
        "summary",
        "Time spent in getaddrinfo (cache misses only)",
    );
    let _ = writeln!(out, "sorahost_dns_seconds_sum {}", dns_us as f64 / 1e6);
    let _ = writeln!(out, "sorahost_dns_seconds_count {}", dns_misses);
    // エラーの原因 (デプロイ先の「エラー 12 件、原因は不明」を無くす)
    header(
        &mut out,
        "errors_total",
        "counter",
        "Failed requests by cause (loop = 508 Loop Detected)",
    );
    for (i, name) in crate::metrics::ERR_CAUSE_NAMES.iter().enumerate() {
        line(
            &mut out,
            "errors_total",
            &format!("cause=\"{}\"", name),
            totals.errors_by_cause[i],
        );
    }
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
    // 区間が 10 段から 24 段になったので、**ヒストグラムは上位 50 ホストまで**にする
    // (`/status` の `hosts[]` と同じ数)。100 ホスト全部だと上限まで埋まったとき
    // `/metrics` が 389 KiB になり、400 KiB の予算に 1 KiB しか残らなかった (実測)。
    // 数え上げ (`host_requests_total` など) はこれまでどおり上位 100 ホスト
    for (host, s) in hosts.iter().take(50) {
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
        "cache_revalidations_dropped_total",
        "counter",
        "Background revalidations skipped because every worker thread was busy",
    );
    line(
        &mut out,
        "cache_revalidations_dropped_total",
        "",
        c.revalidations_dropped.load(Ordering::Relaxed),
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
        "cache_not_stored_rotations_total",
        "counter",
        "Rotations of the memory of keys a leader did not store (two rotations forget a key)",
    );
    line(
        &mut out,
        "cache_not_stored_rotations_total",
        "",
        c.not_stored_rotations(),
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
        let text = render(&m, None, Concurrency::default());
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

    /// 上限といまのスレッド数が gauge として出ること (T11.5)。
    /// `/status` に出しているのと同じ 5 つで、値は上の層から渡ったものがそのまま出る。
    #[test]
    fn renders_the_limits_and_the_thread_counts_as_gauges() {
        let m = Metrics::new();
        let text = render(
            &m,
            None,
            Concurrency {
                max_conns: 137,
                max_threads: 41,
                live_threads: 7,
                idle_threads: 3,
                queued_jobs: 2,
            },
        );
        for (name, value) in [
            ("max_connections", 137),
            ("max_threads", 41),
            ("live_threads", 7),
            ("idle_threads", 3),
            ("queued_jobs", 2),
        ] {
            assert!(
                text.contains(&format!("# TYPE sorahost_{} gauge\n", name)),
                "{} の TYPE が無い:\n{}",
                name,
                text
            );
            assert!(
                text.contains(&format!("sorahost_{} {}\n", name, value)),
                "{} の値が違う:\n{}",
                name,
                text
            );
        }
        // 渡されなければ 0 (= 無制限・数えていない)
        let zeroed = render(&m, None, Concurrency::default());
        assert!(
            zeroed.contains("sorahost_max_connections 0\n"),
            "{}",
            zeroed
        );
    }

    /// 接続スレッドの数が新旧両方の名前で出ること (T12.4 (4))。旧名は 1 版だけ残す。
    #[test]
    fn thread_gauges_are_rendered_under_both_the_old_and_the_new_name() {
        let m = Metrics::new();
        let text = render(
            &m,
            None,
            Concurrency {
                live_threads: 7,
                idle_threads: 3,
                ..Concurrency::default()
            },
        );
        for pat in [
            "sorahost_threads{state=\"live\"} 7\n",
            "sorahost_threads{state=\"idle\"} 3\n",
            "sorahost_live_threads 7\n",
            "sorahost_idle_threads 3\n",
        ] {
            assert!(text.contains(pat), "{} が無い:\n{}", pat, text);
        }
    }

    /// CONNECT 確立のヒストグラム・名前解決・エラーの原因が出ること (T12.4 (4))。
    #[test]
    fn renders_the_connect_histogram_the_dns_summary_and_the_error_causes() {
        use std::time::Duration;
        let m = Metrics::new();
        m.record_host_timed(
            "connect://a:443",
            HostOutcome::Bypass,
            0,
            Duration::from_millis(257),
        );
        m.record_host_detail(
            "connect://b:443",
            HostOutcome::Error,
            0,
            Some(Duration::from_millis(30_000)),
            crate::metrics::Detail {
                cause: Some(crate::metrics::ErrCause::Timeout),
                ..crate::metrics::Detail::default()
            },
        );
        let text = render(&m, None, Concurrency::default());
        assert!(
            text.contains("# TYPE sorahost_connect_seconds histogram\n"),
            "{}",
            text
        );
        // 12 段 + `+Inf` で、累積になっていること
        assert!(
            text.contains("sorahost_connect_seconds_bucket{le=\"0.25\"} 0\n"),
            "{}",
            text
        );
        assert!(
            text.contains("sorahost_connect_seconds_bucket{le=\"0.5\"} 1\n"),
            "{}",
            text
        );
        assert!(
            text.contains("sorahost_connect_seconds_bucket{le=\"+Inf\"} 2\n"),
            "{}",
            text
        );
        assert!(
            text.contains("sorahost_connect_seconds_count 2\n"),
            "{}",
            text
        );
        assert!(
            text.contains("sorahost_connect_seconds_sum 30.257\n"),
            "{}",
            text
        );
        assert!(text.contains("sorahost_dns_seconds_count "), "{}", text);
        assert!(
            text.contains("sorahost_errors_total{cause=\"timeout\"} 1\n"),
            "{}",
            text
        );
        assert!(
            text.contains("sorahost_errors_total{cause=\"loop\"} 0\n"),
            "{}",
            text
        );
    }

    /// 上限まで埋めても `/status` 64 KiB / `/metrics` 400 KiB に収まること (T12.4 (3))。
    #[test]
    fn the_endpoints_stay_within_their_size_budget_when_the_tables_are_full() {
        let m = Metrics::new();
        for i in 0..crate::metrics::MAX_HOSTS {
            // 長めのホスト名 + 全部の区間に件数が入っている最悪の形
            let host = format!("connect://very-long-host-name-{:04}.example.com:443", i);
            for ms in [1, 5, 30, 120, 400, 900, 3000, 12_000] {
                m.record_host_timed(
                    &host,
                    HostOutcome::Bypass,
                    1 << 30,
                    std::time::Duration::from_millis(ms),
                );
            }
        }
        for i in 0..crate::metrics::MAX_CLIENTS {
            m.record_client(
                &format!("2001:db8:1234:5678:9abc:def0:1234:{:04x}", i),
                HostOutcome::Bypass,
                1 << 30,
                Some(std::time::Duration::from_millis(400)),
            );
        }
        let status = m.to_json();
        assert!(status.len() <= 64 * 1024, "/status が {} B", status.len());
        let text = render(&m, None, Concurrency::default());
        assert!(text.len() <= 400 * 1024, "/metrics が {} B", text.len());
    }

    /// 「合流を飛ばす鍵」の記憶を入れ替えた回数が `/metrics` にも出ること (T12.6)。
    #[test]
    fn renders_the_not_stored_rotations_counter() {
        let m = Metrics::new();
        let cache = Cache::new(crate::cache::CacheConfig::disabled());
        let text = render(&m, Some(&cache), Concurrency::default());
        assert!(
            text.contains(
                "# TYPE sorahost_cache_not_stored_rotations_total counter\nsorahost_cache_not_stored_rotations_total 0\n"
            ),
            "{}",
            text
        );
    }
}
