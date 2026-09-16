//! canary の結合テスト (T14.10)。
//!
//! 「利用者の要求が無い時間帯も待ちを測る」ものなので、見るのは 4 つ:
//! **回ると `/status` と `/history` に値が出る**、**`off` で 1 本も繋がない**、
//! **失敗が `/errors` に `kind: "canary"` で 1 件残る**、**`auto` が上位ホストを選ぶ**。
//!
//! canary の状態はプロセスに 1 つ (`crates/metrics/src/canary.rs` の静的な窓と
//! `canary` スレッド 1 本) なので、**この 1 本のテストで順に確かめる** (並列に走らせると
//! 互いの設定を上書きしてしまう)。周期は `PROXY_CANARY_SECS` に当たる口から 1 秒にする。

mod common;

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::*;
use rust_http_proxy::cache::{Cache, CacheConfig};
use rust_http_proxy::canary;

/// 周期 (試験用。`PROXY_CANARY_SECS=1` と同じ)。
const PERIOD: Duration = Duration::from_secs(1);
/// 「1 本も繋がない」ことを見るときに待つ時間 (2 周期 + 余裕)。
const TWO_PERIODS: Duration = Duration::from_millis(2_500);

/// `/status` の `"canary":{…}` を切り出す (中に入れ子の括弧は無い)。
fn canary_obj(status: &str) -> String {
    let at = status
        .find("\"canary\":{")
        .unwrap_or_else(|| panic!("/status に canary が無い: {}", status));
    let rest = &status[at + "\"canary\":".len()..];
    rest[..rest.find('}').unwrap() + 1].to_string()
}

