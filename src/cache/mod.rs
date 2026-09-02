//! 2 段キャッシュ (メモリ + ディスク) の実装。
//!
//! - L1: メモリ LRU キャッシュ ([`memory`])
//! - L2: ディスク LRU キャッシュ ([`disk`])。本文はストリーミングで読み書きし、再起動後も
//!   インデックスを復元する
//!
//! 上限は固定値 (MiB) か、動的な安全マージン ([`margin`]) だけ残して限界まで確保する
//! `auto` モード ([`config::Limit`]) を選べる。`auto` ではバックグラウンドのプローブ
//! ([`probe`]) が使用量を測り直して予算 ([`budget`]) を更新し、他プロセスが資源を
//! 必要とすれば縮退 (LRU 追い出し) する。`reserve` が有効なら予算の未使用分を
//! バラストとして先に確保しておき、キャッシュが育つにつれて置き換える。
//!
//! HTTP レスポンスはワイヤ上のバイト列そのまま (ステータス行 + ヘッダー + ボディ) 保存し、
//! ヒット時はそれをそのままクライアントへ再生する。期限切れでもバリデータを持つ
//! エントリは残し、呼び出し側が再検証 (304) して延命する。

pub mod admission;
pub mod budget;
pub mod config;
pub mod disk;
pub mod diskprobe;
pub mod entry;
pub mod format;
pub mod inflight;
pub mod key;
pub mod lru;
pub mod margin;
pub mod memory;
mod ops;
pub mod probe;
mod quota;
pub mod sink;
pub mod status;
#[cfg(test)]
mod tests;

pub use config::{CacheConfig, DiskQuota, Limit, MIB};
pub use entry::{Body, CachedResponse};
pub use format::Meta;
pub use inflight::{FetchOutcome, FetchTicket};
pub use key::{CacheKey, cache_key, cache_key_variant};
pub use ops::PeekInfo;
pub use sink::{StoreOutcome, StoreSink};

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use budget::Snapshot;
use disk::DiskTier;
use diskprobe::DiskProbe;
use margin::Margin;
use memory::MemTier;
use quota::resolve_quota;

use crate::sysinfo;
use crate::{log_info, log_warn};

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheSource {
    Memory,
    Disk,
}

impl CacheSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheSource::Memory => "memory",
            CacheSource::Disk => "disk",
        }
    }
}

/// 動的マージンのコントローラ (層ごと)。
pub(crate) struct Margins {
    pub host: Margin,
    pub cgroup: Margin,
    pub disk: Margin,
}

pub struct Cache {
    cfg: CacheConfig,
    /// 起動時の確認を経た、実際に使う割当の扱い (`Auto` はホストディスクだと分かれば `Unknown` に落とす)
    quota: DiskQuota,
    mem: MemTier,
    disk: DiskTier,
    /// LRU の採番用カウンタ (両層で共有)
    clock: AtomicU64,
    ticks: AtomicU64,
    snapshot: Mutex<Snapshot>,
    margins: Mutex<Margins>,
    /// 割当が分からないときの探索 (Pterodactyl のみ)
    disk_probe: Mutex<Option<DiskProbe>>,
    /// URL (バリアント無しのキー) → 保存したバリアントのキー。無効化 (POST 等) 用
    variants: Mutex<key::KeyMap<Vec<CacheKey>>>,
    /// 裏で再検証中のキー (同じキーは 1 本だけ)
    revalidating: Mutex<HashSet<CacheKey>>,
    /// 進行中の取得 (同時ミスの合流用)
    inflight: inflight::InFlightTable,
    /// quota モードでの、割当ディレクトリ内の自分以外の使用量
    other_disk_usage: AtomicU64,
    /// このティックまではバラストの再確保を控える (メモリ圧迫・ENOSPC の後)
    backoff_until: AtomicU64,
    /// 直近のプローブで ENOSPC を観測した (バラストの再確保を控える合図)
    disk_enospc_seen: AtomicBool,
    pressure_logged: AtomicBool,
    mem_fallback_warned: AtomicBool,
    disk_fallback_warned: AtomicBool,
    pub hits_mem: AtomicU64,
    pub hits_disk: AtomicU64,
    pub misses: AtomicU64,
    pub stores: AtomicU64,
    pub revalidations: AtomicU64,
    /// 裏で完了した再検証 (304 での延命と差し替えの両方)
    pub background_revalidations: AtomicU64,
    /// 期限切れの表現をそのまま配信した回数 (grace 内・オリジン障害・待ち切れ)
    pub stale_served: AtomicU64,
    /// 同時ミスの合流で、オリジンへ行かずに済んだ要求の数
    pub coalesced: AtomicU64,
    /// 入場制御で見送った保存の数 (初回の要求)
    pub admission_rejected: AtomicU64,
    doorkeeper: admission::Doorkeeper,
    pub evictions: AtomicU64,
    pub bytes_served: AtomicU64,
}

