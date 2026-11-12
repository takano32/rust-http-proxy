//! 転送中の最小の速さ (`PROXY_MIN_TRANSFER_BPS` / `PROXY_MIN_TRANSFER_WINDOW_SECS`。T29.34)。
//!
//! `PROXY_TIMEOUT_SECS` は**無通信**の締め切りで、1 バイトでも動けば待ち直す。1 B / 2 秒でしか
//! 読まない (slow-read) か送らない (slow-POST) 相手は、それだけで接続の枠を握り続けられる
//! (手元で `PROXY_MAX_CONNS=8` の枠が 8 本でふさがり、正常な要求が 503 になった)。
//!
//! ここで数えるのは**相手を待った時間**だけ: 書けずに待った (クライアントが読まない) 時間・
//! 本文を読もうとして待った時間。送るものが無くて待った時間 (オリジンが遅い・トンネルの
//! 片側が黙っている) は数えない (それは相手の遅さではない)。待った時間が窓
//! ([`MinRate::window`]) に達したら、その間に運べたバイトが `bps × 待った秒` に届いているかを
//! 見て、届いていなければ切る。届いていれば数え直す。
//!
//! 時計を読むのは「待ちうる 1 回」の前後だけで、呼ぶ側が選ぶ ([`RateGuard::time`] は
//! 小さな書きには使わない、トンネルは `poll` で書けるのを待つ回にだけ測る、など)。
//! 熱い経路 (1 KiB の応答を 1 回で書く forward) には 1 命令も足さない。

use std::cell::Cell;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 最小の速さの設定。`bps == 0` か窓が 0 なら切らない (前と同じ振る舞い)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinRate {
    /// これを下回ったら切る (バイト/秒)
    pub bps: u64,
    /// 待った時間がこれに達するたびに判定する
    pub window: Duration,
}

impl MinRate {
    /// 既定: 30 秒の窓で 1 KiB/s。
    pub const DEFAULT: MinRate = MinRate {
        bps: 1024,
        window: Duration::from_secs(30),
    };

    /// 切らない。
    pub const OFF: MinRate = MinRate {
        bps: 0,
        window: Duration::from_secs(30),
    };

    #[inline]
    pub fn is_on(&self) -> bool {
        self.bps > 0 && !self.window.is_zero()
    }

    /// 1 本の転送 (1 方向) ぶんの見張りを作る。
    pub fn guard(self, side: Side, conn_id: usize) -> RateGuard {
        RateGuard {
            rate: self,
            side,
            conn_id,
            stalled: Duration::ZERO,
            bytes: 0,
            queue_fd: -1,
            queued_at_start: 0,
            queued_before: Cell::new(UNREAD),
            cut: false,
        }
    }
}

impl Default for MinRate {
    fn default() -> Self {
        MinRate::DEFAULT
    }
}

/// どこで切ったか (`/status` の `slow_transfer_closed` の欄と同じ順)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// forward の応答をクライアントへ書く (slow-read)
    Response = 0,
    /// 要求の本文をクライアントから読む (slow-POST)
    RequestBody = 1,
    /// トンネルの書き側 (読まない相手へ書けずに待った)
    Tunnel = 2,
}

/// [`Side`] の数。
pub const SIDES: usize = 3;

/// `/status` の `slow_transfer_closed` の鍵 ([`Side`] の順)。
pub const SIDE_NAMES: [&str; SIDES] = ["response", "request_body", "tunnel"];

/// 切った本数 (起動からの通算。[`Side`] ごと)。
static CLOSED: [AtomicU64; SIDES] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

/// WARN を種類ごとに 1 分 1 行へ絞る門 (T28.9 と同じ形)。
static WARNED: [crate::log::Throttle; SIDES] = [
    crate::log::Throttle::new(),
    crate::log::Throttle::new(),
    crate::log::Throttle::new(),
];

/// 切った本数 ([`Side`] の順)。
pub fn closed_counts() -> [u64; SIDES] {
    [
        CLOSED[0].load(Ordering::Relaxed),
        CLOSED[1].load(Ordering::Relaxed),
        CLOSED[2].load(Ordering::Relaxed),
    ]
}

