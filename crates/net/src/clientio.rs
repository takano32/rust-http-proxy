//! クライアント接続からの読み取りバッファ。
//!
//! `std` の `BufReader<&TcpStream>` はバッファとストリームを一体で持つので、
//! 「要求と要求の間はストリームだけ手放してバッファは残す」ということができない。
//! ここではその 2 つを分け、[`ClientBuf`] がバッファ (接続の状態) を、
//! [`ClientReader`] が「要求を処理する間だけストリームと組にしたもの」を持つ。
//!
//! 分けておくと、アイドルの接続を別のスレッドへ預けるときに箱ごと動かせる
//! (先読みしたバイトを取りこぼさない)。

use std::io::{self, BufRead, Read};
use std::net::TcpStream;

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
            self.buf = Vec::new();
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
        self.buf.buf = Vec::new();
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
                self.buf.buf.resize(CLIENT_READ_BUF, 0);
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
