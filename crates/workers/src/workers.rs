//! 接続スレッドの使い回しと、生きているスレッドの上限。
//!
//! 「1 接続 = 1 スレッドが専任する」構造はそのままに、**スレッドの生成と破棄だけ**を償却する。
//! 実測 (`strace -f -c`) では 1 接続あたり約 26 システムコールのうち約 16 がスレッドの
//! 生成・破棄 (`clone3` / `mmap` / `mprotect` / `munmap` / `madvise` / `sigaltstack` ×3 /
//! `rseq` / `set_robust_list` / `sched_getaffinity` / `gettid` / `prctl`) だった。
//! keep-alive が効いていれば要求あたりに薄まるが、1 接続 1 要求のクライアントでは全額かかる。
//!
//! 空いたスレッドは後入れ先出しで積み、`IDLE_TIMEOUT` 使われなければ自分で終わる。
//! 積んでおく上限は [`MAX_IDLE`]。スレッドローカルの中継バッファもそのまま引き継がれる。
//!
//! # 生きているスレッドの上限 (T10.5)
//!
//! **「空いている数」([`MAX_IDLE`]) と「生きている数」(`max_live`) は別物**。前者は
//! 「仕事が無いのに置いておくスレッドの数」で、後者は「同時に存在してよいスレッドの数」。
//! 上限が無かったころは、預けた 5,000 本のトンネルが一斉に切れると 1 本ずつワーカーへ
//! 渡すので**一時的に 4,500〜4,700 スレッド**まで増えていた (T8.1 の実測。閉じるのに shutdown・
//! アクセスログ・統計が要るので監視スレッドでは落とせない)。
//!
//! 上限に達したら**新しいスレッドを起こさず、仕事を待ち行列に置く**。**仕事は捨てない**。
//! 呼び出し元 (accept スレッドと監視スレッド) をその場で寝かせないのは、
//!
//! - 監視スレッドを止めると、預かっている接続の起床と期限切れが丸ごと止まる。
//! - accept スレッドを止めると、上限に達している間 `PROXY_MAX_CONNS` の 503 も返せない。
//!
//! ため。待ち行列に置いた仕事は、**仕事を終えたスレッドが空き置き場へ戻る前に引き取る**
//! (どちらも同じロックの中で決めるので、置いた仕事が誰にも拾われない隙間はできない)。
//!
//! # 上限の差し替え (T11.6)
//!
//! 上限は `.env` の再読込で変わる (`PROXY_MAX_CONNS` と同じ扱いに揃える)。当てるのは
//! [`Workers::set_limit`] で、呼ぶのは `serve` が接続ごとに引いている設定と食い違ったときだけ
//! (待ち受けへ `setsockopt` を当て直す T9.3 と同じ形)。**熱い経路に原子操作は増やさない** —
//! 接続ごとにかかるのは `Relaxed` の読みが 1 回だけで、書くのは値が変わったときだけ。

//!
//! # 「後でやればいい仕事」は積まずに返す ([`Workers::try_run`]、T11.3)
//!
//! 待たせてよいのは「誰かがその結果を待っている仕事」(接続の処理) だけ。裏側の再検証
//! (`http::refresh`) のような**後でやればいい仕事**は、上限に達しているときは待ち行列に
//! 積まずに呼び出し元へ返す。積むと、(1) 先に並んだぶんだけ新しい接続の処理が遅れ、
//! (2) 待っている間ずっとキャッシュ側の「再検証中」の印を握り続けるためで、
//! **捨てても正しさは崩れない** (その項目は次の要求で普通のミスとして取り直されるだけ)。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::sync::LockExt;
use crate::{log_debug, log_error, log_info};

type Job = Box<dyn FnOnce() + Send + 'static>;

/// 空きスレッドを積んでおく上限 (これを超えたぶんは仕事が終わり次第終了する)。
const MAX_IDLE: usize = 64;
/// 空きスレッドが何もしないまま終わるまでの時間。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// 接続スレッドのスタック (深い再帰はしないので既定の 8 MiB は要らない)。
const STACK_SIZE: usize = 256 * 1024;

