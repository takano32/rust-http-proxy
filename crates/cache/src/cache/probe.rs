//! バックグラウンドのプローブ。
//!
//! 一定間隔でシステム使用量を測り直し、動的マージン ([`super::margin`]) を更新して予算を
//! 決め、超過分の追い出し・期限切れの掃除・バラストの伸長を行う。スレッドは
//! `Weak<Cache>` しか持たないので、`Cache` が破棄されれば自然に終了する。

use crate::sync::LockExt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};

use super::budget::{self, MemSnapshot, Probe, Snapshot, percent_of};
use super::config::{
    DiskQuota, FALLBACK_DISK, FALLBACK_MEM, Limit, MIB, PTERODACTYL_UNKNOWN_QUOTA_DISK, Reserve,
};
use super::margin::Margin;
use super::{Cache, Margins, now_epoch};
use crate::sysinfo::{self, FsInfo};
use crate::{log_debug, log_info, log_warn};

/// 期限切れ掃除の間隔 (秒)。
const SWEEP_INTERVAL_SECS: u64 = 30;
/// quota モードで割当ディレクトリ内の他者使用量を測り直す、短いほうの間隔 (秒)。
///
/// 測り直しは割当の下のファイルを全部 stat する (tokyo で 1 回およそ 10 ms、
/// 手元の 5,000 ファイルで 10.7 ms)。毎分たどると cache-probe の CPU の 7 割がこれになる
/// (T32.20) ので、ふだんは [`OTHERS_SLOW_SECS`] ごとにし、**割当に近い
/// ([`OTHERS_NEAR_PERCENT`] 以上) か ENOSPC の後だけ**この間隔に戻す。
const OTHERS_INTERVAL_SECS: u64 = 60;
/// 割当から遠いときの測り直しの間隔 (秒。T32.20)。
const OTHERS_SLOW_SECS: u64 = 600;
/// 「割当に近い」とみなす使用率 (%)。自分以外 + 前回から増えたファイルシステムの使用量 + 自分 (T32.20)。
const OTHERS_NEAR_PERCENT: u64 = 80;
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
/// 段階化した先行確保が「実使用量の何倍まで」抱えてよいか (T12.5)。
const STAGED_MULTIPLIER: u64 = 2;

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

/// 自分以外の使用量を測り直す時期か (T32.20)。`since` は前回から経ったティック、
/// `fast` / `slow` は短い / 長い間隔のティック数、`hurry` は ENOSPC の後、
/// `estimate` は悲観的に見積もった割当の使用量。
fn others_due_at(since: u64, fast: u64, slow: u64, hurry: bool, estimate: u64, quota: u64) -> bool {
    if since >= slow {
        return true;
    }
    if since < fast {
        return false;
    }
    hurry || estimate.saturating_mul(100) >= quota.saturating_mul(OTHERS_NEAR_PERCENT)
}

/// 「全体の 1%」と「絶対量 (ただし全体の 1/10 まで)」の大きい方。小さな資源で床が全部を食わないようにする。
fn floor_for(total: u64, absolute: u64) -> u64 {
    (total / 100).max(absolute.min(total / 10))
}

