//! 起動時のディスクキャッシュ走査 (インデックス復元)。
//!
//! 分割ディレクトリ内のファイルは mtime = 有効期限なので `stat` だけで済む。
//! mtime が過去 (期限切れ、またはコピー等で mtime が失われた) ならヘッダーを読んで確認する。
//! 直下にある旧レイアウト (フラット配置) のファイルは分割ディレクトリへ移す。

use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use super::{DiskEntry, DiskTier};
use crate::cache::format;
use crate::cache::key::CacheKey;
use crate::cache::lru::Store;
use crate::log_info;

#[derive(Debug, Default, Clone, Copy)]
pub struct ScanReport {
    pub restored: usize,
    pub expired: usize,
    pub migrated: usize,
    pub removed: usize,
}

impl DiskTier {
    /// 直下の旧レイアウトのエントリを分割ディレクトリへ移し、一時ファイルを消す。
    pub(super) fn scan_root(&self, now: u64, report: &mut ScanReport) -> io::Result<()> {
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
            let Some(key) = name.strip_suffix(".cache").and_then(CacheKey::from_hex) else {
                continue;
            };
            match format::read_meta(&path) {
                Some(meta) if meta.expires_at > now => {
                    let dest = self.path_for(key);
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
            let Some(key) = name.strip_suffix(".cache").and_then(CacheKey::from_hex) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let size = meta.len();
            let expires_at = match mtime_epoch(&meta) {
                Some(t) if t > now => t,
                _ => match format::read_meta(&path) {
                    Some(m) => m.expires_at,
                    None => {
                        let _ = fs::remove_file(&path);
                        report.removed += 1;
                        continue;
                    }
                },
            };
            if expires_at <= now {
                let _ = fs::remove_file(&path);
                report.expired += 1;
                continue;
            }
            index.insert(
                key,
                DiskEntry::new(size, expires_at),
                clock.fetch_add(1, Ordering::Relaxed),
            );
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
