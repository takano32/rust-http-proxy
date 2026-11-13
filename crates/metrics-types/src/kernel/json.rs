//! `/status` の `kernel` の節 ([`status_json_of`]) と `/history` の `kernel` の配列
//! ([`history_json_of`] / [`Sample::push_row`]) を組む。読むのは**引数で受けた値だけ**
//! (グローバルの窓を読んで渡すのは `kernel` の口)。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::PathBuf;

use super::{
    Health, KEYS, Latest, RESOLUTIONS, SRC_CGROUP_CPU, SRC_CGROUP_USAGE, SRC_NETSTAT, SRC_PSI_CPU,
    SRC_PSI_IO, SRC_PSI_MEM, SRC_SNMP, SRC_SOCKSTAT, SRC_STATE_FILE, Sample,
};

impl Sample {
    /// 1 標本を配列 1 行として書く ([`KEYS`] の順。読めない源は `null`)。
    pub(super) fn push_row(&self, out: &mut String) {
        let net = self.has(SRC_NETSTAT);
        let snmp = self.has(SRC_SNMP);
        let sock = self.has(SRC_SOCKSTAT);
        let cpu = self.has(SRC_CGROUP_CPU);
        let _ = write!(out, "[{}", self.t);
        for (v, ok) in [
            (self.listen_overflows, net),
            (self.listen_drops, net),
            (self.tcp_timeouts, net),
            (self.syn_retrans, net),
            (self.abort_on_timeout, net),
            (self.retrans_segs, snmp),
            (self.curr_estab, snmp),
            (self.sockets_inuse, sock),
            (self.time_wait, sock),
            (self.sockets_alloc, sock),
            (self.sockets_mem, sock),
            (self.cpu_nr_throttled, cpu),
            (self.cpu_throttled_usec, cpu),
        ] {
            out.push(',');
            push_u64(out, v, ok);
        }
        for (v, ok) in [
            (self.psi_cpu_some, self.has(SRC_PSI_CPU)),
            (self.psi_cpu_full, self.has(SRC_PSI_CPU)),
            (self.psi_mem_some, self.has(SRC_PSI_MEM)),
            (self.psi_mem_full, self.has(SRC_PSI_MEM)),
            (self.psi_io_some, self.has(SRC_PSI_IO)),
            (self.psi_io_full, self.has(SRC_PSI_IO)),
        ] {
            out.push(',');
            push_f64(out, v, ok);
        }
        let _ = write!(out, ",{},", self.dns_misses);
        // ミスが無かった窓は「1 回の ms」が存在しない
        push_u64(out, self.dns_miss_ms, self.dns_misses > 0);
        out.push(',');
        push_u64(out, self.state_file_errors, self.has(SRC_STATE_FILE));
        // **[`KEYS`] の末尾に足したもの** (T15.0)
        out.push(',');
        push_u64(out, self.cpu_nr_periods, cpu);
        // T16.0 で末尾に足したもの (`user_usec` の行が無い `cpu.stat` では `null`)
        let usage = self.has(SRC_CGROUP_USAGE);
        out.push(',');
        push_u64(out, self.cpu_user_usec, usage);
        out.push(',');
        push_u64(out, self.cpu_system_usec, usage);
        out.push(']');
    }
}

/// `/status` の `kernel` の節 (最新の値と累計、直近 5 分の増分)。
///
/// `l` は最新の生の値、`h` は直近 5 分、`first` は起動して最初に読んだ生の値 (`since_start` の
/// 引き算の相手)、`cgroup_path` は `cpu.stat` を読んでいる階層 (**`cgroup_cpu` が読めたときだけ**
/// 引く。道を引くのはここと `--check` だけで、5 秒ごとには引かない)、`samples` は 5 秒の環の本数。
pub(super) fn status_json_of(
    l: &Latest,
    h: &Health,
    first: Option<&Latest>,
    cgroup_path: impl FnOnce() -> Option<PathBuf>,
    samples: usize,
) -> String {
    let mut out = String::with_capacity(768);
    let _ = write!(out, "{{\"at\":{},\"tcp\":{{", l.at);
    push_tcp(&mut out, l);
    out.push_str("},\"cgroup_cpu\":");
    push_cgroup_cpu(&mut out, l, first, cgroup_path);
    out.push_str(",\"psi\":{");
    push_psi(&mut out, l);
    // 直近 5 分の増分 (`/healthz` が見ているのと同じ値)
    out.push_str("},\"last_5m\":{\"listen_overflows\":");
    push_last_5m(&mut out, h);
    let _ = write!(out, "}},\"samples\":{}}}", samples);
    out
}

