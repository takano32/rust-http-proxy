//! 要求の経路の名前解決を**上限付きの別のスレッド群**に逃がし、締め切りで待つのをやめる (T.32.11)。
//!
//! `getaddrinfo` には締め切りもキャンセルも無く、要求を処理しているワーカーの上で同期に呼ぶと、
//! リゾルバが答えない間ずっと接続の枠とワーカー 1 本を握る (手元の再現: 応答しないリゾルバで
//! 相異なる名前 8 本を同時に出すと `PROXY_MAX_CONNS=8` の枠が 4 秒ふさがり、正当な要求が 503)。
//! そこで OS への問い合わせは `dns-resolve` スレッド (同時に [`limit`] 本まで) が行い、要求側は
//! [`timeout`] (`PROXY_DNS_TIMEOUT_SECS`、既定 10 秒、`0` で今までどおり自分で待つ) だけ待つ。
//!
//! - 締め切りを過ぎたら `dns_timeout` のエラーを返す (呼び出し側は 504 で枠を手放す)。はぐれた
//!   `getaddrinfo` は上限の内側で走り続け、**遅れて答えが出たら表に書く** (次の要求は当たる)。
//! - 上限の本数がもう走っているときは**待たずに** `dns_timeout` を返す (枠を食わない)。この失敗は
//!   負のキャッシュに載せない (その名前を引こうとすらしていないので)。
//! - IP リテラル・表の当たり・serve-stale はここを通らない (速い道は 1 命令も増えない)。
//!   裏の引き直し (`dns-refresh`) と canary も通さない (どちらも要求を待たせていない)。

use super::{Entry, MAX_ENTRIES, TABLE, evict_oldest, system_resolve};
use crate::log_warn;
use crate::sync::LockExt;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// `PROXY_DNS_TIMEOUT_SECS` の既定 (`PROXY_HEADER_TIMEOUT_SECS` の既定と同じ 10 秒)。
pub const TIMEOUT_DEFAULT: Duration = Duration::from_secs(10);
/// 同時に走らせる `getaddrinfo` の上限の既定 (起動時に `PROXY_MAX_THREADS` の 1/4 に当て直す)。
pub const LIMIT_DEFAULT: usize = 16;
/// 締め切り・上限で諦めたエラーの文言の頭 (`/errors` と `/log` に出る。`dns_temp` に数える)。
const TIMEOUT_PREFIX: &str = "dns_timeout";
/// 仕事の無い `dns-resolve` スレッドが終わるまでの時間。
const IDLE_EXIT: Duration = Duration::from_secs(30);
/// `dns-resolve` スレッドのスタック (接続スレッドと同じ。`getaddrinfo` はそこで動いていた)。
const STACK_SIZE: usize = 256 * 1024;

static TIMEOUT_SECS: AtomicU64 = AtomicU64::new(TIMEOUT_DEFAULT.as_secs());
static LIMIT: AtomicUsize = AtomicUsize::new(LIMIT_DEFAULT);
/// いま走っている (または待ち行列にいる) 問い合わせの数。
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// 締め切りを過ぎて諦めた回数 (`/status` の `dns.timeouts`)。
static TIMEOUTS: AtomicU64 = AtomicU64::new(0);
/// 上限の本数が走っていて待たずに断った回数 (`/status` の `dns.resolver_busy`)。
static BUSY: AtomicU64 = AtomicU64::new(0);
/// 諦めたあとに答えが出て、表に書いた回数 (`/status` の `dns.late_answers`)。
static LATE: AtomicU64 = AtomicU64::new(0);

/// 要求側が名前解決を待つ締め切り。`0` で締め切りなし (今までどおり自分のスレッドで引く)。
pub fn set_timeout(d: Duration) {
    TIMEOUT_SECS.store(d.as_secs(), Ordering::Relaxed);
}

pub fn timeout() -> Duration {
    Duration::from_secs(TIMEOUT_SECS.load(Ordering::Relaxed))
}

/// 同時に走らせる `getaddrinfo` の上限 (1 以上)。下げても走っているものは止めない。
pub fn set_limit(n: usize) {
    LIMIT.store(n.max(1), Ordering::Relaxed);
}

pub fn limit() -> usize {
    LIMIT.load(Ordering::Relaxed)
}

/// `PROXY_MAX_THREADS` から上限を決める (1/4、2 以上)。ワーカーより十分少なくしておけば、
/// 相異なる遅い名前がワーカーの数だけ来ても、上限を超えたぶんはすぐ 504 で枠を手放す。
pub fn limit_for_threads(max_threads: usize) -> usize {
    (max_threads / 4).max(2)
}

/// (締め切りで諦めた回数, 上限で断った回数, 遅れて表に書いた回数, いま走っている数)。
pub fn counters() -> (u64, u64, u64, usize) {
    (
        TIMEOUTS.load(Ordering::Relaxed),
        BUSY.load(Ordering::Relaxed),
        LATE.load(Ordering::Relaxed),
        IN_FLIGHT.load(Ordering::Relaxed),
    )
}

/// 締め切り・上限で諦めた名前解決のエラーか (呼び出し側が 502 でなく 504 を返すため)。
///
/// 負のキャッシュは文言だけを持ち直すので、型ではなく文言の頭で見る (エラーの経路だけ)。
pub fn is_timeout(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Other && e.to_string().starts_with(TIMEOUT_PREFIX)
}

