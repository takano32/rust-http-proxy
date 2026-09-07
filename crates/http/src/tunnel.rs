use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::Arc;
#[cfg(not(target_os = "linux"))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(target_os = "linux"))]
use std::thread;
use std::time::{Duration, Instant};

use crate::log::{Access, access};
#[cfg(not(target_os = "linux"))]
use crate::log_trace;
use crate::metrics::{HostOutcome, Metrics};
use crate::net;
use crate::{log_debug, log_warn};

/// `prefix` はリクエストヘッダーの直後に既に読み込んでしまったバイト列 (先にサーバーへ渡す)。
pub fn handle_connect(
    mut client: TcpStream,
    target: &str,
    prefix: &[u8],
    timeout: Duration,
    idle: Option<Duration>,
    conn_id: usize,
    metrics: Arc<Metrics>,
) -> io::Result<()> {
    let started = Instant::now();
    let client_ip = client
        .peer_addr()
        .map(|a| net::canonical_ip(a.ip()).to_string())
        .unwrap_or_else(|_| "-".to_string());
    let addr_str = net::with_default_port(target, 443);

    log_debug!(Some(conn_id), "start CONNECT {}", addr_str);

    let mut server = match connect_with_timeout(&addr_str, timeout) {
        Ok(s) => s,
        Err(e) => {
            log_warn!(
                Some(conn_id),
                "502 Bad Gateway: connect {} failed: {}",
                addr_str,
                e
            );
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
            metrics.record_host_timed(
                &format!("connect://{}", addr_str),
                HostOutcome::Error,
                0,
                started.elapsed(),
            );
            metrics.record_client(&client_ip, HostOutcome::Error, 0, Some(started.elapsed()));
            access(
                conn_id,
                &Access {
                    client: &client_ip,
                    method: "CONNECT",
                    target: &addr_str,
                    version: "HTTP/1.1",
                    status: "502",
                    bytes: 0,
                    duration_ms: started.elapsed().as_secs_f64() * 1000.0,
                    cache: "BYPASS",
                },
            );
            return Err(e);
        }
    };

    // ホスト別の応答時間は接続確立まで (トンネル自体の寿命は応答時間ではない)
    let connect_took = started.elapsed();
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    client.flush()?;
    if !prefix.is_empty() {
        server.write_all(prefix)?;
    }

    let transferred = tunnel(client, server, idle)?;
    metrics.add_bytes(transferred);
    metrics.record_host_timed(
        &format!("connect://{}", addr_str),
        HostOutcome::Bypass,
        transferred,
        connect_took,
    );
    metrics.record_client(
        &client_ip,
        HostOutcome::Bypass,
        transferred,
        Some(connect_took),
    );
    access(
        conn_id,
        &Access {
            client: &client_ip,
            method: "CONNECT",
            target: &addr_str,
            version: "HTTP/1.1",
            status: "200",
            bytes: transferred,
            duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            cache: "BYPASS(tunnel)",
        },
    );
    Ok(())
}

/// 名前解決して接続する (IPv6 / IPv4 を Happy Eyeballs で並行に試す)。
pub fn connect_with_timeout(addr_str: &str, timeout: Duration) -> io::Result<TcpStream> {
    net::connect(addr_str, timeout)
}

/// 双方向にデータを中継し、転送した合計バイト数を返す。
///
/// Linux では 1 スレッドで `poll(2)` を回し、`splice(2)` でカーネル内をコピーする
/// (接続あたりのスレッドが 3 本から 1 本に減り、ユーザー空間へのコピーも無くなる)。
/// `idle` 秒だけ双方向とも動きが無ければ閉じる (`None` で無期限)。
/// Linux 以外では従来どおり 2 スレッドの `io::copy`。
pub fn tunnel(client: TcpStream, server: TcpStream, idle: Option<Duration>) -> io::Result<u64> {
    #[cfg(target_os = "linux")]
    {
        relay::run(client, server, idle)
    }
    #[cfg(not(target_os = "linux"))]
    {
        copy_both_ways(client, server, idle)
    }
}

/// 2 スレッドで双方向に `io::copy` する従来版 (Linux 以外)。
#[cfg(not(target_os = "linux"))]
fn copy_both_ways(
    mut client: TcpStream,
    mut server: TcpStream,
    idle: Option<Duration>,
) -> io::Result<u64> {
    let mut client_clone = client.try_clone()?;
    let mut server_clone = server.try_clone()?;
    // poll が無いので、アイドル打ち切りは読み取りタイムアウトで代用する
    for s in [&client, &server, &client_clone, &server_clone] {
        let _ = s.set_read_timeout(idle);
        let _ = s.set_write_timeout(None);
    }
    let total = Arc::new(AtomicU64::new(0));

    let up = Arc::clone(&total);
    let t1 = thread::spawn(move || {
        let n = io::copy(&mut client, &mut server).unwrap_or(0);
        up.fetch_add(n, Ordering::Relaxed);
        let _ = server.shutdown(std::net::Shutdown::Write);
        n
    });

    let down = Arc::clone(&total);
    let t2 = thread::spawn(move || {
        let n = io::copy(&mut server_clone, &mut client_clone).unwrap_or(0);
        down.fetch_add(n, Ordering::Relaxed);
        let _ = client_clone.shutdown(std::net::Shutdown::Write);
        n
    });

    let sent = t1.join().unwrap_or(0);
    let received = t2.join().unwrap_or(0);
    log_trace!(None, "tunnel finished: {}B up / {}B down", sent, received);
    Ok(total.load(Ordering::Relaxed))
}

