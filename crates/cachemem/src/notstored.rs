//! 「保存されない」と分かっている鍵の記憶 (同時ミスの合流を待ち損にしないため)。
//!
//! 合流 (collapsed forwarding、[`crate::inflight`]) は「1 本がオリジンから取ってきて保存し、
//! 待っていた側はキャッシュから受け取る」ための仕組みなので、**保存されない応答では
//! 待つだけ損**になる。待っていた側は leader が終わるまで寝て、起きてから結局自分で
//! オリジンへ行く。T11.2 の実測では、`Cache-Control: no-store` のオリジンに 8 並列で同じ URL を
//! 叩くと `futex` が 1.757 回/要求 になり、既定プロファイルの forward が `--lite` より
//! 25.2% 重かった (合流を外した計測専用ビルドでは 51.95 → 45.11 us)。
//!
//! そこで leader が「保存しなかった」で終わった鍵をブルームフィルタで覚え、次からは合流を
//! 通さない。**初回の突発 (thundering herd) では今までどおり合流する**ので、合流の本来の目的
//! (保存できる URL の同時ミスを 1 本にまとめる) はそのまま残る。
//!
//! **誤りはどちらに転んでも正しさを壊さない** (これがブルームフィルタを選べる理由):
//!
//! - 偽陽性 (保存できる鍵を「保存されない」と誤る): 合流しないので同時ミスが全員オリジンへ行く。
//!   応答は全員が正しく受け取り、保存も普通に行われる。**今までより遅くなるだけ**。
//! - 偽陰性 (覚えたことを忘れる): 今までどおり合流する。つまり変更前の挙動。
//!
//! **いつ忘れるか**: オリジンが `Cache-Control` を変えることがあるので覚えたままにはしない。
//! doorkeeper ([`crate::admission`]) と同じ 2 枚の入れ替えで、`PROXY_CACHE_TTL_SECS`
//! (既定 300 秒) と同じ周期、または現在の側に [`ROTATE_AFTER`] 個入ったら入れ替える。
//! 覚えている期間は最長で周期の 2 倍。**入れ替えた直後に突発が来れば合流に戻る** (想定内。
//! 待ち損を 1 周期に 1 回だけ払って、オリジンの方針変更に追従する)。
//!
//! 読む側 (要求ごと) はロックを取らない。ビットは [`AtomicU64`] で、読み書きとも `Relaxed`
//! でよい — 上のとおり見え方が古くても新しくても結果は「合流する / しない」の違いにしかならず、
//! 他のデータとの順序関係を必要としない。書く側 (leader が保存しなかったときだけ) は
//! `Mutex` で直列にする。

use crate::sync::LockExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::admission::positions;
use crate::key::CacheKey;

/// 1 枚あたりのビット数 (2^18 = 32 KiB。2 枚で 64 KiB)。
const BITS: usize = 1 << 18;
const WORDS: usize = BITS / 64;
/// 現在の側に入れた鍵がこれに達したら入れ替える (2 ハッシュで誤検出 ~5%)。
const ROTATE_AFTER: usize = 1 << 15;
/// 入れ替えの周期の下限と上限 (秒)。`PROXY_CACHE_TTL_SECS` が極端でも常識の範囲に収める。
const MIN_PERIOD: u64 = 10;
const MAX_PERIOD: u64 = 3600;

pub struct NotStored {
    /// 現在 / 直前の 2 枚。入れ替えは添字を切り替えるだけ (箱そのものは動かさない)
    filters: [Box<[AtomicU64]>; 2],
    /// いま書き込んでいる側の添字 (0 か 1)
    current: AtomicUsize,
    /// この epoch 秒になったら入れ替える。**0 は「1 つも覚えていない」**の意味で、
    /// 読む側はこの 1 回の読み込みだけで抜けられる (キャッシュを使わない構成では常に 0)
    rotate_at: AtomicU64,
    period: u64,
    /// 現在の側に入れた鍵の数 (書き込みの直列化も兼ねる)
    inserted: Mutex<usize>,
    rotations: AtomicU64,
}

fn empty_filter() -> Box<[AtomicU64]> {
    (0..WORDS)
        .map(|_| AtomicU64::new(0))
        .collect::<Vec<_>>()
        .into_boxed_slice()
}

fn test(bits: &[AtomicU64], pos: [usize; 2]) -> bool {
    pos.iter()
        .all(|&p| bits[p / 64].load(Ordering::Relaxed) & (1u64 << (p % 64)) != 0)
}

