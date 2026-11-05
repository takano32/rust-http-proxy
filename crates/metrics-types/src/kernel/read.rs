//! いまの生の値を源から読む (`/proc/net`・cgroup・PSI・名前解決の累計・状態ファイルの書込エラー)。
//!
//! **読む (I/O) と旗を立てる判断を分けてある**: 源の値を与えれば [`Latest::from_sources`] は
//! 試験から直に呼べる (`/proc` の無い環境でも、読めない源が `null` になる形を確かめられる)。

use crate::sysinfo::cgroup::{CgroupCpu, CgroupPressure, cgroup_cpu_and_pressure};
use crate::sysinfo::net::{TcpStats, tcp_stats};

use super::{
    Latest, SRC_CGROUP_CPU, SRC_CGROUP_USAGE, SRC_NETSTAT, SRC_PSI_CPU, SRC_PSI_IO, SRC_PSI_MEM,
    SRC_SNMP, SRC_SOCKSTAT, SRC_STATE_FILE,
};

/// いまの生の値を読む (源ごとに読めたかを旗で持つ)。**5 秒の標本のときだけ**呼ぶこと。
pub(super) fn read_now(t: u64) -> Latest {
    let tcp = tcp_stats();
    // `/proc/self/cgroup` は 1 回だけ読む (T.34.148)
    let (cpu, psi) = cgroup_cpu_and_pressure();
    let dns = crate::dns::resolve_cost_total();
    let state_file = super::STATE_FILE_ERRORS.get().and_then(|f| f());
    Latest::from_sources(t, &tcp, &cpu, &psi, dns, state_file)
}

