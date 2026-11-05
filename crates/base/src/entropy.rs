//! 起動ごとに変わる種 (T.34.74 で `records::salt` と `via::mark` の 2 か所の作り方を 1 つにした)。
//!
//! 外部クレートは足さない方針 (§0) なので、`RandomState` の種 + 時刻 + プロセス番号 + スタックの
//! アドレスを混ぜる (`RandomState` はプロセスごとに無作為な種を持つ)。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

/// 起動ごとに変わる 64 ビット。**呼ぶたびに別の値**を返す (`RandomState::new` は呼ぶたびに
/// 鍵を進め、時刻とスタックのアドレスも変わる)。
///
/// 覚えておくのは呼ぶ側 (`records` の塩と `via` の印はそれぞれ 1 回ずつ呼んで覚える)。
/// 1 つの値を使い回さないのは、`Via` に出す印から塩の一部を逆算されないようにするため。
pub(crate) fn boot_entropy() -> u64 {
    let anchor = 0u8;
    let mut h = RandomState::new().build_hasher();
    h.write_usize(&anchor as *const u8 as usize);
    h.write_u64(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::boot_entropy;

    #[test]
    fn each_call_gives_a_fresh_value() {
        assert_ne!(boot_entropy(), boot_entropy());
    }
}
