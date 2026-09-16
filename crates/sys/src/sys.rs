//! Linux のシステムコールを直接叩く薄い層 (`poll`, `pipe2`, `splice`, `recv`, `epoll`, `setsockopt`, `getsockopt`)。
//! 外部クレートは使わず `unsafe extern "C"` で宣言する。Linux 以外ではこのモジュール自体が無い。

use std::ffi::{c_int, c_uint, c_void};
use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

unsafe extern "C" {
    fn mallopt(param: c_int, value: c_int) -> c_int;
    fn poll(fds: *mut PollFd, nfds: usize, timeout: c_int) -> c_int;
    fn pipe2(fds: *mut c_int, flags: c_int) -> c_int;
    fn splice(
        fd_in: c_int,
        off_in: *mut i64,
        fd_out: c_int,
        off_out: *mut i64,
        len: usize,
        flags: c_uint,
    ) -> isize;
    fn fcntl(fd: c_int, cmd: c_int, arg: c_int) -> c_int;
    fn recv(fd: c_int, buf: *mut c_void, len: usize, flags: c_int) -> isize;
    fn close(fd: c_int) -> c_int;
    fn epoll_create1(flags: c_int) -> c_int;
    fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut EpollEvent) -> c_int;
    fn epoll_wait(epfd: c_int, events: *mut EpollEvent, maxevents: c_int, timeout: c_int) -> c_int;
    fn setsockopt(
        fd: c_int,
        level: c_int,
        name: c_int,
        value: *const c_void,
        len: u32, // socklen_t
    ) -> c_int;
    fn getsockopt(
        fd: c_int,
        level: c_int,
        name: c_int,
        value: *mut c_void,
        // socklen_t の入出力。渡した長さを、カーネルが書いた長さで上書きして返す
        len: *mut u32,
    ) -> c_int;
}

/// `struct pollfd`。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PollFd {
    pub fd: RawFd,
    pub events: i16,
    pub revents: i16,
}

impl PollFd {
    pub fn new(fd: RawFd, events: i16) -> Self {
        PollFd {
            fd,
            events,
            revents: 0,
        }
    }
}

pub const POLLIN: i16 = 0x001;
pub const POLLOUT: i16 = 0x004;
pub const POLLERR: i16 = 0x008;
pub const POLLHUP: i16 = 0x010;

const O_CLOEXEC: c_int = 0o2000000;
const O_NONBLOCK: c_int = 0o4000;
const F_SETPIPE_SZ: c_int = 1031;
const SPLICE_F_MOVE: c_uint = 1;
const SPLICE_F_NONBLOCK: c_uint = 2;
const MSG_PEEK: c_int = 2;
const MSG_DONTWAIT: c_int = 0x40;
const EINTR: i32 = 4;
const EAGAIN: i32 = 11;

/// glibc の `M_ARENA_MAX` (malloc.h の定義値)。
const M_ARENA_MAX: c_int = -8;

/// malloc のアリーナ数に上限を掛ける。**スレッドを作る前に 1 回だけ呼ぶこと。**
///
/// glibc は既定でコア数の 8 倍までアリーナを作り、スレッドごとに別のアリーナを使う。
/// 接続ごとにスレッドが増えるこのプロキシでは、アイドル接続を多数抱えたときに
/// 使われないままのアリーナが RSS に居座る。戻り値は成功したか。
pub fn limit_malloc_arenas(max: c_int) -> bool {
    // SAFETY: 定数のパラメータ番号と値を渡すだけ。失敗は 0 で返る。
    unsafe { mallopt(M_ARENA_MAX, max) == 1 }
}

/// `poll(2)`。`timeout_ms` が負なら無期限。戻り値は準備できた記述子の数 (0 はタイムアウト)。
/// `EINTR` は 0 個として返す (呼び出し側でループする)。
pub fn poll_fds(fds: &mut [PollFd], timeout_ms: c_int) -> io::Result<usize> {
    for f in fds.iter_mut() {
        f.revents = 0;
    }
    // SAFETY: fds は有効なスライスで、その長さを渡している。
    let n = unsafe { poll(fds.as_mut_ptr(), fds.len(), timeout_ms) };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(EINTR) {
            return Ok(0);
        }
        return Err(e);
    }
    Ok(n as usize)
}

/// `pipe2(2)` で作った無名パイプ。`splice` の中継バッファに使う。
pub struct Pipe {
    pub read_fd: RawFd,
    pub write_fd: RawFd,
}

