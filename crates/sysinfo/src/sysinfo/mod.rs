//! OS の資源使用状況 (メモリ / ファイルシステム / 自プロセス) を依存クレートなしで取得する。
//!
//! - [`mem`]: `/proc/meminfo`, `/proc/self/status`, `/proc/pressure/memory`, cgroup v1/v2
//! - [`fs`]: `statvfs(3)` / `fallocate(2)` を libc シンボル直接参照で呼び、`/proc/mounts` で
//!   マウント種別を調べる
//!
//! Linux 以外や `/proc` が無い環境では各関数が `None` / `Unsupported` を返し、
//! 呼び出し側 (キャッシュの自動予算) は固定の既定値へフォールバックする。

pub mod fs;
pub mod inotify;
pub mod mem;

pub use fs::{
    FsInfo, dir_size_excluding, drop_page_cache, fs_info, fs_type, is_ram_backed, is_unsupported,
    preallocate,
};
pub use mem::{
    CgroupMem, MemInfo, MemPressure, cgroup_mem_limits, mem_info, mem_pressure, min_free_bytes,
    process_rss,
};
