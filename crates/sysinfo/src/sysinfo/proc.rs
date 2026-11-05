//! 自プロセスの数え物: スレッド数と、開いている記述子の数 / その上限 (T12.4 (3))、
//! スレッド 1 本ごとの CPU と「いまどのシステムコールに居るか」(T14.3 (2))。
//!
//! Phase 13 が「上限に届いたか」を見るための数字で、**5 秒の標本のときだけ**読む
//! (`/proc` を 1 つ読み、ディレクトリを 1 つ数え、`getrlimit` を 1 回呼ぶ。
//! 要求ごとに読むと熱い経路にシステムコールが増えるので、呼ぶ場所を増やさないこと)。
//!
//! `/proc` の無い環境では `None` を返し、履歴には 0 が入る。

// 責務ごとに子のファイルへ分けてある (T.34.61)。公開の名前はここで出し直す
// (`proxy_sysinfo::sysinfo::proc::…` のまま。metrics-profile が引く綴りも同じ)。
// - `kept`: 小さなファイルの読み方と、開いたままにして読む `KeptFiles`
// - `tasks`: スレッドごとの標本と、その読み取り・解釈
mod kept;
mod tasks;
#[cfg(test)]
use kept::keepable;
pub use kept::{KeptFiles, read_small, read_small_into};
pub use tasks::{Known, TaskSample, TaskScan, TaskStatFiles, scan_tasks, scan_tasks_reusing};
#[cfg(test)]
use tasks::{parse_schedstat, parse_syscall, parse_task_stat};

/// プロセス全体のスレッド数 (`/proc/self/status` の `Threads:`)。
///
/// `/status` の `live_threads` (接続スレッド) とは別物で、こちらは監視スレッド・
/// 履歴スレッド・Happy Eyeballs の試行スレッドまで含んだ実数。
pub fn process_threads() -> Option<u64> {
    parse_status_threads(&read_small(std::path::Path::new("/proc/self/status"))?)
}

pub(crate) fn parse_status_threads(text: &str) -> Option<u64> {
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

/// `RLIMIT_NOFILE` のソフト上限。`getrlimit` を呼ぶのは `sys::max_open_files` の 1 か所だけにした
/// (T.34.69。前はここにも同じ呼び出しがあり、cfg の条件も違っていた)。
#[cfg(target_os = "linux")]
fn max_fds() -> Option<u64> {
    crate::sys::max_open_files()
}

#[cfg(not(target_os = "linux"))]
fn max_fds() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests;