#[derive(Default)]
struct Inner {
    /// 空いているスレッドへの送り口 (後入れ先出し)
    idle: Vec<Sender<Job>>,
    /// 上限に達したので待たせている仕事 (先入れ先出し)。**捨てない**
    queue: VecDeque<Job>,
    /// 生きているスレッドの数 (走っている + 空き置き場に積んである + これから起こす)
    live: usize,
}

/// 空きスレッドの置き場。待ち受けソケット全体で 1 つ共有する。
pub struct Workers {
    inner: Mutex<Inner>,
    /// 生きているスレッドの上限 (`0` で無制限)。決め方は `config::default_max_threads`。
    ///
    /// `.env` の再読込で変わる ([`Workers::set_limit`]) ので鍵の外に出してある。
    /// 読むのは `Inner` の鍵を持っているあいだ (数と突き合わせるため) と、
    /// 接続ごとの比較 ([`Workers::max_threads`]) の 2 か所だけ
    max_live: AtomicUsize,
}

impl Workers {
    /// `max_live` は生きているスレッドの上限 (`0` で無制限)。
    pub fn new(max_live: usize) -> Workers {
        Workers {
            inner: Mutex::new(Inner::default()),
            max_live: AtomicUsize::new(max_live),
        }
    }

    /// 空きスレッドがあればそれに、無ければ新しいスレッドを起こして `job` を実行する。
    /// 上限に達しているときは待ち行列に置く (**仕事は捨てない**)。
    /// スレッドを起こせなかったときだけ `Err(job)` を返す。
    ///
    /// **`Err` で仕事を呼び出し元へ返す性質は壊さないこと** (T9.6 で `OpenGuard` を
    /// 仕事の中に入れ、落ちたら同時接続数の持ち分が戻るようにしてある)。
    pub fn run(self: &Arc<Self>, job: Job) -> Result<(), Job> {
        self.submit(job, true)
    }

    /// [`Workers::run`] と同じだが、**上限に達しているときは待ち行列に積まず `Err(job)` を返す**。
    ///
    /// 「後でやればいい仕事」(裏側の再検証。T11.3) 用。呼び出し元は返ってきた仕事を落とし、
    /// その回の作業をあきらめる。`Err` で仕事が戻る性質は `run` と同じなので、
    /// 仕事に持たせた番人 (`OpenGuard` のような型) の `Drop` はここでも必ず走る。
    pub fn try_run(self: &Arc<Self>, job: Job) -> Result<(), Job> {
        self.submit(job, false)
    }

    /// `queue_when_full` が偽なら、上限に達したときに待ち行列へ積まず仕事を返す。
    fn submit(self: &Arc<Self>, job: Job, queue_when_full: bool) -> Result<(), Job> {
        let mut job = job;
        // 積んである送り口を新しい順に試す。相手が時間切れで終わっていれば send が失敗する
        loop {
            let mut inner = self.inner.locked();
            let Some(tx) = inner.idle.pop() else {
                let cap = self.max_threads();
                if cap == 0 || inner.live < cap {
                    // これから起こす 1 本ぶんの席を先に取る (取ってから鍵を放す)
                    inner.live += 1;
                    drop(inner);
                    return self.start(job);
                }
                if !queue_when_full {
                    // 後でやればいい仕事: 積まずに返す (呼び出し元があきらめる)
                    return Err(job);
                }
                // 上限に達した: スレッドは増やさず仕事を待たせる。仕事を終えたスレッドが
                // 空き置き場へ戻る前にここから引き取る (同じ鍵の中で決めるので取りこぼさない)
                inner.queue.push_back(job);
                return Ok(());
            };
            drop(inner);
            match tx.send(job) {
                Ok(()) => return Ok(()),
                Err(returned) => job = returned.0,
            }
        }
    }

    /// 席を 1 つ取ったあとの spawn。失敗したら席を戻して仕事を呼び出し元へ返す。
    fn start(self: &Arc<Self>, job: Job) -> Result<(), Job> {
        match self.spawn(job) {
            Ok(()) => Ok(()),
            Err(job) => {
                self.release();
                Err(job)
            }
        }
    }

