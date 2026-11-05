//! 自分宛ての口 (`/healthz`・`/status`・`/metrics`・purge と lookup) の結合テスト。
//!
//! 初めは `tests/proxy_test.rs` にあった (T.34.116 で割った)。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::*;

#[test]
fn test_integration_healthz() {
    let proxy_port = start_test_proxy(proxy_config());

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .write_all(
            format!(
                "GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                proxy_port
            )
            .as_bytes(),
        )
        .unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    // `/status` の写しではなく健康診断 (T14.12)。検査の中身は `tests/kernel_test.rs`
    assert!(response.contains("\"ok\":true"), "{}", response);
    assert!(response.contains("\"checks\":{"), "{}", response);
}

/// 版が `-V` と `/status` で同じ文字列であること (T12.6)。
///
/// 起動ログにも同じものを出しているが、そちらは実バイナリを起こす `reload_test` で見ている。
#[test]
fn test_integration_the_version_is_the_same_in_v_and_in_status() {
    // `build.rs` が作った文字列。git のある環境では `0.18.0+<短いハッシュ>`、
    // `.git` の無いところで作ったら `0.18.0+unknown`
    let version = rust_http_proxy::VERSION;
    assert!(
        version.starts_with(&format!("{}+", env!("CARGO_PKG_VERSION"))),
        "版の形が違う: {}",
        version
    );

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
        .arg("-V")
        .output()
        .expect("could not run the proxy binary");
    assert!(out.status.success());
    // 版のあとにビルドの種類が付く (T.28.42。`config_test` が 3 か所の一致を見ている)
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("rust-http-proxy {} ({})", version, rust_http_proxy::BUILD)
    );

    let proxy_port = start_test_proxy(proxy_config());
    let status = status_json(proxy_port);
    assert!(
        status.contains(&format!("\"version\":\"{}\"", version)),
        "{}",
        status
    );
}

#[test]
fn test_integration_status_embeds_the_parts_from_the_upper_layers() {
    // `/status` の組み立ては endpoints の仕事で、指標 (proxy-metrics) は部品を並べるだけ。
    // 受け渡しが切れると、その部品が丸ごと null になる。
    // テスト用のプロキシは再読込スレッドも状態ファイルも持たないので、
    // settings と state_file はもともと null。生きているのは blocklist と dns の 2 つ
    let proxy_port = start_test_proxy(proxy_config());
    let status = status_json(proxy_port);
    for (key, must_contain) in [
        ("\"blocklist\":", "\"entries\":"),
        ("\"dns\":", "\"ttl_secs\":"),
        ("\"ipv6\":", "\"attempts\":"),
    ] {
        let at = status
            .find(key)
            .unwrap_or_else(|| panic!("{} が無い: {}", key, status));
        let tail = &status[at + key.len()..];
        assert!(
            tail[..200.min(tail.len())].contains(must_contain),
            "{} の中身が空 (上の層からの受け渡しが切れている): {}",
            key,
            &tail[..80.min(tail.len())]
        );
    }
    // 部品の位置 (キーの並び) も変わっていないこと
    let order: Vec<&str> = [
        "\"log_level\":",
        "\"settings\":",
        "\"dns\":",
        "\"ipv6\":",
        "\"blocklist\":",
        "\"state_file\":",
        "\"cache\":",
    ]
    .into_iter()
    .filter(|k| status.contains(k))
    .collect();
    assert_eq!(order.len(), 7, "{}", status);
    let mut at = 0;
    for k in order {
        let i = status[at..]
            .find(k)
            .unwrap_or_else(|| panic!("{} の位置が違う: {}", k, status));
        at += i;
    }
}

/// `PROXY_MAX_CONNS` / `PROXY_MAX_THREADS` の `auto` で決まった値と、いまのスレッドの数が
/// `/status` から見えること (T10.7。以前は起動ログにしか出ていなかった)。
#[test]
fn test_integration_status_shows_the_limits_and_the_thread_counts() {
    let mut cfg = proxy_config();
    cfg.max_conns = 137;
    cfg.max_threads = 41;
    let proxy_port = start_test_proxy(cfg);
    let status = status_json(proxy_port);
    assert!(
        status.contains("\"max_conns\":137"),
        "決まった上限が出ていない: {}",
        status
    );
    assert!(
        status.contains("\"max_threads\":41"),
        "スレッドの上限が出ていない: {}",
        status
    );
    // `/status` を引いている接続そのものがワーカースレッドを 1 本使っているので、
    // 生きているスレッドは 1 本以上。待ち行列は空 (上限に当たっていない)
    let live: usize = field(&status, "\"live_threads\":");
    let idle: usize = field(&status, "\"idle_threads\":");
    assert!(
        live >= 1,
        "生きているスレッドが数えられていない: {}",
        status
    );
    assert!(idle <= live, "空きは生きている数を超えない: {}", status);
    assert!(status.contains("\"queued_jobs\":0"), "{}", status);
}

