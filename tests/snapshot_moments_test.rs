//! 起動と停止の雪像 (`snapshots/<時刻>-start.json` と `-shutdown.json`) の結合テスト (T.28.20)。
//!
//! 締めの判定の「前」(再デプロイの直前) と「0 時間」(直後) は、撮った機械にしか残らなかった
//! (T.25.99 は「前」が無く日次で代用、T.27.99 はコンテナに置いた)。プロキシ自身が
//! **停止シグナルで状態ファイルを書く前に 1 枚** (`-shutdown`)、**起動から 60 秒 (`memory.baseline`
//! を書く所) で 1 枚** (`-start`) を日次の雪像の隣に残し、`/snapshots` の `moments` に並べて
//! `/snapshots/<名前>` で返す。
//!
//! ここで見るのは:
//!
//! 1. 起動の 1 枚は待ち時間 (試験では 1 秒) のあとに**1 回だけ**書かれ、一覧と取り寄せの口に出る
//!    (60 秒待たないように `persist::set_baseline_after_secs` を差し替える。このバイナリだけ)
//! 2. 実バイナリに SIGTERM を送ると停止の 1 枚が書かれ、起こし直すと一覧に出てそのまま読める。
//!    停止にかかった時間も出す (後始末の上限 `signal::GRACE` = 3 秒の中)
//!
//! 「60 秒 → SIGTERM で 2 枚」を実バイナリで通したもの (と `snapshot-diff.py --criteria phase25`
//! で読めること) は手で回した (コミットの本文)。

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{ProxyProcess, endpoint_json, proxy_config, start_test_proxy_with_history};
use rust_http_proxy::snapshots;

fn home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rhp-t2820-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 置き場所にある `*-<kind>.json` の名前 (拡張子なし、古い順)。
fn moments(dir: &Path, kind: &str) -> Vec<String> {
    let suffix = format!("-{}.json", kind);
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(&suffix)
                        .map(|stamp| format!("{}-{}", stamp, kind))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// `YYYY-MM-DDTHHMMSSZ-<kind>` の形か。
fn stamped(name: &str, kind: &str) -> bool {
    let Some(stamp) = name.strip_suffix(&format!("-{}", kind)) else {
        return false;
    };
    let b = stamp.as_bytes();
    b.len() == 18
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            17 => *c == b'Z',
            _ => c.is_ascii_digit(),
        })
}

/// 起動から待ち時間がたつと `-start` を 1 枚だけ書き、`/snapshots` と `/snapshots/<名前>` に出る。
#[test]
fn test_integration_the_start_snapshot_is_written_once_after_the_wait() {
    rust_http_proxy::persist::set_baseline_after_secs(1);
    let dir = home("start");
    let snaps = dir.join("snapshots");
    snapshots::configure(Some(snaps.clone()), snapshots::DEFAULT_DAYS);
    let started = Instant::now();
    let (port, _metrics) = start_test_proxy_with_history(proxy_config(), Duration::from_millis(20));
    // 中身が空にならないように少しだけ動かす
    for _ in 0..3 {
        let _ = endpoint_json(port, "/healthz");
    }
    common::wait_until(
        || !moments(&snaps, "start").is_empty(),
        "起動の雪像が書かれる",
    );
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "待ち時間の前に書いている ({:?})",
        started.elapsed()
    );
    let names = moments(&snaps, "start");
    assert_eq!(names.len(), 1, "{:?}", names);
    let name = &names[0];
    assert!(stamped(name, "start"), "{}", name);
    // 何周期たっても増えない (1 回だけ)
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(moments(&snaps, "start"), names);
    // 日次の 1 枚とは別物 (日が変わっていないので日次は無い)
    assert!(moments(&snaps, "shutdown").is_empty());

    let body = std::fs::read_to_string(snaps.join(format!("{}.json", name))).unwrap();
    for key in ["\"taken_at\":", "\"parts\":[", "\"status\":", "\"recent\":"] {
        assert!(body.contains(key), "{} が無い", key);
    }
    let list = endpoint_json(port, "/snapshots");
    assert!(
        list.contains(&format!("{{\"name\":\"{}\",\"kind\":\"start\"", name)),
        "{}",
        list
    );
    assert!(
        list.contains("\"files\":[]"),
        "日次の一覧は空のまま: {}",
        list
    );
    assert_eq!(endpoint_json(port, &format!("/snapshots/{}", name)), body);

    snapshots::configure(None, snapshots::DEFAULT_DAYS);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 実バイナリに SIGTERM を送ると `-shutdown` を 1 枚書き、起こし直すと一覧に出てそのまま読める。
