//! Content-Length のある応答本文を `splice(2)` でオリジンからクライアントへ素通しする。
//!
//! 本文を書き換えず、キャッシュにも保存しない応答なら、ユーザー空間に持ち上げる必要がない。
//! CONNECT トンネルで既に使っている仕掛けと同じ (カーネル内でパイプを経由して移す)。

use std::io;
use std::os::fd::RawFd;
use std::sync::Mutex;

use crate::sync::LockExt;
use crate::sys::{self, Pipe};

/// この大きさ以上の本文だけ [`relay`] に回す。これより小さいと、応答ヘッダーを
/// 読んだ時点でだいたい手元のバッファに入っているので、splice の往復のぶんかえって遅い
/// (実測: 64 KiB では約 171 → 179 us/要求)。
pub(super) const SPLICE_MIN_BYTES: u64 = 128 * 1024;

/// パイプの容量 = 1 回の splice で動かす上限。
///
/// 1 MiB にすると `fs.pipe-user-pages-soft` (既定 16384 ページ = 64 MiB) に当たって
/// 64 本しか作れず、それ以降は黙って既定の 64 KiB に落ちる。256 KiB なら 256 本まで
/// 同じ容量が行き渡る。
const PIPE_CAPACITY: usize = 256 * 1024;
/// 使い回すために置いておくパイプの上限 (1 本あたりカーネル側のメモリを持つ)。
const POOL_MAX: usize = 32;

static POOL: Mutex<Vec<Pipe>> = Mutex::new(Vec::new());

/// プールから借りたパイプ。Drop で返す。
struct Borrowed(Option<Pipe>);

impl Borrowed {
    fn take() -> io::Result<Borrowed> {
        if let Some(p) = POOL.locked().pop() {
            return Ok(Borrowed(Some(p)));
        }
        let p = Pipe::new_blocking()?;
        p.set_capacity(PIPE_CAPACITY as i32);
        Ok(Borrowed(Some(p)))
    }
}

impl Drop for Borrowed {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let mut pool = POOL.locked();
            if pool.len() < POOL_MAX {
                pool.push(p);
            }
        }
    }
}

/// `remaining` バイトを運ぶ。戻り値は (運んだバイト数, 最後まで届いたか)。
///
/// 途中で失敗したらパイプに残ったバイトは失われるので、呼び出し側はこの接続を
/// プールに戻してはいけない (`Err` を返した場合)。
///
/// `guard` が切る設定のとき、`to` (クライアント) は**呼ぶ側が待たない形にしてある**
/// (T29.34)。待つソケットへの splice は 1 回の呼び出しの中でページごとに締め切りを
/// 待ち直すので、読まない相手には 1 回で 20 秒以上止まり、見張りが判定できない
/// (実測 22 秒で 95 KB)。待たない splice が `EAGAIN` なら `poll` で書けるのを待ち、
/// 待った時間を見張りに積む。`poll` は窓の残りより長く眠らず、無通信の締め切り
/// `timeout` (`0` は無期限) を書けないまま越えたら `WouldBlock` で返す (前と同じ扱い)。
pub fn relay(
    from: RawFd,
    to: RawFd,
    remaining: u64,
    guard: &mut proxy_base::minrate::RateGuard,
    timeout: std::time::Duration,
) -> io::Result<(u64, bool)> {
    let borrowed = Borrowed::take()?;
    let pipe = borrowed.0.as_ref().expect("just taken");
    let mut left = remaining;
    let mut moved = 0u64;
    while left > 0 {
        let want = left.min(PIPE_CAPACITY as u64) as usize;
        let n = sys::splice_block(from, pipe.write_fd, want)?;
        if n == 0 {
            // オリジンが Content-Length より早く閉じた
            return Ok((moved, false));
        }
        let mut out = n;
        while out > 0 {
            let w = if guard.is_on() {
                to_client(pipe.read_fd, to, out, guard, timeout)?
            } else {
                sys::splice_block(pipe.read_fd, to, out)?
            };
            if w == 0 {
                return Ok((moved, false));
            }
            out -= w;
            moved += w as u64;
            left -= w as u64;
        }
    }
    Ok((moved, true))
}

/// 待たないクライアントへ 1 回ぶん運ぶ (書けなければ `poll` で待つ。[`relay`] を見よ)。
fn to_client(
    pipe: RawFd,
    to: RawFd,
    len: usize,
    guard: &mut proxy_base::minrate::RateGuard,
    timeout: std::time::Duration,
) -> io::Result<usize> {
    let mut stuck = std::time::Duration::ZERO;
    loop {
        match sys::splice_block(pipe, to, len) {
            Ok(w) => {
                guard.note(std::time::Duration::ZERO, w as u64)?;
                return Ok(w);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        if !timeout.is_zero() && stuck >= timeout {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        let mut wait = guard.left().unwrap_or(timeout);
        if !timeout.is_zero() {
            wait = wait.min(timeout - stuck);
        }
        let ms = wait.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut fds = [sys::PollFd::new(to, sys::POLLOUT)];
        let t0 = std::time::Instant::now();
        let ready = sys::poll_fds(&mut fds, ms)?;
        let waited = t0.elapsed();
        guard.note(waited, 0)?;
        if ready == 0 {
            stuck += waited;
        }
    }
}