/// 上限で断ったエラーか (負のキャッシュに載せない)。
pub(super) fn is_busy(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Other && e.to_string().starts_with("dns_timeout (busy)")
}

fn timeout_error(secs: u64) -> io::Error {
    // 「resolve host」を含めて `ErrCause::Dns` に、「Could not」で始めないで `dns_temp` に数える
    io::Error::other(format!(
        "{TIMEOUT_PREFIX}: could not resolve host within {secs}s"
    ))
}

fn busy_error(n: usize) -> io::Error {
    io::Error::other(format!(
        "{TIMEOUT_PREFIX} (busy): could not resolve host, {n} lookups already in flight"
    ))
}

struct Job {
    host: String,
    port: u16,
    tx: Sender<io::Result<Vec<IpAddr>>>,
}

struct Pool {
    queue: VecDeque<Job>,
    /// 仕事を待って眠っているスレッドの数
    waiting: usize,
    /// 生きている `dns-resolve` スレッドの数 (上限 [`limit`])
    spawned: usize,
}

static POOL: Mutex<Pool> = Mutex::new(Pool {
    queue: VecDeque::new(),
    waiting: 0,
    spawned: 0,
});
static WORK: Condvar = Condvar::new();

/// 要求の経路の名前解決 (ミスのときだけ呼ぶ)。締め切りが `0` なら今までどおり自分で引く。
pub(super) fn resolve(host: &str, port: u16) -> io::Result<Vec<IpAddr>> {
    let secs = TIMEOUT_SECS.load(Ordering::Relaxed);
    if secs == 0 {
        return system_resolve(host, port);
    }
    let cap = limit();
    if IN_FLIGHT.fetch_add(1, Ordering::Relaxed) >= cap {
        IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
        BUSY.fetch_add(1, Ordering::Relaxed);
        return Err(busy_error(cap));
    }
    let (tx, rx) = mpsc::channel();
    if !submit(Job {
        host: host.to_string(),
        port,
        tx,
    }) {
        // スレッドが作れなかった (資源切れ)。今までどおり自分で引く
        IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
        return system_resolve(host, port);
    }
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(r) => r,
        Err(RecvTimeoutError::Timeout) => {
            TIMEOUTS.fetch_add(1, Ordering::Relaxed);
            Err(timeout_error(secs))
        }
        // 引く側が死んだ (起きないはず)。締め切りと同じに扱う
        Err(RecvTimeoutError::Disconnected) => Err(timeout_error(secs)),
    }
}

/// 待ち行列に積み、要るならスレッドを起こすか作る。作れなかったら `false` (積んだ仕事は戻す)。
fn submit(job: Job) -> bool {
    let mut pool = POOL.locked();
    pool.queue.push_back(job);
    if pool.waiting > 0 {
        WORK.notify_one();
    }
    // 眠っている数より積んだ仕事が多ければ 1 本足す。上限に届いていれば、`IN_FLIGHT` が上限の
    // 内側なので、どれかのスレッドが今の仕事を終えて待ち行列を見に来る
    if pool.queue.len() > pool.waiting && pool.spawned < limit() {
        pool.spawned += 1;
        let spawned = thread::Builder::new()
            .name("dns-resolve".into())
            .stack_size(STACK_SIZE)
            .spawn(worker);
        if let Err(e) = spawned {
            pool.spawned -= 1;
            if pool.spawned == 0 {
                pool.queue.pop_back();
                log_warn!(None, "could not start a dns-resolve thread: {}", e);
                return false;
            }
        }
    }
    true
}

fn worker() {
    loop {
        let job = {
            let mut pool = POOL.locked();
            loop {
                if let Some(job) = pool.queue.pop_front() {
                    break job;
                }
                pool.waiting += 1;
                let (next, timed_out) = WORK
                    .wait_timeout(pool, IDLE_EXIT)
                    .unwrap_or_else(|e| e.into_inner());
                pool = next;
                pool.waiting -= 1;
                if timed_out.timed_out() && pool.queue.is_empty() {
                    pool.spawned -= 1;
                    return;
                }
            }
        };
        let result = system_resolve(&job.host, job.port);
        // 数を戻してから渡す (受け取った要求がすぐ次を頼んでも上限に数えられないように)
        IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
        if let Err(mpsc::SendError(Ok(addrs))) = job.tx.send(result) {
            // 待っていた要求はもう諦めた。答えは表に書いておく (次の要求が当たる)
            store_late(&job.host, addrs);
        }
    }
}

/// 諦めたあとに出た答えを表に書く (覚えている失敗も消す)。
fn store_late(host: &str, addrs: Vec<IpAddr>) {
    let key = host.to_ascii_lowercase();
    let now = Instant::now();
    let mut guard = TABLE.locked();
    let table = guard.get_or_insert_with(HashMap::new);
    if table.len() >= MAX_ENTRIES && !table.contains_key(&key) {
        evict_oldest(table);
    }
    let slot = table.entry(key).or_insert_with(|| Entry::empty(now, None));
    slot.addrs = addrs;
    slot.resolved_at = now;
    slot.failed_at = None;
    LATE.fetch_add(1, Ordering::Relaxed);
}