    /// 新しいスレッドを起こし、仕事が終わるたびに自分を空き置き場へ戻すループに入れる。
    ///
    /// **`live` は増やしてあること** (この関数は数えない。減らすのは [`Workers::release`])。
    fn spawn(self: &Arc<Self>, job: Job) -> Result<(), Job> {
        let (tx, rx) = channel::<Job>();
        let first = tx.clone();
        let workers = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name("conn".into())
            .stack_size(STACK_SIZE)
            .spawn(move || {
                // 席の番人はスレッドの中で作る。仕事がパニックしても巻き戻しで Drop が
                // 走り、席が戻る (戻さないと上限のぶんだけ席が消えたままになる)
                let live = Live(workers);
                worker_loop(&live.0, tx, rx);
            });
        match spawned {
            Ok(_) => first.send(job).map_err(|e| e.0),
            Err(_) => Err(job),
        }
    }

    /// スレッドが 1 本消えた (または起こせなかった) ときに席を戻す。
    ///
    /// 戻した結果**待っている仕事の引き取り手が 1 本もいなくなった**ら、代わりを起こす。
    /// 起きるのは「仕事の中でパニックした」ときと「スレッドが作れなかった」とき
    /// (普通に終わるスレッドは、待ち行列が空でないかぎり [`worker_loop`] で引き取ってから戻る)。
    fn release(self: &Arc<Self>) {
        loop {
            let job = {
                let mut inner = self.inner.locked();
                debug_assert!(inner.live > 0, "席は取った数だけ戻す");
                inner.live = inner.live.saturating_sub(1);
                if inner.live > 0 || inner.queue.is_empty() {
                    return;
                }
                // 代わりの 1 本ぶんの席を取ってから起こす
                inner.live += 1;
                inner
                    .queue
                    .pop_front()
                    .expect("just checked it is not empty")
            };
            match self.spawn(job) {
                Ok(()) => return,
                Err(job) => {
                    // スレッドがもう作れない。この仕事はここで落ちる (接続が閉じ、
                    // 仕事が抱えている持ち分は Drop で戻る)。取った席は次の周回で戻す
                    drop(job);
                    log_error!(None, "cannot create a worker thread for a queued job");
                }
            }
        }
    }

    /// 積んである空きスレッドの数 (`/status` 用)。
    pub fn idle_count(&self) -> usize {
        self.inner.locked().idle.len()
    }

    /// 生きているスレッドの数 (走っている + 空き + これから起こす)。
    pub fn live_count(&self) -> usize {
        self.inner.locked().live
    }

    /// 上限に達して待たせている仕事の数。
    pub fn queued(&self) -> usize {
        self.inner.locked().queue.len()
    }

    /// 生きているスレッドの上限 (`0` で無制限)。
    ///
    /// 接続ごとに 1 回だけ引く (`serve` が設定と突き合わせる)。順序は要らないので `Relaxed`
    /// — 上限は「だいたい今の値」であればよく、1 接続ぶん遅れて効いても困らない。
    pub fn max_threads(&self) -> usize {
        self.max_live.load(Ordering::Relaxed)
    }

    /// 生きているスレッドの上限を差し替える (`.env` の再読込。T11.6)。
    ///
    /// **上げたとき**は待たせている仕事を新しい上限まですぐ起こす。空いたスレッドが出るのを
    /// 待たせると、keep-alive の接続 (仕事が長い) では事実上止まったままになるため。
    ///
    /// **下げたときは走っているスレッドを殺さない。** 新しいスレッドを起こさなくなるだけで、
    /// 仕事を終えたスレッドが空き置き場へ戻らずに終わっていく ([`worker_loop`])。
    /// なので**「生きている数 > 新しい上限」の状態がしばらく続く** (それぞれの接続が終わるまで)。
    /// 途中で切ると代理の最中の応答が壊れるので、自然に縮むのを待つ方を採る。
    pub fn set_limit(self: &Arc<Self>, max_live: usize) {
        let previous = self.max_live.swap(max_live, Ordering::Relaxed);
        if previous == max_live {
            return;
        }
        log_info!(
            None,
            "connection thread limit changed from {} to {} (0 = unlimited)",
            previous,
            max_live
        );
        // 上げたぶんの席で待ち行列を片づける (下げたときは 1 本目で false が返って終わる)
        while self.start_queued() {}
    }