/// `tcp` の 11 項目 (読めない源は `null`)。
fn push_tcp(out: &mut String, l: &Latest) {
    let (net, snmp, sock) = (l.has(SRC_NETSTAT), l.has(SRC_SNMP), l.has(SRC_SOCKSTAT));
    for (i, (k, v, ok)) in [
        ("listen_overflows", l.listen_overflows, net),
        ("listen_drops", l.listen_drops, net),
        ("tcp_timeouts", l.tcp_timeouts, net),
        ("syn_retrans", l.syn_retrans, net),
        ("abort_on_timeout", l.abort_on_timeout, net),
        ("retrans_segs", l.retrans_segs, snmp),
        ("curr_estab", l.curr_estab, snmp),
        ("sockets_inuse", l.sockets_inuse, sock),
        ("time_wait", l.time_wait, sock),
        ("sockets_alloc", l.sockets_alloc, sock),
        ("sockets_mem", l.sockets_mem, sock),
    ]
    .into_iter()
    .enumerate()
    {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\":", k);
        push_u64(out, v, ok);
    }
}

/// `cgroup_cpu` (読めなければ `null`)。
fn push_cgroup_cpu(
    out: &mut String,
    l: &Latest,
    first: Option<&Latest>,
    cgroup_path: impl FnOnce() -> Option<PathBuf>,
) {
    if !l.has(SRC_CGROUP_CPU) {
        out.push_str("null");
        return;
    }
    let _ = write!(
        out,
        "{{\"nr_throttled\":{},\"throttled_usec\":{},\"quota_cores\":",
        l.cpu_nr_throttled, l.cpu_throttled_usec
    );
    push_f64(out, l.cpu_quota_cores, l.cpu_quota_cores > 0.0);
    // **ここから下は T15.0 で末尾に足したもの** (既存の 3 鍵の順は動かさない)。
    // `nr_periods` は割合の分母、`path` はどの階層を読んでいるか、
    // `since_start` は**このプロセスが始まってからの**増分 (累計は親の値を含む)
    let _ = write!(out, ",\"nr_periods\":{},\"path\":", l.cpu_nr_periods);
    // 道を引くのはここ (`/status` を組むとき) と `--check` だけ。5 秒ごとには引かない
    match cgroup_path() {
        Some(p) => {
            let _ = write!(out, "\"{}\"", crate::json::escape(&p.display().to_string()));
        }
        None => out.push_str("null"),
    }
    out.push_str(",\"since_start\":");
    let usage = l.has(SRC_CGROUP_USAGE);
    match first {
        Some(f) => {
            let _ = write!(
                out,
                "{{\"nr_periods\":{},\"nr_throttled\":{},\"throttled_usec\":{}",
                l.cpu_nr_periods.saturating_sub(f.cpu_nr_periods),
                l.cpu_nr_throttled.saturating_sub(f.cpu_nr_throttled),
                l.cpu_throttled_usec.saturating_sub(f.cpu_throttled_usec),
            );
            // T16.0 で末尾に足したもの (起動からの増分)。最初の 1 本で読めていなければ
            // 引き算の相手が無いので `null`
            let since_ok = usage && f.has(SRC_CGROUP_USAGE);
            push_usage(
                out,
                [
                    l.cpu_usage_usec.saturating_sub(f.cpu_usage_usec),
                    l.cpu_user_usec.saturating_sub(f.cpu_user_usec),
                    l.cpu_system_usec.saturating_sub(f.cpu_system_usec),
                ],
                since_ok,
            );
            out.push('}');
        }
        None => out.push_str("null"),
    }
    // T16.0 で末尾に足したもの (累計 us。`/proc/self/stat` と違いコンテナ全体)
    push_usage(
        out,
        [l.cpu_usage_usec, l.cpu_user_usec, l.cpu_system_usec],
        usage,
    );
    out.push('}');
}

