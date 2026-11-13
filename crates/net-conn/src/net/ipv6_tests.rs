//! `net` の、IPv6 の全体の勝敗 (`Ipv6Policy` のプロセスに 1 つの状態) を動かす試験。
//! 全体の状態を共有するので `IPV6_TEST_LOCK` を握って直列に回す。時計を引数で受ける判断そのものの
//! 試験は `ipv6` に、ソケットを使うそれ以外の試験は `tests` にある。

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use super::ipv6::{LOSS_LIMIT, POLICY, PROBE_SECS, ipv6_first_for_new_host};
use super::*;

/// 起動直後の状態に戻す (テスト同士が互いの連敗を見ないように)。ホストごとの記憶は
/// 消さない (テストごとに違うホスト名を使っている。`dns::clear()` は同じ表を使う
/// `dns` のテストと衝突する)。
fn reset_ipv6_state() {
    POLICY.reset();
    set_stagger(DEFAULT_STAGGER);
}

/// `[::1]` の黒穴 (backlog 0 の待ち受けを 1 本で埋める) と、生きている `127.0.0.1` の 2 候補。
///
/// `listen(fd, 0)` にすると受け入れ待ち行列が 1 本で埋まり、以後の SYN は黙って捨てられる
/// (`tcp_abort_on_overflow = 0` の既定)。次の `connect` は 1.5 秒以上返らないので、
/// デプロイ先の「IPv6 は経路があるのに繋がらない」を手元で再現できる (2026-09-10 に確認)。
#[cfg(target_os = "linux")]
fn blackhole_v6_and_live_v4() -> Option<(TcpListener, TcpStream, TcpListener, Vec<SocketAddr>)> {
    use std::os::fd::AsRawFd;
    // `libc` は使わない (仕様 S25)。`listen(2)` だけ直接宣言する
    unsafe extern "C" {
        fn listen(fd: i32, backlog: i32) -> i32;
    }
    let hole = TcpListener::bind("[::1]:0").ok()?;
    if unsafe { listen(hole.as_raw_fd(), 0) } != 0 {
        return None;
    }
    let hole_addr = hole.local_addr().ok()?;
    // 1 本つないで待ち行列を埋める (accept しない)
    let filler = TcpStream::connect_timeout(&hole_addr, Duration::from_secs(1)).ok()?;
    let live = TcpListener::bind("127.0.0.1:0").ok()?;
    let live_addr = live.local_addr().ok()?;
    Some((hole, filler, live, vec![hole_addr, live_addr]))
}

/// T12.1 の受け入れ基準 (手元): 黒穴 `[::1]` + 生きている `127.0.0.1` で
/// **1 回目 ≥ 250 ms (設計どおり `STAGGER` を待つ)、同じホストの 2 回目は待たない**。
///
/// 2 回目は**時間ではなく試行の順で見る** (T18.1): 覚えた族 (IPv4) が先頭なら IPv6 の
/// 候補は起動されないので、`attempts` が 1 回目の 1 のまま動かない。前は「2 回目 < 50 ms」
/// で見ていて、混んだ機械 (2 コアで全体テスト) ではスレッドを起こすだけで 50 ms を超えて
/// 稀に落ちた。IPv6 が先頭に来ていれば `attempts` は 2 になるので、守っているものは同じ。
#[cfg(target_os = "linux")]
#[test]
fn happy_eyeballs_skips_unreachable_first_candidate() {
    let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_ipv6_state();
    let Some((_hole, _filler, live, addrs)) = blackhole_v6_and_live_v4() else {
        eprintln!("no IPv6 loopback; skipping");
        return;
    };
    let port = live.local_addr().unwrap().port();
    let host = "t121-learn.invalid";

    let started = Instant::now();
    let stream =
        connect_resolved(host, addrs.clone(), Duration::from_secs(5)).expect("v4 should win");
    let first = started.elapsed();
    assert_eq!(stream.peer_addr().unwrap().port(), port);
    assert!(
        first >= DEFAULT_STAGGER && first < Duration::from_secs(3),
        "1 回目は STAGGER を待って IPv4 で確立するはず: {:?}",
        first
    );
    assert_eq!(crate::dns::preferred_family(host), Some(false));
    assert_eq!(ipv6_counters(), [1, 0, 1], "{}", ipv6_status_json());

    // 2 回目の間だけ間隔を上限 (2 秒) まで延ばす: IPv4 が先頭なら、loopback の接続が
    // 2 秒かからない限り IPv6 の候補は起動されない (既定の 250 ms のままだと、
    // 混んだ機械で IPv4 の 1 本が 250 ms を超えたときに追い掛けの 1 本が出て数が動く)
    set_stagger(Duration::from_millis(*STAGGER_RANGE_MS.end()));
    let started = Instant::now();
    let stream = connect_resolved(host, addrs, Duration::from_secs(5)).expect("v4 should win");
    let second = started.elapsed();
    // 後のテストのために既定へ戻す (失敗しても `reset_ipv6_state` が戻す)
    set_stagger(DEFAULT_STAGGER);
    assert_eq!(stream.peer_addr().unwrap().port(), port);
    assert_eq!(
        ipv6_counters(),
        [1, 0, 1],
        "2 回目は覚えた族 (IPv4) を先頭にするので IPv6 の候補を起動しないはず ({:?}): {}",
        second,
        ipv6_status_json()
    );
}