/// 同時に走らせる裏側の再検証の上限。
const MAX_BACKGROUND_REVALIDATIONS: usize = 32;

impl Cache {
    pub fn new(cfg: CacheConfig) -> Self {
        let quota = if cfg.enabled {
            resolve_quota(&cfg)
        } else {
            cfg.disk_quota
        };
        let cache = Self {
            quota,
            mem: MemTier::new(cfg.reserve),
            disk: DiskTier::new(cfg.dir.clone(), cfg.reserve, cfg.disk_max_entries),
            margins: Mutex::new(Margins {
                host: Margin::new(cfg.mem_keep_free),
                cgroup: Margin::new(cfg.mem_keep_free),
                disk: Margin::new(cfg.disk_keep_free),
            }),
            cfg,
            clock: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            snapshot: Mutex::new(Snapshot::default()),
            disk_probe: Mutex::new(None),
            variants: Mutex::new(key::KeyMap::default()),
            revalidating: Mutex::new(HashSet::new()),
            inflight: inflight::InFlightTable::default(),
            other_disk_usage: AtomicU64::new(0),
            backoff_until: AtomicU64::new(0),
            disk_enospc_seen: AtomicBool::new(false),
            pressure_logged: AtomicBool::new(false),
            mem_fallback_warned: AtomicBool::new(false),
            disk_fallback_warned: AtomicBool::new(false),
            hits_mem: AtomicU64::new(0),
            hits_disk: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stores: AtomicU64::new(0),
            revalidations: AtomicU64::new(0),
            background_revalidations: AtomicU64::new(0),
            stale_served: AtomicU64::new(0),
            coalesced: AtomicU64::new(0),
            admission_rejected: AtomicU64::new(0),
            doorkeeper: admission::Doorkeeper::default(),
            evictions: AtomicU64::new(0),
            bytes_served: AtomicU64::new(0),
        };
        if !cache.cfg.enabled {
            return cache;
        }

        match cache
            .disk
            .init(&cache.clock, now_epoch(), cache.cfg.max_stale.as_secs())
        {
            Ok(r) => log_info!(
                None,
                "disk cache ready at {} ({} entries restored, {} expired removed, {} migrated, {} stray files removed)",
                cache.cfg.dir.display(),
                r.restored,
                r.expired,
                r.migrated,
                r.removed
            ),
            Err(e) => log_warn!(
                None,
                "disk cache disabled for dir {}: {}",
                cache.cfg.dir.display(),
                e
            ),
        }
        if let Some(fstype) = sysinfo::fs_type(cache.disk.dir()).filter(|_| cache.disk.is_ready())
            && sysinfo::is_ram_backed(&fstype)
        {
            log_warn!(
                None,
                "cache dir {} is on {} (RAM-backed): disk reservation disabled; set PROXY_CACHE_DIR to a real disk",
                cache.cfg.dir.display(),
                fstype
            );
            cache.disk.disable_reserve();
        }
        if cache.cfg.pterodactyl && cache.cfg.disk_limit.is_auto() && !cache.quota.is_known() {
            if cache.cfg.disk_probe && cache.disk.reserve_active() {
                let state = cache.quota_root().join(".sorahost-http-proxy.state");
                *cache.disk_probe.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(DiskProbe::load(&state, now_epoch()));
            } else {
                log_warn!(
                    None,
                    "Pterodactyl detected but the disk allocation is unknown and probing is unavailable (needs reservation): disk cache capped at {} MiB; put SERVER_DISK=<allocation MB> (0 = unlimited, auto = trust df) in $HOME/.env to use more",
                    config::PTERODACTYL_UNKNOWN_QUOTA_DISK / MIB
                );
                cache.disk.disable_reserve();
            }
        }

        cache.refresh_other_disk_usage();
        cache.refresh_budget();
        let evicted = cache.mem.enforce() + cache.disk.enforce();
        cache.count_evictions(evicted);
        cache
    }

