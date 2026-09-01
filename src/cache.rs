//! 2 段キャッシュ (メモリ + ディスク) の実装。
//!
//! - L1: メモリキャッシュ (既定 200MB, LRU)
//! - L2: ディスクキャッシュ (既定 2048MB = 2GB, LRU)
//!
//! HTTP レスポンスをワイヤ上のバイト列そのまま (ステータス行 + ヘッダー + ボディ)
//! 保存し、ヒット時はそれをそのままクライアントへ再生する。

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{log_debug, log_info, log_trace, log_warn};

const MAGIC: &str = "SHPC1";
const MIB: u64 = 1024 * 1024;

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

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub enabled: bool,
    /// メモリキャッシュ上限 (バイト)
    pub mem_capacity: u64,
    /// ディスクキャッシュ上限 (バイト)
    pub disk_capacity: u64,
    /// ディスクキャッシュ格納ディレクトリ
    pub dir: PathBuf,
    /// Cache-Control が無い場合の既定 TTL
    pub default_ttl: Duration,
    /// 1 オブジェクトの最大サイズ (バイト)
    pub max_object_size: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mem_capacity: 200 * MIB,
            disk_capacity: 2048 * MIB,
            dir: env::temp_dir().join("sorahost-http-proxy-cache"),
            default_ttl: Duration::from_secs(300),
            max_object_size: 32 * MIB,
        }
    }
}

impl CacheConfig {
    pub fn from_env() -> Self {
        let d = Self::default();
        let mb = |name: &str, fallback: u64| -> u64 {
            env::var(name)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|v| v * MIB)
                .unwrap_or(fallback)
        };

        let enabled = env::var("PROXY_CACHE_ENABLED")
            .map(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
            .unwrap_or(true);

        Self {
            enabled,
            mem_capacity: mb("PROXY_MEM_CACHE_MB", d.mem_capacity),
            disk_capacity: mb("PROXY_DISK_CACHE_MB", d.disk_capacity),
            dir: env::var("PROXY_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or(d.dir),
            default_ttl: env::var("PROXY_CACHE_TTL_SECS")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(d.default_ttl),
            max_object_size: mb("PROXY_CACHE_MAX_OBJECT_MB", d.max_object_size),
        }
    }

    /// テスト用: キャッシュ無効設定。
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
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

struct MemEntry {
    data: Arc<Vec<u8>>,
    stored_at: u64,
    expires_at: u64,
    last_used: u64,
}

struct DiskEntry {
    size: u64,
    expires_at: u64,
    last_used: u64,
}

struct Store<E> {
    entries: HashMap<String, E>,
    bytes: u64,
}

impl<E> Default for Store<E> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
        }
    }
}