/// `PROXY_HE_STAGGER_MS` (T17.6): 間隔を縮めると、黒穴の IPv6 を先頭にした 1 回目が
/// **縮めた間隔だけ**待って IPv4 で確立する (既定の 250 ms は待たない)。
#[cfg(target_os = "linux")]
#[test]
fn ipv6_blackhole_waits_only_the_configured_stagger() {
    let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_ipv6_state();
    let Some((_hole, _filler, live, addrs)) = blackhole_v6_and_live_v4() else {
        eprintln!("no IPv6 loopback; skipping");
        return;
    };
    let port = live.local_addr().unwrap().port();
    let short = Duration::from_millis(20);
    set_stagger(short);
    assert_eq!(stagger(), short);

    let started = Instant::now();
    let stream = connect_resolved("t176-stagger.invalid", addrs, Duration::from_secs(5))
        .expect("v4 should win");
    let took = started.elapsed();
    // 後のテストのために既定へ戻す (失敗しても `reset_ipv6_state` が戻す)
    set_stagger(DEFAULT_STAGGER);
    assert_eq!(stream.peer_addr().unwrap().port(), port);
    assert!(
        took >= short && took < DEFAULT_STAGGER,
        "縮めた間隔だけ待って IPv4 で確立するはず: {:?}",
        took
    );
}

