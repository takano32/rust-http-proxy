//! Linux のシステムコールを直接叩く薄い層 (`poll`, `pipe2`, `splice`, `recv`, `epoll`,
//! `setsockopt`, `socket` + `connect`)。
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

/// 自前の接続 (`socket` + `connect` + `poll` + `getsockopt`) に要る定数と構造体。
///
/// `SOCK_STREAM` (1) も `SOCK_NONBLOCK` (`O_NONBLOCK`) も `SO_ERROR` (4) も
/// **mips / sparc / alpha では値が違う**ので、[`inherit_socket_options`] と同じく
/// **aarch64 / x86_64 だけ**で有効にする (他 arch の呼び出し側は std の
/// `TcpStream::connect_timeout` に落ちる)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod sock {
    use std::ffi::{c_int, c_void};

    pub const AF_INET: c_int = 2;
    pub const AF_INET6: c_int = 10;
    pub const SOCK_STREAM: c_int = 1;
    pub const SOCK_NONBLOCK: c_int = 0o4000; // = O_NONBLOCK
    pub const SOCK_CLOEXEC: c_int = 0o2000000; // = O_CLOEXEC
    pub const SO_ERROR: c_int = 4;
    pub const EINPROGRESS: i32 = 115;
    pub const F_GETFL: c_int = 3;

    /// `struct sockaddr_in` (16 バイト)。ポートとアドレスはネットワークバイト順。
    #[repr(C)]
    pub struct SockAddrIn {
        pub sin_family: u16,
        pub sin_port: u16,
        pub sin_addr: [u8; 4],
        pub sin_zero: [u8; 8],
    }

    /// `struct sockaddr_in6` (28 バイト)。
    #[repr(C)]
    pub struct SockAddrIn6 {
        pub sin6_family: u16,
        pub sin6_port: u16,
        pub sin6_flowinfo: u32,
        pub sin6_addr: [u8; 16],
        pub sin6_scope_id: u32,
    }

    const _: () = assert!(size_of::<SockAddrIn>() == 16);
    const _: () = assert!(size_of::<SockAddrIn6>() == 28);

    /// どちらの `sockaddr` でも同じように `connect` へ渡せるようにする入れ物。
    pub enum RawAddr {
        V4(SockAddrIn),
        V6(SockAddrIn6),
    }

    impl RawAddr {
        pub fn new(addr: &std::net::SocketAddr) -> RawAddr {
            match addr {
                std::net::SocketAddr::V4(a) => RawAddr::V4(SockAddrIn {
                    sin_family: AF_INET as u16,
                    sin_port: a.port().to_be(),
                    sin_addr: a.ip().octets(),
                    sin_zero: [0; 8],
                }),
                std::net::SocketAddr::V6(a) => RawAddr::V6(SockAddrIn6 {
                    sin6_family: AF_INET6 as u16,
                    sin6_port: a.port().to_be(),
                    // flowinfo / scope_id は std もそのまま持っているので変換しない
                    // (ネットワークバイト順に直すのはポートとアドレスだけ)
                    sin6_flowinfo: a.flowinfo(),
                    sin6_addr: a.ip().octets(),
                    sin6_scope_id: a.scope_id(),
                }),
            }
        }

        pub fn domain(&self) -> c_int {
            match self {
                RawAddr::V4(_) => AF_INET,
                RawAddr::V6(_) => AF_INET6,
            }
        }

        /// `connect` に渡すポインタと長さ。
        pub fn as_raw(&self) -> (*const c_void, u32) {
            match self {
                RawAddr::V4(a) => (
                    a as *const SockAddrIn as *const c_void,
                    size_of::<SockAddrIn>() as u32,
                ),
                RawAddr::V6(a) => (
                    a as *const SockAddrIn6 as *const c_void,
                    size_of::<SockAddrIn6>() as u32,
                ),
            }
        }
    }

    unsafe extern "C" {
        pub fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
        pub fn connect(fd: c_int, addr: *const c_void, len: u32) -> c_int;
        pub fn getsockopt(
            fd: c_int,
            level: c_int,
            name: c_int,
            value: *mut c_void,
            len: *mut u32, // socklen_t
        ) -> c_int;
    }
}