impl Pipe {
    pub fn new() -> io::Result<Pipe> {
        let mut fds = [0 as c_int; 2];
        // SAFETY: fds は 2 要素の配列。失敗は -1 で返る。
        if unsafe { pipe2(fds.as_mut_ptr(), O_CLOEXEC | O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Pipe {
            read_fd: fds[0],
            write_fd: fds[1],
        })
    }

    /// 待つパイプ (`splice` がブロックする)。本文の素通しに使う。
    pub fn new_blocking() -> io::Result<Pipe> {
        let mut fds = [0 as c_int; 2];
        // SAFETY: fds は 2 要素の配列。失敗は -1 で返る。
        if unsafe { pipe2(fds.as_mut_ptr(), O_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Pipe {
            read_fd: fds[0],
            write_fd: fds[1],
        })
    }

    /// パイプ容量を広げる (失敗しても既定容量のまま使えるので無視して良い)。
    pub fn set_capacity(&self, bytes: c_int) {
        // SAFETY: 自分で開いた fd に対する fcntl。失敗は -1 で返るだけ。
        unsafe { fcntl(self.write_fd, F_SETPIPE_SZ, bytes) };
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: 自分で開いた fd を 1 回だけ閉じる。
        unsafe {
            close(self.read_fd);
            close(self.write_fd);
        }
    }
}

/// `splice(2)` でカーネル内をコピーする。`Ok(0)` は EOF、`WouldBlock` は今は動かせない。
pub fn splice_move(from: RawFd, to: RawFd, len: usize) -> io::Result<usize> {
    loop {
        // SAFETY: どちらも呼び出し側が保持している有効な記述子。オフセットは使わない。
        let n = unsafe {
            splice(
                from,
                std::ptr::null_mut(),
                to,
                std::ptr::null_mut(),
                len,
                SPLICE_F_MOVE | SPLICE_F_NONBLOCK,
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(EINTR) => continue,
            Some(EAGAIN) => return Err(io::Error::from(io::ErrorKind::WouldBlock)),
            _ => return Err(e),
        }
    }
}

/// 待つ `splice(2)`。相手が読めるようになるまでブロックする。
/// パイプ側にも `O_NONBLOCK` があると `EAGAIN` になるので、[`Pipe::new_blocking`] と組で使う。
pub fn splice_block(from: RawFd, to: RawFd, len: usize) -> io::Result<usize> {
    loop {
        // SAFETY: どちらも呼び出し側が保持している有効な記述子。オフセットは使わない。
        let n = unsafe {
            splice(
                from,
                std::ptr::null_mut(),
                to,
                std::ptr::null_mut(),
                len,
                SPLICE_F_MOVE,
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(EINTR) {
            continue;
        }
        return Err(e);
    }
}

/// `recv(fd, .., MSG_PEEK | MSG_DONTWAIT)` を 1 回だけ呼ぶ (プールの生存確認用)。
/// `Ok(0)` は相手が閉じた、`WouldBlock` は読めるものが無い (= 生きている)。
pub fn peek(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: buf は有効な書込先で、長さを渡している。
    let n = unsafe {
        recv(
            fd,
            buf.as_mut_ptr() as *mut c_void,
            buf.len(),
            MSG_PEEK | MSG_DONTWAIT,
        )
    };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(EAGAIN) {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        return Err(e);
    }
    Ok(n as usize)
}

/// `setsockopt(2)` の定数 (aarch64 / x86_64 で同じ値。他の arch は値が違うので
/// [`inherit_socket_options`] ごと外し、呼び出し側は接続ごとの設定に落ちる)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod sockopt {
    use std::ffi::c_int;

    pub const SOL_SOCKET: c_int = 1;
    pub const SO_RCVTIMEO: c_int = 20;
    pub const SO_SNDTIMEO: c_int = 21;
    pub const IPPROTO_TCP: c_int = 6;
    pub const TCP_NODELAY: c_int = 1;

    /// `struct timeval` (64 bit Linux)。
    #[repr(C)]
    pub struct TimeVal {
        pub tv_sec: i64,
        pub tv_usec: i64,
    }
}

/// 待ち受けソケットに「accept した接続へ引き継がせたいオプション」をまとめて当てる。
///
/// Linux は `accept` のときに `sk_clone_lock` が `struct sock` ごと複製するので、
/// `TCP_NODELAY` / `SO_RCVTIMEO` / `SO_SNDTIMEO` は待ち受けから接続へそのまま引き継がれる。
/// 待ち受けに 1 回当てておけば接続ごとの `setsockopt` 3 回を払わなくて済む。
///
/// **`SO_RCVTIMEO` は `accept()` 自体にも効く** (`timeout` ごとに `EAGAIN` で戻る) ので、
/// 呼び出し側の accept ループは `WouldBlock` を「まだ来ていない」として読み飛ばすこと。
/// `timeout` が 0 (= 無期限) のときは `accept()` も無期限に待つ。
///
/// 失敗したら `Err` を返す。呼び出し側は**従来どおり接続ごとに設定する**こと
/// (カーネルが引き継がない環境でも動きが変わらないように)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn inherit_socket_options(listener_fd: RawFd, timeout: Duration) -> io::Result<()> {
    use sockopt::*;

    let mut tv = TimeVal {
        tv_sec: timeout.as_secs().min(i64::MAX as u64) as i64,
        tv_usec: timeout.subsec_micros() as i64,
    };
    // Linux では `timeval {0, 0}` が「タイムアウト無し」。`PROXY_TIMEOUT_SECS=0` は
    // まさにそれを意味するので (T10.6)、0 はそのまま渡して**継承させる**。
    // 以前はここで断って接続ごとの設定に落としていたが、落ちた先の
    // `set_write_timeout(Some(ZERO))` を std が `InvalidInput` で拒むので、
    // その設定では全接続が失敗していた。
    // 0 でない指定が丸めで「無期限」に化けるのは困るので、そちらは 1 us に上げる
    // (std と同じ丸め方)
    if !timeout.is_zero() && tv.tv_sec == 0 && tv.tv_usec == 0 {
        tv.tv_usec = 1;
    }
    let one: c_int = 1;
    // Nagle を切る。応答ヘッダーと本文を別々に write すると delayed ACK と噛み合って
    // 1 要求あたり 40 ms 止まるため
    set(listener_fd, IPPROTO_TCP, TCP_NODELAY, &one)?;
    set(listener_fd, SOL_SOCKET, SO_RCVTIMEO, &tv)?;
    set(listener_fd, SOL_SOCKET, SO_SNDTIMEO, &tv)?;
    Ok(())
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn inherit_socket_options(_listener_fd: RawFd, _timeout: Duration) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "socket option constants are only known for aarch64 and x86_64",
    ))
}

/// `setsockopt` を 1 つ当てる。`value` は C 側の型と同じレイアウトを持つ値。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn set<T>(fd: RawFd, level: c_int, name: c_int, value: &T) -> io::Result<()> {
    // SAFETY: value は呼び出しの間だけ有効なら良く (カーネルが値を複製する)、
    // 長さもその型の大きさをそのまま渡している。
    let r = unsafe {
        setsockopt(
            fd,
            level,
            name,
            value as *const T as *const c_void,
            size_of::<T>() as u32,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// カーネルが持っている TCP 接続 1 本の様子 (`getsockopt(SOL_TCP, TCP_INFO)` の一部。T14.5)。
///
/// 平滑化 RTT が読めれば「物理 (往復) と自分 (それ以外)」が切り分けられ、再送の数で
/// 相手までの回線の質が分かる。読むのは**接続の終わりに 1 回だけ** (要求ごとには読まない)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpInfo {
    /// 平滑化 RTT (us)。`tcpi_rtt`
    pub rtt_us: u32,
    /// RTT のばらつき (us)。`tcpi_rttvar`
    pub rttvar_us: u32,
    /// いま再送中のセグメント数。`tcpi_retrans`
    pub retrans: u32,
    /// この接続で再送した通算。`tcpi_total_retrans`
    pub total_retrans: u32,
    /// 失われたと見なしたセグメント数。`tcpi_lost`
    pub lost: u32,
    /// 輻輳窓 (セグメント)。`tcpi_snd_cwnd`
    pub cwnd: u32,
    /// 経路 MTU。`tcpi_pmtu`
    pub pmtu: u32,
}

/// `struct tcp_info` の欄の位置 (バイト)。**先頭 8 バイトが `__u8` の旗** で、
/// 以後は `__u32` が並ぶ。値は動作環境の `/usr/include/linux/tcp.h` を
/// `offsetof` で実際に確かめたもの (2026-09-16、Linux 6.6 のヘッダーで
/// `sizeof(struct tcp_info)` は 280。カーネルが増えても**前の欄は動かない**)。
mod tcpinfo {
    pub const LOST: usize = 32;
    pub const RETRANS: usize = 36;
    pub const PMTU: usize = 60;
    pub const RTT: usize = 68;
    pub const RTTVAR: usize = 72;
    pub const SND_CWND: usize = 80;
    pub const TOTAL_RETRANS: usize = 100;
    /// 読む緩衝の大きさ (`tcpi_total_retrans` の次まで)。カーネルは `min(len, sizeof)`
    /// しか書かないので、これより新しい欄は取らないし、古いカーネルでも落ちない
    pub const LEN: usize = TOTAL_RETRANS + 4;
    /// ここまで書かれていれば RTT は読めた (`tcpi_rttvar` の次まで)
    pub const MIN_USEFUL: usize = RTTVAR + 4;
}

/// `getsockopt` の `level` に使う TCP (= `IPPROTO_TCP`)。
///
/// [`inherit_socket_options`] の定数と違い、**この 2 つはどの arch でも同じ値**
/// (`IPPROTO_*` は IANA の番号、`TCP_*` は linux/tcp.h で arch に依らない) なので、
/// `tcp_info` は arch で切り分けずに使える。
const SOL_TCP: c_int = 6;
/// `TCP_INFO` (linux/tcp.h)。
const TCP_INFO: c_int = 11;

/// カーネルの RTT と再送を 1 本ぶん読む (`getsockopt` 1 回)。
///
/// 読めない相手 (TCP でない・もう閉じている) や、RTT の欄まで書かれなかった
/// 古いカーネルでは `None`。**呼ぶのは接続の終わりだけ** — 要求ごとに呼ぶと
/// システムコールが 1 要求 1 回増える。
pub fn tcp_info(fd: RawFd) -> Option<TcpInfo> {
    let mut buf = [0u8; tcpinfo::LEN];
    let mut len = tcpinfo::LEN as u32;
    // SAFETY: buf は LEN バイトの配列で、その長さを socklen_t として渡している。
    // カーネルは min(len, sizeof(struct tcp_info)) バイトだけ書き、書いた長さを len に返す。
    let r = unsafe {
        getsockopt(
            fd,
            SOL_TCP,
            TCP_INFO,
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
        )
    };
    if r < 0 || (len as usize) < tcpinfo::MIN_USEFUL {
        return None;
    }
    // 書かれなかった後ろの欄は 0 のまま返る (緩衝を 0 で作ってある)
    let at = |off: usize| {
        if off + 4 > len as usize {
            return 0;
        }
        u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
    };
    Some(TcpInfo {
        rtt_us: at(tcpinfo::RTT),
        rttvar_us: at(tcpinfo::RTTVAR),
        retrans: at(tcpinfo::RETRANS),
        total_retrans: at(tcpinfo::TOTAL_RETRANS),
        lost: at(tcpinfo::LOST),
        cwnd: at(tcpinfo::SND_CWND),
        pmtu: at(tcpinfo::PMTU),
    })
}

/// `getrlimit(2)` / `setrlimit(2)` の定数と `struct rlimit`。
///
/// `RLIMIT_NOFILE` の番号 (7) は asm-generic の値で、mips / sparc では違う。`rlim_t` の幅も
/// libc 次第 (64 ビット環境の glibc / musl はどちらも 64 ビット、32 ビットの musl は 64 ビットだが
/// 32 ビットの glibc は 32 ビット) で、食い違うと呼び出し側のスタックを壊す。
/// [`inherit_socket_options`] と同じく **aarch64 / x86_64 だけ**で有効にし、それ以外では
/// 「分からない」を返す (呼び出し側は固定の既定値に落ちる)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod rlimit {
    use std::ffi::c_int;

    pub const RLIMIT_NOFILE: c_int = 7;

    /// `struct rlimit` (64 ビットの `rlim_t` 2 つ)。
    #[repr(C)]
    pub struct RLimit {
        pub cur: u64,
        pub max: u64,
    }

    unsafe extern "C" {
        pub fn getrlimit(resource: c_int, rlim: *mut RLimit) -> c_int;
        pub fn setrlimit(resource: c_int, rlim: *const RLimit) -> c_int;
    }
}

/// このプロセスが開ける記述子の数 (`RLIMIT_NOFILE` の soft limit)。
/// 分からなければ `None` (呼び出し側は固定の既定値に落ちる)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn max_open_files() -> Option<u64> {
    let mut lim = rlimit::RLimit { cur: 0, max: 0 };
    // SAFETY: lim はこの呼び出しの間だけ有効なら良い書込先。失敗は -1 で返る。
    if unsafe { rlimit::getrlimit(rlimit::RLIMIT_NOFILE, &mut lim) } < 0 {
        return None;
    }
    Some(lim.cur)
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn max_open_files() -> Option<u64> {
    None
}

/// `RLIMIT_NOFILE` の soft limit を下げる (hard limit はそのまま)。
/// **記述子の少ない環境を再現するためのもの** (`tests/maxconns_test.rs`)。プロセス全体に効くので、
/// 呼ぶのは「そのテストだけが動いているテストバイナリ」に限ること。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn set_max_open_files(soft: u64) -> io::Result<()> {
    let mut lim = rlimit::RLimit { cur: 0, max: 0 };
    // SAFETY: 上と同じ。今の hard limit を読んでから、それを超えない値に下げる。
    if unsafe { rlimit::getrlimit(rlimit::RLIMIT_NOFILE, &mut lim) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let next = rlimit::RLimit {
        cur: soft.min(lim.max),
        max: lim.max,
    };
    // SAFETY: next は呼び出しの間だけ有効なら良い (カーネルが値を読むだけ)。
    if unsafe { rlimit::setrlimit(rlimit::RLIMIT_NOFILE, &next) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn set_max_open_files(_soft: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "RLIMIT_NOFILE is only wired up for aarch64 and x86_64",
    ))
}

/// `struct epoll_event`。
///
/// **x86_64 (と x32) だけ `__attribute__((packed))` が付く** ため 12 バイト、
/// それ以外 (aarch64 など) は詰め物込みで 16 バイト。C 側と食い違うと
/// `epoll_wait` が書き戻す配列の刻みがずれるので、arch ごとに合わせる。
#[repr(C)]
#[cfg_attr(target_arch = "x86_64", repr(packed))]
#[derive(Clone, Copy, Default)]
pub struct EpollEvent {
    events: u32,
    data: u64,
}

#[cfg(target_arch = "x86_64")]
const _: () = assert!(size_of::<EpollEvent>() == 12);
#[cfg(not(target_arch = "x86_64"))]
const _: () = assert!(size_of::<EpollEvent>() == 16);

impl EpollEvent {
    pub fn new(events: u32, token: u64) -> Self {
        EpollEvent {
            events,
            data: token,
        }
    }

