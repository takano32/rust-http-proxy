//! 片方向の中継 ([`Dir`]) と、スレッドごとの中継パイプの置き場。
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;

use crate::sys::{self, Pipe};

/// 1 回の splice / read で動かす最大バイト数 (パイプ容量と同じ)。
const CHUNK: usize = 1 << 20;

/// 「相手が黙って消えた」= `ETIMEDOUT` か (T14.52)。
///
/// TCP keepalive (`PROXY_TCP_KEEPALIVE`) が尽きると、カーネルはそのソケットの
/// 保留エラーを `ETIMEDOUT` にする。以後の `splice` / `read` / `write` / `poll` は
/// この errno で返るので、閉じた理由を `client_dead` と書ける。
/// `SO_RCVTIMEO` / `SO_SNDTIMEO` の締め切りは Linux では `EAGAIN` (= `WouldBlock`)
/// なので**ここには混ざらない**。
#[inline]
pub(super) fn went_away(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::TimedOut
}

/// スレッドごとに使い回す中継パイプの数 (トンネル 1 本が両方向で 2 本使う)。
const POOLED_PIPES: usize = 2;

thread_local! {
    /// 使い終わった中継パイプの置き場 (スレッドごと)。
    ///
    /// 短命なトンネルは 1 本あたり `pipe2` 2 回・`fcntl(F_SETPIPE_SZ)` 2 回・
    /// `close` 4 回を払っていた (`--only connect` の実測でシステムコール 29.03 回/本 のうち 8 回)。
    /// 空になったパイプはスレッドに残しておき、次のトンネルが使い回す。
    /// **`Drop` を持つ [`Pipe`] を置くので、出し入れは必ず `try_with` で行うこと**
    /// (スレッドの終了中に `with` を呼ぶと `AccessError` で panic し、
    /// 「thread local panicked on drop」でプロセスごと abort する)。
    static PIPES: std::cell::RefCell<Vec<Pipe>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// 置き場から 1 本取る。無ければ作る (容量を広げるのは作るときだけ)。
#[inline]
fn take_pipe() -> io::Result<Pipe> {
    if let Ok(Some(pipe)) = PIPES.try_with(|c| c.borrow_mut().pop()) {
        return Ok(pipe);
    }
    let pipe = Pipe::new()?;
    pipe.set_capacity(CHUNK as i32);
    Ok(pipe)
}

/// 空のパイプを置き場へ返す。置き場が一杯 (かスレッドの終了中) なら閉じる。
#[inline]
fn give_pipe(pipe: Pipe) {
    let _ = PIPES.try_with(move |c| {
        let mut pool = c.borrow_mut();
        if pool.len() < POOLED_PIPES {
            pool.push(pipe);
        }
    });
}

/// 片方向の中継。データはパイプ (splice) か、使えなければ中間バッファに置く。
/// 最初にデータが動くまで作らない (アイドルのトンネルは資源を持たない)。
pub(super) enum Relay {
    Unset,
    Pipe(Pipe),
    Buf(Vec<u8>),
}

pub(super) struct Dir {
    /// `socks` の添字 (0 = クライアント, 1 = サーバー)
    pub(super) src: usize,
    pub(super) dst: usize,
    pub(super) relay: Relay,
    /// まだ相手に渡していないバイト数
    pub(super) pending: usize,
    /// バッファ方式で次に書き出す位置
    pub(super) offset: usize,
    /// poll が「読める」と言った (最初は分からないので待つ側から始める)
    pub(super) readable: bool,
    pub(super) src_eof: bool,
    pub(super) done: bool,
    pub(super) moved: u64,
}

impl Dir {
    #[inline]
    pub(super) fn new(src: usize, dst: usize) -> Dir {
        Dir {
            src,
            dst,
            relay: Relay::Unset,
            pending: 0,
            offset: 0,
            readable: false,
            src_eof: false,
            done: false,
            moved: 0,
        }
    }

    /// 送信元から中継バッファへ移す。`Ok(0)` は EOF。
    #[inline]
    pub(super) fn fill(&mut self, socks: &[TcpStream; 2]) -> io::Result<usize> {
        if matches!(self.relay, Relay::Unset) {
            self.relay = match take_pipe() {
                Ok(p) => Relay::Pipe(p),
                Err(_) => Relay::Buf(vec![0u8; 64 * 1024]),
            };
        }
        match &mut self.relay {
            Relay::Unset => unreachable!("just initialised"),
            Relay::Pipe(pipe) => {
                match sys::splice_move(socks[self.src].as_raw_fd(), pipe.write_fd, CHUNK) {
                    Ok(n) => Ok(n),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(e),
                    // 相手が消えた (T14.52): これは「splice が使えない」ではなく
                    // 「接続が死んだ」なので、64 KiB の緩衝を確保して読み直さずに
                    // そのまま上へ返す (呼び出し側が `client_dead` と書く)
                    Err(e) if went_away(&e) => Err(e),
                    // splice が使えないソケット (EINVAL など) はバッファ方式へ落として次周で読む
                    Err(_) => {
                        self.relay = Relay::Buf(vec![0u8; 64 * 1024]);
                        self.offset = 0;
                        Err(io::Error::from(io::ErrorKind::WouldBlock))
                    }
                }
            }
            Relay::Buf(buf) => {
                let n = (&socks[self.src]).read(buf)?;
                self.offset = 0;
                Ok(n)
            }
        }
    }

    /// 中継バッファから送信先へ移す。
    #[inline]
    pub(super) fn drain(&mut self, socks: &[TcpStream; 2]) -> io::Result<usize> {
        match &mut self.relay {
            Relay::Unset => Ok(0),
            Relay::Pipe(pipe) => {
                sys::splice_move(pipe.read_fd, socks[self.dst].as_raw_fd(), self.pending)
            }
            Relay::Buf(buf) => {
                let end = self.offset + self.pending;
                (&socks[self.dst]).write(&buf[self.offset..end])
            }
        }
    }

    /// 中継の置き場を手放す。パイプは**空のときだけ**スレッドの置き場へ返す。
    ///
    /// 中身が残っているパイプを使い回すと、次のトンネルに**他人のバイト**が流れる。
    /// `pending` はパイプに入っていてまだ渡していないバイト数なので、これが 0 の
    /// ときだけ返す (渡せなくなったときは呼び出し側が `Relay::Unset` にして閉じる)。
    #[inline]
    pub(super) fn drop_relay(&mut self) {
        if let Relay::Pipe(pipe) = std::mem::replace(&mut self.relay, Relay::Unset)
            && self.pending == 0
        {
            give_pipe(pipe);
        }
        self.offset = 0;
    }

    /// 「今すぐ動かすものが無く、まだ両方向とも生きている」か (預けてよいかの判定)。
    #[inline]
    pub(super) fn quiet(&self) -> bool {
        self.pending == 0 && !self.src_eof && !self.done
    }
}
