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
use crate::format::{self, Meta};
use crate::key::CacheKey;
use crate::log_info;
use crate::lru::Store;

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

/// 分割ディレクトリのファイル 1 つを、ヘッダーを読まずに索引へ入れてよいか (T34.96)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// 高速パス: mtime に有効期限が入っていて、まだ先。この `Meta` で索引に入れる
    /// (`stored_at` は 0 = ヒット時にヘッダーから読み直す)
    IndexOnly(Meta),
    /// mtime が過去 (期限切れ、またはコピー等で mtime が失われた) か読めない: ヘッダーを読む
    ReadHeader,
}

/// ファイルの mtime (epoch 秒) と拡張子のバリデータの有無から、扱いを決める (I/O の無い
/// 純粋な関数。T34.96)。
fn classify(mtime: Option<u64>, validators: bool, now: u64) -> Keep {
    match mtime {
        Some(t) if t > now => Keep::IndexOnly(Meta {
            stored_at: 0,
            expires_at: t,
            validators,
        }),
        _ => Keep::ReadHeader,
    }
}

/// 同じキーの `.cache` と `.vcache` が両方あったとき、先に索引にあった方 (`old`) を残すか
/// (期限の長い方を残す。同じなら後から見つけた方。T34.96)。
fn keep_longer(old: &Meta, new: &Meta) -> bool {
    old.expires_at > new.expires_at
}

impl DiskTier {
    /// 直下の旧レイアウトのエントリを分割ディレクトリへ移し、一時ファイルを消す。
    pub(crate) fn scan_root(
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
                Some(meta) if !meta.is_garbage(now, max_stale) => {
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

    pub(crate) fn scan_shard(
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
            let meta = match classify(mtime_epoch(&stat), validators, now) {
                Keep::IndexOnly(meta) => meta,
                Keep::ReadHeader => match format::read_meta(&path) {
                    Some(m) => m,
                    None => {
                        let _ = fs::remove_file(&path);
                        report.removed += 1;
                        continue;
                    }
                },
            };
            if meta.is_garbage(now, max_stale) {
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
                if keep_longer(&old.meta, &meta) {
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

pub(crate) fn set_expiry_mtime(path: &Path, expires_at: u64) {
    if let (Ok(f), Some(t)) = (
        OpenOptions::new().write(true).open(path),
        UNIX_EPOCH.checked_add(Duration::from_secs(expires_at)),
    ) {
        let _ = f.set_modified(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "0123456789abcdef0123456789abcdef";

    /// ファイル名は `<キー 32 桁>.cache` か `.vcache` だけを受ける (T34.96)。
    #[test]
    fn a_name_is_a_key_and_its_extension() {
        // `CacheKey` は Debug を持たないので、== で比べる
        let key = CacheKey::from_hex(HEX).expect("32 桁の 16 進");
        assert!(parse_name(&format!("{}.cache", HEX)) == Some((key, false)));
        assert!(parse_name(&format!("{}.vcache", HEX)) == Some((key, true)));
        assert!(parse_name(&format!("{}.tmp", HEX)).is_none());
        assert!(parse_name(HEX).is_none(), "拡張子が無い");
        assert!(parse_name("ballast.reserve").is_none());
        assert!(parse_name("xyz.cache").is_none(), "16 進でない");
        assert!(
            parse_name(&format!("{}.cache", &HEX[..31])).is_none(),
            "桁が足りない"
        );
        assert!(parse_name(".cache").is_none());
    }

    /// mtime がまだ先ならヘッダーを読まない。過去・ちょうど・読めないなら読む (T34.96)。
    #[test]
    fn only_a_future_mtime_skips_the_header() {
        assert_eq!(
            classify(Some(101), true, 100),
            Keep::IndexOnly(Meta {
                stored_at: 0,
                expires_at: 101,
                validators: true,
            })
        );
        assert_eq!(classify(Some(100), false, 100), Keep::ReadHeader);
        assert_eq!(classify(Some(0), false, 100), Keep::ReadHeader);
        assert_eq!(classify(None, true, 100), Keep::ReadHeader);
    }

    /// `.cache` と `.vcache` が両方あれば期限の長い方を残す (同じなら後から見つけた方) (T34.96)。
    #[test]
    fn the_longer_lived_twin_is_kept() {
        let m = |expires_at| Meta {
            stored_at: 0,
            expires_at,
            validators: false,
        };
        assert!(keep_longer(&m(200), &m(100)));
        assert!(!keep_longer(&m(100), &m(200)));
        assert!(!keep_longer(&m(100), &m(100)));
    }

    /// 有効期限を書いた mtime はそのまま epoch 秒で読み戻せる (T34.96)。
    #[test]
    fn the_expiry_round_trips_through_the_mtime() {
        let path = std::env::temp_dir().join(format!(
            "rust-http-proxy-scan-test-{}.cache",
            std::process::id()
        ));
        fs::write(&path, b"x").expect("書ける");
        set_expiry_mtime(&path, 2_000_000_000);
        let got = mtime_epoch(&fs::metadata(&path).expect("stat"));
        let _ = fs::remove_file(&path);
        assert_eq!(got, Some(2_000_000_000));
    }
}