impl Latest {
    /// 源から読んだ値を 1 つの生の値にまとめ、読めた源の旗を立てる。
    ///
    /// `dns` は名前解決の累計 `(合計 us, ミスの回数)`、`state_file` は状態ファイルの書込エラーの累計
    /// (永続化していなければ `None` = 旗を立てない)。
    pub(super) fn from_sources(
        t: u64,
        tcp: &TcpStats,
        cpu: &CgroupCpu,
        psi: &CgroupPressure,
        (dns_us_sum, dns_misses): (u64, u64),
        state_file: Option<u64>,
    ) -> Latest {
        let mut l = Latest {
            at: t,
            dns_misses,
            dns_us_sum,
            ..Latest::default()
        };
        if let Some(e) = tcp.ext {
            l.avail |= SRC_NETSTAT;
            l.listen_overflows = e.listen_overflows;
            l.listen_drops = e.listen_drops;
            l.tcp_timeouts = e.tcp_timeouts;
            l.syn_retrans = e.syn_retrans;
            l.abort_on_timeout = e.abort_on_timeout;
        }
        if let Some(s) = tcp.snmp {
            l.avail |= SRC_SNMP;
            l.retrans_segs = s.retrans_segs;
            l.curr_estab = s.curr_estab;
        }
        if let Some(s) = tcp.sock {
            l.avail |= SRC_SOCKSTAT;
            l.sockets_inuse = s.inuse;
            l.time_wait = s.tw;
            l.sockets_alloc = s.alloc;
            l.sockets_mem = s.mem;
        }
        // cgroup v2 で cpu.stat が読めたときだけ (v1 と読めない環境は `null`)
        if cpu.nr_throttled.is_some() || cpu.throttled_usec.is_some() || cpu.nr_periods.is_some() {
            l.avail |= SRC_CGROUP_CPU;
            l.cpu_nr_throttled = cpu.nr_throttled.unwrap_or(0);
            l.cpu_throttled_usec = cpu.throttled_usec.unwrap_or(0);
            l.cpu_nr_periods = cpu.nr_periods.unwrap_or(0);
            // 同じファイルの `usage_usec` / `user_usec` / `system_usec` (T16.0)。
            // カーネルは 3 行を揃えて書くので、3 つとも読めたときだけ数で出す
            if let (Some(usage), Some(user), Some(system)) =
                (cpu.usage_usec, cpu.user_usec, cpu.system_usec)
            {
                l.avail |= SRC_CGROUP_USAGE;
                l.cpu_usage_usec = usage;
                l.cpu_user_usec = user;
                l.cpu_system_usec = system;
            }
        }
        l.cpu_quota_cores = cpu.quota_cores.unwrap_or(0.0);
        if let Some(p) = psi.cpu {
            l.avail |= SRC_PSI_CPU;
            l.psi_cpu_some = p.some_avg10;
            l.psi_cpu_full = p.full_avg10;
        }
        if let Some(p) = psi.memory {
            l.avail |= SRC_PSI_MEM;
            l.psi_mem_some = p.some_avg10;
            l.psi_mem_full = p.full_avg10;
        }
        if let Some(p) = psi.io {
            l.avail |= SRC_PSI_IO;
            l.psi_io_some = p.some_avg10;
            l.psi_io_full = p.full_avg10;
        }
        if let Some(n) = state_file {
            l.avail |= SRC_STATE_FILE;
            l.state_file_errors = n;
        }
        l
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysinfo::cgroup::Psi;
    use crate::sysinfo::net::{SockStat, TcpExt};

    /// 読めた源の旗だけが立つ: 全部読めなければ名前解決の累計だけ、`cpu.stat` は絞りの 3 行の
    /// どれかで `cgroup_cpu`、使用の 3 行が揃ったときだけ `cgroup_usage`、状態ファイルは `Some` のとき。
    #[test]
    fn from_sources_sets_a_flag_per_readable_source() {
        let none = Latest::from_sources(
            7,
            &TcpStats::default(),
            &CgroupCpu::default(),
            &CgroupPressure::default(),
            (100_000, 10),
            None,
        );
        assert_eq!(none.avail, 0);
        assert_eq!(
            (none.at, none.dns_us_sum, none.dns_misses),
            (7, 100_000, 10)
        );
        assert_eq!(none.cpu_quota_cores, 0.0, "`max` (無制限) は 0");

        let tcp = TcpStats {
            ext: Some(TcpExt {
                listen_overflows: 3,
                syn_retrans: 2,
                ..TcpExt::default()
            }),
            snmp: None,
            sock: Some(SockStat {
                tw: 30_912,
                inuse: 50,
                ..SockStat::default()
            }),
        };
        let cpu = CgroupCpu {
            nr_throttled: Some(915),
            quota_cores: Some(1.5),
            ..CgroupCpu::default()
        };
        let psi = CgroupPressure {
            cpu: Some(Psi {
                some_avg10: 43.02,
                full_avg10: 2.99,
            }),
            memory: None,
            io: None,
        };
        let l = Latest::from_sources(9, &tcp, &cpu, &psi, (0, 0), Some(4));
        assert_eq!(
            l.avail,
            SRC_NETSTAT | SRC_SOCKSTAT | SRC_CGROUP_CPU | SRC_PSI_CPU | SRC_STATE_FILE
        );
        assert_eq!((l.listen_overflows, l.syn_retrans), (3, 2));
        assert_eq!((l.time_wait, l.sockets_inuse), (30_912, 50));
        assert_eq!(l.retrans_segs, 0, "snmp は読めていない");
        assert_eq!((l.cpu_nr_throttled, l.cpu_nr_periods), (915, 0));
        assert_eq!(l.cpu_quota_cores, 1.5);
        assert_eq!((l.psi_cpu_some, l.psi_cpu_full), (43.02, 2.99));
        assert_eq!(l.state_file_errors, 4);

        // `usage_usec` / `user_usec` / `system_usec` は 3 つ揃ったときだけ (T16.0)
        let two = CgroupCpu {
            nr_periods: Some(8_000),
            usage_usec: Some(31_000_000),
            user_usec: Some(13_000_000),
            ..CgroupCpu::default()
        };
        let l = Latest::from_sources(1, &TcpStats::default(), &two, &psi, (0, 0), None);
        assert_eq!(
            l.avail & (SRC_CGROUP_CPU | SRC_CGROUP_USAGE),
            SRC_CGROUP_CPU
        );
        assert_eq!(l.cpu_usage_usec, 0, "揃っていないので入れない");
        let three = CgroupCpu {
            system_usec: Some(18_000_000),
            ..two
        };
        let l = Latest::from_sources(1, &TcpStats::default(), &three, &psi, (0, 0), None);
        assert_eq!(
            l.avail & (SRC_CGROUP_CPU | SRC_CGROUP_USAGE),
            SRC_CGROUP_CPU | SRC_CGROUP_USAGE
        );
        assert_eq!(
            (l.cpu_usage_usec, l.cpu_user_usec, l.cpu_system_usec),
            (31_000_000, 13_000_000, 18_000_000)
        );
    }
}
