//! キャッシュの再検証と古い表現の提供 (304・オリジンの失敗・grace・裏の再検証) の結合テスト。
//!
//! 初めは `tests/cache_test.rs` にあった (T.34.118 で割った)。

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

mod common;
use common::*;

#[test]
fn test_integration_stale_entry_is_revalidated_with_304() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = revalidating_origin(Arc::clone(&counter), false);
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-reval"));
    let url = format!("http://127.0.0.1:{}/reval", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(
        first.contains("revalidated body") && !first.contains("X-Cache"),
        "{}",
        first
    );

    // max-age=0 なので 2 回目は条件付きで再検証 → 304 → 保存済み本文を配信
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.starts_with("HTTP/1.1 200 OK"), "{}", second);
    assert!(
        second.contains("X-Cache: REVALIDATED from rust-http-proxy (memory)"),
        "{}",
        second
    );
    assert!(second.contains("revalidated body"), "{}", second);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "origin sees a conditional request"
    );
}

#[test]
fn test_integration_stale_is_served_when_origin_fails() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = revalidating_origin(Arc::clone(&counter), true);
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-stale"));
    let url = format!("http://127.0.0.1:{}/stale", origin_port);
    let host = format!("127.0.0.1:{}", origin_port);

    get_via_proxy(proxy_port, &url, &host);
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.starts_with("HTTP/1.1 200 OK"), "{}", second);
    assert!(
        second.contains("X-Cache: STALE from rust-http-proxy (memory)"),
        "{}",
        second
    );
    assert!(second.contains("revalidated body"), "{}", second);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_integration_grace_serves_stale_and_refreshes_in_background() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(|req, n| {
            if req.contains("If-None-Match: \"g1\"") {
                // 再検証では鮮度を長めに延ばす。max-age=1 のままだと、再検証の完了を
                // 待っている間に再び期限が切れて、次が HIT にならないことがある
                return b"HTTP/1.1 304 Not Modified\r\nETag: \"g1\"\r\nCache-Control: max-age=60\r\nConnection: close\r\n\r\n".to_vec();
            }
            let body = format!("grace body {}", n);
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"g1\"\r\nCache-Control: max-age=1\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    );
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-grace"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/grace", host);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.ends_with("grace body 1"), "{}", first);
    thread::sleep(Duration::from_millis(1500));

    // 期限切れ直後: すぐ古い表現が返り (REFRESHING)、裏で再検証が走る
    let started = std::time::Instant::now();
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(
        second.contains("X-Cache: REFRESHING from rust-http-proxy (memory)"),
        "{}",
        second
    );
    assert!(second.ends_with("grace body 1"), "{}", second);
    // 裏で取り直すので、オリジンの応答を待っていない = 十分速い (負荷の高い CI でも通る幅にする)
    assert!(started.elapsed() < Duration::from_millis(2000));

    // 裏の再検証 (304) が「終わる」まで待つ。オリジンに届いた時点 (counter >= 2) では
    // まだ保存が終わっていないことがあるので、実行中の数が 0 に戻るのを見る
    wait_until(
        || {
            counter.load(Ordering::SeqCst) >= 2
                && status_json(proxy_port).contains("\"revalidating\":0")
        },
        "the background revalidation should finish",
    );
    let third = get_via_proxy(proxy_port, &url, &host);
    assert!(third.contains("X-Cache: HIT"), "{}", third);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "one conditional request in the background"
    );
}

/// 名前が `name` の OS スレッドの数。Linux 以外では数えられないので常に 0。
#[cfg(target_os = "linux")]
fn threads_named(name: &str) -> usize {
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    dir.filter_map(|e| e.ok())
        .filter(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == name))
        .count()
}

#[cfg(not(target_os = "linux"))]
fn threads_named(_name: &str) -> usize {
    0
}

