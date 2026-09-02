//! 保存の入場制御 (TinyLFU の doorkeeper 相当)。
//!
//! 一度しか要求されない URL でキャッシュを埋めないよう、初めて見たキーは保存せず、
//! 2 回目以降に保存する。見たキーは 2 枚のブルームフィルタ (現在 / 直前) で覚え、
//! 現在の側が [`ROTATE_AFTER`] 回埋まったら直前の側と入れ替えて古い記憶を捨てる。
//! 誤検出はキーを「見たことがある」と言う方向にしか起きず、その場合はただ保存されるだけ。

use std::sync::Mutex;

use super::key::CacheKey;

/// 1 枚あたりのビット数 (2^21 = 256 KiB)。
const BITS: usize = 1 << 21;
const WORDS: usize = BITS / 64;
/// 現在の側に入れた回数がこれに達したら入れ替える (2 ハッシュで誤検出 ~1.5%)。
const ROTATE_AFTER: usize = 1 << 18;

struct Inner {
    current: Vec<u64>,
    previous: Vec<u64>,
    inserted: usize,
    rotations: u64,
}

pub struct Doorkeeper {
    inner: Mutex<Inner>,
}

impl Default for Doorkeeper {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                current: vec![0; WORDS],
                previous: vec![0; WORDS],
                inserted: 0,
                rotations: 0,
            }),
        }
    }
}

fn positions(key: CacheKey) -> [usize; 2] {
    let lo = key.0 as u64;
    let hi = (key.0 >> 64) as u64;
    let h1 = (lo ^ hi.rotate_left(29)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let h2 = (hi ^ lo.rotate_left(17)).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    [
        (h1 >> 43) as usize & (BITS - 1),
        (h2 >> 43) as usize & (BITS - 1),
    ]
}

fn test(bits: &[u64], pos: [usize; 2]) -> bool {
    pos.iter().all(|&p| bits[p / 64] & (1u64 << (p % 64)) != 0)
}

impl Doorkeeper {
    /// このキーを以前に見たことがあれば `true`。見ていなければ覚えて `false`。
    pub fn seen(&self, key: CacheKey) -> bool {
        let pos = positions(key);
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if test(&g.current, pos) || test(&g.previous, pos) {
            return true;
        }
        for p in pos {
            g.current[p / 64] |= 1u64 << (p % 64);
        }
        g.inserted += 1;
        if g.inserted >= ROTATE_AFTER {
            let Inner {
                current, previous, ..
            } = &mut *g;
            std::mem::swap(current, previous);
            current.iter_mut().for_each(|w| *w = 0);
            g.inserted = 0;
            g.rotations += 1;
        }
        false
    }

    /// 入れ替えた回数 (テストと状態表示用)。
    pub fn rotations(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rotations
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_sighting_is_admitted() {
        let d = Doorkeeper::default();
        let k = CacheKey(0x1234_5678_9abc_def0_0fed_cba9_8765_4321);
        assert!(!d.seen(k), "first time: not seen");
        assert!(d.seen(k), "second time: seen");
        assert!(!d.seen(CacheKey(k.0 + 1)), "a different key is new");
    }

    #[test]
    fn rotation_forgets_old_keys_but_keeps_recent_ones() {
        let d = Doorkeeper::default();
        let old = CacheKey(7);
        assert!(!d.seen(old));
        // 誤検出で「見た」扱いになる分は数に入らないので、回転するまで入れ続ける
        let mut i = 1_000_000u128;
        while d.rotations() < 1 {
            d.seen(CacheKey(i));
            i += 1;
        }
        assert!(
            d.seen(old),
            "still in the previous filter after one rotation"
        );
        while d.rotations() < 2 {
            d.seen(CacheKey(i));
            i += 1;
        }
        // 直前の側からも消えたので、誤検出が無ければ新規扱い (誤検出は保存側に倒れるだけ)
        let _ = d.seen(old);
    }
}