/// メモリ層の予算の決め ([`mem_budget_for`] の戻り値。T34.81)。
#[derive(Debug, Clone, Copy, PartialEq)]
struct MemBudget {
    /// 上限 (バイト)
    cap: u64,
    /// ホスト全体に対して空けておく量 (`/status` の `mem_keep_free`)
    keep_free: u64,
    /// 最も厳しい cgroup に対して空けておく量 (無ければ 0)
    cgroup_keep_free: u64,
    /// 起動ログに添える (何の使用率か, %)
    detail: (&'static str, f64),
}

/// `auto` のメモリ層の予算を、測った値とマージンから決める (I/O もログも無い。T34.81)。
///
/// マージンは観測で動くので `&mut` で受ける: ホストの PSI はホスト側のマージンにだけ、
/// cgroup の PSI は cgroup 側にだけ効かせる (cgroup に PSI が無ければホストの PSI を見る)。
fn mem_budget_for(m: &MemSnapshot, owned: u64, percent: u8, margins: &mut Margins) -> MemBudget {
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
    let host_floor =
        (m.min_free.saturating_mul(MIN_FREE_MULTIPLIER)).max(floor_for(m.total, MEM_FLOOR));
    let host_keep = margins.host.keep_free(host_floor, m.active_file);
    let cg_keep = cg.map_or(0, |cg| {
        margins.cgroup.keep_free(floor_for(cg.limit, MEM_FLOOR), 0)
    });
    let detail = match cg {
        Some(cg) => (
            "container memory",
            cg.usage as f64 * 100.0 / cg.limit.max(1) as f64,
        ),
        None => ("system memory", m.used_percent()),
    };
    MemBudget {
        cap: budget::mem_budget(owned, m, percent, host_keep, cg_keep),
        keep_free: host_keep,
        cgroup_keep_free: cg_keep,
        detail,
    }
}

/// ディスク層の予算の決め ([`disk_budget_for`] の戻り値。T34.81)。
#[derive(Debug, Clone, Copy, PartialEq)]
struct DiskBudget {
    /// 上限 (バイト)
    cap: u64,
    /// 空けておく量 (`/status` の `disk_keep_free`)
    keep_free: u64,
    /// 起動ログに添える「何の使用率か」
    what: &'static str,
}

/// `auto` のディスク層の予算を、測った値とマージンから決める (I/O もログも無い。T34.81)。
///
/// `f` は分母にするファイルシステム (割当があれば割当で計算した値)、`real` は割当で
/// 計算しているときの実際のファイルシステム (その空きは超えられない)。`enospc` は
/// 前回からキャッシュの書き込みが ENOSPC に当たったか。
fn disk_budget_for(
    f: &FsInfo,
    real: Option<&FsInfo>,
    owned: u64,
    percent: u8,
    enospc: bool,
    quota: DiskQuota,
    margin: &mut Margin,
) -> DiskBudget {
    margin.observe(f.used.saturating_sub(owned));
    let floor = floor_for(f.total, DISK_FLOOR);
    if enospc {
        margin.on_pressure(percent_of(f.total, BACKOFF_PERCENT).max(floor), f.total);
    } else {
        margin.on_calm();
    }
    let keep = margin.keep_free(floor, 0);
    let what = match quota {
        DiskQuota::Fixed(_) | DiskQuota::Auto => "disk quota",
        _ => "filesystem",
    };
    let mut cap = budget::disk_budget(owned, f, percent, keep);
    // 割当で計算していても、実際のファイルシステムの空きは超えられない
    if let Some(real) = real {
        let real_keep = margin.keep_free(floor_for(real.total, DISK_FLOOR), 0);
        cap = cap.min(budget::disk_budget(owned, real, percent, real_keep));
    }
    DiskBudget {
        cap,
        keep_free: keep,
        what,
    }
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
        if self.others_due(tick) {
            self.refresh_other_disk_usage();
        }

        let pressure = self.refresh_budget();
        // 段階化 (既定) では上限が実使用量から決まるので、使用量が減れば先にバラストを返す
        let mem_ceiling = self.reserve_ceiling(self.mem.usage().0, false);
        let disk_ceiling = self.reserve_ceiling(self.disk.usage().0, self.disk_probing());
        self.mem
            .release_ballast(self.mem.usage().0, self.mem.capacity().min(mem_ceiling));
        self.disk.release_ballast_to(disk_ceiling);
        // 予算で追い出したら理由を残す (T32.7。中身は `ops.rs`)
        let evicted = self.enforce_budget();
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
        if self.cfg.reserve.is_on() && tick >= self.backoff_until.load(Ordering::Relaxed) {
            let m = self.mem.fill_ballast(mem_ceiling);
            let d = self.disk.fill_ballast(disk_ceiling);
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

    /// 割当の探索中か (ディスクのバラストで Wings の上限を探っている最中は段階化しない)。
    fn disk_probing(&self) -> bool {
        self.disk_probe.locked().is_some()
    }

    /// この層が抱えてよい合計 (エントリ + バラスト) の上限。
    ///
    /// **使われるまで確保しない** (T12.5): 1 件も保存していない間は 0 で、保存が始まったら
    /// 実使用量の [`STAGED_MULTIPLIER`] 倍まで。これに予算 (`capacity`) が別途掛かるので、
    /// 「実際に使っているぶんと同じだけを先に押さえる」動きになる。`eager` は従来どおり
    /// 予算いっぱい、`off` は 0。`probing` は Pterodactyl の割当探索中
    /// (バラストを伸ばして上限を探る仕組みなので段階化しない)。
    fn reserve_ceiling(&self, used: u64, probing: bool) -> u64 {
        match self.cfg.reserve {
            Reserve::Off => 0,
            Reserve::Eager => u64::MAX,
            Reserve::Staged if probing => u64::MAX,
            Reserve::Staged => {
                if self.counters.stores.load(Ordering::Relaxed) == 0 {
                    0
                } else {
                    used.saturating_mul(STAGED_MULTIPLIER)
                }
            }
        }
    }

    /// このティックで自分以外の使用量を測り直すか (T32.20)。
    ///
    /// 割当から遠いうちは [`OTHERS_SLOW_SECS`] ごと。近づいたかは**悲観的に**見積もる:
    /// 前回測った自分以外の量に、前回からファイルシステム全体の使用量が**増えた**ぶんの和
    /// (プローブごとの増えだけを足し、減ったぶんは引かない = 同じファイルシステムの別の所で
    /// 消されても、割当の下で書かれたかもしれない量を見失わない) と自分の持ち分を足す。
    /// statvfs は予算の計算で毎ティック読んでいるので、ここで増えるシステムコールは無い
    fn others_due(&self, tick: u64) -> bool {
        let DiskQuota::Fixed(quota) = self.quota else {
            return false;
        };
        let since = tick.saturating_sub(self.others_walked_at.load(Ordering::Relaxed));
        let grown = self.fs_grown.load(Ordering::Relaxed);
        let estimate = self
            .other_disk_usage
            .load(Ordering::Relaxed)
            .saturating_add(grown)
            .saturating_add(self.disk.owned());
        others_due_at(
            since,
            self.ticks_per(OTHERS_INTERVAL_SECS),
            self.ticks_per(OTHERS_SLOW_SECS),
            tick < self.others_hurry_until.load(Ordering::Relaxed),
            estimate,
            quota,
        )
    }

    /// quota モードで、割当ディレクトリ内の自分以外の使用量を測り直す。
    pub(super) fn refresh_other_disk_usage(&self) {
        if !matches!(self.quota, DiskQuota::Fixed(_)) {
            return;
        }
        let others = sysinfo::dir_size_excluding(self.quota_root(), self.disk.dir());
        self.other_disk_usage.store(others, Ordering::Relaxed);
        // 次の見積もりの起点 (T32.20)。測れなければ 0 (= 増えたぶんを足さない)
        let fs_used = sysinfo::fs_info(self.disk.dir()).map_or(0, |f| f.used);
        self.fs_used.store(fs_used, Ordering::Relaxed);
        self.fs_grown.store(0, Ordering::Relaxed);
        self.others_walked_at
            .store(self.ticks.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    /// Pterodactyl の上で、ディスクの割当が分からないまま `auto` で動いているか。
    ///
    /// このときはホスト FS は見ず、割当の探索 ([`DiskProbe`](super::diskprobe::DiskProbe))
    /// か固定の小さな上限 ([`PTERODACTYL_UNKNOWN_QUOTA_DISK`]) にする。`new()` とプローブの
    /// 両方がこの 1 つを読む (T34.81)。
    pub(super) fn quota_unknown_on_pterodactyl(&self) -> bool {
        self.cfg.pterodactyl && self.cfg.disk_limit.is_auto() && !self.quota.is_known()
    }

    /// システム使用量を測り直し、マージンと両層の上限を更新する。戻り値はメモリ圧迫の有無。
    ///
    /// 測る ([`budget::take`]・ENOSPC の数) → 決める ([`Self::decide_mem`] /
    /// [`Self::decide_disk`]。式は純粋な [`mem_budget_for`] / [`disk_budget_for`]) →
    /// 書く (上限・ログ・スナップショット) の 3 段 (T34.81)。
    pub fn refresh_budget(&self) -> bool {
        // --- 測る ---
        let quota = match self.quota {
            DiskQuota::Fixed(q) => Some((q, self.other_disk_usage.load(Ordering::Relaxed))),
            _ => None,
        };
        // Pterodactyl で割当が分からなければホスト FS は見ず、固定の小さな上限にする
        let unknown_quota = self.quota_unknown_on_pterodactyl();
        let want_fs = self.cfg.disk_limit.is_auto() && self.disk.is_ready() && !unknown_quota;
        // `auto` は割当ディレクトリ自体の statvfs (df 相当) を分母にする
        let probe_dir = if self.quota == DiskQuota::Auto {
            self.quota_root()
        } else {
            self.disk.dir()
        };
        let mut snap = budget::take(&Probe {
            dir: probe_dir,
            want_mem: self.cfg.mem_limit.is_auto(),
            want_fs,
            mem_alloc: self.cfg.mem_alloc,
            quota,
            owned_disk: self.disk.owned(),
        });
        let enospc = self.disk.take_enospc();

        // --- 決める ---
        let mut margins = self.margins.locked();
        let (mem_cap, mem_detail) = self.decide_mem(&mut snap, &mut margins);
        let (disk_cap, entry_cap, disk_detail) = self.decide_disk(
            &mut snap,
            &mut margins,
            unknown_quota,
            quota.is_some(),
            enospc,
        );
        drop(margins);

        // --- 書く ---
        self.announce("memory", self.mem.capacity(), mem_cap, mem_detail);
        self.announce("disk", self.disk.capacity(), disk_cap, disk_detail);
        self.mem.set_capacity(mem_cap);
        self.disk.set_capacity(disk_cap);
        if let Some(e) = entry_cap {
            self.disk.set_entry_capacity(e);
        }
        let pressure = self.note_pressure(&snap);
        *self.snapshot.locked() = snap;
        pressure
    }

    /// メモリ層の上限を決める (`auto` で測れなければ固定の上限と 1 回だけの警告)。
    fn decide_mem(
        &self,
        snap: &mut Snapshot,
        margins: &mut Margins,
    ) -> (u64, Option<(&'static str, f64)>) {
        match self.cfg.mem_limit {
            Limit::Fixed(b) => (b, None),
            Limit::Auto { percent } => match &snap.mem {
                Some(m) => {
                    let b = mem_budget_for(m, self.mem.owned(), percent, margins);
                    snap.mem_keep_free = b.keep_free;
                    snap.cgroup_keep_free = b.cgroup_keep_free;
                    (b.cap, Some(b.detail))
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
        }
    }

    /// ディスク層の上限 (と、割当の探索中なら実データの上限) を決める。
    ///
    /// 割当の探索中は、実データの上限 (confirmed) と全体の上限 (cap) が分かれる。
    fn decide_disk(
        &self,
        snap: &mut Snapshot,
        margins: &mut Margins,
        unknown_quota: bool,
        has_quota: bool,
        enospc: u64,
    ) -> (u64, Option<u64>, Option<(&'static str, f64)>) {
        if !self.disk.is_ready() {
            return (0, None, None);
        }
        if unknown_quota {
            return self.decide_probed_disk(margins);
        }
        let percent = match self.cfg.disk_limit {
            Limit::Fixed(b) => return (b, None, None),
            Limit::Auto { percent } => percent,
        };
        let Some(f) = &snap.fs else {
            if !self.disk_fallback_warned.swap(true, Ordering::Relaxed) {
                log_warn!(
                    None,
                    "filesystem usage is not measurable here; disk cache fixed at {} MiB",
                    FALLBACK_DISK / MIB
                );
            }
            return (FALLBACK_DISK, None, None);
        };
        // 割当で計算していても、実際のファイルシステムの空きは超えられない
        let real = if has_quota {
            sysinfo::fs_info(self.disk.dir())
        } else {
            None
        };
        if let Some(real) = &real {
            // 自分以外の量を測り直すかの見積もりに使う (T32.20)
            let prev = self.fs_used.swap(real.used.max(1), Ordering::Relaxed);
            if prev != 0 && real.used > prev {
                self.fs_grown.fetch_add(real.used - prev, Ordering::Relaxed);
            }
        }
        let b = disk_budget_for(
            f,
            real.as_ref(),
            self.disk.owned(),
            percent,
            enospc > 0,
            self.quota,
            &mut margins.disk,
        );
        if enospc > 0 {
            self.disk_enospc_seen.store(true, Ordering::Relaxed);
            // 割当の下の自分以外の量は、しばらく短い間隔で測り直す (T32.20)
            self.others_hurry_until.store(
                self.ticks.load(Ordering::Relaxed) + self.ticks_per(OTHERS_SLOW_SECS),
                Ordering::Relaxed,
            );
            log_info!(
                None,
                "disk is full ({} ENOSPC on cache writes): backing off",
                enospc
            );
        }
        snap.disk_keep_free = b.keep_free;
        (b.cap, None, Some((b.what, f.used_percent())))
    }

    /// 割当が分からない Pterodactyl での上限 (探索中ならその結果、無ければ固定の小さな上限)。
    fn decide_probed_disk(
        &self,
        margins: &mut Margins,
    ) -> (u64, Option<u64>, Option<(&'static str, f64)>) {
        let mut probe = self.disk_probe.locked();
        let Some(pr) = probe.as_mut() else {
            return (PTERODACTYL_UNKNOWN_QUOTA_DISK, None, None);
        };
        let (confirmed, mut cap) = pr.tick(
            now_epoch(),
            self.disk.usage().0,
            self.disk.owned(),
            self.disk.take_fill_failed(),
        );
        // 実際のファイルシステムの空きは超えられない
        if let Some(real) = sysinfo::fs_info(self.disk.dir()) {
            let real_keep = margins.disk.keep_free(floor_for(real.total, DISK_FLOOR), 0);
            cap = cap.min(budget::disk_budget(
                self.disk.owned(),
                &real,
                100,
                real_keep,
            ));
        }
        (cap, Some(confirmed), Some(("probed allocation", 0.0)))
    }

    /// メモリ圧迫の有無を返し、圧迫に入ったときだけ 1 回ログに出す。
    fn note_pressure(&self, snap: &Snapshot) -> bool {
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
        pressure
    }

    fn announce(&self, tier: &str, old: u64, new: u64, usage: Option<(&str, f64)>) {
        if old == new {
            return;
        }
        let diff = old.abs_diff(new);
        let significant = old == 0 || (diff * 20 > old && diff >= 64 * MIB);
        let detail = usage
            .map(|(what, pct)| {
                if pct > 0.0 {
                    format!(" ({} {:.1}% used)", what, pct)
                } else {
                    format!(" ({})", what)
                }
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn others_are_walked_every_ten_minutes_unless_near_the_quota_or_after_enospc() {
        // 5 秒ごとのプローブ: 短い間隔 12 ティック、長い間隔 120 ティック (T32.20)
        let (fast, slow, q) = (12, 120, 1000);
        // 割当から遠い (50%) うちは 60 秒経っても測らず、600 秒で測る
        assert!(!others_due_at(12, fast, slow, false, 500, q));
        assert!(!others_due_at(119, fast, slow, false, 500, q));
        assert!(others_due_at(120, fast, slow, false, 500, q));
        // 80% 以上なら 60 秒ごと (それより短くはしない)
        assert!(!others_due_at(11, fast, slow, false, 800, q));
        assert!(others_due_at(12, fast, slow, false, 800, q));
        // ENOSPC の後も 60 秒ごと
        assert!(others_due_at(12, fast, slow, true, 0, q));
        assert!(!others_due_at(5, fast, slow, true, 0, q));
    }

    const GIB: u64 = 1 << 30;

    fn margins() -> Margins {
        Margins {
            host: Margin::with_interval(0, 5),
            cgroup: Margin::with_interval(0, 5),
            disk: Margin::with_interval(0, 5),
        }
    }

    fn mem(total: u64, available: u64) -> MemSnapshot {
        MemSnapshot {
            total,
            available,
            active_file: 0,
            min_free: 0,
            cgroups: Vec::new(),
            pressure: None,
        }
    }

    const HOT: sysinfo::MemPressure = sysinfo::MemPressure {
        some_avg10: 50.0,
        full_avg10: 10.0,
    };
    const CALM: sysinfo::MemPressure = sysinfo::MemPressure {
        some_avg10: 0.0,
        full_avg10: 0.0,
    };

    fn cgroup(
        limit: u64,
        usage: u64,
        pressure: Option<sysinfo::MemPressure>,
    ) -> sysinfo::CgroupMem {
        sysinfo::CgroupMem {
            limit,
            usage,
            pressure,
        }
    }

    /// 床は「全体の 1%」「絶対量」「全体の 1/10」の 3 形 (T34.81)。
    #[test]
    fn the_floor_takes_one_of_three_shapes() {
        // 大きな資源: 全体の 1% が絶対量より大きい
        assert_eq!(floor_for(1024 * GIB, 256 * MIB), 1024 * GIB / 100);
        // 中くらい: 絶対量
        assert_eq!(floor_for(10 * GIB, 256 * MIB), 256 * MIB);
        // 小さな資源: 絶対量は全体の 1/10 で頭打ち (床が全部を食わない)
        assert_eq!(floor_for(GIB, 256 * MIB), GIB / 10);
    }

    /// cgroup も PSI も無ければ、ホストの床だけで決まる (T34.81)。
    #[test]
    fn without_cgroup_or_psi_the_host_floor_decides() {
        let mut mg = margins();
        let m = mem(10 * GIB, 6 * GIB);
        let b = mem_budget_for(&m, GIB, 100, &mut mg);
        let floor = floor_for(10 * GIB, MEM_FLOOR);
        assert_eq!(b.keep_free, floor);
        assert_eq!(b.cgroup_keep_free, 0);
        assert_eq!(b.cap, budget::mem_budget(GIB, &m, 100, floor, 0));
        assert_eq!(b.detail, ("system memory", m.used_percent()));
        assert_eq!(mg.host.backoff(), 0);
        assert_eq!(mg.cgroup.backoff(), 0);
    }

    /// cgroup が無くホストの PSI が熱ければ、ホスト側だけが退く (T34.81)。
    #[test]
    fn host_psi_without_cgroup_backs_off_only_the_host() {
        let mut mg = margins();
        let mut m = mem(10 * GIB, 6 * GIB);
        m.pressure = Some(HOT);
        let b = mem_budget_for(&m, GIB, 100, &mut mg);
        assert_eq!(mg.host.backoff(), percent_of(10 * GIB, BACKOFF_PERCENT));
        assert_eq!(
            mg.cgroup.backoff(),
            0,
            "cgroup が無ければ cgroup 側は触らない"
        );
        assert_eq!(b.keep_free, percent_of(10 * GIB, BACKOFF_PERCENT));
    }

    /// cgroup があれば cgroup の床と使用率を出し、PSI が無ければどちらも退かない (T34.81)。
    #[test]
    fn a_cgroup_without_psi_adds_its_own_floor() {
        let mut mg = margins();
        let mut m = mem(64 * GIB, 60 * GIB);
        m.cgroups.push(cgroup(4 * GIB, GIB, None));
        // 緩い方の cgroup は見ない (最も厳しい 1 つだけ)
        m.cgroups.push(cgroup(8 * GIB, 7 * GIB, None));
        let b = mem_budget_for(&m, 0, 100, &mut mg);
        let cg_floor = floor_for(4 * GIB, MEM_FLOOR);
        assert_eq!(b.cgroup_keep_free, cg_floor);
        assert_eq!(b.detail, ("container memory", 25.0));
        assert_eq!(b.cap, budget::mem_budget(0, &m, 100, b.keep_free, cg_floor));
        assert_eq!(mg.host.backoff(), 0);
        assert_eq!(mg.cgroup.backoff(), 0);
    }

    /// cgroup に PSI が無ければホストの PSI を見るが、あれば cgroup の PSI だけを見る (T34.81)。
    #[test]
    fn a_cgroup_follows_its_own_psi_and_falls_back_to_the_host() {
        // cgroup に PSI が無い + ホストが熱い → 両方が退く
        let mut mg = margins();
        let mut m = mem(64 * GIB, 60 * GIB);
        m.pressure = Some(HOT);
        m.cgroups.push(cgroup(4 * GIB, GIB, None));
        mem_budget_for(&m, 0, 100, &mut mg);
        assert_eq!(mg.host.backoff(), percent_of(64 * GIB, BACKOFF_PERCENT));
        assert_eq!(mg.cgroup.backoff(), percent_of(4 * GIB, BACKOFF_PERCENT));

        // cgroup の PSI は穏やか + ホストが熱い → ホストだけが退く
        let mut mg = margins();
        m.cgroups[0].pressure = Some(CALM);
        mem_budget_for(&m, 0, 100, &mut mg);
        assert_eq!(mg.host.backoff(), percent_of(64 * GIB, BACKOFF_PERCENT));
        assert_eq!(mg.cgroup.backoff(), 0);

        // cgroup の PSI が熱い + ホストは穏やか → cgroup だけが退く
        let mut mg = margins();
        m.pressure = Some(CALM);
        m.cgroups[0].pressure = Some(HOT);
        let b = mem_budget_for(&m, 0, 100, &mut mg);
        assert_eq!(mg.host.backoff(), 0);
        assert_eq!(mg.cgroup.backoff(), percent_of(4 * GIB, BACKOFF_PERCENT));
        assert_eq!(b.cgroup_keep_free, percent_of(4 * GIB, BACKOFF_PERCENT));
    }

    fn fs(total: u64, used: u64) -> FsInfo {
        FsInfo {
            total,
            used,
            available: total - used,
        }
    }

    /// ENOSPC の後は全体の 5% (床より小さければ床) だけ退く (T34.81)。
    #[test]
    fn enospc_backs_the_disk_off_by_five_percent_or_the_floor() {
        let f = fs(100 * GIB, 50 * GIB);
        let mut m = Margin::with_interval(0, 5);
        let calm = disk_budget_for(&f, None, GIB, 100, false, DiskQuota::Unknown, &mut m);
        assert_eq!(calm.keep_free, floor_for(100 * GIB, DISK_FLOOR));
        assert_eq!(calm.what, "filesystem");
        assert_eq!(m.backoff(), 0);

        let mut m = Margin::with_interval(0, 5);
        let full = disk_budget_for(&f, None, GIB, 100, true, DiskQuota::Unlimited, &mut m);
        assert_eq!(m.backoff(), 5 * GIB);
        assert_eq!(full.keep_free, 5 * GIB);
        assert_eq!(full.cap, budget::disk_budget(GIB, &f, 100, 5 * GIB));

        // 小さなファイルシステムでは 5% より床 (256 MiB と 1/10 の小さい方) が大きい
        let small = fs(4 * GIB, GIB);
        let mut m = Margin::with_interval(0, 5);
        disk_budget_for(&small, None, 0, 100, true, DiskQuota::Unknown, &mut m);
        assert_eq!(m.backoff(), floor_for(4 * GIB, DISK_FLOOR));
    }

    /// 割当で計算していても、実際のファイルシステムの空きで切る (T34.81)。
    #[test]
    fn the_real_filesystem_caps_a_quota_budget() {
        let quota = fs(10 * GIB, 2 * GIB);
        let mut m = Margin::with_interval(0, 5);
        let alone = disk_budget_for(
            &quota,
            None,
            GIB,
            100,
            false,
            DiskQuota::Fixed(10 * GIB),
            &mut m,
        );
        assert_eq!(alone.what, "disk quota");
        // 実際のファイルシステムはほぼ満杯: 自分の持ち分より伸びない
        let real = fs(100 * GIB, 99 * GIB);
        let mut m = Margin::with_interval(0, 5);
        let capped = disk_budget_for(
            &quota,
            Some(&real),
            GIB,
            100,
            false,
            DiskQuota::Fixed(10 * GIB),
            &mut m,
        );
        assert!(capped.cap < alone.cap, "{} < {}", capped.cap, alone.cap);
        assert_eq!(
            capped.cap,
            budget::disk_budget(GIB, &real, 100, floor_for(100 * GIB, DISK_FLOOR))
        );
        // 実際の空きが十分なら、割当の側の値のまま
        let roomy = fs(100 * GIB, 10 * GIB);
        let mut m = Margin::with_interval(0, 5);
        let b = disk_budget_for(
            &quota,
            Some(&roomy),
            GIB,
            100,
            false,
            DiskQuota::Auto,
            &mut m,
        );
        assert_eq!(b.cap, alone.cap);
        assert_eq!(b.keep_free, alone.keep_free);
    }
}
