//! デプロイ先のパターンを手元で再生する (`--only replay --replay-file <path>`。T14.29)。
//!
//! バーストのとき T13.2 (追い出し) と T14.6 (写真) が本当に効くかは、**バーストが来るまで
//! 分からない**。`/recent` (T14.4) には「いつ・誰が・どこへ・どれだけ」があるので、
//! **同じ時間間隔・同じ本数**で手元の内蔵オリジンへ張り直せば、バーストの形だけ再現できる。
//!
//! 読める形は 3 つ (どれも `{"…":[…]}` の中身を探すだけ):
//!
//!   1. `/recent` の応答そのもの (`{"recent":[…]}`)
//!   2. `/snapshot` (`{"recent":{"recent":[…]},…}`。`hosts` があれば RTT もそこから読む)
//!   3. `/connections` (`{"connections":[…]}`)。**まだ閉じていない接続**なので寿命が無く、
//!      `age_secs` を「開いてからの秒」と「これから生きる秒」の両方に使う。`bytes` は
//!      上り下りの合計しか無いので全部下りに寄せる。**`/recent` が無いデプロイ先 (Phase 13)
//!      でも実データを再生できる**ようにするための読み口
//!
//! 宛先は `host:port` の**ホスト名だけ**を CONNECT の target に載せ、ポートは内蔵オリジンの
//! 1 つのポートに置き換える。名前解決は `scripts/deployed-like.sh --hosts-from <path>` が
//! 名前空間の中の `/etc/hosts` を差し替えて全部 `127.0.0.1` に向ける (T14.16 の仕組み)。
//!
//! RTT は `/hosts` の `rtt_ms.avg` (T14.5) があれば**内蔵オリジンの応答遅延**で真似る
//! (無ければ 0)。デプロイ先が Phase 13 のうちは `rtt_ms` 自体が無いので 0 になる。
//!
//! **外部クレートは使わない** (TODO.md §0) ので、JSON も [`json`] の小さな読み取りで済ませる。
//!
//! 中の割り方 (T34.4): JSON の読みは [`json`]、個票への変換は [`shots`] (どちらも純粋)、
//! 合図の形と内蔵オリジンは [`origin`]、1 本ぶんの再生は [`tunnel`]。ここは段取り
//! (読む → RTT を重ねる → 相手を立てる → 間隔どおりに張る → 集計を印字する) だけ。

mod json;
mod origin;
mod shots;
mod tunnel;

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use crate::report::Report;
use json::{Json, parse};
use origin::spawn_replay_origin;
use shots::{ReadStats, Shot, offsets_ms, read_rtts, read_shots};
use tunnel::{Tally, run_one};

/// `--only replay` の引数。
pub(crate) struct ReplayArgs<'a> {
    pub(crate) proxy: SocketAddr,
    /// 個票のファイル (`--replay-file`)
    pub(crate) file: &'a str,
    /// RTT を読むファイル (`--rtt-file`。個票と同じ JSON に `hosts` があればそちらも使う)
    pub(crate) rtt_file: Option<&'a str>,
    /// 何倍速か (`--speed`)
    pub(crate) speed: f64,
    /// 再生を打ち切る秒 (`--replay-secs`。0 なら最後まで)
    pub(crate) limit_secs: u64,
    /// `/status` と `/bursts` を引く先 (既定は `--proxy` と同じ)
    pub(crate) admin: SocketAddr,
}

pub(crate) fn run(a: &ReplayArgs<'_>) {
    let (text, mut shots, st) = load_shots(a.file);
    let with_rtt = apply_rtts(&text, a.rtt_file, &mut shots);

    let origin = match spawn_replay_origin() {
        Ok(o) => o,
        Err(e) => {
            println!("replay  cannot start the replay origin: {}", e);
            std::process::exit(2);
        }
    };
    check_resolvable(&shots[0].host, origin.port(), a.file);

    let ats: Vec<u64> = shots.iter().map(|s| s.at).collect();
    let offsets = offsets_ms(&ats, a.speed);
    print_plan(a, &st, &shots, &ats, &offsets, origin, with_rtt);

    let (tally, elapsed, skipped) = replay_all(a, origin.port(), shots, offsets);
    print_result(&tally, elapsed, skipped);
    print_proxy_side(a.admin);
}

/// 個票のファイルを読む。読めない・再生できる CONNECT が 1 本も無いときは印字して 2 で終わる。
fn load_shots(file: &str) -> (String, Vec<Shot>, ReadStats) {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            println!("replay  cannot read {}: {}", file, e);
            std::process::exit(2);
        }
    };
    let (shots, st) = match read_shots(&text) {
        Ok(v) => v,
        Err(e) => {
            println!("replay  {} を読めません: {}", file, e);
            std::process::exit(2);
        }
    };
    if shots.is_empty() {
        println!(
            "replay  {} に再生できる CONNECT の個票がありません ({} 行、http {} 件)",
            file, st.rows, st.http
        );
        std::process::exit(2);
    }
    (text, shots, st)
}

/// RTT は同じ JSON の `hosts` を先に見て、`--rtt-file` があれば重ねる。RTT を持てた個票の数を返す。
fn apply_rtts(text: &str, rtt_file: Option<&str>, shots: &mut [Shot]) -> usize {
    let mut rtts = read_rtts(text);
    if let Some(path) = rtt_file {
        match std::fs::read_to_string(path) {
            Ok(t) => rtts.extend(read_rtts(&t)),
            Err(e) => println!("replay  cannot read {}: {}", path, e),
        }
    }
    let mut with_rtt = 0usize;
    for s in shots.iter_mut() {
        if let Some(&ms) = rtts.get(&s.host) {
            s.rtt_ms = ms;
            with_rtt += 1;
        }
    }
    with_rtt
}

