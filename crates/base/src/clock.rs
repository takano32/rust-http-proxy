//! 壁時計 (epoch 秒)。キャッシュの鮮度、履歴、状態ファイルの時刻に使う。
//!
//! # 時計のポート (T35.1)
//!
//! 「いまは何秒か」はキャッシュの鮮度・`Age`・期限切れの掃除・再検証の延命が横断して読む
//! 関心ごと。これを [`Clock`] という 1 つのポート (Strategy) にまとめ、組み立ての根
//! (本番は `run` の起動、試験は `tests/common` の配線) で 1 回だけ実装を決めて `Cache` に注入する。
//! キャッシュの層とその上 (`http` の参照・再検証・配信、`/lookup`) は、自由関数の
//! [`now_epoch`] を直に呼ばず、注入された時計 (`Cache::now`) だけから「いま」を読む。
//!
//! - 本番は [`SystemClock`] (中身は今までと同じ [`now_epoch`])。
//! - 試験は [`ManualClock`] (手で [`ManualClock::advance`] する)。TTL の切れを実時間で
//!   待たずに作れる。
//!
//! 採らなかった案と理由:
//! - **大域の差し替え (ambient context) + RAII のガード**: 結合試験は 1 つのバイナリの中で
//!   並んで走り、どの試験のプロキシも同じプロセスにいる。大域の時計を差し替えると隣の試験の
//!   キャッシュまで時間が飛ぶ (ガードで戻しても、戻すまでの間は漏れる)。注入なら時計は
//!   その `Cache` だけのもので、`Cache` が落ちれば一緒に消える (範囲はプロキシの寿命そのもの)。
//! - **`Cache<C: Clock>` の総称**: 単相化で間接呼び出しは消えるが、型引数が `http`・`server`・
//!   `endpoints` の `&Cache` を持つ所すべてに伝わる。読みは 1 要求に数回で、仮想呼び出し
//!   1 回 (数 Ir) は `clock_gettime` の vDSO (数十 Ir) に比べて小さいので、`Arc<dyn Clock>` を
//!   起動時に 1 回だけ作る形にした (要求ごとの確保・鍵は無い)。
//! - **単調時計 (`Instant`) もポートに入れる**: 締め切り (ヘッダーの締め切り・最低転送速度・
//!   合流の待ち) は `poll`・`SO_RCVTIMEO`・`Condvar` の待ちと組で、カーネルが実時間で
//!   起こす。読む「いま」だけ偽っても待ちは縮まないので、今は入れない (`Instant::now()` のまま)。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 現在時刻 (epoch 秒)。時計が 1970 年より前なら 0。
pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// 時計のポート: 「いま」(epoch 秒) を読む口 (T35.1)。
pub trait Clock: Send + Sync {
    /// いまの epoch 秒。
    fn epoch(&self) -> u64;
}

/// 共有する時計 (組み立ての根で 1 回だけ作り、`Cache` などに配る)。
pub type SharedClock = Arc<dyn Clock>;

/// 本番の時計: OS の壁時計 ([`now_epoch`])。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    #[inline]
    fn epoch(&self) -> u64 {
        now_epoch()
    }
}

/// 本番の時計を共有の形で (起動のときに 1 回だけ呼ぶ)。
pub fn system() -> SharedClock {
    Arc::new(SystemClock)
}

/// 手で進める時計 (試験用。T35.1)。`Arc` で共有し、試験が [`advance`](Self::advance) した
/// ぶんだけ、注入された側の「いま」が進む。戻すことはできない (壁時計の後戻りは別の話)。
#[derive(Debug)]
pub struct ManualClock {
    secs: AtomicU64,
}

impl ManualClock {
    /// `start` 秒から始まる時計。
    pub fn at(start: u64) -> Arc<Self> {
        Arc::new(Self {
            secs: AtomicU64::new(start),
        })
    }

    /// いまの壁時計から始まる時計 (オリジンの `Date` と大きくずれないように)。
    pub fn starting_now() -> Arc<Self> {
        Self::at(now_epoch())
    }

    /// `by` だけ進める (秒未満は切り捨て)。
    pub fn advance(&self, by: Duration) {
        self.secs.fetch_add(by.as_secs(), Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn epoch(&self) -> u64 {
        self.secs.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_clock_reads_the_wall_clock() {
        let before = now_epoch();
        let got = SystemClock.epoch();
        assert!(got >= before && got <= now_epoch());
    }

    #[test]
    fn a_manual_clock_moves_only_when_advanced_and_is_shared() {
        let clock = ManualClock::at(1_000);
        let shared: SharedClock = clock.clone();
        assert_eq!(shared.epoch(), 1_000);
        assert_eq!(shared.epoch(), 1_000, "勝手に進まない");
        clock.advance(Duration::from_millis(2_900));
        assert_eq!(shared.epoch(), 1_002, "秒未満は切り捨て");
    }
}
