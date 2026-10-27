//! クライアント接続からの読み取りバッファ。
//!
//! `std` の `BufReader<&TcpStream>` はバッファとストリームを一体で持つので、
//! 「要求と要求の間はストリームだけ手放してバッファは残す」ということができない。
//! ここではその 2 つを分け、[`ClientBuf`] がバッファ (接続の状態) を、
//! [`ClientReader`] が「要求を処理する間だけストリームと組にしたもの」を持つ。
//!
//! 分けておくと、アイドルの接続を別のスレッドへ預けるときに箱ごと動かせる
//! (先読みしたバイトを取りこぼさない)。

use std::cell::RefCell;
use std::io::{self, BufRead, Read};
use std::net::TcpStream;

// 先読みバッファの**スレッドごとの**置き場 (T.29.24)。
//
// 接続ごとに `resize(8 KiB, 0)` していたぶんの**ゼロ埋め (8,210 命令/接続) と malloc/free** を
// 無くす。`release()` / `take_buffered()` / `Drop` で返し、`fill_buf` が借りる。中身は前の接続の
// バイト列のままだが、**外へ見せるのは `read` が書いた `[..n]` だけ** (`buffered()` は
// `[pos..cap]`、`cap` は直前の `read` の戻り値) なので前の接続の内容は漏れない。
//
// プロセスで 1 つの置き場 (`Mutex`) にすると、接続のたびに全ワーカーが同じ鍵を 2 回取り合う。
// ワーカーは使い回されるスレッドなので、スレッドごとに少しだけ持てば足りる (預けた接続は
// 別のワーカーで終わることがあるが、返した先のスレッドで次に使われるだけ)。
// 1 スレッドに置くのは最大 `BUF_POOL_MAX` 本 (32 KiB)。スレッドが終われば一緒に返る。
thread_local! {
    static BUF_POOL: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
}
/// 1 スレッドの置き場の上限 (これを超えたぶんは返さずに解放する)。
const BUF_POOL_MAX: usize = 4;

fn take_buf() -> Vec<u8> {
    BUF_POOL
        .try_with(|p| p.borrow_mut().pop())
        .ok()
        .flatten()
        .unwrap_or_else(|| vec![0u8; CLIENT_READ_BUF])
}

fn give_buf(b: Vec<u8>) {
    if b.len() != CLIENT_READ_BUF {
        return;
    }
    // スレッドの後始末の途中 (置き場がもう無い) なら、そのまま解放する
    let _ = BUF_POOL.try_with(|p| {
        let mut p = p.borrow_mut();
        if p.len() < BUF_POOL_MAX {
            p.push(b);
        }
    });
}

/// 読み取りバッファの大きさ (`std` の `BufReader` の既定と同じ)。
pub const CLIENT_READ_BUF: usize = 8 * 1024;

/// クライアント接続の先読みバッファ。接続の状態としてそのまま持ち運べる。
#[derive(Default)]
pub struct ClientBuf {
    buf: Vec<u8>,
    /// まだ読み出していない範囲 `buf[pos..cap]`
    pos: usize,
    cap: usize,
}

impl ClientBuf {
    #[inline]
    pub fn new() -> ClientBuf {
        ClientBuf::default()
    }

    /// まだ読み出していないバイト列。
    #[inline]
    pub fn buffered(&self) -> &[u8] {
        &self.buf[self.pos..self.cap]
    }

    #[inline]
    pub fn has_buffered(&self) -> bool {
        self.pos < self.cap
    }

    /// 読み残しが無ければバッファ領域を手放す (アイドル中に 8 KiB を抱えないため)。
    /// 読み残しがあるときは何もしない。
    #[inline]
    pub fn release(&mut self) {
        if !self.has_buffered() {
            give_buf(std::mem::take(&mut self.buf));
            self.pos = 0;
            self.cap = 0;
        }
    }

    /// このバッファと `stream` を組にして、1 要求ぶんの読み取りに使う。
    #[inline]
    pub fn reader<'a>(&'a mut self, stream: &'a TcpStream) -> ClientReader<'a> {
        ClientReader { buf: self, stream }
    }
}

impl Drop for ClientBuf {
    fn drop(&mut self) {
        give_buf(std::mem::take(&mut self.buf));
    }
}

/// [`ClientBuf`] と `TcpStream` を組にしたもの。`BufRead` として使う。
pub struct ClientReader<'a> {
    buf: &'a mut ClientBuf,
    stream: &'a TcpStream,
}

impl ClientReader<'_> {
    /// まだ読み出していないバイト列 (`BufReader::buffer` と同じ)。
    #[inline]
    pub fn buffer(&self) -> &[u8] {
        self.buf.buffered()
    }

    /// 先読みしたバイトを取り出し、バッファを空にして領域も手放す。
    /// CONNECT でトンネルへ移るときのように、以後この接続を要求として読まない場合に使う。
    pub fn take_buffered(&mut self) -> Vec<u8> {
        let out = self.buf.buffered().to_vec();
        self.buf.pos = 0;
        self.buf.cap = 0;
        give_buf(std::mem::take(&mut self.buf.buf));
        out
    }
}