/// `/status?sort=` が上位 50 の切り出しだけを変えること (T13.3)。
///
/// デプロイ先では要求数の上位 50 にエラーも遅い名前解決も出てこない (58.6 時間で
/// エラー 99 件のうち 80 件が上位 50 の外)。ここでは 4 つのホストを作って、
/// 鍵ごとに先頭が入れ替わり、既定と知らない値は今までの順のままであることを見る。
#[test]
fn test_integration_status_sort_brings_the_bad_hosts_to_the_front() {
    let (origin_port, _origin) = start_mock_origin();
    // 300 ms かけて返すオリジン (`?sort=slow` の的)
    let (slow_port, _slow) = start_origin(
        Arc::new(AtomicUsize::new(0)),
        Arc::new(|_req: &str, _n: usize| {
            thread::sleep(Duration::from_millis(300));
            let body = "slow origin";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        }),
    );
    // 誰も待っていないポート (`?sort=errors` の的)。**束縛したまま持つ** (T20.2): 前は束縛して
    // すぐ手放していたので、使うまでの間 (遅いオリジンの 300 ms を含む) に同じ番号を
    // ほかの待ち受けが取ることがあった。取ったのがプロキシ (下の `start_test_proxy` か、
    // 並んで走っている別のテストバイナリのもの) だと、`/dead` は「自分宛ての知らないパス」
    // になって 502 ではなく 404 が返る
    // (この値が関数の終わりまで生きている間、番号は押さえられている)
    let dead_hold = RefusedPort::reserve();
    let dead_port = dead_hold.port();
    let proxy_port = start_test_proxy(proxy_config());

    // (1) 要求数の多い健全なホスト。IP リテラルなので名前解決の表を通らない
    let busy = format!("127.0.0.1:{}", origin_port);
    for i in 0..3 {
        let r = get_via_proxy(proxy_port, &format!("http://{}/ok{}", busy, i), &busy);
        assert!(r.starts_with("HTTP/1.1 200 OK"), "{}", r);
    }
    // (2) 名前で引くホスト。`localhost` は名前解決の表を通るので `dns_misses` が付く
    let named = format!("localhost:{}", origin_port);
    let r = get_via_proxy(proxy_port, &format!("http://{}/named", named), &named);
    assert!(r.starts_with("HTTP/1.1 200 OK"), "{}", r);
    // (3) 遅いホスト (1 件だけだが平均が飛び抜けて遅い)
    let slow = format!("127.0.0.1:{}", slow_port);
    let r = get_via_proxy(proxy_port, &format!("http://{}/slow", slow), &slow);
    assert!(r.starts_with("HTTP/1.1 200 OK"), "{}", r);
    // (4) 接続を拒まれるホスト (エラー 1 件、原因は refused)
    let dead = format!("127.0.0.1:{}", dead_port);
    let r = get_via_proxy(proxy_port, &format!("http://{}/dead", dead), &dead);
    assert!(r.starts_with("HTTP/1.1 502"), "{}", r);

    let sorted = |q: &str| first_host(&status_json_query(proxy_port, q));
    // 既定 (問い合わせ無し) は今までどおり要求数順
    assert_eq!(
        first_host(&status_json(proxy_port)),
        format!("http://{}", busy)
    );
    assert_eq!(sorted(""), format!("http://{}", busy));
    assert_eq!(sorted("?sort=requests"), format!("http://{}", busy));
    // エラー 1 件のホストが先頭 (要求数では最下位)
    assert_eq!(sorted("?sort=errors"), format!("http://{}", dead));
    // 名前解決を払ったホストが先頭 (loopback のミスは 1 ms 未満で `dns_ms_sum` が
    // 0 に丸まるので、同点は `dns_misses` で崩れる)
    assert_eq!(sorted("?sort=dns"), format!("http://{}", named));
    // 平均の遅いホストが先頭
    assert_eq!(sorted("?sort=slow"), format!("http://{}", slow));
    // 知らない値・空の値は既定に倒れる
    assert_eq!(sorted("?sort=nonsense"), format!("http://{}", busy));
    assert_eq!(sorted("?sort="), format!("http://{}", busy));
    assert_eq!(sorted("?other=1"), format!("http://{}", busy));
    // `/healthz` は問い合わせを読まない (T14.12 で `/status` の写しではなくなったので、
    // そもそも `hosts[]` を持たない軽い応答になっている)
    let health = endpoint_on(
        proxy_port,
        &format!(
            "GET /healthz?sort=errors HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            proxy_port
        ),
    );
    assert!(health.contains("\"checks\":{"), "{}", health);
    assert!(!health.contains("\"hosts\":"), "{}", health);

    // JSON の形は変わらない: 4 ホストが全部入り、内訳の列もそのまま
    let by_errors = status_json_query(proxy_port, "?sort=errors");
    assert_eq!(hosts_in(&by_errors).len(), 4, "{}", by_errors);
    assert_eq!(
        hosts_in(&by_errors).len(),
        hosts_in(&status_json(proxy_port)).len()
    );
    assert!(by_errors.contains("\"errors_by_cause\":["), "{}", by_errors);
    assert!(
        by_errors.contains("\"clients\":[{\"client\":\"127.0.0.1\""),
        "接続元は要求数順のまま: {}",
        by_errors
    );
    // 監視が 5 秒ごとに引く口なので、太らせない
    assert!(by_errors.len() <= 64 * 1024, "{} バイト", by_errors.len());
}

