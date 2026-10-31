//! 名前解決の締め切り (`PROXY_DNS_TIMEOUT_SECS`) と、引かせるスレッド群の上限の結合テスト (T.32.11)。
//!
//! **このファイルにはテストを 1 つしか置かないこと。** 名前解決の差し替え
//! (`dns::set_resolver_for_test`)・締め切り・上限はプロセス全体に効くので、同じテストバイナリの
//! 他のテストを道連れにしてしまう。
//!
//! 応答しないリゾルバ (手元の再現は `unshare -m` で `/etc/resolv.conf` を黙る 127.0.0.1:53 に
//! 向けたもの) の代わりに、`slow-` で始まる名前は 3 秒眠ってから失敗する関数に差し替える。

use std::io;
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

mod common;
use common::*;

use rust_http_proxy::dns;

/// 遅い名前の `getaddrinfo` の代わり (3 秒眠ってから失敗)。速い名前は 127.0.0.1 を即答する。
fn fake_resolver(host: &str, _: u16) -> io::Result<Vec<IpAddr>> {
    if host.starts_with("slow-") {
        thread::sleep(Duration::from_secs(3));
        return Err(io::Error::other("Temporary failure in name resolution"));
    }
    // 締め切りより少し遅れて答えが出る名前
    if host.starts_with("late-") {
        thread::sleep(Duration::from_millis(1500));
    }
    Ok(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
}

fn connect_status(proxy_port: u16, target: &str) -> (String, Duration) {
    let t0 = Instant::now();
    let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    std::io::Write::write_all(
        &mut s,
        format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes(),
    )
    .unwrap();
    let line = read_connect_response(&mut s);
    (line, t0.elapsed())
}

#[test]
fn test_integration_slow_names_give_up_with_504_and_leave_room_for_fast_requests() {
    dns::set_resolver_for_test(Some(fake_resolver));
    dns::set_timeout(Duration::from_secs(1));
    // ワーカー 8 本 → 名前解決のスレッド群は 2 本 (本番の配線と同じ 1/4)
    let threads = 8;
    dns::set_resolver_limit(dns::resolver_limit_for_threads(threads));
    assert_eq!(dns::resolver_limit(), 2);

    let (origin_port, _origin) = start_mock_origin();
    let mut cfg = proxy_config();
    cfg.max_conns = threads;
    cfg.max_threads = threads;
    let proxy_port = start_test_proxy(cfg);
    let fast = format!("fast.t3211.test:{origin_port}");

    // 速い名前を 1 回引いて表に載せる (本番の熱い名前と同じ = 当たりの道)
    let warm = get_via_proxy(proxy_port, &format!("http://{fast}/warm"), &fast);
    assert!(warm.starts_with("HTTP/1.1 200"), "{warm}");

    // 相異なる遅い名前をワーカーの数だけ同時に出す (再現の攻撃と同じ形)
    let started = Instant::now();
    let slow: Vec<_> = (0..threads)
        .map(|i| {
            thread::spawn(move || connect_status(proxy_port, &format!("slow-{i}.t3211.test:443")))
        })
        .collect();
    thread::sleep(Duration::from_millis(300));
    // 遅い名前が引かれている窓の中で、正当な速い要求が 200 を受ける (前は 503)
    let t0 = Instant::now();
    let during = get_via_proxy(proxy_port, &format!("http://{fast}/during"), &fast);
    let during_took = t0.elapsed();
    assert!(
        during.starts_with("HTTP/1.1 200"),
        "遅い名前の窓の中の速い要求: {during}"
    );
    assert!(during_took < Duration::from_secs(1), "{during_took:?}");

    let results: Vec<_> = slow.into_iter().map(|h| h.join().unwrap()).collect();
    for (line, took) in &results {
        assert!(line.starts_with("HTTP/1.1 504"), "遅い名前: {line}");
        // 締め切り (1 秒) で枠を手放す。`getaddrinfo` の 3 秒は待たない
        assert!(*took < Duration::from_millis(2500), "{took:?}");
    }
    assert!(started.elapsed() < Duration::from_millis(2900));

    let status = status_json(proxy_port);
    // 2 本は締め切りで、残りは上限で待たずに断った
    assert_eq!(status_number(&status, "timeouts"), 2, "{status}");
    assert_eq!(
        status_number(&status, "resolver_busy"),
        threads as u64 - 2,
        "{status}"
    );
    // ワーカーが名前解決に張り付いていない (この `/status` の 1 本だけが動いている)
    let busy_workers =
        status_number(&status, "live_threads") - status_number(&status, "idle_threads");
    assert!(busy_workers <= 2, "{status}");
    // はぐれた `getaddrinfo` は 3 秒で終わり、スレッド群の数は 0 に戻る
    wait_until(
        || dns::resolver_counters().3 == 0,
        "はぐれた名前解決が終わる",
    );
    let errors = endpoint_json(proxy_port, "/errors");
    // `/errors` の個票は名前解決の一時的な失敗 (`dns_temp`) の 504 (`/log` の WARN の文言は `dns_timeout`)
    assert!(
        errors.contains("\"cause\":\"dns_temp\"") && errors.contains("\"status\":504"),
        "{errors}"
    );

    // 平文の要求も同じ: 締め切りで 504。遅れて出た答えは表に書かれ、次の要求は当たって 200
    let late = format!("late-1.t3211.test:{origin_port}");
    let t0 = Instant::now();
    let first = get_via_proxy(proxy_port, &format!("http://{late}/a"), &late);
    assert!(first.starts_with("HTTP/1.1 504"), "{first}");
    assert!(
        t0.elapsed() < Duration::from_millis(1400),
        "{:?}",
        t0.elapsed()
    );
    wait_until(|| dns::resolver_counters().2 == 1, "遅れた答えを表に書く");
    let second = get_via_proxy(proxy_port, &format!("http://{late}/b"), &late);
    assert!(second.starts_with("HTTP/1.1 200"), "{second}");

    dns::set_resolver_for_test(None);
    dns::set_timeout(dns::TIMEOUT_DEFAULT);
}
