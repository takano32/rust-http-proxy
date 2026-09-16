//! poison を無視してロックを取る補助。パニックしたスレッドが持っていたロックでも、
//! 統計や設定の読み書きは続けられる方がよい (壊れて困る不変条件は無い)。
//!
//! あわせて**ロックの取り合い**と**ワーカーの待ち行列の待ち**を数える口を持つ
//! (`/profile`。T14.3 (3))。数える側をここに置いてあるのは、4 つのロックが別々の
//! クレート (統計 / 名前解決 / 預かり所 / ワーカー) に居て、どれもこの層に依存するため。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};

/// 取り合いを数えるロックの名前 ([`LOCK_CONTENDED`] の添字の順)。
pub const LOCK_NAMES: [&str; 4] = ["stats", "dns", "park", "workers"];

/// ホスト別統計の表 (`Metrics::record` が要求ごとに取る)。
pub const LOCK_STATS: usize = 0;
/// 名前解決の表 (`dns::resolve_host` が要求ごとに取る)。
pub const LOCK_DNS: usize = 1;
/// 暇な接続の預かり所 (`IdleWatch`)。
pub const LOCK_PARK: usize = 2;
/// 接続スレッドの置き場と待ち行列 (`Workers`)。
pub const LOCK_WORKERS: usize = 3;

/// 「待たされた」回数 ([`LockExt::locked_counted`] が数える)。
pub static LOCK_CONTENDED: [AtomicU64; LOCK_NAMES.len()] =
    [const { AtomicU64::new(0) }; LOCK_NAMES.len()];

/// 待たされた回数の累計 ([`LOCK_NAMES`] の順)。
pub fn lock_contended() -> [u64; LOCK_NAMES.len()] {
    let mut out = [0u64; LOCK_NAMES.len()];
    for (o, c) in out.iter_mut().zip(LOCK_CONTENDED.iter()) {
        *o = c.load(Ordering::Relaxed);
    }
    out
}

/// ワーカーの待ち行列で待った仕事の数・合計 ms・最大 ms (累計)。
static QUEUE_WAITED: AtomicU64 = AtomicU64::new(0);
static QUEUE_MS_SUM: AtomicU64 = AtomicU64::new(0);
static QUEUE_MS_MAX: AtomicU64 = AtomicU64::new(0);
/// 窓ごとの最大 (`/profile` の標本が読んで 0 に戻す)。
static QUEUE_MS_WINDOW_MAX: AtomicU64 = AtomicU64::new(0);

/// 待ち行列に積まれていた仕事を 1 つ数える (`Workers` が取り出すときに呼ぶ)。
///
/// **積むのは上限に達しているときだけ**なので、熱い経路は 1 度もここを通らない。
pub fn note_queue_wait(ms: u64) {
    QUEUE_WAITED.fetch_add(1, Ordering::Relaxed);
    QUEUE_MS_SUM.fetch_add(ms, Ordering::Relaxed);
    QUEUE_MS_MAX.fetch_max(ms, Ordering::Relaxed);
    QUEUE_MS_WINDOW_MAX.fetch_max(ms, Ordering::Relaxed);
}

/// 待ち行列の累計 (件数, 合計 ms, 最大 ms)。
pub fn queue_totals() -> [u64; 3] {
    [
        QUEUE_WAITED.load(Ordering::Relaxed),
        QUEUE_MS_SUM.load(Ordering::Relaxed),
        QUEUE_MS_MAX.load(Ordering::Relaxed),
    ]
}

/// この窓の最大待ち (ms) を読んで 0 に戻す。
pub fn take_queue_window_max() -> u64 {
    QUEUE_MS_WINDOW_MAX.swap(0, Ordering::Relaxed)
}

pub trait LockExt<T> {
    fn locked(&self) -> MutexGuard<'_, T>;

    /// `try_lock` して、**取れなかったときだけ**数えてから待つ (T14.3 (3))。
    ///
    /// 取り合いが無ければ費用は [`LockExt::locked`] と同じ (`try_lock` も `lock` も
    /// 空いているロックは CAS 1 回で取る)。増えるのは分岐 1 つだけで、
    /// `fetch_add` を通るのは**本当に待たされたとき**に限る。
    fn locked_counted(&self, contended: &AtomicU64) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    #[inline]
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[inline]
    fn locked_counted(&self, contended: &AtomicU64) -> MutexGuard<'_, T> {
        match self.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            Err(TryLockError::WouldBlock) => {
                contended.fetch_add(1, Ordering::Relaxed);
                self.locked()
            }
        }
    }
}

pub trait RwLockExt<T> {
    fn read_locked(&self) -> RwLockReadGuard<'_, T>;
    fn write_locked(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
    #[inline]
    fn read_locked(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(|e| e.into_inner())
    }
    #[inline]
    fn write_locked(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// 空いていれば数えない (取り合いが無いときの費用は `locked` と同じ)。
    #[test]
    fn an_uncontended_lock_is_not_counted() {
        let n = AtomicU64::new(0);
        let m = Mutex::new(1);
        for _ in 0..100 {
            assert_eq!(*m.locked_counted(&n), 1);
        }
        assert_eq!(n.load(Ordering::Relaxed), 0);
    }

    /// 誰かが握っていたら 1 回数えてから待つ。
    #[test]
    fn a_contended_lock_is_counted_once() {
        let n = Arc::new(AtomicU64::new(0));
        let m = Arc::new(Mutex::new(0));
        let held = m.locked();
        let (m2, n2) = (Arc::clone(&m), Arc::clone(&n));
        let h = std::thread::spawn(move || {
            let mut g = m2.locked_counted(&n2);
            *g += 1;
        });
        // 相手が `try_lock` に失敗して数えるまで待つ
        while n.load(Ordering::Relaxed) == 0 {
            std::thread::yield_now();
        }
        drop(held);
        h.join().expect("待っていた側が取れること");
        assert_eq!(n.load(Ordering::Relaxed), 1);
        assert_eq!(*m.locked(), 1);
    }

    /// poison したロックでも数えずに中身が取れる。
    #[test]
    fn a_poisoned_lock_still_opens() {
        let n = AtomicU64::new(0);
        let m = Arc::new(Mutex::new(5));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _g = m2.locked();
            panic!("わざと落とす");
        })
        .join();
        assert_eq!(*m.locked_counted(&n), 5);
        assert_eq!(n.load(Ordering::Relaxed), 0);
    }

    /// 待ち行列の待ちは件数・合計・最大で溜まり、窓の最大だけ 0 に戻せる。
    #[test]
    fn the_queue_wait_totals_add_up() {
        let [w0, s0, _] = queue_totals();
        take_queue_window_max();
        note_queue_wait(7);
        note_queue_wait(3);
        let [w1, s1, m1] = queue_totals();
        assert_eq!(w1 - w0, 2);
        assert_eq!(s1 - s0, 10);
        assert!(m1 >= 7);
        assert_eq!(take_queue_window_max(), 7);
        assert_eq!(take_queue_window_max(), 0, "読んだら 0 に戻る");
    }
}
