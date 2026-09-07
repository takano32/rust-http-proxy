//! ファイルシステム関連: 空き容量 (statvfs)、事前確保 (fallocate)、マウント種別の判定。
//!
//! 外部クレートを使わず、std が常にリンクする libc のシンボルを直接宣言して呼ぶ。
//! 64bit Linux 以外では `fs_info` は `None`、`preallocate` は `Unsupported` を返す。

use std::fs::File;
use std::io;
use std::path::Path;

/// `df` と同じ流儀の使用量 (バイト)。`total = used + available` で root 予約分は含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsInfo {
    pub total: u64,
    pub used: u64,
    pub available: u64,
}

impl FsInfo {
    pub fn used_percent(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used as f64 * 100.0 / self.total as f64
        }
    }
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod imp {
    use super::FsInfo;
    use std::ffi::{CString, c_char, c_int, c_ulong};
    use std::fs::File;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    /// glibc / musl (64bit) の `struct statvfs`。使う先頭 8 フィールドの並びは両者で共通で、
    /// 末尾は実際の構造体 (112 バイト) より大きめに確保して書き込みを受け止める。
    #[repr(C)]
    struct StatVfs {
        f_bsize: c_ulong,
        f_frsize: c_ulong,
        f_blocks: u64,
        f_bfree: u64,
        f_bavail: u64,
        f_files: u64,
        f_ffree: u64,
        f_favail: u64,
        _tail: [u64; 16],
    }

    unsafe extern "C" {
        fn statvfs(path: *const c_char, buf: *mut StatVfs) -> c_int;
        fn fallocate(fd: c_int, mode: c_int, offset: i64, len: i64) -> c_int;
        fn posix_fadvise(fd: c_int, offset: i64, len: i64, advice: c_int) -> c_int;
    }

    const POSIX_FADV_DONTNEED: c_int = 4;

    pub fn fs_info(path: &Path) -> Option<FsInfo> {
        let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: 全フィールドが整数なのでゼロ初期化は妥当。statvfs は成功時に構造体を埋める。
        let mut st: StatVfs = unsafe { std::mem::zeroed() };
        if unsafe { statvfs(c_path.as_ptr(), &mut st) } != 0 {
            return None;
        }
        let unit = if st.f_frsize > 0 {
            st.f_frsize
        } else {
            st.f_bsize
        } as u64;
        let used = st.f_blocks.saturating_sub(st.f_bfree).saturating_mul(unit);
        let available = st.f_bavail.saturating_mul(unit);
        Some(FsInfo {
            total: used.saturating_add(available),
            used,
            available,
        })
    }

    pub fn preallocate(file: &File, offset: u64, len: u64) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let (Ok(offset), Ok(len)) = (i64::try_from(offset), i64::try_from(len)) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "range too large",
            ));
        };
        // SAFETY: 有効な fd と範囲を渡すだけで、メモリ安全性に影響する副作用はない。
        if unsafe { fallocate(file.as_raw_fd(), 0, offset, len) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn drop_page_cache(file: &File) {
        // SAFETY: ヒントを渡すだけで失敗しても害はない (戻り値は無視する)。
        unsafe {
            posix_fadvise(file.as_raw_fd(), 0, 0, POSIX_FADV_DONTNEED);
        }
    }
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
mod imp {
    use super::FsInfo;
    use std::fs::File;
    use std::io;
    use std::path::Path;

    pub fn fs_info(_path: &Path) -> Option<FsInfo> {
        None
    }

    pub fn preallocate(_file: &File, _offset: u64, _len: u64) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "fallocate is not available",
        ))
    }

    pub fn drop_page_cache(_file: &File) {}
}

/// ファイルシステムの空き容量。
pub fn fs_info(path: &Path) -> Option<FsInfo> {
    imp::fs_info(path)
}

/// `file` の `offset` から `len` バイト分のブロックを実際に確保する (sparse にはしない)。
pub fn preallocate(file: &File, offset: u64, len: u64) -> io::Result<()> {
    imp::preallocate(file, offset, len)
}

/// ファイルのページキャッシュを手放すようカーネルに助言する (L1 と L2 の二重キャッシュを避ける)。
/// dirty なページは書き戻しが始まるだけで、次回の助言か通常の回収で消える。
pub fn drop_page_cache(file: &File) {
    imp::drop_page_cache(file)
}

