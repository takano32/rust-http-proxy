//! L1 メモリ層: LRU ストアと、予算の未使用分を実際に確保しておくバラスト。
//!
//! バラストは 64 MiB 単位の `Vec<u8>` を 0 以外で埋めて全ページをコミットさせたもの。
//! キャッシュエントリが増えるとその分だけバラストを解放し、常に
//! `エントリ + バラスト <= 予算` を保つ。予算が縮んだときはバラスト → LRU の順に手放す。
//!
//! 期限切れでもバリデータ (ETag / Last-Modified) を持つエントリは再検証用に残す。

use crate::sync::LockExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::format::Meta;
use super::key::CacheKey;
use super::lru::{LruEntry, Store};
use crate::{log_debug, log_warn};

/// バラストの割り当て単位。glibc の mmap 閾値の上限 (32 MiB) を超えるサイズにして、
/// 解放時に確実に OS へ返るようにする。
pub const BALLAST_CHUNK: usize = 64 * 1024 * 1024;
/// 予算縮退時に 1 回のロックで追い出す最大件数 (ロック保持時間を抑える)。
const EVICT_BATCH: usize = 256;

pub struct MemEntry {
    pub data: Arc<Vec<u8>>,
    pub meta: Meta,
    last_used: u64,
}

impl LruEntry for MemEntry {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn last_used(&self) -> u64 {
        self.last_used
    }
    fn set_last_used(&mut self, seq: u64) {
        self.last_used = seq;
    }
}

/// `get` の結果。`meta.expires_at <= now` なら stale (再検証が必要)。
pub struct MemHit {
    pub data: Arc<Vec<u8>>,
    pub meta: Meta,
}

pub struct MemTier {
    store: Mutex<Store<MemEntry>>,
    capacity: AtomicU64,
    ballast: Mutex<Vec<Vec<u8>>>,
    ballast_bytes: AtomicU64,
    reserve: bool,
    alloc_failed: AtomicBool,
}

impl MemTier {
    pub fn new(reserve: bool) -> Self {
        Self {
            store: Mutex::new(Store::default()),
            capacity: AtomicU64::new(0),
            ballast: Mutex::new(Vec::new()),
            ballast_bytes: AtomicU64::new(0),
            reserve,
            alloc_failed: AtomicBool::new(false),
        }
    }

    pub fn capacity(&self) -> u64 {
        self.capacity.load(Ordering::Relaxed)
    }

    pub fn set_capacity(&self, bytes: u64) {
        self.capacity.store(bytes, Ordering::Relaxed);
    }

    /// (エントリ合計バイト, 件数)
    pub fn usage(&self) -> (u64, usize) {
        let store = self.store.locked();
        (store.bytes(), store.len())
    }

    pub fn ballast_bytes(&self) -> u64 {
        self.ballast_bytes.load(Ordering::Relaxed)
    }

    /// エントリ + バラスト (自分がシステムから取っている分)。
    pub fn owned(&self) -> u64 {
        self.usage().0.saturating_add(self.ballast_bytes())
    }

    /// LRU に触らずにエントリの (サイズ, メタ情報) を見る。
    pub fn peek(&self, key: CacheKey) -> Option<(u64, Meta)> {
        let store = self.store.locked();
        store.get(key).map(|e| (e.data.len() as u64, e.meta))
    }

    /// 全エントリを消し、件数を返す。
    pub fn clear(&self) -> usize {
        let old = {
            let mut store = self.store.locked();
            std::mem::take(&mut *store)
        };
        old.len()
    }

    /// エントリがあれば返す (期限切れでもバリデータ付きなら返す)。再検証できない期限切れは削除。
    pub fn get(&self, key: CacheKey, now: u64, seq: u64) -> Option<MemHit> {
        let mut store = self.store.locked();
        let entry = store.get(key)?;
        if entry.meta.expires_at > now || entry.meta.validators {
            let hit = MemHit {
                data: Arc::clone(&entry.data),
                meta: entry.meta,
            };
            store.touch(key, seq);
            return Some(hit);
        }
        let removed = store.remove(key);
        drop(store);
        drop(removed);
        None
    }

    /// 挿入し、上限超過分を LRU で追い出す。戻り値は追い出し件数。
    pub fn insert(&self, key: CacheKey, data: Arc<Vec<u8>>, meta: Meta, seq: u64) -> usize {
        let capacity = self.capacity();
        if data.len() as u64 > capacity {
            return 0;
        }
        let mut dropped: Vec<Arc<Vec<u8>>> = Vec::new();
        let (bytes, evicted) = {
            let mut store = self.store.locked();
            let entry = MemEntry {
                data,
                meta,
                last_used: 0,
            };
            if let Some(old) = store.insert(key, entry, seq) {
                dropped.push(old.data);
            }
            let mut evicted = 0;
            while store.bytes() > capacity {
                let Some((k, e)) = store.pop_lru() else {
                    break;
                };
                log_debug!(
                    None,
                    "cache L1 EVICT key={} freed={}B (usage {}/{} B)",
                    k,
                    e.data.len(),
                    store.bytes(),
                    capacity
                );
                dropped.push(e.data);
                evicted += 1;
            }
            (store.bytes(), evicted)
        };
        self.release_ballast(bytes, capacity);
        drop(dropped);
        evicted
    }