/// 宛先へ接続し、**nonblocking のままの** [`std::net::TcpStream`] を返す。
///
/// std の `TcpStream::connect_timeout` は「nonblocking を on にして接続を待ち、戻る前に
/// off に戻す」ので `ioctl(FIONBIO)` を 2 回払う。CONNECT の中継は結局 nonblocking で
/// 回すので、そこで on に戻し直す 3 回目まで払っていた (T10.1 の実測で `ioctl` 4.00 回/本、
/// うち 3 回が無駄)。ここでは `socket(SOCK_NONBLOCK | SOCK_CLOEXEC)` で最初から
/// nonblocking にして作り、`connect` の `EINPROGRESS` を `poll(POLLOUT)` で待って
/// `getsockopt(SO_ERROR)` で結果を取る。**`ioctl` は 1 回も要らない。**
///
/// ブロッキングで使いたい呼び出し側 (オリジンプール) は、戻ってきたソケットに
/// `set_nonblocking(false)` を 1 回だけ当てること (それでも std より 1 回少ない)。
///
/// `timeout` が `None` なら締め切り無し (OS が諦めるまで待つ。`Duration::ZERO` を
/// `None` に直すのは呼び出し側の役目 — `proxy_base::timeout::for_socket`)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn connect_nonblocking(
    addr: &std::net::SocketAddr,
    timeout: Option<Duration>,
) -> io::Result<std::net::TcpStream> {
    use sock::*;
    use std::os::fd::FromRawFd;

    let raw = RawAddr::new(addr);
    // SAFETY: 定数のフラグを渡すだけ。失敗は -1 で返る。
    let fd = unsafe { socket(raw.domain(), SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // ここから先はどこで失敗しても `stream` が落ちて fd が閉じる。
    // SAFETY: 今 `socket` が返したばかりの、他の誰も持っていない記述子。
    let stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
    let (sa, len) = raw.as_raw();
    // SAFETY: sa はこの呼び出しの間だけ有効なら良く (カーネルが読むだけ)、長さも合わせてある。
    if unsafe { connect(fd, sa, len) } < 0 {
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            // nonblocking なので普通はこちら。完了を poll で待つ
            Some(EINPROGRESS | EINTR) => wait_until_connected(fd, timeout)?,
            _ => return Err(e),
        }
    }
    Ok(stream)
}

/// `connect` が `EINPROGRESS` で戻ったあと、完了 (か失敗) まで待つ。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn wait_until_connected(fd: RawFd, timeout: Option<Duration>) -> io::Result<()> {
    use sock::*;
    use std::time::Instant;

    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        let timeout_ms = match deadline {
            None => -1,
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "connection timed out",
                    ));
                }
                // 1 ms 未満を 0 に丸めると即座にタイムアウト扱いになるので切り上げる
                (left.as_millis().min(c_int::MAX as u128) as c_int).max(1)
            }
        };
        let mut fds = [PollFd::new(fd, POLLOUT)];
        // `poll_fds` は EINTR も 0 個で返す。締め切りがあれば次の周回の頭で判定し、
        // 無ければそのまま待ち直す
        if poll_fds(&mut fds, timeout_ms)? > 0 {
            break;
        }
    }
    // 接続が成功したか失敗したかは `SO_ERROR` にしか出ない
    // (`poll` は失敗のときも POLLOUT を立てる)
    let mut err: c_int = 0;
    let mut len = size_of::<c_int>() as u32;
    // SAFETY: err と len はこの呼び出しの間だけ有効なら良い書込先。
    let r = unsafe {
        getsockopt(
            fd,
            sockopt::SOL_SOCKET,
            SO_ERROR,
            &mut err as *mut c_int as *mut c_void,
            &mut len,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    if err != 0 {
        return Err(io::Error::from_raw_os_error(err));
    }
    Ok(())
}

/// この記述子が nonblocking かどうか (`fcntl(F_GETFL) & O_NONBLOCK`)。
///
/// **[`connect_nonblocking`] の約束 (「中継へは nonblocking のまま、プールへは
/// ブロッキングで渡す」) をテストで固定するためのもの。** 熱い経路では呼ばない。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn is_nonblocking(fd: RawFd) -> io::Result<bool> {
    // SAFETY: 呼び出し側が持っている有効な記述子の状態を読むだけ。失敗は -1 で返る。
    let flags = unsafe { fcntl(fd, sock::F_GETFL, 0) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(flags & O_NONBLOCK != 0)
}

/// 値の分からない arch では自前の接続を持たない (呼び出し側は std の
/// `TcpStream::connect_timeout` に落ちる)。
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn connect_nonblocking(
    _addr: &std::net::SocketAddr,
    _timeout: Option<Duration>,
) -> io::Result<std::net::TcpStream> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "socket constants are only known for aarch64 and x86_64",
    ))
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn is_nonblocking(_fd: RawFd) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "socket constants are only known for aarch64 and x86_64",
    ))
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

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn connects_and_leaves_the_socket_nonblocking() {
        // T11.1: std の connect_timeout は nonblocking を on/off するので ioctl を 2 回
        // 払う。こちらは最初から SOCK_NONBLOCK で作り、そのまま中継へ渡す
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client =
            connect_nonblocking(&addr, Some(std::time::Duration::from_secs(5))).unwrap();
        let (mut accepted, _) = listener.accept().unwrap();
        assert!(
            is_nonblocking(client.as_raw_fd()).unwrap(),
            "中継はこのまま poll で回すので nonblocking のまま返す"
        );
        // 読めるものが無ければ即 WouldBlock (= 待たない)
        let mut buf = [0u8; 4];
        assert_eq!(
            client.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        // 実際に通じている
        accepted.write_all(b"okay").unwrap();
        for _ in 0..100 {
            if client.read(&mut buf).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(&buf, b"okay");
        // 戻せばブロッキングになる (オリジンプールの経路はこの 1 回だけ払う)
        client.set_nonblocking(false).unwrap();
        assert!(!is_nonblocking(client.as_raw_fd()).unwrap());
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn connect_reports_refusal_through_so_error() {
        // 失敗は connect ではなく getsockopt(SO_ERROR) にしか出ない
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = connect_nonblocking(&addr, Some(std::time::Duration::from_secs(5))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn connect_gives_up_on_an_unreachable_address() {
        // 締め切りを置いたら、そこで諦めて戻ってくること (TEST-NET-1 宛)。
        // 環境によっては即 EHOSTUNREACH / ENETUNREACH になるので、
        // 「短い時間で Err が返る」ことだけを固定する
        let addr: std::net::SocketAddr = "192.0.2.1:9".parse().unwrap();
        let started = std::time::Instant::now();
        let err =
            connect_nonblocking(&addr, Some(std::time::Duration::from_millis(200))).unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "締め切りを無視して待っている: {:?} ({})",
            started.elapsed(),
            err
        );
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn connects_over_ipv6_loopback() {
        // sockaddr_in6 のレイアウトが合っていること (合っていないと connect が
        // EAFNOSUPPORT / EINVAL で落ちる)。IPv6 の無い環境では bind に失敗するので飛ばす
        let Ok(listener) = TcpListener::bind("[::1]:0") else {
            return;
        };
        let addr = listener.local_addr().unwrap();
        let client = connect_nonblocking(&addr, Some(std::time::Duration::from_secs(5))).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        assert_eq!(accepted.peer_addr().unwrap(), client.local_addr().unwrap());
    }

    #[test]
    fn poll_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut fds = [PollFd::new(a.as_raw_fd(), POLLIN)];
        assert_eq!(poll_fds(&mut fds, 10).unwrap(), 0);
    }
}
