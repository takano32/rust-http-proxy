//! 自動モードの予算計算とシステム使用量のスナップショット。
//!
//! ```text
//! 予算 = 自分が保持しているバイト数 + (目標使用量 − 現在の使用量)
//! ```
//!
//! 「現在の使用量」には自分の保持分 (エントリ + バラスト) も含まれるので、
//! 他プロセスの増減がそのまま予算の伸縮になる。cgroup 制限がある場合は
//! その階層ごとに同じ計算をして最小を取る。PSI でメモリ圧迫が観測されたら
//! さらに全体の 5% を返して「固まらない」側に倒す。

use std::path::Path;

use crate::sysinfo::{self, CgroupMem, FsInfo, MemPressure};

/// この値以上の PSI avg10 (%) を圧迫とみなす。
pub const PRESSURE_SOME_AVG10: f64 = 20.0;
pub const PRESSURE_FULL_AVG10: f64 = 5.0;
/// 圧迫時に追加で返す割合 (全体に対する %)。
const PRESSURE_BACKOFF_PERCENT: u8 = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct MemSnapshot {
    pub total: u64,
    pub available: u64,
    pub cgroups: Vec<CgroupMem>,
    pub pressure: Option<MemPressure>,
}

impl MemSnapshot {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }

    pub fn used_percent(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used() as f64 * 100.0 / self.total as f64
        }
    }

    /// ホスト全体か所属 cgroup のどこかで PSI が閾値を超えているか。
    pub fn under_pressure(&self) -> bool {
        let hot = |p: &MemPressure| {
            p.some_avg10 >= PRESSURE_SOME_AVG10 || p.full_avg10 >= PRESSURE_FULL_AVG10
        };
        self.pressure.as_ref().is_some_and(hot)
            || self
                .cgroups
                .iter()
                .any(|c| c.pressure.as_ref().is_some_and(hot))
    }

    /// 最も強い圧迫値 (ログ用)。
    pub fn max_pressure(&self) -> Option<MemPressure> {
        std::iter::once(self.pressure)
            .chain(self.cgroups.iter().map(|c| c.pressure))
            .flatten()
            .max_by(|a, b| a.some_avg10.total_cmp(&b.some_avg10))
    }

    /// 最も厳しい cgroup 制限 (あれば)。
    pub fn cgroup_limit(&self) -> Option<u64> {
        self.cgroups.iter().map(|c| c.limit).min()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub taken_at: u64,
    pub mem: Option<MemSnapshot>,
    pub fs: Option<FsInfo>,
    pub rss: Option<u64>,
}

/// 測定に必要な入力。
pub struct Probe<'a> {
    pub dir: &'a Path,
    pub want_mem: bool,
    pub want_fs: bool,
    /// コンテナのメモリ割当 (`SERVER_MEMORY`)。cgroup と同じ扱いで上限に加える。
    pub mem_alloc: Option<u64>,
    /// (ディスク割当, 割当ディレクトリ内の自分以外の使用量)。指定時は statvfs を使わない。
    pub quota: Option<(u64, u64)>,
    /// 自分が現在ディスクに保持しているバイト数 (エントリ + バラスト)。
    pub owned_disk: u64,
}

/// 現在のシステム使用量を測る。不要な層の情報は読まない。
pub fn take(p: &Probe<'_>) -> Snapshot {
    let rss = sysinfo::process_rss();
    let mem = if p.want_mem {
        sysinfo::mem_info().map(|m| {
            let mut cgroups = sysinfo::cgroup_mem_limits();
            if let Some(limit) = p.mem_alloc {
                // cgroup が読めるならその使用量、読めなければ自プロセスの RSS を使う
                let usage = cgroups.first().map(|c| c.usage).or(rss).unwrap_or(0);
                cgroups.push(CgroupMem {
                    limit,
                    usage,
                    pressure: None,
                });
            }
            MemSnapshot {
                total: m.total,
                available: m.available,
                cgroups,
                pressure: sysinfo::mem_pressure(),
            }
        })
    } else {
        None
    };
    let fs = if !p.want_fs {
        None
    } else if let Some((quota, others)) = p.quota {
        let used = others.saturating_add(p.owned_disk);
        Some(FsInfo {
            total: quota,
            used,
            available: quota.saturating_sub(used),
        })
    } else {
        sysinfo::fs_info(p.dir)
    };
    Snapshot {
        taken_at: super::now_epoch(),
        mem,
        fs,
        rss,
    }
}

pub fn percent_of(total: u64, percent: u8) -> u64 {
    (total as u128 * percent as u128 / 100) as u64
}

fn apply(owned: u64, headroom: i128) -> u64 {
    (owned as i128 + headroom).clamp(0, u64::MAX as i128) as u64
}

