//! ストリーミング保存の受け口。本文が届くたびに `write` し、正常終了なら `finish`。

use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::Cache;
use super::disk::DiskWriter;
use super::format::Meta;
use super::key::CacheKey;
use crate::{log_debug, log_warn};

/// 保存結果。
#[derive(Debug, Default, Clone, Copy)]
pub struct StoreOutcome {
    pub memory: bool,
    pub disk: bool,
    pub bytes: u64,
}

/// ストリーミング保存の受け口。本文が届くたびに `write` し、正常終了なら `finish`。
pub struct StoreSink<'a> {
    pub(super) cache: &'a Cache,
    pub(super) key: CacheKey,
    pub(super) url: String,
    pub(super) meta: Meta,
    pub(super) ttl: Duration,
    pub(super) mem_buf: Option<Vec<u8>>,
    pub(super) disk: Option<DiskWriter<'a>>,
    pub(super) total: u64,
    pub(super) conn_id: usize,
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
            match w.finish(cache.tick()) {
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
