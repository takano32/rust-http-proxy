//! 打ち手 (T.34.80 で lib.rs から分けた)。
//!
//! `CONC` 本のスレッドで forward / CONNECT を回し、本数の上限 ([`Budget`]) を分け合う。

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::CONC;
use crate::cpu::Charge;

/// 打ち手 `CONC` 本が分け合う「あと何回やってよいか」(T14.43 の上限)。
///
/// 秒数だけで止めると、速い CPU では 88,000 要求 / 22,000 本まで行ってしまう
/// ([`crate::MAX_REQUESTS`] / [`crate::MAX_CONNECTS`] の説明)。**原子 1 つを 8 本で取り合う**が、
/// 1 要求に 1 回なので、要求そのものの数十 us に比べれば無視できる。
pub(crate) struct Budget {
    pub(crate) used: AtomicU64,
    pub(crate) cap: u64,
}

impl Budget {
    /// 1 回ぶん取る (取れたら真)。
    pub(crate) fn take(&self) -> bool {
        self.used.fetch_add(1, Ordering::Relaxed) < self.cap
    }

    /// 上限に当たって止めたか (全部が終わってから読む)。
    pub(crate) fn spent(&self) -> bool {
        self.used.load(Ordering::Relaxed) >= self.cap
    }
}

/// `CONC` 本のスレッドで `work` を `dur` のあいだ (または `cap` 回まで) 回し、
/// (成功, 失敗, 上限に当たったか) を返す。
pub(crate) fn spawn_load<F>(
    name: &'static str,
    work: F,
    dur: Duration,
    cap: u64,
) -> (u64, u64, bool)
where
    F: Fn(Instant, &Budget) -> (u64, u64) + Send + Sync + 'static,
{
    let deadline = Instant::now() + dur;
    let work = Arc::new(work);
    let budget = Arc::new(Budget {
        used: AtomicU64::new(0),
        cap,
    });
    let mut handles: Vec<JoinHandle<(u64, u64)>> = Vec::with_capacity(CONC);
    for _ in 0..CONC {
        let work = Arc::clone(&work);
        let budget = Arc::clone(&budget);
        let spawned = thread::Builder::new().name(name.into()).spawn(move || {
            let _charge = Charge;
            work(deadline, &budget)
        });
        if let Ok(h) = spawned {
            handles.push(h);
        }
    }
    let (mut ok, mut bad) = (0u64, 0u64);
    for h in handles {
        if let Ok((o, b)) = h.join() {
            ok += o;
            bad += b;
        }
    }
    (ok, bad, budget.spent())
}

/// keep-alive の forward を回す 1 本ぶん (200 が返った数と、それ以外の数)。
pub(crate) fn forward_worker(
    proxy: SocketAddr,
    origin: SocketAddr,
    deadline: Instant,
    budget: &Budget,
) -> (u64, u64) {
    let request = format!("GET http://{0}/ HTTP/1.1\r\nHost: {0}\r\n\r\n", origin).into_bytes();
    let (mut ok, mut bad) = (0u64, 0u64);
    let mut conn: Option<(TcpStream, BufReader<TcpStream>)> = None;
    let mut line = String::new();
    // **秒数の判定が先**。上限を取るのは「まだ時間がある」ときだけ (時間切れで
    // 抜けた回まで上限を食うと、当たっていないのに当たったことになる)
    while Instant::now() < deadline && budget.take() {
        if conn.is_none() {
            let Ok(sock) = TcpStream::connect(proxy) else {
                bad += 1;
                return (ok, bad);
            };
            let _ = sock.set_nodelay(true);
            let Ok(clone) = sock.try_clone() else {
                bad += 1;
                return (ok, bad);
            };
            conn = Some((sock, BufReader::new(clone)));
        }
        let Some((sock, reader)) = conn.as_mut() else {
            break;
        };
        match sock
            .write_all(&request)
            .and_then(|()| read_response(reader, &mut line))
        {
            Ok((200, keep)) => {
                ok += 1;
                if !keep {
                    conn = None;
                }
            }
            // 403 (ACL) や 503 (上限) はここに来る。数えるだけで CPU の割り算には使わない
            Ok((_, _)) => {
                bad += 1;
                conn = None;
            }
            Err(_) => {
                bad += 1;
                conn = None;
            }
        }
    }
    (ok, bad)
}

/// CONNECT を張っては閉じる 1 本ぶん (確立できた本数と、断られた数)。
pub(crate) fn connect_worker(
    proxy: SocketAddr,
    sink: SocketAddr,
    deadline: Instant,
    budget: &Budget,
) -> (u64, u64) {
    let (mut ok, mut bad) = (0u64, 0u64);
    while Instant::now() < deadline && budget.take() {
        match open_tunnel(proxy, sink) {
            Ok(sock) => {
                ok += 1;
                let _ = sock.shutdown(Shutdown::Both);
            }
            Err(_) => {
                bad += 1;
                // 断られ続けるなら待っても無駄なので、少し休んでから次 (ここで回り続けて
                // CPU を焼くと、測っているものが「403 を返す費用」になってしまう)
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
    (ok, bad)
}

/// 応答を 1 つ読み切る。戻り値は (状態コード, この接続を使い続けてよいか)。
fn read_response(reader: &mut BufReader<TcpStream>, line: &mut String) -> io::Result<(u16, bool)> {
    let mut status = 0u16;
    let mut length: Option<u64> = None;
    let mut keep = true;
    let mut first = true;
    loop {
        line.clear();
        if reader.read_line(line)? == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        if first {
            first = false;
            status = line
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().ok();
            } else if k.eq_ignore_ascii_case("connection") && v.trim().eq_ignore_ascii_case("close")
            {
                keep = false;
            }
        }
    }
    let mut body = [0u8; 16 * 1024];
    let mut left = length.unwrap_or(0);
    while left > 0 {
        let want = left.min(body.len() as u64) as usize;
        reader.read_exact(&mut body[..want])?;
        left -= want as u64;
    }
    Ok((status, keep))
}

/// CONNECT を張り、`200` を読み切ったソケットを返す。
///
/// `BufReader` を残さないのは `crates/bench` の `open_tunnel_bare` と同じ理由
/// (CONNECT の応答のあとには何も続かないので、1 バイトずつ読んでも取りこぼさない)。
fn open_tunnel(proxy: SocketAddr, target: SocketAddr) -> io::Result<TcpStream> {
    let mut sock = TcpStream::connect(proxy)?;
    sock.set_nodelay(true)?;
    let req = format!("CONNECT {0} HTTP/1.1\r\nHost: {0}\r\n\r\n", target);
    sock.write_all(req.as_bytes())?;
    let mut head = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if sock.read(&mut byte)? == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        head.push(byte[0]);
        if head.len() > 4096 {
            return Err(io::Error::other("CONNECT response header too long"));
        }
    }
    if !head.starts_with(b"HTTP/1.1 200") {
        return Err(io::Error::other("CONNECT was refused"));
    }
    Ok(sock)
}
