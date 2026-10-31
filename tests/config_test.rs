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
    let (child, port, dir, _) = start_proxy_logged(tag, env_file, vars);
    (child, port, dir)
}

/// [`start_proxy`] と同じだが、待ち受けの行までに出た起動ログの行も返す (T.28.6)。
fn start_proxy_logged(
    tag: &str,
    env_file: &str,
    vars: &[(&str, &str)],
) -> (KillOnDrop, u16, std::path::PathBuf, Vec<String>) {
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
    let mut log = Vec::new();
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(20)) {
        if let Some((_, rest)) = line.split_once("listening on ") {
            port = rest
                .split_whitespace()
                .next()
                .and_then(|a| a.rsplit(':').next())
                .and_then(|p| p.parse::<u16>().ok());
            break;
        }
        log.push(line);
    }
    let port = port.expect("the proxy did not log its listening port");
    (child, port, dir, log)
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

/// `--check` の最後の行から「読めなかったものの名前」を取り出す。
///
/// `check: ok (…)` なら空、`check: 1 of 7 not readable (proc_schedstat) — …` なら
/// `["proc_schedstat"]`。どちらの行にも括弧があるので `ok` を先に見る。
fn check_missing(text: &str) -> Vec<String> {
    let line = text
        .lines()
        .find(|l| l.starts_with("check: "))
        .unwrap_or_else(|| panic!("check: の行が無い:\n{}", text));
    if line.starts_with("check: ok") {
        return Vec::new();
    }
    let inner = line
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .unwrap_or_else(|| panic!("読めなかったものの名前が無い: {}", line))
        .0;
    inner.split(',').map(|s| s.trim().to_string()).collect()
}

/// `capabilities` の 8 項目が `/status` に出ること (T14.15、T15.0 (5) で 1 つ増えた)。
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
        "proc_schedstat",
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