#[test]
fn test_integration_canary_measures_dns_and_connect_without_any_user_request() {
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());

    // (1) 宛先を手で並べたら、利用者の要求が 1 本も無いまま測り始める
    let probed = Arc::new(AtomicUsize::new(0));
    let (target_port, _t) = start_counting_origin(Arc::clone(&probed), "");
    let target = format!("127.0.0.1:{}", target_port);
    canary::configure(&target, PERIOD);

    // 本番の main.rs と同じ配線: canary を回すのは履歴スレッドの周期 (T14.10)
    let cache = Arc::new(Cache::new(CacheConfig::disabled()));
    let _history = rust_http_proxy::history::spawn(Arc::clone(&metrics), cache, None);

    wait_until(
        || probed.load(Ordering::SeqCst) >= 2,
        "canary が 2 周期ぶん繋ぐ",
    );
    let status = status_json(proxy_port);
    let canary = canary_obj(&status);
    assert!(canary.contains("\"mode\":\"hosts\""), "{}", canary);
    assert!(canary.contains("\"secs\":1"), "{}", canary);
    assert!(
        canary.contains(&format!("\"host\":\"{}\"", target)),
        "{}",
        canary
    );
    assert!(canary.contains("\"dns_ms\":"), "{}", canary);
    assert!(canary.contains("\"connect_ms\":"), "{}", canary);
    assert!(canary.contains("\"error\":null"), "{}", canary);
    let at = status_number(&canary, "at");
    assert!(at > 1_700_000_000, "時刻が epoch 秒でない: {}", canary);
    assert!(status_number(&canary, "runs") >= 2, "{}", canary);
    assert_eq!(status_number(&canary, "failures"), 0, "{}", canary);

    // `/history` には**別の配列**として出る (既存の keys / samples は変わらない)
    let history = endpoint_json(proxy_port, "/history?res=5");
    assert!(
        history.contains("\"keys\":[\"t\",\"requests\","),
        "既存の列が変わった: {}",
        &history[..120]
    );
    let canary_series = canary_obj_series(&history);
    assert!(
        canary_series.starts_with(
            "{\"keys\":[\"t\",\"canary_dns_ms\",\"canary_connect_ms\",\"canary_host\"],\"samples\":[["
        ),
        "{}",
        canary_series
    );
    assert!(
        canary_series.contains(&format!("\"{}\"]", target)),
        "宛先が無い: {}",
        canary_series
    );
    // 1 分の窓にも同じ行がある (`/history?res=60`)
    let minute = canary_obj_series(&endpoint_json(proxy_port, "/history?res=60"));
    assert!(minute.contains(&format!("\"{}\"]", target)), "{}", minute);
    // /metrics にも最後の値が出る
    let prom = endpoint_json(proxy_port, "/metrics");
    assert!(
        prom.contains("sorahost_canary_seconds{stage=\"dns\"}"),
        "{}",
        prom.lines()
            .filter(|l| l.contains("canary"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(prom.contains("sorahost_canary_seconds{stage=\"connect\"}"));

    // (2) `off` にしたら 1 本も繋がない (試験のオリジンの accept 回数 0)
    canary::configure("off", PERIOD);
    std::thread::sleep(TWO_PERIODS);
    probed.store(0, Ordering::SeqCst);
    std::thread::sleep(TWO_PERIODS);
    assert_eq!(
        probed.load(Ordering::SeqCst),
        0,
        "off なのに繋いだ ({})",
        canary_obj(&status_json(proxy_port))
    );
    assert!(canary_obj(&status_json(proxy_port)).contains("\"mode\":\"off\""));

    // (3) 失敗 (閉じたポート) は `/errors` に `kind: "canary"` で 1 件
    let dead_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dead = format!("127.0.0.1:{}", dead_port);
    canary::configure(&dead, PERIOD);
    wait_until(
        || endpoint_json(proxy_port, "/errors").contains("\"kind\":\"canary\""),
        "canary の失敗が /errors に出る",
    );
    let errors = endpoint_json(proxy_port, "/errors");
    assert!(
        errors.contains(&format!("\"target\":\"{}\"", dead)),
        "{}",
        errors
    );
    assert!(errors.contains("\"cause\":\"refused\""), "{}", errors);
    let failing = canary_obj(&status_json(proxy_port));
    assert!(failing.contains("\"error\":\""), "{}", failing);
    assert!(status_number(&failing, "failures") >= 1, "{}", failing);
    // 利用者に返したエラーの集計には混ざらない (この試験では利用者のエラーは 0 件)
    let prom = endpoint_json(proxy_port, "/metrics");
    assert!(
        prom.contains("sorahost_errors_total{cause=\"refused\"} 0"),
        "canary の失敗が集計に混ざった: {}",
        prom.lines()
            .filter(|l| l.starts_with("sorahost_errors_total"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // (4) `auto` は上位ホスト (CONNECT のもの) を選ぶ
    let tunnelled = Arc::new(AtomicUsize::new(0));
    let (origin_port, _o) = start_counting_origin(Arc::clone(&tunnelled), "");
    let origin = format!("127.0.0.1:{}", origin_port);
    {
        let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        use std::io::Write;
        stream
            .write_all(
                format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", origin, origin).as_bytes(),
            )
            .unwrap();
        assert!(read_connect_response(&mut stream).starts_with("HTTP/1.1 200"));
    }
    wait_until(
        || status_json(proxy_port).contains(&format!("connect://{}", origin)),
        "CONNECT の宛先がホスト別統計に載る",
    );
    tunnelled.store(0, Ordering::SeqCst);
    canary::configure("auto", PERIOD);
    wait_until(
        || canary_obj(&status_json(proxy_port)).contains(&format!("\"host\":\"{}\"", origin)),
        "auto が上位ホストを選ぶ",
    );
    wait_until(
        || tunnelled.load(Ordering::SeqCst) >= 1,
        "auto で選んだ宛先に繋ぐ",
    );
    let auto = canary_obj(&status_json(proxy_port));
    assert!(auto.contains("\"mode\":\"auto\""), "{}", auto);
    assert!(auto.contains("\"error\":null"), "{}", auto);

    // 後片付け (このプロセスの canary スレッドを黙らせる)
    canary::configure("off", PERIOD);
}

/// `/history` の `"canary":{…}` を切り出す (`samples` の入れ子を数えて閉じる)。
fn canary_obj_series(history: &str) -> String {
    let at = history
        .find("\"canary\":{")
        .unwrap_or_else(|| panic!("/history に canary が無い: {}", history));
    let rest = &history[at + "\"canary\":".len()..];
    let mut depth = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return rest[..=i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("canary の括弧が閉じていない: {}", rest);
}

/// 落ちても子プロセスを残さない番人 (`reload_test.rs` と同じ形)。
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// テスト用の `.env` を書く (変えるのは `PROXY_CANARY` だけ)。
fn write_env(dir: &std::path::Path, canary: &str) {
    std::fs::write(
        dir.join(".env"),
        format!(
            "SERVER_PORT=0\n\
             PROXY_BIND=127.0.0.1\n\
             PROXY_LOG_LEVEL=info\n\
             PROXY_CANARY={}\n\
             PROXY_CANARY_SECS=1\n",
            canary
        ),
    )
    .unwrap();
}

/// **実バイナリ**で `PROXY_CANARY` / `PROXY_CANARY_SECS` が効き、`.env` の書き換えで
/// 止められること (環境変数 → `Config` → `canary` → 履歴スレッド、の配線を丸ごと見る)。
#[test]
fn test_integration_the_env_file_turns_the_canary_on_and_off() {
    let probed = Arc::new(AtomicUsize::new(0));
    let (target_port, _t) = start_counting_origin(Arc::clone(&probed), "");
    let target = format!("127.0.0.1:{}", target_port);

    let dir = std::env::temp_dir().join(format!("rhp-t1410-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_env(&dir, &target);

    let mut child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
            .env("HOME", &dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("could not start the proxy binary"),
    );
    let stdout = child.0.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut port = None;
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(20)) {
        if let Some(rest) = line.split_once("listening on ") {
            port = rest
                .1
                .split_whitespace()
                .next()
                .and_then(|a| a.rsplit(':').next())
                .and_then(|p| p.parse::<u16>().ok());
            break;
        }
    }
    let port = port.expect("the proxy did not log its listening port");

    // 2 周期ぶん繋いだら、`/status` と `/history` に値が出ている
    wait_until(
        || probed.load(Ordering::SeqCst) >= 2,
        "`.env` の canary が 2 周期ぶん繋ぐ",
    );
    let canary = canary_obj(&status_json(port));
    assert!(canary.contains("\"mode\":\"hosts\""), "{}", canary);
    assert!(canary.contains("\"secs\":1"), "{}", canary);
    assert!(
        canary.contains(&format!("\"host\":\"{}\"", target)),
        "{}",
        canary
    );
    assert!(status_number(&canary, "at") > 1_700_000_000, "{}", canary);
    let series = canary_obj_series(&endpoint_json(port, "/history?res=5"));
    assert!(series.contains(&format!("\"{}\"]", target)), "{}", series);

    // `.env` を書き換えたら止まる (即時反映)
    write_env(&dir, "off");
    wait_until(
        || canary_obj(&status_json(port)).contains("\"mode\":\"off\""),
        "`.env` の書き換えで canary が止まる",
    );
    assert!(
        status_json(port).contains("PROXY_CANARY"),
        "再読込で当てたキーとして出る"
    );
    probed.store(0, Ordering::SeqCst);
    std::thread::sleep(TWO_PERIODS);
    assert_eq!(probed.load(Ordering::SeqCst), 0, "off なのに繋いだ");

    drop(child);
    let _ = std::fs::remove_dir_all(&dir);
}