/// `/status` に問い合わせを付けて引く (T13.3 の `?sort=`)。
fn status_json_query(proxy_port: u16, query: &str) -> String {
    endpoint_on(
        proxy_port,
        &format!(
            "GET /status{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            query, proxy_port
        ),
    )
}

/// 自分宛ての要求を 1 本投げて応答を全部読む。
fn endpoint_on(proxy_port: u16, req: &str) -> String {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = stream.read_to_string(&mut out);
    out
}

/// `/status` の `hosts[]` に出ている順のホスト名。
fn hosts_in(status: &str) -> Vec<String> {
    let at = status.find("\"hosts\":[").expect("hosts[] が無い");
    let end = status[at..]
        .find("],\"clients\"")
        .expect("hosts[] の終わり")
        + at;
    status[at..end]
        .split("{\"host\":\"")
        .skip(1)
        .map(|s| s.split('"').next().unwrap_or("").to_string())
        .collect()
}

/// `/status` の `hosts[]` の先頭のホスト名。
fn first_host(status: &str) -> String {
    hosts_in(status).first().cloned().unwrap_or_default()
}

/// `/status` に出した上限といまのスレッド数が、`/metrics` にも gauge として出ること (T11.5)。
/// 運用で見るのは `/metrics` の方なので、`/status` だけだと片肺になる。
#[test]
fn test_integration_metrics_shows_the_limits_and_the_thread_counts() {
    let mut cfg = proxy_config();
    cfg.max_conns = 137;
    cfg.max_threads = 41;
    let proxy_port = start_test_proxy(cfg);
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .write_all(
            format!(
                "GET /metrics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                proxy_port
            )
            .as_bytes(),
        )
        .unwrap();
    let mut metrics = String::new();
    stream.read_to_string(&mut metrics).unwrap();
    for name in [
        "max_connections",
        "max_threads",
        "live_threads",
        "idle_threads",
        "queued_jobs",
    ] {
        assert!(
            metrics.contains(&format!("# TYPE sorahost_{} gauge\n", name)),
            "sorahost_{} の gauge が無い: {}",
            name,
            metrics
        );
    }
    assert!(
        metrics.contains("sorahost_max_connections 137\n"),
        "決まった上限が出ていない: {}",
        metrics
    );
    assert!(
        metrics.contains("sorahost_max_threads 41\n"),
        "スレッドの上限が出ていない: {}",
        metrics
    );
    assert!(
        metrics.contains("sorahost_queued_jobs 0\n"),
        "待ち行列は空のはず: {}",
        metrics
    );
    // Happy Eyeballs の族ごとの勝敗 (T12.1)。誰もつないでいないので 0 のまま出る
    for name in [
        "ipv6_attempts_total",
        "ipv6_wins_total",
        "ipv6_losses_total",
    ] {
        assert!(
            metrics.contains(&format!("# TYPE sorahost_{} counter\n", name))
                && metrics.contains(&format!("\nsorahost_{} 0\n", name)),
            "sorahost_{} が無い: {}",
            name,
            metrics
        );
    }
    // `/metrics` を引いている接続そのものがワーカースレッドを 1 本使っている
    let live: usize = field(&metrics, "\nsorahost_live_threads ");
    let idle: usize = field(&metrics, "\nsorahost_idle_threads ");
    assert!(
        live >= 1,
        "生きているスレッドが数えられていない: {}",
        metrics
    );
    assert!(idle <= live, "空きは生きている数を超えない: {}", metrics);
}