pub struct Cache {
    cfg: CacheConfig,
    mem: Mutex<Store<MemEntry>>,
    disk: Mutex<Store<DiskEntry>>,
    clock: AtomicU64,
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
            cfg,
            mem: Mutex::new(Store::default()),
            disk: Mutex::new(Store::default()),
            clock: AtomicU64::new(0),
            hits_mem: AtomicU64::new(0),
            hits_disk: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stores: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            bytes_served: AtomicU64::new(0),
        };
        if cache.cfg.enabled {
            cache.init_disk();
        }
        cache
    }

    pub fn config(&self) -> &CacheConfig {
        &self.cfg
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }

    /// ディスクキャッシュディレクトリを作成し、既存エントリのインデックスを構築する。
    fn init_disk(&self) {
        if let Err(e) = fs::create_dir_all(&self.cfg.dir) {
            log_warn!(
                None,
                "disk cache disabled for dir {}: {}",
                self.cfg.dir.display(),
                e
            );
            return;
        }

        let entries = match fs::read_dir(&self.cfg.dir) {
            Ok(e) => e,
            Err(e) => {
                log_warn!(None, "failed to scan disk cache dir: {}", e);
                return;
            }
        };

        let mut store = self.disk.lock().unwrap_or_else(|p| p.into_inner());
        let mut scanned = 0usize;
        let mut expired = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("cache") {
                continue;
            }
            let key = match path.file_stem().and_then(|s| s.to_str()) {
                Some(k) => k.to_string(),
                None => continue,
            };
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let expires_at = match read_meta(&path) {
                Some(meta) => meta.expires_at,
                None => {
                    let _ = fs::remove_file(&path);
                    continue;
                }
            };
            if expires_at <= now_epoch() {
                let _ = fs::remove_file(&path);
                expired += 1;
                continue;
            }
            let last_used = self.tick();
            store.bytes += size;
            store.entries.insert(
                key,
                DiskEntry {
                    size,
                    expires_at,
                    last_used,
                },
            );
            scanned += 1;
        }
        drop(store);

        log_info!(
            None,
            "disk cache ready at {} ({} entries restored, {} expired removed, limit {} MiB)",
            self.cfg.dir.display(),
            scanned,
            expired,
            self.cfg.disk_capacity / MIB
        );
        self.enforce_disk_limit();
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.cfg.dir.join(format!("{}.cache", key))
    }

    /// キャッシュを引く。メモリ → ディスクの順に探索し、ディスクヒットはメモリへ昇格させる。
    pub fn get(&self, key: &str, conn_id: usize) -> Option<(CachedResponse, CacheSource)> {
        if !self.cfg.enabled {
            return None;
        }
        let now = now_epoch();

        // L1: memory
        {
            let mut store = self.mem.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(entry) = store.entries.get(key) {
                if entry.expires_at > now {
                    let resp = CachedResponse {
                        bytes: Arc::clone(&entry.data),
                        stored_at: entry.stored_at,
                        expires_at: entry.expires_at,
                    };
                    let seq = self.clock.fetch_add(1, Ordering::Relaxed);
                    if let Some(e) = store.entries.get_mut(key) {
                        e.last_used = seq;
                    }
                    drop(store);
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
                log_trace!(Some(conn_id), "cache L1 entry expired key={}", key);
                let size = entry.data.len() as u64;
                store.entries.remove(key);
                store.bytes = store.bytes.saturating_sub(size);
            }
        }

        // L2: disk
        let path = self.path_for(key);
        let present = {
            let store = self.disk.lock().unwrap_or_else(|p| p.into_inner());
            store.entries.get(key).map(|e| e.expires_at)
        };
        if let Some(expires_at) = present {
            if expires_at > now {
                match read_entry(&path) {
                    Ok(Some((meta, body))) if meta.expires_at > now => {
                        let data = Arc::new(body);
                        {
                            let seq = self.clock.fetch_add(1, Ordering::Relaxed);
                            let mut store = self.disk.lock().unwrap_or_else(|p| p.into_inner());
                            if let Some(e) = store.entries.get_mut(key) {
                                e.last_used = seq;
                            }
                        }
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
                        self.insert_mem(key, Arc::clone(&data), meta.stored_at, meta.expires_at);
                        return Some((
                            CachedResponse {
                                bytes: data,
                                stored_at: meta.stored_at,
                                expires_at: meta.expires_at,
                            },
                            CacheSource::Disk,
                        ));
                    }
                    Ok(_) => self.remove_disk(key),
                    Err(e) => {
                        log_warn!(Some(conn_id), "cache L2 read failed key={}: {}", key, e);
                        self.remove_disk(key);
                    }
                }
            } else {
                self.remove_disk(key);
            }
        }

        self.misses.fetch_add(1, Ordering::Relaxed);
        log_debug!(Some(conn_id), "cache MISS key={}", key);
        None
    }

    /// レスポンスをメモリとディスクの両方へ保存する。
    pub fn put(&self, key: &str, url: &str, bytes: Vec<u8>, ttl: Duration, conn_id: usize) {
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

        self.insert_mem(key, Arc::clone(&data), stored_at, expires_at);
        self.write_disk(key, url, &data, stored_at, expires_at, conn_id);
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

    fn insert_mem(&self, key: &str, data: Arc<Vec<u8>>, stored_at: u64, expires_at: u64) {
        let size = data.len() as u64;
        if size > self.cfg.mem_capacity {
            return;
        }
        let seq = self.tick();
        let mut store = self.mem.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(old) = store.entries.remove(key) {
            store.bytes = store.bytes.saturating_sub(old.data.len() as u64);
        }
        store.bytes += size;
        store.entries.insert(
            key.to_string(),
            MemEntry {
                data,
                stored_at,
                expires_at,
                last_used: seq,
            },
        );

        // LRU eviction
        while store.bytes > self.cfg.mem_capacity {
            let victim = store
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    if let Some(e) = store.entries.remove(&k) {
                        store.bytes = store.bytes.saturating_sub(e.data.len() as u64);
                        self.evictions.fetch_add(1, Ordering::Relaxed);
                        log_debug!(
                            None,
                            "cache L1 EVICT key={} freed={}B (usage {}/{} B)",
                            k,
                            e.data.len(),
                            store.bytes,
                            self.cfg.mem_capacity
                        );
                    }
                }
                None => break,
            }
        }
    }

    fn write_disk(
        &self,
        key: &str,
        url: &str,
        data: &[u8],
        stored_at: u64,
        expires_at: u64,
        conn_id: usize,
    ) {
        if data.len() as u64 > self.cfg.disk_capacity {
            return;
        }
        let path = self.path_for(key);
        let tmp = self.cfg.dir.join(format!("{}.tmp", key));
        let mut blob = Vec::with_capacity(data.len() + 128);
        blob.extend_from_slice(MAGIC.as_bytes());
        blob.push(b'\n');
        blob.extend_from_slice(url.replace(['\n', '\r'], "").as_bytes());
        blob.push(b'\n');
        blob.extend_from_slice(format!("{} {}\n", stored_at, expires_at).as_bytes());
        blob.extend_from_slice(data);

        if let Err(e) = fs::write(&tmp, &blob).and_then(|_| fs::rename(&tmp, &path)) {
            log_warn!(Some(conn_id), "cache L2 write failed key={}: {}", key, e);
            let _ = fs::remove_file(&tmp);
            return;
        }

        let size = blob.len() as u64;
        let seq = self.tick();
        {
            let mut store = self.disk.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(old) = store.entries.remove(key) {
                store.bytes = store.bytes.saturating_sub(old.size);
            }
            store.bytes += size;
            store.entries.insert(
                key.to_string(),
                DiskEntry {
                    size,
                    expires_at,
                    last_used: seq,
                },
            );
        }
        log_trace!(Some(conn_id), "cache L2 wrote {} ({}B)", path.display(), size);
        self.enforce_disk_limit();
    }

    fn remove_disk(&self, key: &str) {
        let _ = fs::remove_file(self.path_for(key));
        let mut store = self.disk.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(e) = store.entries.remove(key) {
            store.bytes = store.bytes.saturating_sub(e.size);
        }
    }

    fn enforce_disk_limit(&self) {
        loop {
            let victim = {
                let store = self.disk.lock().unwrap_or_else(|p| p.into_inner());
                if store.bytes <= self.cfg.disk_capacity {
                    return;
                }
                store
                    .entries
                    .iter()
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(k, e)| (k.clone(), e.size))
            };
            match victim {
                Some((k, size)) => {
                    self.remove_disk(&k);
                    self.evictions.fetch_add(1, Ordering::Relaxed);
                    log_debug!(None, "cache L2 EVICT key={} freed={}B", k, size);
                }
                None => return,
            }
        }
    }

    pub fn mem_usage(&self) -> (u64, usize) {
        let store = self.mem.lock().unwrap_or_else(|p| p.into_inner());
        (store.bytes, store.entries.len())
    }

    pub fn disk_usage(&self) -> (u64, usize) {
        let store = self.disk.lock().unwrap_or_else(|p| p.into_inner());
        (store.bytes, store.entries.len())
    }

    /// `/status` 用の JSON フラグメント。
    pub fn to_json(&self) -> String {
        let (mem_bytes, mem_entries) = self.mem_usage();
        let (disk_bytes, disk_entries) = self.disk_usage();
        let hits_mem = self.hits_mem.load(Ordering::Relaxed);
        let hits_disk = self.hits_disk.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let lookups = hits_mem + hits_disk + misses;
        let hit_ratio = if lookups == 0 {
            0.0
        } else {
            (hits_mem + hits_disk) as f64 / lookups as f64
        };

        format!(
            concat!(
                "{{\"enabled\":{},\"hits\":{},\"hits_memory\":{},\"hits_disk\":{},",
                "\"misses\":{},\"hit_ratio\":{:.4},\"stores\":{},\"evictions\":{},",
                "\"bytes_served\":{},",
                "\"memory\":{{\"used_bytes\":{},\"limit_bytes\":{},\"entries\":{}}},",
                "\"disk\":{{\"used_bytes\":{},\"limit_bytes\":{},\"entries\":{},\"dir\":\"{}\"}}}}"
            ),
            self.cfg.enabled,
            hits_mem + hits_disk,
            hits_mem,
            hits_disk,
            misses,
            hit_ratio,
            self.stores.load(Ordering::Relaxed),
            self.evictions.load(Ordering::Relaxed),
            self.bytes_served.load(Ordering::Relaxed),
            mem_bytes,
            self.cfg.mem_capacity,
            mem_entries,
            disk_bytes,
            self.cfg.disk_capacity,
            disk_entries,
            self.cfg.dir.display().to_string().replace('\\', "/").replace('"', "'")
        )
    }
}

