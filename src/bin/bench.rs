//! `std` だけで書いたベンチ。Python 版 (`scripts/bench.py`) と同じ 3 種を測るが、
//! 負荷側もオリジンも Rust なのでプロキシ自身の限界が見える。
//!
//! ```text
//! cargo run --release --bin bench -- --proxy 127.0.0.1:18080 --conc 8 --seconds 5
//! ```
//!
//! 測るもの:
//!   1. direct  : オリジン直結の要求/秒 (ベンチが律速していないことの確認。5 万 req/s 以上出ること)
//!   2. forward : 平文 HTTP をプロキシ経由で転送したときの要求/秒と p50/p99
//!   3. tunnel  : CONNECT トンネル 1 本のスループット (MiB/s)
//!   4. connect : CONNECT の確立/秒 (短命トンネル)

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// トンネルのスループット計測で送るバイト数。
const SINK_BYTES: u64 = 256 << 20;

struct Args {
    proxy: Option<String>,
    conc: usize,
    seconds: u64,
    body_bytes: usize,
}

fn usage() -> ! {
    eprintln!(
        "usage: bench [--proxy HOST:PORT] [--conc N] [--seconds N] [--body-bytes N]\n\
         \n\
         Without --proxy only the direct (origin) baseline is measured."
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut args = Args {
        proxy: None,
        conc: 8,
        seconds: 5,
        body_bytes: 1024,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--proxy" => args.proxy = Some(value()),
            "--conc" => args.conc = value().parse().unwrap_or_else(|_| usage()),
            "--seconds" => args.seconds = value().parse().unwrap_or_else(|_| usage()),
            "--body-bytes" => args.body_bytes = value().parse().unwrap_or_else(|_| usage()),
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    if args.conc == 0 || args.seconds == 0 {
        usage();
    }
    args
}

// ---------------------------------------------------------------- オリジン

/// 固定応答を 1 回の `write_all` で返す keep-alive オリジン。
fn spawn_origin(body_bytes: usize) -> io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let body = vec![b'x'; body_bytes];
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    let response = Arc::new(response);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let response = Arc::clone(&response);
            thread::spawn(move || {
                let _ = stream.set_nodelay(true);
                let _ = serve_origin(stream, &response);
            });
        }
    });
    Ok(addr)
}

/// 要求ヘッダーを読み飛ばし、1 要求ごとに固定応答を返す (本文付きの要求は来ない前提)。
fn serve_origin(mut stream: TcpStream, response: &[u8]) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    loop {
        let mut headers = 0usize;
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            headers += 1;
            if headers > 256 {
                return Ok(());
            }
        }
        stream.write_all(response)?;
    }
}

/// 接続されたら `SINK_BYTES` を送って閉じる (トンネルのスループット用)。
fn spawn_blaster() -> io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let _ = stream.set_nodelay(true);
                let chunk = vec![b'y'; 1 << 20];
                let mut stream = stream;
                let mut sent = 0u64;
                while sent < SINK_BYTES {
                    if stream.write_all(&chunk).is_err() {
                        break;
                    }
                    sent += chunk.len() as u64;
                }
                let _ = stream.shutdown(Shutdown::Both);
            });
        }
    });
    Ok(addr)
}

/// 接続されたらすぐ閉じる (短命トンネルの相手)。
fn spawn_sink() -> io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    });
    Ok(addr)
}

// ---------------------------------------------------------------- 集計

struct Report {
    ops: u64,
    bytes: u64,
    elapsed: Duration,
    latencies_us: Vec<u32>,
}

impl Report {
    fn percentile(&self, p: f64) -> f64 {
        if self.latencies_us.is_empty() {
            return 0.0;
        }
        let idx = ((self.latencies_us.len() - 1) as f64 * p).round() as usize;
        self.latencies_us[idx] as f64 / 1000.0
    }

    fn print(&self, label: &str) {
        let secs = self.elapsed.as_secs_f64().max(1e-9);
        println!(
            "{:<8} {:>9.0} op/s  {:>8.1} MiB/s  p50 {:>7.3} ms  p99 {:>7.3} ms  ({} ops in {:.1}s)",
            label,
            self.ops as f64 / secs,
            self.bytes as f64 / secs / (1024.0 * 1024.0),
            self.percentile(0.50),
            self.percentile(0.99),
            self.ops,
            secs,
        );
    }
}

/// `conc` 本のスレッドで `seconds` 秒だけ `work` を回す。`work` は 1 回の操作の
/// (バイト数) を返し、レイテンシは呼び出し側で測る。
fn run_load<F>(conc: usize, seconds: u64, work: F) -> Report
where
    F: Fn(&AtomicBool, &mut Vec<u32>, &mut u64) + Send + Sync + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let work = Arc::new(work);
    let started = Instant::now();
    let mut handles = Vec::with_capacity(conc);
    for _ in 0..conc {
        let stop = Arc::clone(&stop);
        let work = Arc::clone(&work);
        handles.push(thread::spawn(move || {
            let mut lat = Vec::new();
            let mut bytes = 0u64;
            work(&stop, &mut lat, &mut bytes);
            (lat, bytes)
        }));
    }
    thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    let mut latencies_us = Vec::new();
    let mut bytes = 0u64;
    for h in handles {
        if let Ok((lat, b)) = h.join() {
            latencies_us.extend(lat);
            bytes += b;
        }
    }
    let elapsed = started.elapsed();
    latencies_us.sort_unstable();
    Report {
        ops: latencies_us.len() as u64,
        bytes,
        elapsed,
        latencies_us,
    }
}