impl NotStored {
    /// `ttl_secs` は `PROXY_CACHE_TTL_SECS` (既定の TTL)。入れ替えの周期に使う。
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            filters: [empty_filter(), empty_filter()],
            current: AtomicUsize::new(0),
            rotate_at: AtomicU64::new(0),
            period: ttl_secs.clamp(MIN_PERIOD, MAX_PERIOD),
            inserted: Mutex::new(0),
            rotations: AtomicU64::new(0),
        }
    }

    /// この鍵は「保存されない」と覚えているか。覚えていなければ `false` = 今までどおり合流する。
    ///
    /// `now` は要求の処理で既に読んである epoch 秒 (この判定のために時計を読まない)。
    pub fn contains(&self, key: CacheKey, now: u64) -> bool {
        let at = self.rotate_at.load(Ordering::Relaxed);
        if at == 0 {
            // 1 つも覚えていない (キャッシュ無効・保存できるオリジンではここで終わる)
            return false;
        }
        if now >= at {
            self.rotate(now);
        }
        let pos = positions(key, BITS);
        let cur = self.current.load(Ordering::Relaxed) & 1;
        test(&self.filters[cur], pos) || test(&self.filters[cur ^ 1], pos)
    }

    /// leader が「保存しなかった」で終わった鍵を覚える。
    pub fn remember(&self, key: CacheKey, now: u64) {
        let pos = positions(key, BITS);
        let mut inserted = self.inserted.locked();
        let at = self.rotate_at.load(Ordering::Relaxed);
        if at != 0 && (now >= at || *inserted >= ROTATE_AFTER) {
            self.rotate_locked(&mut inserted, now);
        }
        let cur = self.current.load(Ordering::Relaxed) & 1;
        let bits = &self.filters[cur];
        if test(bits, pos) {
            return; // 既に覚えている (入れ替えの周期は「新しく入れた鍵の数」で決める)
        }
        for p in pos {
            bits[p / 64].fetch_or(1u64 << (p % 64), Ordering::Relaxed);
        }
        *inserted += 1;
        // 覚えている間だけ時計を動かす (空の間は読む側が 1 回の読み込みで抜けられる)
        if self.rotate_at.load(Ordering::Relaxed) == 0 {
            self.rotate_at
                .store(now.saturating_add(self.period), Ordering::Relaxed);
        }
    }

    /// 入れ替えた回数 (テストと状態表示用)。
    pub fn rotations(&self) -> u64 {
        self.rotations.load(Ordering::Relaxed)
    }

    fn rotate(&self, now: u64) {
        let mut inserted = self.inserted.locked();
        let at = self.rotate_at.load(Ordering::Relaxed);
        if at != 0 && now >= at {
            self.rotate_locked(&mut inserted, now);
        }
    }

    fn rotate_locked(&self, inserted: &mut usize, now: u64) {
        // 出ていく側にも 1 つも入っていなければ、入れ替えたあとは 2 枚とも空になる。
        // そのときは時計を止めて、読む側を最短の経路に戻す
        let empty_after = *inserted == 0;
        let next = (self.current.load(Ordering::Relaxed) & 1) ^ 1;
        // これから「現在」になる側 (= いままでの「直前」) を消してから切り替える。
        // 読む側は消している最中の側も見るが、消えた分は「覚えていない」に倒れるだけ
        for w in self.filters[next].iter() {
            w.store(0, Ordering::Relaxed);
        }
        self.current.store(next, Ordering::Relaxed);
        *inserted = 0;
        self.rotate_at.store(
            if empty_after {
                0
            } else {
                now.saturating_add(self.period)
            },
            Ordering::Relaxed,
        );
        self.rotations.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u128) -> CacheKey {
        CacheKey(n)
    }

    #[test]
    fn nothing_is_remembered_at_first() {
        let n = NotStored::new(300);
        assert!(!n.contains(key(1), 1_000));
        // 何も覚えていない間は時計も動いていない
        assert_eq!(n.rotations(), 0);
    }

    #[test]
    fn a_remembered_key_is_reported_and_others_are_not() {
        let n = NotStored::new(300);
        n.remember(key(1), 1_000);
        assert!(n.contains(key(1), 1_000));
        assert!(!n.contains(key(2), 1_000));
    }

    #[test]
    fn a_key_is_forgotten_after_two_rotations() {
        let n = NotStored::new(300);
        n.remember(key(1), 1_000);
        // 1 周期後: まだ「直前」の側にいる
        assert!(n.contains(key(1), 1_301));
        assert_eq!(n.rotations(), 1);
        // その間に別の鍵を覚えれば、次の入れ替えでも時計は止まらない
        n.remember(key(2), 1_301);
        // 2 周期後: 最初の鍵は消える (誤検出が無ければ)
        assert!(!n.contains(key(1), 1_602));
        assert!(n.contains(key(2), 1_602));
        assert_eq!(n.rotations(), 2);
    }

    #[test]
    fn the_clock_stops_once_everything_is_forgotten() {
        let n = NotStored::new(300);
        n.remember(key(7), 1_000);
        assert!(n.contains(key(7), 1_301)); // 1 回目の入れ替え
        assert!(!n.contains(key(7), 1_602)); // 2 回目で空になる
        assert_eq!(n.rotations(), 2);
        // もう入れ替えは起きない (読む側は最初の 1 回の読み込みで抜ける)
        assert!(!n.contains(key(7), 100_000));
        assert_eq!(n.rotations(), 2);
        // 覚え直せばまた動き出す
        n.remember(key(7), 100_000);
        assert!(n.contains(key(7), 100_000));
    }

    #[test]
    fn the_period_is_clamped() {
        assert_eq!(NotStored::new(0).period, MIN_PERIOD);
        assert_eq!(NotStored::new(u64::MAX).period, MAX_PERIOD);
        assert_eq!(NotStored::new(300).period, 300);
    }

    #[test]
    fn filling_the_current_side_rotates_too() {
        // 周期を待たなくても、入れた数が上限に達したら入れ替わる
        let n = NotStored::new(3600);
        // 誤検出で「もう覚えている」扱いになる分は数に入らないので、回転するまで入れ続ける
        let mut i = 1_000_000u128;
        let limit = i + 4 * ROTATE_AFTER as u128;
        while n.rotations() < 1 && i < limit {
            n.remember(key(i), 0);
            i += 1;
        }
        assert!(n.rotations() >= 1, "the fill limit rotates on its own");
    }
}