/// メモリ層の予算 (バイト)。`owned` は現在保持しているエントリ + バラストの合計。
pub fn mem_budget(owned: u64, m: &MemSnapshot, percent: u8) -> u64 {
    let mut headroom = percent_of(m.total, percent) as i128 - m.used() as i128;
    for cg in &m.cgroups {
        let h = percent_of(cg.limit, percent) as i128 - cg.usage as i128;
        headroom = headroom.min(h);
    }
    if m.under_pressure() {
        headroom -= percent_of(m.total, PRESSURE_BACKOFF_PERCENT) as i128;
    }
    apply(owned, headroom)
}

/// ディスク層の予算 (バイト)。`owned` は現在のエントリ + バラストの合計。
pub fn disk_budget(owned: u64, f: &FsInfo, percent: u8) -> u64 {
    apply(owned, percent_of(f.total, percent) as i128 - f.used as i128)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn snap(total: u64, available: u64) -> MemSnapshot {
        MemSnapshot {
            total,
            available,
            cgroups: Vec::new(),
            pressure: None,
        }
    }

    #[test]
    fn budget_grows_into_headroom_and_shrinks_under_others() {
        // 10 GiB 中 4 GiB 使用 (うち 1 GiB は自分) → 目標 9 GiB まで残り 5 GiB + 自分の 1 GiB
        let b = mem_budget(GIB, &snap(10 * GIB, 6 * GIB), 90);
        assert_eq!(b, 6 * GIB);
        // 他プロセスが増えて使用 9.5 GiB → 予算は自分の保持分より小さくなる (縮退)
        let b = mem_budget(GIB, &snap(10 * GIB, GIB / 2), 90);
        assert_eq!(b, GIB / 2);
        // さらに悪化しても 0 で止まる
        assert_eq!(mem_budget(GIB, &snap(10 * GIB, 0), 90), 0);
        assert_eq!(percent_of(u64::MAX, 100), u64::MAX);
    }

    #[test]
    fn cgroup_limit_caps_budget() {
        let mut m = snap(64 * GIB, 60 * GIB);
        m.cgroups.push(CgroupMem {
            limit: 2 * GIB,
            usage: GIB,
            pressure: None,
        });
        // ホストは余裕でも cgroup 2 GiB の 90% − 1 GiB = 0.8 GiB しか伸びない
        let b = mem_budget(0, &m, 90);
        assert_eq!(b, percent_of(2 * GIB, 90) - GIB);
        assert_eq!(m.cgroup_limit(), Some(2 * GIB));
    }

    #[test]
    fn pressure_backs_off_five_percent() {
        let mut m = snap(10 * GIB, 5 * GIB);
        let calm = mem_budget(0, &m, 90);
        m.pressure = Some(MemPressure {
            some_avg10: 30.0,
            full_avg10: 0.0,
        });
        assert!(m.under_pressure());
        assert_eq!(mem_budget(0, &m, 90), calm - GIB / 2);
        assert!((m.used_percent() - 50.0).abs() < 1e-9);

        // cgroup 側の圧迫だけでも検知する
        let mut c = snap(10 * GIB, 5 * GIB);
        c.cgroups.push(CgroupMem {
            limit: GIB,
            usage: 0,
            pressure: Some(MemPressure {
                some_avg10: 0.0,
                full_avg10: 9.0,
            }),
        });
        assert!(c.under_pressure());
        assert_eq!(c.max_pressure().unwrap().full_avg10, 9.0);
    }

    #[test]
    fn quota_mode_synthesizes_fs_info() {
        let dir = std::env::temp_dir();
        let snap = take(&Probe {
            dir: &dir,
            want_mem: false,
            want_fs: true,
            mem_alloc: None,
            quota: Some((100 * GIB, 30 * GIB)),
            owned_disk: 10 * GIB,
        });
        let f = snap.fs.unwrap();
        assert_eq!(f.total, 100 * GIB);
        assert_eq!(f.used, 40 * GIB);
        assert_eq!(f.available, 60 * GIB);
        assert!(snap.mem.is_none());
        // 予算 = 90 GiB − 他者の 30 GiB = 60 GiB
        assert_eq!(disk_budget(10 * GIB, &f, 90), 60 * GIB);
    }

    #[test]
    fn disk_budget_follows_df_style_usage() {
        let f = FsInfo {
            total: 100 * GIB,
            used: 30 * GIB,
            available: 70 * GIB,
        };
        assert_eq!(disk_budget(10 * GIB, &f, 90), 70 * GIB);
        let full = FsInfo {
            total: 100 * GIB,
            used: 95 * GIB,
            available: 5 * GIB,
        };
        assert_eq!(disk_budget(10 * GIB, &full, 90), 5 * GIB);
    }
}
