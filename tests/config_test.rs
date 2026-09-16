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

    // `/config` にも同じものが出る (T14.15 は `/status` と `/config` の 2 か所に出す)。
    // **`/healthz` はもう `/status` の写しではない** ので、そちらでは見ない (T14.12)
    assert!(endpoint_json(port, "/config").contains("\"capabilities\":{"));
    let health = endpoint_json(port, "/healthz");
    assert!(health.contains("\"checks\":{"), "{}", health);

    drop(_child);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `"KEY":{"value":V,"source":"S"}` から (V, S) を取る。
fn setting(json: &str, key: &str) -> (String, String) {
    let pat = format!("\"{}\":{{\"value\":", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, json))
        + pat.len();
    let rest = &json[at..];
    let end = rest.find(",\"source\":").expect("source");
    let value = rest[..end].to_string();
    let src = rest[end + ",\"source\":\"".len()..]
        .split('"')
        .next()
        .expect("source value")
        .to_string();
    (value, src)
}

/// `/config` が**効いている値とその出どころ**を出し、再読込で両方が追随すること (T14.15)。
#[test]
fn test_integration_config_shows_effective_values_and_their_source() {
    let env_file = |ttl: Option<u32>| {
        let mut text = String::from("SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\n");
        if let Some(ttl) = ttl {
            text.push_str(&format!("PROXY_DNS_TTL_SECS={}\n", ttl));
        }
        text
    };
    // `.env` に 1 つ、実際の環境変数に 1 つ、残りは既定のまま
    let (_child, port, dir) = start_proxy(
        "config",
        &env_file(Some(30)),
        &[("PROXY_KEEPALIVE_SECS", "7")],
    );

    let json = endpoint_json(port, "/config");
    assert_eq!(
        setting(&json, "PROXY_DNS_TTL_SECS"),
        ("30".to_string(), "env_file".to_string()),
        "`.env` に書いた行: {}",
        json
    );
    assert_eq!(
        setting(&json, "PROXY_KEEPALIVE_SECS"),
        ("7".to_string(), "env".to_string()),
        "環境変数で渡した行: {}",
        json
    );
    assert_eq!(
        setting(&json, "PROXY_TIMEOUT_SECS"),
        ("30".to_string(), "default".to_string()),
        "書いていない行: {}",
        json
    );
    // `.env` 自身の場所と、この環境で何が読めるかも同じ 1 枚に出る
    assert!(
        json.contains(&dir.join(".env").display().to_string()),
        "{}",
        json
    );
    assert!(json.contains("\"capabilities\":{"), "{}", json);
    assert!(json.contains("\"truncated\":false"), "{}", json);
    // 秘密は無い (証明書はパスだけ、鍵の類の設定はそもそも無い)
    assert_eq!(
        setting(&json, "PROXY_TLS_CA_FILE"),
        ("null".to_string(), "default".to_string())
    );

    // `.env` を書き換えると値も出どころも追随する
    std::fs::write(dir.join(".env"), env_file(Some(45))).unwrap();
    wait_until(
        || setting(&endpoint_json(port, "/config"), "PROXY_DNS_TTL_SECS").0 == "45",
        ".env の書き換えが /config に出る",
    );
    assert_eq!(
        setting(&endpoint_json(port, "/config"), "PROXY_DNS_TTL_SECS"),
        ("45".to_string(), "env_file".to_string())
    );

    // 消すと既定に戻り、出どころも `default` に戻る
    std::fs::write(dir.join(".env"), env_file(None)).unwrap();
    wait_until(
        || setting(&endpoint_json(port, "/config"), "PROXY_DNS_TTL_SECS").0 == "60",
        ".env から消すと既定に戻る",
    );
    assert_eq!(
        setting(&endpoint_json(port, "/config"), "PROXY_DNS_TTL_SECS"),
        ("60".to_string(), "default".to_string())
    );
    // 環境変数の行は `.env` の書き換えでは動かない
    assert_eq!(
        setting(&endpoint_json(port, "/config"), "PROXY_KEEPALIVE_SECS"),
        ("7".to_string(), "env".to_string())
    );

    // `/` の案内にも出ている (ブラウザで開いた人が辿れること)
    let listing = endpoint_json(port, "/");
    assert!(listing.contains("/config"), "{}", listing);

    drop(_child);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `rust-http-proxy --check` が**起動せずに**環境と効く設定を印字して終わること (T14.15)。
///
/// 終了コードは `capabilities` の 6 項目 (名前解決を除く) が全部読めたら 0、
/// 1 つでも読めなければ 1。どちらの機械でも落ちないよう、印字と終了コードの
/// **辻褄が合っていること**を見る (この機械では 0)。
#[test]
fn test_integration_check_prints_capabilities_and_settings() {
    let dir = std::env::temp_dir().join(format!("rhp-t1415-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".env"), "PROXY_DNS_TTL_SECS=30\n").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
        .arg("--check")
        // 引数も効く (「この設定で起動したらどうなるか」が見られること)
        .args(["-p", "3128"])
        .env("HOME", &dir)
        .env("PROXY_KEEPALIVE_SECS", "7")
        .output()
        .expect("--check が動かない");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let code = out.status.code().expect("exit code");

    // 7 項目が印字される
    for key in [
        "proc_syscall",
        "tcp_info",
        "cgroup_cpu",
        "cgroup_pressure",
        "ipv6_route",
        "home_writable",
        "resolver_ms",
    ] {
        assert!(text.contains(key), "{} が印字されない:\n{}", key, text);
    }
    // 印字と終了コードの辻褄 (読めないものがあれば 1、無ければ 0)
    if text.contains("check: ok") {
        assert_eq!(code, 0, "全部読めるなら 0:\n{}", text);
        assert!(!text.contains("[NO]"), "{}", text);
    } else {
        assert_eq!(code, 1, "読めないものがあれば 1:\n{}", text);
        assert!(text.contains("[NO]"), "{}", text);
    }
    #[cfg(target_os = "linux")]
    assert_eq!(code, 0, "この機械では全部読める:\n{}", text);

    // 効いている設定と出どころ (`.env` / 環境変数 / 引数 / 既定) が並ぶ
    assert!(
        text.contains("env_file  PROXY_DNS_TTL_SECS") && text.contains(" 30"),
        "{}",
        text
    );
    assert!(text.contains("env       PROXY_KEEPALIVE_SECS"), "{}", text);
    assert!(text.contains("cli       SERVER_PORT"), "{}", text);
    assert!(text.contains("default   PROXY_TIMEOUT_SECS"), "{}", text);
    assert!(
        text.contains(&dir.join(".env").display().to_string()),
        "{}",
        text
    );
    // 起動していない (待ち受けの行が無い)
    assert!(!text.contains("listening on"), "{}", text);

    let _ = std::fs::remove_dir_all(&dir);
}
