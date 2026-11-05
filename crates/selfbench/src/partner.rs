//! 相手役 (内蔵オリジンと sink。T34.80 で lib.rs から分けた)。

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use proxy_base::sync::LockExt;

use crate::cpu::Charge;

/// 内蔵オリジンが返す本文の大きさ。§1 の既定 (`--body-bytes 1024`) と揃えてある。
const BODY_BYTES: usize = 1024;

/// 内蔵オリジンが「もう終わり」に気づくまでの待ち (読み取りタイムアウト)。
const ORIGIN_POLL: Duration = Duration::from_millis(50);

/// 自分の中の相手役 (forward の固定応答オリジン / CONNECT の sink)。
///
/// **どちらも `127.0.0.1` の使い捨てポート**で、外からは何も来ない前提。
/// 止めるときは旗を立ててから自分で 1 本繋いで `accept` を起こす。
pub(crate) struct Server {
    pub(crate) addr: SocketAddr,
    stop: Arc<AtomicBool>,
    acceptor: Option<JoinHandle<()>>,
    conns: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Server {
    /// 固定応答を返す keep-alive オリジン (forward の相手)。
    pub(crate) fn origin() -> io::Result<Server> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let conns: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        // 応答は 1 回の `write_all` で出す。`no-store` なのでキャッシュには入らない
        // (§1 のベンチの既定と同じ条件 = 毎回オリジンまで行く)
        let body = vec![b'x'; BODY_BYTES];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let response = Arc::new(response);
        let acceptor = {
            let stop = Arc::clone(&stop);
            let conns = Arc::clone(&conns);
            thread::Builder::new()
                .name("sb-origin".into())
                .spawn(move || {
                    let _charge = Charge;
                    for stream in listener.incoming() {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let Ok(stream) = stream else { continue };
                        let response = Arc::clone(&response);
                        let stop = Arc::clone(&stop);
                        let spawned =
                            thread::Builder::new()
                                .name("sb-conn".into())
                                .spawn(move || {
                                    let _charge = Charge;
                                    serve_origin(stream, &response, &stop);
                                });
                        if let Ok(h) = spawned {
                            conns.locked().push(h);
                        }
                    }
                })?
        };
        Ok(Server {
            addr,
            stop,
            acceptor: Some(acceptor),
            conns,
        })
    }

    /// 繋がれたらすぐ閉じる相手 (CONNECT の sink)。1 スレッドだけで受ける。
    pub(crate) fn sink() -> io::Result<Server> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let acceptor = {
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("sb-sink".into())
                .spawn(move || {
                    let _charge = Charge;
                    for stream in listener.incoming() {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        if let Ok(s) = stream {
                            let _ = s.shutdown(Shutdown::Both);
                        }
                    }
                })?
        };
        Ok(Server {
            addr,
            stop,
            acceptor: Some(acceptor),
            conns: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// 相手役を止め、**スレッドが自分の CPU を足し終えるまで待つ**。
    pub(crate) fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // `accept` で寝ているスレッドを起こすためだけの 1 本 (旗が立っているので受けたら抜ける)
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.acceptor.take() {
            let _ = h.join();
        }
        let handles: Vec<JoinHandle<()>> = self.conns.locked().drain(..).collect();
        for h in handles {
            let _ = h.join();
        }
    }
}

/// 要求の終わり (空行) を数えては固定応答を返す。**行の中身は読まない** (固定応答なので要らない)。
///
/// 読み取りタイムアウトを置いてあるのは、止めるときに気づくため
/// (プロキシはオリジンへの接続をプールに持ったまま次の要求を待つので、閉じてくれない)。
fn serve_origin(mut stream: TcpStream, response: &[u8], stop: &AtomicBool) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(ORIGIN_POLL));
    let mut buf = [0u8; 2048];
    let mut matched = 0usize;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                for _ in 0..count_requests(&mut matched, &buf[..n]) {
                    if stream.write_all(response).is_err() {
                        return;
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// `data` の中に「ヘッダーの終わり」(`\r\n\r\n`) がいくつあるかを数える。
///
/// `matched` は**前の呼び出しからの続き**(何バイトまで一致していたか)。読みが途中で
/// 切れても数え落とさないように、呼ぶ側が持ち回る。
pub(crate) fn count_requests(matched: &mut usize, data: &[u8]) -> usize {
    const END: &[u8; 4] = b"\r\n\r\n";
    let mut hits = 0usize;
    for &b in data {
        if b == END[*matched] {
            *matched += 1;
            if *matched == END.len() {
                *matched = 0;
                hits += 1;
            }
        } else {
            *matched = usize::from(b == b'\r');
        }
    }
    hits
}
