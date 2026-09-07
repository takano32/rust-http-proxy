//! 接続スレッドの使い回し。
//!
//! 「1 接続 = 1 スレッドが専任する」構造はそのままに、**スレッドの生成と破棄だけ**を償却する。
//! 実測 (`strace -f -c`) では 1 接続あたり約 26 システムコールのうち約 16 がスレッドの
//! 生成・破棄 (`clone3` / `mmap` / `mprotect` / `munmap` / `madvise` / `sigaltstack` ×3 /
//! `rseq` / `set_robust_list` / `sched_getaffinity` / `gettid` / `prctl`) だった。
//! keep-alive が効いていれば要求あたりに薄まるが、1 接続 1 要求のクライアントでは全額かかる。
//!
//! 空いたスレッドは後入れ先出しで積み、`IDLE_TIMEOUT` 使われなければ自分で終わる。
//! 積んでおく上限は [`MAX_IDLE`]。スレッドローカルの中継バッファもそのまま引き継がれる。

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::log_debug;
use crate::sync::LockExt;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// 空きスレッドを積んでおく上限 (これを超えたぶんは仕事が終わり次第終了する)。
const MAX_IDLE: usize = 64;
/// 空きスレッドが何もしないまま終わるまでの時間。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// 接続スレッドのスタック (深い再帰はしないので既定の 8 MiB は要らない)。
const STACK_SIZE: usize = 256 * 1024;

/// 空きスレッドの置き場。待ち受けソケット全体で 1 つ共有する。
#[derive(Default)]
pub struct Workers {
    /// 空いているスレッドへの送り口 (後入れ先出し)
    idle: Mutex<Vec<Sender<Job>>>,
}

impl Workers {
    pub fn new() -> Workers {
        Workers::default()
    }

    /// 空きスレッドがあればそれに、無ければ新しいスレッドを起こして `job` を実行する。
    /// スレッドを起こせなかったときだけ `Err(job)` を返す。
    pub fn run(self: &Arc<Self>, job: Job) -> Result<(), Job> {
        let mut job = job;
        // 積んである送り口を新しい順に試す。相手が時間切れで終わっていれば send が失敗する
        loop {
            let Some(tx) = self.idle.locked().pop() else {
                break;
            };
            match tx.send(job) {
                Ok(()) => return Ok(()),
                Err(returned) => job = returned.0,
            }
        }
        self.spawn(job)
    }

    /// 新しいスレッドを起こし、仕事が終わるたびに自分を空き置き場へ戻すループに入れる。
    fn spawn(self: &Arc<Self>, job: Job) -> Result<(), Job> {
        let (tx, rx) = channel::<Job>();
        let first = tx.clone();
        let workers = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name("conn".into())
            .stack_size(STACK_SIZE)
            .spawn(move || worker_loop(workers, tx, rx));
        match spawned {
            Ok(_) => first.send(job).map_err(|e| e.0),
            Err(_) => Err(job),
        }
    }

    /// 積んである空きスレッドの数 (`/status` 用)。
    pub fn idle_count(&self) -> usize {
        self.idle.locked().len()
    }
}

/// 仕事を 1 つこなすたびに空き置き場へ戻り、`IDLE_TIMEOUT` 何も来なければ終わる。
fn worker_loop(workers: Arc<Workers>, tx: Sender<Job>, rx: Receiver<Job>) {
    let mut job = match rx.recv() {
        Ok(j) => j,
        Err(_) => return,
    };
    loop {
        job();
        {
            let mut idle = workers.idle.locked();
            if idle.len() >= MAX_IDLE {
                log_debug!(None, "worker thread exiting ({} already idle)", idle.len());
                return;
            }
            idle.push(tx.clone());
        }
        match rx.recv_timeout(IDLE_TIMEOUT) {
            Ok(j) => job = j,
            // 時間切れ・送り口が全部落ちた: 自分の送り口は置き場に残るが、
            // 次に取り出した側の send が失敗して捨てられる (遅延回収)
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

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
    fn reuses_one_thread_for_sequential_jobs() {
        let w = Arc::new(Workers::new());
        let (tx, rx) = mpsc::channel();
        let mut ids = Vec::new();
        for _ in 0..5 {
            let tx = tx.clone();
            w.run(Box::new(move || {
                let _ = tx.send(thread::current().id());
            }))
            .unwrap_or_else(|_| panic!("could not get a thread"));
            ids.push(rx.recv().unwrap());
            // 仕事が終わってから置き場に戻るまでの間に次を渡すと別スレッドになるので、
            // 戻ったのを見てから次へ (競合しないテストにする)
            wait_until(|| w.idle_count() == 1);
        }
        assert_eq!(ids.len(), 5);
        assert!(
            ids.windows(2).all(|p| p[0] == p[1]),
            "直列の仕事は同じスレッドが使い回される: {:?}",
            ids
        );
        assert_eq!(w.idle_count(), 1);
    }

    #[test]
    fn survives_a_panicking_job() {
        // 仕事がパニックしてもプールは使えるままであること
        // (そのスレッドは死に、置き場に残った送り口は次に取り出した側が捨てる)
        let w = Arc::new(Workers::new());
        let (tx, rx) = mpsc::channel();
        {
            let tx = tx.clone();
            w.run(Box::new(move || {
                let _ = tx.send(());
                panic!("intentional panic in a worker job");
            }))
            .unwrap_or_else(|_| panic!("could not get a thread"));
        }
        rx.recv().unwrap();
        thread::sleep(Duration::from_millis(100));

        // 次の仕事はちゃんと走る
        let (tx2, rx2) = mpsc::channel();
        for i in 0..3 {
            let tx2 = tx2.clone();
            w.run(Box::new(move || {
                let _ = tx2.send(i);
            }))
            .unwrap_or_else(|_| panic!("could not get a thread after the panic"));
            assert_eq!(rx2.recv().unwrap(), i);
        }
    }

    #[test]
    fn runs_concurrent_jobs_on_separate_threads() {
        let w = Arc::new(Workers::new());
        let (start_tx, start_rx) = mpsc::channel::<()>();
        let (id_tx, id_rx) = mpsc::channel();
        let hold = Arc::new(Mutex::new(()));
        let guard = hold.locked();
        for _ in 0..4 {
            let (id_tx, hold, start_tx) = (id_tx.clone(), Arc::clone(&hold), start_tx.clone());
            w.run(Box::new(move || {
                let _ = start_tx.send(());
                let _ = id_tx.send(thread::current().id());
                // 全員が同時に走っていることを確かめるため、解放されるまで待つ
                let _held = hold.locked();
            }))
            .unwrap_or_else(|_| panic!("could not get a thread"));
        }
        for _ in 0..4 {
            start_rx.recv().unwrap();
        }
        let mut ids: Vec<_> = (0..4).map(|_| id_rx.recv().unwrap()).collect();
        drop(guard);
        ids.sort_by_key(|i| format!("{:?}", i));
        ids.dedup();
        assert_eq!(ids.len(), 4, "同時に走る仕事は別スレッド");
    }
}
