//! 預かり所の監視スレッド (`idle-watch`) は、預かりが 0 の間は眠ったまま起きない (T29.23)。
//!
//! **このファイルのテストは 1 本だけ**にしてある: 同じテストバイナリで別のテストが
//! 自分のプロキシ (と自分の `idle-watch`) を立てると、どのスレッドが起きたのか分からなくなる。

#![cfg(target_os = "linux")]

use std::io::Read;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::thread;
use std::time::{Duration, Instant};

mod common;
use common::*;

/// `idle-watch` が `epoll_wait` で眠っているときの様子 (T34.161)。
///
/// 「起きたか」は**スレッドが `epoll_wait` から戻ったか**で見る。戻る道は 3 つしかない:
/// 期限が来る (待ちの引数 `timeout` が -1 でない)・呼び鈴 (eventfd) が鳴る (戻ったら
/// `drain` で必ず `read` するので、スレッドの `syscr` が増える)・預かった接続に事象が来る
/// (預かりが 0 なら無い)。なので「預かりが 0 の間は `timeout` が -1 で、`syscr` が増えない」は
/// 「起きない」と同じことを言っている。
///
/// 以前は `voluntary_ctxt_switches` (自分から眠った回数) の差で見ていたが、これは眠りに
/// **入るとき**に増えるので、眠りに入る前 (立ち上げの直後や、預かりを引き取って片付けている
/// 最中) に数を取ると、そのあと眠るだけで 1 増える。カーネルの中だけで起きて眠り直す
/// (プロキシからは見えない) ときも増える。`cargo test --workspace` の負荷の下で 1 回
/// 「預かりが 0 なのに起きた」と落ちた。ここでは眠っているのを見てから読むので、
/// 待つ長さ (負荷) に左右されない。
struct Asleep {
    /// 眠っている `epoll_wait` の `timeout` (ミリ秒。-1 = 期限なし)
    timeout_ms: i32,
    /// スレッドが起動から呼んだ `read` 系の数 (`/proc/self/task/<tid>/io` の `syscr`)
    reads: u64,
}

/// このプロセスの `idle-watch` が `epoll_wait` で眠っているのを見るまで待って、様子を返す。
///
/// スレッドの名前は**生まれたスレッドが自分で付ける**ので、立ち上げの直後は見えないことがある
/// (見えて眠るまで待つ)。
fn idle_watch_asleep() -> Asleep {
    wait_for(
        "idle-watch はこのプロセスに 1 本で、epoll_wait で眠っているはず",
        Duration::from_millis(20),
        Limit::Tries(500),
        || match &idle_watch_threads()[..] {
            [dir] => asleep_in_epoll(dir),
            _ => None,
        },
    )
}

fn idle_watch_threads() -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc/self/task").unwrap() {
        let dir = entry.unwrap().path();
        let comm = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
        if comm.trim() == "idle-watch" {
            found.push(dir);
        }
    }
    found
}

/// `epoll_wait` / `epoll_pwait` のシステムコール番号 (`wchan` が名前を出さない核のため)。
#[cfg(target_arch = "x86_64")]
const EPOLL_WAIT_NRS: &[&str] = &["232", "281"];
#[cfg(target_arch = "aarch64")]
const EPOLL_WAIT_NRS: &[&str] = &["22"];
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const EPOLL_WAIT_NRS: &[&str] = &[];

/// 眠っていて (`State: S`)、眠っている所が `epoll_wait` (`wchan` の名前か、`syscall` の番号)
/// なら、その引数の `timeout` (`syscall` の 4 つ目の引数。x86_64 の `epoll_wait` と aarch64 の
/// `epoll_pwait` で同じ位置) と `syscr` を返す。読んでいる間に起きて眠り直したら
/// (`syscr` が変わったら) `None` (読み直す)。
fn asleep_in_epoll(dir: &std::path::Path) -> Option<Asleep> {
    let reads = read_syscr(dir)?;
    let status = std::fs::read_to_string(dir.join("status")).ok()?;
    let sleeping = status
        .lines()
        .find_map(|l| l.strip_prefix("State:"))
        .is_some_and(|v| v.trim_start().starts_with('S'));
    // 眠っている間は「番号 引数 1〜6 sp pc」。走っていれば `running` で読めない
    let syscall = std::fs::read_to_string(dir.join("syscall")).ok()?;
    let mut fields = syscall.split_whitespace();
    let nr = fields.next()?;
    let wchan = std::fs::read_to_string(dir.join("wchan")).unwrap_or_default();
    if !sleeping || !(wchan.contains("epoll") || EPOLL_WAIT_NRS.contains(&nr)) {
        return None;
    }
    let arg = fields.nth(3)?;
    let raw = u64::from_str_radix(arg.strip_prefix("0x")?, 16).ok()?;
    // `int` で渡すので下 32 ビット
    let timeout_ms = raw as u32 as i32;
    (read_syscr(dir)? == reads).then_some(Asleep { timeout_ms, reads })
}

