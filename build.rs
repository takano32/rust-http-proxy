//! 版の文字列 (`PROXY_VERSION`) を作るだけのビルドスクリプト (T12.6)。
//!
//! **外部クレートは使わない** (このリポジトリの方針。`std::process::Command` で `git` を呼ぶだけ)。
//! 形は `CARGO_PKG_VERSION` + `+<git の短いハッシュ>` で、追跡しているファイルに
//! 未コミットの変更があれば `-dirty` を足す (例: `0.1.0+144b992`、`0.1.0+144b992-dirty`)。
//! `git` が無い・`.git` が無い (`cargo install --git` の展開先、`git archive` で配ったソース、
//! Docker の `COPY` でソースだけ入れた場合) は `0.1.0+unknown` にして、**ビルドは通す**。
//!
//! 作り直しの条件は HEAD と参照だけにしてある (ソースを触るたびに `git status` を
//! 走らせない)。そのため、**コミットせずにソースだけ直したときは `-dirty` が遅れて付く**
//! ことがある。配るバイナリは常にきれいな作業ツリーから作るので実害はない。

use std::path::Path;
use std::process::Command;

/// `git` を呼んで標準出力を返す (`git` が無い・失敗したら `None`)。
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    Some(s.trim().to_string())
}

fn main() {
    // HEAD と参照が動いたら作り直す。worktree では `.git` がファイルで HEAD の実体は
    // 別の場所にあるので、パスは決め打ちにせず `git` に聞く
    for name in ["HEAD", "refs/heads"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name])
            && Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={}", path);
        }
    }

    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let version = match git(&["rev-parse", "--short", "HEAD"]) {
        Some(hash) if !hash.is_empty() => {
            // 追跡していないファイル (作業メモ、退避したバイナリ) は汚れに数えない
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty());
            format!("{}+{}{}", pkg, hash, if dirty { "-dirty" } else { "" })
        }
        _ => format!("{}+unknown", pkg),
    };
    println!("cargo:rustc-env=PROXY_VERSION={}", version);
}
