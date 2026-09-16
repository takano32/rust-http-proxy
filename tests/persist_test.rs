//! 状態ファイル (`.rust-http-proxy.rrd`) の版上げの結合テスト (T12.4 (1))。
//!
//! **版 1 のファイルが置いてある状態で起動しても落ちない**こと (読み捨てて作り直す) と、
//! 大きさが新しい固定値になり書込エラーが出ないことを、実バイナリを起こして確かめる。

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::status_json;

/// `"key":` に続く数を取る (テスト用の雑な取り出し。キーは前後の `"` を含めて渡す)。
fn status_number(json: &str, key: &str) -> u64 {
    let pat = format!("{}:", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("no {} in {}", key, json))
        + pat.len();
    json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} is not a number", key))
}

/// 版 2 の固定の大きさ (`proxy_rrd::rrd::FILE_SIZE`)。
const FILE_SIZE: u64 = 4 * 1024 * 1024;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 版 1 のファイルを模して置く (識別子 `SHPRRD01` + 約 1 MiB の中身)。
fn write_version_one_rrd(path: &std::path::Path) {
    let mut buf = vec![0u8; 1_064_960];
    buf[..8].copy_from_slice(b"SHPRRD01");
    // 中身は「前の版で書かれたレコード」のつもりの適当なバイト列 (CRC は合わない)
    for (i, b) in buf[4096..].iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    std::fs::write(path, &buf).unwrap();
}

#[test]
fn test_integration_an_old_state_file_is_replaced_by_the_new_fixed_size_one() {
    let dir = std::env::temp_dir().join(format!("rhp-t124-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rrd = dir.join(".rust-http-proxy.rrd");
    write_version_one_rrd(&rrd);
    assert_eq!(std::fs::metadata(&rrd).unwrap().len(), 1_064_960);

    // 既定プロファイル (lite だと統計を永続化しない)。ログは info で起動行を読む
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\nPROXY_CACHE_RESERVE=off\n",
    )
    .unwrap();

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
    let mut state_line = String::new();
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(30)) {
        if line.contains("state file ") && line.contains("created") {
            state_line = line.clone();
        }
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
    assert!(
        state_line.contains("created"),
        "版 1 のファイルは読み捨てて作り直すはず: {:?}",
        state_line
    );

    let status = status_json(port);
    assert!(
        status.contains(&format!("\"bytes\":{}", FILE_SIZE)),
        "state_file.bytes が新しい固定の大きさでない: {}",
        status
    );
    assert!(
        status.contains("\"write_errors\":0"),
        "書込エラーが出ている: {}",
        status
    );
    assert_eq!(
        std::fs::metadata(&rrd).unwrap().len(),
        FILE_SIZE,
        "ファイルの実サイズも固定"
    );

    // ---- `/status` の窓とプロセスの数え物 (T12.4 (4)) ----
    assert!(
        status.contains("\"since_start_secs\":") && status.contains("\"restored_since\":"),
        "窓の目印が無い: {}",
        status
    );
    // まだ 1 件も要求を通していないので、通算の始まりは 0 (= `hosts[]` が空)
    assert!(status.contains("\"restored_since\":0"), "{}", status);
    let pid = child.0.id();
    let fds = status_number(&status, "\"fds\"");
    let max_fds = status_number(&status, "\"max_fds\"");
    let threads = status_number(&status, "\"threads\"");
    let ls = std::process::Command::new("ls")
        .arg(format!("/proc/{}/fd", pid))
        .output()
        .expect("ls");
    let counted = String::from_utf8_lossy(&ls.stdout).lines().count() as u64;
    assert!(
        fds.abs_diff(counted) <= 2,
        "/status の fds {} と ls /proc/{}/fd {} が ±2 で一致しない",
        fds,
        pid,
        counted
    );
    assert!(max_fds >= fds, "max_fds {} < fds {}", max_fds, fds);
    assert!(threads >= 2, "スレッド数 {}", threads);
    assert!(
        status.len() <= 64 * 1024,
        "/status が {} B (64 KiB 超)",
        status.len()
    );

    drop(child);
    let _ = std::fs::remove_dir_all(&dir);
}

/// T14.5 より前に書かれた `.rrd` (版 2 のまま、`HostStats` が 49 項目) を置いて起動しても
/// 落ちず、新しい 4 欄 (RTT と再送) が 0 で読み戻ること。
///
/// **版は上げていない** ので、古いファイルは読み捨てられずにそのまま復元される。
/// `Dec` は足りなければ 0 を返すので、末尾に足した欄だけが 0 になるのが期待の動き。
#[test]
fn test_integration_a_pre_rtt_state_file_restores_with_zero_rtt() {
    use rust_http_proxy::rrd::{Enc, Rrd};

    let dir = std::env::temp_dir().join(format!("rhp-t145-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".rust-http-proxy.rrd");

    // 版 2 のファイルを作り、**T14.5 より前の形** (名前 128 B + 49 項目) で 1 行ずつ書く
    {
        let (rrd, created) = Rrd::open(&path).unwrap();
        assert!(created);
        for (region, name, requests) in [
            (rrd.layout.hosts, "connect://mtalk.google.com:5228", 920u64),
            (rrd.layout.clients, "198.51.100.7", 463),
        ] {
            let mut e = Enc::new();
            e.str(name, 128)
                .u64(1_700_000_000) // last_seen
                .u64(requests)
                .u64(0) // hits
                .u64(0) // misses
                .u64(requests) // bypass
                .u64(0) // errors
                .u64(0) // blocked
                .u64(1024) // bytes
                .u64(requests) // timed
                .u64(requests * 30) // duration_ms_sum
                .u64(31); // duration_ms_max
            for _ in 0..25 {
                e.u64(0); // buckets (24 段 + 上限なし)
            }
            e.u64(0).u64(0).u64(requests * 30).u64(requests).u64(0); // dns/connect/族
            for _ in 0..8 {
                e.u64(0); // errors_by_cause
            }
            assert_eq!(e.0.len(), 128 + 49 * 8, "T14.5 より前の 1 行は 520 B");
            rrd.write(region, 0, &e.0).unwrap();
        }
    }

    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\nPROXY_CACHE_RESERVE=off\n",
    )
    .unwrap();
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
    let mut state_line = String::new();
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(30)) {
        if line.contains("state file ") {
            state_line = line.clone();
        }
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
    assert!(
        !state_line.contains("created"),
        "版は上げていないので作り直さないはず: {:?}",
        state_line
    );
    assert!(
        state_line.contains("1 hosts, 1 clients restored"),
        "古いファイルの行が読み戻っていない: {:?}",
        state_line
    );

    let status = status_json(port);
    assert!(
        status.contains("\"host\":\"connect://mtalk.google.com:5228\""),
        "{}",
        status
    );
    assert!(status.contains("\"requests\":920"), "{}", status);
    assert!(status.contains("\"client\":\"198.51.100.7\""), "{}", status);
    // 末尾に足した 4 欄は 0 で読み戻る = RTT は「標本なし」
    assert_eq!(
        status.matches("\"rtt_ms\":null,\"retrans\":0").count(),
        2,
        "古い行の RTT が null で出ていない: {}",
        status
    );
    assert!(status.contains("\"write_errors\":0"), "{}", status);

    drop(child);
    let _ = std::fs::remove_dir_all(&dir);
}
