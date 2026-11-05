//! Linux 以外の中継 (2 スレッドで双方向に `io::copy` する)。
use std::io;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crate::log_trace;

/// 双方向にデータを中継し、(上り, 下り) のバイト数を返す (Linux 以外)。
///
/// `idle` 秒だけ双方向とも動きが無ければ閉じる (`None` で無期限)。
/// Linux では 1 スレッドで `poll(2)` を回して `splice(2)` でカーネル内をコピーする
/// ([`relay`]。接続あたりのスレッドが 3 本から 1 本に減り、ユーザー空間へのコピーも無くなる)。
#[cfg(not(target_os = "linux"))]
pub fn tunnel(
    client: TcpStream,
    server: TcpStream,
    idle: Option<Duration>,
) -> io::Result<(u64, u64)> {
    copy_both_ways(client, server, idle)
}

/// 2 スレッドで双方向に `io::copy` する従来版 (Linux 以外)。
#[cfg(not(target_os = "linux"))]
fn copy_both_ways(
    mut client: TcpStream,
    mut server: TcpStream,
    idle: Option<Duration>,
) -> io::Result<(u64, u64)> {
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
    let _ = total.load(Ordering::Relaxed);
    Ok((sent, received))
}