    /// 起きた事象のビット。
    pub fn events(&self) -> u32 {
        // packed なので参照を作らず値で取り出す
        self.events
    }

    /// 登録時に預けた目印 (このプロキシでは fd 番号を入れる)。
    pub fn token(&self) -> u64 {
        self.data
    }
}

pub const EPOLLIN: u32 = 0x001;
pub const EPOLLERR: u32 = 0x008;
pub const EPOLLHUP: u32 = 0x010;
pub const EPOLLRDHUP: u32 = 0x2000;

const EPOLL_CLOEXEC: c_int = O_CLOEXEC;
const EPOLL_CTL_ADD: c_int = 1;
const EPOLL_CTL_DEL: c_int = 2;

/// `epoll_create1(2)` で作った監視の集合。落ちるときに fd を閉じる。
pub struct Epoll {
    fd: RawFd,
}

impl Epoll {
    pub fn new() -> io::Result<Epoll> {
        // SAFETY: 定数のフラグを渡すだけ。失敗は -1 で返る。
        let fd = unsafe { epoll_create1(EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Epoll { fd })
    }

    /// 記述子を集合に足す。`token` は [`EpollEvent::token`] でそのまま返ってくる。
    pub fn add(&self, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
        let mut ev = EpollEvent::new(events, token);
        // SAFETY: ev はこの呼び出しの間だけ有効なら良い (カーネルは値を複製する)。
        let r = unsafe { epoll_ctl(self.fd, EPOLL_CTL_ADD, fd, &mut ev) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// 記述子を集合から外す。
    pub fn delete(&self, fd: RawFd) -> io::Result<()> {
        // SAFETY: DEL では event は見られない (Linux 2.6.9 以降は NULL 可)。
        let r = unsafe { epoll_ctl(self.fd, EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// `epoll_wait(2)`。戻り値は `events` の先頭何個が埋まったか (0 はタイムアウト)。
    /// `timeout_ms` が負なら無期限。`EINTR` は 0 個として返す。
    pub fn wait(&self, events: &mut [EpollEvent], timeout_ms: c_int) -> io::Result<usize> {
        let max = events.len().min(c_int::MAX as usize) as c_int;
        // SAFETY: events は max 個ぶんの書込先として有効。
        let n = unsafe { epoll_wait(self.fd, events.as_mut_ptr(), max, timeout_ms) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(EINTR) {
                return Ok(0);
            }
            return Err(e);
        }
        Ok(n as usize)
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        // SAFETY: 自分で開いた fd を 1 回だけ閉じる。
        unsafe { close(self.fd) };
    }
}

/// 待ち受けソケットを自分で作るための定数と束縛 (`socket` / `bind` / `listen`。T14.47)。
///
/// `std` の `TcpListener::bind` は **`listen(fd, 128)` 固定**なので、backlog を選ぶには
/// ソケットを自分で作るしかない。値は [`sockopt`] と同じく **aarch64 / x86_64 だけ**で
/// 有効にする (`SOCK_CLOEXEC` と `SO_REUSEADDR` は arch によって値が違う)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod listensock {
    use std::ffi::{c_int, c_void};

    pub const AF_INET: c_int = 2;
    pub const AF_INET6: c_int = 10;
    pub const SOCK_STREAM: c_int = 1;
    /// `SOCK_CLOEXEC` は `O_CLOEXEC` と同じ値 (asm-generic)。
    pub const SOCK_CLOEXEC: c_int = 0o2000000;
    pub const SO_REUSEADDR: c_int = 2;

    /// `struct sockaddr_in` (16 B)。`sin_port` と `sin_addr` はネットワークバイト順。
    #[repr(C)]
    pub struct SockAddrIn {
        pub sin_family: u16,
        pub sin_port: u16,
        pub sin_addr: [u8; 4],
        pub sin_zero: [u8; 8],
    }

    /// `struct sockaddr_in6` (28 B)。
    #[repr(C)]
    pub struct SockAddrIn6 {
        pub sin6_family: u16,
        pub sin6_port: u16,
        pub sin6_flowinfo: u32,
        pub sin6_addr: [u8; 16],
        pub sin6_scope_id: u32,
    }

    unsafe extern "C" {
        pub fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
        pub fn bind(fd: c_int, addr: *const c_void, len: u32) -> c_int;
        pub fn listen(fd: c_int, backlog: c_int) -> c_int;
    }
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const _: () = assert!(size_of::<listensock::SockAddrIn>() == 16);
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const _: () = assert!(size_of::<listensock::SockAddrIn6>() == 28);

/// backlog を選んで待ち受けソケットを 1 本作る (`socket` → `SO_REUSEADDR` → `bind` → `listen`。T14.47)。
///
/// 返すのは **`listen` まで済んだ生の記述子**で、呼び出し側が `TcpListener::from_raw_fd` で
/// 包む (包んだ時点で閉じる責任も移る)。途中で失敗したらここで閉じてから `Err` を返す。
///
/// `std` の `TcpListener::bind` との違いは **backlog だけ**。`SO_REUSEADDR` は `std` と同じく
/// `bind` の前に立て、**`IPV6_V6ONLY` は触らない** (カーネルの既定 =
/// `/proc/sys/net/ipv6/bindv6only` に任せる。`[::]` が IPv4 も受ける今の姿を変えないため)。
/// `backlog` はカーネルが `/proc/sys/net/core/somaxconn` で頭打ちにするので、
/// 大きめの値を渡しても溢れない。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn listen_socket(addr: std::net::SocketAddr, backlog: u32) -> io::Result<RawFd> {
    use listensock::*;

    let port = addr.port().to_be();
    let (domain, v4, v6) = match addr.ip() {
        std::net::IpAddr::V4(ip) => (
            AF_INET,
            Some(SockAddrIn {
                sin_family: AF_INET as u16,
                sin_port: port,
                sin_addr: ip.octets(),
                sin_zero: [0; 8],
            }),
            None,
        ),
        std::net::IpAddr::V6(ip) => (
            AF_INET6,
            None,
            Some(SockAddrIn6 {
                sin6_family: AF_INET6 as u16,
                sin6_port: port,
                sin6_flowinfo: 0,
                sin6_addr: ip.octets(),
                // 待ち受けでは scope id を使わない (リンクローカルを明示するときだけ要る)
                sin6_scope_id: 0,
            }),
        ),
    };
    // SAFETY: 引数は定数だけ。失敗は -1 で返る。
    let fd = unsafe { socket(domain, SOCK_STREAM | SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // ここから先の失敗は、開けた記述子を閉じてから返す (漏らさない)
    let failed = |fd: RawFd| -> io::Error {
        let e = io::Error::last_os_error();
        // SAFETY: 自分で開いた fd を 1 回だけ閉じる。
        unsafe { close(fd) };
        e
    };
    // `std` の `TcpListener::bind` と同じく bind の前に `SO_REUSEADDR` を立てる
    // (TIME_WAIT の残っているポートにも待ち受けられるように)
    let one: c_int = 1;
    if set(fd, sockopt::SOL_SOCKET, SO_REUSEADDR, &one).is_err() {
        return Err(failed(fd));
    }
    // SAFETY: どちらの枝も、その族の `sockaddr` とその大きさを対で渡している。
    let bound = unsafe {
        match (&v4, &v6) {
            (Some(a), _) => bind(
                fd,
                a as *const SockAddrIn as *const c_void,
                size_of::<SockAddrIn>() as u32,
            ),
            (_, Some(a)) => bind(
                fd,
                a as *const SockAddrIn6 as *const c_void,
                size_of::<SockAddrIn6>() as u32,
            ),
            // `IpAddr` は 2 つしか無いので、どちらかは必ず `Some`
            (None, None) => -1,
        }
    };
    if bound < 0 {
        return Err(failed(fd));
    }
    // SAFETY: 自分で開いた記述子を listen するだけ。カーネルが somaxconn で頭打ちにする。
    if unsafe { listen(fd, backlog.min(c_int::MAX as u32) as c_int) } < 0 {
        return Err(failed(fd));
    }
    Ok(fd)
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn listen_socket(_addr: std::net::SocketAddr, _backlog: u32) -> io::Result<RawFd> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "socket constants are only known for aarch64 and x86_64",
    ))
}

/// TCP keepalive の定数 (T14.52)。
///
/// `SO_KEEPALIVE` は `SOL_SOCKET` の 9 で、[`sockopt`] の値と同じく **arch によって
/// 違う** (asm-generic は 9、mips や alpha は別) ので同じ条件で囲む。
/// `TCP_KEEPIDLE` / `TCP_KEEPINTVL` / `TCP_KEEPCNT` は linux/tcp.h の 4 / 5 / 6 で
/// arch に依らない ([`TCP_INFO`] と同じ)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod keepalive {
    use std::ffi::c_int;

    pub const SO_KEEPALIVE: c_int = 9;
    pub const TCP_KEEPIDLE: c_int = 4;
    pub const TCP_KEEPINTVL: c_int = 5;
    pub const TCP_KEEPCNT: c_int = 6;
}

/// 1 本のソケットに当たっている TCP keepalive (T14.52。読み戻し用)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keepalive {
    /// `SO_KEEPALIVE` が立っているか
    pub on: bool,
    /// 無通信がこれだけ続いたら最初の探りを送る (秒。`TCP_KEEPIDLE`)
    pub idle_secs: u32,
    /// 探りと探りの間隔 (秒。`TCP_KEEPINTVL`)
    pub intvl_secs: u32,
    /// 返事の無い探りを何回まで送るか (`TCP_KEEPCNT`)
    pub count: u32,
}

/// 消えたクライアントを見つけるための TCP keepalive を 1 本に当てる (T14.52)。
///
/// **`setsockopt` を 4 回**呼ぶ (接続あたりの固定費はこの 4 回だけ。当てないときは
/// 呼び出し側が分岐 1 回で飛ばす)。`TCP_NODELAY` などと違って**待ち受けから継承させて
/// いない**のは、`PROXY_TCP_KEEPALIVE` を `.env` で変えたときに次の接続から効くように
/// するため (待ち受けに当てると当て直しの機会が無い)。
///
/// 当てたあと相手が消えると、カーネルは `idle_secs + intvl_secs * count` 秒ほどで
/// そのソケットの保留エラーを `ETIMEDOUT` にする。以後の `read` / `write` / `splice` は
/// その errno で返るので、呼び出し側は「相手が消えた」と記録できる。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn set_keepalive(fd: RawFd, idle_secs: u32, intvl_secs: u32, count: u32) -> io::Result<()> {
    use keepalive::*;

    let on: c_int = 1;
    set(fd, sockopt::SOL_SOCKET, SO_KEEPALIVE, &on)?;
    // 0 は「カーネルの既定のまま」になってしまうので、最低 1 秒 / 1 回に上げる
    let idle: c_int = idle_secs.max(1).min(c_int::MAX as u32) as c_int;
    let intvl: c_int = intvl_secs.max(1).min(c_int::MAX as u32) as c_int;
    let cnt: c_int = count.max(1).min(c_int::MAX as u32) as c_int;
    set(fd, SOL_TCP, TCP_KEEPIDLE, &idle)?;
    set(fd, SOL_TCP, TCP_KEEPINTVL, &intvl)?;
    set(fd, SOL_TCP, TCP_KEEPCNT, &cnt)?;
    Ok(())
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn set_keepalive(_fd: RawFd, _idle: u32, _intvl: u32, _count: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SO_KEEPALIVE is only known for aarch64 and x86_64",
    ))
}

/// いま当たっている keepalive を読み戻す (`getsockopt` 4 回)。
///
/// 使うのは**テストと診断だけ**で、接続の経路からは呼ばない。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn keepalive_of(fd: RawFd) -> io::Result<Keepalive> {
    use keepalive::*;