/// Linux の 1 スレッド中継 (`poll` + `splice`)。
#[cfg(target_os = "linux")]
mod relay {
    use std::io::{self, Read, Write};
    use std::net::{Shutdown, TcpStream};
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    use crate::log_trace;
    use crate::sys::{self, POLLERR, POLLHUP, POLLIN, POLLOUT, Pipe, PollFd};

    /// 1 回の splice / read で動かす最大バイト数 (パイプ容量と同じ)。
    const CHUNK: usize = 1 << 20;

    /// 片方向の中継。データはパイプ (splice) か、使えなければ中間バッファに置く。
    /// 最初にデータが動くまで作らない (アイドルのトンネルは資源を持たない)。
    enum Relay {
        Unset,
        Pipe(Pipe),
        Buf(Vec<u8>),
    }

    struct Dir {
        /// `socks` の添字 (0 = クライアント, 1 = サーバー)
        src: usize,
        dst: usize,
        relay: Relay,
        /// まだ相手に渡していないバイト数
        pending: usize,
        /// バッファ方式で次に書き出す位置
        offset: usize,
        /// poll が「読める」と言った (最初は分からないので待つ側から始める)
        readable: bool,
        src_eof: bool,
        done: bool,
        moved: u64,
    }

    impl Dir {
        fn new(src: usize, dst: usize) -> Dir {
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
        fn fill(&mut self, socks: &[TcpStream; 2]) -> io::Result<usize> {
            if matches!(self.relay, Relay::Unset) {
                self.relay = match Pipe::new() {
                    Ok(p) => {
                        p.set_capacity(CHUNK as i32);
                        Relay::Pipe(p)
                    }
                    Err(_) => Relay::Buf(vec![0u8; 64 * 1024]),
                };
            }
            match &mut self.relay {
                Relay::Unset => unreachable!("just initialised"),
                Relay::Pipe(pipe) => {
                    match sys::splice_move(socks[self.src].as_raw_fd(), pipe.write_fd, CHUNK) {
                        Ok(n) => Ok(n),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(e),
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
        fn drain(&mut self, socks: &[TcpStream; 2]) -> io::Result<usize> {
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
    }

    pub fn run(client: TcpStream, server: TcpStream, idle: Option<Duration>) -> io::Result<u64> {
        client.set_nonblocking(true)?;
        server.set_nonblocking(true)?;
        let socks = [client, server];
        let mut dirs = [Dir::new(0, 1), Dir::new(1, 0)];
        let timeout_ms: i32 = match idle {
            Some(d) => d.as_millis().min(i32::MAX as u128) as i32,
            None => -1,
        };
        let mut total = 0u64;

        'outer: loop {
            let mut progressed = false;
            for d in dirs.iter_mut() {
                if d.done {
                    continue;
                }
                // 送信元 → 中継 (読めると分かってから中継バッファを用意する)
                if !d.src_eof && d.pending == 0 && d.readable {
                    match d.fill(&socks) {
                        Ok(0) => {
                            d.src_eof = true;
                            progressed = true;
                        }
                        Ok(n) => {
                            d.pending = n;
                            d.offset = 0;
                            progressed = true;
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => d.readable = false,
                        Err(_) => {
                            d.src_eof = true;
                            progressed = true;
                        }
                    }
                }
                // 中継 → 送信先
                while d.pending > 0 {
                    match d.drain(&socks) {
                        Ok(0) => break,
                        Ok(n) => {
                            d.pending -= n;
                            d.offset += n;
                            d.moved += n as u64;
                            total += n as u64;
                            progressed = true;
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        // 送信先が閉じた: この方向は終わり、相手にも伝える
                        Err(_) => {
                            d.pending = 0;
                            d.src_eof = true;
                            progressed = true;
                            break;
                        }
                    }
                }
                if d.src_eof && d.pending == 0 {
                    let _ = socks[d.dst].shutdown(Shutdown::Write);
                    d.done = true;
                    progressed = true;
                }
            }
            if dirs.iter().all(|d| d.done) {
                break;
            }
            if progressed {
                continue;
            }

            // どちらも動かなかったので待つ
            let mut events = [0i16; 2];
            for d in dirs.iter() {
                if d.done {
                    continue;
                }
                if !d.src_eof && d.pending == 0 {
                    events[d.src] |= POLLIN;
                }
                if d.pending > 0 {
                    events[d.dst] |= POLLOUT;
                }
            }
            let mut fds = [
                PollFd::new(socks[0].as_raw_fd(), events[0]),
                PollFd::new(socks[1].as_raw_fd(), events[1]),
            ];
            if sys::poll_fds(&mut fds, timeout_ms)? == 0 && timeout_ms >= 0 {
                log_trace!(None, "tunnel idle timeout after {}ms", timeout_ms);
                break 'outer;
            }
            for d in dirs.iter_mut() {
                // HUP / ERR でも read して EOF を確かめる
                if fds[d.src].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
                    d.readable = true;
                }
            }
        }

        log_trace!(
            None,
            "tunnel finished: {}B up / {}B down",
            dirs[0].moved,
            dirs[1].moved
        );
        Ok(total)
    }
}