/// `/status` の JSON や `/metrics` の本文から `key` に続く数を取る (テスト用の雑な取り出し)。
fn field(status: &str, key: &str) -> usize {
    let at = status
        .find(key)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, status))
        + key.len();
    let end = status[at..]
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(status.len() - at);
    status[at..at + end].parse().expect("数字")
}

#[test]
fn test_integration_metrics_purge_and_lookup_endpoints() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) =
        start_counting_origin(Arc::clone(&counter), "Cache-Control: max-age=60\r\n");
    let proxy_port = start_test_proxy_with_cache(proxy_config(), cache_cfg("shp-it-endpoints"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/asset", host);

    get_via_proxy(proxy_port, &url, &host);
    let hit = get_via_proxy(proxy_port, &url, &host);
    assert!(hit.contains("X-Cache: HIT"));

    let endpoint = |req: &str| {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        stream.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).unwrap();
        out
    };

    // /lookup はエントリを報告する (LRU には触らない)
    let looked = endpoint(&format!(
        "GET /lookup?url={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        url, proxy_port
    ));
    assert!(looked.starts_with("HTTP/1.1 200 OK"), "{}", looked);
    assert!(
        looked.contains("\"found\":true") && looked.contains("\"memory\":true"),
        "{}",
        looked
    );

    // /metrics は Prometheus 形式でヒット数とホスト別統計を出す
    let metrics = endpoint(&format!(
        "GET /metrics HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        proxy_port
    ));
    assert!(metrics.contains("text/plain; version=0.0.4"), "{}", metrics);
    assert!(
        metrics.contains("sorahost_cache_hits_total{tier=\"memory\"} 1"),
        "{}",
        metrics
    );
    assert!(
        metrics.contains(&format!(
            "sorahost_host_hits_total{{host=\"http://{}\"}} 1",
            host
        )),
        "{}",
        metrics
    );
    assert!(
        metrics.contains(&format!(
            "sorahost_host_misses_total{{host=\"http://{}\"}} 1",
            host
        )),
        "{}",
        metrics
    );

    // /status にもホスト別が入る
    let status = endpoint(&format!(
        "GET /status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        proxy_port
    ));
    assert!(
        status.contains(&format!(
            "\"host\":\"http://{}\",\"requests\":2,\"hits\":1,\"misses\":1",
            host
        )),
        "{}",
        status
    );

    // PURGE メソッドで消える → 次は MISS
    let purged = endpoint(&format!("PURGE {} HTTP/1.1\r\nHost: {}\r\n\r\n", url, host));
    assert!(
        purged.starts_with("HTTP/1.1 200 OK") && purged.contains("\"purged\":1"),
        "{}",
        purged
    );
    let after = get_via_proxy(proxy_port, &url, &host);
    assert!(!after.contains("X-Cache"), "{}", after);
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    // /purge?all=1 で全消去、/lookup は 404
    let all = endpoint(&format!(
        "GET /purge?all=1 HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        proxy_port
    ));
    assert!(all.contains("\"all\":true"), "{}", all);
    let gone = endpoint(&format!(
        "GET /lookup?url={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        url, proxy_port
    ));
    assert!(
        gone.starts_with("HTTP/1.1 404") && gone.contains("\"found\":false"),
        "{}",
        gone
    );
    let bad = endpoint(&format!(
        "GET /purge HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        proxy_port
    ));
    assert!(bad.starts_with("HTTP/1.1 400"), "{}", bad);
}