struct Meta {
    stored_at: u64,
    expires_at: u64,
}

fn read_meta(path: &Path) -> Option<Meta> {
    let data = fs::read(path).ok()?;
    parse_blob(&data).map(|(meta, _)| meta)
}

fn read_entry(path: &Path) -> io::Result<Option<(Meta, Vec<u8>)>> {
    let data = fs::read(path)?;
    Ok(parse_blob(&data).map(|(meta, offset)| (meta, data[offset..].to_vec())))
}

/// キャッシュファイルのヘッダーを解析し、(メタ情報, ボディ開始オフセット) を返す。
fn parse_blob(data: &[u8]) -> Option<(Meta, usize)> {
    let mut offset = 0usize;
    let mut lines = Vec::with_capacity(3);
    for _ in 0..3 {
        let nl = data[offset..].iter().position(|&b| b == b'\n')? + offset;
        lines.push(std::str::from_utf8(&data[offset..nl]).ok()?.to_string());
        offset = nl + 1;
    }
    if lines[0] != MAGIC {
        return None;
    }
    let mut nums = lines[2].split_whitespace();
    let stored_at = nums.next()?.parse().ok()?;
    let expires_at = nums.next()?.parse().ok()?;
    Some((Meta { stored_at, expires_at }, offset))
}

