//! スレッド 1 本ごとの標本 (`/proc/<pid>/task/<tid>/` の `stat`・`syscall`・`schedstat`。T14.3 (2)) と、
//! その読み取り。T34.61 で `proc.rs` から移した (本体は変えていない)。

use super::kept::pread_all;
use super::read_small_into;

/// スレッド 1 本の標本 (`/proc/<pid>/task/<tid>/`。T14.3 (2))。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSample {
    pub tid: u32,
    /// スレッド名 (`stat` の 2 番目の項目。**15 文字で切られる**)
    pub comm: String,
    /// utime + stime (clock tick)
    pub ticks: u64,
    /// そのうちユーザー空間の utime (clock tick。T16.0。カーネル側は `ticks - utime`)
    pub utime: u64,
    /// `stat` の状態 (`R` 走行可能 / `S` 休眠 / `D` 割り込めない休眠 …)
    pub state: char,
    /// `syscall` の番号。`Some(n >= 0)` = そのシステムコールの中、`Some(-1)` = 走行中、
    /// `None` = 読めなかった (seccomp / `hidepid` / Linux 以外)
    pub syscall: Option<i64>,
    /// **走れるのに走れなかった時間** の通算 (ns。`schedstat` の 2 番目の項目)。
    /// `None` = 読めなかった (`CONFIG_SCHEDSTATS` の無いカーネル / Linux 以外)
    pub run_delay_ns: Option<u64>,
}

/// [`scan_tasks`] の結果。
#[derive(Debug, Default)]
pub struct TaskScan {
    pub tasks: Vec<TaskSample>,
    /// `syscall` が 1 本でも読めたか (読めなければ `/profile` は `partial`)
    pub syscalls_readable: bool,
    /// `schedstat` が 1 本でも読めたか (読めなければ `/profile` の `run_delay_us` は `null`)
    pub schedstat_readable: bool,
}

/// `root` (普通は `/proc/self/task`) の下のスレッドを全部読む。
///
/// **1 本につき開くのは 3 ファイルだけ** (`stat` と `syscall` と `schedstat`)。スレッド名は
/// `stat` の 2 番目の項目にあるので `comm` は開かない (128 スレッドで 1 秒に 384 回の open)。
/// ディレクトリが読めなければ `None` (呼び出し側は `sampler: "off"`)。
///
/// `buf` は読み取りの使い回し用 (毎回確保しないため)。
pub fn scan_tasks(root: &std::path::Path, buf: &mut Vec<u8>) -> Option<TaskScan> {
    scan_tasks_reusing(root, buf, &mut TaskStatFiles::new(0), |_, _, _| {
        Known::default()
    })
}

/// スレッドごとの `stat` を開いたまま持つ置き場 (T32.21)。標本を取る側が 1 つ持つ。
///
/// 毎秒の標本はスレッド 1 本ごとに `stat` を open・read・read・close していた (冷えた状態で
/// 1 本 7 us、`profile-sample` のシステムコールの 9 割)。開いたまま `pread` 1 回で読み、
/// 消えたスレッドの分は標本のたびに閉じる。tid が使い回されても、古い記述子は死んだ
/// スレッドを指して `ESRCH` を返すので開き直す。`cap` を越えるスレッドは今までどおり開いて読む
/// (記述子をスレッドの数だけ使うので、`ulimit -n` の小さい環境で接続のぶんを食わないように)。
pub struct TaskStatFiles {
    root: std::path::PathBuf,
    cap: usize,
    pub(super) map: std::collections::HashMap<u32, std::fs::File>,
}

impl TaskStatFiles {
    pub fn new(cap: usize) -> Self {
        Self {
            root: std::path::PathBuf::new(),
            cap,
            map: std::collections::HashMap::new(),
        }
    }

    /// 開いたままにしている数。
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// `tid` の `stat` (`dir` の下) を読む。
    fn read<'a>(
        &mut self,
        tid: u32,
        dir: &std::path::Path,
        buf: &'a mut Vec<u8>,
    ) -> Option<&'a str> {
        if self.cap == 0 {
            return read_into(&dir.join("stat"), buf);
        }
        if let Some(f) = self.map.get(&tid) {
            if let Some(n) = pread_all(f, buf) {
                return std::str::from_utf8(&buf[..n]).ok();
            }
            self.map.remove(&tid);
        }
        let f = std::fs::File::open(dir.join("stat")).ok()?;
        let n = pread_all(&f, buf)?;
        if self.map.len() < self.cap {
            self.map.insert(tid, f);
        }
        std::str::from_utf8(&buf[..n]).ok()
    }
}

