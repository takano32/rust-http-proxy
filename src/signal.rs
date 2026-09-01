//! SIGTERM / SIGINT で `ballast.reserve` を切り詰めてから終了する。
//!
//! Pterodactyl (Wings) はシグナルで停止するので、停止中にバラストがディスク使用量として
//! 残らないようにする。ハンドラ内では async-signal-safe な `truncate(2)` と `_exit(2)` だけを使う。

use std::ffi::{CString, c_int};
use std::path::Path;
use std::sync::OnceLock;

static BALLAST: OnceLock<CString> = OnceLock::new();

#[cfg(unix)]
mod imp {
    use std::ffi::{c_char, c_int};

    unsafe extern "C" {
        pub fn signal(sig: c_int, handler: usize) -> usize;
        pub fn truncate(path: *const c_char, len: i64) -> c_int;
        pub fn _exit(code: c_int) -> !;
    }

    pub const SIGINT: c_int = 2;
    pub const SIGTERM: c_int = 15;
}

#[cfg(unix)]
extern "C" fn on_signal(_sig: c_int) {
    if let Some(path) = BALLAST.get() {
        // SAFETY: NUL 終端の静的な文字列を渡すだけ。失敗しても何もしない。
        unsafe {
            imp::truncate(path.as_ptr(), 0);
        }
    }
    // SAFETY: プロセスを即座に終了する (デストラクタは走らない)。
    unsafe { imp::_exit(0) }
}

/// SIGTERM / SIGINT のハンドラを登録する。`ballast` は終了時に空にするファイル。
pub fn install(ballast: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if let Ok(c) = CString::new(ballast.as_os_str().as_bytes()) {
            let _ = BALLAST.set(c);
        }
        let handler = on_signal as extern "C" fn(c_int) as usize;
        // SAFETY: ハンドラは async-signal-safe な関数しか呼ばない。
        unsafe {
            imp::signal(imp::SIGTERM, handler);
            imp::signal(imp::SIGINT, handler);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ballast;
    }
}
