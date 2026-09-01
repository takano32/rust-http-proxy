//! 起動時のディスクキャッシュ走査 (インデックス復元)。
//!
//! 分割ディレクトリ内のファイルは mtime = 有効期限、拡張子 = バリデータの有無なので
//! `stat` だけで済む。mtime が過去 (期限切れ、またはコピー等で mtime が失われた) なら
//! ヘッダーを読んで確認し、再検証できないか古すぎるものは消す。
//! 直下にある旧レイアウト (フラット配置) のファイルは分割ディレクトリへ移す。

use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use super::{DiskEntry, DiskTier};
use crate::cache::format::{self, Meta};
use crate::cache::key::CacheKey;
use crate::cache::lru::Store;
use crate::cache::memory::is_garbage;
use crate::log_info;

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanReport {
    pub restored: usize,
    pub expired: usize,
    pub migrated: usize,
    pub removed: usize,
}

/// ファイル名からキーとバリデータの有無を取り出す。
fn parse_name(name: &str) -> Option<(CacheKey, bool)> {
    if let Some(stem) = name.strip_suffix(".vcache") {
        return CacheKey::from_hex(stem).map(|k| (k, true));
    }
    name.strip_suffix(".cache")
        .and_then(CacheKey::from_hex)
        .map(|k| (k, false))
}

impl DiskTier {
    /// 直下の旧レイアウトのエントリを分割ディレクトリへ移し、一時ファイルを消す。
    pub(super) fn scan_root(
        &self,
        now: u64,
        max_stale: u64,
        report: &mut ScanReport,
    ) -> io::Result<()> {
        for entry in fs::read_dir(&self.dir)?.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(".tmp") {
                let _ = fs::remove_file(&path);
                report.removed += 1;
                continue;
            }
            let Some((key, _)) = parse_name(name) else {
                continue;
            };
            match format::read_meta(&path) {
                Some(meta) if !is_garbage(&meta, now, max_stale) => {
                    let dest = self.path_for(key, meta.validators);
                    if fs::rename(&path, &dest).is_ok() {
                        set_expiry_mtime(&dest, meta.expires_at);
                        report.migrated += 1;
                    }
                }
                Some(_) => {
                    let _ = fs::remove_file(&path);
                    report.expired += 1;
                }
                None => {
                    let _ = fs::remove_file(&path);
                    report.removed += 1;
                }
            }
        }
        Ok(())
    }

    pub(super) fn scan_shard(
        &self,
        shard: &Path,
        index: &mut Store<DiskEntry>,
        clock: &AtomicU64,
        now: u64,
        max_stale: u64,
        report: &mut ScanReport,
    ) {
        let Ok(entries) = fs::read_dir(shard) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(".tmp") {
                let _ = fs::remove_file(&path);
                report.removed += 1;
                continue;
            }
            let Some((key, validators)) = parse_name(name) else {
                continue;
            };
            let Ok(stat) = entry.metadata() else {
                continue;
            };
            let size = stat.len();
            let meta = match mtime_epoch(&stat) {
                // 高速パス: mtime に有効期限が入っていて、まだ先
                Some(t) if t > now => Meta {
                    stored_at: 0,
                    expires_at: t,
                    validators,
                },
                _ => match format::read_meta(&path) {
                    Some(m) => m,
                    None => {
                        let _ = fs::remove_file(&path);
                        report.removed += 1;
                        continue;
                    }
                },
            };
            if is_garbage(&meta, now, max_stale) {
                let _ = fs::remove_file(&path);
                report.expired += 1;
                continue;
            }
            if index.len() >= self.max_entries() {
                let _ = fs::remove_file(&path);
                report.removed += 1;
                continue;
            }
            let seq = clock.fetch_add(1, Ordering::Relaxed);
            if let Some(old) = index.insert(key, DiskEntry::new(size, meta), seq)
                && old.meta.validators != validators
            {
                // 同じキーの .cache と .vcache が両方ある (確定途中で落ちた等) → 期限の長い方を残す
                if old.meta.expires_at > meta.expires_at {
                    let _ = fs::remove_file(&path);
                    index.insert(key, old, seq);
                } else {
                    let _ = fs::remove_file(self.path_for(key, old.meta.validators));
                }
                report.removed += 1;
                continue;
            }
            report.restored += 1;
            if report.restored.is_multiple_of(100_000) {
                log_info!(None, "disk cache scan: {} entries so far", report.restored);
            }
        }
    }
}

fn mtime_epoch(meta: &fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

pub(super) fn set_expiry_mtime(path: &Path, expires_at: u64) {
    if let (Ok(f), Some(t)) = (
        OpenOptions::new().write(true).open(path),
        UNIX_EPOCH.checked_add(Duration::from_secs(expires_at)),
    ) {
        let _ = f.set_modified(t);
    }
}