/// 遅すぎて切ったことを表すエラーの中身 (`is_slow` で見分ける)。
#[derive(Debug)]
pub(crate) struct TooSlow {
    pub bytes: u64,
    pub waited: Duration,
    pub min_bps: u64,
}

impl std::fmt::Display for TooSlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "transfer slower than {} B/s ({} B in {:.1}s of waiting)",
            self.min_bps,
            self.bytes,
            self.waited.as_secs_f64()
        )
    }
}

impl std::error::Error for TooSlow {}

/// 切ったあとの書き・読みのエラーの中身 (T36.33)。待たずに返し、本数も数えない。
#[derive(Debug)]
pub(crate) struct AlreadyCut {
    pub min_bps: u64,
}

impl std::fmt::Display for AlreadyCut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "transfer already closed as slower than {} B/s",
            self.min_bps
        )
    }
}

impl std::error::Error for AlreadyCut {}

/// [`RateGuard`] が切ったエラーか (切ったあとのエラーも含む)。種類は `TimedOut`
/// (締め切りの仲間として数えられる)。
#[cfg(test)]
pub(crate) fn is_slow(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(|inner| inner.is::<TooSlow>() || inner.is::<AlreadyCut>())
}

/// 1 方向の見張り。待った時間と、そのあいだに運んだバイトを積む。
#[derive(Debug, Clone)]
pub struct RateGuard {
    rate: MinRate,
    side: Side,
    conn_id: usize,
    /// 窓の中で相手を待った合計
    stalled: Duration,
    /// 同じ窓で書き先に渡した (受け取られた) バイト
    bytes: u64,
    /// 書き先のソケット (`-1` = 見ない)。渡したバイトから送信キューに残っている分を引く
    /// ([`RateGuard::watch_queue`])
    queue_fd: i32,
    /// 窓の始まり (最初の待ちに入る前) の送信キューの量
    queued_at_start: u64,
    /// 窓がまだ始まっていないときに、待ちに入る直前に読んだ送信キューの量 (`UNREAD` = 読んでいない)。
    /// 最初の待ちが積まれたら [`RateGuard::queued_at_start`] に移す。待ちの前に呼ばれる
    /// [`RateGuard::left`] (`&self`) からも置くので `Cell` (T36.29)
    queued_before: Cell<u64>,
    /// 一度切ったか。切ったあとの待ちうる 1 回は呼ばずにエラーで返す (T36.33)。前は切ると
    /// 窓を数え直すだけだったので、`BufWriter` の Drop の flush がもう一度無通信の締め切りまで
    /// 待ち、大きな書きならもう一度判定して 1 本を 2 回数えた
    cut: bool,
}

/// [`RateGuard::queued_before`] を読んでいない印。
const UNREAD: u64 = u64::MAX;

impl RateGuard {
    /// 切らない見張り (設定が `0` のとき・試験の道具)。
    pub fn off() -> RateGuard {
        MinRate::OFF.guard(Side::Response, 0)
    }

    #[inline]
    pub fn is_on(&self) -> bool {
        self.rate.is_on()
    }

    /// 書き先のソケットを覚える。判定のとき、渡したバイトから**送信キューに残っている分**
    /// (`SIOCOUTQ`) を引いて「相手に届いた量」で見る。
    ///
    /// 書けるようになるのを待つと、カーネルは送信バッファを広げながら受け取り続ける
    /// (自動調整。手元で読まない相手でも 3 秒待つごとに 100 KB 前後を受け取った) ので、
    /// 受け取られた量では遅い相手が速く見える。キューを読むのは窓の始まりと判定の 2 回だけ。
    ///
    /// **窓の始まりのキューは、最初の待ちに入る前に読む** (T36.29)。待ちのあいだにキューは
    /// 相手へ流れて減る (`poll` の `POLLOUT` はキューが送信バッファの 2/3 ほどに減るまで
    /// 返らない) ので、待ち終わってから読むと、最初の待ちの時間は数えるのにその間に届いた
    /// 分 (手元で 1 回 1.4 MB) を数えない。64 KiB を 69 ms ごとに読む相手 (0.94 MB/s) が
    /// 2 秒の窓で 0.72 MB (= 下限 384 KiB/s を割る) に見えて切られた。待つ前に読める口は
    /// [`RateGuard::time`] (自分で待つ) と [`RateGuard::left`] (`poll` で待つ呼ぶ側が
    /// 待つ直前に聞く) の 2 つ。どちらも通らずに積まれた待ちは、前と同じく待ち終わりに読む。
    pub fn watch_queue(&mut self, fd: i32) {
        self.queue_fd = fd;
    }

