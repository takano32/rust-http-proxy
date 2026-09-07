//! Linux のシステムコールを直接叩く薄い層 (`poll`, `pipe2`, `splice`, `recv`, `epoll`)。
//! 外部クレートは使わず `unsafe extern "C"` で宣言する。Linux 以外ではこのモジュール自体が無い。

use std::ffi::{c_int, c_uint, c_void};
use std::io;
use std::os::fd::RawFd;

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
    fn poll_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut fds = [PollFd::new(a.as_raw_fd(), POLLIN)];
        assert_eq!(poll_fds(&mut fds, 10).unwrap(), 0);
    }
}