#[test]
fn test_integration_sigterm_leaves_a_shutdown_snapshot() {
    let dir = home("shutdown");
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\nPROXY_CACHE_RESERVE=off\n\
         PROXY_CANARY=off\n",
    )
    .unwrap();
    let snaps = dir.join(".rust-http-proxy").join("snapshots");

    let mut proxy = ProxyProcess::start(&dir);
    for _ in 0..20 {
        let _ = endpoint_json(proxy.port, "/healthz");
    }
    let t = Instant::now();
    proxy.stop();
    let took = t.elapsed();
    let lines = proxy.drain_log(Duration::from_millis(200));
    println!(
        "T.28.20 SIGTERM から終わるまで {:.0} ms (後始末の上限 {:?})",
        took.as_secs_f64() * 1000.0,
        rust_http_proxy::signal::GRACE
    );
    let names = moments(&snaps, "shutdown");
    assert_eq!(names.len(), 1, "{:?} / {:#?}", names, lines);
    let name = names[0].clone();
    assert!(stamped(&name, "shutdown"), "{}", name);
    let logged = lines
        .iter()
        .find(|l| l.contains("shutdown snapshot"))
        .unwrap_or_else(|| panic!("停止の雪像の行が無い: {:#?}", lines));
    println!("T.28.20 {}", logged);
    // 後始末が終わったことの行 (状態ファイルも書けている = 雪像が後始末の時間を食い潰していない)
    assert!(
        lines.iter().any(|l| l.contains("statistics saved to")),
        "{:#?}",
        lines
    );
    assert!(took < rust_http_proxy::signal::GRACE, "{:?}", took);

    let body = std::fs::read_to_string(snaps.join(format!("{}.json", name))).unwrap();
    // 停止の 1 枚は `recent` を切らずに全部入れる (部の `truncated` が立たない)
    // (部はどれもその口の出力そのもので `{"schema":1,` で始まる。`/status` の中の `recent` と分ける)
    let recent = body
        .split(",\"recent\":{\"schema\":")
        .nth(1)
        .expect("recent の部が無い");
    assert!(recent.contains("\"truncated\":false"), "{}", &recent[..200]);

    // 起こし直すと一覧に出て、そのまま読める (`PROXY_ENDPOINTS_READONLY` でなくても同じ口)
    let proxy = ProxyProcess::start(&dir);
    let list = endpoint_json(proxy.port, "/snapshots");
    assert!(
        list.contains(&format!("{{\"name\":\"{}\",\"kind\":\"shutdown\"", name)),
        "{}",
        list
    );
    assert_eq!(
        endpoint_json(proxy.port, &format!("/snapshots/{}", name)),
        body
    );
    // 読めない名前は 404 (置き場所の外は見ない)
    for bad in [
        "2026-10-25T203134Z-reboot",
        "2026-10-25T2031Z-shutdown",
        "..%2F..%2F.env",
    ] {
        let out = common::raw_get(
            proxy.port,
            &format!(
                "GET /snapshots/{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                bad, proxy.port
            ),
        );
        assert!(out.starts_with("HTTP/1.1 404 "), "{} -> {}", bad, out);
    }
    drop(proxy);
    let _ = std::fs::remove_dir_all(&dir);
}
