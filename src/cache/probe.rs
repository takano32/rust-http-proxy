//! バックグラウンドのプローブ。
//!
//! 一定間隔でシステム使用量を測り直し、動的マージン ([`super::margin`]) を更新して予算を
//! 決め、超過分の追い出し・期限切れの掃除・バラストの伸長を行う。スレッドは
//! `Weak<Cache>` しか持たないので、`Cache` が破棄されれば自然に終了する。

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};

use super::budget::{self, Probe, percent_of};
use super::config::{
    DiskQuota, FALLBACK_DISK, FALLBACK_MEM, Limit, MIB, PTERODACTYL_UNKNOWN_QUOTA_DISK,
};
use super::{Cache, now_epoch};
use crate::sysinfo;
use crate::{log_debug, log_info, log_warn};

/// 期限切れ掃除の間隔 (秒)。
const SWEEP_INTERVAL_SECS: u64 = 30;
/// quota モードで割当ディレクトリ内の他者使用量を測り直す間隔 (秒)。
const OTHERS_INTERVAL_SECS: u64 = 60;
/// 圧迫を検知したあと、バラストの再確保を控える時間 (秒)。
const PRESSURE_BACKOFF_SECS: u64 = 60;
/// これ以上まとめて確保したときは info ログに出す。
const ANNOUNCE_RESERVE_BYTES: u64 = 256 * MIB;
/// カーネル床: `vm.min_free_kbytes` のこの倍を空けておく。
const MIN_FREE_MULTIPLIER: u64 = 4;
/// 床の絶対量 (ただし全体の 1/10 を超えない)。
const MEM_FLOOR: u64 = 64 * MIB;
const DISK_FLOOR: u64 = 256 * MIB;
/// 圧迫時の最初のバックオフ (全体に対する %)。
const BACKOFF_PERCENT: u8 = 5;

pub fn spawn(cache: &Arc<Cache>) -> Option<JoinHandle<()>> {
    let interval = cache.config().probe_interval;
    if !cache.enabled() || interval.is_zero() {
        return None;
    }
    let weak: Weak<Cache> = Arc::downgrade(cache);
    thread::Builder::new()
        .name("cache-probe".into())
        .spawn(move || {
            loop {
                match weak.upgrade() {
                    Some(c) => c.probe_tick(),
                    None => break,
                }
                thread::sleep(interval);
            }
        })
        .ok()
}

/// 「全体の 1%」と「絶対量 (ただし全体の 1/10 まで)」の大きい方。小さな資源で床が全部を食わないようにする。
fn floor_for(total: u64, absolute: u64) -> u64 {
    (total / 100).max(absolute.min(total / 10))
}

impl Cache {
    fn ticks_per(&self, secs: u64) -> u64 {
        let interval = self.cfg.probe_interval.as_secs().max(1);
        (secs / interval).max(1)
    }

    /// 1 回分のプローブ処理。`new()` の直後とバックグラウンドスレッドから呼ばれる。
    pub fn probe_tick(&self) {
        if !self.cfg.enabled {
            return;
        }
        let tick = self.ticks.fetch_add(1, Ordering::Relaxed) + 1;
        if tick.is_multiple_of(self.ticks_per(OTHERS_INTERVAL_SECS)) {
            self.refresh_other_disk_usage();
        }

        let pressure = self.refresh_budget();
        let evicted = self.mem.enforce() + self.disk.enforce();
        self.count_evictions(evicted);

        if tick.is_multiple_of(self.ticks_per(SWEEP_INTERVAL_SECS)) {
            let now = now_epoch();
            let max_stale = self.cfg.max_stale.as_secs();
            let n = self.mem.sweep(now, max_stale) + self.disk.sweep(now, max_stale);
            if n > 0 {
                log_debug!(None, "cache sweep: removed {} expired entries", n);
            }
        }

        if pressure || self.disk_enospc_seen.swap(false, Ordering::Relaxed) {
            self.backoff_until.store(
                tick + self.ticks_per(PRESSURE_BACKOFF_SECS),
                Ordering::Relaxed,
            );
        }
        if self.cfg.reserve && tick >= self.backoff_until.load(Ordering::Relaxed) {
            let m = self.mem.fill_ballast();
            let d = self.disk.fill_ballast();
            if m + d > 0 {
                let msg = format!(
                    "reserved +{} MiB memory / +{} MiB disk (ballast now {} MiB / {} MiB)",
                    m / MIB,
                    d / MIB,
                    self.mem.ballast_bytes() / MIB,
                    self.disk.ballast_bytes() / MIB
                );
                if m + d >= ANNOUNCE_RESERVE_BYTES {
                    log_info!(None, "{}", msg);
                } else {
                    log_debug!(None, "{}", msg);
                }
            }
        }
    }

