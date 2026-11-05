//! L1 のバラスト (予算の未使用分を実際に確保しておく `Vec<u8>` の束。T34.100 で
//! `memory.rs` から分けた)。
//!
//! 64 MiB 単位の `Vec<u8>` を 0 以外で埋めて全ページをコミットさせたもの。キャッシュの
//! エントリが増えるとその分だけ解放し、常に `エントリ + バラスト <= 予算` を保つ。

use crate::log_warn;
use crate::sync::LockExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// バラストの割り当て単位。glibc の mmap 閾値の上限 (32 MiB) を超えるサイズにして、
/// 解放時に確実に OS へ返るようにする。
pub const BALLAST_CHUNK: usize = 64 * 1024 * 1024;

/// バラストの塊と、その合計 (鍵を取らずに読めるよう原子で持つ)。
pub(super) struct MemBallast {
    chunks: Mutex<Vec<Vec<u8>>>,
    bytes: AtomicU64,
    /// 一度でも確保に失敗したら、以後は伸ばさない
    alloc_failed: AtomicBool,
}

impl MemBallast {
    pub(super) fn new() -> Self {
        Self {
            chunks: Mutex::new(Vec::new()),
            bytes: AtomicU64::new(0),
            alloc_failed: AtomicBool::new(false),
        }
    }

    /// いま抱えているバイト数。
    pub(super) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// `cache_bytes + バラスト > capacity` の間、塊を解放する (解放は鍵を放してから)。
    pub(super) fn release(&self, cache_bytes: u64, capacity: u64) {
        if self.bytes() == 0 {
            return;
        }
        let mut freed = Vec::new();
        {
            let mut chunks = self.chunks.locked();
            while cache_bytes.saturating_add(self.bytes()) > capacity {
                let Some(chunk) = chunks.pop() else {
                    break;
                };
                self.bytes.fetch_sub(chunk.len() as u64, Ordering::Relaxed);
                freed.push(chunk);
            }
        }
        drop(freed);
    }

    /// `cache_bytes() + バラスト + 1 塊` が `limit()` に収まる間、塊を足す。戻り値は足したバイト数。
    ///
    /// 上限とエントリの合計は 1 塊ごとに読み直す (確保の間に予算もエントリも動くため)。
    pub(super) fn fill(&self, limit: impl Fn() -> u64, cache_bytes: impl Fn() -> u64) -> u64 {
        if self.alloc_failed.load(Ordering::Relaxed) {
            return 0;
        }
        let mut added = 0u64;
        loop {
            let capacity = limit();
            let owned = cache_bytes().saturating_add(self.bytes());
            if owned.saturating_add(BALLAST_CHUNK as u64) > capacity {
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
                let mut chunks = self.chunks.locked();
                chunks.push(chunk);
                self.bytes
                    .fetch_add(BALLAST_CHUNK as u64, Ordering::Relaxed);
            }
            added += BALLAST_CHUNK as u64;
        }
        added
    }
}
