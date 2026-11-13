//! 自プロセスの数え物と識別: スレッドの番号 (`gettid`)、開ける記述子の数 (`RLIMIT_NOFILE`)、
//! カーネルの起動ごとの識別子 (`boot_id`)。

use std::io;

unsafe extern "C" {
    /// `gettid(2)`。このスレッドの番号 (`/proc/self/task/<tid>` の名前と同じ値)。
    /// 下の [`gettid`] と名前がぶつかるので、宣言だけ別名にしてある
    #[link_name = "gettid"]
    fn c_gettid() -> i32;
}

thread_local! {
    /// このスレッドの番号。**スレッドの一生に 1 回だけ**システムコールを引く (T15.0)。
    static TID: u32 = unsafe { c_gettid() } as u32;
}

/// このスレッドの番号 (`gettid(2)`。`/proc/self/task/<tid>` の名前と同じ値。T15.0)。
///
/// **システムコールを引くのはスレッドごとに 1 回だけ**で、2 回目からはスレッドローカルの
/// 読み出し 1 回。`/connections` の `tid` に出して「どの接続をどのスレッドが受け持って
/// いるか」を結ぶために使う (`/profile` の `threads_top` の `tid` と同じ番号)。
/// スレッドの後始末の最中 (スレッドローカルが落ちたあと) は `0` = 「分からない」。
pub fn gettid() -> u32 {
    TID.try_with(|t| *t).unwrap_or(0)
}

/// `getrlimit(2)` / `setrlimit(2)` の定数と `struct rlimit`。
///
/// `RLIMIT_NOFILE` の番号 (7) は asm-generic の値で、mips / sparc では違う。`rlim_t` の幅も
/// libc 次第 (64 ビット環境の glibc / musl はどちらも 64 ビット、32 ビットの musl は 64 ビットだが
/// 32 ビットの glibc は 32 ビット) で、食い違うと呼び出し側のスタックを壊す。
/// [`super::inherit_socket_options`] と同じく **aarch64 / x86_64 だけ**で有効にし、それ以外では
/// 「分からない」を返す (呼び出し側は固定の既定値に落ちる)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod rlimit {
    use std::ffi::c_int;

    pub const RLIMIT_NOFILE: c_int = 7;

    /// `struct rlimit` (64 ビットの `rlim_t` 2 つ)。
    #[repr(C)]
    pub struct RLimit {
        pub cur: u64,
        pub max: u64,
    }

    unsafe extern "C" {
        pub fn getrlimit(resource: c_int, rlim: *mut RLimit) -> c_int;
        pub fn setrlimit(resource: c_int, rlim: *const RLimit) -> c_int;
    }
}

/// このプロセスが開ける記述子の数 (`RLIMIT_NOFILE` の soft limit)。
/// 分からなければ `None` (呼び出し側は固定の既定値に落ちる)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn max_open_files() -> Option<u64> {
    let mut lim = rlimit::RLimit { cur: 0, max: 0 };
    // SAFETY: lim はこの呼び出しの間だけ有効なら良い書込先。失敗は -1 で返る。
    if unsafe { rlimit::getrlimit(rlimit::RLIMIT_NOFILE, &mut lim) } < 0 {
        return None;
    }
    Some(lim.cur)
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn max_open_files() -> Option<u64> {
    None
}

/// `RLIMIT_NOFILE` の soft limit を下げる (hard limit はそのまま)。
/// **記述子の少ない環境を再現するためのもの** (`tests/maxconns_test.rs`)。プロセス全体に効くので、
/// 呼ぶのは「そのテストだけが動いているテストバイナリ」に限ること。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
pub fn set_max_open_files(soft: u64) -> io::Result<()> {
    let mut lim = rlimit::RLimit { cur: 0, max: 0 };
    // SAFETY: 上と同じ。今の hard limit を読んでから、それを超えない値に下げる。
    if unsafe { rlimit::getrlimit(rlimit::RLIMIT_NOFILE, &mut lim) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let next = rlimit::RLimit {
        cur: soft.min(lim.max),
        max: lim.max,
    };
    // SAFETY: next は呼び出しの間だけ有効なら良い (カーネルが値を読むだけ)。
    if unsafe { rlimit::setrlimit(rlimit::RLIMIT_NOFILE, &next) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub fn set_max_open_files(_soft: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "RLIMIT_NOFILE is only wired up for aarch64 and x86_64",
    ))
}

/// カーネルの起動ごとに変わる識別子 (`/proc/sys/kernel/random/boot_id`、36 文字の UUID。T24.0)。
///
/// **ホストの再起動と、プロセスだけの再起動を切り分ける**ための口。ホスト (カーネル) が
/// 起動し直すと値が変わり、プロセスを起動し直しただけなら変わらない。読めない環境
/// (procfs が無い・権限が無い) は `None` を返し、呼ぶ側は `null` を出す (**落とさない**)。
pub fn boot_id() -> Option<String> {
    parse_boot_id(&std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?)
}

/// `boot_id` のファイルの中身を読む (前後の空白と改行を落とし、空なら `None`)。
pub(super) fn parse_boot_id(text: &str) -> Option<String> {
    let id = text.trim();
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_id_is_trimmed_and_empty_is_none() {
        assert_eq!(
            parse_boot_id("5c3e2d1a-0f4b-4c7e-9a2d-1b3c4d5e6f70\n").as_deref(),
            Some("5c3e2d1a-0f4b-4c7e-9a2d-1b3c4d5e6f70")
        );
        assert_eq!(parse_boot_id("\n"), None);
        assert_eq!(parse_boot_id(""), None);
    }
}