    /// 待ちうる 1 回 (`write` / `read` / `splice`) を測って積む。切るなら `Err`。
    ///
    /// 切らない設定なら時計を読まずにそのまま呼ぶ。`f` が失敗したらそのエラーを返す
    /// (待った時間は積む)。
    #[inline]
    pub fn time(&mut self, f: impl FnOnce() -> io::Result<usize>) -> io::Result<usize> {
        if !self.rate.is_on() {
            return f();
        }
        if self.cut {
            return Err(self.already_cut());
        }
        if self.stalled.is_zero() {
            // この待ちで窓が始まる: 待つ前のキューを読む (T36.29)
            self.read_queue_before();
        }
        let t0 = Instant::now();
        let r = f();
        let waited = t0.elapsed();
        let n = *r.as_ref().unwrap_or(&0);
        self.note(waited, n as u64)?;
        r
    }

    /// 待った時間 `waited` と運んだ `bytes` を積み、待ちが窓に達していれば判定する。
    ///
    /// 運んだだけ (`waited == 0`) のときは足し算 1 回で戻る。**バイトを数え始めるのは最初に
    /// 待ってから**: 待たずに運べた分 (送信バッファが空いていた最初の数 MB) まで窓に入れると、
    /// 最初の窓はそれだけで通ってしまい、遅い相手を切るのが 1 窓ぶん遅れる。
    #[inline]
    pub fn note(&mut self, waited: Duration, bytes: u64) -> io::Result<()> {
        if !self.rate.is_on() {
            return Ok(());
        }
        if waited.is_zero() {
            if !self.stalled.is_zero() {
                self.bytes = self.bytes.saturating_add(bytes);
            }
            return Ok(());
        }
        self.waited(waited, bytes)
    }

    /// [`RateGuard::note`] の待った回 (時計を読んだ回だけ来る)。
    #[cold]
    fn waited(&mut self, waited: Duration, bytes: u64) -> io::Result<()> {
        if self.cut {
            return Err(self.already_cut());
        }
        if self.stalled.is_zero() {
            // 窓の始まり: いまキューに残っている分は前の窓のもの。待つ前に読んであれば
            // それを使う (待ちのあいだに届いた分も数える。T36.29)
            let before = self.queued_before.replace(UNREAD);
            self.queued_at_start = if before == UNREAD {
                unsent(self.queue_fd)
            } else {
                before
            };
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.stalled += waited;
        if self.stalled < self.rate.window {
            return Ok(());
        }
        let need = self.rate.bps as u128 * self.stalled.as_micros() / 1_000_000;
        // 届いた量 = 渡した量 − (いまキューに残っている量 − 窓の始まりに残っていた量)
        let queued_now = unsent(self.queue_fd);
        let bytes = (self.bytes + self.queued_at_start).saturating_sub(queued_now);
        let stalled = self.stalled;
        self.bytes = 0;
        self.stalled = Duration::ZERO;
        if bytes as u128 >= need {
            return Ok(());
        }
        let slow = TooSlow {
            bytes,
            waited: stalled,
            min_bps: self.rate.bps,
        };
        self.cut = true;
        let side = self.side as usize;
        CLOSED[side].fetch_add(1, Ordering::Relaxed);
        // 遅い相手は数を決められるので、WARN は種類ごとに 1 分 1 行 (本数は `/status`)
        if let Some(more) = WARNED[side].pass(crate::log::WARN_EVERY_SECS) {
            crate::log_warn!(
                Some(self.conn_id),
                "closing a slow {}: {}{}",
                SIDE_NAMES[side].replace('_', " "),
                slow,
                more
            );
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, slow))
    }

