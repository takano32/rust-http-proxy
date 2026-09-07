//! SIGTERM / SIGINT で、統計を状態ファイルに書き、`ballast.reserve` を切り詰めてから終了する。
//!
//! Pterodactyl (Wings) はシグナルで停止する。ハンドラ内で使えるのは async-signal-safe な
//! 関数だけなので、ハンドラは self-pipe に 1 バイト書くだけにし、待っているスレッドが
//! 後始末 ([`install`] に渡した処理) を行う。後始末が [`GRACE`] を超えたら、あるいは 2 回目の
//! シグナルが来たら、バラストだけ `truncate(2)` して即座に `_exit(2)` する。

use std::ffi::{CString, c_int};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

use crate::log_info;

/// 後始末に許す時間。Wings 側の停止猶予より十分短くしておく。
pub const GRACE: Duration = Duration::from_secs(3);

static BALLAST: OnceLock<CString> = OnceLock::new();
static PIPE_WR: AtomicI32 = AtomicI32::new(-1);
static SIGNALED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
mod imp {
    use std::ffi::{c_char, c_int, c_void};

    unsafe extern "C" {
        pub fn signal(sig: c_int, handler: usize) -> usize;
        pub fn truncate(path: *const c_char, len: i64) -> c_int;
        pub fn _exit(code: c_int) -> !;
        pub fn pipe(fds: *mut c_int) -> c_int;
        pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
        pub fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    }

    pub const SIGINT: c_int = 2;
    pub const SIGTERM: c_int = 15;
}

/// バラストを切り詰めて即座に終了する (async-signal-safe)。
#[cfg(unix)]
fn truncate_and_exit() -> ! {
    if let Some(path) = BALLAST.get() {
        // SAFETY: NUL 終端の静的な文字列を渡すだけ。失敗しても何もしない。
        unsafe {
            imp::truncate(path.as_ptr(), 0);
        }
    }
    // SAFETY: プロセスを即座に終了する (デストラクタは走らない)。
    unsafe { imp::_exit(0) }
}

#[cfg(unix)]
extern "C" fn on_signal(_sig: c_int) {
    // 2 回目、または待ち受けスレッドが無いなら即座に終わる
    if SIGNALED.swap(true, Ordering::SeqCst) {
        truncate_and_exit();
    }
    let fd = PIPE_WR.load(Ordering::SeqCst);
    if fd < 0 {
        truncate_and_exit();
    }
    let byte = 1u8;
    // SAFETY: write(2) は async-signal-safe。1 バイト書くだけで、失敗は無視する。
    let n = unsafe { imp::write(fd, &byte as *const u8 as *const _, 1) };
    if n != 1 {
        truncate_and_exit();
    }
}

/// SIGTERM / SIGINT のハンドラを登録する。`ballast` は終了時に空にするファイル (あれば)。
/// `on_stop` はシグナル後に別スレッドで 1 回だけ呼ばれ、終わるか [`GRACE`] を過ぎたら終了する。
pub fn install(ballast: Option<&Path>, on_stop: Box<dyn FnOnce() + Send>) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if let Some(b) = ballast
            && let Ok(c) = CString::new(b.as_os_str().as_bytes())
        {
            let _ = BALLAST.set(c);
        }
        let mut fds = [-1 as c_int; 2];
        // SAFETY: 2 要素の配列を渡す。失敗なら -1。
        if unsafe { imp::pipe(fds.as_mut_ptr()) } == 0 {
            let rd = fds[0];
            PIPE_WR.store(fds[1], Ordering::SeqCst);
            let spawned = thread::Builder::new()
                .name("shutdown".into())
                .spawn(move || {
                    let mut byte = 0u8;
                    loop {
                        // SAFETY: 1 バイトの有効なバッファ。EINTR なら読み直す。
                        let n = unsafe { imp::read(rd, &mut byte as *mut u8 as *mut _, 1) };
                        if n == 1 {
                            break;
                        }
                        if n == 0 {
                            return;
                        }
                    }
                    log_info!(None, "stop signal received; saving state");
                    // 後始末は別スレッドで走らせ、GRACE を過ぎたら待たずに終わる
                    let done = std::sync::Arc::new(AtomicBool::new(false));
                    let d = std::sync::Arc::clone(&done);
                    let _ = thread::Builder::new()
                        .name("on-stop".into())
                        .spawn(move || {
                            on_stop();
                            d.store(true, Ordering::SeqCst);
                        });
                    let deadline = std::time::Instant::now() + GRACE;
                    while !done.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(20));
                    }
                    if !done.load(Ordering::SeqCst) {
                        log_info!(None, "state save did not finish within {:?}", GRACE);
                    }
                    truncate_and_exit();
                });
            if spawned.is_err() {
                PIPE_WR.store(-1, Ordering::SeqCst);
            }
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
        let _ = (ballast, on_stop);
    }
}
