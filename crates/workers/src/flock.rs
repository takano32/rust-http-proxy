//! `accept()` で待つスレッドの群れ (leader / follower)。
//!
//! これまでは待ち受け 1 本につき accept 専用のスレッドが 1 本あり、受けた接続を
//! `Box<dyn FnOnce>` にしてチャネルで [`Workers`](crate::workers::Workers) へ渡していた。
//! 渡すたびに `futex` が 2 回 (送る側が起こす + ワーカーの `recv_timeout` が待つ) 走り、
//! しかも別のコアでワーカーが起きるのを待つ (実測 2.00 回/接続。TASKS.md の T8.2)。
//!
//! ここでは **accept したスレッドがそのまま接続を処理する**。待ち受けには複数のスレッドが
//! 同時に `accept()` で入り、接続を取ったスレッドは「自分以外に待っている人が居ないとき
//! だけ」1 本補充してから処理に移る。ふつうは誰かが待っているので `futex` は 1 回も要らない。
//!
//! 群れの増やし方・減らし方:
//!
//! - 増やす: [`Flock::refill`] が空き置き場から 1 本起こし、居なければ上限まで新しく起こす。
//!   上限に当たったら `false` を返すので、呼び出し側はその接続だけ `Workers` へ渡して
//!   自分は `accept()` に戻る (**待ち受けが空にならないことを最優先**)。
//! - 減らす: 待ち受けに `SO_RCVTIMEO` が載っていると `accept()` は暇なとき時間切れで戻る。
//!   そのとき他に待っている人が居れば [`Flock::stand_by`] で空き置き場へ下がり、
//!   [`IDLE_TIMEOUT`] 起こされなければ自分で終わる (`Workers` と同じ後入れ先出し)。
//!
//! この置き場は**数と合図だけ**を持つ。スレッドの中身 (accept ループ) は呼び出し側にある。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use crate::sync::LockExt;

/// 群れに置くスレッドの上限 (`Workers` の `MAX_IDLE` と同じ)。
pub const MAX_THREADS: usize = 64;
/// 空きスレッドが起こされないまま終わるまでの時間。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// 置き場から取り出された後、起こす合図が届くのを待つ上限。
///
/// 時間切れと「取り出し」が同時に起きたときの取りこぼしを防ぐためだけの待ち。
/// 取り出した側は必ず送るので、ふつうは即座に届く。
const WAKE_GRACE: Duration = Duration::from_secs(1);
/// 群れのスレッドのスタック (`Workers` と同じ。深い再帰はしない)。
pub const STACK_SIZE: usize = 256 * 1024;

/// 1 つの待ち受けに張り付くスレッドの群れ。待ち受けごとに 1 つ持つ。
#[derive(Debug)]
pub struct Flock {
    /// いま `accept()` で待っているスレッドの数
    waiting: AtomicUsize,
    /// 群れのスレッドの数 (accept 待ち + 接続を処理中 + 空き置き場)
    threads: AtomicUsize,
    /// 空いているスレッドへの合図の口 (後入れ先出し。`(スレッドの番号, 送り口)`)
    idle: Mutex<Vec<(u64, Sender<()>)>>,
    max: usize,
    idle_timeout: Duration,
}

impl Default for Flock {
    fn default() -> Flock {
        Flock::new()
    }
}

/// 空き置き場に置いた口を、自分のものだと見分けるための番号。
static NEXT_STANDBY_ID: AtomicU64 = AtomicU64::new(1);

/// 群れのスレッドが空き置き場で待つときに使う合図の口 (スレッドごとに 1 つ)。
pub struct Standby {
    /// 置き場から自分の口を取り除くための番号
    id: u64,
    tx: Sender<()>,
    rx: Receiver<()>,
}

impl Default for Standby {
    fn default() -> Standby {
        Standby::new()
    }
}

impl Standby {
    pub fn new() -> Standby {
        let (tx, rx) = channel();
        Standby {
            id: NEXT_STANDBY_ID.fetch_add(1, Ordering::Relaxed),
            tx,
            rx,
        }
    }
}

impl Flock {
    pub fn new() -> Flock {
        Flock::with_limits(MAX_THREADS, IDLE_TIMEOUT)
    }

    /// 上限と空き置き場の待ち時間を指定して作る (試験用)。
    pub fn with_limits(max: usize, idle_timeout: Duration) -> Flock {
        Flock {
            waiting: AtomicUsize::new(0),
            threads: AtomicUsize::new(0),
            idle: Mutex::new(Vec::new()),
            // 0 本だと誰も accept しなくなる
            max: max.max(1),
            idle_timeout,
        }
    }

    /// 群れに 1 本加える (数の帳簿だけ。スレッドは呼び出し側が用意する)。
    /// 群れの 1 本目 (`serve` を呼んだスレッド) を数えるのに使う。
    pub fn enroll(&self) {
        self.threads.fetch_add(1, Ordering::Relaxed);
    }

    /// 群れから 1 本抜ける (パニックでスレッドが死んだときの後始末)。
    pub fn retire(&self) {
        self.threads.fetch_sub(1, Ordering::Relaxed);
    }

    /// `accept()` に入る直前に呼ぶ。
    pub fn enter_accept(&self) {
        self.waiting.fetch_add(1, Ordering::Relaxed);
    }

    /// `accept()` から戻った直後に呼ぶ。**自分を除いて**まだ待っている人の数を返す。
    pub fn leave_accept(&self) -> usize {
        self.waiting.fetch_sub(1, Ordering::Relaxed) - 1
    }

