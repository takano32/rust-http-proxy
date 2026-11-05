//! ディスク割当の扱いの決定と、読み取り失敗の分類。

use std::io;
use std::path::Path;

use crate::config::{CacheConfig, DiskQuota, MIB};
use crate::sysinfo;
use crate::{log_info, log_warn};

/// 読めない原因がファイル側 (消えた・壊れた) にあるか。fd 枯渇や EIO のような一時的な失敗は含めない。
pub fn is_permanent(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
    )
}

/// `Auto` は `df <割当ディレクトリ>` が割当を示すときだけ採用する。`/` と同じファイルシステム
/// (total が一致) ならホストディスクが見えているだけなので `Unknown` に落とす。
/// Pterodactyl では判断材料として df 相当の数字を常にログに出す。
///
/// T34.67 で判断を純粋な [`decide`] に、ログを [`QuotaDecision::log`] に分けた (文面と順は前と同じ)。
pub fn resolve_quota(cfg: &CacheConfig) -> DiskQuota {
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
    // `/` の大きさは要るときだけ読む (前と同じく、割当の df が読めたあとに 1 回)
    let d = decide(cfg.disk_quota, cfg.pterodactyl, df, || {
        sysinfo::fs_info(Path::new("/"))
    });
    d.log(root);
    d.quota()
}

/// [`decide`] の答え。`Configured` 以外は、`auto` を採るか (`Taken`) 諦めるか (ほか) と、その理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuotaDecision {
    /// 設定した値をそのまま使う (`auto` でもなく、推し量りもしない)
    Configured(DiskQuota),
    /// 割当のディレクトリの df が読めない
    Unmeasured { inferring: bool },
    /// `/` と同じファイルシステム (ホストのディスクが見えているだけ)。値は df の total
    SameAsRoot { inferring: bool, total: u64 },
    /// 推し量りで、`/` より小さくない (データ用のディスク丸ごとらしい)。値は df と `/` の total
    NotSmaller { total: u64, root_total: u64 },
    /// df の total を割当として採る
    Taken { inferring: bool, total: u64 },
}

/// 割当の決め方 (純粋。ファイルシステムは呼ぶ側が読む)。
///
/// 明示の auto: `/` と別のファイルシステムなら割当とみなす。
/// 未設定 (Pterodactyl): さらに `/` より小さいときだけ割当とみなす。ボリュームが別ディスクに
/// あるだけのホストで、そのディスク丸ごとを割当と誤認して Wings に止められないための安全側の条件。
/// `rootfs` (`/` の df) は df が読めて、要るときだけ 1 回呼ぶ。
pub(crate) fn decide(
    configured: DiskQuota,
    pterodactyl: bool,
    df: Option<sysinfo::FsInfo>,
    rootfs: impl FnOnce() -> Option<sysinfo::FsInfo>,
) -> QuotaDecision {
    let inferring = configured == DiskQuota::Unknown && pterodactyl;
    if configured != DiskQuota::Auto && !inferring {
        return QuotaDecision::Configured(configured);
    }
    let Some(f) = df else {
        return QuotaDecision::Unmeasured { inferring };
    };
    let rootfs = rootfs();
    if rootfs.is_some_and(|r| r.total == f.total) {
        return QuotaDecision::SameAsRoot {
            inferring,
            total: f.total,
        };
    }
    if inferring && rootfs.is_some_and(|r| f.total >= r.total) {
        return QuotaDecision::NotSmaller {
            total: f.total,
            root_total: rootfs.map_or(0, |r| r.total),
        };
    }
    QuotaDecision::Taken {
        inferring,
        total: f.total,
    }
}

impl QuotaDecision {
    /// 使う割当。
    pub(crate) fn quota(self) -> DiskQuota {
        match self {
            QuotaDecision::Configured(q) => q,
            QuotaDecision::Taken { .. } => DiskQuota::Auto,
            _ => DiskQuota::Unknown,
        }
    }

