//! 個票 1 本ぶんの再生: CONNECT を張り、合図 → 上り → 下りを流し、寿命まで握る。

use std::io::{self, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::origin::{MAX_DOWN, PREAMBLE, encode_preamble};
use super::shots::Shot;
use crate::http::open_tunnel;

/// 再生の途中経過 (全スレッドで 1 つ)。
#[derive(Default)]
pub(super) struct Tally {
    /// CONNECT が張れなかった数
    pub(super) failed: AtomicU64,
    /// 寿命より前にプロキシ側から閉じられた数 (T13.2 の追い出しはここに出る)
    pub(super) cut: AtomicU64,
    pub(super) up: AtomicU64,
    pub(super) down: AtomicU64,
    /// CONNECT の確立時間 (us)
    pub(super) connect_us: Mutex<Vec<u32>>,
}

/// 個票 1 本ぶんを再生する。
pub(super) fn run_one(
    proxy: SocketAddr,
    origin_port: u16,
    shot: &Shot,
    speed: f64,
    deadline: Option<Instant>,
    tally: &Tally,
) {
    // ホスト名はそのまま、ポートだけ内蔵オリジンのものに置き換える
    let target = format!("{}:{}", shot.host, origin_port);
    let t0 = Instant::now();
    let Ok((sock, mut reader)) = open_tunnel(proxy, &target) else {
        tally.failed.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let connect_us = t0.elapsed().as_micros().min(u32::MAX as u128) as u32;
    tally
        .connect_us
        .lock()
        .expect("connect_us")
        .push(connect_us);

    let life_ms = (shot.secs as f64 * 1000.0 / speed.max(1e-9)).round() as u64;
    let mut until = t0 + Duration::from_millis(life_ms);
    if let Some(d) = deadline {
        until = until.min(d);
    }
    // 読みが永久に止まらないように、寿命 + 5 秒で必ず諦める
    let _ = sock.set_read_timeout(Some(
        (until.saturating_duration_since(Instant::now()) + Duration::from_secs(5))
            .max(Duration::from_millis(1)),
    ));

    let alive =
        transfer(&sock, &mut reader, shot, tally).is_ok() && hold(&sock, &mut reader, until);
    if !alive {
        tally.cut.fetch_add(1, Ordering::Relaxed);
    }
    let _ = sock.shutdown(Shutdown::Both);
}

/// 合図 → 上り → 下り。**上りは最低 16 バイト** (合図) 流れる。
fn transfer(
    mut sock: &TcpStream,
    reader: &mut BufReader<TcpStream>,
    shot: &Shot,
    tally: &Tally,
) -> io::Result<()> {
    sock.write_all(&encode_preamble(shot.down, shot.rtt_ms))?;
    tally.up.fetch_add(PREAMBLE, Ordering::Relaxed);

    // 合図のぶんを引いた残りを上りとして流す (個票の上りが 16 B 未満なら合図だけ)
    let mut left = shot.up.saturating_sub(PREAMBLE);
    if left > 0 {
        let chunk = vec![b'x'; left.min(64 * 1024) as usize];
        while left > 0 {
            let want = left.min(chunk.len() as u64) as usize;
            sock.write_all(&chunk[..want])?;
            tally.up.fetch_add(want as u64, Ordering::Relaxed);
            left -= want as u64;
        }
    }

    let mut left = shot.down.min(MAX_DOWN);
    if left > 0 {
        let mut buf = vec![0u8; left.min(64 * 1024) as usize];
        while left > 0 {
            let want = left.min(buf.len() as u64) as usize;
            let n = reader.read(&mut buf[..want])?;
            if n == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            tally.down.fetch_add(n as u64, Ordering::Relaxed);
            left -= n as u64;
        }
    }
    Ok(())
}

/// 寿命が尽きるまで握る。**プロキシ側から閉じられたら `false`**
/// (T13.2 の追い出しはここで見える)。
fn hold(sock: &TcpStream, reader: &mut BufReader<TcpStream>, until: Instant) -> bool {
    let mut buf = [0u8; 256];
    loop {
        let now = Instant::now();
        if now >= until {
            return true;
        }
        // 0 の待ちは `SO_RCVTIMEO` が受け付けないので 1 ms を下限にする
        let wait = (until - now).max(Duration::from_millis(1));
        if sock.set_read_timeout(Some(wait)).is_err() {
            thread::sleep(wait);
            return true;
        }
        match reader.read(&mut buf) {
            Ok(0) => return false,
            Ok(_) => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return false,
        }
    }
}