/// max-age=1 + ETag の表現を返し、条件付き要求には `delay` だけ待ってから 304 を返すオリジン。
fn slow_revalidating_origin(
    counter: Arc<AtomicUsize>,
    delay: Duration,
) -> (u16, thread::JoinHandle<()>) {
    start_origin(
        counter,
        Arc::new(move |req: &str, _n| {
            if req.contains("If-None-Match:") {
                // 再検証を重ならせるために、条件付き要求だけ遅らせる
                thread::sleep(delay);
                return b"HTTP/1.1 304 Not Modified\r\nETag: \"w1\"\r\nCache-Control: max-age=60\r\nConnection: close\r\n\r\n".to_vec();
            }
            let body = "worker pool body";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"w1\"\r\nCache-Control: max-age=1\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    )
}

/// 裏側の再検証は接続スレッドの置き場の中で走る (T11.3)。
///
/// 以前は再検証のたびに `thread::spawn` していたので、`PROXY_MAX_THREADS` (T10.5) の
/// **外**にスレッドが増えた。再検証が集中しても
///
/// - `Workers` の外に「revalidate」スレッドが 1 本も出ないこと
/// - 生きているスレッドが上限を超えないこと
///
/// を、burst の最中に細かく見張って確かめる。
#[test]
fn test_integration_background_revalidation_stays_inside_the_worker_pool() {
    const MAX_THREADS: usize = 4;
    const URLS: usize = 8;

    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        slow_revalidating_origin(Arc::clone(&counter), Duration::from_millis(300));
    let mut cfg = proxy_config();
    cfg.max_threads = MAX_THREADS;
    let (proxy_port, workers) = start_test_proxy_with_workers(cfg, cache_cfg("shp-it-revalpool"));
    let host = format!("127.0.0.1:{}", origin_port);
    let urls: Vec<String> = (0..URLS)
        .map(|i| format!("http://{}/pool{}", host, i))
        .collect();

    // まず全部を保存する (max-age=1)
    for url in &urls {
        let r = get_via_proxy(proxy_port, url, &host);
        assert!(r.ends_with("worker pool body"), "{}", r);
    }
    thread::sleep(Duration::from_millis(1200));
    let before_reval = json::uint(&status_json(proxy_port), "background_revalidations");

    // burst の最中ずっと見張る
    let stop = Arc::new(AtomicBool::new(false));
    let max_live = Arc::new(AtomicUsize::new(0));
    let stray = Arc::new(AtomicUsize::new(0));
    let watcher = {
        let (stop, max_live, stray, workers) = (
            Arc::clone(&stop),
            Arc::clone(&max_live),
            Arc::clone(&stray),
            Arc::clone(&workers),
        );
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                max_live.fetch_max(workers.live_count(), Ordering::SeqCst);
                stray.fetch_max(threads_named("revalidate"), Ordering::SeqCst);
                thread::sleep(Duration::from_millis(2));
            }
        })
    };

    // 期限切れの表現を次々に叩く。1 本ずつでも、オリジンが 300 ms 待つので裏側の再検証は
    // 重なっていく (上限 4 のうち 1 本は要求を処理しているので、裏に回せるのは 3 本まで)
    for url in &urls {
        let r = get_via_proxy(proxy_port, url, &host);
        assert!(r.starts_with("HTTP/1.1 200 OK"), "{}", r);
        assert!(r.ends_with("worker pool body"), "{}", r);
    }
    // 裏で走っている再検証が終わるまで見張り続ける
    wait_until(
        || status_json(proxy_port).contains("\"revalidating\":0"),
        "background revalidations should finish",
    );
    stop.store(true, Ordering::SeqCst);
    watcher.join().expect("watcher thread");

    assert_eq!(
        stray.load(Ordering::SeqCst),
        0,
        "再検証が置き場の外でスレッドを起こしている"
    );
    assert!(
        max_live.load(Ordering::SeqCst) <= MAX_THREADS,
        "生きているスレッドが上限 {} を超えた: {}",
        MAX_THREADS,
        max_live.load(Ordering::SeqCst)
    );
    // 上限 4 に 8 本ぶつけたので、裏で走ったものと捨てたものの両方が出る
    let status = status_json(proxy_port);
    let done = json::uint(&status, "background_revalidations") - before_reval;
    let dropped = json::uint(&status, "revalidations_dropped");
    assert!(
        done >= 1,
        "空きがあるときは裏で走らせるはず: done={} dropped={}",
        done,
        dropped
    );
    assert_eq!(
        done + dropped,
        URLS as u64,
        "どの要求も「裏で走る」か「捨てる」のどちらか: done={} dropped={}",
        done,
        dropped
    );
}

/// 空いている接続スレッドが 1 本も無ければ、裏側の再検証は**待ち行列に積まず捨てる** (T11.3)。
///
/// 捨てても正しさは崩れない: その要求はそのまま同期の再検証に回り、クライアントは
/// 新しい表現を受け取る。捨てた回数は `/status` に出る。
#[test]
fn test_integration_background_revalidation_is_dropped_when_threads_are_capped() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        slow_revalidating_origin(Arc::clone(&counter), Duration::from_millis(0));
    let mut cfg = proxy_config();
    // 上限 1 = いま要求を処理しているスレッドで全部。裏の再検証に回せる空きは無い
    cfg.max_threads = 1;
    let proxy_port = start_test_proxy_with_cache(cfg, cache_cfg("shp-it-revaldrop"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/capped", host);

    let first = get_via_proxy(proxy_port, &url, &host);
    assert!(first.ends_with("worker pool body"), "{}", first);
    thread::sleep(Duration::from_millis(1200));

    // 期限切れ直後。裏へ回せないので REFRESHING にはならず、同期で再検証して返す
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(second.starts_with("HTTP/1.1 200 OK"), "{}", second);
    assert!(second.ends_with("worker pool body"), "{}", second);
    assert!(
        !second.contains("REFRESHING"),
        "空きが無いのに裏へ回してはいけない: {}",
        second
    );

    let status = status_json(proxy_port);
    assert_eq!(
        json::uint(&status, "revalidations_dropped"),
        1,
        "捨てた再検証が数えられていない: {}",
        status
    );
    assert_eq!(
        json::uint(&status, "revalidating"),
        0,
        "捨てたのに「再検証中」の印が残っている: {}",
        status
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "オリジンへは最初の取得と同期の再検証の 2 回"
    );
}

#[test]
fn test_integration_slow_origin_gives_up_and_serves_stale() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_origin(
        Arc::clone(&counter),
        Arc::new(|_req, n| {
            if n > 1 {
                thread::sleep(Duration::from_secs(4));
            }
            let body = "slow body";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"s1\"\r\nCache-Control: max-age=0\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    );
    let mut cfg = cache_cfg("shp-it-slow");
    cfg.stale_wait = Duration::from_secs(1);
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cfg);
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/slow", host);

    get_via_proxy(proxy_port, &url, &host);
    // 期限切れ (max-age=0) → 同期再検証だが、1 秒待って応答が無ければ stale を返す
    let started = std::time::Instant::now();
    let second = get_via_proxy(proxy_port, &url, &host);
    assert!(
        second.contains("X-Cache: STALE from rust-http-proxy (memory)"),
        "{}",
        second
    );
    assert!(second.ends_with("slow body"), "{}", second);
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "{:?}",
        started.elapsed()
    );
}
