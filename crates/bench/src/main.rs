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
//!   3. tunnel  : CONNECT トンネル `--conc N` 本のスループット合計 (MiB/s、`--seconds` 秒)。
//!      **この経路はベンチ側が律速する** (プロキシは `splice` でコピー 0 回、ベンチは送りと受けで
//!      コピー 2 回)。`scripts/cpu-per-request.sh --only tunnel` は両方を big コアに置いて測る (T10.8)。
//!      `docs/status.md` の「トンネル 1 本」は `--conc 1` の値
//!   4. connect : CONNECT の確立/秒 (短命トンネル)
//!   5. idle-tunnels: `--conc N` 本の CONNECT を張ったまま `--seconds` 秒握る
//!      (プロキシ側のスレッド数と RSS を見るためのモード。`--only idle-tunnels` でだけ走る)
//!   6. idle-conns: `--conc N` 本の keep-alive 接続に **1 要求ずつ通してから** `--seconds` 秒握る
//!      (同じくプロキシ側のスレッド数と RSS を見るためのモード。`--only idle-conns` でだけ走る)
//!   7. syscall-cost: この機械での `sendto` / `recvfrom` 1 回の実費 (プロキシは使わない。
//!      `--only syscall-cost` でだけ走る。**`taskset` で cpu を固定して使うこと**)
//!   8. connect-multi: 4 と同じだが宛先が**名前** (`multi.test`) なので、プロキシ側で
//!      候補が 2 つ (黒穴の AAAA と生きている A) になり Happy Eyeballs 本体を通る。
//!      `scripts/deployed-like.sh` の中でだけ走る (`--only connect-multi`)
//!   9. replay: デプロイ先の `/recent` (T14.4) の個票を**同じ間隔・同じ本数**で張り直す
//!      (`--only replay --replay-file <path> [--speed 10]`。T14.29)。宛先のホスト名は
//!      そのままに、ポートだけ内蔵オリジンへ置き換える。名前解決は
//!      `scripts/deployed-like.sh --hosts-from <path>` の `/etc/hosts` に任せる
//!
//! 中の割り方 (T34.4): 引数は [`args`]、相手役は [`origin`]、集計と `conc` 本で回す骨組みは
//! [`report`]、HTTP の打ち手は [`http`]、握り続けるモードは [`idle`]、名前宛ての CONNECT は
//! [`multi`]、システムコールの実費は [`syscost`]、個票の再生は [`replay`]。ここに残るのは
//! モードの振り分け ([`main`]) と、トンネルと CONNECT の 2 モード。

mod args;
mod http;
mod idle;
mod multi;
mod origin;
mod replay;
mod report;
mod syscost;

use std::net::{Shutdown, SocketAddr};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use args::Args;
use http::{drain_tunnel, http_load, open_tunnel};
use origin::{spawn_blaster, spawn_origin, spawn_sink};
use report::run_load;

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

/// デプロイ先の個票の再生 (相手は自前の再生オリジンなので、固定応答オリジンは使わない)。
fn replay_mode(args: &Args) {
    let (Some(proxy), Some(file)) = (args.proxy.as_deref(), args.replay_file.as_deref()) else {
        eprintln!("bench: --only replay needs --proxy and --replay-file");
        args::usage();
    };
    let proxy = parse_addr(proxy);
    replay::run(&replay::ReplayArgs {
        proxy,
        file,
        rtt_file: args.rtt_file.as_deref(),
        speed: args.speed,
        limit_secs: args.replay_secs,
        admin: args.admin.as_deref().map_or(proxy, parse_addr),
    });
}

/// トンネル `conc` 本のスループット合計 (他のモードと同じく `seconds` 秒で終わる)。
fn tunnel_mode(proxy: SocketAddr, conc: usize, seconds: u64) {
    let blaster = spawn_blaster().expect("blaster");
    let blaster = blaster.to_string();
    // 2 本目以降を先に張ってスレッドへ渡し、**1 本目はこのスレッドで回す**。
    // こうすると `--conc 1` では下の `for` が 1 度も回らず、スレッドを 1 本も
    // 足さない元のままの経路になる (`docs/status.md` の「トンネル 1 本」はこの条件の値なので、
    // 並列を足したことで数字が動かないようにする)。
    let mut readers = Vec::new();
    let mut failed = 0usize;
    for _ in 1..conc {
        match open_tunnel(proxy, &blaster) {
            Ok((sock, reader)) => {
                readers.push(thread::spawn(move || drain_tunnel(sock, reader, seconds)));
            }
            Err(_) => failed += 1,
        }
    }
    match open_tunnel(proxy, &blaster) {
        Ok((sock, reader)) => {
            let t0 = Instant::now();
            let mut got = drain_tunnel(sock, reader, seconds);
            let secs = t0.elapsed().as_secs_f64().max(1e-9);
            for h in readers {
                got += h.join().unwrap_or(0);
            }
            let live = conc - failed;
            println!(
                "{:<8} {:>9} op/s  {:>8.1} MiB/s  ({} MiB through {} tunnel{})",
                "tunnel",
                live,
                got as f64 / secs / (1024.0 * 1024.0),
                got / (1 << 20),
                live,
                if live == 1 { "" } else { "s" }
            );
            if failed > 0 {
                println!("tunnel   {} of {} tunnels failed to open", failed, conc);
            }
        }
        Err(e) => {
            // 1 本目が張れなくても、先に張った 2 本目以降は流れている。
            // 待たずに次のモードへ進むとその通信が混ざるので、ここで待つ
            for h in readers {
                let _ = h.join();
            }
            println!("tunnel   skipped: {}", e);
        }
    }
}

