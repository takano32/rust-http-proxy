//! 2 段キャッシュ (メモリ + ディスク) の実装。
//!
//! - L1: メモリ LRU キャッシュ ([`memory`])
//! - L2: ディスク LRU キャッシュ ([`disk`])。再起動後もインデックスを復元する
//!
//! 上限は固定値 (MiB) か、システムの使用率が目標 (既定 90%) に達するまで自動で確保する
//! `auto` モード ([`config::Limit`]) を選べる。`auto` ではバックグラウンドのプローブ
//! ([`probe`]) が使用量を測り直して予算 ([`budget`]) を更新し、他プロセスが資源を
//! 必要とすれば縮退 (LRU 追い出し) する。`reserve` が有効なら予算の未使用分を
//! バラストとして先に確保しておき、キャッシュが育つにつれて置き換える。
//!
//! HTTP レスポンスはワイヤ上のバイト列そのまま (ステータス行 + ヘッダー + ボディ) 保存し、
//! ヒット時はそれをそのままクライアントへ再生する。

pub mod budget;
pub mod config;
pub mod disk;
pub mod format;
pub mod key;
pub mod lru;
pub mod memory;
pub mod probe;
pub mod status;
#[cfg(test)]
mod tests;

pub use config::{CacheConfig, Limit, MIB};
pub use key::{CacheKey, cache_key};

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use budget::Snapshot;
use disk::DiskTier;
use memory::MemTier;

use crate::sysinfo;
use crate::{log_debug, log_info, log_warn};

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

#[derive(Debug)]
pub struct CachedResponse {
    pub bytes: Arc<Vec<u8>>,
    pub stored_at: u64,
    pub expires_at: u64,
}

impl CachedResponse {
    pub fn age(&self) -> u64 {
        now_epoch().saturating_sub(self.stored_at)
    }
}

pub struct Cache {
    cfg: CacheConfig,
    mem: MemTier,
    disk: DiskTier,
    /// LRU の採番用カウンタ (両層で共有)
    clock: AtomicU64,
    ticks: AtomicU64,
    snapshot: Mutex<Snapshot>,
    /// quota モードでの、割当ディレクトリ内の自分以外の使用量
    other_disk_usage: AtomicU64,
    /// このティックまではバラストの再確保を控える (メモリ圧迫後)
    backoff_until: AtomicU64,
    pressure_logged: AtomicBool,
    mem_fallback_warned: AtomicBool,
    disk_fallback_warned: AtomicBool,
    pub hits_mem: AtomicU64,
    pub hits_disk: AtomicU64,
    pub misses: AtomicU64,
    pub stores: AtomicU64,
    pub evictions: AtomicU64,
    pub bytes_served: AtomicU64,
}

