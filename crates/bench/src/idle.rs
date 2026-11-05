//! 握り続けるモード (`--only idle-tunnels` / `--only idle-conns`)。プロキシ側のスレッド数と RSS を見る。

use std::io::{self, Read};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use crate::http::{open_idle_conn, open_tunnel_bare};
use crate::origin::spawn_holder;
use crate::report::Report;

/// `conc` 本の CONNECT を張り、`seconds` 秒そのまま握る。
///
/// プロキシ側の「アイドルなトンネル 1 本あたりのスレッドと RSS」を見るためのモード。
/// 5,000 本張るので、ベンチ側は 1 スレッドで接続を `Vec` に持つ (5,000 スレッドを作らない)。
/// プロキシの `PROXY_MAX_CONNS` (既定 4096) に当たるので、測るときは `0` か 8192 にする。
pub(crate) fn idle_tunnels(proxy: SocketAddr, conc: usize, seconds: u64) {
    let holder = spawn_holder().expect("holder");
    let mut held: Vec<TcpStream> = Vec::with_capacity(conc);
    let mut latencies_us: Vec<u32> = Vec::with_capacity(conc);
    let mut failed = 0usize;
    let started = Instant::now();
    for _ in 0..conc {
        let t0 = Instant::now();
        match open_tunnel_bare(proxy, holder) {
            Ok(sock) => {
                latencies_us.push(t0.elapsed().as_micros().min(u32::MAX as u128) as u32);
                held.push(sock);
            }
            Err(_) => failed += 1,
        }
    }
    let opened = started.elapsed();
    println!(
        "idle-tun {} tunnels open in {:.1}s ({} failed); holding {}s",
        held.len(),
        opened.as_secs_f64(),
        failed,
        seconds
    );
    // 握ったまま待つ (この間にプロキシのスレッド数と RSS を見る)
    thread::sleep(Duration::from_secs(seconds));
    // 握れた本数を「操作数」として出す (scripts/cpu-per-request.sh がこの行から読む)
    latencies_us.sort_unstable();
    Report {
        ops: held.len() as u64,
        bytes: 0,
        elapsed: started.elapsed(),
        latencies_us,
    }
    .print("idle-tun");
    drop(held);
}

/// `conc` 本の keep-alive 接続を張り、**1 要求ずつ通してから** `seconds` 秒そのまま握る。
///
/// プロキシ側の「暇な keep-alive 接続 1 本あたりのスレッドと RSS」を見るためのモード
/// ([`idle_tunnels`] の HTTP 版)。2,000 本張るので、ベンチ側は **1 スレッドで接続を `Vec` に持つ**
/// (2,000 スレッドを作らない)。プロキシの `PROXY_MAX_CONNS` に当たるので、測るときは 8192 にする。
///
/// **1 要求通してから暇にするのが大事**。要求を 1 本も通していない接続はプロキシから見ると
/// 「まだ 1 バイトも読んでいない接続」で、預ける仕組み (T6.5 の猶予) にかかる状態が違う。
/// 見たいのは「1 要求通したあと暇になった接続」の方なので、必ず 1 往復させてから握る。
///
/// **握る時間はプロキシの `PROXY_KEEPALIVE_SECS` (既定 15 秒) より短くすること。**
/// 越えると預かり所が期限切れで閉じてしまい、途中から本数が減った状態を測ることになる。
/// 最後に「まだ生きている本数」を印字するので、減っていればその行で分かる。
pub(crate) fn idle_conns(proxy: SocketAddr, origin: SocketAddr, conc: usize, seconds: u64) {
    let request = format!("GET http://{0}/ HTTP/1.1\r\nHost: {0}\r\n\r\n", origin).into_bytes();
    let mut held: Vec<TcpStream> = Vec::with_capacity(conc);
    let mut latencies_us: Vec<u32> = Vec::with_capacity(conc);
    let mut line = String::new();
    let mut failed = 0usize;
    let started = Instant::now();
    for _ in 0..conc {
        let t0 = Instant::now();
        match open_idle_conn(proxy, &request, &mut line) {
            Ok(sock) => {
                latencies_us.push(t0.elapsed().as_micros().min(u32::MAX as u128) as u32);
                held.push(sock);
            }
            Err(_) => failed += 1,
        }
    }
    let opened = started.elapsed();
    println!(
        "idle-con {} keep-alive conns open (1 request each) in {:.1}s ({} failed); holding {}s",
        held.len(),
        opened.as_secs_f64(),
        failed,
        seconds
    );
    // 握ったまま待つ (この間にプロキシのスレッド数と RSS を見る)
    thread::sleep(Duration::from_secs(seconds));
    // まだ生きている本数を数える。プロキシは黙っているはずなので、生きていれば
    // ノンブロッキングの read が WouldBlock、閉じられていれば 0 バイトの EOF になる。
    let alive = held.iter().filter(|sock| still_open(sock)).count();
    println!(
        "idle-con {} of {} still open after {}s",
        alive,
        held.len(),
        seconds
    );
    // 握れた本数を「操作数」として出す (scripts/cpu-per-request.sh がこの行から読む)
    latencies_us.sort_unstable();
    Report {
        ops: held.len() as u64,
        bytes: 0,
        elapsed: started.elapsed(),
        latencies_us,
    }
    .print("idle-con");
    drop(held);
}

/// 黙っている相手に対して、まだ開いているか (ノンブロッキングの read が `WouldBlock` なら開いている、
/// 0 バイトの EOF や他の失敗なら閉じられている)。
fn still_open(sock: &TcpStream) -> bool {
    let _ = sock.set_nonblocking(true);
    let mut byte = [0u8; 1];
    let mut r: &TcpStream = sock;
    matches!(r.read(&mut byte), Err(ref e) if e.kind() == io::ErrorKind::WouldBlock)
}