    /// 待ち受けに 1 本補充する。空き置き場に居れば起こし、居なければ `spawn` で増やす。
    ///
    /// `spawn` はスレッドを起こせたら `true` を返すこと。上限に当たった (または `spawn` が
    /// 失敗した) ときだけ `false` を返す。**このときだけ `futex` が要る。**
    pub fn refill(&self, spawn: impl FnOnce() -> bool) -> bool {
        // 積んである合図の口を新しい順に試す。相手が時間切れで終わっていれば send が失敗する
        loop {
            let Some((_, tx)) = self.idle.locked().pop() else {
                break;
            };
            if tx.send(()).is_ok() {
                return true;
            }
        }
        // 空きが無いので増やす。上限に当たったら諦める (呼び出し側は Workers へ渡す)
        let mut n = self.threads.load(Ordering::Relaxed);
        loop {
            if n >= self.max {
                return false;
            }
            match self
                .threads
                .compare_exchange_weak(n, n + 1, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(current) => n = current,
            }
        }
        if spawn() {
            return true;
        }
        self.threads.fetch_sub(1, Ordering::Relaxed);
        false
    }

    /// `accept()` から降りて空き置き場で待つ。起こされたら `true`。
    ///
    /// `false` を返したときはもう群れの一員ではない (呼び出し側はスレッドを終わらせる)。
    pub fn stand_by(&self, standby: &Standby) -> bool {
        self.idle.locked().push((standby.id, standby.tx.clone()));
        if standby.rx.recv_timeout(self.idle_timeout).is_ok() {
            return true;
        }
        // 時間切れ。自分の口がまだ置き場にあれば誰も起こしていないので抜けてよい。
        // 取り出された後なら合図が必ず来る (取り出した側は必ず送る) ので少しだけ待つ。
        // ここを見ないと「時間切れと取り出しが同時」のときに補充を 1 本落とす
        let taken = {
            let mut idle = self.idle.locked();
            match idle.iter().position(|(id, _)| *id == standby.id) {
                Some(i) => {
                    idle.remove(i);
                    false
                }
                None => true,
            }
        };
        if taken && standby.rx.recv_timeout(WAKE_GRACE).is_ok() {
            return true;
        }
        self.threads.fetch_sub(1, Ordering::Relaxed);
        false
    }

    /// いま `accept()` で待っているスレッドの数。
    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Relaxed)
    }

    /// 群れのスレッドの数 (accept 待ち + 処理中 + 空き置き場)。
    pub fn threads(&self) -> usize {
        self.threads.load(Ordering::Relaxed)
    }

    /// 空き置き場に積んである数 (`/status` 用)。
    pub fn idle_count(&self) -> usize {
        self.idle.locked().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;

    /// `cond` が真になるまで最大 2 秒待つ。
    fn wait_until(cond: impl Fn() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("condition did not hold within 2s");
    }

    #[test]
    fn leave_accept_counts_the_others() {
        let f = Flock::new();
        f.enter_accept();
        f.enter_accept();
        assert_eq!(f.waiting(), 2);
        assert_eq!(f.leave_accept(), 1, "自分を除いた数を返す");
        assert_eq!(f.leave_accept(), 0);
    }

    #[test]
    fn refill_wakes_a_thread_from_the_idle_pile() {
        let f = Arc::new(Flock::new());
        f.enroll();
        let (woke_tx, woke_rx) = mpsc::channel();
        let g = Arc::clone(&f);
        thread::spawn(move || {
            let standby = Standby::new();
            let _ = woke_tx.send(g.stand_by(&standby));
        });
        wait_until(|| f.idle_count() == 1);

        // 積んである口があるので spawn は呼ばれない
        assert!(f.refill(|| panic!("spawn must not be called")));
        assert!(woke_rx.recv().unwrap(), "起こされた側は true を受け取る");
        assert_eq!(f.threads(), 1, "起こしただけでは増えない");
        assert_eq!(f.idle_count(), 0);
    }

    #[test]
    fn refill_spawns_up_to_the_maximum() {
        let f = Flock::with_limits(3, IDLE_TIMEOUT);
        f.enroll();
        assert!(f.refill(|| true));
        assert!(f.refill(|| true));
        assert_eq!(f.threads(), 3);
        assert!(!f.refill(|| panic!("上限を超えて起こしてはいけない")));
        assert_eq!(f.threads(), 3, "断ったぶんは数えない");
    }

    #[test]
    fn refill_gives_back_the_slot_when_the_thread_cannot_start() {
        let f = Flock::with_limits(4, IDLE_TIMEOUT);
        f.enroll();
        assert!(!f.refill(|| false), "スレッドを起こせなければ false");
        assert_eq!(f.threads(), 1, "起こせなかったぶんは戻す");
    }

    #[test]
    fn stand_by_gives_up_after_the_idle_timeout() {
        let f = Arc::new(Flock::with_limits(8, Duration::from_millis(50)));
        f.enroll();
        let g = Arc::clone(&f);
        let h = thread::spawn(move || {
            let standby = Standby::new();
            g.stand_by(&standby)
        });
        assert!(!h.join().unwrap(), "時間切れなら false");
        assert_eq!(f.threads(), 0, "群れから抜ける");
        assert_eq!(
            f.idle_count(),
            0,
            "自分の口は置き場から取り除いてから終わる"
        );
    }
}
