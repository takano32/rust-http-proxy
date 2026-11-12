//! 試験の一時ディレクトリと、実バイナリに読ませる `.env` の共通の文面 (T34.107)。

use std::path::{Path, PathBuf};

/// 実バイナリを起こす試験の `.env` に共通の 3 行 (空いているポート・ループバックだけ・
/// 起動ログを読むための info)。
pub const ENV_BASE: &str = "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\n";

/// `<一時ディレクトリ>/<name>-<pid>` を空にして返す (作りはしない。キャッシュの置き場のように、
/// 使う側が作るもの用)。
///
/// pid を付けるのは、worktree を並べて同じ試験を回したとき、別の実行が使っている最中の
/// 置き場を消さないため (T34.162。[`scratch_dir`] と同じ)。
pub fn fresh_dir(name: &str) -> PathBuf {
    sweep_dead_once();
    let dir = std::env::temp_dir().join(format!("{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// `<一時ディレクトリ>/<tag>-<pid>` を空にして作り、返す (`HOME` や状態ファイルの置き場)。
///
/// pid を付けるのは、同じ試験バイナリを並べて回しても置き場がぶつからないため。
pub fn scratch_dir(tag: &str) -> PathBuf {
    sweep_dead_once();
    let dir = std::env::temp_dir().join(format!("{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 前の実行が残した置き場を、このプロセスで最初に置き場を作るときに 1 度だけ消す (T36.24)。
///
/// [`scratch_dir`] と [`fresh_dir`] の置き場は `<tag>-<pid>` で、試験の終わりには消さない
/// (落ちた試験の中身を後から見られるように。呼ぶ側の `remove_dir_all` も、落ちれば届かない)。
/// そのままだと回すたびに一時ディレクトリに溜まる (この機械で 1,000 を超えた) ので、
/// **pid のプロセスがもう居ない**置き場だけを消す。居る pid (並べて回している別の worktree の
/// 試験) の置き場は消さない。消すのは試験の置き場の名前 (`rhp-` か `shp-` で始まり、
/// `-<数字>` で終わる) だけ。
fn sweep_dead_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // pid が生きているかを `/proc/<pid>` で見るので、`/proc` の無い所では何もしない
        if !Path::new("/proc/self").exists() {
            return;
        }
        sweep_dead(&std::env::temp_dir());
    });
}

/// `dir` の直下の試験の置き場のうち、pid のプロセスが居ないものを消す (消した数を返す)。
pub fn sweep_dead(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(scratch_pid) else {
            continue;
        };
        if !Path::new("/proc").join(pid.to_string()).exists()
            && std::fs::remove_dir_all(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// 試験の置き場の名前 (`rhp-…-<pid>` / `shp-…-<pid>`) なら pid を返す。
pub fn scratch_pid(name: &str) -> Option<u32> {
    if !(name.starts_with("rhp-") || name.starts_with("shp-")) {
        return None;
    }
    let (_, pid) = name.rsplit_once('-')?;
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

/// `dir/.env` に [`ENV_BASE`] と `extra` (行ごとに `\n` 終端) を書く。
pub fn write_env(dir: &Path, extra: &str) {
    std::fs::write(dir.join(".env"), format!("{}{}", ENV_BASE, extra)).unwrap();
}
