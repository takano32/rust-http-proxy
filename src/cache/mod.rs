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

pub mod budget;
pub mod config;
pub mod disk;
pub mod format;
pub mod key;
pub mod lru;
pub mod margin;
pub mod memory;
pub mod probe;
pub mod status;
#[cfg(test)]
mod tests;

pub use config::{CacheConfig, Limit, MIB};
pub use format::Meta;
pub use key::{CacheKey, cache_key, cache_key_variant};

use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use budget::Snapshot;
use disk::{DiskTier, DiskWriter};
use margin::Margin;
use memory::MemTier;

use crate::sysinfo;
use crate::{log_debug, log_info, log_warn};

/// ヘッダー部の上限 (これを超える場合は壊れているとみなす)。
const HEAD_MAX: usize = 1024 * 1024;

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

/// `Cursor` で読むための `Arc<Vec<u8>>` ラッパ。
struct ArcBytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for ArcBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// キャッシュ済みレスポンスの本文 (ヘッダー部の後ろ)。
pub enum Body {
    Memory { data: Arc<Vec<u8>>, offset: usize },
    File(BufReader<File>),
}

/// キャッシュ済みレスポンス。`head` はステータス行 + ヘッダー + 空行、`body` はその続き。
pub struct CachedResponse {
    pub head: Vec<u8>,
    pub body: Body,
    /// ワイヤバイト列全体の長さ (head + body)
    pub size: u64,
    pub meta: Meta,
}

impl CachedResponse {
    pub fn is_fresh(&self, now: u64) -> bool {
        self.meta.expires_at > now
    }

    pub fn age(&self) -> u64 {
        now_epoch().saturating_sub(self.meta.stored_at)
    }

    pub fn ttl_left(&self, now: u64) -> u64 {
        self.meta.expires_at.saturating_sub(now)
    }

    /// 本文を読むリーダー。
    pub fn into_body_reader(self) -> Box<dyn Read + Send> {
        match self.body {
            Body::Memory { data, offset } => {
                let mut cur = Cursor::new(ArcBytes(data));
                cur.set_position(offset as u64);
                Box::new(cur)
            }
            Body::File(reader) => Box::new(reader),
        }
    }

    /// ワイヤバイト列全体を読む (テスト・小さなエントリ用)。
    pub fn read_all(self) -> io::Result<Vec<u8>> {
        let mut out = self.head.clone();
        self.into_body_reader().read_to_end(&mut out)?;
        Ok(out)
    }
}

/// ワイヤバイト列の先頭からヘッダー部 (空行まで) を読む。
fn read_head<R: BufRead>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(512);
    loop {
        let start = head.len();
        let n = reader.read_until(b'\n', &mut head)?;
        if n == 0 || head.len() > HEAD_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cached response has no header terminator",
            ));
        }
        let line = &head[start..];
        if line == b"\r\n" || line == b"\n" {
            return Ok(head);
        }
    }
}

fn head_len(wire: &[u8]) -> usize {
    wire.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(wire.len())
}

/// 保存結果。
#[derive(Debug, Default, Clone, Copy)]
pub struct StoreOutcome {
    pub memory: bool,
    pub disk: bool,
    pub bytes: u64,
}

/// ストリーミング保存の受け口。本文が届くたびに `write` し、正常終了なら `finish`。
pub struct StoreSink<'a> {
    cache: &'a Cache,
    key: CacheKey,
    url: String,
    meta: Meta,
    ttl: Duration,
    mem_buf: Option<Vec<u8>>,
    disk: Option<DiskWriter<'a>>,
    total: u64,
    conn_id: usize,
}

