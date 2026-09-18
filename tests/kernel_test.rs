//! カーネルと cgroup の窓と、本当の `/healthz` の結合テスト (T14.12)。
//!
//! 見るのは 3 つ:
//!
//! 1. `/healthz` が**軽い JSON** で 200 と `{"ok":true,...}` を返すこと
//! 2. `PROXY_MAX_CONNS=1` を 1 本握ったまま呼ぶと `connections` の検査が偽で **503**
//!    (T13.2 の「上限 + 4 本」の枠があるので `/healthz` 自体は届く)
//! 3. 実バイナリ (履歴スレッドが動く配線) で `/history?res=5` の `kernel` の配列に
//!    `time_wait` と `psi_cpu_some_avg10` が出ること。`/status` の `kernel` と
//!    `/metrics` の `sorahost_kernel_*` も同じ標本から出る
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::time::Duration;

mod common;
use common::*;

/// 自分宛ての 1 要求を投げて応答全文を返す (状態行を見るので `endpoint_json` は使わない)。
fn get(port: u16, path: &str) -> String {
    raw_get(
        port,
        &format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            path, port
        ),
    )
}

fn body_of(resp: &str) -> &str {
    resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
}

/// 読むだけで何も返さず、閉じもしないリスナー (`overload_test` と同じ道具)。
///
/// ここへ張ったトンネルでクライアントが送信側だけ閉じると**片方向だけ EOF**になり、
/// 預かり所に預けられない = 上限に当たっても「閉じて席を作る」が効かない。
fn start_quiet_origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while !matches!(stream.read(&mut buf), Ok(0) | Err(_)) {}
                std::mem::forget(stream);
            });
        }
    });
    port
}

/// `/healthz` は `/status` の写しではなく、軽い健康診断の JSON を返す。
#[test]
fn test_integration_healthz_is_a_small_health_report() {
    let port = start_test_proxy(proxy_config());

    let resp = get(port, "/healthz");
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{}", resp);
    let body = body_of(&resp);
    assert!(
        body.starts_with("{\"schema\":1,\"ok\":true,\"checks\":{"),
        "{}",
        body
    );
    // 検査の 6 つが全部あること (読めないものは null)
    for check in [
        "listening",
        "fds",
        "connections",
        "state_file",
        "listen_overflows",
        "resolver",
    ] {
        assert!(
            body.contains(&format!("\"{}\":", check)),
            "{} が無い",
            check
        );
    }
    // 待ち受けと記述子はこの環境でも必ず調べられる
    assert!(body.contains("\"listening\":{\"ok\":true"), "{}", body);
    assert!(body.contains("\"fds\":{\"ok\":true"), "{}", body);
    // **`/status` の写しではない**: ホスト別統計も設定も入っていない軽い応答
    assert!(!body.contains("\"hosts\":"), "{}", body);
    assert!(!body.contains("\"settings\":"), "{}", body);
    assert!(body.len() <= 1024, "{} バイト: {}", body.len(), body);
    // 問い合わせは読まない (監視が叩く口の意味を変えない)
    let with_query = get(port, "/healthz?sort=errors");
    assert!(with_query.starts_with("HTTP/1.1 200 OK"), "{}", with_query);
    assert!(
        !body_of(&with_query).contains("\"hosts\":"),
        "{}",
        with_query
    );
}

/// 上限を 1 本握ったまま呼ぶと `connections` の検査が偽で 503。
///
/// 握るのは**片方向だけ EOF のトンネル** (預けられない = 席を作れない) なので、
/// `/healthz` は T13.2 の「上限 + 4 本」の枠で届く。届いた上で 503 を返すのが
/// このタスクの要点 (「取りに行けない」と「取りに行けたが不健康」は別物)。
#[test]
fn test_integration_healthz_is_503_when_the_connection_limit_is_full() {
    let origin_port = start_quiet_origin();
    let mut cfg = park_config();
    cfg.max_conns = 1;
    cfg.keepalive = Duration::from_secs(60);
    let (port, metrics) = start_test_proxy_with_metrics(cfg);

    let mut tunnel = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
    tunnel.set_nodelay(true).unwrap();
    tunnel
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    tunnel
        .write_all(
            format!(
                "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                origin_port, origin_port
            )
            .as_bytes(),
        )
        .unwrap();
    let head = read_connect_response(&mut tunnel);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    tunnel.shutdown(std::net::Shutdown::Write).unwrap();
    wait_until(
        || metrics.active_connections.load(Ordering::Relaxed) == 1,
        "トンネルが 1 本立つ",
    );

    let resp = get(port, "/healthz");
    assert!(
        resp.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{}",
        resp
    );
    let body = body_of(&resp);
    assert!(body.starts_with("{\"schema\":1,\"ok\":false,"), "{}", body);
    assert!(
        body.contains("\"connections\":{\"ok\":false,\"active\":1,\"max\":1}"),
        "{}",
        body
    );
    // 他の検査は巻き込まれていない (落ちた理由が 1 つだけ読めること)
    assert!(body.contains("\"listening\":{\"ok\":true"), "{}", body);
    assert!(body.contains("\"fds\":{\"ok\":true"), "{}", body);

    // 続けて引いても同じ (検査は状態を変えない)
    let again = get(port, "/healthz");
    assert!(
        again.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{}",
        again
    );
    // 握ったトンネルはここで落とすが、**閉じるのを待たない**: 片方向 EOF の相手が
    // 何も送ってこないので、プロキシがこの close に気づくのは `PROXY_TUNNEL_IDLE_SECS`
    // (既定 300 秒) のあとになる。席が空いたときに 200 に戻ることは上のテストで見ている
    drop(tunnel);
}

