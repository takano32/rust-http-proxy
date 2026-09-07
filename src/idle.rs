//! アイドルな接続を 1 本の監視スレッド (`epoll`) に預ける。
//!
//! これまでは「1 接続 = 1 スレッドが専任」だったので、次の要求を待っているだけの
//! 接続も OS スレッドを 1 本握っていた。実測では暇な接続 1000 本でスレッド 1003 本・
//! RSS 44 MB (28.3 kB/接続)。要求を処理していない接続はスレッドを手放し、
//! 読めるようになったら空いているワーカーへ戻す。
//!
//! 預かるのは 2 種類ある (どちらも同じ集合・同じ監視スレッドで見る):
//!
//! - **keep-alive の HTTP 接続** ([`Parked::Http`]): 記述子はクライアントの 1 本。
//! - **CONNECT トンネル** ([`Parked::Tunnel`]): 記述子は**クライアントとサーバーの 2 本**で、
//!   どちらが動いても引き上げる。両方向とも暇なトンネルだけを預かる (T8.1)。
//!
//! 預かっている間も `Conn` / `tunnel::Idle` は生きたまま (ソケットも持ち分も保持する) なので、
//! 記述子番号が別の接続に再利用されることはない。
//!
//! Linux 以外では [`IdleWatch::start`] が失敗し、呼び出し側は元の
//! 「スレッドがブロッキング read で待つ」経路に落ちる。

use std::io;
use std::sync::Arc;
use std::time::Instant;

use crate::Conn;
use crate::metrics::Metrics;
use crate::workers::Workers;