    /// いまの窓で 1 度でも待ったか (待ち始めた相手は小さな書きも測る)。切ったあとも `true`
    /// (小さな書きも [`RateGuard::time`] に通して、待たずにエラーで返す。T36.33)。
    #[inline]
    pub(crate) fn stalling(&self) -> bool {
        !self.stalled.is_zero() || self.cut
    }

    /// 切ったあとの書き・読みのエラー (待たない・数えない。T36.33)。
    #[cold]
    fn already_cut(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            AlreadyCut {
                min_bps: self.rate.bps,
            },
        )
    }

    /// 窓の残り (この先これだけ待てば判定に届く)。切らない設定なら `None`。
    /// 待ちの上限を決められる呼ぶ側 (トンネルと splice の `poll`) が、**待つ直前に**聞く。
    ///
    /// 窓がまだ始まっていなければ、ここで送信キューを読んでおく (次に積まれる待ちで窓が
    /// 始まるので、その待ちの前の量になる。[`RateGuard::watch_queue`] を見よ。T36.29)。
    pub fn left(&self) -> Option<Duration> {
        if !self.rate.is_on() {
            return None;
        }
        if self.stalled.is_zero() {
            self.read_queue_before();
        }
        Some(self.rate.window.saturating_sub(self.stalled))
    }

    /// 窓が始まる待ちの前の送信キューを読んで置く (待つ回だけ来る)。
    #[cold]
    fn read_queue_before(&self) {
        if self.queue_fd >= 0 {
            self.queued_before.set(unsent(self.queue_fd));
        }
    }
}

/// ソケットの送信キューに残っているバイト (`ioctl(SIOCOUTQ)`。送ったがまだ確認されていない分を含む)。
/// `fd < 0` か読めなければ 0。
#[cfg(target_os = "linux")]
fn unsent(fd: i32) -> u64 {
    if fd < 0 {
        return 0;
    }
    unsafe extern "C" {
        fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
    }
    /// `SIOCOUTQ` (= `TIOCOUTQ`。x86_64 と aarch64 で同じ値)
    const SIOCOUTQ: std::ffi::c_ulong = 0x5411;
    let mut v: i32 = 0;
    // SAFETY: 有効かもしれない記述子と、`int` 1 つの書き込み先を渡すだけ。失敗は -1
    if unsafe { ioctl(fd, SIOCOUTQ, &mut v as *mut i32) } == 0 {
        v.max(0) as u64
    } else {
        0
    }
}

#[cfg(not(target_os = "linux"))]
fn unsent(_fd: i32) -> u64 {
    0
}

/// この大きさに満たない書きは測らない ([`GuardedWrite`])。
///
/// 1 KiB の応答は 1 回の `write` で出て、待つことは無い (ソケットの送信バッファに入る)。
/// 枠を握り続けるには送信バッファを越える大きな応答が要り、それは `BufWriter` の容量
/// (64 KiB) ごとの書きになるので、大きな書きだけ測れば足りる。いまの窓で 1 度でも
/// 待った相手は ([`RateGuard::stalling`]) 小さな書きも測る。
pub(crate) const TIMED_MIN_BYTES: usize = 16 * 1024;

/// クライアントへの書きを包んで、大きな書きの待ちを [`RateGuard`] に積む (`BufWriter` の中に置く)。
///
/// **見張りは最初に測る書きが来たときに作る** (1 KiB の応答のように 1 回の小さな書きで終わる
/// 要求は、作るのも送信キューの記述子を覚えるのも省く。熱い経路の命令数を増やさないため)。
pub struct GuardedWrite<'a> {
    inner: &'a std::net::TcpStream,
    rate: &'a MinRate,
    conn_id: usize,
    guard: Option<RateGuard>,
}