/// `psi` の 3 資源 (読めない資源は `null`)。
fn push_psi(out: &mut String, l: &Latest) {
    for (i, (k, some, full, ok)) in [
        ("cpu", l.psi_cpu_some, l.psi_cpu_full, l.has(SRC_PSI_CPU)),
        ("memory", l.psi_mem_some, l.psi_mem_full, l.has(SRC_PSI_MEM)),
        ("io", l.psi_io_some, l.psi_io_full, l.has(SRC_PSI_IO)),
    ]
    .into_iter()
    .enumerate()
    {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\":", k);
        if ok {
            out.push_str("{\"some_avg10\":");
            push_f64(out, some, true);
            out.push_str(",\"full_avg10\":");
            push_f64(out, full, true);
            out.push('}');
        } else {
            out.push_str("null");
        }
    }
}

/// `last_5m` の 3 つ (先頭の鍵 `listen_overflows` は呼ぶ側が書いてある)。
fn push_last_5m(out: &mut String, h: &Health) {
    push_u64(
        out,
        h.listen_overflows_5m.unwrap_or(0),
        h.listen_overflows_5m.is_some(),
    );
    out.push_str(",\"state_file_errors\":");
    push_u64(
        out,
        h.state_file_errors_5m.unwrap_or(0),
        h.state_file_errors_5m.is_some(),
    );
    out.push_str(",\"dns_miss_ms\":");
    push_u64(out, h.dns_miss_ms.unwrap_or(0), h.dns_miss_ms.is_some());
}

/// `/history` に足す `kernel` の配列 (`res` は 0 = 5 秒、1 = 60 秒。`ring` はその環)。
pub(super) fn history_json_of(res: usize, ring: &VecDeque<Sample>) -> String {
    let mut out = String::with_capacity(128 + ring.len() * 160);
    let _ = write!(out, "{{\"interval_secs\":{},\"keys\":[", RESOLUTIONS[res].0);
    for (i, k) in KEYS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", k);
    }
    out.push_str("],\"samples\":[");
    for (i, s) in ring.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        s.push_row(&mut out);
    }
    out.push_str("]}");
    out
}

/// `,"usage_usec":…,"user_usec":…,"system_usec":…` (T16.0。読めなければ 3 つとも `null`)。
fn push_usage(out: &mut String, v: [u64; 3], ok: bool) {
    for (k, v) in ["usage_usec", "user_usec", "system_usec"].iter().zip(v) {
        let _ = write!(out, ",\"{}\":", k);
        push_u64(out, v, ok);
    }
}

fn push_u64(out: &mut String, v: u64, ok: bool) {
    if ok {
        let _ = write!(out, "{}", v);
    } else {
        out.push_str("null");
    }
}

