//! Linux のシステムコールを直接叩く薄い層 (`poll`, `pipe2`, `splice`, `recv`, `epoll`, `setsockopt`, `getsockopt`)。
//! 外部クレートは使わず `unsafe extern "C"` で宣言する。Linux 以外ではこのモジュール自体が無い。
//!
//! 責務ごとに子のファイルへ分けてある (T34.3)。公開の名前はここで出し直すので、呼ぶ側の綴り
//! (`sys::splice_move` など) は `sys.rs` が 1 枚だった頃と同じ:
//!
//! - `io`: `poll` / `pipe2` / `splice` / `recv(MSG_PEEK)` (中継のバイトごとに通る熱い経路)
//! - `epoll`: `epoll` と `eventfd` (暇な接続の預かり所の監視)
//! - `sockopt`: 待ち受けから引き継がせるオプション・TCP keepalive と、`setsockopt` / `getsockopt` の包み
//! - `tcpinfo`: `getsockopt(SOL_TCP, TCP_INFO)` (カーネルの RTT と再送)
//! - `listen`: backlog を選べる待ち受けソケット (`socket` / `bind` / `listen`)
//! - `process`: `gettid` / `RLIMIT_NOFILE` / `boot_id`
//! - `malloc`: glibc の `mallopt`

use std::ffi::c_int;

mod epoll;
mod io;
mod listen;
mod malloc;
mod process;
mod sockopt;
mod tcpinfo;

#[cfg(test)]
mod tests;

pub use epoll::{EPOLLERR, EPOLLHUP, EPOLLIN, EPOLLRDHUP, Epoll, EpollEvent, EventFd};
pub use io::{
    POLLERR, POLLHUP, POLLIN, POLLOUT, Pipe, PollFd, peek, poll_fds, splice_block, splice_move,
};
pub use listen::listen_socket;
pub use malloc::{fix_malloc_mmap_threshold, limit_malloc_arenas};
pub use process::{boot_id, gettid, max_open_files, set_max_open_files};
pub use sockopt::{Keepalive, inherit_socket_options, keepalive_of, set_keepalive};
pub use tcpinfo::{TcpInfo, tcp_info};

// 子が共有する定数と束縛 (値は asm-generic。子からは `super::…` で引く)
const O_CLOEXEC: c_int = 0o2000000;
const O_NONBLOCK: c_int = 0o4000;
const EINTR: i32 = 4;
const EAGAIN: i32 = 11;

/// `getsockopt` / `setsockopt` の `level` に使う TCP (= `IPPROTO_TCP`)。
///
/// [`inherit_socket_options`] の定数と違い、**`IPPROTO_*` は IANA の番号、`TCP_*` は linux/tcp.h で
/// arch に依らない**ので、`tcp_info` と keepalive は arch で切り分けずに使える。
const SOL_TCP: c_int = 6;

unsafe extern "C" {
    fn close(fd: c_int) -> c_int;
}
