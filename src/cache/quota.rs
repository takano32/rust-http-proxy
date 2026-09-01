//! ディスク割当の扱いの決定と、読み取り失敗の分類。

use std::io;
use std::path::Path;

use super::config::{CacheConfig, DiskQuota, MIB};
use crate::sysinfo;
use crate::{log_info, log_warn};

/// 読めない原因がファイル側 (消えた・壊れた) にあるか。fd 枯渇や EIO のような一時的な失敗は含めない。
pub(super) fn is_permanent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
    )
}

/// `Auto` は `df <割当ディレクトリ>` が割当を示すときだけ採用する。`/` と同じファイルシステム
/// (total が一致) ならホストディスクが見えているだけなので `Unknown` に落とす。
/// Pterodactyl では判断材料として df 相当の数字を常にログに出す。
pub(super) fn resolve_quota(cfg: &CacheConfig) -> DiskQuota {
    let root = cfg.quota_root.as_deref().unwrap_or(&cfg.dir);
    let df = sysinfo::fs_info(root);
    if cfg.pterodactyl
        && let Some(f) = df
    {
        log_info!(
            None,
            "df -B1 {}: total {} MiB, used {} MiB, available {} MiB (compare with the panel's Disk Space)",
            root.display(),
            f.total / MIB,
            f.used / MIB,
            f.available / MIB
        );
    }
    // 明示の auto: `/` と別のファイルシステムなら割当とみなす。
    // 未設定 (Pterodactyl): さらに `/` より小さいときだけ割当とみなす。ボリュームが別ディスクに
    // あるだけのホストで、そのディスク丸ごとを割当と誤認して Wings に止められないための安全側の条件。
    let inferring = cfg.disk_quota == DiskQuota::Unknown && cfg.pterodactyl;
    if cfg.disk_quota != DiskQuota::Auto && !inferring {
        return cfg.disk_quota;
    }
    let label = if inferring {
        "disk allocation not configured"
    } else {
        "SERVER_DISK=auto"
    };
    let Some(f) = df else {
        log_warn!(
            None,
            "{} and {} cannot be measured; treating the allocation as unknown",
            label,
            root.display()
        );
        return DiskQuota::Unknown;
    };
    let rootfs = sysinfo::fs_info(Path::new("/"));
    if rootfs.is_some_and(|r| r.total == f.total) {
        log_warn!(
            None,
            "{}: df {} reports the same filesystem as / ({} MiB), i.e. the host disk rather than the allocation; treating the allocation as unknown",
            label,
            root.display(),
            f.total / MIB
        );
        return DiskQuota::Unknown;
    }
    if inferring && rootfs.is_some_and(|r| f.total >= r.total) {
        log_warn!(
            None,
            "{}: df {} ({} MiB) is not smaller than / ({} MiB), so it is probably a whole data disk rather than the allocation; treating the allocation as unknown (set SERVER_DISK to override)",
            label,
            root.display(),
            f.total / MIB,
            rootfs.map_or(0, |r| r.total / MIB)
        );
        return DiskQuota::Unknown;
    }
    log_info!(
        None,
        "disk allocation {} df {}: {} MiB",
        if inferring {
            "inferred from"
        } else {
            "taken from"
        },
        root.display(),
        f.total / MIB
    );
    DiskQuota::Auto
}
