//! `capabilities` (この環境で何が読めるか) と、`rust-http-proxy --check` (起動せずに環境と効く設定を
//! 印字して終わる)・`-V` の結合テスト (T14.15)。
//!
//! 初めは `tests/config_test.rs` にあった (T34.120 で割った)。

mod common;

use std::process::Command;

use common::{ProxyProcess, endpoint_json, json, scratch_dir, status_json, wait_until, write_env};

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

/// `rust-http-proxy --check` が**起動せずに**環境と効く設定を印字して終わること (T14.15)。
///
/// 終了コードは `capabilities` の 7 項目 (名前解決を除く) が全部読めたら 0、
/// 1 つでも読めなければ 1。どちらの機械でも落ちないよう、印字と終了コードの
/// **辻褄が合っていること**を見る。
///
/// Linux では `proc_schedstat` **だけ**は読めなくてよい (`CONFIG_SCHEDSTATS` の無い
/// カーネルには `/proc/<tid>/schedstat` がファイルごと無い。T15.0)。ほかが 1 つでも
/// 読めなければ、それはこの機械の設定がおかしいので落とす。
#[test]
fn test_integration_check_prints_capabilities_and_settings() {
    let dir = scratch_dir("rhp-t1415-check");
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
    // 確かめても読めないもの** (cgroup の `cpu.stat` や IPv6 の既定経路が無いコンテナ。T32.24)。印字が機械と食い違うのは落とす
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
    // CONNECT 専用の締め切りも `--check` に出る (書いていないので既定の 3 秒。T15.6、T23.3、T28.43)
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
    let dir = scratch_dir("rhp-t176-stagger");
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

/// ビルドの種類 (cargo のプロファイル名) が `-V`・`--check`・`/status` に出る (T28.42)。
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

    let dir = scratch_dir("rhp-t2842-build");
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

/// `--check` の読み手が先に閉じても (`--check | head -1`)、パニックせずに黙って終わる (T28.48)。
/// 以前は `println!` が `EPIPE` で `failed printing to stdout` のパニックを出していた。
#[test]
fn test_integration_check_is_quiet_when_the_reader_closes_early() {
    use std::io::Read;
    use std::process::Stdio;
    let dir = scratch_dir("rhp-t2848");
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

/// `capabilities` の 8 項目が `/status` に出ること (T14.15、T15.0 で 1 つ増えた)。
///
/// 測るのは `.env` の監視スレッドなので、起動直後の一瞬は `null` のことがある
/// (名前解決の測定に最大 2 秒かかる)。出そろうまで待って形を見る。
#[test]
fn test_integration_capabilities_appear_in_status() {
    // 置き場は前と同じ `rhp-t1415-caps-<pid>` (`config_test.rs` の start_proxy と同じ名前)
    let dir = scratch_dir("rhp-t1415-caps");
    write_env(&dir, "");
    let _child = ProxyProcess::start(&dir);
    let port = _child.port;
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
        let v = json::raw_value(&status, key);
        assert!(v == "true" || v == "false", "{} = {}", key, v);
    }
    // この機械 (Linux、cgroup v2、`$HOME` あり) では全部読める
    #[cfg(target_os = "linux")]
    {
        assert_eq!(
            json::raw_value(&status, "proc_syscall"),
            "true",
            "{}",
            status
        );
        assert_eq!(json::raw_value(&status, "tcp_info"), "true", "{}", status);
        assert_eq!(
            json::raw_value(&status, "home_writable"),
            "true",
            "{}",
            status
        );
    }
    // 名前解決は数値か null (外に出られない環境では null)
    let resolver = json::raw_value(&status, "resolver_ms");
    assert!(
        resolver == "null" || resolver.parse::<u64>().is_ok(),
        "resolver_ms = {}",
        resolver
    );
    // 測った時刻が入っている (いつの判定かが読めること)
    assert!(
        json::raw_value(&status, "checked_at")
            .parse::<u64>()
            .unwrap_or(0)
            > 0,
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