impl<'a> GuardedWrite<'a> {
    pub fn new(
        inner: &'a std::net::TcpStream,
        rate: &'a MinRate,
        conn_id: usize,
    ) -> GuardedWrite<'a> {
        GuardedWrite {
            inner,
            rate,
            conn_id,
            guard: None,
        }
    }

    pub fn get_ref(&self) -> &'a std::net::TcpStream {
        self.inner
    }

    /// 切る設定か (切らないなら時計も読まない)。
    #[inline]
    pub fn is_on(&self) -> bool {
        self.rate.is_on()
    }

    /// 見張り (無ければ作る。送信キューを見る記述子もここで覚える)。
    pub fn guard(&mut self) -> &mut RateGuard {
        let (rate, conn_id, inner) = (*self.rate, self.conn_id, self.inner);
        self.guard.get_or_insert_with(|| {
            let mut g = rate.guard(Side::Response, conn_id);
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                g.watch_queue(inner.as_raw_fd());
            }
            #[cfg(not(unix))]
            let _ = inner;
            g
        })
    }

    /// 測る書き (大きな書きか、もう待ち始めた相手)。
    #[inline(never)]
    fn write_timed(&mut self, buf: &[u8]) -> io::Result<usize> {
        use io::Write as _;
        let mut inner = self.inner;
        self.guard().time(|| inner.write(buf))
    }
}

