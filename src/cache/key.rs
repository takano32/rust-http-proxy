//! キャッシュキー (128bit) とそのハッシュマップ用ハッシャ。

use std::collections::HashMap;
use std::fmt;
use std::hash::{BuildHasherDefault, Hasher};

/// メソッドと URL から作る 128bit のキー。16 進 32 桁で表示・ファイル名化する。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CacheKey(pub u128);

impl CacheKey {
    /// 16 進 32 桁の文字列 (ファイル名の stem) から復元する。
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 32 {
            return None;
        }
        u128::from_str_radix(s, 16).ok().map(Self)
    }

    /// ディスク上の分割ディレクトリ番号 (上位 8bit)。
    pub fn shard(self) -> u8 {
        (self.0 >> 120) as u8
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

impl fmt::Debug for CacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CacheKey({:032x})", self.0)
    }
}

/// FNV-1a を 2 系統走らせた 128bit 相当のキャッシュキー。
pub fn cache_key(method: &str, url: &str) -> CacheKey {
    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0x9e37_79b9_7f4a_7c15;
    for b in method
        .as_bytes()
        .iter()
        .chain(b"|".iter())
        .chain(url.as_bytes())
    {
        h1 ^= *b as u64;
        h1 = h1.wrapping_mul(0x0000_0100_0000_01b3);
        h2 = h2.rotate_left(7) ^ (*b as u64);
        h2 = h2.wrapping_mul(0x8864_3f65_e5a2_9d2b);
    }
    CacheKey(((h1 as u128) << 64) | h2 as u128)
}

/// キーは既にハッシュ値なので、HashMap 用ハッシャは上下 64bit を畳むだけにする。
#[derive(Default, Clone, Copy)]
pub struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0.rotate_left(5) ^ *b as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }

    fn write_u128(&mut self, i: u128) {
        self.0 = (i as u64) ^ ((i >> 64) as u64);
    }
}

pub type KeyMap<V> = HashMap<CacheKey, V, BuildHasherDefault<KeyHasher>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_distinct_and_round_trips_hex() {
        let a = cache_key("GET", "http://example.com/a");
        assert_eq!(a, cache_key("GET", "http://example.com/a"));
        assert_ne!(a, cache_key("GET", "http://example.com/b"));
        assert_ne!(a, cache_key("HEAD", "http://example.com/a"));
        let hex = a.to_string();
        assert_eq!(hex.len(), 32);
        assert_eq!(CacheKey::from_hex(&hex), Some(a));
        assert_eq!(a.shard() as u128, a.0 >> 120);
        assert!(CacheKey::from_hex("abc").is_none());
        assert!(CacheKey::from_hex("zz000000000000000000000000000000").is_none());
    }

    #[test]
    fn key_map_works_with_custom_hasher() {
        let mut m: KeyMap<u32> = KeyMap::default();
        m.insert(cache_key("GET", "http://a/"), 1);
        m.insert(cache_key("GET", "http://b/"), 2);
        assert_eq!(m.get(&cache_key("GET", "http://a/")), Some(&1));
        assert_eq!(m.len(), 2);
    }
}
