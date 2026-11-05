//! 試験の一時ディレクトリと、実バイナリに読ませる `.env` の共通の文面 (T.34.107)。

use std::path::{Path, PathBuf};

/// 実バイナリを起こす試験の `.env` に共通の 3 行 (空いているポート・ループバックだけ・
/// 起動ログを読むための info)。
pub const ENV_BASE: &str = "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\n";

/// `<一時ディレクトリ>/<name>-<pid>` を空にして返す (作りはしない。キャッシュの置き場のように、
/// 使う側が作るもの用)。
///
/// pid を付けるのは、worktree を並べて同じ試験を回したとき、別の実行が使っている最中の
/// 置き場を消さないため (T.34.162。[`scratch_dir`] と同じ)。
pub fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// `<一時ディレクトリ>/<tag>-<pid>` を空にして作り、返す (`HOME` や状態ファイルの置き場)。
///
/// pid を付けるのは、同じ試験バイナリを並べて回しても置き場がぶつからないため。
pub fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `dir/.env` に [`ENV_BASE`] と `extra` (行ごとに `\n` 終端) を書く。
pub fn write_env(dir: &Path, extra: &str) {
    std::fs::write(dir.join(".env"), format!("{}{}", ENV_BASE, extra)).unwrap();
}
