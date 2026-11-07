//! 既定のキャッシュのディレクトリ選びは `.env` の `HOME` も見て、`--check` では作らない (T35.8)。
//!
//! 0.35 までは、選ぶときの `HOME`/`XDG_CACHE_HOME` をプロセスの環境変数からだけ読んでいた
//! (`.env` に `HOME` を書くと、`PROXY_DISK_QUOTA_ROOT` の既定と食い違う)。また、設定を読む
//! だけで候補のディレクトリを `create_dir_all` し `.write-test` を書いていた (`--check` でも)。
#![cfg(target_os = "linux")]

mod common;
use common::*;

/// `--check` の設定の一覧から `key` の行を取る。
fn setting_line<'a>(stdout: &'a str, key: &str) -> &'a str {
    stdout
        .lines()
        .find(|l| l.split_whitespace().nth(1) == Some(key))
        .unwrap_or_else(|| panic!("{} が無い: {}", key, stdout))
}

#[test]
fn test_integration_default_cache_dir_follows_home_in_the_env_file_and_check_creates_nothing() {
    let dir = scratch_dir("rhp-t358");
    let home2 = dir.join("home2");
    std::fs::create_dir_all(&home2).unwrap();
    // Pterodactyl の印を立てて `~/.cache` を最初の候補にする (root では `/var/cache` が先に選ばれるため)
    std::fs::write(
        dir.join(".env"),
        format!("HOME={}\nP_SERVER_UUID=t358\n", home2.display()),
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
        .arg("--check")
        .env("HOME", &dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);

    let want = home2.join(".cache/rust-http-proxy");
    let cache_dir = setting_line(&stdout, "PROXY_CACHE_DIR");
    assert!(
        cache_dir.ends_with(&want.display().to_string()),
        "{}",
        cache_dir
    );
    // 割当の根と同じ HOME を見る
    let quota_root = setting_line(&stdout, "PROXY_DISK_QUOTA_ROOT");
    assert!(
        quota_root.ends_with(&home2.display().to_string()),
        "{}",
        quota_root
    );
    // `--check` は 1 つも作らない
    assert!(
        !home2.join(".cache").exists(),
        "{} を作った",
        want.display()
    );
    assert!(!dir.join(".cache").exists(), "プロセスの HOME の側を作った");

    let _ = std::fs::remove_dir_all(&dir);
}
