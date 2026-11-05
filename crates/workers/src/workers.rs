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
use std::time::{Duration, Instant};

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
    /// 上限に達したので待たせている仕事 (先入れ先出し)。**捨てない**。
    /// 積んだ時刻も一緒に持ち、取り出すときに待った時間を数える (T14.3 (3))
    queue: VecDeque<(Instant, Job)>,
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

    /// [`Workers::try_run`] と同じだが、**上限に達していたら上限の外のスレッドを 1 本起こして**
    /// `job` を実行する (待ち行列には積まない。T.28.0)。
    ///
    /// 上限の外で受けた接続 (`OVERFLOW_SLOTS` の 4 本。T13.2) 用。上限ぶんのワーカーが
    /// 黙った接続に握られていると、待ち行列に積んだ `/status` はそれが放されるまで動けない。
    /// 本数は呼び出し元の枠 (4 本) で縛られているので、ここでは数えない
    /// (`live` にも入れない = 上限の計算を変えない)。スレッドは仕事が終われば終わる。
    /// スレッドを起こせなかったときだけ `Err(job)` を返す (`run` と同じ性質)。
    pub fn run_outside(self: &Arc<Self>, job: Job) -> Result<(), Job> {
        let job = match self.try_run(job) {
            Ok(()) => return Ok(()),
            Err(job) => job,
        };
        // 仕事は起こしたあとで渡す (`spawn` と同じ形。起こせなければ手元に残る)
        let (tx, rx) = channel::<Job>();
        spawn_conn_thread(job, tx, move || {
            if let Ok(job) = rx.recv() {
                job();
            }
        })
    }

    /// `queue_when_full` が偽なら、上限に達したときに待ち行列へ積まず仕事を返す。
    fn submit(self: &Arc<Self>, job: Job, queue_when_full: bool) -> Result<(), Job> {
        let mut job = job;
        // 積んである送り口を新しい順に試す。相手が時間切れで終わっていれば send が失敗する
        loop {
            let mut inner = self
                .inner
                .locked_counted(&crate::sync::LOCK_CONTENDED[crate::sync::LOCK_WORKERS]);
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
                // 空き置き場へ戻る前にここから引き取る (同じ鍵の中で決めるので取りこぼさない)。
                // **ここを通るのは上限に達しているときだけ**なので、時計は熱い経路に乗らない
                inner.queue.push_back((Instant::now(), job));
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
        spawn_conn_thread(job, first, move || {
            // 席の番人はスレッドの中で作る。仕事がパニックしても巻き戻しで Drop が
            // 走り、席が戻る (戻さないと上限のぶんだけ席が消えたままになる)
            let live = Live(workers);
            worker_loop(&live.0, tx, rx);
        })
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
                take_queued(
                    inner
                        .queue
                        .pop_front()
                        .expect("just checked it is not empty"),
                )
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
            let Some(queued) = inner.queue.pop_front() else {
                return false;
            };
            let job = take_queued(queued);
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

/// 接続スレッド (`conn`、スタック [`STACK_SIZE`]) を 1 本起こし、`body` を回させる。
///
/// 起こせたら最初の仕事を `first` (`body` が受け取る受け口の送り口) へ渡す。起こせなければ
/// 仕事を呼び出し元へ返す (送るのは起こしたあと = 起こせなかった仕事は手元に残る)。
fn spawn_conn_thread(
    job: Job,
    first: Sender<Job>,
    body: impl FnOnce() + Send + 'static,
) -> Result<(), Job> {
    let spawned = thread::Builder::new()
        .name("conn".into())
        .stack_size(STACK_SIZE)
        .spawn(body);
    match spawned {
        Ok(_) => first.send(job).map_err(|e| e.0),
        Err(_) => Err(job),
    }
}

/// 待ち行列から取り出した仕事の「待った時間」を数えて中身を返す (T14.3 (3))。
fn take_queued((at, job): (Instant, Job)) -> Job {
    crate::sync::note_queue_wait(at.elapsed().as_millis().min(u64::MAX as u128) as u64);
    job
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
            let mut inner = workers
                .inner
                .locked_counted(&crate::sync::LOCK_CONTENDED[crate::sync::LOCK_WORKERS]);
            // 上限で待たせている仕事があれば、空き置き場へ戻らずそのまま次を取る
            if let Some(next) = inner.queue.pop_front() {
                drop(inner);
                job = take_queued(next);
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
mod tests;
