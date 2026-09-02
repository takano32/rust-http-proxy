//! `inotify(7)` の最小ラッパー。ディレクトリを監視し、名前を指定したファイルの
//! 作成・書込完了・rename・削除を検知する。libc のシンボルを直接参照するので依存は無い。
//!
//! Linux 以外では [`Watch::open`] が `Unsupported` を返し、呼び出し側は mtime の
//! ポーリングへフォールバックする。

use std::io;
use std::path::Path;
use std::time::Duration;

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::time::Duration;

    unsafe extern "C" {
        fn inotify_init1(flags: c_int) -> c_int;
        fn inotify_add_watch(fd: c_int, path: *const c_char, mask: u32) -> c_int;
        fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
        fn close(fd: c_int) -> c_int;
        fn poll(fds: *mut PollFd, nfds: u64, timeout: c_int) -> c_int;
    }

    #[repr(C)]
    struct PollFd {
        fd: c_int,
        events: i16,
        revents: i16,
    }

    const IN_CLOEXEC: c_int = 0o2000000;
    const IN_NONBLOCK: c_int = 0o4000;
    const IN_CLOSE_WRITE: u32 = 0x8;
    const IN_CREATE: u32 = 0x100;
    const IN_DELETE: u32 = 0x200;
    const IN_MOVED_TO: u32 = 0x80;
    const IN_MOVED_FROM: u32 = 0x40;
    const POLLIN: i16 = 0x1;
    const EINTR: i32 = 4;
    const EAGAIN: i32 = 11;

    pub struct Watch {
        fd: c_int,
        name: Vec<u8>,
    }

    impl Watch {
        pub fn open(dir: &Path, name: &str) -> io::Result<Watch> {
            let c_dir = CString::new(dir.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
            // SAFETY: 引数はフラグ定数のみ。失敗は -1 で返る。
            let fd = unsafe { inotify_init1(IN_CLOEXEC | IN_NONBLOCK) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let mask = IN_CLOSE_WRITE | IN_CREATE | IN_DELETE | IN_MOVED_TO | IN_MOVED_FROM;
            // SAFETY: fd は直前に得た有効な記述子、パスは NUL 終端。
            if unsafe { inotify_add_watch(fd, c_dir.as_ptr(), mask) } < 0 {
                let e = io::Error::last_os_error();
                // SAFETY: 自分で開いた fd を閉じる。
                unsafe { close(fd) };
                return Err(e);
            }
            Ok(Watch {
                fd,
                name: name.as_bytes().to_vec(),
            })
        }

        /// `timeout` まで待ち、監視対象のファイルに変化があれば `true`。
        pub fn wait(&self, timeout: Duration) -> io::Result<bool> {
            let mut pfd = PollFd {
                fd: self.fd,
                events: POLLIN,
                revents: 0,
            };
            let ms = timeout.as_millis().min(i32::MAX as u128) as c_int;
            // SAFETY: pfd は 1 要素の有効な配列。
            let n = unsafe { poll(&mut pfd, 1, ms) };
            if n < 0 {
                let e = io::Error::last_os_error();
                return if e.raw_os_error() == Some(EINTR) {
                    Ok(false)
                } else {
                    Err(e)
                };
            }
            if n == 0 {
                return Ok(false);
            }
            self.drain()
        }

        /// 溜まったイベントを全部読み、名前が一致するものがあれば `true`。
        fn drain(&self) -> io::Result<bool> {
            let mut hit = false;
            let mut buf = [0u8; 4096];
            loop {
                // SAFETY: buf は有効な書込先で、長さを渡している。
                let n = unsafe { read(self.fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    match e.raw_os_error() {
                        Some(EAGAIN) => return Ok(hit),
                        Some(EINTR) => continue,
                        _ => return Err(e),
                    }
                }
                if n == 0 {
                    return Ok(hit);
                }
                hit |= self.scan(&buf[..n as usize]);
            }
        }

        /// `struct inotify_event` (wd i32, mask u32, cookie u32, len u32, name[len]) を走査する。
        fn scan(&self, mut b: &[u8]) -> bool {
            const HEAD: usize = 16;
            let mut hit = false;
            while b.len() >= HEAD {
                let len = u32::from_ne_bytes([b[12], b[13], b[14], b[15]]) as usize;
                let end = HEAD.saturating_add(len).min(b.len());
                let name = &b[HEAD..end];
                let name = name.split(|&c| c == 0).next().unwrap_or(&[]);
                hit |= name == self.name.as_slice();
                b = &b[end..];
            }
            hit
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            // SAFETY: open で得た fd を 1 回だけ閉じる。
            unsafe { close(self.fd) };
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::io;
    use std::path::Path;
    use std::time::Duration;

    pub struct Watch;

    impl Watch {
        pub fn open(_dir: &Path, _name: &str) -> io::Result<Watch> {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }
        pub fn wait(&self, _timeout: Duration) -> io::Result<bool> {
            Ok(false)
        }
    }
}

/// ディレクトリ `dir` の中のファイル `name` を監視する。
pub struct Watch(imp::Watch);

impl Watch {
    pub fn open(dir: &Path, name: &str) -> io::Result<Watch> {
        imp::Watch::open(dir, name).map(Watch)
    }

    /// `timeout` まで待ち、`name` に作成・書込完了・rename・削除があれば `true`。
    pub fn wait(&self, timeout: Duration) -> io::Result<bool> {
        self.0.wait(timeout)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn detects_write_and_rename() {
        let dir = std::env::temp_dir().join(format!("sorahost-inotify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = Watch::open(&dir, ".env").unwrap();
        assert!(!w.wait(Duration::from_millis(50)).unwrap());

        std::fs::write(dir.join("other"), "x").unwrap();
        assert!(
            !w.wait(Duration::from_millis(200)).unwrap(),
            "other files ignored"
        );

        std::fs::write(dir.join(".env"), "A=1\n").unwrap();
        assert!(w.wait(Duration::from_secs(2)).unwrap(), "write detected");

        std::fs::write(dir.join(".env.tmp"), "A=2\n").unwrap();
        std::fs::rename(dir.join(".env.tmp"), dir.join(".env")).unwrap();
        assert!(w.wait(Duration::from_secs(2)).unwrap(), "rename detected");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
