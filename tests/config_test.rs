//! `capabilities` (この環境で何が読めるか) と `/config` (効いている設定とその出どころ) と
//! `--check` の結合テスト (T14.15)。
//!
//! どれも**実バイナリを起こして**見る。`capabilities` を測るのは `.env` の監視スレッドで、
//! 出どころ (`default` / `env` / `env_file`) は `$HOME/.env` と実際の環境変数の両方が要るので、
//! プロセスの中に閉じたテストでは配線が丸ごとは見えない。

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::{endpoint_json, status_json, wait_until};

/// 落ちても子プロセスを残さない番人。
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `$HOME` を一時ディレクトリにしてプロキシを起こす。`env_file` はそこに置く `.env` の中身、
/// `vars` は**実際の環境変数**として渡すもの (出どころの `env` と `env_file` を作り分ける)。
/// 返すのは (子プロセス, 待ち受けポート, `$HOME`)。
fn start_proxy(
    tag: &str,
    env_file: &str,
    vars: &[(&str, &str)],
) -> (KillOnDrop, u16, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("rhp-t1415-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".env"), env_file).unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"));
    cmd.env("HOME", &dir);
    for (k, v) in vars {
        cmd.env(k, v);
    }
    let mut child = KillOnDrop(
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("could not start the proxy binary"),
    );
    // 起動ログは標準出力へ出る。読み続けないとパイプが詰まるので専用スレッドで引き取る
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
        if let Some((_, rest)) = line.split_once("listening on ") {
            port = rest
                .split_whitespace()
                .next()
                .and_then(|a| a.rsplit(':').next())
                .and_then(|p| p.parse::<u16>().ok());
            break;
        }
    }
    let port = port.expect("the proxy did not log its listening port");
    (child, port, dir)
}

/// `"key":` の後ろを 1 語だけ取る (数値・`true` / `false` ・`null`)。
fn value_of(json: &str, key: &str) -> String {
    let pat = format!("\"{}\":", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, json))
        + pat.len();
    json[at..]
        .chars()
        .take_while(|c| !matches!(c, ',' | '}' | ']'))
        .collect::<String>()
        .trim()
        .to_string()
}

/// `capabilities` の 7 項目が `/status` に出ること (T14.15)。
///
/// 測るのは `.env` の監視スレッドなので、起動直後の一瞬は `null` のことがある
/// (名前解決の測定に最大 2 秒かかる)。出そろうまで待って形を見る。
#[test]
fn test_integration_capabilities_appear_in_status() {
    let (_child, port, dir) = start_proxy(
        "caps",
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\n",
        &[],
    );
    wait_until(
        || !status_json(port).contains("\"capabilities\":null"),
        "capabilities が測り終わる",
    );
    let status = status_json(port);
    for key in [
        "proc_syscall",
        "tcp_info",
        "cgroup_cpu",
        "cgroup_pressure",
        "ipv6_route",
        "resolver_ms",
        "home_writable",
    ] {
        assert!(
            status.contains(&format!("\"{}\":", key)),
            "{} が /status に無い: {}",
            key,
            status
        );
    }
    // 真偽で答えるものは真偽で出る (「読めなかった」と「無かった」を分けるため)
    for key in ["proc_syscall", "tcp_info", "home_writable"] {
        let v = value_of(&status, key);
        assert!(v == "true" || v == "false", "{} = {}", key, v);
    }
    // この機械 (Linux、cgroup v2、`$HOME` あり) では全部読める
    #[cfg(target_os = "linux")]
    {
        assert_eq!(value_of(&status, "proc_syscall"), "true", "{}", status);
        assert_eq!(value_of(&status, "tcp_info"), "true", "{}", status);
        assert_eq!(value_of(&status, "home_writable"), "true", "{}", status);
    }
    // 名前解決は数値か null (外に出られない環境では null)
    let resolver = value_of(&status, "resolver_ms");
    assert!(
        resolver == "null" || resolver.parse::<u64>().is_ok(),
        "resolver_ms = {}",
        resolver
    );
    // 測った時刻が入っている (いつの判定かが読めること)
    assert!(
        value_of(&status, "checked_at").parse::<u64>().unwrap_or(0) > 0,
        "{}",
        status
    );
    // 測るのに使った一時ファイルは残さない
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(".rust-http-proxy.wtest"))
        .collect();
    assert!(leftovers.is_empty(), "書き込みの検査の跡: {:?}", leftovers);

    // `/healthz` は `/status` と同じ JSON なのでこちらにも出る
    assert!(endpoint_json(port, "/healthz").contains("\"capabilities\":{"));

    drop(_child);
    let _ = std::fs::remove_dir_all(&dir);
}