    Ok(Keepalive {
        on: get_int(fd, sockopt::SOL_SOCKET, SO_KEEPALIVE)? != 0,
        idle_secs: get_int(fd, SOL_TCP, TCP_KEEPIDLE)?.max(0) as u32,
        intvl_secs: get_int(fd, SOL_TCP, TCP_KEEPINTVL)?.max(0) as u32,
        count: get_int(fd, SOL_TCP, TCP_KEEPCNT)?.max(0) as u32,
    })
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn keepalive_of(_fd: RawFd) -> io::Result<Keepalive> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SO_KEEPALIVE is only known for aarch64 and x86_64",
    ))
}

/// `getsockopt` で `c_int` を 1 つ読む ([`set`] の裏返し)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn get_int(fd: RawFd, level: c_int, name: c_int) -> io::Result<c_int> {
    let mut v: c_int = 0;
    let mut len = size_of::<c_int>() as u32;
    // SAFETY: `c_int` 1 つぶんの領域とその長さを対で渡している。カーネルは
    // min(len, 4) バイトだけ書き、書いた長さを len に返す。
    let r = unsafe {
        getsockopt(
            fd,
            level,
            name,
            &mut v as *mut c_int as *mut c_void,
            &mut len,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;

    #[test]
    fn splices_through_a_pipe() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut a = TcpStream::connect(addr).unwrap();
        let (b, _) = listener.accept().unwrap();
        let (c, _) = (TcpStream::connect(addr).unwrap(), ());
        let (mut d, _) = listener.accept().unwrap();

        a.write_all(b"hello splice").unwrap();
        let pipe = Pipe::new().unwrap();
        pipe.set_capacity(1 << 20);
        let n = splice_move(b.as_raw_fd(), pipe.write_fd, 64 * 1024).unwrap();
        assert_eq!(n, 12);
        let m = splice_move(pipe.read_fd, c.as_raw_fd(), n).unwrap();
        assert_eq!(m, 12);
        let mut buf = [0u8; 12];
        d.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello splice");
    }

    #[test]
    fn peek_reports_eof_and_would_block() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let a = TcpStream::connect(addr).unwrap();
        let (b, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(
            peek(a.as_raw_fd(), &mut buf).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(b);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(peek(a.as_raw_fd(), &mut buf).unwrap(), 0, "peer closed");
    }

    #[test]
    fn epoll_reports_readable_and_peer_close() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut a = TcpStream::connect(addr).unwrap();
        let (b, _) = listener.accept().unwrap();

        let ep = Epoll::new().unwrap();
        let token = 0xdead_beef_cafe_u64;
        ep.add(b.as_raw_fd(), EPOLLIN | EPOLLRDHUP, token).unwrap();

        let mut evs = [EpollEvent::default(); 8];
        // まだ何も来ていない
        assert_eq!(ep.wait(&mut evs, 10).unwrap(), 0);

        a.write_all(b"x").unwrap();
        assert_eq!(ep.wait(&mut evs, 1000).unwrap(), 1);
        assert_eq!(evs[0].token(), token);
        assert!(evs[0].events() & EPOLLIN != 0);

        drop(a);
        assert_eq!(ep.wait(&mut evs, 1000).unwrap(), 1);
        assert!(evs[0].events() & (EPOLLRDHUP | EPOLLIN) != 0);

        ep.delete(b.as_raw_fd()).unwrap();
        assert_eq!(ep.wait(&mut evs, 10).unwrap(), 0, "外したら報告されない");
    }

    #[test]
    fn epoll_wakes_when_another_thread_adds_a_ready_fd() {
        // 監視スレッドが epoll_wait で待っている最中に別スレッドが足しても届くこと
        // (park は必ずワーカースレッド側から呼ばれる)
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut a = TcpStream::connect(addr).unwrap();
        let (b, _) = listener.accept().unwrap();
        a.write_all(b"already here").unwrap();

        let ep = std::sync::Arc::new(Epoll::new().unwrap());
        let ep2 = std::sync::Arc::clone(&ep);
        let fd = b.as_raw_fd();
        let adder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            ep2.add(fd, EPOLLIN, 7).unwrap();
        });
        let mut evs = [EpollEvent::default(); 4];
        assert_eq!(ep.wait(&mut evs, 3000).unwrap(), 1);
        assert_eq!(evs[0].token(), 7);
        adder.join().unwrap();
    }

    #[test]
    fn epoll_add_rejects_a_regular_file() {
        // epoll に入れられない記述子は EPERM。park は失敗を握りつぶさず旧経路へ落とす
        let ep = Epoll::new().unwrap();
        let f = std::fs::File::open("/proc/self/cmdline").unwrap();
        assert!(ep.add(f.as_raw_fd(), EPOLLIN, 0).is_err());
    }

    #[test]
    fn accepted_sockets_inherit_the_listener_options() {
        // カーネルの挙動を固定するテスト。accept したソケットが待ち受けの
        // TCP_NODELAY / SO_RCVTIMEO / SO_SNDTIMEO を引き継がない環境が出たら、
        // ここが落ちて「接続ごとに設定する」経路へ戻せる (T9.3)
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let timeout = std::time::Duration::from_secs(7);
        inherit_socket_options(listener.as_raw_fd(), timeout).unwrap();

        let _client = TcpStream::connect(addr).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        assert!(accepted.nodelay().unwrap(), "TCP_NODELAY を引き継ぐ");
        assert_eq!(accepted.read_timeout().unwrap(), Some(timeout));
        assert_eq!(accepted.write_timeout().unwrap(), Some(timeout));
    }

    #[test]
    fn accept_times_out_with_the_receive_timeout() {
        // SO_RCVTIMEO は accept() にも効く。serve の accept ループはこれを
        // 「まだ来ていない」として読み飛ばす (ログも sleep も無しに continue)
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        inherit_socket_options(listener.as_raw_fd(), std::time::Duration::from_millis(50)).unwrap();
        let started = std::time::Instant::now();
        let err = listener.accept().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "無期限に待っていない"
        );
    }

    #[test]
    fn zero_timeout_is_inherited_as_no_timeout() {
        // `PROXY_TIMEOUT_SECS=0` = 無期限 (T10.6)。Linux の `timeval {0, 0}` が
        // まさに「タイムアウト無し」なので、そのまま継承させられる
        // (接続ごとの `set_write_timeout(Some(ZERO))` は std が断るので使えない)。
        // カーネルがこの意味を変えたらここが落ちる
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        inherit_socket_options(listener.as_raw_fd(), std::time::Duration::ZERO).unwrap();

        let _client = TcpStream::connect(addr).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        assert!(accepted.nodelay().unwrap(), "TCP_NODELAY は引き継ぐ");
        assert_eq!(accepted.read_timeout().unwrap(), None, "読みは無期限");
        assert_eq!(accepted.write_timeout().unwrap(), None, "書きも無期限");
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn reports_the_open_file_limit() {
        // 幅や番号が食い違っていれば、ここが 0 や桁違いの値になって落ちる
        // (下げる方はテストのプロセス全体に効くので、ここでは呼ばない)
        let soft = max_open_files().expect("getrlimit(RLIMIT_NOFILE)");
        assert!(soft >= 64, "soft limit が小さすぎる: {}", soft);
        let open = std::fs::File::open("/proc/self/cmdline").unwrap();
        assert!(
            (open.as_raw_fd() as u64) < soft,
            "開いている fd 番号 {} が soft limit {} を超えている",
            open.as_raw_fd(),
            soft
        );
    }

    #[test]
    fn poll_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut fds = [PollFd::new(a.as_raw_fd(), POLLIN)];
        assert_eq!(poll_fds(&mut fds, 10).unwrap(), 0);
    }

    /// `TCP_INFO` が loopback の接続で読めること (欄の位置が合っているかの確認。T14.5)。
    ///
    /// 位置がずれていれば RTT が桁違いになるか、MTU が 65,536 (loopback) で出なくなる。
    #[test]
    fn reads_the_kernel_rtt_of_a_loopback_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();

        for s in [&client, &server] {
            let i = tcp_info(s.as_raw_fd()).expect("TCP_INFO が読めない");
            // loopback の往復は 1 ms 未満 (0 も含む。確立直後は RTT の標本が 1 つ)
            assert!(i.rtt_us < 1_000, "loopback の RTT が大きすぎる: {:?}", i);
            assert_eq!(i.retrans, 0, "{:?}", i);
            assert_eq!(i.total_retrans, 0, "{:?}", i);
            assert_eq!(i.lost, 0, "{:?}", i);
            // 輻輳窓と MTU は「欄の位置が合っているか」の目印 (ずれると 0 か桁違いになる)
            assert!(i.cwnd > 0 && i.cwnd < 100_000, "cwnd が変: {:?}", i);
            assert!((1_000..=70_000).contains(&i.pmtu), "経路 MTU が変: {:?}", i);
        }
    }

    /// TCP でない記述子は `None` (落ちない)。
    #[test]
    fn tcp_info_is_none_for_a_file() {
        let f = std::fs::File::open("/proc/self/cmdline").unwrap();
        assert_eq!(tcp_info(f.as_raw_fd()), None);
    }

    /// 自分で作った待ち受けが `std` の `TcpListener` と同じように使えること (T14.47)。
    ///
    /// `sockaddr` の組み立て (族・ポートのバイト順) が違っていれば `bind` が `EINVAL` に
    /// なるか、`local_addr` が別のアドレスで返る。
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn listen_socket_binds_ipv4_with_the_given_backlog() {
        use std::os::fd::FromRawFd;

        let want: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let fd = listen_socket(want, 5).expect("listen_socket(127.0.0.1:0)");
        // SAFETY: listen まで済んだ記述子を 1 回だけ包む (以後は TcpListener が閉じる)。
        let listener = unsafe { TcpListener::from_raw_fd(fd) };
        let bound = listener.local_addr().unwrap();
        assert_eq!(bound.ip(), want.ip(), "{}", bound);
        assert_ne!(bound.port(), 0, "ephemeral port が割り当たっていない");

        let client = TcpStream::connect(bound).unwrap();
        let (accepted, peer) = listener.accept().unwrap();
        assert_eq!(peer.ip(), want.ip());
        drop((client, accepted));
    }

    /// IPv6 側も同じ (`sockaddr_in6` は 28 B で欄の並びが違う)。
    /// `::1` が無い機械では黙って飛ばす。
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn listen_socket_binds_ipv6_with_the_given_backlog() {
        use std::os::fd::FromRawFd;

        if TcpListener::bind("[::1]:0").is_err() {
            return;
        }
        let want: std::net::SocketAddr = "[::1]:0".parse().unwrap();
        let fd = listen_socket(want, 1024).expect("listen_socket([::1]:0)");
        // SAFETY: 上と同じ。
        let listener = unsafe { TcpListener::from_raw_fd(fd) };
        let bound = listener.local_addr().unwrap();
        assert_eq!(bound.ip(), want.ip(), "{}", bound);
        let client = TcpStream::connect(bound).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        drop((client, accepted));
    }

    /// 使用中のポートは `AddrInUse` で返り、記述子を漏らさない (失敗の枝で close している)。
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn listen_socket_reports_address_in_use_without_leaking() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = taken.local_addr().unwrap();
        // `SO_REUSEADDR` は「TIME_WAIT のポートを使える」だけで、生きている待ち受けとは共有しない
        let open_fds = || std::fs::read_dir("/proc/self/fd").unwrap().count();
        let before = open_fds();
        for _ in 0..8 {
            let err = listen_socket(addr, 128).expect_err("使用中のポートに bind できてしまった");
            assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "{:?}", err);
        }
        assert_eq!(open_fds(), before, "失敗した 8 回ぶんの記述子が残っている");
    }

    /// 消えたクライアントを見つけるための TCP keepalive が、当てたとおりに
    /// カーネルへ届いていること (T14.52)。`setsockopt` の定数が間違っていれば
    /// `ENOPROTOOPT` になるか、読み戻した値が食い違う。
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn keepalive_lands_on_the_socket_and_reads_back() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, _) = listener.accept().unwrap();

        // 何も当てていないソケットは off (既定の idle は `/proc/sys/net/ipv4/tcp_keepalive_time`)
        let before = keepalive_of(accepted.as_raw_fd()).expect("getsockopt");
        assert!(
            !before.on,
            "はじめから SO_KEEPALIVE が立っている: {:?}",
            before
        );

        // 既定と違う値を選ぶ (既定の 60/10/3 と取り違えないように)
        set_keepalive(accepted.as_raw_fd(), 7, 3, 5).expect("setsockopt");
        assert_eq!(
            keepalive_of(accepted.as_raw_fd()).expect("getsockopt"),
            Keepalive {
                on: true,
                idle_secs: 7,
                intvl_secs: 3,
                count: 5,
            }
        );
        // 当てていない方 (クライアント側) は素のまま
        assert!(!keepalive_of(client.as_raw_fd()).expect("getsockopt").on);
        drop((client, accepted));
    }

    /// 0 を渡しても「カーネルの既定のまま」にはならず 1 に上がること (T14.52)。
    ///
    /// `TCP_KEEPIDLE=0` は `setsockopt` が `EINVAL` を返す値なので、丸めずに
    /// 渡すと 4 回のうち 1 回が失敗して keepalive が中途半端に当たる。
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn keepalive_rounds_zero_up_to_one() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, _) = listener.accept().unwrap();

        set_keepalive(accepted.as_raw_fd(), 0, 0, 0).expect("setsockopt");
        assert_eq!(
            keepalive_of(accepted.as_raw_fd()).expect("getsockopt"),
            Keepalive {
                on: true,
                idle_secs: 1,
                intvl_secs: 1,
                count: 1,
            }
        );
        drop((client, accepted));
    }

    /// TCP でない記述子には当たらない (失敗を握りつぶさずに `Err` で返すこと)。
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn keepalive_fails_on_something_that_is_not_a_socket() {
        let f = std::fs::File::open("/proc/self/stat").unwrap();
        assert!(set_keepalive(f.as_raw_fd(), 60, 10, 3).is_err());
        assert!(keepalive_of(f.as_raw_fd()).is_err());
    }
}