/// 全体の記憶: 連続 3 回負けたら**初めて見るホストも** IPv4 を先頭にする (T12.1 の 2)。
#[cfg(target_os = "linux")]
#[test]
fn unseen_hosts_try_ipv4_first_after_three_losses() {
    let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_ipv6_state();
    let Some((_hole, _filler, live, addrs)) = blackhole_v6_and_live_v4() else {
        eprintln!("no IPv6 loopback; skipping");
        return;
    };
    let port = live.local_addr().unwrap().port();
    for i in 0..LOSS_LIMIT {
        let host = format!("t121-loser-{}.invalid", i);
        let started = Instant::now();
        connect_resolved(&host, addrs.clone(), Duration::from_secs(5)).expect("v4 should win");
        assert!(started.elapsed() >= DEFAULT_STAGGER, "{} 回目: 待つはず", i);
    }
    assert!(ipv6_v4_first(), "{}", ipv6_status_json());

    // 初めて見るホスト (記憶なし) でも待たない。**時間ではなく試行の順で見る** (T20.2。
    // `happy_eyeballs_skips_unreachable_first_candidate` と同じ形): IPv4 が先頭なら IPv6 の
    // 候補は起動されないので、`attempts` が 3 連敗の 3 のまま動かない。前は「< 50 ms」で
    // 見ていて、混んだ機械ではスレッドを起こすだけで超えうる。IPv6 が先頭に来ていれば
    // `attempts` は 4 になるので、守っているものは同じ。
    // 4 本目の間だけ間隔を上限 (2 秒) まで延ばす (既定の 250 ms のままだと、混んだ機械で
    // IPv4 の 1 本が 250 ms を超えたときに追い掛けの 1 本が出て数が動く)
    set_stagger(Duration::from_millis(*STAGGER_RANGE_MS.end()));
    let started = Instant::now();
    let stream = connect_resolved("t121-fresh.invalid", addrs, Duration::from_secs(5))
        .expect("v4 should win");
    let elapsed = started.elapsed();
    // 後の部分とほかのテストのために既定へ戻す (失敗しても `reset_ipv6_state` が戻す)
    set_stagger(DEFAULT_STAGGER);
    assert_eq!(stream.peer_addr().unwrap().port(), port);
    let [attempts, wins, losses] = ipv6_counters();
    assert_eq!(
        (attempts, wins, losses),
        (LOSS_LIMIT, 0, LOSS_LIMIT),
        "3 回負けたあとは初めて見るホストも IPv4 が先頭で、IPv6 の候補を起動しないはず ({:?}): {}",
        elapsed,
        ipv6_status_json()
    );
    // 4 本目で attempts が増えていないのは、IPv4 が先頭で勝ったから
    // (IPv6 の候補は残っているが起動されない = 負けた試行のスレッドと fd も残らない。T12.2)

    // IPv6 をやめたわけではない: IPv4 が死んでいれば IPv6 で拾い、勝った時点で IPv4 優先は解ける
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);
    let v6 = TcpListener::bind("[::1]:0").unwrap();
    let stream = connect_resolved(
        "t121-v4dead.invalid",
        vec![dead_addr, v6.local_addr().unwrap()],
        Duration::from_secs(5),
    )
    .expect("IPv6 should pick it up");
    assert!(stream.peer_addr().unwrap().is_ipv6());
    assert!(!ipv6_v4_first(), "1 度勝ったら解除: {}", ipv6_status_json());
}

/// IPv4 優先に落ちた状態を作る (3 連敗、探りの時刻はもう来ている)。
fn force_v4_first() {
    POLICY.loss_streak.store(LOSS_LIMIT, Ordering::Relaxed);
    POLICY.probe_at.store(0, Ordering::Relaxed);
    assert!(ipv6_v4_first());
}

/// T17.7: canary が IPv6 を探っている間は、探りの時刻が来ても**利用者の経路では探らない**
/// (IPv6 を先頭に戻さない = `stagger()` を払わない、`attempts` も増えない)。
#[test]
fn ipv6_probe_is_left_to_the_canary_while_it_probes() {
    let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_ipv6_state();
    force_v4_first();
    assert!(
        ipv6_status_json().contains(",\"probe_by\":\"request\","),
        "{}",
        ipv6_status_json()
    );
    // canary の IPv6 側が 1 本試して繋がらなかった (黒穴の網)
    note_canary_ipv6(false);
    assert!(ipv6_v4_first(), "繋がらなければ解かない");
    assert!(
        ipv6_status_json().contains(",\"v4_first\":true,\"probe_by\":\"canary\","),
        "{}",
        ipv6_status_json()
    );
    for _ in 0..3 {
        assert!(
            !ipv6_first_for_new_host(),
            "canary が探っている間は探らない"
        );
    }
    assert_eq!(
        POLICY.probe_at.load(Ordering::Relaxed),
        0,
        "探りの予約も動かさない"
    );

    // 実際に繋いでも IPv6 の候補は起動されない (IPv4 が先頭で即勝つ)
    let Ok(closed_v6) = TcpListener::bind("[::1]:0") else {
        eprintln!("no IPv6 loopback; skipping the connect half");
        return;
    };
    let v6_addr = closed_v6.local_addr().unwrap();
    drop(closed_v6);
    let live = TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = connect_resolved(
        "t177-canary-probes.invalid",
        vec![v6_addr, live.local_addr().unwrap()],
        Duration::from_secs(5),
    )
    .expect("v4 should win");
    assert!(stream.peer_addr().unwrap().is_ipv4());
    assert_eq!(ipv6_counters(), [0, 0, 0], "{}", ipv6_status_json());
}

