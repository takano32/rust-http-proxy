//! `/config` (効いている設定とその出どころ) の結合テスト (T14.15)。`capabilities` (この環境で
//! 何が読めるか) と `--check` は `config_check_test.rs` に割った (T.34.120)。
//!
//! どれも**実バイナリを起こして**見る。`capabilities` を測るのは `.env` の監視スレッドで、
//! 出どころ (`default` / `env` / `env_file`) は `$HOME/.env` と実際の環境変数の両方が要るので、
//! プロセスの中に閉じたテストでは配線が丸ごとは見えない。

mod common;

use std::process::Command;

use common::{ENV_BASE, ProxyProcess, endpoint_json, scratch_dir, status_json, wait_until};

/// `$HOME` を一時ディレクトリにしてプロキシを起こす。`env_file` はそこに置く `.env` の中身、
/// `vars` は**実際の環境変数**として渡すもの (出どころの `env` と `env_file` を作り分ける)。
/// 返すのは (子プロセス, 待ち受けポート, `$HOME`)。
fn start_proxy(
    tag: &str,
    env_file: &str,
    vars: &[(&str, &str)],
) -> (ProxyProcess, u16, std::path::PathBuf) {
    let (child, port, dir, _) = start_proxy_logged(tag, env_file, vars);
    (child, port, dir)
}

/// [`start_proxy`] と同じだが、待ち受けの行までに出た起動ログの行も返す (T.28.6)。
fn start_proxy_logged(
    tag: &str,
    env_file: &str,
    vars: &[(&str, &str)],
) -> (ProxyProcess, u16, std::path::PathBuf, Vec<String>) {
    let dir = scratch_dir(&format!("rhp-t1415-{}", tag));
    std::fs::write(dir.join(".env"), env_file).unwrap();

    // 子プロセスは落ちても残らない (`ProxyProcess` の番人)。起動ログは標準出力へ出るので、
    // 待ち受けの行までに出た行を `startup_log` で受け取る
    let mut builder = ProxyProcess::builder(&dir);
    for (k, v) in vars {
        builder = builder.env(k, v);
    }
    let child = builder.start();
    let (port, log) = (child.port, child.startup_log.clone());
    (child, port, dir, log)
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

/// `/config` の行を表で突き合わせる: (鍵, 値, 出どころ, 落ちたときの注記)。落ちたら注記と JSON を出す。
fn assert_settings(json: &str, want: &[(&str, &str, &str, &str)]) {
    for (key, value, source, note) in want {
        assert_eq!(
            setting(json, key),
            (value.to_string(), source.to_string()),
            "{} {}: {}",
            key,
            note,
            json
        );
    }
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
    assert_settings(
        &json,
        &[
            ("PROXY_MIN_TRANSFER_BPS", "0", "env", ""),
            ("PROXY_MIN_TRANSFER_WINDOW_SECS", "10", "env_file", ""),
        ],
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
        let mut text = String::from(ENV_BASE);
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
    assert_settings(
        &json,
        &[
            ("PROXY_DNS_TTL_SECS", "30", "env_file", "`.env` に書いた行"),
            ("PROXY_KEEPALIVE_SECS", "7", "env", "環境変数で渡した行"),
            ("PROXY_TIMEOUT_SECS", "30", "default", "書いていない行"),
            // CONNECT 専用の締め切りも書いていないので、既定の 3 秒が出どころ `default` で出る
            // (`PROXY_TIMEOUT_SECS` は 30 のまま。T15.6 (2)、既定を 5 秒にした T23.3、3 秒にした T.28.43)
            (
                "PROXY_CONNECT_TIMEOUT_SECS",
                "3",
                "default",
                "書いていない行 (既定は 3 秒)",
            ),
            // Happy Eyeballs の間隔も書いていないので、既定の 250 ms が出どころ `default` で出る (T17.6)
            (
                "PROXY_HE_STAGGER_MS",
                "250",
                "default",
                "書いていない行 (既定は 250 ms)",
            ),
        ],
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
    assert_settings(&json, &[("PROXY_TLS_CA_FILE", "null", "default", "")]);

    // `.env` を書き換えると値も出どころも追随する
    std::fs::write(dir.join(".env"), env_file(Some(45))).unwrap();
    wait_until(
        || setting(&endpoint_json(port, "/config"), "PROXY_DNS_TTL_SECS").0 == "45",
        ".env の書き換えが /config に出る",
    );
    let config = endpoint_json(port, "/config");
    assert_settings(&config, &[("PROXY_DNS_TTL_SECS", "45", "env_file", "")]);

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

/// `.env` の打ち間違い (知らない名前) と読めない値 (T.28.6): 起動ログに WARN を 1 行ずつ、
/// `/status` の `settings.invalid` / `settings.unknown` に名前が出る。再読込で直せば消え、
/// 書き損じれば出る。**正しい `.env` では WARN が 1 行も増えない**。
#[test]
fn test_integration_env_typos_and_unreadable_values_are_reported() {
    let base = ENV_BASE;
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
    assert_settings(&config, &[("PROXY_KEEPALIVE_SECS", "15", "default", "")]);

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
    let dir = scratch_dir("rhp-t2806-check");
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
    let base = ENV_BASE;
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
    let env = format!("{}PROXY_ALLOW_LOCAL=Private\n", ENV_BASE);
    let (child, port, dir, log) = start_proxy_logged("t2841", &env, &[]);
    assert!(!log.iter().any(|l| l.contains(" is not ")), "{:#?}", log);
    assert!(status_json(port).contains("\"invalid\":[],\"unknown\":[]"));
    let config = endpoint_json(port, "/config");
    assert_settings(
        &config,
        &[("PROXY_ALLOW_LOCAL", "\"private\"", "env_file", "")],
    );
    drop(child);
    let _ = std::fs::remove_dir_all(&dir);

    let dir = scratch_dir("rhp-t2841-check");
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