#[cfg(target_os = "linux")]
pub use linux::IdleWatch;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::collections::{BTreeSet, HashMap};
    use std::os::fd::RawFd;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;
    use std::thread;

    use crate::sync::LockExt;
    use crate::sys::{EPOLLIN, EPOLLRDHUP, Epoll, EpollEvent};
    use crate::tunnel;
    use crate::{log_debug, log_error, log_warn};

    /// 1 回の `epoll_wait` で受け取る事象の上限。
    const MAX_EVENTS: usize = 256;
    /// 期限を待つとき `epoll_wait` を最大どれだけ寝かせるか (ミリ秒)。
    ///
    /// 新しく預かる接続の期限はふつう既存のどれより後なので、預けるたびに監視スレッドを
    /// 起こす仕掛け (eventfd) は要らない。設定を読み直して keepalive が短くなった直後だけ
    /// 期限が前後しうるので、その取りこぼしをこの間隔で拾う。
    const MAX_WAIT_MS: i32 = 1000;
    /// 期限切れを 1 周で何本まで片づけるか。
    ///
    /// 監視スレッドが `epoll_wait` に戻るまでの間、読めるようになった接続は待たされる。
    /// TCP 接続の `close(2)` はこの機械で 1 本 10.8 us (Cortex-A78) / 64.0 us (A55)
    /// かかるので、上限なしに落とすと 4,096 本で 42 ms / 262 ms 止まる。計測済みの
    /// forward p99 は 1.33 ms なので、それを越えない 16 本 (A55 でも 1.0 ms) で切る。
    /// 残りは次の周回で片づく (期限が過ぎていれば [`IdleWatch::next_timeout`] が 0 を
    /// 返すのですぐ戻り、しかもその 1 回で溜まった EPOLLIN も拾えるので、
    /// 生きている接続が期限切れの列に割り込める)。
    const EXPIRE_BATCH: usize = 16;

    /// 預かっているもの。
    enum Parked {
        Http(Box<Conn>),
        Tunnel(Box<tunnel::Idle>),
    }

    impl Parked {
        /// epoll に入れる記述子。HTTP はクライアントの 1 本、トンネルは 2 本。
        fn fds(&self) -> ([RawFd; 2], usize) {
            match self {
                Parked::Http(conn) => ([conn.client_fd(), -1], 1),
                Parked::Tunnel(idle) => (idle.fds(), 2),
            }
        }

        /// ログ用の接続番号。
        fn id(&self) -> usize {
            match self {
                Parked::Http(conn) => conn.id(),
                Parked::Tunnel(idle) => idle.id(),
            }
        }

        fn is_tunnel(&self) -> bool {
            matches!(self, Parked::Tunnel(_))
        }
    }

    /// 預かっている 1 件。
    struct Entry {
        what: Parked,
        /// `deadlines` から引くために覚えておく (両者は必ず同じ集合に保つ)
        deadline: Instant,
    }

    struct Inner {
        /// 監視スレッドが生きているか。**この旗はロックの中だけで見ること。**
        /// 外の `AtomicBool` にすると「生きている」と読んだ直後に監視スレッドが
        /// 終わり、預けた接続が誰にも見られないまま残る隙間ができる。
        alive: bool,
        /// 預かっているもの。鍵は通し番号: **トンネルは記述子が 2 本ある**ので
        /// 記述子そのものは鍵にできない (2 本から同じ 1 件を引けること)
        entries: HashMap<u64, Entry>,
        /// (期限, 鍵)。期限の早い順に取り出す。`entries` と鍵の集合は常に一致する
        deadlines: BTreeSet<(Instant, u64)>,
        /// 次に配る鍵
        next_key: u64,
        /// そのうちトンネルの数 (`/status` の parked_tunnels)
        tunnels: usize,
    }

    /// アイドル接続の預かり所。プロセス全体で 1 つ持つ。
    pub struct IdleWatch {
        epoll: Epoll,
        inner: Mutex<Inner>,
        workers: Arc<Workers>,
        metrics: Arc<Metrics>,
    }

    /// 暇になったトンネルの預け先。
    ///
    /// 依存の向きが 本体 → `proxy-tunnel` なので、トンネル側はこの型を知らない
    /// (trait 越しに預けて、起きたら `tunnel::resume` で戻す)。
    impl tunnel::Park for IdleWatch {
        fn park(&self, idle: Box<tunnel::Idle>) -> Result<(), Box<tunnel::Idle>> {
            // 期限が作れない (Instant が溢れる) なら預けない
            let Some(deadline) = idle.deadline() else {
                return Err(idle);
            };
            match self.park_any(Parked::Tunnel(idle), deadline) {
                Ok(()) => Ok(()),
                Err(Parked::Tunnel(idle)) => Err(idle),
                Err(Parked::Http(_)) => unreachable!("we passed a tunnel in"),
            }
        }
    }

    impl IdleWatch {
        /// 監視スレッドを起こす。epoll が作れなければ `Err` (呼び出し側は旧経路へ)。
        pub fn start(workers: Arc<Workers>, metrics: Arc<Metrics>) -> io::Result<Arc<IdleWatch>> {
            let watch = Arc::new(IdleWatch {
                epoll: Epoll::new()?,
                inner: Mutex::new(Inner {
                    alive: true,
                    entries: HashMap::new(),
                    deadlines: BTreeSet::new(),
                    next_key: 1,
                    tunnels: 0,
                }),
                workers,
                metrics: Arc::clone(&metrics),
            });
            let w = Arc::clone(&watch);
            thread::Builder::new()
                .name("idle-watch".into())
                .spawn(move || {
                    // 監視スレッドが死んだら預かっている接続を全部閉じ、以後は
                    // park を断る (呼び出し側はスレッドで待つ元の経路に戻る)
                    if catch_unwind(AssertUnwindSafe(|| watch_loop(&w))).is_err() {
                        log_error!(None, "idle connection watcher panicked");
                    }
                    log_error!(
                        None,
                        "idle connection watcher stopped; falling back to one thread per connection"
                    );
                    w.shut_down();
                })?;
            metrics.park_watcher_alive.store(true, Ordering::Relaxed);
            Ok(watch)
        }

        /// keep-alive の接続を預ける。預かれたら `Ok(())`、断ったら `Err(conn)`
        /// (呼び出し側が続ける)。
        pub fn park(&self, conn: Box<Conn>, deadline: Instant) -> Result<(), Box<Conn>> {
            match self.park_any(Parked::Http(conn), deadline) {
                Ok(()) => Ok(()),
                Err(Parked::Http(conn)) => Err(conn),
                Err(Parked::Tunnel(_)) => unreachable!("we passed a connection in"),
            }
        }

        /// 預かっている数 (`/status` 用)。
        pub fn parked(&self) -> usize {
            self.inner.locked().entries.len()
        }

        /// 預かる。断ったら渡されたものをそのまま返す。
        fn park_any(&self, what: Parked, deadline: Instant) -> Result<(), Parked> {
            let (fds, n) = what.fds();
            let mut inner = self.inner.locked();
            if !inner.alive {
                return Err(what);
            }
            let key = inner.next_key;
            inner.next_key += 1;
            // **ロックを握ったまま epoll に足すこと。** 先に足すと、監視スレッドが
            // `entries` にまだ無い鍵の事象を見て取りこぼす
            if let Err(e) = self.add_fds(&fds[..n], key) {
                log_warn!(Some(what.id()), "cannot park this connection: {}", e);
                return Err(what);
            }
            if what.is_tunnel() {
                inner.tunnels += 1;
            }
            inner.deadlines.insert((deadline, key));
            inner.entries.insert(key, Entry { what, deadline });
            self.publish(&inner);
            Ok(())
        }

        /// 記述子をまとめて epoll に足す。途中で失敗したら足したぶんを戻す。
        fn add_fds(&self, fds: &[RawFd], key: u64) -> io::Result<()> {
            for (i, &fd) in fds.iter().enumerate() {
                if let Err(e) = self.epoll.add(fd, EPOLLIN | EPOLLRDHUP, key) {
                    for &added in &fds[..i] {
                        let _ = self.epoll.delete(added);
                    }
                    return Err(e);
                }
            }
            Ok(())
        }

        /// 記述子をまとめて epoll から外す (トンネルは 2 本とも)。
        fn del_fds(&self, fds: &[RawFd]) {
            for &fd in fds {
                let _ = self.epoll.delete(fd);
            }
        }

        fn publish(&self, inner: &Inner) {
            self.metrics
                .parked_connections
                .store(inner.entries.len(), Ordering::Relaxed);
            self.metrics
                .parked_tunnels
                .store(inner.tunnels, Ordering::Relaxed);
        }

        /// 事象が来たものを引き取る。`entries` に無ければ `None`。
        ///
        /// トンネルは記述子 2 本を同じ鍵で登録しているので、1 回の `epoll_wait` で
        /// 同じトンネルの事象が 2 つ来ることがある。**2 つ目はここで `None` になって
        /// 無視される** (引き取るときに鍵も記述子も両方消しているため)。
        fn take(&self, key: u64) -> Option<Parked> {
            let mut inner = self.inner.locked();
            let entry = inner.entries.remove(&key)?;
            // 期限も必ず一緒に消す。残すと期限の集合だけが際限なく育つ
            inner.deadlines.remove(&(entry.deadline, key));
            let (fds, n) = entry.what.fds();
            self.del_fds(&fds[..n]);
            if entry.what.is_tunnel() {
                inner.tunnels -= 1;
            }
            self.publish(&inner);
            Some(entry.what)
        }

        /// 期限が来たものを引き上げる。
        fn expire(&self, now: Instant) -> Vec<Parked> {
            let mut due = Vec::new();
            let mut inner = self.inner.locked();
            while due.len() < EXPIRE_BATCH {
                let Some(&(deadline, _)) = inner.deadlines.first() else {
                    break;
                };
                if deadline > now {
                    break;
                }
                let (_, key) = inner.deadlines.pop_first().expect("just peeked");
                if let Some(entry) = inner.entries.remove(&key) {
                    let (fds, n) = entry.what.fds();
                    self.del_fds(&fds[..n]);
                    if entry.what.is_tunnel() {
                        inner.tunnels -= 1;
                    }
                    due.push(entry.what);
                }
            }
            if !due.is_empty() {
                self.publish(&inner);
            }
            due
        }

        /// 次の `epoll_wait` に渡す待ち時間 (ミリ秒)。
        fn next_timeout(&self, now: Instant) -> i32 {
            let inner = self.inner.locked();
            match inner.deadlines.first() {
                Some(&(deadline, _)) => {
                    let ms = deadline.saturating_duration_since(now).as_millis();
                    (ms.min(MAX_WAIT_MS as u128)) as i32
                }
                None => MAX_WAIT_MS,
            }
        }

        /// 監視スレッドが終わるときに、預かっているものを全部閉じて以後の park を断る。
        fn shut_down(&self) {
            let mut inner = self.inner.locked();
            inner.alive = false;
            let left: Vec<Parked> = inner.entries.drain().map(|(_, e)| e.what).collect();
            inner.deadlines.clear();
            inner.tunnels = 0;
            self.publish(&inner);
            self.metrics
                .park_watcher_alive
                .store(false, Ordering::Relaxed);
            drop(inner);
            if !left.is_empty() {
                log_error!(None, "closing {} parked connections", left.len());
            }
        }

        /// 読めるようになった keep-alive 接続を空いているワーカーへ戻す。
        fn resume_conn(&self, conn: Box<Conn>) {
            let id = conn.id();
            // 渡せなかったときは仕事ごと返ってくる。落とせば Conn も落ちて接続が閉じる
            if self
                .workers
                .run(Box::new(move || crate::run_conn(conn)))
                .is_err()
            {
                log_error!(Some(id), "no thread to resume a parked connection; closing");
            }
        }

        /// 動きのあったトンネルを空いているワーカーへ戻す。
        fn resume_tunnel(&self, idle: Box<tunnel::Idle>) {
            let id = idle.id();
            if self
                .workers
                .run(Box::new(move || tunnel::resume(idle)))
                .is_err()
            {
                log_error!(Some(id), "no thread to resume a parked tunnel; closing");
            }
        }

        /// 期限切れのトンネルをワーカーで閉じる。
        ///
        /// 落とすだけで済む HTTP 接続と違って、トンネルは閉じるときにアクセスログと
        /// 統計 (ホスト別・接続元別のロック) を通る。監視スレッドでやると、その間
        /// 読めるようになった接続が待たされる。
        fn expire_tunnel(&self, idle: Box<tunnel::Idle>) {
            let id = idle.id();
            if self
                .workers
                .run(Box::new(move || tunnel::expire(idle)))
                .is_err()
            {
                // 仕事ごと返ってきた = ここで落ちる (閉じてログも統計も出る)
                log_error!(Some(id), "no thread to close an idle tunnel");
            }
        }
    }

    fn watch_loop(watch: &Arc<IdleWatch>) {
        let mut events = [EpollEvent::default(); MAX_EVENTS];
        log_debug!(None, "idle connection watcher started");
        loop {
            let timeout = watch.next_timeout(Instant::now());
            let n = match watch.epoll.wait(&mut events, timeout) {
                Ok(n) => n,
                Err(e) => {
                    log_error!(None, "epoll_wait failed: {}", e);
                    return;
                }
            };
            let mut closed = 0usize;
            for ev in &events[..n] {
                let Some(what) = watch.take(ev.token()) else {
                    continue;
                };
                match what {
                    Parked::Http(conn) => {
                        if ev.events() & EPOLLIN == 0 {
                            // 読めるものが無いのに知らせが来た = 相手が黙って切った。
                            // わざわざワーカーを起こして 0 バイトを読ませる必要はない
                            // (水準通知なので、データがあれば必ず EPOLLIN も立つ)
                            closed += 1;
                            drop(conn);
                            continue;
                        }
                        watch.resume_conn(conn);
                    }
                    // トンネルは EPOLLIN が無い (HUP / ERR だけの) ときもワーカーへ戻す。
                    // 閉じるのに shutdown・アクセスログ・統計が要るため
                    Parked::Tunnel(idle) => watch.resume_tunnel(idle),
                }
            }
            if closed > 0 {
                log_debug!(
                    None,
                    "closing {} connections the client went away on",
                    closed
                );
            }
            let expired = watch.expire(Instant::now());
            if !expired.is_empty() {
                log_debug!(
                    None,
                    "closing {} parked connections past their deadline",
                    expired.len()
                );
                for what in expired {
                    match what {
                        // ロックの外で落とす (close(2) の間、預けたいワーカーを待たせない)
                        Parked::Http(conn) => drop(conn),
                        Parked::Tunnel(idle) => watch.expire_tunnel(idle),
                    }
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub struct IdleWatch;

#[cfg(not(target_os = "linux"))]
impl IdleWatch {
    pub fn start(_workers: Arc<Workers>, _metrics: Arc<Metrics>) -> io::Result<Arc<IdleWatch>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "parking idle connections needs epoll (Linux)",
        ))
    }

    pub fn park(&self, conn: Box<Conn>, _deadline: Instant) -> Result<(), Box<Conn>> {
        Err(conn)
    }

    pub fn parked(&self) -> usize {
        0
    }
}