    /// quota モードで、割当ディレクトリ内の自分以外の使用量を測り直す。
    pub(super) fn refresh_other_disk_usage(&self) {
        if !matches!(self.quota, DiskQuota::Fixed(_)) {
            return;
        }
        let others = sysinfo::dir_size_excluding(self.quota_root(), self.disk.dir());
        self.other_disk_usage.store(others, Ordering::Relaxed);
    }

    /// システム使用量を測り直し、マージンと両層の上限を更新する。戻り値はメモリ圧迫の有無。
    pub fn refresh_budget(&self) -> bool {
        let want_mem = self.cfg.mem_limit.is_auto();
        let quota = match self.quota {
            DiskQuota::Fixed(q) => Some((q, self.other_disk_usage.load(Ordering::Relaxed))),
            _ => None,
        };
        // Pterodactyl で割当が分からなければホスト FS は見ず、固定の小さな上限にする
        let unknown_quota =
            self.cfg.pterodactyl && self.cfg.disk_limit.is_auto() && !self.quota.is_known();
        let want_fs = self.cfg.disk_limit.is_auto() && self.disk.is_ready() && !unknown_quota;
        // `auto` は割当ディレクトリ自体の statvfs (df 相当) を分母にする
        let probe_dir = if self.quota == DiskQuota::Auto {
            self.quota_root()
        } else {
            self.disk.dir()
        };
        let mut snap = budget::take(&Probe {
            dir: probe_dir,
            want_mem,
            want_fs,
            mem_alloc: self.cfg.mem_alloc,
            quota,
            owned_disk: self.disk.owned(),
        });
        let enospc = self.disk.take_enospc();
        let mut margins = self.margins.lock().unwrap_or_else(|p| p.into_inner());

        let (mem_cap, mem_detail) = match self.cfg.mem_limit {
            Limit::Fixed(b) => (b, None),
            Limit::Auto { percent } => match &snap.mem {
                Some(m) => {
                    let owned = self.mem.owned();
                    // 最も厳しい cgroup (コンテナならこれが効く)
                    let cg = m.cgroups.iter().min_by_key(|c| c.limit).copied();
                    margins.host.observe(m.used().saturating_sub(owned));
                    if let Some(cg) = cg {
                        margins.cgroup.observe(cg.usage.saturating_sub(owned));
                    }
                    // ホストの PSI はホスト側のマージンにだけ、cgroup の PSI は cgroup 側にだけ効かせる
                    if m.host_pressure() {
                        margins
                            .host
                            .on_pressure(percent_of(m.total, BACKOFF_PERCENT), m.total);
                    } else {
                        margins.host.on_calm();
                    }
                    if let Some(cg) = cg {
                        if m.cgroup_pressure().unwrap_or_else(|| m.host_pressure()) {
                            margins
                                .cgroup
                                .on_pressure(percent_of(cg.limit, BACKOFF_PERCENT), cg.limit);
                        } else {
                            margins.cgroup.on_calm();
                        }
                    }
                    let host_floor = (m.min_free.saturating_mul(MIN_FREE_MULTIPLIER))
                        .max(floor_for(m.total, MEM_FLOOR));
                    let host_keep = margins.host.keep_free(host_floor, m.active_file);
                    let cg_keep = cg.map_or(0, |cg| {
                        margins.cgroup.keep_free(floor_for(cg.limit, MEM_FLOOR), 0)
                    });
                    snap.mem_keep_free = host_keep;
                    snap.cgroup_keep_free = cg_keep;
                    let detail = match cg {
                        Some(cg) => (
                            "container memory",
                            cg.usage as f64 * 100.0 / cg.limit.max(1) as f64,
                        ),
                        None => ("system memory", m.used_percent()),
                    };
                    (
                        budget::mem_budget(owned, m, percent, host_keep, cg_keep),
                        Some(detail),
                    )
                }
                None => {
                    if !self.mem_fallback_warned.swap(true, Ordering::Relaxed) {
                        log_warn!(
                            None,
                            "memory usage is not measurable here; memory cache fixed at {} MiB",
                            FALLBACK_MEM / MIB
                        );
                    }
                    (FALLBACK_MEM, None)
                }
            },
        };

        let (disk_cap, disk_detail) = if !self.disk.is_ready() {
            (0, None)
        } else if unknown_quota {
            (PTERODACTYL_UNKNOWN_QUOTA_DISK, None)
        } else {
            match self.cfg.disk_limit {
                Limit::Fixed(b) => (b, None),
                Limit::Auto { percent } => match &snap.fs {
                    Some(f) => {
                        let owned = self.disk.owned();
                        margins.disk.observe(f.used.saturating_sub(owned));
                        let floor = floor_for(f.total, DISK_FLOOR);
                        if enospc > 0 {
                            margins.disk.on_pressure(
                                percent_of(f.total, BACKOFF_PERCENT).max(floor),
                                f.total,
                            );
                            self.disk_enospc_seen.store(true, Ordering::Relaxed);
                            log_info!(
                                None,
                                "disk is full ({} ENOSPC on cache writes): backing off",
                                enospc
                            );
                        } else {
                            margins.disk.on_calm();
                        }
                        let keep = margins.disk.keep_free(floor, 0);
                        snap.disk_keep_free = keep;
                        let what = match self.quota {
                            DiskQuota::Fixed(_) | DiskQuota::Auto => "disk quota",
                            _ => "filesystem",
                        };
                        let mut cap = budget::disk_budget(owned, f, percent, keep);
                        // 割当で計算していても、実際のファイルシステムの空きは超えられない
                        if quota.is_some()
                            && let Some(real) = sysinfo::fs_info(self.disk.dir())
                        {
                            let real_keep =
                                margins.disk.keep_free(floor_for(real.total, DISK_FLOOR), 0);
                            cap = cap.min(budget::disk_budget(owned, &real, percent, real_keep));
                        }
                        (cap, Some((what, f.used_percent())))
                    }
                    None => {
                        if !self.disk_fallback_warned.swap(true, Ordering::Relaxed) {
                            log_warn!(
                                None,
                                "filesystem usage is not measurable here; disk cache fixed at {} MiB",
                                FALLBACK_DISK / MIB
                            );
                        }
                        (FALLBACK_DISK, None)
                    }
                },
            }
        };
        drop(margins);

        self.announce("memory", self.mem.capacity(), mem_cap, mem_detail);
        self.announce("disk", self.disk.capacity(), disk_cap, disk_detail);
        self.mem.set_capacity(mem_cap);
        self.disk.set_capacity(disk_cap);

        let pressure = snap.mem.as_ref().is_some_and(|m| m.under_pressure());
        if pressure {
            if !self.pressure_logged.swap(true, Ordering::Relaxed) {
                let p = snap.mem.as_ref().and_then(|m| m.max_pressure());
                log_info!(
                    None,
                    "memory pressure detected (PSI some={:.1}% full={:.1}%): releasing reservations",
                    p.map_or(0.0, |p| p.some_avg10),
                    p.map_or(0.0, |p| p.full_avg10)
                );
            }
        } else {
            self.pressure_logged.store(false, Ordering::Relaxed);
        }

        *self.snapshot.lock().unwrap_or_else(|p| p.into_inner()) = snap;
        pressure
    }

    fn announce(&self, tier: &str, old: u64, new: u64, usage: Option<(&str, f64)>) {
        if old == new {
            return;
        }
        let diff = old.abs_diff(new);
        let significant = old == 0 || (diff * 20 > old && diff >= 64 * MIB);
        let detail = usage
            .map(|(what, pct)| format!(" ({} {:.1}% used)", what, pct))
            .unwrap_or_default();
        let msg = format!(
            "{} cache budget: {} MiB -> {} MiB{}",
            tier,
            old / MIB,
            new / MIB,
            detail
        );
        if significant {
            log_info!(None, "{}", msg);
        } else {
            log_debug!(None, "{}", msg);
        }
    }
}