impl io::Write for GuardedWrite<'_> {
    // 1 KiB の応答の書きはここを 1 回通るだけなので、呼び出しの往復も残さない
    #[inline(always)]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() < TIMED_MIN_BYTES && self.guard.as_ref().is_none_or(|g| !g.stalling()) {
            return self.inner.write(buf);
        }
        if !self.rate.is_on() {
            return self.inner.write(buf);
        }
        self.write_timed(buf)
    }

    #[inline(always)]
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Duration = Duration::from_secs(30);

    fn rate(bps: u64) -> MinRate {
        MinRate { bps, window: W }
    }

    #[test]
    fn slower_than_the_rate_is_cut_once_the_window_is_full() {
        // 切ると WARN をログの環に 1 行書くので、環を数える試験と並ばない (T18.4 と同じ鍵)
        let _ring = crate::sync::LockExt::locked(&crate::log::tests::RING_TEST_LOCK);
        let mut g = rate(1024).guard(Side::Response, 1);
        let before = closed_counts()[0];
        // 29 秒待って 10 B: まだ窓に満たない
        g.note(Duration::from_secs(29), 10).unwrap();
        // 合わせて 30 秒・20 B < 30 KiB → 切る
        let e = g.note(Duration::from_secs(1), 10).unwrap_err();
        assert!(is_slow(&e), "{e}");
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(closed_counts()[0] > before);
    }

    #[test]
    fn faster_than_the_rate_passes_and_the_window_starts_over() {
        let mut g = rate(1024).guard(Side::Tunnel, 1);
        // 30 秒待って 31 KiB: 1 KiB/s を少し上回る → 通して数え直す
        g.note(Duration::from_secs(15), 15 * 1024).unwrap();
        g.note(Duration::from_secs(15), 16 * 1024).unwrap();
        assert_eq!(g.left(), Some(W));
        // 待たずに運んだぶんは時間に入らない (速い相手は窓に届かない)
        g.note(Duration::ZERO, 1 << 30).unwrap();
        assert_eq!(g.left(), Some(W));
    }

    /// 待つ前に運んだバイト (空いていた送信バッファに入った分) は窓に入れない。
    #[test]
    fn bytes_before_the_first_wait_do_not_count() {
        // 切ると WARN をログの環に 1 行書くので、環を数える試験と並ばない (T18.4 と同じ鍵)
        let _ring = crate::sync::LockExt::locked(&crate::log::tests::RING_TEST_LOCK);
        let mut g = rate(1024).guard(Side::Response, 1);
        g.note(Duration::ZERO, 4 << 20).unwrap();
        g.note(Duration::from_secs(15), 100).unwrap();
        // 待ち始めてからの分は数える
        g.note(Duration::ZERO, 100).unwrap();
        let e = g.note(Duration::from_secs(15), 100).unwrap_err();
        assert!(e.to_string().contains("300 B"), "{e}");
    }

    #[test]
    fn zero_never_cuts_and_never_reads_the_clock() {
        let mut g = rate(0).guard(Side::RequestBody, 1);
        assert!(!g.is_on());
        g.note(Duration::from_secs(3600), 0).unwrap();
        assert_eq!(g.left(), None);
        assert_eq!(g.time(|| Ok(7)).unwrap(), 7);
        let off = MinRate {
            bps: 1024,
            window: Duration::ZERO,
        };
        assert!(!off.is_on());
    }

    #[test]
    fn time_passes_errors_through_and_counts_the_wait() {
        // 切ると WARN をログの環に 1 行書くので、環を数える試験と並ばない (T18.4 と同じ鍵)
        let _ring = crate::sync::LockExt::locked(&crate::log::tests::RING_TEST_LOCK);
        let rate = MinRate {
            bps: 1 << 20,
            window: Duration::from_millis(20),
        };
        let mut g = rate.guard(Side::RequestBody, 1);
        let e = g
            .time(|| {
                std::thread::sleep(Duration::from_millis(25));
                Ok(1)
            })
            .unwrap_err();
        assert!(is_slow(&e));
        // 切った見張りは次の 1 回を呼ばない (T36.33) ので、素通しは新しい見張りで見る
        let mut g = rate.guard(Side::RequestBody, 1);
        let e = g
            .time(|| Err(io::Error::from(io::ErrorKind::BrokenPipe)))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::BrokenPipe);
        assert!(!is_slow(&e));
    }

    /// 送信キューが詰まった接続 (書き手, 読み手) と、詰まったときのキューの量。
    #[cfg(target_os = "linux")]
    fn clogged() -> (std::net::TcpStream, std::net::TcpStream, u64) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let w = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (r, _) = l.accept().unwrap();
        refill(&w);
        w.set_nonblocking(true).unwrap();
        use std::os::fd::AsRawFd as _;
        let q = unsent(w.as_raw_fd());
        assert!(q > 0, "キューが詰まらない");
        (w, r, q)
    }

    /// 送信キューをもう書けなくなるまで詰める (記述子は待つ形に戻す)。
    #[cfg(target_os = "linux")]
    fn refill(mut w: &std::net::TcpStream) {
        use std::io::Write as _;
        w.set_nonblocking(true).unwrap();
        let chunk = [0u8; 64 * 1024];
        while w.write(&chunk).is_ok() {}
        w.set_nonblocking(false).unwrap();
    }

    /// 送信キューが `below` を下回るまで、読み手から読む (= 待ちのあいだに相手へ届く)。
    #[cfg(target_os = "linux")]
    fn drain_until(w: &std::net::TcpStream, r: &mut std::net::TcpStream, below: u64) {
        use std::io::Read as _;
        use std::os::fd::AsRawFd as _;
        let mut buf = vec![0u8; 256 * 1024];
        let t0 = Instant::now();
        while unsent(w.as_raw_fd()) >= below {
            assert!(t0.elapsed() < Duration::from_secs(10), "キューが減らない");
            let _ = r.read(&mut buf).unwrap();
        }
    }

    /// `poll` で待つ呼ぶ側 (splice・トンネル): 最初の待ちのあいだに相手へ届いた分も数える
    /// (T36.29)。前は待ち終わってからキューを読んだので、届いた分が 0 に見えて切った。
    #[test]
    #[cfg(target_os = "linux")]
    fn what_drains_during_the_first_poll_wait_counts() {
        use std::os::fd::AsRawFd as _;
        let _ring = crate::sync::LockExt::locked(&crate::log::tests::RING_TEST_LOCK);
        let (w, mut r, q) = clogged();
        let mut g = MinRate {
            bps: 100_000,
            window: Duration::from_millis(10),
        }
        .guard(Side::Response, 1);
        g.watch_queue(w.as_raw_fd());
        // 待つ直前に窓の残りを聞き、待つあいだに半分が届く
        assert_eq!(g.left(), Some(Duration::from_millis(10)));
        drain_until(&w, &mut r, q / 2);
        // 窓いっぱい待って、書けたのは 0 B。届いたのは q/2 以上 → 10 ms で 1000 B を上回る
        g.note(Duration::from_millis(10), 0).unwrap();
        assert_eq!(g.left(), Some(Duration::from_millis(10)), "通して数え直す");
    }

    /// 自分で待つ書き ([`RateGuard::time`]) も、待つ前のキューから数える (T36.29)。
    #[test]
    #[cfg(target_os = "linux")]
    fn what_drains_during_the_first_timed_write_counts() {
        use std::os::fd::AsRawFd as _;
        let _ring = crate::sync::LockExt::locked(&crate::log::tests::RING_TEST_LOCK);
        let (w, mut r, q) = clogged();
        let mut g = MinRate {
            bps: 100_000,
            window: Duration::from_millis(10),
        }
        .guard(Side::Response, 1);
        g.watch_queue(w.as_raw_fd());
        let n = g
            .time(|| {
                drain_until(&w, &mut r, q / 2);
                std::thread::sleep(Duration::from_millis(10));
                Ok(0)
            })
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(g.left(), Some(Duration::from_millis(10)), "通して数え直す");
    }

    /// 待つ前に読んでいなければ、前と同じく待ち終わりのキューから数える (届いた分は見えない)。
    #[test]
    #[cfg(target_os = "linux")]
    fn without_asking_before_the_wait_the_queue_is_read_after_it() {
        use std::os::fd::AsRawFd as _;
        let _ring = crate::sync::LockExt::locked(&crate::log::tests::RING_TEST_LOCK);
        let (w, mut r, q) = clogged();
        let mut g = MinRate {
            bps: 100_000,
            window: Duration::from_millis(10),
        }
        .guard(Side::Response, 1);
        g.watch_queue(w.as_raw_fd());
        drain_until(&w, &mut r, q / 2);
        let e = g.note(Duration::from_millis(10), 0).unwrap_err();
        assert!(is_slow(&e), "{e}");
    }

    /// 一度切った見張りは、次の書き (`BufWriter` の flush・Drop の flush) を待たずにエラーで返し、
    /// 2 回目を数えない (T36.33)。前は切ると窓を数え直すだけだったので、残りの書きがもう一度
    /// 無通信の締め切りまで待ち、大きければもう一度判定して数えた。
    #[test]
    #[cfg(target_os = "linux")]
    fn after_cutting_the_next_write_returns_without_waiting() {
        use std::io::Write as _;
        let _ring = crate::sync::LockExt::locked(&crate::log::tests::RING_TEST_LOCK);
        let (w, _r, _) = clogged();
        w.set_nonblocking(false).unwrap();
        let timeout = Duration::from_millis(300);
        w.set_write_timeout(Some(timeout)).unwrap();
        let rate = MinRate {
            bps: 1 << 30,
            window: Duration::from_millis(50),
        };
        let before = closed_counts()[0];
        let mut out = io::BufWriter::with_capacity(64 * 1024, GuardedWrite::new(&w, &rate, 1));
        // 1 回目: 容量ちょうどの書きは素通しで測られ、締め切りまで待って切られる
        let e = out.write_all(&[0u8; 64 * 1024]).unwrap_err();
        assert!(is_slow(&e), "{e}");
        assert_eq!(closed_counts()[0], before + 1);
        // 2 回目: 送信キューを詰め直しても、小さな書きの flush も大きな書きの Drop も待たない
        // (前は flush が締め切りまで待ち、Drop の 20 KiB はもう一度判定されて数えられた)
        out.write_all(&[0u8; 100]).unwrap();
        refill(&w);
        let t0 = Instant::now();
        let e = out.flush().unwrap_err();
        assert!(is_slow(&e), "{e}");
        out.write_all(&[0u8; 20 * 1024]).unwrap();
        refill(&w);
        drop(out);
        assert!(
            t0.elapsed() < timeout / 2,
            "切ったあとに待った: {:?}",
            t0.elapsed()
        );
        assert_eq!(closed_counts()[0], before + 1, "1 本を 2 回数えた");
        // 見張りそのものも、切ったあとは待ちうる 1 回を呼ばずに返す
        let mut g = rate.guard(Side::Response, 1);
        let _ = g.note(Duration::from_millis(50), 0).unwrap_err();
        let e = g.time(|| unreachable!("切ったあとに待った")).unwrap_err();
        assert!(is_slow(&e), "{e}");
        let e = g.note(Duration::from_millis(50), 0).unwrap_err();
        assert!(is_slow(&e), "{e}");
        assert_eq!(closed_counts()[0], before + 2);
    }
}
