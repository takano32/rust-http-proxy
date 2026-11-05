//! 名前宛ての CONNECT (`--only connect-multi`)。Happy Eyeballs の本体を通す唯一のモード。

use std::net::{Shutdown, SocketAddr, ToSocketAddrs};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::http::open_tunnel;
use crate::origin::spawn_sink;
use crate::report::run_load;

/// `--only connect-multi` の宛先の名前。
///
/// `scripts/deployed-like.sh` が名前空間の中の `/etc/hosts` に
/// **AAAA (黒穴) と A (生きているループバック) の 2 行**で置く。
/// この名前はどの実在のドメインとも衝突しない (`.test` は RFC 6761 の予約)。
const MULTI_HOST: &str = "multi.test";

/// 名前宛ての CONNECT を `--seconds` 秒張り続ける (`--only connect-multi`)。
///
/// `--only connect` との違いは**宛先だけ**で、IP リテラルではなく名前を書く。プロキシ側では候補が 2 つ (黒穴の AAAA と生きている A) になるので
/// **Happy Eyeballs の本体** (`crates/net-conn/src/net.rs` の `connect_candidates`) を通る。
/// `--only connect` は候補が 1 つ (`addrs.len() == 1`) の短絡に入るため、この経路は
/// Phase 12 まで一度も手元で測れていなかった (T10.1 が「無罪」と結論した理由。T14.16)。
///
/// 出す数字は **1 本目の確立時間**と、そのあとの p50 / p95 / max。
/// T12.1 より前のプロキシは毎回 `STAGGER` の 250 ms を払い、今のプロキシは
/// 1 本目だけ払って以後は払わない (= p50 が 1 ms 未満)。
///
/// **`--conc` は 3 以上にすること。** T12.1 の「全体で 3 回続けて負けたら IPv4 を先頭」は
/// 1 本目の負けを数えるが、2 本目からはホストごとの記憶 (`last_win_v6`) が効いて
/// そもそも IPv6 を試さない。同時に走り出した `--conc` 本ぶんだけが「まだ誰も負けていない」
/// 状態で賭けるので、`/status` の `v4_first` が立つのは `--conc >= 3` のときだけ。
pub(crate) fn connect_multi(proxy: SocketAddr, conc: usize, seconds: u64) {
    let sink = spawn_sink().expect("sink");
    let target = format!("{}:{}", MULTI_HOST, sink.port());
    // 名前が引けない機械 (= `scripts/deployed-like.sh` の外) では測らずに終わる。
    // 終了コード 2 は「この機械では測れない」の意味で、スクリプトが見分けられる。
    let addrs: Vec<SocketAddr> = match (MULTI_HOST, sink.port()).to_socket_addrs() {
        Ok(it) => it.collect(),
        Err(e) => {
            println!(
                "connect-multi: cannot resolve {} ({}); run it inside scripts/deployed-like.sh",
                MULTI_HOST, e
            );
            std::process::exit(2);
        }
    };
    let v6 = addrs.iter().find(|a| a.is_ipv6());
    let v4 = addrs.iter().find(|a| a.is_ipv4());
    let (Some(v6), Some(v4)) = (v6, v4) else {
        println!(
            "connect-multi: {} needs one AAAA and one A, got {:?}; \
             run it inside scripts/deployed-like.sh",
            MULTI_HOST, addrs
        );
        std::process::exit(2);
    };
    println!(
        "cnct-mlt target {} -> {} (v6, blackholed) + {} (v4, alive)",
        target,
        v6.ip(),
        v4.ip()
    );
    // 各スレッドの **1 本目**だけ別に集める (まだ誰も負けていない状態で張る 1 本)。
    // 触るのはスレッドあたり 1 回だけなので、熱い経路に錠は入らない。
    let firsts = Arc::new(Mutex::new(Vec::<u32>::new()));
    let collector = Arc::clone(&firsts);
    let report = run_load(conc, seconds, move |stop, lat, _bytes| {
        let mut first = true;
        while !stop.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            match open_tunnel(proxy, &target) {
                Ok((sock, _)) => {
                    let us = t0.elapsed().as_micros().min(u32::MAX as u128) as u32;
                    if first {
                        first = false;
                        collector.lock().expect("firsts").push(us);
                    }
                    lat.push(us);
                    let _ = sock.shutdown(Shutdown::Both);
                }
                Err(_) => thread::sleep(Duration::from_millis(2)),
            }
        }
    });
    let mut first = firsts.lock().expect("firsts").clone();
    first.sort_unstable();
    if first.is_empty() {
        println!("cnct-mlt no CONNECT succeeded (is the proxy allowed to reach 127.0.0.1?)");
        std::process::exit(2);
    }
    println!(
        "cnct-mlt first {:>8.3} ms  (median of {} threads' first CONNECT; min {:.3} max {:.3})",
        first[first.len() / 2] as f64 / 1000.0,
        first.len(),
        first[0] as f64 / 1000.0,
        first[first.len() - 1] as f64 / 1000.0,
    );
    report.print_multi("cnct-mlt");
}
