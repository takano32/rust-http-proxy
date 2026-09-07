//! アイドルな keep-alive 接続を 1 本の監視スレッド (`epoll`) に預ける。
//!
//! これまでは「1 接続 = 1 スレッドが専任」だったので、次の要求を待っているだけの
//! 接続も OS スレッドを 1 本握っていた。実測では暇な接続 1000 本でスレッド 1003 本・
//! RSS 44 MB (28.3 kB/接続)。要求を処理していない接続はスレッドを手放し、
//! 読めるようになったら空いているワーカーへ戻す。
//!
//! 預かっている間も `Conn` は生きたまま (ソケットも持ち分も保持する) なので、
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
    use crate::{log_debug, log_error, log_warn};

    /// 1 回の `epoll_wait` で受け取る事象の上限。
    const MAX_EVENTS: usize = 256;
    /// 期限を待つとき `epoll_wait` を最大どれだけ寝かせるか (ミリ秒)。
    ///
    /// 新しく預かる接続の期限はふつう既存のどれより後なので、預けるたびに監視スレッドを
    /// 起こす仕掛け (eventfd) は要らない。設定を読み直して keepalive が短くなった直後だけ
    /// 期限が前後しうるので、その取りこぼしをこの間隔で拾う。
    const MAX_WAIT_MS: i32 = 1000;

    /// 預かっている 1 接続。
    struct Parked {
        conn: Box<Conn>,
        /// `deadlines` から引くために覚えておく (両者は必ず同じ集合に保つ)
        deadline: Instant,
    }

    struct Inner {
        /// 監視スレッドが生きているか。**この旗はロックの中だけで見ること。**
        /// 外の `AtomicBool` にすると「生きている」と読んだ直後に監視スレッドが
        /// 終わり、預けた接続が誰にも見られないまま残る隙間ができる。
        alive: bool,
        conns: HashMap<RawFd, Parked>,
        /// (期限, fd)。期限の早い順に取り出す。`conns` と鍵の集合は常に一致する
        deadlines: BTreeSet<(Instant, RawFd)>,
    }

    /// アイドル接続の預かり所。プロセス全体で 1 つ持つ。
    pub struct IdleWatch {
        epoll: Epoll,
        inner: Mutex<Inner>,
        workers: Arc<Workers>,
        metrics: Arc<Metrics>,
    }

    impl IdleWatch {
        /// 監視スレッドを起こす。epoll が作れなければ `Err` (呼び出し側は旧経路へ)。
        pub fn start(workers: Arc<Workers>, metrics: Arc<Metrics>) -> io::Result<Arc<IdleWatch>> {
            let watch = Arc::new(IdleWatch {
                epoll: Epoll::new()?,
                inner: Mutex::new(Inner {
                    alive: true,
                    conns: HashMap::new(),
                    deadlines: BTreeSet::new(),
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

        /// 接続を預ける。預かれたら `Ok(())`、断ったら `Err(conn)` (呼び出し側が続ける)。
        pub fn park(&self, conn: Box<Conn>, deadline: Instant) -> Result<(), Box<Conn>> {
            let fd = conn.client_fd();
            let mut inner = self.inner.locked();
            if !inner.alive {
                return Err(conn);
            }
            // **ロックを握ったまま epoll に足すこと。** 先に足すと、監視スレッドが
            // `conns` にまだ無い fd の事象を見て取りこぼす
            if let Err(e) = self.epoll.add(fd, EPOLLIN | EPOLLRDHUP, fd as u64) {
                log_warn!(Some(conn.id()), "cannot park this connection: {}", e);
                return Err(conn);
            }
            debug_assert!(!inner.conns.contains_key(&fd), "fd is owned by one Conn");
            inner.deadlines.insert((deadline, fd));
            inner.conns.insert(fd, Parked { conn, deadline });
            self.publish(&inner);
            Ok(())
        }

        /// 預かっている接続の数 (`/status` 用)。
        pub fn parked(&self) -> usize {
            self.inner.locked().conns.len()
        }

        fn publish(&self, inner: &Inner) {
            self.metrics
                .parked_connections
                .store(inner.conns.len(), Ordering::Relaxed);
        }

        /// 事象が来た記述子を引き取る。`conns` に無ければ `None` (期限切れと競合した)。
        fn take(&self, fd: RawFd) -> Option<Box<Conn>> {
            let mut inner = self.inner.locked();
            let parked = inner.conns.remove(&fd)?;
            // 期限も必ず一緒に消す。残すと期限の集合だけが際限なく育つ
            inner.deadlines.remove(&(parked.deadline, fd));
            let _ = self.epoll.delete(fd);
            self.publish(&inner);
            Some(parked.conn)
        }

        /// 期限が来た接続を引き上げる。返した `Conn` を落とすと接続が閉じる。
        // Conn は大きいので箱のまま運ぶ (箱から出すと積み替えのコピーが要る)
        #[allow(clippy::vec_box)]
        fn expire(&self, now: Instant) -> Vec<Box<Conn>> {
            let mut due = Vec::new();
            let mut inner = self.inner.locked();
            while let Some(&(deadline, _)) = inner.deadlines.first() {
                if deadline > now {
                    break;
                }
                let (_, fd) = inner.deadlines.pop_first().expect("just peeked");
                if let Some(parked) = inner.conns.remove(&fd) {
                    let _ = self.epoll.delete(fd);
                    due.push(parked.conn);
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

        /// 監視スレッドが終わるときに、預かっている接続を全部閉じて以後の park を断る。
        fn shut_down(&self) {
            let mut inner = self.inner.locked();
            inner.alive = false;
            let left: Vec<Parked> = inner.conns.drain().map(|(_, p)| p).collect();
            inner.deadlines.clear();
            self.publish(&inner);
            self.metrics
                .park_watcher_alive
                .store(false, Ordering::Relaxed);
            drop(inner);
            if !left.is_empty() {
                log_error!(None, "closing {} parked connections", left.len());
            }
        }

        /// 読めるようになった接続を空いているワーカーへ戻す。
        fn resume(&self, conn: Box<Conn>) {
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
                let Some(conn) = watch.take(ev.token() as RawFd) else {
                    continue;
                };
                if ev.events() & EPOLLIN == 0 {
                    // 読めるものが無いのに知らせが来た = 相手が黙って切った。
                    // わざわざワーカーを起こして 0 バイトを読ませる必要はない
                    // (水準通知なので、データがあれば必ず EPOLLIN も立つ)
                    closed += 1;
                    drop(conn);
                    continue;
                }
                watch.resume(conn);
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
                    "closing {} idle connections past keep-alive",
                    expired.len()
                );
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