    /// 待ち行列の先頭を新しいスレッドで始める。上限に当たった・待ち行列が空・
    /// スレッドを作れなかったときは `false`。
    fn start_queued(self: &Arc<Self>) -> bool {
        let job = {
            let mut inner = self.inner.locked();
            let cap = self.max_threads();
            if cap != 0 && inner.live >= cap {
                return false;
            }
            let Some(job) = inner.queue.pop_front() else {
                return false;
            };
            // これから起こす 1 本ぶんの席を先に取る (取ってから鍵を放す)
            inner.live += 1;
            job
        };
        // 失敗しても `start` が席を戻す。仕事はここで落ちる (接続が閉じ、持ち分は Drop で戻る)
        if self.start(job).is_err() {
            log_error!(None, "cannot create a worker thread for a queued job");
            return false;
        }
        true
    }
}

/// 生きているスレッド 1 本ぶんの席の番人。落ちると席が戻る (パニックしても通る)。
struct Live(Arc<Workers>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// 仕事を 1 つこなすたびに空き置き場へ戻り、`IDLE_TIMEOUT` 何も来なければ終わる。
fn worker_loop(workers: &Arc<Workers>, tx: Sender<Job>, rx: Receiver<Job>) {
    let mut job = match rx.recv() {
        Ok(j) => j,
        Err(_) => return,
    };
    loop {
        job();
        {
            let mut inner = workers.inner.locked();
            // 上限で待たせている仕事があれば、空き置き場へ戻らずそのまま次を取る
            if let Some(next) = inner.queue.pop_front() {
                drop(inner);
                job = next;
                continue;
            }
            // 上限が下がっていたら (T11.6) 空き置き場へ戻らずに終わる。走っている仕事は
            // 殺さないので、こうして**仕事を終えたスレッドから 1 本ずつ**自然に減らす
            let cap = workers.max_threads();
            if cap != 0 && inner.live > cap {
                log_debug!(
                    None,
                    "worker thread exiting ({} live over the limit of {})",
                    inner.live,
                    cap
                );
                return;
            }
            if inner.idle.len() >= MAX_IDLE {
                log_debug!(
                    None,
                    "worker thread exiting ({} already idle)",
                    inner.idle.len()
                );
                return;
            }
            inner.idle.push(tx.clone());
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

    /// `cond` が真になるまで 10 ms 刻みで最大 5 秒待つ。
    ///
    /// 混んでいる機械ではスレッドが起きるまでに 100 ms 以上かかることがあるので、
    /// 待ち時間を決め打ちしない。`#[track_caller]` を付けているので、時間切れの
    /// パニックは**呼んだ側の行**を指す。
    #[track_caller]
    fn wait_until(cond: impl Fn() -> bool) {
        for _ in 0..500 {
            if cond() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("condition did not hold within 5s");
    }

    /// `cond` が 100 ms のあいだ (10 ms ごとに 10 回) 真のままであることを確かめる。
    ///
    /// 「これ以上増えない」のような**起きないこと**を見るための待ち方。待てば真になる
    /// 条件は [`wait_until`] を使う。1 回寝てから 1 回見るのと違い、途中で破れても捕まえる。
    #[track_caller]
    fn stays_true(what: &str, cond: impl Fn() -> bool) {
        for _ in 0..10 {
            thread::sleep(Duration::from_millis(10));
            assert!(cond(), "{}", what);
        }
    }

    #[test]
    fn reuses_one_thread_for_sequential_jobs() {
        let w = Arc::new(Workers::new(0));
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
        assert_eq!(w.live_count(), 1, "生きているスレッドも 1 本");
    }

    #[test]
    fn survives_a_panicking_job() {
        // 仕事がパニックしてもプールは使えるままであること
        // (そのスレッドは死に、置き場に残った送り口は次に取り出した側が捨てる)
        let w = Arc::new(Workers::new(0));
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
        // 席が戻るのはパニックしたスレッドが落ちきってからで、混んでいる機械では
        // 100 ms では足りない (決め打ちで待つと落ちる)。戻るまで待つ
        wait_until(|| w.live_count() == 0);

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
        let w = Arc::new(Workers::new(0));
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

    /// 上限に達したらスレッドを増やさず待たせる。**仕事は 1 つも捨てない** (T10.5)。
    #[test]
    fn caps_live_threads_and_queues_the_rest() {
        let w = Arc::new(Workers::new(2));
        let started = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        let hold = Arc::new(Mutex::new(()));
        let guard = hold.locked();
        for _ in 0..8 {
            let (started, done, hold) =
                (Arc::clone(&started), Arc::clone(&done), Arc::clone(&hold));
            w.run(Box::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                let _held = hold.locked();
                done.fetch_add(1, Ordering::SeqCst);
            }))
            .unwrap_or_else(|_| panic!("could not get a thread"));
        }
        // 走れるのは上限の 2 本だけ。残りは待ち行列で待つ
        wait_until(|| started.load(Ordering::SeqCst) == 2);
        // 3 本目が走り出さないことは待っても真にならないので、しばらく見張る
        stays_true("上限を越えて走らない", || {
            started.load(Ordering::SeqCst) == 2
        });
        assert_eq!(w.live_count(), 2, "生きているスレッドは上限まで");
        assert_eq!(w.queued(), 6, "残りは待ち行列 (捨てない)");
        // 手を放せば残りも同じ 2 本で順に片づく
        drop(guard);
        wait_until(|| done.load(Ordering::SeqCst) == 8);
        assert_eq!(w.queued(), 0);
        assert!(w.live_count() <= 2, "増えていない: {}", w.live_count());
    }

    /// 「後でやればいい仕事」は上限に達したら積まずに返る (T11.3)。
    ///
    /// **仕事が呼び出し元へ戻ってくること**が要点で、戻ってきた仕事を落とせば、
    /// その中に入れた番人 (ここでは `Bell`) の `Drop` が走る (T9.6 と同じ性質)。
    #[test]
    fn try_run_gives_the_job_back_instead_of_queueing_it() {
        struct Bell(Arc<AtomicUsize>);
        impl Drop for Bell {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let w = Arc::new(Workers::new(1));
        let started = Arc::new(AtomicUsize::new(0));
        let hold = Arc::new(Mutex::new(()));
        let guard = hold.locked();
        {
            let (started, hold) = (Arc::clone(&started), Arc::clone(&hold));
            w.run(Box::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                let _held = hold.locked();
            }))
            .unwrap_or_else(|_| panic!("could not get a thread"));
        }
        wait_until(|| started.load(Ordering::SeqCst) == 1);

        // 上限は 1 で、その 1 本は塞がっている
        let dropped = Arc::new(AtomicUsize::new(0));
        let bell = Bell(Arc::clone(&dropped));
        let ran = Arc::new(AtomicUsize::new(0));
        let returned = {
            let ran = Arc::clone(&ran);
            w.try_run(Box::new(move || {
                let _bell = bell;
                ran.fetch_add(1, Ordering::SeqCst);
            }))
        };
        assert!(returned.is_err(), "上限に達したら仕事は戻ってくる");
        assert_eq!(w.queued(), 0, "待ち行列には積まない");
        assert_eq!(w.live_count(), 1, "スレッドも増えない");
        assert_eq!(dropped.load(Ordering::SeqCst), 0, "まだ落としていない");
        drop(returned);
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "落とせば番人の Drop が走る"
        );
        assert_eq!(ran.load(Ordering::SeqCst), 0, "仕事そのものは走らない");

        // 空きができれば `try_run` は普通に走る
        drop(guard);
        wait_until(|| w.idle_count() == 1);
        let (tx, rx) = mpsc::channel();
        w.try_run(Box::new(move || {
            let _ = tx.send(());
        }))
        .unwrap_or_else(|_| panic!("空きがあるのに走らせられなかった"));
        rx.recv().unwrap();
    }

    /// 上限を上げると、待たせていた仕事がその場で動き出す (T11.6)。
    #[test]
    fn raising_the_limit_starts_the_queued_jobs() {
        let w = Arc::new(Workers::new(1));
        let started = Arc::new(AtomicUsize::new(0));
        let hold = Arc::new(Mutex::new(()));
        let guard = hold.locked();
        for _ in 0..4 {
            let (started, hold) = (Arc::clone(&started), Arc::clone(&hold));
            w.run(Box::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                let _held = hold.locked();
            }))
            .unwrap_or_else(|_| panic!("could not get a thread"));
        }
        wait_until(|| started.load(Ordering::SeqCst) == 1);
        assert_eq!(w.queued(), 3, "上限 1 なので 3 つ待っている");

        w.set_limit(4);
        assert_eq!(w.max_threads(), 4);
        // 空いたスレッドを待たずに、待ち行列がその場で片づく
        wait_until(|| started.load(Ordering::SeqCst) == 4);
        assert_eq!(w.queued(), 0);
        assert_eq!(w.live_count(), 4);
        drop(guard);
    }

    /// `0` に戻すと無制限 (T10.5 以前の動き) に戻る (T11.6)。
    #[test]
    fn setting_the_limit_to_zero_means_unlimited() {
        let w = Arc::new(Workers::new(1));
        let started = Arc::new(AtomicUsize::new(0));
        let hold = Arc::new(Mutex::new(()));
        let guard = hold.locked();
        for _ in 0..6 {
            let (started, hold) = (Arc::clone(&started), Arc::clone(&hold));
            w.run(Box::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                let _held = hold.locked();
            }))
            .unwrap_or_else(|_| panic!("could not get a thread"));
        }
        wait_until(|| w.queued() == 5);
        w.set_limit(0);
        wait_until(|| started.load(Ordering::SeqCst) == 6);
        assert_eq!(w.queued(), 0);
        drop(guard);
    }

    /// 上限を下げても走っている仕事は殺さず、終わったスレッドから 1 本ずつ減る (T11.6)。
    #[test]
    fn lowering_the_limit_lets_the_running_jobs_finish() {
        let w = Arc::new(Workers::new(4));
        let started = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        let hold = Arc::new(Mutex::new(()));
        let guard = hold.locked();
        let job = |w: &Arc<Workers>| {
            let (started, done, hold) =
                (Arc::clone(&started), Arc::clone(&done), Arc::clone(&hold));
            w.run(Box::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                let _held = hold.locked();
                done.fetch_add(1, Ordering::SeqCst);
            }))
            .unwrap_or_else(|_| panic!("could not get a thread"));
        };
        for _ in 0..4 {
            job(&w);
        }
        wait_until(|| started.load(Ordering::SeqCst) == 4);

        w.set_limit(1);
        assert_eq!(w.live_count(), 4, "走っているスレッドは殺さない");
        assert_eq!(
            started.load(Ordering::SeqCst),
            4,
            "走っている仕事は止まらない"
        );
        // 生きている数が新しい上限を超えているので、次の仕事は待たされる
        job(&w);
        assert_eq!(w.queued(), 1);

        drop(guard);
        wait_until(|| done.load(Ordering::SeqCst) == 5);
        // 仕事を終えたスレッドは空き置き場へ戻らずに終わり、上限まで縮む
        wait_until(|| w.live_count() <= 1);
        assert!(w.idle_count() <= 1, "空きも上限まで: {}", w.idle_count());
    }

    /// 上限が 1 のとき、走っている仕事がパニックしても待ち行列が片づくこと (T10.5)。
    #[test]
    fn queued_jobs_survive_a_panicking_job() {
        let w = Arc::new(Workers::new(1));
        let (tx, rx) = mpsc::channel();
        let started = Arc::new(AtomicUsize::new(0));
        let hold = Arc::new(Mutex::new(()));
        let guard = hold.locked();
        {
            let (tx, started, hold) = (tx.clone(), Arc::clone(&started), Arc::clone(&hold));
            w.run(Box::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(0);
                // 後続が待ち行列に入るまで走り続け、そのうえでパニックする
                let _held = hold.locked();
                panic!("intentional panic in a worker job");
            }))
            .unwrap_or_else(|_| panic!("could not get a thread"));
        }
        rx.recv().unwrap();
        for i in 1..=2 {
            let (tx, started) = (tx.clone(), Arc::clone(&started));
            w.run(Box::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(i);
            }))
            .unwrap_or_else(|_| panic!("could not get a thread"));
        }
        assert_eq!(w.queued(), 2, "上限 1 なので 2 つとも待たされる");
        // パニックしたスレッドが抱えていた席は戻り、待ち行列は代わりのスレッドが片づける
        drop(guard);
        let mut got = vec![rx.recv().unwrap(), rx.recv().unwrap()];
        got.sort();
        assert_eq!(got, vec![1, 2], "待たせた仕事は捨てられない");
        assert_eq!(started.load(Ordering::SeqCst), 3);
    }
}