fn push_f64(out: &mut String, v: f64, ok: bool) {
    if ok && v.is_finite() {
        let _ = write!(out, "{:.2}", v);
    } else {
        out.push_str("null");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// `/status` の `kernel` の節を 1 バイト単位で縛る (読めた源・`since_start`・直近 5 分)。
    #[test]
    fn the_status_json_of_a_known_state_is_byte_for_byte() {
        let avail = SRC_NETSTAT
            | SRC_SNMP
            | SRC_SOCKSTAT
            | SRC_CGROUP_CPU
            | SRC_CGROUP_USAGE
            | SRC_PSI_CPU
            | SRC_STATE_FILE;
        let l = Latest {
            at: 1_000_005,
            avail,
            listen_overflows: 3,
            listen_drops: 1,
            syn_retrans: 2,
            retrans_segs: 10,
            curr_estab: 40,
            sockets_inuse: 50,
            time_wait: 30_912,
            sockets_alloc: 60,
            sockets_mem: 7,
            cpu_nr_throttled: 965,
            cpu_throttled_usec: 5_000,
            cpu_nr_periods: 8_050,
            cpu_usage_usec: 31_000_000,
            cpu_user_usec: 13_000_000,
            cpu_system_usec: 18_000_000,
            cpu_quota_cores: 1.5,
            psi_cpu_some: 43.02,
            psi_cpu_full: 2.99,
            ..Latest::default()
        };
        let first = Latest {
            at: 1_000_000,
            avail,
            cpu_nr_throttled: 915,
            cpu_nr_periods: 8_000,
            cpu_usage_usec: 30_000_000,
            cpu_user_usec: 12_700_000,
            cpu_system_usec: 17_300_000,
            ..Latest::default()
        };
        let h = Health {
            listen_overflows_5m: Some(3),
            state_file_errors_5m: Some(0),
            dns_miss_ms: Some(13),
            cpu_throttled_5m: Some((50, 50)),
            sampled_secs: Some(5),
        };
        let json = status_json_of(
            &l,
            &h,
            Some(&first),
            || Some(PathBuf::from("/sys/fs/cgroup/a b")),
            2,
        );
        assert_eq!(
            json,
            concat!(
                "{\"at\":1000005,\"tcp\":{\"listen_overflows\":3,\"listen_drops\":1,\"tcp_timeouts\":0,",
                "\"syn_retrans\":2,\"abort_on_timeout\":0,\"retrans_segs\":10,\"curr_estab\":40,",
                "\"sockets_inuse\":50,\"time_wait\":30912,\"sockets_alloc\":60,\"sockets_mem\":7},",
                "\"cgroup_cpu\":{\"nr_throttled\":965,\"throttled_usec\":5000,\"quota_cores\":1.50,",
                "\"nr_periods\":8050,\"path\":\"/sys/fs/cgroup/a b\",",
                "\"since_start\":{\"nr_periods\":50,\"nr_throttled\":50,\"throttled_usec\":5000,",
                "\"usage_usec\":1000000,\"user_usec\":300000,\"system_usec\":700000},",
                "\"usage_usec\":31000000,\"user_usec\":13000000,\"system_usec\":18000000},",
                "\"psi\":{\"cpu\":{\"some_avg10\":43.02,\"full_avg10\":2.99},\"memory\":null,\"io\":null},",
                "\"last_5m\":{\"listen_overflows\":3,\"state_file_errors\":0,\"dns_miss_ms\":13},",
                "\"samples\":2}"
            )
        );
    }

    /// cgroup が読めない環境では `cgroup_cpu` が `null` で、**階層の道も引かない**。
    /// 読めない源は `null`、最初の 1 本が無ければ `since_start` も `null`。
    #[test]
    fn an_unreadable_cgroup_is_null_and_its_path_is_not_looked_up() {
        let l = Latest {
            at: 5,
            avail: SRC_NETSTAT,
            listen_overflows: 1,
            ..Latest::default()
        };
        let asked = Cell::new(false);
        let json = status_json_of(
            &l,
            &Health::default(),
            None,
            || {
                asked.set(true);
                None
            },
            0,
        );
        assert!(!asked.get(), "読めないときは道を引かない");
        assert_eq!(
            json,
            concat!(
                "{\"at\":5,\"tcp\":{\"listen_overflows\":1,\"listen_drops\":0,\"tcp_timeouts\":0,",
                "\"syn_retrans\":0,\"abort_on_timeout\":0,\"retrans_segs\":null,\"curr_estab\":null,",
                "\"sockets_inuse\":null,\"time_wait\":null,\"sockets_alloc\":null,\"sockets_mem\":null},",
                "\"cgroup_cpu\":null,\"psi\":{\"cpu\":null,\"memory\":null,\"io\":null},",
                "\"last_5m\":{\"listen_overflows\":null,\"state_file_errors\":null,\"dns_miss_ms\":null},",
                "\"samples\":0}"
            )
        );
        // 読めるが最初の 1 本が無い (起動直後の 1 本目を読む前) と、道が無いときは `null`
        let cg = Latest {
            avail: SRC_CGROUP_CPU,
            cpu_nr_periods: 8,
            ..Latest::default()
        };
        let json = status_json_of(&cg, &Health::default(), None, || None, 1);
        assert!(
            json.contains(
                "\"cgroup_cpu\":{\"nr_throttled\":0,\"throttled_usec\":0,\"quota_cores\":null,\"nr_periods\":8,\"path\":null,\"since_start\":null,\"usage_usec\":null,\"user_usec\":null,\"system_usec\":null}"
            ),
            "{json}"
        );
    }
}