    /// 再検証に成功したので有効期限を延ばす。エントリが無ければ false。
    pub fn refresh(&self, key: CacheKey, stored_at: u64, expires_at: u64, seq: u64) -> bool {
        let mut store = self.store.locked();
        let Some(entry) = store.get_mut(key) else {
            return false;
        };
        entry.meta.stored_at = stored_at;
        entry.meta.expires_at = expires_at;
        store.touch(key, seq);
        true
    }

    pub fn remove(&self, key: CacheKey) -> bool {
        let removed = {
            let mut store = self.store.locked();
            store.remove(key)
        };
        removed.is_some()
    }

    /// 予算に合わせる (プローブから呼ぶ)。バラスト → LRU の順に手放し、追い出し件数を返す。
    pub fn enforce(&self) -> usize {
        let capacity = self.capacity();
        self.release_ballast(self.usage().0, capacity);
        let mut total = 0;
        loop {
            let mut dropped = Vec::new();
            {
                let mut store = self.store.locked();
                while store.bytes() > capacity && dropped.len() < EVICT_BATCH {
                    match store.pop_lru() {
                        Some((_, e)) => dropped.push(e.data),
                        None => break,
                    }
                }
            }
            if dropped.is_empty() {
                break;
            }
            total += dropped.len();
            drop(dropped);
        }
        if total > 0 {
            log_debug!(
                None,
                "cache L1 shrink: evicted {} entries to fit {} B",
                total,
                capacity
            );
        }
        total
    }

    /// `cache_bytes + バラスト > capacity` の間、バラストを解放する。
    pub fn release_ballast(&self, cache_bytes: u64, capacity: u64) {
        if self.ballast_bytes() == 0 {
            return;
        }
        let mut freed = Vec::new();
        {
            let mut ballast = self.ballast.locked();
            while cache_bytes.saturating_add(self.ballast_bytes()) > capacity {
                let Some(chunk) = ballast.pop() else {
                    break;
                };
                self.ballast_bytes
                    .fetch_sub(chunk.len() as u64, Ordering::Relaxed);
                freed.push(chunk);
            }
        }
        drop(freed);
    }

    /// 予算の未使用分をバラストで埋める。戻り値は追加したバイト数。
    pub fn fill_ballast(&self) -> u64 {
        if !self.reserve || self.alloc_failed.load(Ordering::Relaxed) {
            return 0;
        }
        let mut added = 0u64;
        loop {
            let capacity = self.capacity();
            if self.owned().saturating_add(BALLAST_CHUNK as u64) > capacity {
                break;
            }
            let mut chunk: Vec<u8> = Vec::new();
            if chunk.try_reserve_exact(BALLAST_CHUNK).is_err() {
                self.alloc_failed.store(true, Ordering::Relaxed);
                log_warn!(
                    None,
                    "memory reservation stopped: allocating {} MiB failed",
                    BALLAST_CHUNK / (1024 * 1024)
                );
                break;
            }
            // 0 以外で埋めて calloc 最適化を避け、全ページを実際にコミットさせる
            chunk.resize(BALLAST_CHUNK, 0xA5);
            {
                let mut ballast = self.ballast.locked();
                ballast.push(chunk);
                self.ballast_bytes
                    .fetch_add(BALLAST_CHUNK as u64, Ordering::Relaxed);
            }
            added += BALLAST_CHUNK as u64;
        }
        added
    }

    /// 期限切れで再検証できない、または期限から `max_stale` 秒以上経ったエントリを削除する。
    pub fn sweep(&self, now: u64, max_stale: u64) -> usize {
        let mut dropped = Vec::new();
        {
            let mut store = self.store.locked();
            let keys = store.keys_where(|e| is_garbage(&e.meta, now, max_stale));
            for key in keys {
                if let Some(e) = store.remove(key) {
                    dropped.push(e.data);
                }
            }
        }
        let n = dropped.len();
        drop(dropped);
        n
    }
}

/// 期限切れで再検証できない、または期限から `max_stale` 秒以上経っているか。
pub fn is_garbage(meta: &Meta, now: u64, max_stale: u64) -> bool {
    meta.expires_at <= now && (!meta.validators || meta.expires_at.saturating_add(max_stale) <= now)
}