    /// 決めた理由を 1 行出す (`Configured` は何も出さない)。`root` は割当のディレクトリ。
    pub(crate) fn log(self, root: &Path) {
        let label = |inferring: bool| {
            if inferring {
                "disk allocation not configured"
            } else {
                "SERVER_DISK=auto"
            }
        };
        match self {
            QuotaDecision::Configured(_) => {}
            QuotaDecision::Unmeasured { inferring } => log_warn!(
                None,
                "{} and {} cannot be measured; treating the allocation as unknown",
                label(inferring),
                root.display()
            ),
            QuotaDecision::SameAsRoot { inferring, total } => log_warn!(
                None,
                "{}: df {} reports the same filesystem as / ({} MiB), i.e. the host disk rather than the allocation; treating the allocation as unknown",
                label(inferring),
                root.display(),
                total / MIB
            ),
            QuotaDecision::NotSmaller { total, root_total } => log_warn!(
                None,
                "{}: df {} ({} MiB) is not smaller than / ({} MiB), so it is probably a whole data disk rather than the allocation; treating the allocation as unknown (set SERVER_DISK to override)",
                label(true),
                root.display(),
                total / MIB,
                root_total / MIB
            ),
            QuotaDecision::Taken { inferring, total } => log_info!(
                None,
                "disk allocation {} df {}: {} MiB",
                if inferring {
                    "inferred from"
                } else {
                    "taken from"
                },
                root.display(),
                total / MIB
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysinfo::FsInfo;

    fn fs(total_mib: u64) -> Option<FsInfo> {
        Some(FsInfo {
            total: total_mib * MIB,
            used: 0,
            available: total_mib * MIB,
        })
    }

    /// `/` を読んではいけない場面で呼ばれたら落とす。
    fn no_root() -> Option<FsInfo> {
        panic!("/ の df は読まない");
    }

    #[test]
    fn a_configured_quota_is_used_as_is_without_reading_anything() {
        for q in [DiskQuota::Fixed(5 * MIB), DiskQuota::Unlimited] {
            assert_eq!(
                decide(q, true, fs(10), no_root),
                QuotaDecision::Configured(q)
            );
        }
        // Pterodactyl でなければ未設定は未設定のまま
        let d = decide(DiskQuota::Unknown, false, fs(10), no_root);
        assert_eq!(d, QuotaDecision::Configured(DiskQuota::Unknown));
        assert_eq!(d.quota(), DiskQuota::Unknown);
    }

    #[test]
    fn an_unmeasurable_directory_gives_unknown() {
        let d = decide(DiskQuota::Auto, false, None, no_root);
        assert_eq!(d, QuotaDecision::Unmeasured { inferring: false });
        assert_eq!(d.quota(), DiskQuota::Unknown);
        let d = decide(DiskQuota::Unknown, true, None, no_root);
        assert_eq!(d, QuotaDecision::Unmeasured { inferring: true });
    }

    #[test]
    fn the_same_filesystem_as_root_is_the_host_disk() {
        let d = decide(DiskQuota::Auto, false, fs(100), || fs(100));
        assert_eq!(
            d,
            QuotaDecision::SameAsRoot {
                inferring: false,
                total: 100 * MIB
            }
        );
        assert_eq!(d.quota(), DiskQuota::Unknown);
    }

    #[test]
    fn inferring_refuses_a_disk_not_smaller_than_root() {
        let d = decide(DiskQuota::Unknown, true, fs(200), || fs(100));
        assert_eq!(
            d,
            QuotaDecision::NotSmaller {
                total: 200 * MIB,
                root_total: 100 * MIB
            }
        );
        assert_eq!(d.quota(), DiskQuota::Unknown);
    }

    #[test]
    fn explicit_auto_takes_a_bigger_separate_disk() {
        // 明示の auto は `/` より大きくても採る (推し量りのときだけ断る)
        let d = decide(DiskQuota::Auto, true, fs(200), || fs(100));
        assert_eq!(
            d,
            QuotaDecision::Taken {
                inferring: false,
                total: 200 * MIB
            }
        );
        assert_eq!(d.quota(), DiskQuota::Auto);
    }

    #[test]
    fn inferring_takes_a_smaller_separate_filesystem_or_an_unreadable_root() {
        let d = decide(DiskQuota::Unknown, true, fs(10), || fs(100));
        assert_eq!(
            d,
            QuotaDecision::Taken {
                inferring: true,
                total: 10 * MIB
            }
        );
        // `/` が読めなければ比べられないので、df の値を採る (前と同じ)
        let d = decide(DiskQuota::Unknown, true, fs(300), || None);
        assert_eq!(d.quota(), DiskQuota::Auto);
    }
}
