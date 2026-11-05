//! `/metrics` のうちホスト別・接続元別の表。
//!
//! T.34.27 で `prom.rs` の `render` から割った。行を動かしただけで、出す順も文面も変えていない。

use super::*;
use crate::metrics::HostStats;

/// ホスト別の数え物 (上位 100。`host_*_total`)。
pub(super) fn host_metrics(out: &mut String, hosts: &[(String, HostStats)]) {
    header(
        out,
        "host_requests_total",
        "counter",
        "Requests per origin host",
    );
    header(
        out,
        "host_hits_total",
        "counter",
        "Cache hits per origin host",
    );
    header(
        out,
        "host_misses_total",
        "counter",
        "Cache misses per origin host",
    );
    header(
        out,
        "host_bypass_total",
        "counter",
        "Uncacheable requests per origin host",
    );
    header(
        out,
        "host_errors_total",
        "counter",
        "5xx/502 responses per origin host",
    );
    header(out, "host_bytes_total", "counter", "Bytes per origin host");
    for (host, s) in hosts {
        let l = format!("host=\"{}\"", escape(host));
        line(out, "host_requests_total", &l, s.requests);
        line(out, "host_hits_total", &l, s.hits);
        line(out, "host_misses_total", &l, s.misses);
        line(out, "host_bypass_total", &l, s.bypass);
        line(out, "host_errors_total", &l, s.errors);
        line(out, "host_bytes_total", &l, s.bytes);
    }
    header(
        out,
        "host_blocked_total",
        "counter",
        "Requests refused by the ACL or blocklist per host",
    );
    for (host, s) in hosts {
        if s.blocked > 0 {
            line(
                out,
                "host_blocked_total",
                &format!("host=\"{}\"", escape(host)),
                s.blocked,
            );
        }
    }
}

/// 接続元別の数え物 (上位 100。`client_*_total`)。
pub(super) fn client_metrics(out: &mut String, clients: &[(String, HostStats)]) {
    header(
        out,
        "client_requests_total",
        "counter",
        "Requests per client address",
    );
    header(
        out,
        "client_bytes_total",
        "counter",
        "Bytes per client address",
    );
    header(
        out,
        "client_blocked_total",
        "counter",
        "Refused requests per client address",
    );
    for (client, c) in clients {
        let l = format!("client=\"{}\"", escape(client));
        line(out, "client_requests_total", &l, c.requests);
        line(out, "client_bytes_total", &l, c.bytes);
        line(out, "client_blocked_total", &l, c.blocked);
    }
}

/// ホスト別の応答時間のヒストグラム (上位 50。`host_request_duration_seconds`)。
pub(super) fn host_duration_metrics(out: &mut String, hosts: &[(String, HostStats)]) {
    header(
        out,
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
}
