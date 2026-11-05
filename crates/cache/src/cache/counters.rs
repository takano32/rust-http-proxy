//! キャッシュの数え物 (12 個の通算カウンタ)。
//!
//! T34.88 で `Cache` の `pub` の欄から移した。書くのはこのクレートの中 (と `Cache` の `note_*`) だけで、
//! 外へは読み口 ([`Counters::hits_mem`] など) だけを出す。どれも `Relaxed` の通算値で、
//! `/status`・`/metrics`・5 秒の標本が読む。

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Counters {
    pub(crate) hits_mem: AtomicU64,
    pub(crate) hits_disk: AtomicU64,
    pub(crate) misses: AtomicU64,
    pub(crate) stores: AtomicU64,
    pub(crate) revalidations: AtomicU64,
    /// 裏で完了した再検証 (304 での延命と差し替えの両方)
    pub(crate) background_revalidations: AtomicU64,
    /// 接続スレッドの上限に当たって捨てた裏側の再検証の数 (T11.3)。
    /// 捨てても正しさは崩れない (その項目は次の要求で普通のミスとして取り直される)
    pub(crate) revalidations_dropped: AtomicU64,
    /// 期限切れの表現をそのまま配信した回数 (grace 内・オリジン障害・待ち切れ)
    pub(crate) stale_served: AtomicU64,
    /// 同時ミスの合流で、オリジンへ行かずに済んだ要求の数
    pub(crate) coalesced: AtomicU64,
    /// 入場制御で見送った保存の数 (初回の要求)
    pub(crate) admission_rejected: AtomicU64,
    pub(crate) evictions: AtomicU64,
    pub(crate) bytes_served: AtomicU64,
}

impl Counters {
    #[inline]
    pub fn hits_mem(&self) -> u64 {
        self.hits_mem.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn hits_disk(&self) -> u64 {
        self.hits_disk.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn stores(&self) -> u64 {
        self.stores.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn revalidations(&self) -> u64 {
        self.revalidations.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn background_revalidations(&self) -> u64 {
        self.background_revalidations.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn revalidations_dropped(&self) -> u64 {
        self.revalidations_dropped.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn stale_served(&self) -> u64 {
        self.stale_served.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn coalesced(&self) -> u64 {
        self.coalesced.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn admission_rejected(&self) -> u64 {
        self.admission_rejected.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn bytes_served(&self) -> u64 {
        self.bytes_served.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 読み口はそれぞれ自分の欄を読む (取り違えが無い)。
    #[test]
    fn each_reader_reads_its_own_field() {
        let c = Counters::default();
        let fields = [
            &c.hits_mem,
            &c.hits_disk,
            &c.misses,
            &c.stores,
            &c.revalidations,
            &c.background_revalidations,
            &c.revalidations_dropped,
            &c.stale_served,
            &c.coalesced,
            &c.admission_rejected,
            &c.evictions,
            &c.bytes_served,
        ];
        for (i, f) in fields.iter().enumerate() {
            f.store(i as u64 + 1, Ordering::Relaxed);
        }
        let read = [
            c.hits_mem(),
            c.hits_disk(),
            c.misses(),
            c.stores(),
            c.revalidations(),
            c.background_revalidations(),
            c.revalidations_dropped(),
            c.stale_served(),
            c.coalesced(),
            c.admission_rejected(),
            c.evictions(),
            c.bytes_served(),
        ];
        assert_eq!(read, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    }
}
