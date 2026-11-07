//! `PROXY_LOG_LEVEL` が無い・読めないときは、起動時と再読込で同じ既定 (`--lite` なら `warn`、
//! ほかは `info`) に倒し、知らせの文面も実際の行き先と使える値に合わせる (T35.5)。
//!
//! 0.35 までは 3 か所がずれていた: 再読込で `.env` から消すと lite でも `info` に戻る、
//! 読めない値の知らせが lite でも `falling back to 'info'` と言う (実際は `warn`)、
//! `--check` の `expected` に `trace` が無い。実バイナリを起こして確かめる。
#![cfg(target_os = "linux")]

mod common;
use common::*;

/// lite で `PROXY_LOG_LEVEL` を `.env` から消して再読込すると、起動時の既定 `warn` に戻る。
#[test]
fn test_integration_lite_falls_back_to_warn_when_the_level_is_removed_on_reload() {
    let dir = scratch_dir("rhp-t355-reload");
    let base = "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_PROFILE=lite\n";
    std::fs::write(dir.join(".env"), format!("{}PROXY_LOG_LEVEL=debug\n", base)).unwrap();
    let proxy = ProxyProcess::start(&dir);
    let status = endpoint_json(proxy.port, "/status");
    assert_eq!(json::string(&status, "log_level"), "DEBUG", "{}", status);

    std::fs::write(dir.join(".env"), base).unwrap();
    // 再読込の 1 行は info なので、`warn` に下がったあとは出ない。`/status` の回数で待つ
    wait_until(
        || endpoint_json(proxy.port, "/status").contains("\"reloads\":1,"),
        "the .env reload",
    );
    let status = endpoint_json(proxy.port, "/status");
    assert_eq!(json::string(&status, "log_level"), "WARN", "{}", status);

    drop(proxy);
    let _ = std::fs::remove_dir_all(&dir);
}

/// lite で読めない値を書くと、知らせは実際の行き先 `warn` を言う。
///
/// `warn` では待ち受けの行 (info) が出ないので [`ProxyProcess`] は使えない。知らせは
/// 標準出力の 1 行目に出るので、それだけ読んで止める。
#[test]
fn test_integration_lite_says_it_falls_back_to_warn_for_an_unknown_level() {
    use std::io::BufRead;
    let dir = scratch_dir("rhp-t355-unknown");
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_PROFILE=lite\nPROXY_LOG_LEVEL=loud\n",
    )
    .unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
        .env("HOME", &dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut first = String::new();
    let read = std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut first);
    let _ = child.kill();
    let _ = child.wait();
    read.unwrap();
    assert_eq!(
        first.trim_end(),
        "Unknown PROXY_LOG_LEVEL 'loud', falling back to 'warn'"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--check` の読めない値の行は、使える値に `trace` も挙げる。
#[test]
fn test_integration_check_lists_trace_among_the_log_levels() {
    let dir = scratch_dir("rhp-t355-check");
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=loud\n",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
        .arg("--check")
        .env("HOME", &dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|l| l.contains("PROXY_LOG_LEVEL") && l.contains("expected"))
        .unwrap_or_else(|| panic!("{}", stdout));
    assert!(
        line.ends_with("(expected error, warn, info, debug or trace)"),
        "{}",
        line
    );

    let _ = std::fs::remove_dir_all(&dir);
}
