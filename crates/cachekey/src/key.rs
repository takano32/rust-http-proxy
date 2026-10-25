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
    cache_key_variant(method, url, "")
}

/// URL のうち大小を区別しない先頭 (スキームと authority = `scheme://host:port`) の長さ (T.28.10 (a))。
///
/// RFC 3986 §6.2.2.1 はスキームとホストの大小を区別しない。`://` が無ければ 0 (何も畳まない)。
/// authority は最初の `/`・`?`・`#` の手前まで (`parse_origin` と同じ切り方)。
fn case_insensitive_prefix(url: &[u8]) -> usize {
    let Some(sep) = url.windows(3).position(|w| w == b"://") else {
        return 0;
    };
    let start = sep + 3;
    start
        + url[start..]
            .iter()
            .position(|&b| matches!(b, b'/' | b'?' | b'#'))
            .unwrap_or(url.len() - start)
}

/// `variant` (正規化した Accept-Encoding など) で表現を分けたキー。空文字なら `cache_key` と同じ。
///
/// URL のスキームとホストは**小文字に畳んで**から混ぜる (T.28.10 (a))。`http://LOCALHOST:P/x` が
/// `http://localhost:P/x` の項目に当たるように。もともと小文字の URL はバイト列が変わらないので、
/// 今までのキー (ディスクに残った項目) はそのまま使える。パスの大小は区別する (RFC 3986 のとおり)。
pub fn cache_key_variant(method: &str, url: &str, variant: &str) -> CacheKey {
    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0x9e37_79b9_7f4a_7c15;
    let separator: &[u8] = if variant.is_empty() { b"" } else { b"\x00" };
    let fold = case_insensitive_prefix(url.as_bytes());
    let url_bytes = url
        .as_bytes()
        .iter()
        .enumerate()
        .map(|(i, &b)| if i < fold { b.to_ascii_lowercase() } else { b });
    for b in method
        .as_bytes()
        .iter()
        .copied()
        .chain(b"|".iter().copied())
        .chain(url_bytes)
        .chain(separator.iter().copied())
        .chain(variant.as_bytes().iter().copied())
    {
        let b = &b;
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
        assert_eq!(cache_key_variant("GET", "http://example.com/a", ""), a);
        assert_ne!(cache_key_variant("GET", "http://example.com/a", "gzip"), a);
        let hex = a.to_string();
        assert_eq!(hex.len(), 32);
        assert_eq!(CacheKey::from_hex(&hex), Some(a));
        assert_eq!(a.shard() as u128, a.0 >> 120);
        assert!(CacheKey::from_hex("abc").is_none());
        assert!(CacheKey::from_hex("zz000000000000000000000000000000").is_none());
    }

    /// スキームとホストの大小は畳み、パスの大小は区別する (T.28.10 (a))。
    #[test]
    fn scheme_and_host_are_case_insensitive() {
        let a = cache_key("GET", "http://localhost:8080/x");
        assert_eq!(cache_key("GET", "http://LOCALHOST:8080/x"), a);
        assert_eq!(cache_key("GET", "HTTP://LocalHost:8080/x"), a);
        assert_ne!(cache_key("GET", "http://localhost:8080/X"), a);
        // `?` と `#` も authority の終わり
        assert_eq!(
            cache_key("GET", "http://EXAMPLE.com?Q"),
            cache_key("GET", "http://example.com?Q")
        );
        assert_ne!(
            cache_key("GET", "http://example.com?Q"),
            cache_key("GET", "http://example.com?q")
        );
        // 小文字の URL のキーは今までと同じ値 (ディスクに残った項目が引ける)
        assert_eq!(cache_key("GET", "http://example.com/a").to_string(), {
            let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
            let mut h2: u64 = 0x9e37_79b9_7f4a_7c15;
            for b in b"GET|http://example.com/a" {
                h1 ^= *b as u64;
                h1 = h1.wrapping_mul(0x0000_0100_0000_01b3);
                h2 = h2.rotate_left(7) ^ (*b as u64);
                h2 = h2.wrapping_mul(0x8864_3f65_e5a2_9d2b);
            }
            CacheKey(((h1 as u128) << 64) | h2 as u128).to_string()
        });
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
