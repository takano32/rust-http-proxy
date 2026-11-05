//! HTTP の打ち手: 応答を 1 つ読み切る、keep-alive の負荷、CONNECT を張る、トンネルを読み続ける。

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crate::report::{Report, run_load};

/// トンネルのスループット計測で 1 回に読む大きさ。
///
/// 送る側 ([`crate::origin::spawn_blaster`]) は相手が閉じるまで送り続け、受ける側は `--seconds` 秒で止める。
/// **転送量ではなく時間で終わらせる**のは、他のモードと揃えるためと、短すぎる計測だと
/// `/proc/<pid>/stat` の 10 ms 刻みが結果を丸めてしまうため。256 MiB 固定だった頃は
/// 0.2 秒しか走らず、プロキシの user CPU が 1 tick 未満で「0.00 us/MiB」に見えていた (T10.8)。
const TUNNEL_READ_BYTES: usize = 256 * 1024;

/// 応答を 1 つ読み切る。戻り値は読んだバイト数と keep-alive 可否。
pub(crate) fn read_response(
    reader: &mut BufReader<TcpStream>,
    line: &mut String,
) -> io::Result<(u64, bool)> {
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
pub(crate) fn http_load(
    target: SocketAddr,
    request: Vec<u8>,
    conc: usize,
    seconds: u64,
    keepalive: bool,
) -> Report {
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
                    if !keep || !keepalive {
                        conn = None;
                    }
                }
                Err(_) => conn = None,
            }
        }
    })
}

/// CONNECT を張って 200 応答まで読む。
///
/// `target` は**文字列**。IP リテラル (`127.0.0.1:38123`) なら
/// プロキシは名前解決を通らず、名前 (`multi.test:38123`) なら通る
/// (`--only connect` と `--only connect-multi` の違いはここだけ)。
pub(crate) fn open_tunnel(
    proxy: SocketAddr,
    target: &str,
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

/// 張ったトンネルを `seconds` 秒読み続け、受け取ったバイト数を返す。
///
/// 読み終わったら**その場で切る**。切らないと blaster が送り続けたまま
/// 次のモードの計測に混ざる。
pub(crate) fn drain_tunnel(sock: TcpStream, mut reader: BufReader<TcpStream>, seconds: u64) -> u64 {
    let mut buf = vec![0u8; TUNNEL_READ_BYTES];
    let deadline = Duration::from_secs(seconds);
    let t0 = Instant::now();
    let mut got = 0u64;
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n as u64,
        }
        if t0.elapsed() >= deadline {
            break;
        }
    }
    let _ = sock.shutdown(Shutdown::Both);
    got
}

/// CONNECT を張り、`200` を読み切ったソケットだけを返す (`BufReader` を残さない)。
///
/// アイドルトンネルを 5,000 本握るモード用。[`open_tunnel`] は 1 本につき
/// `try_clone` した記述子と 8 KiB の `BufReader` を持つので、本数ぶん積むと
/// ベンチ側が先に重くなる。CONNECT の応答のあとには何も続かないので 1 バイトずつ読む。
pub(crate) fn open_tunnel_bare(proxy: SocketAddr, target: SocketAddr) -> io::Result<TcpStream> {
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

/// keep-alive 接続を 1 本張り、**1 要求だけ通してから**そのソケットを返す。
///
/// 応答を読むための [`BufReader`] (と `try_clone` した記述子) はここで捨てる。
/// 2,000 本ぶん抱えるとベンチ側が先に重くなるため ([`open_tunnel_bare`] と同じ理由)。
/// 応答のあとに続くものは無いので、捨てても取りこぼさない。
pub(crate) fn open_idle_conn(
    proxy: SocketAddr,
    request: &[u8],
    line: &mut String,
) -> io::Result<TcpStream> {
    let mut sock = TcpStream::connect(proxy)?;
    sock.set_nodelay(true)?;
    let mut reader = BufReader::new(sock.try_clone()?);
    sock.write_all(request)?;
    let (_, keep) = read_response(&mut reader, line)?;
    if !keep {
        return Err(io::Error::other("proxy closed the keep-alive connection"));
    }
    drop(reader);
    Ok(sock)
}
