//! 自プロセスの数え物: スレッド数と、開いている記述子の数 / その上限 (T12.4 (3))。
//!
//! Phase 13 が「上限に届いたか」を見るための数字で、**5 秒の標本のときだけ**読む
//! (`/proc` を 1 つ読み、ディレクトリを 1 つ数え、`getrlimit` を 1 回呼ぶ。
//! 要求ごとに読むと熱い経路にシステムコールが増えるので、呼ぶ場所を増やさないこと)。
//!
//! `/proc` の無い環境では `None` を返し、履歴には 0 が入る。

/// プロセス全体のスレッド数 (`/proc/self/status` の `Threads:`)。
///
/// `/status` の `live_threads` (接続スレッド) とは別物で、こちらは監視スレッド・
/// 履歴スレッド・Happy Eyeballs の試行スレッドまで含んだ実数。
pub fn process_threads() -> Option<u64> {
    parse_status_threads(&std::fs::read_to_string("/proc/self/status").ok()?)
}

pub fn parse_status_threads(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("Threads:"))
        .and_then(|v| v.trim().parse().ok())
}

/// 開いている記述子の数と上限 (`/proc/self/fd` の中身の数と `RLIMIT_NOFILE` のソフト上限)。
///
/// 数えるときに自分でも `/proc/self/fd` を 1 つ開くので、**その 1 本を引く**
/// (`ls /proc/<pid>/fd | wc -l` と同じ値になるように。あちらも同じ 1 本を数えている)。
pub fn process_fds() -> Option<(u64, u64)> {
    let n = std::fs::read_dir("/proc/self/fd").ok()?.count() as u64;
    Some((n, max_fds().unwrap_or(0)))
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn max_fds() -> Option<u64> {
    // 64bit Linux の `struct rlimit` は u64 2 つ (soft, hard)
    #[repr(C)]
    struct RLimit {
        cur: u64,
        max: u64,
    }
    const RLIMIT_NOFILE: i32 = 7;
    unsafe extern "C" {
        fn getrlimit(resource: i32, rlim: *mut RLimit) -> i32;
    }
    let mut l = RLimit { cur: 0, max: 0 };
    // SAFETY: 呼び出し先は書き込み先の大きさを `struct rlimit` (16 バイト) と見なし、
    // こちらもちょうどその大きさを渡している
    if unsafe { getrlimit(RLIMIT_NOFILE, &mut l) } != 0 {
        return None;
    }
    Some(l.cur)
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
fn max_fds() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_thread_count() {
        assert_eq!(
            parse_status_threads("Name:\tx\nThreads:\t7\nVmRSS:\t 4 kB\n"),
            Some(7)
        );
        assert_eq!(parse_status_threads("Name:\tx\n"), None);
    }

    /// 実機では数えられること (`/proc` があれば)。
    #[cfg(target_os = "linux")]
    #[test]
    fn counts_threads_and_fds_of_this_process() {
        assert!(process_threads().unwrap_or(0) >= 1);
        let (fds, max) = process_fds().unwrap();
        assert!(fds >= 3, "少なくとも stdin/stdout/stderr はある: {}", fds);
        assert!(max >= fds, "上限 {} < いま {}", max, fds);
    }
}