/// [`scan_tasks_reusing`] の呼び出し側が「もう分かっている」と返す値 (T29.21)。
/// `Some` の項目は対応するファイルを開かずにその値を使い、`None` の項目は読む。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Known {
    /// `syscall` の値 ([`TaskSample::syscall`] と同じ意味)
    pub syscall: Option<Option<i64>>,
    /// `schedstat` の値 ([`TaskSample::run_delay_ns`] と同じ意味)
    pub run_delay_ns: Option<Option<u64>>,
}

/// [`scan_tasks`] の、前回の値を使い回せる版 (T29.21)。
///
/// `stat` を先に読み、`reuse(tid, 状態, ticks)` が [`Known`] で値を返した項目は `syscall` /
/// `schedstat` を**開かずに**その値を使う (呼び出し側は「休眠のままで ticks が前回と同じ」=
/// 走っていないスレッドに前回の値を返す。暇なときはほぼ全部がこれで、1 本 3 ファイルが
/// 1 ファイルになる)。
pub fn scan_tasks_reusing(
    root: &std::path::Path,
    buf: &mut Vec<u8>,
    files: &mut TaskStatFiles,
    mut reuse: impl FnMut(u32, char, u64) -> Known,
) -> Option<TaskScan> {
    let mut out = TaskScan::default();
    // 見る場所が変わったら持っている記述子は使えない (試験が差し替える)
    if files.root != root {
        files.map.clear();
        files.root = root.to_path_buf();
    }
    for entry in std::fs::read_dir(root).ok()? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(tid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let dir = entry.path();
        let Some(text) = files.read(tid, &dir, buf) else {
            continue;
        };
        let Some((comm, state, ticks, utime)) = parse_task_stat(text) else {
            continue;
        };
        let known = reuse(tid, state, ticks);
        let syscall = known
            .syscall
            .unwrap_or_else(|| read_into(&dir.join("syscall"), buf).and_then(parse_syscall));
        let run_delay_ns = known
            .run_delay_ns
            .unwrap_or_else(|| read_into(&dir.join("schedstat"), buf).and_then(parse_schedstat));
        if syscall.is_some() {
            out.syscalls_readable = true;
        }
        if run_delay_ns.is_some() {
            out.schedstat_readable = true;
        }
        out.tasks.push(TaskSample {
            tid,
            comm,
            ticks,
            utime,
            state,
            syscall,
            run_delay_ns,
        });
    }
    // 消えたスレッドの記述子を閉じる (読めたスレッドだけ残す)
    if !files.map.is_empty() {
        let mut live: Vec<u32> = out.tasks.iter().map(|t| t.tid).collect();
        live.sort_unstable();
        files.map.retain(|tid, _| live.binary_search(tid).is_ok());
    }
    Some(out)
}

/// `path` を `buf` に読み込んで借用で返す (毎回 `String` を作らない)。
fn read_into<'a>(path: &std::path::Path, buf: &'a mut Vec<u8>) -> Option<&'a str> {
    read_small_into(path, buf)
}

/// `/proc/<pid>/task/<tid>/stat` から (名前, 状態, utime + stime, utime) を取る。
///
/// **comm は括弧で囲まれていて空白も括弧も含みうる**ので、最初の `(` と最後の `)` で切る。
/// utime を別に返すのは、ユーザー空間とカーネル側を分けて読むため (T16.0。
/// `codegen-units` の類が効くのはユーザー空間だけで、合計では差が埋もれる)。
pub(crate) fn parse_task_stat(text: &str) -> Option<(String, char, u64, u64)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    let mut it = text.get(close + 1..)?.split_whitespace();
    let state = it.next()?.chars().next()?;
    // state のあとは ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt
    // (10 個) が並び、その次が utime (項目 14) と stime (項目 15)
    let utime: u64 = it.nth(10)?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    Some((comm, state, utime + stime, utime))
}

/// `/proc/<pid>/task/<tid>/schedstat` の 1 行目から **走れるのに走れなかった時間** (ns) を取る。
///
/// 1 行は `"run_time_ns wait_time_ns nr_timeslices"` で、**2 番目**が
/// 「走行可能なのに CPU に乗れずに待たされた ns の通算」(`sched_info.run_delay`)。
/// CPU の絞り (cgroup の quota) と隣のプロセスとの取り合いは、どちらもここに出る。
pub(crate) fn parse_schedstat(text: &str) -> Option<u64> {
    text.split_whitespace().nth(1)?.parse().ok()
}

/// `/proc/<pid>/task/<tid>/syscall` の 1 行目からシステムコール番号を取る。
/// `running` と `-1` はどちらも「システムコールの中に居ない」= `Some(-1)`。
pub(crate) fn parse_syscall(text: &str) -> Option<i64> {
    let first = text.split_whitespace().next()?;
    if first == "running" {
        return Some(-1);
    }
    first.parse::<i64>().ok()
}
