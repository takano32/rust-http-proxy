//! `Cache` 全体 (両層 + 予算 + 再検証) の結合テスト。各層の単体テストはそれぞれのモジュールにある。
//! 補助関数はここに置き、テスト本体は basic / disk / budget に分ける。

mod basic;
mod budget;
mod disk;

use std::env;
use std::fs;
use std::path::PathBuf;

use super::Cache;
use super::config::CacheConfig;

pub(super) fn test_dir(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(name);
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// `$TMPDIR` は tmpfs のことが多いので、fallocate を試すテストは実ディスク上の `target/` を使う。
pub(super) fn on_disk_test_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-cache")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    dir
}

pub(super) fn fresh(name: &str, mem: u64, disk: u64) -> Cache {
    Cache::new(CacheConfig::fixed(mem, disk, test_dir(name)))
}

/// ワイヤ形式のレスポンスを組み立てる。
pub(super) fn wire(body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"t\"\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}