impl StoreSink<'_> {
    pub fn write(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len() as u64);
        if let Some(buf) = self.mem_buf.as_mut() {
            if self.total > self.cache.cfg.mem_max_object_size {
                self.mem_buf = None;
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        if let Some(w) = self.disk.as_mut()
            && let Err(e) = w.write(chunk)
        {
            if e.kind() == io::ErrorKind::FileTooLarge {
                log_debug!(
                    Some(self.conn_id),
                    "cache L2 SKIP: object exceeds the disk cache limit url={}",
                    self.url
                );
            } else {
                log_warn!(
                    Some(self.conn_id),
                    "cache L2 write failed key={}: {}",
                    self.key,
                    e
                );
            }
            if let Some(w) = self.disk.take() {
                w.abort();
            }
        }
    }

    pub fn finish(mut self) -> StoreOutcome {
        let cache = self.cache;
        let mut out = StoreOutcome {
            bytes: self.total,
            ..StoreOutcome::default()
        };
        if let Some(buf) = self.mem_buf.take() {
            let evicted = cache
                .mem
                .insert(self.key, Arc::new(buf), self.meta, cache.tick());
            cache.count_evictions(evicted);
            out.memory = cache.mem.capacity() >= self.total;
        }
        if let Some(w) = self.disk.take() {
            match w.finish() {
                Ok(r) => {
                    cache.count_evictions(r.evicted);
                    out.disk = r.stored;
                }
                Err(e) => log_warn!(
                    Some(self.conn_id),
                    "cache L2 write failed key={}: {}",
                    self.key,
                    e
                ),
            }
        }
        if out.memory || out.disk {
            cache.stores.fetch_add(1, Ordering::Relaxed);
            log_debug!(
                Some(self.conn_id),
                "cache STORE key={} size={}B ttl={}s validators={} tiers={}{} url={}",
                self.key,
                self.total,
                self.ttl.as_secs(),
                self.meta.validators,
                if out.memory { "L1" } else { "" },
                if out.disk { "L2" } else { "" },
                self.url
            );
        }
        out
    }

    /// 途中で諦める (本文が途切れた等)。
    pub fn abort(mut self) {
        if let Some(w) = self.disk.take() {
            w.abort();
        }
        self.mem_buf = None;
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
    mem: MemTier,
    disk: DiskTier,
    /// LRU の採番用カウンタ (両層で共有)
    clock: AtomicU64,
    ticks: AtomicU64,
    snapshot: Mutex<Snapshot>,
    margins: Mutex<Margins>,
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
    pub revalidations: AtomicU64,
    pub evictions: AtomicU64,
    pub bytes_served: AtomicU64,
}

impl Cache {
    pub fn new(cfg: CacheConfig) -> Self {
        let cache = Self {
            mem: MemTier::new(cfg.reserve),
            disk: DiskTier::new(cfg.dir.clone(), cfg.reserve),
            margins: Mutex::new(Margins {
                host: Margin::new(cfg.mem_keep_free),
                cgroup: Margin::new(cfg.mem_keep_free),
                disk: Margin::new(cfg.disk_keep_free),
            }),
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
            revalidations: AtomicU64::new(0),
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
        if cache.cfg.pterodactyl && cache.cfg.disk_limit.is_auto() && !cache.cfg.disk_quota_set {
            log_warn!(
                None,
                "Pterodactyl detected but the disk allocation is unknown (Pterodactyl does not pass it to the container): sizing the disk cache against the host filesystem, which can exceed the server's Disk Space and block the next start; set PROXY_DISK_QUOTA_MB (or SERVER_DISK) to the allocation in MB, or 0 if unlimited"
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

    /// ディスク上のパス (インデックスに無ければ `None`)。
    pub fn disk_path(&self, key: CacheKey) -> Option<PathBuf> {
        self.disk
            .lookup(key)
            .map(|m| self.disk.path_for(key, m.validators))
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

    /// キャッシュを引く。メモリ → ディスクの順に探索し、小さなディスクヒットはメモリへ昇格させる。
    /// 期限切れでもバリデータ付きなら返す (呼び出し側が `is_fresh` で判断して再検証する)。
    pub fn get(&self, key: CacheKey, conn_id: usize) -> Option<(CachedResponse, CacheSource)> {
        if !self.cfg.enabled {
            return None;
        }
        let now = now_epoch();

        if let Some(hit) = self.mem.get(key, now, self.tick()) {
            let offset = head_len(&hit.data);
            let resp = CachedResponse {
                head: hit.data[..offset].to_vec(),
                size: hit.data.len() as u64,
                body: Body::Memory {
                    data: hit.data,
                    offset,
                },
                meta: hit.meta,
            };
            self.hits_mem.fetch_add(1, Ordering::Relaxed);
            self.bytes_served.fetch_add(resp.size, Ordering::Relaxed);
            log_debug!(
                Some(conn_id),
                "cache L1 {} key={} size={}B age={}s",
                if resp.is_fresh(now) { "HIT" } else { "STALE" },
                key,
                resp.size,
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

    /// L2 を引き、小さければ L1 へ昇格させる。壊れた・再検証できない期限切れのエントリはここで消す。
    fn get_from_disk(&self, key: CacheKey, now: u64, conn_id: usize) -> Option<CachedResponse> {
        if !self.disk.is_ready() {
            return None;
        }
        let indexed = self.disk.lookup(key)?;
        if indexed.expires_at <= now && !indexed.validators {
            self.disk.remove(key);
            return None;
        }
        let hit = match self.disk.open(key) {
            Ok(Some(hit)) if hit.meta.expires_at > now || hit.meta.validators => hit,
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
        let meta = hit.meta;
        let size = hit.size;
        let mut reader = BufReader::with_capacity(256 * 1024, hit.file);
        let resp = if size <= self.cfg.mem_max_object_size {
            let mut data = Vec::with_capacity(size as usize);
            if let Err(e) = reader.read_to_end(&mut data) {
                log_warn!(Some(conn_id), "cache L2 read failed key={}: {}", key, e);
                self.disk.remove(key);
                return None;
            }
            sysinfo::drop_page_cache(reader.get_ref());
            let data = Arc::new(data);
            let evicted = self.mem.insert(key, Arc::clone(&data), meta, self.tick());
            self.count_evictions(evicted);
            let offset = head_len(&data);
            CachedResponse {
                head: data[..offset].to_vec(),
                size,
                body: Body::Memory { data, offset },
                meta,
            }
        } else {
            let head = match read_head(&mut reader) {
                Ok(h) => h,
                Err(e) => {
                    log_warn!(Some(conn_id), "cache L2 read failed key={}: {}", key, e);
                    self.disk.remove(key);
                    return None;
                }
            };
            CachedResponse {
                head,
                size,
                body: Body::File(reader),
                meta,
            }
        };
        self.disk.touch(key, self.tick());
        self.hits_disk.fetch_add(1, Ordering::Relaxed);
        self.bytes_served.fetch_add(size, Ordering::Relaxed);
        log_debug!(
            Some(conn_id),
            "cache L2 {} key={} size={}B age={}s{}",
            if resp.is_fresh(now) { "HIT" } else { "STALE" },
            key,
            size,
            now.saturating_sub(meta.stored_at),
            if size <= self.cfg.mem_max_object_size {
                " (promoted to L1)"
            } else {
                " (streaming from disk)"
            }
        );
        Some(resp)
    }

    /// ストリーミング保存を開始する。`expected` はレスポンスの Content-Length (分かれば)。
    pub fn begin_store(
        &self,
        key: CacheKey,
        url: &str,
        ttl: Duration,
        validators: bool,
        expected: Option<u64>,
        conn_id: usize,
    ) -> StoreSink<'_> {
        let stored_at = now_epoch();
        let meta = Meta {
            stored_at,
            expires_at: stored_at.saturating_add(ttl.as_secs()),
            validators,
        };
        let mem_buf =
            if self.cfg.enabled && expected.is_none_or(|e| e <= self.cfg.mem_max_object_size) {
                Some(Vec::with_capacity(
                    expected
                        .unwrap_or(64 * 1024)
                        .min(self.cfg.mem_max_object_size) as usize,
                ))
            } else {
                None
            };
        let disk = if self.cfg.enabled {
            match self.disk.begin(
                key,
                url,
                meta,
                self.tick(),
                expected,
                self.cfg.max_object_size,
            ) {
                Ok(w) => w,
                Err(e) => {
                    log_warn!(Some(conn_id), "cache L2 write failed key={}: {}", key, e);
                    None
                }
            }
        } else {
            None
        };
        StoreSink {
            cache: self,
            key,
            url: url.to_string(),
            meta,
            ttl,
            mem_buf,
            disk,
            total: 0,
            conn_id,
        }
    }

    /// 一括保存 (小さなレスポンス・テスト用)。
    pub fn put(&self, key: CacheKey, url: &str, bytes: Vec<u8>, ttl: Duration, conn_id: usize) {
        self.put_with(key, url, bytes, ttl, false, conn_id);
    }

    pub fn put_with(
        &self,
        key: CacheKey,
        url: &str,
        bytes: Vec<u8>,
        ttl: Duration,
        validators: bool,
        conn_id: usize,
    ) -> StoreOutcome {
        if !self.cfg.enabled {
            return StoreOutcome::default();
        }
        let mut sink =
            self.begin_store(key, url, ttl, validators, Some(bytes.len() as u64), conn_id);
        sink.write(&bytes);
        sink.finish()
    }

    /// 再検証 (304) に成功したので有効期限を延ばす。戻り値は新しい期限。
    pub fn refresh(&self, key: CacheKey, ttl: Duration, conn_id: usize) -> u64 {
        let expires_at = now_epoch().saturating_add(ttl.as_secs());
        let in_mem = self.mem.refresh(key, expires_at, self.tick());
        let on_disk = self.disk.refresh(key, expires_at, self.tick());
        if in_mem || on_disk {
            self.revalidations.fetch_add(1, Ordering::Relaxed);
        }
        log_debug!(
            Some(conn_id),
            "cache REFRESH key={} ttl={}s (L1={} L2={})",
            key,
            ttl.as_secs(),
            in_mem,
            on_disk
        );
        expires_at
    }

    pub fn remove(&self, key: CacheKey) {
        self.mem.remove(key);
        self.disk.remove(key);
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        if self.cfg.enabled {
            self.disk.shutdown();
        }
    }
}