impl Read for ClientReader<'_> {
    #[inline]
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // 手元に何も無く、要求が大きいならバッファを経由しない (BufReader と同じ)
        if !self.buf.has_buffered() && out.len() >= CLIENT_READ_BUF {
            return (&*self.stream).read(out);
        }
        let available = self.fill_buf()?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for ClientReader<'_> {
    #[inline]
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if !self.buf.has_buffered() {
            if self.buf.buf.len() < CLIENT_READ_BUF {
                self.buf.buf = if self.buf.buf.is_empty() {
                    take_buf()
                } else {
                    let mut b = std::mem::take(&mut self.buf.buf);
                    b.resize(CLIENT_READ_BUF, 0);
                    b
                };
            }
            let n = (&*self.stream).read(&mut self.buf.buf)?;
            self.buf.pos = 0;
            self.buf.cap = n;
        }
        Ok(self.buf.buffered())
    }

    #[inline]
    fn consume(&mut self, amount: usize) {
        self.buf.pos = (self.buf.pos + amount).min(self.buf.cap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        (c, s)
    }

    /// 使い回した器でも、見えるのは今回 `read` が書いたぶんだけ (前の接続の中身が漏れない。T.29.24)。
    #[test]
    fn a_pooled_buffer_shows_only_what_this_read_wrote() {
        let (client, mut peer) = pair();
        peer.write_all(&[b'S'; 4000]).unwrap();
        let mut first = ClientBuf::new();
        {
            let mut r = first.reader(&client);
            let got = r.fill_buf().unwrap().len();
            r.consume(got);
        }
        // 読み残しが無いので器はプールへ戻る
        first.release();
        drop(first);
        let (client2, mut peer2) = pair();
        peer2.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut second = ClientBuf::new();
        let mut r = second.reader(&client2);
        let got = r.fill_buf().unwrap().to_vec();
        assert_eq!(got, b"GET / HTTP/1.1\r\n\r\n");
        assert!(!r.buffer().contains(&b'S'));
    }

    /// プールは上限で止まり、大きさの違う器は受け取らない。
    #[test]
    fn the_pool_is_bounded() {
        for _ in 0..(BUF_POOL_MAX + 10) {
            give_buf(vec![0u8; CLIENT_READ_BUF]);
        }
        give_buf(vec![0u8; 16]);
        BUF_POOL.with(|p| {
            let pool = p.borrow();
            assert_eq!(pool.len(), BUF_POOL_MAX);
            assert!(pool.iter().all(|b| b.len() == CLIENT_READ_BUF));
        });
    }

    #[test]
    fn reads_lines_and_keeps_the_rest_buffered() {
        let (client, mut peer) = pair();
        peer.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\nleftover")
            .unwrap();
        drop(peer); // read_to_end が終わるように相手を閉じる
        let mut buf = ClientBuf::new();
        {
            let mut r = buf.reader(&client);
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            assert_eq!(line, "GET / HTTP/1.1\r\n");
            line.clear();
            r.read_line(&mut line).unwrap();
            assert_eq!(line, "Host: x\r\n");
        }
        // 読み残しは ClientBuf に残り、リーダーを作り直しても失われない
        assert!(buf.has_buffered());
        {
            let mut r = buf.reader(&client);
            let mut rest = String::new();
            r.read_line(&mut rest).unwrap();
            assert_eq!(rest, "\r\n");
            let mut tail = Vec::new();
            r.read_to_end(&mut tail).unwrap();
            assert_eq!(tail, b"leftover");
        }
    }

    #[test]
    fn release_only_drops_the_buffer_when_nothing_is_left() {
        let (client, mut peer) = pair();
        peer.write_all(b"ab").unwrap();
        let mut buf = ClientBuf::new();
        {
            let mut r = buf.reader(&client);
            let mut one = [0u8; 1];
            r.read_exact(&mut one).unwrap();
        }
        assert!(buf.has_buffered(), "1 バイト残っている");
        buf.release();
        assert!(buf.has_buffered(), "読み残しがあるので手放さない");
        {
            let mut r = buf.reader(&client);
            let mut one = [0u8; 1];
            r.read_exact(&mut one).unwrap();
        }
        assert!(!buf.has_buffered());
        buf.release();
        assert_eq!(buf.buf.capacity(), 0, "空なら領域ごと手放す");
    }

    #[test]
    fn large_reads_bypass_the_buffer() {
        let (client, mut peer) = pair();
        let payload = vec![b'z'; 32 * 1024];
        let sender = std::thread::spawn(move || {
            peer.write_all(&payload).unwrap();
        });
        let mut buf = ClientBuf::new();
        let mut r = buf.reader(&client);
        let mut got = vec![0u8; 32 * 1024];
        r.read_exact(&mut got).unwrap();
        sender.join().unwrap();
        assert!(got.iter().all(|b| *b == b'z'));
    }
}
