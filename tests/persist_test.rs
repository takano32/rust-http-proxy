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

    drop(child);
    let _ = std::fs::remove_dir_all(&dir);
}
