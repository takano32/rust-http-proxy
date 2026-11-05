//! `epoll(7)` と `eventfd(2)` の薄い包み (暇な keep-alive 接続の預かり所の監視。T29.23)。

use std::ffi::{c_int, c_uint, c_void};
use std::io;
use std::os::fd::RawFd;

use super::{EINTR, O_CLOEXEC, O_NONBLOCK};

unsafe extern "C" {
    fn eventfd(initval: c_uint, flags: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn epoll_create1(flags: c_int) -> c_int;
    fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut EpollEvent) -> c_int;
    fn epoll_wait(epfd: c_int, events: *mut EpollEvent, maxevents: c_int, timeout: c_int) -> c_int;
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
        unsafe { super::close(self.fd) };
    }
}

/// `eventfd(2)` (待たない・`exec` で閉じる)。眠っている `epoll_wait` を起こす呼び鈴 (T29.23)。
///
/// 鳴らした回数はカーネルの中の 8 バイトの数に足されるだけなので、何度鳴らしても溢れず、
/// 読むまで `EPOLLIN` が立ち続ける (読む前に鳴らせば、次の `epoll_wait` はすぐ戻る)。
pub struct EventFd {
    fd: RawFd,
}

impl EventFd {
    pub fn new() -> io::Result<EventFd> {
        // SAFETY: 定数のフラグを渡すだけ。失敗は -1 で返る。
        // `EFD_CLOEXEC` / `EFD_NONBLOCK` は `O_CLOEXEC` / `O_NONBLOCK` と同じ値
        let fd = unsafe { eventfd(0, O_CLOEXEC | O_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(EventFd { fd })
    }

    /// epoll に入れる記述子。
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// 鳴らす (数に 1 足す)。失敗しても次の周回で拾えるので結果は返さない。
    pub fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: 8 バイトの値を指すポインタと長さ 8 を渡す。
        unsafe { write(self.fd, (&one as *const u64).cast(), 8) };
    }

    /// 溜まった呼び出しを読んで 0 に戻す (`EPOLLIN` を下ろす)。
    pub fn drain(&self) {
        let mut n: u64 = 0;
        // SAFETY: 8 バイトの書込先と長さ 8 を渡す。待たないので空なら EAGAIN が返るだけ。
        unsafe { read(self.fd, (&mut n as *mut u64).cast(), 8) };
    }
}

impl Drop for EventFd {
    fn drop(&mut self) {
        // SAFETY: 自分で開いた fd を 1 回だけ閉じる。
        unsafe { super::close(self.fd) };
    }
}
