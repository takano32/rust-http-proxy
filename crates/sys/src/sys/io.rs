//! `poll(2)` / `pipe2(2)` / `splice(2)` / `recv(MSG_PEEK)` の薄い包み。**中継のバイトごとに通る熱い経路**
//! (トンネルの `splice`、forward の本文の素通し) なので、ここは関数の形も費用も `sys.rs` にあった頃のまま。

use std::ffi::{c_int, c_uint, c_void};
use std::io;
use std::os::fd::RawFd;

use super::{EAGAIN, EINTR, O_CLOEXEC, O_NONBLOCK};

unsafe extern "C" {
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

const F_SETPIPE_SZ: c_int = 1031;
const SPLICE_F_MOVE: c_uint = 1;
const SPLICE_F_NONBLOCK: c_uint = 2;
const MSG_PEEK: c_int = 2;
const MSG_DONTWAIT: c_int = 0x40;

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
            super::close(self.read_fd);
            super::close(self.write_fd);
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