/// 名前が引けるか 1 度だけ確かめる (引けないとプロキシが全部 502 を返して何も測れない)。
fn check_resolvable(first: &str, origin_port: u16, file: &str) {
    if (first, origin_port).to_socket_addrs().is_err() {
        println!(
            "replay  cannot resolve {}; run it inside \
             scripts/deployed-like.sh --hosts-from {} -- …",
            first, file
        );
        std::process::exit(2);
    }
}

/// 再生の前に、読んだ個票の内訳と条件を 2 行で印字する。
fn print_plan(
    a: &ReplayArgs<'_>,
    st: &ReadStats,
    shots: &[Shot],
    ats: &[u64],
    offsets: &[u64],
    origin: SocketAddr,
    with_rtt: usize,
) {
    let span = ats.last().copied().unwrap_or(0) - ats[0];
    let hosts: BTreeSet<&str> = shots.iter().map(|s| s.host.as_str()).collect();
    println!(
        "replay   {} ({}): {} 個票 (connect {} / http {} / 落とした {})、宛先 {} 種、\
         span {}s -> {:.1}s (speed {}x)",
        a.file,
        st.source,
        st.rows,
        st.connect,
        st.http,
        st.dropped,
        hosts.len(),
        span,
        offsets.last().copied().unwrap_or(0) as f64 / 1000.0,
        a.speed,
    );
    println!(
        "replay   origin {} | RTT を真似る個票 {} / {} | 打ち切り {}",
        origin,
        with_rtt,
        shots.len(),
        if a.limit_secs == 0 {
            "なし".to_string()
        } else {
            format!("{}s", a.limit_secs)
        }
    );
}

/// 個票を間隔どおりに 1 本 1 スレッドで張り、全部が閉じるのを待つ。
/// 戻り値は (集計, かかった時間, 打ち切りで張らなかった本数)。
fn replay_all(
    a: &ReplayArgs<'_>,
    origin_port: u16,
    shots: Vec<Shot>,
    offsets: Vec<u64>,
) -> (Arc<Tally>, Duration, usize) {
    let tally = Arc::new(Tally::default());
    let started = Instant::now();
    let deadline = (a.limit_secs > 0).then(|| started + Duration::from_secs(a.limit_secs));
    let mut handles = Vec::with_capacity(shots.len());
    let mut skipped = 0usize;
    for (shot, off) in shots.into_iter().zip(offsets) {
        let due = Duration::from_millis(off);
        let now = started.elapsed();
        if due > now {
            thread::sleep(due - now);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            skipped += 1;
            continue;
        }
        let proxy = a.proxy;
        let speed = a.speed;
        let tally = Arc::clone(&tally);
        handles.push(thread::spawn(move || {
            run_one(proxy, origin_port, &shot, speed, deadline, &tally);
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    (tally, started.elapsed(), skipped)
}

/// 張れた本数・切られた本数・流したバイトと、確立時間の分位点を印字する。
fn print_result(tally: &Tally, elapsed: Duration, skipped: usize) {
    let mut connect_us = tally.connect_us.lock().expect("connect_us").clone();
    connect_us.sort_unstable();
    let failed = tally.failed.load(Ordering::Relaxed);
    let cut = tally.cut.load(Ordering::Relaxed);
    println!(
        "replay   {} tunnels in {:.1}s (failed {}, cut by the proxy {}, not started {}, \
         up {} B, down {} B)",
        connect_us.len(),
        elapsed.as_secs_f64(),
        failed,
        cut,
        skipped,
        tally.up.load(Ordering::Relaxed),
        tally.down.load(Ordering::Relaxed),
    );
    Report {
        ops: connect_us.len() as u64,
        bytes: tally.up.load(Ordering::Relaxed) + tally.down.load(Ordering::Relaxed),
        elapsed,
        latencies_us: connect_us,
    }
    .print_multi("replay");
}

// ------------------------------------------------------------ プロキシ側の数字

/// `/status` の `evicted_idle` / `rejected_overload` と `/bursts` の枚数を 1 行で印字する。
fn print_proxy_side(admin: SocketAddr) {
    let status = fetch(admin, "/status").and_then(|b| parse(&b).ok());
    let bursts = fetch(admin, "/bursts").and_then(|b| parse(&b).ok());
    let num = |v: &Option<Json>, key: &str| -> String {
        v.as_ref()
            .and_then(|d| d.get(key))
            .and_then(Json::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string())
    };
    println!(
        "replay   /status evicted_idle {} rejected_overload {} active {}/{} | \
         /bursts recorded {} kept {}",
        num(&status, "evicted_idle"),
        num(&status, "rejected_overload"),
        num(&status, "active_connections"),
        num(&status, "max_conns"),
        num(&bursts, "recorded"),
        num(&bursts, "kept"),
    );
}

/// プロキシ自身のエンドポイントを 1 回引く (T12.3 の `local_path`。自分宛ては素通しされない)。
fn fetch(admin: SocketAddr, path: &str) -> Option<String> {
    let mut sock = TcpStream::connect_timeout(&admin, Duration::from_secs(2)).ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, admin
    );
    sock.write_all(req.as_bytes()).ok()?;
    let mut all = String::new();
    sock.read_to_string(&mut all).ok()?;
    let body = all.split_once("\r\n\r\n").map(|(_, b)| b)?;
    Some(body.to_string())
}