/// 転送中の最小の速さの 2 つの鍵が `/config` に出て、`.env` と環境変数から読めること (T.29.34)。
#[test]
fn test_integration_min_transfer_keys_are_read_and_shown() {
    let (_child, port, dir) = start_proxy(
        "mintransfer",
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_MIN_TRANSFER_WINDOW_SECS=10\n",
        &[("PROXY_MIN_TRANSFER_BPS", "0")],
    );
    let json = endpoint_json(port, "/config");
    assert_eq!(
        setting(&json, "PROXY_MIN_TRANSFER_BPS"),
        ("0".to_string(), "env".to_string()),
        "{}",
        json
    );
    assert_eq!(
        setting(&json, "PROXY_MIN_TRANSFER_WINDOW_SECS"),
        ("10".to_string(), "env_file".to_string()),
        "{}",
        json
    );
    // `/status` は切った本数を向きごとに出す (起動直後は全部 0)
    let status = endpoint_json(port, "/status");
    assert!(
        status
            .contains("\"slow_transfer_closed\":{\"response\":0,\"request_body\":0,\"tunnel\":0}"),
        "{}",
        status
    );
    let _ = std::fs::remove_dir_all(&dir);
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
    // CONNECT 専用の締め切りも書いていないので、既定の 3 秒が出どころ `default` で出る
    // (`PROXY_TIMEOUT_SECS` は 30 のまま。T15.6 (2)、既定を 5 秒にした T23.3、3 秒にした T.28.43)
    assert_eq!(
        setting(&json, "PROXY_CONNECT_TIMEOUT_SECS"),
        ("3".to_string(), "default".to_string()),
        "書いていない行 (既定は 3 秒): {}",
        json
    );
    // Happy Eyeballs の間隔も書いていないので、既定の 250 ms が出どころ `default` で出る (T17.6)
    assert_eq!(
        setting(&json, "PROXY_HE_STAGGER_MS"),
        ("250".to_string(), "default".to_string()),
        "書いていない行 (既定は 250 ms): {}",
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
/// 終了コードは `capabilities` の 7 項目 (名前解決を除く) が全部読めたら 0、
/// 1 つでも読めなければ 1。どちらの機械でも落ちないよう、印字と終了コードの
/// **辻褄が合っていること**を見る。
///
/// Linux では `proc_schedstat` **だけ**は読めなくてよい (`CONFIG_SCHEDSTATS` の無い
/// カーネルには `/proc/<tid>/schedstat` がファイルごと無い。T15.0 (5))。ほかが 1 つでも
/// 読めなければ、それはこの機械の設定がおかしいので落とす。
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

    // 8 項目が印字される
    for key in [
        "proc_syscall",
        "tcp_info",
        "cgroup_cpu",
        "cgroup_pressure",
        "ipv6_route",
        "home_writable",
        "proc_schedstat",
        "resolver_ms",
    ] {
        assert!(text.contains(key), "{} が印字されない:\n{}", key, text);
    }
    // 印字と終了コードの辻褄 (読めないものがあれば 1、無ければ 0)
    let missing = check_missing(&text);
    if missing.is_empty() {
        assert_eq!(code, 0, "全部読めるなら 0:\n{}", text);
        assert!(!text.contains("[NO]"), "{}", text);
    } else {
        assert_eq!(code, 1, "読めないものがあれば 1:\n{}", text);
        assert!(text.contains("[NO]"), "{}", text);
    }
    // この機械で読めなくてよいのは `proc_schedstat` (カーネルの設定しだい) と、**試験のプロセスが同じ機械で
    // 確かめても読めないもの** (cgroup の `cpu.stat` や IPv6 の既定経路が無いコンテナ。T.32.24)。印字が機械と食い違うのは落とす
    #[cfg(target_os = "linux")]
    {
        let here = rust_http_proxy::sysinfo::capabilities::probe();
        let may_miss = here.missing();
        assert!(
            missing
                .iter()
                .all(|m| m == "proc_schedstat" || may_miss.contains(&m.as_str())),
            "proc_schedstat とこの機械で読めないもの ({:?}) 以外も読めない ({:?}):\n{}",
            may_miss,
            missing,
            text
        );
    }

    // 効いている設定と出どころ (`.env` / 環境変数 / 引数 / 既定) が並ぶ
    assert!(
        text.contains("env_file  PROXY_DNS_TTL_SECS") && text.contains(" 30"),
        "{}",
        text
    );
    assert!(text.contains("env       PROXY_KEEPALIVE_SECS"), "{}", text);
    assert!(text.contains("cli       SERVER_PORT"), "{}", text);
    assert!(text.contains("default   PROXY_TIMEOUT_SECS"), "{}", text);
    // CONNECT 専用の締め切りも `--check` に出る (書いていないので既定の 3 秒。T15.6 (2)、T23.3、T.28.43)
    assert!(
        text.contains("default   PROXY_CONNECT_TIMEOUT_SECS"),
        "{}",
        text
    );
    // Happy Eyeballs の間隔も `--check` に出る (書いていないので既定の 250 ms。T17.6)
    assert!(text.contains("default   PROXY_HE_STAGGER_MS"), "{}", text);
    assert!(
        text.contains(&dir.join(".env").display().to_string()),
        "{}",
        text
    );
    // 起動していない (待ち受けの行が無い)
    assert!(!text.contains("listening on"), "{}", text);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `PROXY_HE_STAGGER_MS` (T17.6): 範囲の中なら書いた値が出どころつきで効き、範囲の外は
/// **warn を 1 行出して既定の 250 ms (出どころ `default`) に戻す**。`--check` で両方を見る。
#[test]
fn test_integration_he_stagger_is_read_once_and_falls_back_when_out_of_range() {
    let dir = std::env::temp_dir().join(format!("rhp-t176-stagger-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let check = |env_file: &str| {
        std::fs::write(dir.join(".env"), env_file).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
            .arg("--check")
            .env("HOME", &dir)
            .env("XDG_CACHE_HOME", &dir)
            .env_remove("PROXY_HE_STAGGER_MS")
            .output()
            .expect("--check が動かない");
        // ログがどちらへ出ても拾えるように両方を繋ぐ
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };
    let line = |text: &str| {
        text.lines()
            // 設定の行 (`  <出どころ> <名前> <値>`) だけを拾う (warn の行にも名前が入るため)
            .find(|l| l.split_whitespace().nth(1) == Some("PROXY_HE_STAGGER_MS"))
            .unwrap_or_else(|| panic!("PROXY_HE_STAGGER_MS の行が無い:\n{}", text))
            .to_string()
    };

    // 範囲の中: `.env` の値が出どころ `env_file` で効く
    let text = check("PROXY_HE_STAGGER_MS=100\n");
    let l = line(&text);
    let l = l.trim_start();
    assert!(
        l.starts_with("env_file  PROXY_HE_STAGGER_MS") && l.trim_end().ends_with(" 100"),
        "{}",
        text
    );
    assert!(
        !text.contains("PROXY_HE_STAGGER_MS=\""),
        "警告は出ない:\n{}",
        text
    );

    // 範囲の外 (5 ms と 5,000 ms): 既定の 250 ms・出どころ `default` に戻して 1 行知らせる
    for bad in ["5", "5000"] {
        let text = check(&format!("PROXY_HE_STAGGER_MS={}\n", bad));
        let l = line(&text);
        let l = l.trim_start();
        assert!(
            l.starts_with("default   PROXY_HE_STAGGER_MS") && l.trim_end().ends_with(" 250"),
            "{}: {}",
            bad,
            text
        );
        assert_eq!(
            text.matches(&format!("PROXY_HE_STAGGER_MS=\"{}\"", bad))
                .count(),
            1,
            "warn は 1 行: {}",
            text
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// `.env` の打ち間違い (知らない名前) と読めない値 (T.28.6): 起動ログに WARN を 1 行ずつ、
/// `/status` の `settings.invalid` / `settings.unknown` に名前が出る。再読込で直せば消え、
/// 書き損じれば出る。**正しい `.env` では WARN が 1 行も増えない**。
#[test]
fn test_integration_env_typos_and_unreadable_values_are_reported() {
    let base = "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\n";
    let bad = format!(
        "{}PROXY_TIMOUT_SECS=20\nPROXY_KEEPALIVE_SECS=abc\nPROXY_TIMEOUT_SECS=xyz\nPROXY_FOO_BAR=1\n",
        base
    );
    let (child, port, dir, log) = start_proxy_logged("t2806", &bad, &[]);
    let warns: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("WARN") && (l.contains("is not a") || l.contains("unknown setting")))
        .collect();
    assert_eq!(warns.len(), 4, "WARN は 4 行: {:#?}", log);
    assert!(
        warns
            .iter()
            .any(|l| l.contains("PROXY_TIMOUT_SECS")
                && l.contains("did you mean PROXY_TIMEOUT_SECS")),
        "{:#?}",
        warns
    );
    let settings = || {
        let json = status_json(port);
        let at = json.find("\"settings\":").expect("settings");
        json[at..].to_string()
    };
    let s = settings();
    for key in [
        "{\"key\":\"PROXY_KEEPALIVE_SECS\",\"value\":\"abc\"}",
        "{\"key\":\"PROXY_TIMEOUT_SECS\",\"value\":\"xyz\"}",
        "{\"key\":\"PROXY_TIMOUT_SECS\",\"suggest\":\"PROXY_TIMEOUT_SECS\"}",
        "{\"key\":\"PROXY_FOO_BAR\",\"suggest\":null}",
    ] {
        assert!(s.contains(key), "{} が無い: {}", key, s);
    }
    // 読めない値は既定のまま (出どころも `default`)
    let config = endpoint_json(port, "/config");
    assert_eq!(
        setting(&config, "PROXY_KEEPALIVE_SECS"),
        ("15".to_string(), "default".to_string())
    );

    // 直すと消える
    std::fs::write(
        dir.join(".env"),
        format!("{}PROXY_KEEPALIVE_SECS=7\n", base),
    )
    .unwrap();
    wait_until(
        || settings().contains("\"invalid\":[],\"unknown\":[]"),
        "直した .env で invalid / unknown が空になる",
    );
    // 再読込で書き損じると出る
    std::fs::write(dir.join(".env"), format!("{}PROXY_DNS_TTL_SECS=1m\n", base)).unwrap();
    wait_until(
        || settings().contains("{\"key\":\"PROXY_DNS_TTL_SECS\",\"value\":\"1m\"}"),
        "再読込の読めない値が settings.invalid に出る",
    );
    drop(child);
    let _ = std::fs::remove_dir_all(&dir);

    // 正しい `.env` では 1 行も出ない
    let (child, port, dir, log) = start_proxy_logged(
        "t2806-ok",
        &format!("{}PROXY_KEEPALIVE_SECS=7\n", base),
        &[],
    );
    assert!(
        !log.iter()
            .any(|l| l.contains("is not a") || l.contains("unknown setting")),
        "{:#?}",
        log
    );
    assert!(
        status_json(port).contains("\"invalid\":[],\"unknown\":[]"),
        "正しい .env では空"
    );
    drop(child);
    let _ = std::fs::remove_dir_all(&dir);

    // `--check` にも名前が出る
    let dir = std::env::temp_dir().join(format!("rhp-t2806-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".env"), &bad).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
        .arg("--check")
        .env("HOME", &dir)
        .env("XDG_CACHE_HOME", &dir)
        .output()
        .expect("--check が動かない");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let at = text
        .find("not readable (ignored")
        .unwrap_or_else(|| panic!("読めなかった行の見出しが無い:\n{}", text));
    let tail = &text[at..];
    for key in [
        "PROXY_KEEPALIVE_SECS",
        "PROXY_TIMEOUT_SECS",
        "PROXY_FOO_BAR",
        "did you mean PROXY_TIMEOUT_SECS?",
    ] {
        assert!(tail.contains(key), "{} が --check に無い:\n{}", key, text);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 真偽の鍵・`PROXY_PROFILE`・`PROXY_SLO` の読めない値も WARN 1 行ずつ (T.28.31)。
/// 扱いは今までどおり (真偽の鍵は on、`PROXY_PROFILE` は既定、`PROXY_SLO` は読めた項目だけ当てる)。
#[test]
fn test_integration_unreadable_flags_profile_and_slo_are_reported() {
    let base = "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\n";
    let bad = format!(
        "{}PROXY_ALLOW_LOCAL=maybe\nPROXY_PROFILE=fast\nPROXY_SLO=connect_p50_ms=20,error_rate=abc,foo=1\n",
        base
    );
    let (child, port, dir, log) = start_proxy_logged("t2831", &bad, &[]);
    let warns: Vec<&String> = log
        .iter()
        .filter(|l| l.contains("WARN") && l.contains(" is not "))
        .collect();
    assert_eq!(warns.len(), 3, "WARN は 3 行: {:#?}", log);
    let json = status_json(port);
    let s = &json[json.find("\"settings\":").expect("settings")..];
    for key in [
        "{\"key\":\"PROXY_ALLOW_LOCAL\",\"value\":\"maybe\"}",
        "{\"key\":\"PROXY_PROFILE\",\"value\":\"fast\"}",
        "{\"key\":\"PROXY_SLO\",\"value\":\"error_rate=abc,foo=1\"}",
    ] {
        assert!(s.contains(key), "{} が無い: {}", key, s);
    }
    let config = endpoint_json(port, "/config");
    assert_eq!(setting(&config, "PROXY_ALLOW_LOCAL").0, "true");
    let slo = setting(&config, "PROXY_SLO").0;
    assert!(
        slo.contains("connect_p50_ms=20") && slo.contains("error_rate=0.005"),
        "読めた項目だけ当たる: {}",
        slo
    );
    drop(child);
    let _ = std::fs::remove_dir_all(&dir);

    // 正しい書き方では 1 行も出ない
    let (child, port, dir, log) = start_proxy_logged(
        "t2831-ok",
        &format!(
            "{}PROXY_ALLOW_LOCAL=off\nPROXY_IPV6=yes\nPROXY_SLO=connect_p50_ms=20,\n",
            base
        ),
        &[],
    );
    assert!(!log.iter().any(|l| l.contains(" is not ")), "{:#?}", log);
    assert!(status_json(port).contains("\"invalid\":[],\"unknown\":[]"));
    drop(child);
    let _ = std::fs::remove_dir_all(&dir);
}

/// T.28.41: `PROXY_ALLOW_LOCAL=private` は読める値 (WARN を出さない)。`/config` には文字列で、
/// `--check` には `private` と出る。`on` / `off` は今までどおり真偽で出る。
#[test]
fn test_integration_allow_local_private_is_a_readable_value() {
    let env =
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\nPROXY_ALLOW_LOCAL=Private\n";
    let (child, port, dir, log) = start_proxy_logged("t2841", env, &[]);
    assert!(!log.iter().any(|l| l.contains(" is not ")), "{:#?}", log);
    assert!(status_json(port).contains("\"invalid\":[],\"unknown\":[]"));
    let config = endpoint_json(port, "/config");
    assert_eq!(
        setting(&config, "PROXY_ALLOW_LOCAL"),
        ("\"private\"".to_string(), "env_file".to_string())
    );
    drop(child);
    let _ = std::fs::remove_dir_all(&dir);

    let dir = std::env::temp_dir().join(format!("rhp-t2841-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".env"), env).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
        .arg("--check")
        .env("HOME", &dir)
        .env("XDG_CACHE_HOME", &dir)
        .output()
        .expect("--check が動かない");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let line = text
        .lines()
        .find(|l| l.contains("PROXY_ALLOW_LOCAL"))
        .unwrap_or_else(|| panic!("PROXY_ALLOW_LOCAL の行が無い:\n{}", text));
    assert!(line.trim_end().ends_with(" private"), "{}", line);
    assert!(!text.contains("not readable (ignored"), "{}", text);
    let _ = std::fs::remove_dir_all(&dir);
}

/// ビルドの種類 (cargo のプロファイル名) が `-V`・`--check`・`/status` に出る (T.28.42)。
///
/// 値は `build.rs` が `OUT_DIR` から読む (`release` と `dist` を分けるのは
/// `crates/server/build_profile.rs` の単体の試験)。この試験のバイナリは `cargo test` の
/// プロファイルで作られるので、`debug` (`--release` を付ければ `release`) と同じ値が
/// 3 か所にそろうことを見る。
#[test]
fn test_integration_build_profile_is_shown_in_version_check_and_status() {
    let build = rust_http_proxy::BUILD;
    let expected = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    assert_eq!(build, expected, "テストのビルドのプロファイル");

    let dir = std::env::temp_dir().join(format!("rhp-t2842-build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
        .arg("-V")
        .env("HOME", &dir)
        .output()
        .expect("-V が動かない");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(
        text.trim(),
        format!("rust-http-proxy {} ({})", rust_http_proxy::VERSION, build),
        "{}",
        text
    );

    let out = Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
        .arg("--check")
        .env("HOME", &dir)
        .env("XDG_CACHE_HOME", &dir)
        .output()
        .expect("--check が動かない");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(
        text.lines().nth(1),
        Some(format!("build profile: {}", build).as_str()),
        "2 行目にビルドの種類:\n{}",
        text
    );
    let _ = std::fs::remove_dir_all(&dir);

    // `/status` は末尾に足した `build` (既存の鍵の順は変えない)
    let port = common::start_test_proxy(common::proxy_config());
    let status = status_json(port);
    assert!(
        status.ends_with(&format!(",\"build\":\"{}\"}}", build)),
        "{}",
        &status[status.len().saturating_sub(200)..]
    );
}

/// `--check` の読み手が先に閉じても (`--check | head -1`)、パニックせずに黙って終わる (T.28.48)。
/// 以前は `println!` が `EPIPE` で `failed printing to stdout` のパニックを出していた。
#[test]
fn test_integration_check_is_quiet_when_the_reader_closes_early() {
    use std::io::Read;
    use std::process::Stdio;
    let dir = std::env::temp_dir().join(format!("rhp-t2848-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
        .arg("--check")
        .env("HOME", &dir)
        .env("XDG_CACHE_HOME", &dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("--check が動かない");
    // 1 行目の分だけ読んで閉じる (`head -1` と同じ)
    let mut out = child.stdout.take().unwrap();
    let mut first = [0u8; 16];
    let _ = out.read(&mut first);
    drop(out);
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(!err.contains("panicked"), "{}", err);
    // 終わり方は「書けたので 0」か「SIGPIPE で黙って終わる」のどちらか (パニックの 101 ではない)
    assert_ne!(status.code(), Some(101), "{:?}: {}", status, err);
    let _ = std::fs::remove_dir_all(&dir);
}