fn read_syscr(dir: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(dir.join("io"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("syscr:"))
        .and_then(|v| v.trim().parse().ok())
}

/// 預かりが 0 の間 (`quiet` の長さ) に、`idle-watch` が起きないこと。`when` は文面用。
/// 終わりの様子を返す。
fn assert_stays_asleep(quiet: Duration, when: &str) -> Asleep {
    let before = idle_watch_asleep();
    // 期限つきで待っていない (以前は 1 秒ごとに起きていた)。これは時間に頼らずに言える
    assert_eq!(
        before.timeout_ms, -1,
        "{}のに idle-watch が {} ms の期限つきで待っている",
        when, before.timeout_ms
    );
    thread::sleep(quiet);
    let after = idle_watch_asleep();
    assert_eq!(
        after.timeout_ms, -1,
        "{}のに idle-watch が {} ms の期限つきで待ち直した",
        when, after.timeout_ms
    );
    assert_eq!(
        after.reads - before.reads,
        0,
        "{}のに idle-watch が呼び鈴で {} 回起きた",
        when,
        after.reads - before.reads
    );
    after
}

fn parked(proxy_port: u16) -> u64 {
    status_number(&status_json(proxy_port), "parked_connections")
}

#[test]
fn test_integration_idle_watch_sleeps_while_nothing_is_parked() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let mut cfg = park_config();
    cfg.keepalive = Duration::from_secs(30);
    let (proxy_port, live) = start_test_proxy_with_live_config(cfg.clone());
    let host = format!("127.0.0.1:{}", origin_port);
    // 立ち上がって 1 本目の `/status` (Connection: close なので預けない) が返るまで待つ
    wait_until(|| parked(proxy_port) == 0, "the proxy should start");

    // (1) 預かりが 0 の間は起きない (以前は 1 秒ごとに起きていた: 2.5 秒で 2〜3 回)
    let quiet = assert_stays_asleep(Duration::from_millis(2500), "預かりが 0 な");

    // (2) 眠っている所へ預けると呼び鈴で起き、期限 (30 秒) を待つ側に回る。
    //     そのあと keepalive を 300 ms に縮めて 2 本目を預けると、**先頭より早い期限**
    //     なのでもう一度起こされ、2 本目は 30 秒ではなく 300 ms で閉じる
    let mut a = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut a, &host, "/a");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    wait_until(
        || parked(proxy_port) == 1,
        "the first connection should be parked",
    );
    // 呼び鈴で起きて (`drain` の `read` が増え)、期限つきで待ち直す。上の「起きない」を
    // 見ている目が、起きたときには確かに動くことの裏づけでもある
    let woken = wait_for(
        "預けたのに idle-watch が呼び鈴で起きて期限つきで待ち直さない",
        Duration::from_millis(20),
        Limit::Tries(500),
        || Some(idle_watch_asleep()).filter(|w| w.timeout_ms > 0),
    );
    assert!(
        woken.reads > quiet.reads,
        "預けたのに呼び鈴を読んでいない ({} -> {})",
        quiet.reads,
        woken.reads
    );
    assert!(
        woken.timeout_ms <= 30_000,
        "期限 (30 秒) より長く待っている ({} ms)",
        woken.timeout_ms
    );
    let mut short = cfg.clone();
    short.keepalive = Duration::from_millis(300);
    *live.write().unwrap() = Arc::new(short);
    let mut b = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let (head, _) = one_keepalive_request(&mut b, &host, "/b");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    let t0 = Instant::now();
    b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = [0u8; 1];
    let got = b.read(&mut buf);
    let took = t0.elapsed();
    assert!(
        matches!(got, Ok(0)),
        "早い期限で預けたのに閉じられない ({:?} / {:?})",
        got,
        took
    );
    assert!(
        took < Duration::from_secs(5),
        "早い期限で預けたのに監視スレッドが起きなかった ({:?})",
        took
    );
    assert_eq!(parked(proxy_port), 1, "1 本目 (期限 30 秒) は預かったまま");

    // (3) 1 本目を閉じて預かりが 0 に戻れば、また眠ったまま
    drop(a);
    wait_until(
        || parked(proxy_port) == 0,
        "the first connection should be released",
    );
    assert_stays_asleep(Duration::from_millis(2500), "預かりが 0 に戻った");
}