/// FNV-1a を 2 系統走らせた 128bit 相当のキャッシュキー。
pub fn cache_key(method: &str, url: &str) -> String {
    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0x9e37_79b9_7f4a_7c15;
    for b in method.as_bytes().iter().chain(b"|".iter()).chain(url.as_bytes()) {
        h1 ^= *b as u64;
        h1 = h1.wrapping_mul(0x0000_0100_0000_01b3);
        h2 = h2.rotate_left(7) ^ (*b as u64);
        h2 = h2.wrapping_mul(0x8864_3f65_e5a2_9d2b);
    }
    format!("{:016x}{:016x}", h1, h2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg(dir: &str, mem: u64, disk: u64) -> CacheConfig {
        CacheConfig {
            enabled: true,
            mem_capacity: mem,
            disk_capacity: disk,
            dir: env::temp_dir().join(dir),
            default_ttl: Duration::from_secs(60),
            max_object_size: 1024 * 1024,
        }
    }

    fn fresh(dir: &str, mem: u64, disk: u64) -> Cache {
        let cfg = test_cfg(dir, mem, disk);
        let _ = fs::remove_dir_all(&cfg.dir);
        Cache::new(cfg)
    }

    #[test]
    fn test_cache_key_stable_and_distinct() {
        let a = cache_key("GET", "http://example.com/a");
        assert_eq!(a, cache_key("GET", "http://example.com/a"));
        assert_ne!(a, cache_key("GET", "http://example.com/b"));
        assert_ne!(a, cache_key("HEAD", "http://example.com/a"));
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn test_put_get_roundtrip() {
        let cache = fresh("shp-test-roundtrip", 1024 * 1024, 4 * 1024 * 1024);
        let key = cache_key("GET", "http://example.com/x");
        cache.put(&key, "http://example.com/x", b"payload".to_vec(), Duration::from_secs(60), 1);

        let (resp, src) = cache.get(&key, 1).expect("expected hit");
        assert_eq!(&resp.bytes[..], b"payload");
        assert_eq!(src, CacheSource::Memory);
        assert_eq!(cache.hits_mem.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_disk_hit_after_memory_eviction() {
        // メモリ 100 バイト上限 -> 2 件目投入で 1 件目が L1 から追い出される
        let cache = fresh("shp-test-l2", 100, 4 * 1024 * 1024);
        let k1 = cache_key("GET", "http://example.com/1");
        let k2 = cache_key("GET", "http://example.com/2");
        cache.put(&k1, "http://example.com/1", vec![b'a'; 80], Duration::from_secs(60), 1);
        cache.put(&k2, "http://example.com/2", vec![b'b'; 80], Duration::from_secs(60), 2);

        assert_eq!(cache.mem_usage().1, 1, "L1 should hold only one entry");

        let (resp, src) = cache.get(&k1, 3).expect("expected disk hit");
        assert_eq!(src, CacheSource::Disk);
        assert_eq!(resp.bytes.len(), 80);
        assert_eq!(cache.hits_disk.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_expired_entry_is_a_miss() {
        let cache = fresh("shp-test-expire", 1024 * 1024, 1024 * 1024);
        let key = cache_key("GET", "http://example.com/exp");
        cache.put(&key, "http://example.com/exp", b"old".to_vec(), Duration::from_secs(0), 1);
        assert!(cache.get(&key, 1).is_none());
        assert_eq!(cache.misses.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_disk_capacity_eviction() {
        let cache = fresh("shp-test-diskcap", 10 * 1024 * 1024, 500);
        for i in 0..10 {
            let url = format!("http://example.com/{}", i);
            let key = cache_key("GET", &url);
            cache.put(&key, &url, vec![b'x'; 200], Duration::from_secs(60), i);
        }
        let (bytes, entries) = cache.disk_usage();
        assert!(bytes <= 500, "disk usage {} exceeds limit", bytes);
        assert!(entries < 10);
        assert!(cache.evictions.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn test_oversized_object_not_cached() {
        let mut cfg = test_cfg("shp-test-oversize", 1024 * 1024, 1024 * 1024);
        cfg.max_object_size = 10;
        let _ = fs::remove_dir_all(&cfg.dir);
        let cache = Cache::new(cfg);
        let key = cache_key("GET", "http://example.com/big");
        cache.put(&key, "http://example.com/big", vec![b'z'; 100], Duration::from_secs(60), 1);
        assert!(cache.get(&key, 1).is_none());
    }

    #[test]
    fn test_disabled_cache_is_noop() {
        let cache = Cache::new(CacheConfig::disabled());
        let key = cache_key("GET", "http://example.com/off");
        cache.put(&key, "http://example.com/off", b"body".to_vec(), Duration::from_secs(60), 1);
        assert!(cache.get(&key, 1).is_none());
    }

    #[test]
    fn test_disk_index_restored_on_startup() {
        let cfg = test_cfg("shp-test-restore", 1024 * 1024, 1024 * 1024);
        let _ = fs::remove_dir_all(&cfg.dir);
        let key = cache_key("GET", "http://example.com/persist");
        {
            let cache = Cache::new(cfg.clone());
            cache.put(&key, "http://example.com/persist", b"persisted".to_vec(), Duration::from_secs(600), 1);
        }
        let cache2 = Cache::new(cfg);
        assert_eq!(cache2.disk_usage().1, 1);
        let (resp, src) = cache2.get(&key, 1).expect("expected restored disk hit");
        assert_eq!(&resp.bytes[..], b"persisted");
        assert_eq!(src, CacheSource::Disk);
    }

    #[test]
    fn test_to_json_contains_limits() {
        let cache = fresh("shp-test-json", 200 * MIB, 2048 * MIB);
        let json = cache.to_json();
        assert!(json.contains("\"limit_bytes\":209715200"), "{}", json);
        assert!(json.contains("\"limit_bytes\":2147483648"), "{}", json);
        assert!(json.contains("\"hit_ratio\":0.0000"), "{}", json);
    }
}