    /// バックグラウンドのプローブスレッドを起動する (`probe_interval` が 0 なら起動しない)。
    pub fn spawn_probe(cache: &Arc<Cache>) -> Option<JoinHandle<()>> {
        probe::spawn(cache)
    }

    pub fn config(&self) -> &CacheConfig {
        &self.cfg
    }

    /// 実際に使っているディスク割当の扱い。
    pub fn disk_quota(&self) -> DiskQuota {
        self.quota
    }

    /// 割当ディレクトリ (無ければキャッシュディレクトリ)。
    pub fn quota_root(&self) -> &Path {
        self.cfg.quota_root.as_deref().unwrap_or(&self.cfg.dir)
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn mem_capacity(&self) -> u64 {
        self.mem.capacity()
    }

    pub fn disk_capacity(&self) -> u64 {
        self.disk.capacity()
    }

    /// (エントリ合計バイト, 件数)
    pub fn mem_usage(&self) -> (u64, usize) {
        self.mem.usage()
    }

    pub fn disk_usage(&self) -> (u64, usize) {
        self.disk.usage()
    }

    /// 先行確保しているバラストのバイト数。
    pub fn mem_reserved(&self) -> u64 {
        self.mem.ballast_bytes()
    }

    pub fn disk_reserved(&self) -> u64 {
        self.disk.ballast_bytes()
    }

    /// ディスク上のパス (インデックスに無ければ `None`)。
    pub fn disk_path(&self, key: CacheKey) -> Option<PathBuf> {
        self.disk
            .lookup(key)
            .map(|m| self.disk.path_for(key, m.validators))
    }

    /// バラストファイルのパス (シグナル時の切り詰め用)。
    pub fn ballast_path(&self) -> PathBuf {
        self.disk.ballast_path()
    }

    /// このキーの応答を保存してよいか (入場制御)。層に余裕があるうちは何でも保存し、
    /// 最後の層が 90% 埋まったら「2 回目以降に見たキー」だけ通す。
    pub fn admit(&self, key: CacheKey) -> bool {
        if !self.cfg.admission {
            return true;
        }
        let seen = self.doorkeeper.seen(key);
        if seen || !self.under_pressure() {
            return true;
        }
        self.admission_rejected.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// 最後の層 (ディスクがあればディスク、無ければメモリ) が 90% 以上埋まっているか。
    fn under_pressure(&self) -> bool {
        let (used, cap) = if self.disk.capacity() > 0 {
            (self.disk.usage().0, self.disk.capacity())
        } else {
            (self.mem.usage().0, self.mem.capacity())
        };
        cap > 0 && used.saturating_mul(10) >= cap.saturating_mul(9)
    }

    /// 同じキーの取得が進行中かを見て、leader になるか待つ側になるかを決める。
    pub fn begin_fetch(&self, key: CacheKey) -> FetchTicket<'_> {
        self.inflight.begin(key)
    }

    /// 進行中の取得の数。
    pub fn inflight_count(&self) -> usize {
        self.inflight.len()
    }

    /// 裏側の再検証を始めてよいか (同じキーが進行中、または上限なら false)。
    pub fn begin_revalidation(&self, key: CacheKey) -> bool {
        let mut set = self.revalidating.lock().unwrap_or_else(|p| p.into_inner());
        if set.len() >= MAX_BACKGROUND_REVALIDATIONS || set.contains(&key) {
            return false;
        }
        set.insert(key);
        true
    }

    pub fn end_revalidation(&self, key: CacheKey) {
        let mut set = self.revalidating.lock().unwrap_or_else(|p| p.into_inner());
        set.remove(&key);
    }

    /// 進行中の裏側の再検証の数。
    pub fn revalidating_count(&self) -> usize {
        self.revalidating
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// 直近のプローブで観測したシステム使用量。
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }

    fn count_evictions(&self, n: usize) {
        if n > 0 {
            self.evictions.fetch_add(n as u64, Ordering::Relaxed);
        }
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        if self.cfg.enabled {
            self.disk.shutdown();
        }
    }
}