/// 実バイナリ (履歴スレッドが動く配線) で、5 秒の標本がカーネルの窓に入ること。
///
/// テスト用プロキシ (`start_test_proxy`) は履歴スレッドを起こさないので、この 1 本だけ
/// `HOME` を渡して本物を起こす (`--lite` / `PROXY_STATS_PERSIST=off` では窓は空のまま =
/// `/status` の `kernel` は `null`、というのも仕様のうち)。
#[test]
fn test_integration_kernel_window_shows_up_in_history_status_and_metrics() {
    let dir = std::env::temp_dir().join(format!("rhp-t1412-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\n",
    )
    .unwrap();
    let proxy = ProxyProcess::start(&dir);

    // (1) `/history?res=5` の `kernel` の配列
    let hist = endpoint_json(proxy.port, "/history?res=5");
    let kernel = hist
        .split_once("\"kernel\":")
        .map(|(_, k)| k)
        .unwrap_or_else(|| panic!("/history に kernel が無い: {}", hist));
    assert!(
        kernel.starts_with("{\"interval_secs\":5,\"keys\":[\"t\","),
        "{}",
        &kernel[..80.min(kernel.len())]
    );
    for key in [
        "time_wait",
        "psi_cpu_some_avg10",
        "listen_overflows",
        // T15.0 (6) で `keys` の末尾に足した列 (絞られた割合の分母)
        "cpu_nr_periods",
    ] {
        assert!(kernel.contains(&format!("\"{}\"", key)), "{} が無い", key);
    }
    // 標本が 1 本以上あり、列の数が keys と合っている
    let keys: Vec<&str> = kernel
        .split_once("\"keys\":[")
        .and_then(|(_, r)| r.split_once(']'))
        .map(|(k, _)| k.split(',').collect())
        .unwrap();
    let rows = kernel
        .split_once("\"samples\":[[")
        .unwrap_or_else(|| panic!("標本が 1 本も無い: {}", kernel))
        .1;
    let first: Vec<&str> = rows.split(']').next().unwrap().split(',').collect();
    assert_eq!(
        first.len(),
        keys.len(),
        "列の数が keys と合わない: {:?}",
        first
    );
    // Linux なので `/proc/net/sockstat` は読める = `time_wait` は数
    let tw = first[keys.iter().position(|k| *k == "\"time_wait\"").unwrap()];
    assert!(
        tw.parse::<u64>().is_ok(),
        "time_wait が数で出ていない: {}",
        tw
    );
    // 1 時間の解像度にはこの窓が無い
    let hour = endpoint_json(proxy.port, "/history?res=3600");
    assert!(hour.contains("\"kernel\":null"), "{}", hour);

    // (2) `/status` の `kernel` の節
    let status = endpoint_json(proxy.port, "/status");
    let k = status
        .split_once("\"kernel\":")
        .map(|(_, k)| k)
        .unwrap_or_else(|| panic!("/status に kernel が無い"));
    assert!(k.starts_with("{\"at\":"), "{}", &k[..60.min(k.len())]);
    assert!(k.contains("\"time_wait\":"), "{}", k);
    assert!(k.contains("\"psi\":"), "{}", k);
    assert!(k.contains("\"last_5m\":"), "{}", k);
    // cgroup が読める機械なら、絞りの割合の分母と読んでいる道と起動からの増分が出る
    // (T15.0 (6))。cgroup v1 / cgroup 無しの機械では節ごと `null`
    let cg = k
        .split_once("\"cgroup_cpu\":")
        .map(|(_, r)| r)
        .unwrap_or_else(|| panic!("/status に cgroup_cpu が無い: {}", k));
    if !cg.starts_with("null") {
        for key in ["nr_periods", "path", "since_start"] {
            assert!(
                cg.contains(&format!("\"{}\":", key)),
                "{} が無い: {}",
                key,
                cg
            );
        }
        let since = cg
            .split_once("\"since_start\":")
            .map(|(_, r)| r.split('}').next().unwrap())
            .unwrap();
        // 起動からの増分は「いまの累計 − 最初に読んだ累計」なので、数えた期間より多くない
        for key in ["nr_periods", "nr_throttled", "throttled_usec"] {
            assert!(since.contains(&format!("\"{}\":", key)), "{}", since);
        }
    }
    let tw = k
        .split_once("\"time_wait\":")
        .map(|(_, r)| r.split([',', '}']).next().unwrap())
        .unwrap();
    assert!(
        tw.parse::<u64>().is_ok(),
        "/status の time_wait が数でない: {}",
        tw
    );

    // (3) `/metrics` の累計と PSI
    let metrics = endpoint_json(proxy.port, "/metrics");
    for name in [
        "sorahost_kernel_listen_overflows_total",
        "sorahost_kernel_time_wait",
        "sorahost_kernel_retrans_segs_total",
        "sorahost_psi_some_avg10{resource=\"cpu\"}",
    ] {
        assert!(metrics.contains(name), "{} が /metrics に無い", name);
    }

    // (4) 状態ファイルがあるので `/healthz` の `state_file` の検査は `null` でなくなる
    let resp = get(proxy.port, "/healthz");
    let health = body_of(&resp);
    assert!(
        health.starts_with("{\"schema\":1,\"ok\":true,"),
        "{}",
        health
    );
    assert!(
        health.contains("\"state_file\":{\"ok\":true,\"write_errors_5m\":0}"),
        "{}",
        health
    );
    assert!(
        health.contains("\"listen_overflows\":{\"ok\":true"),
        "{}",
        health
    );

    drop(proxy);
    let _ = std::fs::remove_dir_all(&dir);
}
