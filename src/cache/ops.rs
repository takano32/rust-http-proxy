//! `Cache` の読み書き操作: 参照 (L1 → L2、昇格)、ストリーミング保存、延命、無効化。

use std::io::{BufReader, Read};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::entry::{Body, CachedResponse, head_len, read_head};
use super::format::Meta;
use super::key::{CacheKey, cache_key};
use super::quota::is_permanent;
use super::sink::{StoreOutcome, StoreSink};
use super::{Cache, CacheSource, now_epoch};
use crate::sysinfo;
use crate::{log_debug, log_warn};

impl Cache {
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
                // 壊れている・無い場合だけ消す。一時的な I/O エラー (fd 枯渇など) では残す
                if is_permanent(&e) {
                    log_warn!(Some(conn_id), "cache L2 read failed key={}: {}", key, e);
                    self.disk.remove(key);
                } else {
                    log_warn!(
                        Some(conn_id),
                        "cache L2 temporarily unreadable key={}: {}",
                        key,
                        e
                    );
                }
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
                if is_permanent(&e) {
                    self.disk.remove(key);
                }
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
    /// URL とバリアントの対応を覚えておく (無効化用)。
    pub fn remember_variant(&self, url: &str, key: CacheKey) {
        let base = cache_key("GET", url);
        let mut v = self.variants.lock().unwrap_or_else(|p| p.into_inner());
        let list = v.entry(base).or_default();
        if !list.contains(&key) {
            list.push(key);
        }
    }
    /// URL のすべてのバリアントを消す (unsafe メソッドへの成功応答時, RFC 9111 §4.4)。
    /// 再起動前に保存されたバリアントは把握できないので、ベストエフォート。
    pub fn invalidate(&self, url: &str, conn_id: usize) -> usize {
        let base = cache_key("GET", url);
        let mut keys = {
            let mut v = self.variants.lock().unwrap_or_else(|p| p.into_inner());
            v.remove(&base).unwrap_or_default()
        };
        if !keys.contains(&base) {
            keys.push(base);
        }
        let mut removed = 0;
        for k in keys {
            let had = self.mem.remove(k) | self.disk.lookup(k).is_some();
            self.disk.remove(k);
            if had {
                removed += 1;
            }
        }
        if removed > 0 {
            log_debug!(
                Some(conn_id),
                "cache INVALIDATE url={} ({} variants)",
                url,
                removed
            );
        }
        removed
    }
    /// ストリーミング保存を開始する。`expected` はレスポンスの Content-Length (分かれば)、
    /// `age` は受信時点で既に経過している時間 (上流キャッシュの Age / Date から)。
    #[allow(clippy::too_many_arguments)]
    pub fn begin_store(
        &self,
        key: CacheKey,
        url: &str,
        ttl: Duration,
        age: u64,
        validators: bool,
        expected: Option<u64>,
        conn_id: usize,
    ) -> StoreSink<'_> {
        let now = now_epoch();
        let meta = Meta {
            stored_at: now.saturating_sub(age),
            expires_at: now.saturating_add(ttl.as_secs().saturating_sub(age)),
            validators,
        };
        self.remember_variant(url, key);
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
        let mut sink = self.begin_store(
            key,
            url,
            ttl,
            0,
            validators,
            Some(bytes.len() as u64),
            conn_id,
        );
        sink.write(&bytes);
        sink.finish()
    }
    /// 再検証 (304) に成功したので有効期限を延ばし、経過時間 (Age) を `age` から数え直す。
    /// 戻り値は新しい期限。
    pub fn refresh(&self, key: CacheKey, ttl: Duration, age: u64, conn_id: usize) -> u64 {
        let now = now_epoch();
        let stored_at = now.saturating_sub(age);
        let expires_at = now.saturating_add(ttl.as_secs().saturating_sub(age));
        let in_mem = self.mem.refresh(key, stored_at, expires_at, self.tick());
        let on_disk = self.disk.refresh(key, stored_at, expires_at, self.tick());
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
