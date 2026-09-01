//! バックグラウンドのプローブ。
//!
//! 一定間隔でシステム使用量を測り直して予算を更新し、超過分の追い出し、
//! 期限切れの掃除、バラストの伸長を行う。スレッドは `Weak<Cache>` しか持たないので、
//! `Cache` が破棄されれば自然に終了する。

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};

use super::budget::{self, Probe};
use super::config::{FALLBACK_DISK, FALLBACK_MEM, Limit, MIB};
use super::{Cache, now_epoch};
use crate::sysinfo;
use crate::{log_debug, log_info, log_warn};

/// 期限切れ掃除と quota モードの他者使用量の再計測を行う間隔 (秒)。
const SWEEP_INTERVAL_SECS: u64 = 30;
/// メモリ圧迫を検知したあと、バラストの再確保を控える時間 (秒)。
const PRESSURE_BACKOFF_SECS: u64 = 60;
/// これ以上まとめて確保したときは info ログに出す。
const ANNOUNCE_RESERVE_BYTES: u64 = 256 * MIB;

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
        let sweep_every = self.ticks_per(SWEEP_INTERVAL_SECS);
        let sweep_now = tick.is_multiple_of(sweep_every);
        if sweep_now {
            self.refresh_other_disk_usage();
        }

        let pressure = self.refresh_budget();
        let evicted = self.mem.enforce() + self.disk.enforce();
        self.count_evictions(evicted);

        if sweep_now {
            let now = now_epoch();
            let n = self.mem.sweep_expired(now) + self.disk.sweep_expired(now);
            if n > 0 {
                log_debug!(None, "cache sweep: removed {} expired entries", n);
            }
        }

        if pressure {
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
        let (Some(_), Some(root)) = (self.cfg.disk_quota, &self.cfg.quota_root) else {
            return;
        };
        let others = sysinfo::dir_size_excluding(root, self.disk.dir());
        self.other_disk_usage.store(others, Ordering::Relaxed);
    }

    /// ホストのファイルシステム全体を分母にしてよいか。Pterodactyl では割当 (quota) が
    /// 別に強制されるので、quota 未設定のときはホスト全体を見ずに固定値へ倒す。
    fn host_fs_allowed(&self) -> bool {
        !(self.cfg.pterodactyl && self.cfg.disk_quota.is_none())
    }

    /// システム使用量を測り直し、両層の上限を更新する。戻り値はメモリ圧迫の有無。
    pub fn refresh_budget(&self) -> bool {
        let want_mem = self.cfg.mem_limit.is_auto();
        let quota = self
            .cfg
            .disk_quota
            .map(|q| (q, self.other_disk_usage.load(Ordering::Relaxed)));
        let want_fs = self.cfg.disk_limit.is_auto()
            && self.disk.is_ready()
            && (quota.is_some() || self.host_fs_allowed());
        let snap = budget::take(&Probe {
            dir: self.disk.dir(),
            want_mem,
            want_fs,
            mem_alloc: self.cfg.mem_alloc,
            quota,
            owned_disk: self.disk.owned(),
        });

        let mem_cap = match self.cfg.mem_limit {
            Limit::Fixed(b) => b,
            Limit::Auto { percent } => match &snap.mem {
                Some(m) => budget::mem_budget(self.mem.owned(), m, percent),
                None => {
                    if !self.mem_fallback_warned.swap(true, Ordering::Relaxed) {
                        log_warn!(
                            None,
                            "memory usage is not measurable here; memory cache fixed at {} MiB",
                            FALLBACK_MEM / MIB
                        );
                    }
                    FALLBACK_MEM
                }
            },
        };
        let disk_cap = if !self.disk.is_ready() {
            0
        } else {
            match self.cfg.disk_limit {
                Limit::Fixed(b) => b,
                Limit::Auto { percent } => match &snap.fs {
                    Some(f) => budget::disk_budget(self.disk.owned(), f, percent),
                    None => {
                        if self.host_fs_allowed()
                            && !self.disk_fallback_warned.swap(true, Ordering::Relaxed)
                        {
                            log_warn!(
                                None,
                                "filesystem usage is not measurable here; disk cache fixed at {} MiB",
                                FALLBACK_DISK / MIB
                            );
                        }
                        FALLBACK_DISK
                    }
                },
            }
        };

        self.announce(
            "memory",
            self.mem.capacity(),
            mem_cap,
            snap.mem
                .as_ref()
                .map(|m| ("system memory", m.used_percent())),
        );
        self.announce(
            "disk",
            self.disk.capacity(),
            disk_cap,
            snap.fs.map(|f| {
                (
                    if quota.is_some() {
                        "disk quota"
                    } else {
                        "filesystem"
                    },
                    f.used_percent(),
                )
            }),
        );
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
