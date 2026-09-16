//! OS の資源使用状況 (メモリ / ファイルシステム / 自プロセス) を依存クレートなしで取得する。
//!
//! - [`mem`]: `/proc/meminfo`, `/proc/self/status`, `/proc/pressure/memory`, cgroup v1/v2
//! - [`fs`]: `statvfs(3)` / `fallocate(2)` を libc シンボル直接参照で呼び、`/proc/mounts` で
//!   マウント種別を調べる
//! - [`proc`]: `/proc/self/status` のスレッド数と `/proc/self/fd` の記述子の数 / `RLIMIT_NOFILE`、
//!   `/proc/self/task/*/{stat,syscall}` のスレッド別 CPU と状態 (T14.3 (2))
//! - [`malloc`]: glibc の `mallinfo2(3)` でヒープの内訳 (T14.21)
//! - [`net`]: `/proc/net/{netstat,snmp,sockstat}` のカーネルの TCP 統計 (T14.12)
//! - [`cgroup`]: cgroup v2 の CPU の絞り (`cpu.stat` / `cpu.max`) と PSI (T14.12)
//! - [`capabilities`]: この環境で何が読めるか (`/proc` の syscall、`TCP_INFO`、cgroup、IPv6、
//!   リゾルバ、`$HOME`)。`null` が「無かった」のか「読めなかった」のかを先に答える (T14.15)
//!
//! Linux 以外や `/proc` が無い環境では各関数が `None` / `Unsupported` を返し、
//! 呼び出し側 (キャッシュの自動予算) は固定の既定値へフォールバックする。

pub mod capabilities;
pub mod cgroup;
pub mod fs;
pub mod inotify;
pub mod malloc;
pub mod mem;
pub mod net;
pub mod proc;

pub use capabilities::Capabilities;
pub use cgroup::{CgroupCpu, CgroupPressure, Psi, cgroup_cpu, cgroup_pressure};
pub use fs::{
    FsInfo, dir_size_excluding, drop_page_cache, fs_info, fs_type, is_ram_backed, is_unsupported,
    preallocate,
};
pub use malloc::{MallocInfo, arena_max, malloc_info, set_arena_max};
pub use mem::{
    CgroupMem, MemInfo, MemPressure, cgroup_mem_limits, mem_info, mem_pressure, min_free_bytes,
    process_rss,
};
pub use net::{SockStat, TcpExt, TcpSnmp, TcpStats, tcp_stats};
pub use proc::{TaskSample, TaskScan, process_fds, process_threads, scan_tasks};