// ---------------------------------------------------------------- HTTP

/// 応答を 1 つ読み切る。戻り値は読んだバイト数と keep-alive 可否。
fn read_response(reader: &mut BufReader<TcpStream>, line: &mut String) -> io::Result<(u64, bool)> {
    let mut total = 0u64;
    let mut length: Option<u64> = None;
    let mut keep = true;
    loop {
        line.clear();
        let n = reader.read_line(line)?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        total += n as u64;
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
    let mut body = vec![0u8; 64 * 1024];
    let mut left = length.unwrap_or(0);
    while left > 0 {
        let want = left.min(body.len() as u64) as usize;
        reader.read_exact(&mut body[..want])?;
        left -= want as u64;
        total += want as u64;
    }
    Ok((total, keep))
}

/// keep-alive で HTTP 要求を投げ続ける負荷。`request` は 1 要求ぶんのバイト列。
fn http_load(target: SocketAddr, request: Vec<u8>, conc: usize, seconds: u64) -> Report {
    run_load(conc, seconds, move |stop, lat, bytes| {
        let mut conn: Option<(TcpStream, BufReader<TcpStream>)> = None;
        let mut line = String::new();
        while !stop.load(Ordering::Relaxed) {
            if conn.is_none() {
                let Ok(s) = TcpStream::connect(target) else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let _ = s.set_nodelay(true);
                let Ok(clone) = s.try_clone() else { continue };
                conn = Some((s, BufReader::new(clone)));
            }
            let (sock, reader) = conn.as_mut().expect("connected");
            let t0 = Instant::now();
            let outcome = sock
                .write_all(&request)
                .and_then(|()| read_response(reader, &mut line));
            match outcome {
                Ok((n, keep)) => {
                    lat.push(t0.elapsed().as_micros().min(u32::MAX as u128) as u32);
                    *bytes += n;
                    if !keep {
                        conn = None;
                    }
                }
                Err(_) => conn = None,
            }
        }
    })
}

/// CONNECT を張って 200 応答まで読む。
fn open_tunnel(
    proxy: SocketAddr,
    target: SocketAddr,
) -> io::Result<(TcpStream, BufReader<TcpStream>)> {
    let mut sock = TcpStream::connect(proxy)?;
    sock.set_nodelay(true)?;
    let req = format!("CONNECT {0} HTTP/1.1\r\nHost: {0}\r\n\r\n", target);
    sock.write_all(req.as_bytes())?;
    let mut reader = BufReader::new(sock.try_clone()?);
    let mut line = String::new();
    let mut status_ok = false;
    let mut first = true;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        if first {
            status_ok = line.contains(" 200 ");
            first = false;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    if !status_ok {
        return Err(io::Error::other("CONNECT was refused"));
    }
    Ok((sock, reader))
}

fn parse_addr(s: &str) -> SocketAddr {
    use std::net::ToSocketAddrs;
    s.to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .unwrap_or_else(|| {
            eprintln!("bench: cannot resolve {}", s);
            std::process::exit(2);
        })
}

fn main() {
    let args = parse_args();
    let origin = spawn_origin(args.body_bytes).expect("origin");
    println!(
        "bench: conc={} seconds={} body={}B origin={}",
        args.conc, args.seconds, args.body_bytes, origin
    );

    let direct_req = format!("GET / HTTP/1.1\r\nHost: {}\r\n\r\n", origin).into_bytes();
    http_load(origin, direct_req, args.conc, args.seconds).print("direct");

    let Some(proxy) = args.proxy.as_deref() else {
        return;
    };
    let proxy = parse_addr(proxy);

    let forward_req = format!("GET http://{0}/ HTTP/1.1\r\nHost: {0}\r\n\r\n", origin).into_bytes();
    http_load(proxy, forward_req, args.conc, args.seconds).print("forward");

    // トンネル 1 本のスループット (時間ではなく転送量で終わる)
    let blaster = spawn_blaster().expect("blaster");
    match open_tunnel(proxy, blaster) {
        Ok((_sock, mut reader)) => {
            let mut buf = vec![0u8; 256 * 1024];
            let t0 = Instant::now();
            let mut got = 0u64;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got += n as u64,
                }
                if got >= SINK_BYTES {
                    break;
                }
            }
            let secs = t0.elapsed().as_secs_f64().max(1e-9);
            println!(
                "{:<8} {:>9} op/s  {:>8.1} MiB/s  ({} MiB through one tunnel)",
                "tunnel",
                1,
                got as f64 / secs / (1024.0 * 1024.0),
                got / (1 << 20)
            );
        }
        Err(e) => println!("tunnel   skipped: {}", e),
    }

    // CONNECT の確立/秒
    let sink = spawn_sink().expect("sink");
    run_load(args.conc, args.seconds, move |stop, lat, _bytes| {
        while !stop.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            match open_tunnel(proxy, sink) {
                Ok((sock, _)) => {
                    lat.push(t0.elapsed().as_micros().min(u32::MAX as u128) as u32);
                    let _ = sock.shutdown(Shutdown::Both);
                }
                Err(_) => thread::sleep(Duration::from_millis(2)),
            }
        }
    })
    .print("connect");
}
