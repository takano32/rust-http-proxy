//! バイト数上限付き LRU インデックス。
//!
//! HashMap (キー → エントリ) と BTreeMap (最終利用シーケンス → キー) の 2 本立てで、
//! 参照 (touch) も最古の追い出し (pop_lru) も O(log n)。シーケンスはキャッシュ全体で
//! 単調増加するカウンタから採番するので衝突しない。

use std::collections::BTreeMap;

use super::key::{CacheKey, KeyMap};

pub trait LruEntry {
    fn size(&self) -> u64;
    fn last_used(&self) -> u64;
    fn set_last_used(&mut self, seq: u64);
}

pub struct Store<E> {
    entries: KeyMap<E>,
    order: BTreeMap<u64, CacheKey>,
    bytes: u64,
}

impl<E> Default for Store<E> {
    fn default() -> Self {
        Self {
            entries: KeyMap::default(),
            order: BTreeMap::new(),
            bytes: 0,
        }
    }
}

impl<E: LruEntry> Store<E> {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, key: CacheKey) -> Option<&E> {
        self.entries.get(&key)
    }

    /// 可変参照。`last_used` は変えないこと (順序は `touch` で更新する)。
    pub fn get_mut(&mut self, key: CacheKey) -> Option<&mut E> {
        self.entries.get_mut(&key)
    }

    /// 最終利用を `seq` に更新する。存在しなければ false。
    pub fn touch(&mut self, key: CacheKey, seq: u64) -> bool {
        let Some(e) = self.entries.get_mut(&key) else {
            return false;
        };
        let old = e.last_used();
        if old != seq {
            self.order.remove(&old);
            e.set_last_used(seq);
            self.order.insert(seq, key);
        }
        true
    }

    /// 挿入 (同じキーがあれば置き換え) し、置き換えた旧エントリを返す。
    pub fn insert(&mut self, key: CacheKey, mut entry: E, seq: u64) -> Option<E> {
        entry.set_last_used(seq);
        let size = entry.size();
        let old = self.entries.insert(key, entry);
        if let Some(old) = &old {
            self.order.remove(&old.last_used());
            self.bytes = self.bytes.saturating_sub(old.size());
        }
        self.order.insert(seq, key);
        self.bytes = self.bytes.saturating_add(size);
        old
    }

    pub fn remove(&mut self, key: CacheKey) -> Option<E> {
        let e = self.entries.remove(&key)?;
        self.order.remove(&e.last_used());
        self.bytes = self.bytes.saturating_sub(e.size());
        Some(e)
    }

    /// 最も長く使われていないエントリを取り出す。
    pub fn pop_lru(&mut self) -> Option<(CacheKey, E)> {
        loop {
            let (_, key) = self.order.pop_first()?;
            if let Some(e) = self.entries.remove(&key) {
                self.bytes = self.bytes.saturating_sub(e.size());
                return Some((key, e));
            }
        }
    }

    /// 条件に合うキーを集める (期限切れ掃除用)。
    pub fn keys_where(&self, pred: impl Fn(&E) -> bool) -> Vec<CacheKey> {
        self.entries
            .iter()
            .filter(|(_, e)| pred(e))
            .map(|(k, _)| *k)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct E {
        size: u64,
        seq: u64,
    }

    impl LruEntry for E {
        fn size(&self) -> u64 {
            self.size
        }
        fn last_used(&self) -> u64 {
            self.seq
        }
        fn set_last_used(&mut self, seq: u64) {
            self.seq = seq;
        }
    }

    fn k(n: u128) -> CacheKey {
        CacheKey(n)
    }

    #[test]
    fn evicts_least_recently_used_first() {
        let mut s: Store<E> = Store::default();
        s.insert(k(1), E { size: 10, seq: 0 }, 1);
        s.insert(k(2), E { size: 20, seq: 0 }, 2);
        s.insert(k(3), E { size: 30, seq: 0 }, 3);
        assert_eq!(s.bytes(), 60);
        assert!(s.touch(k(1), 4)); // 1 が最新になる
        assert!(!s.touch(k(9), 5));

        let (victim, e) = s.pop_lru().unwrap();
        assert_eq!(victim, k(2));
        assert_eq!(e.size, 20);
        assert_eq!(s.pop_lru().unwrap().0, k(3));
        assert_eq!(s.pop_lru().unwrap().0, k(1));
        assert!(s.pop_lru().is_none());
        assert_eq!(s.bytes(), 0);
        assert!(s.is_empty());
    }

    #[test]
    fn replace_and_remove_keep_bytes_consistent() {
        let mut s: Store<E> = Store::default();
        s.insert(k(1), E { size: 10, seq: 0 }, 1);
        let old = s.insert(k(1), E { size: 25, seq: 0 }, 2).unwrap();
        assert_eq!(old.size, 10);
        assert_eq!(s.bytes(), 25);
        assert_eq!(s.len(), 1);
        assert_eq!(s.remove(k(1)).unwrap().size, 25);
        assert_eq!(s.bytes(), 0);
        assert!(s.remove(k(1)).is_none());
    }

    #[test]
    fn keys_where_filters() {
        let mut s: Store<E> = Store::default();
        s.insert(k(1), E { size: 1, seq: 0 }, 1);
        s.insert(k(2), E { size: 2, seq: 0 }, 2);
        let big = s.keys_where(|e| e.size > 1);
        assert_eq!(big, vec![k(2)]);
    }
}