/// T17.7: canary の IPv6 側が繋がったら `v4_first` が解ける (勝敗の数には足さない)。
#[test]
fn ipv6_canary_success_lifts_v4_first() {
    let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_ipv6_state();
    force_v4_first();
    note_canary_ipv6(true);
    assert!(!ipv6_v4_first(), "{}", ipv6_status_json());
    assert!(ipv6_first_for_new_host(), "解けたら IPv6 が先頭");
    assert_eq!(ipv6_counters(), [0, 0, 0]);
    assert!(
        ipv6_status_json().contains(",\"v4_first\":false,\"probe_by\":\"canary\","),
        "{}",
        ipv6_status_json()
    );
}

/// T17.7: canary が探っていない (`off`・`--lite`・`PROXY_CANARY_IPV6=off`・AAAA の無い宛先)
/// か、最後に試してから 600 秒を過ぎたら、**今までどおり**利用者の経路で 600 秒に 1 回探る。
#[test]
fn ipv6_probe_stays_on_requests_without_the_canary() {
    let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_ipv6_state();
    force_v4_first();
    // canary が 1 度も試していない: 1 回だけ探り、次は 600 秒後
    assert!(
        ipv6_first_for_new_host(),
        "探りの時刻が来ていれば 1 回だけ探る"
    );
    assert!(!ipv6_first_for_new_host(), "2 回目は探らない");
    let now = crate::clock::now_epoch();
    let at = POLICY.probe_at.load(Ordering::Relaxed);
    assert!(
        at >= now + PROBE_SECS - 1,
        "次の探りは 600 秒後: {} / {}",
        at,
        now
    );
    assert!(ipv6_status_json().contains(",\"probe_by\":\"request\","));

    // canary が最後に試したのが 600 秒より前 (止まった): 利用者の経路に戻る
    POLICY.probe_at.store(0, Ordering::Relaxed);
    POLICY.canary_at.store(now - PROBE_SECS, Ordering::Relaxed);
    assert!(ipv6_status_json().contains(",\"probe_by\":\"request\","));
    assert!(
        ipv6_first_for_new_host(),
        "古い canary の印では探りを止めない"
    );
}

/// T18.1: `request_probes` は **`v4_first` の間に利用者の経路で探った回だけ**を数える。
///
/// - `v4_first` でないとき (IPv6 が先頭なのは探りではない) は増えない
/// - canary が見ていないときは 600 秒に 1 回だけ増え、`request_probe_at` に時刻が残る
/// - canary が探っている間は増えない
#[test]
fn ipv6_request_probes_count_only_the_probes_on_the_request_path() {
    let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_ipv6_state();
    let tail = |probes: u64, at: u64| {
        format!(
            ",\"request_probes\":{},\"request_probe_at\":{}}}",
            probes, at
        )
    };
    // 起動直後 (`v4_first` でない): IPv6 が先頭だが探りではない。欄は末尾に 0 で出る
    assert!(ipv6_first_for_new_host());
    assert_eq!(
        ipv6_status_json(),
        "{\"attempts\":0,\"wins\":0,\"losses\":0,\"v4_first\":false,\"probe_by\":\"request\",\"request_probes\":0,\"request_probe_at\":0}",
        "鍵の順は今までのまま、新しい 2 つは末尾"
    );

    // `v4_first` で canary が見ていない: 探りの時刻が来た 1 回だけ数える
    force_v4_first();
    let before = crate::clock::now_epoch();
    assert!(ipv6_first_for_new_host(), "1 回目は探る");
    let at = POLICY.request_probe_at.load(Ordering::Relaxed);
    assert!(
        at >= before && at <= crate::clock::now_epoch(),
        "最後に探った時刻 (epoch 秒): {}",
        at
    );
    assert!(
        ipv6_status_json().ends_with(&tail(1, at)),
        "{}",
        ipv6_status_json()
    );
    for _ in 0..3 {
        assert!(!ipv6_first_for_new_host(), "600 秒が来るまで探らない");
    }
    assert_eq!(
        POLICY.request_probes.load(Ordering::Relaxed),
        1,
        "探らない回は数えない"
    );
    // 600 秒たった (時刻は進められないので予約を今に戻す): もう 1 回だけ
    POLICY.probe_at.store(0, Ordering::Relaxed);
    assert!(ipv6_first_for_new_host());
    assert!(!ipv6_first_for_new_host());
    assert_eq!(
        POLICY.request_probes.load(Ordering::Relaxed),
        2,
        "600 秒に 1 回"
    );

    // canary が IPv6 を試した: 予約の時刻が来ていても利用者の経路では探らず、数も動かない
    POLICY.probe_at.store(0, Ordering::Relaxed);
    note_canary_ipv6(false);
    for _ in 0..3 {
        assert!(!ipv6_first_for_new_host());
    }
    let at = POLICY.request_probe_at.load(Ordering::Relaxed);
    assert_eq!(
        ipv6_status_json(),
        format!(
            "{{\"attempts\":0,\"wins\":0,\"losses\":0,\"v4_first\":true,\"probe_by\":\"canary\"{}",
            tail(2, at)
        )
    );
    // 勝敗の数とは別物 (探りを数えても `attempts` は動かない。あちらは候補を起動した回数)
    assert_eq!(ipv6_counters(), [0, 0, 0]);
}