impl Cache {
    pub fn new(cfg: CacheConfig) -> Self {
        let cache = Self {
            mem: MemTier::new(cfg.reserve),
            disk: DiskTier::new(cfg.dir.clone(), cfg.reserve),
            cfg,
            clock: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            snapshot: Mutex::new(Snapshot::default()),
            other_disk_usage: AtomicU64::new(0),
            backoff_until: AtomicU64::new(0),
            pressure_logged: AtomicBool::new(false),
            mem_fallback_warned: AtomicBool::new(false),
            disk_fallback_warned: AtomicBool::new(false),
            hits_mem: AtomicU64::new(0),
            hits_disk: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stores: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            bytes_served: AtomicU64::new(0),
        };
        if !cache.cfg.enabled {
            return cache;
        }

        match cache.disk.init(&cache.clock, now_epoch()) {
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
        if cache.cfg.pterodactyl && cache.cfg.disk_limit.is_auto() && cache.cfg.disk_quota.is_none()
        {
            log_warn!(
                None,
                "Pterodactyl detected but PROXY_DISK_QUOTA_MB is not set: disk cache limited to {} MiB; set it to the server's disk allocation (MB) to use up to {}% of it",
                config::FALLBACK_DISK / MIB,
                cache
                    .cfg
                    .disk_limit
                    .target_percent()
                    .unwrap_or(config::DEFAULT_TARGET_PERCENT)
            );
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

    pub fn disk_path(&self, key: CacheKey) -> PathBuf {
        self.disk.path_for(key)
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

    /// キャッシュを引く。メモリ → ディスクの順に探索し、ディスクヒットはメモリへ昇格させる。
    pub fn get(&self, key: CacheKey, conn_id: usize) -> Option<(CachedResponse, CacheSource)> {
        if !self.cfg.enabled {
            return None;
        }
        let now = now_epoch();

        if let Some(resp) = self.mem.get(key, now, self.tick()) {
            self.hits_mem.fetch_add(1, Ordering::Relaxed);
            self.bytes_served
                .fetch_add(resp.bytes.len() as u64, Ordering::Relaxed);
            log_debug!(
                Some(conn_id),
                "cache L1 HIT key={} size={}B age={}s",
                key,
                resp.bytes.len(),
                resp.age()
            );
            return Some((resp, CacheSource::Memory));
        }

        if let Some(resp) = self.get_from_disk(key, now, conn_id) {
            return Some((resp, CacheSource::Disk));
        }

        self.misses.fetch_add(1, Ordering::Relaxed);
        log_debug!(Some(conn_id), "cache MISS key={}", key);
        None
    }

    /// L2 を引き、ヒットしたら L1 へ昇格させる。壊れた・期限切れのエントリはここで消す。
    fn get_from_disk(&self, key: CacheKey, now: u64, conn_id: usize) -> Option<CachedResponse> {
        if !self.disk.is_ready() {
            return None;
        }
        let expires_at = self.disk.lookup_expiry(key)?;
        if expires_at <= now {
            self.disk.remove(key);
            return None;
        }
        let (meta, body) = match self.disk.read(key) {
            Ok(Some((meta, body))) if meta.expires_at > now => (meta, body),
            Ok(_) => {
                self.disk.remove(key);
                return None;
            }
            Err(e) => {
                log_warn!(Some(conn_id), "cache L2 read failed key={}: {}", key, e);
                self.disk.remove(key);
                return None;
            }
        };
        let data = Arc::new(body);
        self.disk.touch(key, self.tick());
        self.hits_disk.fetch_add(1, Ordering::Relaxed);
        self.bytes_served
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        log_debug!(
            Some(conn_id),
            "cache L2 HIT key={} size={}B age={}s (promoting to L1)",
            key,
            data.len(),
            now.saturating_sub(meta.stored_at)
        );
        let evicted = self.mem.insert(
            key,
            Arc::clone(&data),
            meta.stored_at,
            meta.expires_at,
            self.tick(),
        );
        self.count_evictions(evicted);
        Some(CachedResponse {
            bytes: data,
            stored_at: meta.stored_at,
            expires_at: meta.expires_at,
        })
    }

    /// レスポンスをメモリとディスクの両方へ保存する。
    pub fn put(&self, key: CacheKey, url: &str, bytes: Vec<u8>, ttl: Duration, conn_id: usize) {
        if !self.cfg.enabled {
            return;
        }
        let size = bytes.len() as u64;
        if size > self.cfg.max_object_size {
            log_debug!(
                Some(conn_id),
                "cache SKIP (object {}B exceeds max {}B) url={}",
                size,
                self.cfg.max_object_size,
                url
            );
            return;
        }

        let stored_at = now_epoch();
        let expires_at = stored_at.saturating_add(ttl.as_secs());
        let data = Arc::new(bytes);

        let evicted = self
            .mem
            .insert(key, Arc::clone(&data), stored_at, expires_at, self.tick());
        self.count_evictions(evicted);
        if self.disk.is_ready() {
            match self
                .disk
                .write(key, url, &data, stored_at, expires_at, self.tick())
            {
                Ok(out) => self.count_evictions(out.evicted),
                Err(e) => log_warn!(Some(conn_id), "cache L2 write failed key={}: {}", key, e),
            }
        }
        self.stores.fetch_add(1, Ordering::Relaxed);

        log_debug!(
            Some(conn_id),
            "cache STORE key={} size={}B ttl={}s url={}",
            key,
            size,
            ttl.as_secs(),
            url
        );
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        if self.cfg.enabled {
            self.disk.shutdown();
        }
    }
}
