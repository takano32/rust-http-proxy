//! `/metrics` のうちカーネルと cgroup の統計 (T14.12。T.34.27 で `prom.rs` から移した)。

use super::*;
use crate::kernel::{self, Latest};

/// カーネルと cgroup の統計 (T14.12)。**5 秒の標本で読んだ最新の値**をそのまま出す
/// (`/metrics` を引いた瞬間に `/proc` を読み直したりはしない)。
///
/// 読めなかった源 (Linux 以外、`/proc/net` の無いコンテナ、cgroup v1、PSI 無しのカーネル) と、
/// まだ 1 本も標本が無いとき (`--lite` / `PROXY_STATS_PERSIST=off` では履歴スレッドが
/// 動かない) は**その系列を出さない** (0 を出すと「溢れていない」と読めてしまう)。
pub(super) fn kernel_metrics(out: &mut String) {
    let Some(k) = kernel::latest() else {
        return;
    };
    net_metrics(out, &k);
    cgroup_metrics(out, &k);
    psi_metrics(out, &k);
}

/// 待ち受けと TCP の数え物 (`/proc/net/netstat`・`snmp`・`sockstat`。`kernel_*`)。
fn net_metrics(out: &mut String, k: &Latest) {
    if k.has(kernel::SRC_NETSTAT) {
        for (name, help, v) in [
            (
                "kernel_listen_overflows_total",
                "Connections dropped because the accept queue was full (/proc/net/netstat)",
                k.listen_overflows,
            ),
            (
                "kernel_listen_drops_total",
                "Connections dropped on a listening socket for any reason",
                k.listen_drops,
            ),
            (
                "kernel_tcp_timeouts_total",
                "TCP retransmission timers that expired",
                k.tcp_timeouts,
            ),
            (
                "kernel_syn_retrans_total",
                "SYN segments retransmitted (the client waited 1-3 s)",
                k.syn_retrans,
            ),
            (
                "kernel_abort_on_timeout_total",
                "Connections given up after retransmitting",
                k.abort_on_timeout,
            ),
        ] {
            header(out, name, "counter", help);
            line(out, name, "", v);
        }
    }
    if k.has(kernel::SRC_SNMP) {
        header(
            out,
            "kernel_retrans_segs_total",
            "counter",
            "TCP segments retransmitted, machine wide (/proc/net/snmp)",
        );
        line(out, "kernel_retrans_segs_total", "", k.retrans_segs);
        header(
            out,
            "kernel_curr_estab",
            "gauge",
            "Sockets in ESTABLISHED or CLOSE-WAIT, machine wide",
        );
        line(out, "kernel_curr_estab", "", k.curr_estab);
    }
    if k.has(kernel::SRC_SOCKSTAT) {
        // TIME_WAIT は `tcp_max_tw_buckets` に当たると新しい接続が張れなくなる
        header(
            out,
            "kernel_time_wait",
            "gauge",
            "Sockets in TIME_WAIT, machine wide (/proc/net/sockstat)",
        );
        line(out, "kernel_time_wait", "", k.time_wait);
        header(
            out,
            "kernel_sockets_inuse",
            "gauge",
            "TCP sockets in use (IPv4 table)",
        );
        line(out, "kernel_sockets_inuse", "", k.sockets_inuse);
        header(
            out,
            "kernel_sockets_alloc",
            "gauge",
            "TCP sockets allocated",
        );
        line(out, "kernel_sockets_alloc", "", k.sockets_alloc);
    }
}

/// cgroup の CPU の絞りと割り当て (`cgroup_cpu_*`)。
fn cgroup_metrics(out: &mut String, k: &Latest) {
    if k.has(kernel::SRC_CGROUP_CPU) {
        header(
            out,
            "cgroup_cpu_throttled_seconds_total",
            "counter",
            "Time this cgroup was throttled by its CPU quota (cpu.stat)",
        );
        line(
            out,
            "cgroup_cpu_throttled_seconds_total",
            "",
            format!("{:.6}", k.cpu_throttled_usec as f64 / 1e6),
        );
        header(
            out,
            "cgroup_cpu_throttled_periods_total",
            "counter",
            "Periods in which this cgroup was throttled (cpu.stat nr_throttled)",
        );
        line(
            out,
            "cgroup_cpu_throttled_periods_total",
            "",
            k.cpu_nr_throttled,
        );
        // 絞られた**割合**を出すための分母 (T15.0 (6))。
        // `rate(throttled_periods[5m]) / rate(periods[5m])` で読む
        header(
            out,
            "cgroup_cpu_periods_total",
            "counter",
            "CPU quota periods that elapsed for this cgroup (cpu.stat nr_periods)",
        );
        line(out, "cgroup_cpu_periods_total", "", k.cpu_nr_periods);
    }
    if k.cpu_quota_cores > 0.0 {
        header(
            out,
            "cgroup_cpu_quota_cores",
            "gauge",
            "CPU cores this cgroup may use (cpu.max quota / period)",
        );
        line(
            out,
            "cgroup_cpu_quota_cores",
            "",
            format!("{:.3}", k.cpu_quota_cores),
        );
    }
}

/// PSI (`psi_*_avg10`)。
fn psi_metrics(out: &mut String, k: &Latest) {
    // PSI: 直近 10 秒のうち、その資源を待って進めなかった時間の割合 (%)
    let psi = [
        ("cpu", kernel::SRC_PSI_CPU, k.psi_cpu_some, k.psi_cpu_full),
        (
            "memory",
            kernel::SRC_PSI_MEM,
            k.psi_mem_some,
            k.psi_mem_full,
        ),
        ("io", kernel::SRC_PSI_IO, k.psi_io_some, k.psi_io_full),
    ];
    if psi.iter().any(|(_, src, _, _)| k.has(*src)) {
        header(
            out,
            "psi_some_avg10",
            "gauge",
            "Share of the last 10 s in which at least one task stalled on this resource (%)",
        );
        for (res, src, some, _) in psi {
            if k.has(src) {
                line(
                    out,
                    "psi_some_avg10",
                    &format!("resource=\"{}\"", res),
                    format!("{:.2}", some),
                );
            }
        }
        header(
            out,
            "psi_full_avg10",
            "gauge",
            "Share of the last 10 s in which every task stalled on this resource (%)",
        );
        for (res, src, _, full) in psi {
            if k.has(src) {
                line(
                    out,
                    "psi_full_avg10",
                    &format!("resource=\"{}\"", res),
                    format!("{:.2}", full),
                );
            }
        }
    }
}