/// IPv6 が生きている条件では今までどおり IPv6 が勝ち、IPv4 優先には落ちない。
#[cfg(target_os = "linux")]
#[test]
fn ipv6_still_wins_when_it_works() {
    let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_ipv6_state();
    let Ok(v6) = TcpListener::bind("[::1]:0") else {
        eprintln!("no IPv6 loopback; skipping");
        return;
    };
    let v4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let addrs = vec![v6.local_addr().unwrap(), v4.local_addr().unwrap()];
    let host = "t121-v6ok.invalid";
    let stream = connect_resolved(host, addrs.clone(), Duration::from_secs(5)).unwrap();
    assert!(stream.peer_addr().unwrap().is_ipv6(), "IPv6 が勝つはず");
    assert_eq!(crate::dns::preferred_family(host), Some(true));
    let stream = connect_resolved(host, addrs, Duration::from_secs(5)).unwrap();
    assert!(stream.peer_addr().unwrap().is_ipv6(), "2 回目も IPv6");
    assert!(!ipv6_v4_first());
    assert_eq!(ipv6_counters(), [2, 2, 0]);
}

/// T12.2 の計測: 負けた Happy Eyeballs の試行がスレッドと fd を何本残すか。
///
/// 黒穴 `[::1]` (backlog 0 を 1 本で埋めたもの) + 生きた `127.0.0.1` の 2 候補へ
/// `connect_resolved` を 100 回呼び、1 回ごとに `/proc/self/status` の `Threads` と
/// `/proc/self/fd` の数を読む。締め切りは 2 秒 (デプロイ先の `PROXY_TIMEOUT_SECS` は 30)。
///
///   cargo test -p proxy-net --release -- --ignored --exact \
///     net::ipv6_tests::measures_leftover_attempts --nocapture --test-threads=1
///
/// 2026-09-10 の実測 (T12.2 はこの数字で「やらない」と決めた):
///
/// - 順に 100 本・毎回別ホスト: 残るのは最大 4 / 最後 3 スレッド (fd も同数)、基準値に戻るまで 1.7 秒
/// - 順に 100 本・同じホスト: +0 (記憶が効くと IPv6 の候補はそもそも起動されない)
/// - 600 秒ごとの探り 1 本: +1 スレッド。**残る時間は締め切りそのもの** (2 秒 → 1.8 秒、4 秒 → 3.8 秒)
/// - 起動直後に 100 本が**同時に**来ると +14〜100 (最初の `STAGGER` 250 ms の間に来たぶんだけ賭ける)。
///   学習後に 100 本同時なら +0
#[cfg(target_os = "linux")]
#[test]
#[ignore = "T12.2 の計測 (十数秒かかる)"]
fn measures_leftover_attempts() {
    let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some((_hole, _filler, live, addrs)) = blackhole_v6_and_live_v4() else {
        eprintln!("no IPv6 loopback; skipping");
        return;
    };
    live.set_nonblocking(true).unwrap();
    let timeout = Duration::from_secs(2);

    fn threads() -> usize {
        std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("Threads:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap()
    }
    fn fds() -> usize {
        std::fs::read_dir("/proc/self/fd").unwrap().count()
    }
    let drain = || while live.accept().is_ok() {};
    // 基準値に戻るまでの時間 (0.1 秒刻みで 5 秒まで)
    let settle = |base_t: usize, base_f: usize| -> Option<Duration> {
        let started = Instant::now();
        for _ in 0..51 {
            if threads() <= base_t && fds() <= base_f {
                return Some(started.elapsed());
            }
            thread::sleep(Duration::from_millis(100));
        }
        None
    };
    let show = |d: Option<Duration>| match d {
        Some(d) => format!("{:.1} s", d.as_secs_f64()),
        None => "> 5 s".to_string(),
    };

    eprintln!("\n== T12.2 計測 (timeout {:?}) ==", timeout);
    eprintln!("条件 | 直後 Threads | 直後 fd | 戻るまで");

    // (1) 起動直後から順に 100 本、ホストは全部初めて見る名前 (T12.1 の学習が効く形)
    reset_ipv6_state();
    crate::dns::clear();
    drain();
    let (base_t, base_f) = (threads(), fds());
    let (mut peak_t, mut peak_f) = (0usize, 0usize);
    let mut per_call = Vec::new();
    let started = Instant::now();
    for i in 0..100 {
        let host = format!("t122-serial-{}.invalid", i);
        let at = Instant::now();
        let s = connect_resolved(&host, addrs.clone(), timeout).expect("v4 should win");
        let took = at.elapsed();
        let (t, f) = (
            threads().saturating_sub(base_t),
            fds().saturating_sub(base_f),
        );
        if i < 6 {
            per_call.push(format!(
                "{}本目 {:.1}ms T+{} fd+{}",
                i + 1,
                took.as_secs_f64() * 1e3,
                t,
                f
            ));
        }
        peak_t = peak_t.max(t);
        peak_f = peak_f.max(f);
        drop(s);
        drain();
    }
    let burst = started.elapsed();
    let (end_t, end_f) = (
        threads().saturating_sub(base_t),
        fds().saturating_sub(base_f),
    );
    let back = settle(base_t, base_f);
    eprintln!(
        "(1) 順に 100 本・毎回別ホスト ({:.2} s) | 最大 +{} / 最後 +{} | 最大 +{} / 最後 +{} | {}",
        burst.as_secs_f64(),
        peak_t,
        end_t,
        peak_f,
        end_f,
        show(back)
    );
    eprintln!("    最初の 6 本: {}", per_call.join(" / "));
    eprintln!("    {}", ipv6_status_json());

    // (2) 同じホストへ 100 本 (ホストごとの記憶が効いた後)
    drain();
    let (base_t, base_f) = (threads(), fds());
    let (mut peak_t, mut peak_f) = (0usize, 0usize);
    let host = "t122-samehost.invalid";
    connect_resolved(host, addrs.clone(), timeout).unwrap();
    drain();
    for _ in 0..100 {
        let s = connect_resolved(host, addrs.clone(), timeout).expect("v4 should win");
        peak_t = peak_t.max(threads().saturating_sub(base_t));
        peak_f = peak_f.max(fds().saturating_sub(base_f));
        drop(s);
        drain();
    }
    let (end_t, end_f) = (
        threads().saturating_sub(base_t),
        fds().saturating_sub(base_f),
    );
    let back = settle(base_t, base_f);
    eprintln!(
        "(2) 順に 100 本・同じホスト | 最大 +{} / 最後 +{} | 最大 +{} / 最後 +{} | {}",
        peak_t,
        end_t,
        peak_f,
        end_f,
        show(back)
    );

    // (3) 600 秒ごとの IPv6 の探り 1 本 (時刻は進められないので `probe_at` を 0 に戻す)。
    // 締め切りを 2 秒と 4 秒で 2 回測り、「残る時間 = PROXY_TIMEOUT_SECS」を確かめる
    for t in [timeout, Duration::from_secs(4)] {
        drain();
        let (base_t, base_f) = (threads(), fds());
        POLICY.probe_at.store(0, Ordering::Relaxed);
        let at = Instant::now();
        let s = connect_resolved("t122-probe.invalid", addrs.clone(), t).unwrap();
        let took = at.elapsed();
        let (end_t, end_f) = (
            threads().saturating_sub(base_t),
            fds().saturating_sub(base_f),
        );
        drop(s);
        drain();
        let back = settle(base_t, base_f);
        eprintln!(
            "(3) 600 秒ごとの探り 1 本・締め切り {:?} ({:.1} ms) | +{} | +{} | {}",
            t,
            took.as_secs_f64() * 1e3,
            end_t,
            end_f,
            show(back)
        );
        crate::dns::clear();
    }

    // (4) 起動直後に 100 本が**同時に**来る (ページを 1 枚開いた形。学習が間に合わない)
    reset_ipv6_state();
    crate::dns::clear();
    drain();
    let (base_t, base_f) = (threads(), fds());
    let started = Instant::now();
    thread::scope(|scope| {
        let mut hs = Vec::new();
        for i in 0..100 {
            let addrs = addrs.clone();
            hs.push(scope.spawn(move || {
                let host = format!("t122-para-{}.invalid", i);
                connect_resolved(&host, addrs, timeout).map(drop)
            }));
        }
        // 呼び出し側のスレッドは全部終わらせてから数える (残るのは負けた試行だけ)
        for h in hs {
            let _ = h.join();
        }
    });
    let burst = started.elapsed();
    let (end_t, end_f) = (
        threads().saturating_sub(base_t),
        fds().saturating_sub(base_f),
    );
    drain();
    let back = settle(base_t, base_f);
    eprintln!(
        "(4) 同時に 100 本・起動直後 ({:.2} s) | +{} | +{} | {}",
        burst.as_secs_f64(),
        end_t,
        end_f,
        show(back)
    );
    eprintln!("    {}", ipv6_status_json());

    // (5) 学習が済んだあとに 100 本が同時に来る (2 枚目以降のページ)
    drain();
    let (base_t, base_f) = (threads(), fds());
    let started = Instant::now();
    thread::scope(|scope| {
        let mut hs = Vec::new();
        for i in 0..100 {
            let addrs = addrs.clone();
            hs.push(scope.spawn(move || {
                let host = format!("t122-para2-{}.invalid", i);
                connect_resolved(&host, addrs, timeout).map(drop)
            }));
        }
        for h in hs {
            let _ = h.join();
        }
    });
    let burst = started.elapsed();
    let (end_t, end_f) = (
        threads().saturating_sub(base_t),
        fds().saturating_sub(base_f),
    );
    drain();
    let back = settle(base_t, base_f);
    eprintln!(
        "(5) 同時に 100 本・学習後 ({:.2} s) | +{} | +{} | {}",
        burst.as_secs_f64(),
        end_t,
        end_f,
        show(back)
    );
    eprintln!("    {}\n", ipv6_status_json());
}

/// Linux 以外は黒穴 (`listen(fd, 0)`) を作れないので、到達不能アドレスで従来どおり見る。
#[cfg(not(target_os = "linux"))]
#[test]
fn happy_eyeballs_skips_unreachable_first_candidate() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // 到達不能 (TEST-NET-1) → ループバックの順で試させる
    let addrs = vec![
        SocketAddr::new("192.0.2.1".parse().unwrap(), 9),
        SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port),
    ];
    let started = Instant::now();
    let stream = connect_resolved("t121-unreachable.invalid", addrs, Duration::from_secs(5))
        .expect("loopback should win");
    assert_eq!(stream.peer_addr().unwrap().port(), port);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "did not wait for the dead address"
    );
}