/// CONNECT の確立/秒 (短命トンネル)。
fn connect_mode(proxy: SocketAddr, conc: usize, seconds: u64) {
    let sink = spawn_sink().expect("sink").to_string();
    run_load(conc, seconds, move |stop, lat, _bytes| {
        while !stop.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            match open_tunnel(proxy, &sink) {
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

fn main() {
    let args = args::parse_args();

    // システムコールの実費だけを測るモード (プロキシもオリジンも使わない)
    if args.only == "syscall-cost" {
        syscost::run(args.seconds);
        return;
    }

    // デプロイ先の個票の再生 (相手は自前の再生オリジンなので、下の固定応答オリジンは使わない)
    if args.only == "replay" {
        replay_mode(&args);
        return;
    }

    let origin = spawn_origin(args.body_bytes, args.cacheable).expect("origin");
    println!(
        "bench: conc={} seconds={} body={}B cacheable={} keep-alive={} origin={}",
        args.conc, args.seconds, args.body_bytes, args.cacheable, !args.no_keepalive, origin
    );

    if wants(&args, "direct") {
        direct_mode(&args, origin);
    }

    let Some(proxy) = args.proxy.as_deref() else {
        return;
    };
    proxy_modes(&args, parse_addr(proxy), origin);
}

/// `--only` がこのモードを選んでいるか (`all` は全部。握り続けるモードなどは別に見る)。
fn wants(args: &Args, name: &str) -> bool {
    args.only == "all" || args.only == name
}

/// オリジン直結の要求/秒 (ベンチが律速していないことの確認)。
fn direct_mode(args: &Args, origin: SocketAddr) {
    let direct_req = format!("GET / HTTP/1.1\r\nHost: {}\r\n\r\n", origin).into_bytes();
    http_load(
        origin,
        direct_req,
        args.conc,
        args.seconds,
        !args.no_keepalive,
    )
    .print("direct");
}

/// 平文 HTTP をプロキシ経由で転送したときの要求/秒と p50/p99。
fn forward_mode(args: &Args, proxy: SocketAddr, origin: SocketAddr) {
    let forward_req = format!("GET http://{0}/ HTTP/1.1\r\nHost: {0}\r\n\r\n", origin).into_bytes();
    http_load(
        proxy,
        forward_req,
        args.conc,
        args.seconds,
        !args.no_keepalive,
    )
    .print("forward");
}

/// プロキシを通すモードを、印字の順に回す (T34.102 で `main` から出した)。
fn proxy_modes(args: &Args, proxy: SocketAddr, origin: SocketAddr) {
    if wants(args, "forward") {
        forward_mode(args, proxy, origin);
    }

    // トンネル `--conc N` 本のスループット合計 (他のモードと同じく `--seconds` 秒で終わる)
    if wants(args, "tunnel") {
        tunnel_mode(proxy, args.conc, args.seconds);
    }

    // アイドルなトンネルを握り続ける (--only idle-tunnels のときだけ。all には入れない)
    if args.only == "idle-tunnels" {
        idle::idle_tunnels(proxy, args.conc, args.seconds);
        return;
    }

    // 1 要求ずつ通した keep-alive 接続を握り続ける (--only idle-conns のときだけ。all には入れない)
    if args.only == "idle-conns" {
        idle::idle_conns(proxy, origin, args.conc, args.seconds);
        return;
    }

    // 名前宛ての CONNECT (--only connect-multi のときだけ。名前空間が要るので all には入れない)
    if args.only == "connect-multi" {
        multi::connect_multi(proxy, args.conc, args.seconds);
        return;
    }

    // CONNECT の確立/秒
    if !wants(args, "connect") {
        return;
    }
    connect_mode(proxy, args.conc, args.seconds);
}