/// `root` 配下の通常ファイルの見かけのサイズ (バイト) を合計する。`exclude` 配下は数えない。
/// シンボリックリンクは辿らない。Pterodactyl (Wings) のディスク使用量計算と同じ流儀。
pub fn dir_size_excluding(root: &Path, exclude: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == exclude {
                continue;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
            }
        }
    }
    total
}

/// `preallocate` の失敗が「この環境では使えない」種類のものか (一時的な ENOSPC などと区別する)。
pub fn is_unsupported(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::Unsupported
        || matches!(e.raw_os_error(), Some(95) | Some(38) | Some(22)) // EOPNOTSUPP / ENOSYS / EINVAL
}

/// `path` が属するマウントのファイルシステム種別 (`/proc/mounts` から)。
pub fn fs_type(path: &Path) -> Option<String> {
    let canon = std::fs::canonicalize(path).ok()?;
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    parse_mounts(&mounts, &canon)
}

/// `/proc/mounts` の内容から `path` を含む最も深いマウントポイントの種別を返す。
pub fn parse_mounts(text: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(_dev), Some(mnt), Some(fstype)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let mnt = unescape_mount(mnt);
        let mp = Path::new(&mnt);
        if path.starts_with(mp) {
            let depth = mp.components().count();
            // 同じ深さなら後勝ち (後からマウントされた方が見えている)
            if best.is_none_or(|(d, _)| depth >= d) {
                best = Some((depth, fstype));
            }
        }
    }
    best.map(|(_, t)| t.to_string())
}

/// `/proc/mounts` はスペース等を 8 進エスケープ (`\040`) で表す。
fn unescape_mount(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

/// RAM 上に置かれるファイルシステムか (ここに「ディスク」キャッシュを置くと RAM を食う)。
pub fn is_ram_backed(fstype: &str) -> bool {
    matches!(fstype, "tmpfs" | "ramfs" | "devtmpfs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_deepest_mount() {
        let mounts = "/dev/sda1 / ext4 rw 0 0\ntmpfs /tmp tmpfs rw 0 0\n/dev/sdb1 /tmp/big\\040disk xfs rw 0 0\n";
        assert_eq!(
            parse_mounts(mounts, Path::new("/home/x")).as_deref(),
            Some("ext4")
        );
        assert_eq!(
            parse_mounts(mounts, Path::new("/tmp/cache")).as_deref(),
            Some("tmpfs")
        );
        assert_eq!(
            parse_mounts(mounts, Path::new("/tmp/big disk/c")).as_deref(),
            Some("xfs")
        );
        assert!(is_ram_backed("tmpfs") && !is_ram_backed("ext4"));
    }

    #[test]
    fn dir_size_skips_excluded_subtree() {
        let root = std::env::temp_dir().join("shp-test-dirsize");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("keep/deep")).unwrap();
        std::fs::create_dir_all(root.join("cache")).unwrap();
        std::fs::write(root.join("a.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(root.join("keep/deep/b.bin"), vec![0u8; 50]).unwrap();
        std::fs::write(root.join("cache/big.bin"), vec![0u8; 10_000]).unwrap();
        assert_eq!(dir_size_excluding(&root, &root.join("cache")), 150);
        assert_eq!(dir_size_excluding(&root, Path::new("/nonexistent")), 10_150);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn used_percent_is_zero_for_empty_fs() {
        let f = FsInfo {
            total: 0,
            used: 0,
            available: 0,
        };
        assert_eq!(f.used_percent(), 0.0);
        let g = FsInfo {
            total: 200,
            used: 50,
            available: 150,
        };
        assert_eq!(g.used_percent(), 25.0);
    }

    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    #[test]
    fn statvfs_and_fallocate_work_on_tempdir() {
        let dir = std::env::temp_dir();
        let info = fs_info(&dir).expect("statvfs should succeed");
        assert!(info.total > 0 && info.used + info.available == info.total);

        let path = dir.join("shp-test-fallocate.bin");
        let file = File::create(&path).unwrap();
        match preallocate(&file, 0, 1 << 20) {
            Ok(()) => assert_eq!(file.metadata().unwrap().len(), 1 << 20),
            Err(e) => assert!(is_unsupported(&e), "unexpected error: {}", e),
        }
        drop(file);
        let _ = std::fs::remove_file(&path);
    }
}
