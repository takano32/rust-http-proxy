//! `proc` の単体試験 (T.34.61 で `proc.rs` の `mod tests` をそのまま移した)。

use super::kept::KEPT_REOPEN_EVERY;
use super::*;

/// 読む中身は `fs::read_to_string` と同じであること (4 KiB を越える・空・UTF-8 でない・無い。T.29.20)。
#[test]
fn read_small_matches_read_to_string() {
    let dir = std::env::temp_dir().join(format!("read-small-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let big: String = (0..3000).map(|i| format!("line {i}\n")).collect();
    let cases = [
        ("big", big.as_bytes()),
        ("empty", b"".as_slice()),
        ("bad", b"\xff\xfe"),
    ];
    let mut buf = Vec::new();
    for (name, body) in cases {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let want = std::fs::read_to_string(&path).ok();
        assert_eq!(read_small(&path), want, "{name}");
        assert_eq!(
            read_small_into(&path, &mut buf).map(str::to_owned),
            want,
            "{name}"
        );
    }
    assert_eq!(read_small(&dir.join("none")), None);
    let _ = std::fs::remove_dir_all(&dir);
    #[cfg(target_os = "linux")]
    {
        let a = read_small(std::path::Path::new("/proc/self/cgroup")).unwrap();
        assert_eq!(a, std::fs::read_to_string("/proc/self/cgroup").unwrap());
    }
}

/// 開いたままのファイル (T.32.21): 消えて現れる・初めから無い・4 KiB を越える。
#[test]
fn kept_files_follow_a_file_that_vanishes_and_reappears() {
    let dir = std::env::temp_dir().join(format!("kept-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let kept = KeptFiles::new();
    let mut buf = Vec::new();
    let path = dir.join("value");
    std::fs::write(&path, "1\n").unwrap();
    assert_eq!(kept.read(&path, &mut buf), Some("1\n"));
    assert_eq!(kept.open_count(), 1);
    // 中身が変われば次の読みで見える (pread は先頭から読み直す)
    std::fs::write(&path, "22\n").unwrap();
    assert_eq!(kept.read(&path, &mut buf), Some("22\n"));
    // 消えて同じパスに作り直された: 開き直しの周期のうちに新しい中身に追いつく
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "333\n").unwrap();
    let caught_up = (0..KEPT_REOPEN_EVERY + 1).any(|_| kept.read(&path, &mut buf) == Some("333\n"));
    assert!(caught_up);
    // 消えたまま: 読めない (一度読めたパスなので、開き直しは試し続ける)
    std::fs::remove_file(&path).unwrap();
    let gone = (0..KEPT_REOPEN_EVERY + 1).all(|i| {
        let r = kept.read(&path, &mut buf).map(str::to_owned);
        // 開き直すまでは古い記述子が消えたファイルの中身を返しうる
        i < KEPT_REOPEN_EVERY || r.is_none()
    });
    assert!(gone);
    std::fs::write(&path, "4\n").unwrap();
    assert_eq!(kept.read(&path, &mut buf), Some("4\n"));
    // 初めから無かったパスは覚えておき、あとで現れても開き直さない (次の起動まで)
    let never = dir.join("never");
    assert_eq!(kept.read(&never, &mut buf), None);
    std::fs::write(&never, "x").unwrap();
    assert_eq!(kept.read(&never, &mut buf), None);
    // 4 KiB を越えるファイルも全部読める (器を倍にして続きを pread)
    let big: String = (0..3000).map(|i| format!("line {i}\n")).collect();
    let bigp = dir.join("big");
    std::fs::write(&bigp, &big).unwrap();
    for _ in 0..3 {
        assert_eq!(kept.read(&bigp, &mut buf), Some(big.as_str()));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// procfs の実物を開いたまま読んでも `fs::read_to_string` と同じ形 (T.32.21)。
/// `/proc/net/netstat` は 4 KiB を越えうる (数値は動くので、見出しの行を比べる)。
#[cfg(target_os = "linux")]
#[test]
fn kept_proc_files_read_like_read_to_string() {
    assert!(keepable(std::path::Path::new("/proc/self/status")));
    assert!(keepable(std::path::Path::new("/sys/fs/cgroup/memory.max")));
    assert!(!keepable(std::path::Path::new("/proc/self/task/1/stat")));
    assert!(!keepable(std::path::Path::new("/proc/thread-self/stat")));
    assert!(!keepable(std::path::Path::new("/tmp/x")));
    let headers =
        |t: &str| -> Vec<String> { t.lines().step_by(2).map(str::to_owned).collect::<Vec<_>>() };
    for name in ["/proc/net/netstat", "/proc/net/snmp"] {
        let path = std::path::Path::new(name);
        let Ok(want) = std::fs::read_to_string(path) else {
            continue;
        };
        for _ in 0..3 {
            let got = read_small(path).unwrap();
            assert_eq!(headers(&got), headers(&want), "{name}");
        }
    }
    let cg = std::path::Path::new("/proc/self/cgroup");
    for _ in 0..3 {
        assert_eq!(read_small(cg), std::fs::read_to_string(cg).ok());
    }
}

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

/// **`ls /proc/<pid>/fd | wc -l` と ±2 で一致すること** (T12.4 (3) の受け入れ基準)。
/// どちらも数える側が `/proc/<pid>/fd` を 1 つ開くので、その 1 本のぶんが誤差に入る。
///
/// 同じテストプロセスの他のテストが同時に fd を開け閉めする (ソケットや `/proc` の読み) ので、
/// 1 回の比較では ±2 を外れることがある (実測 1/14 程度)。それは数え方の誤りではないので、
/// 何回か測り直して 1 回でも合えばよしとする。
#[cfg(target_os = "linux")]
#[test]
fn the_fd_count_agrees_with_ls_proc_pid_fd() {
    // 数える前に 1 本開けておく (0 本の偶然の一致にならないように)
    let _keep = std::fs::File::open("/proc/self/status").unwrap();
    let mut last = (0, 0);
    for _ in 0..20 {
        let (ours, _) = process_fds().unwrap();
        let out = std::process::Command::new("ls")
            .arg(format!("/proc/{}/fd", std::process::id()))
            .output()
            .expect("ls");
        let theirs = String::from_utf8_lossy(&out.stdout).lines().count() as u64;
        if ours.abs_diff(theirs) <= 2 {
            return;
        }
        last = (ours, theirs);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!(
        "process_fds {} と ls {} が ±2 で一致しない (20 回とも)",
        last.0, last.1
    );
}

/// `stat` の comm に空白と括弧が入っていても名前・状態・CPU が取れること (T14.3 (2))。
#[test]
fn parses_a_task_stat_line() {
    let line = "1234 (we (ir)d name) S 1 1 0 0 -1 4194368 0 0 0 0 111 222 0 0 20 0 8 0 100 0";
    let (comm, state, ticks, utime) = parse_task_stat(line).expect("読めること");
    assert_eq!(comm, "we (ir)d name");
    assert_eq!(state, 'S');
    assert_eq!(ticks, 333);
    // ユーザー空間は utime (項目 14) だけ (T16.0)
    assert_eq!(utime, 111);
    assert_eq!(parse_task_stat("こわれている"), None);
}

/// T.34.61: カーネルが 15 文字で切った comm (`TASK_COMM_LEN` - 1) はそのまま返す (末尾を足さない)。
#[test]
fn a_task_stat_with_a_truncated_comm_keeps_the_15_characters() {
    let line = "77 (proxy-history-w) R 1 1 0 0 -1 4194368 0 0 0 0 5 7 0 0 20 0 8 0 100 0";
    let (comm, state, ticks, utime) = parse_task_stat(line).expect("読めること");
    assert_eq!(comm, "proxy-history-w");
    assert_eq!(comm.len(), 15);
    assert_eq!((state, ticks, utime), ('R', 12, 5));
    // 空の comm も読める
    let (comm, ..) = parse_task_stat("8 () S 1 1 0 0 -1 0 0 0 0 0 1 2").expect("空の名前");
    assert_eq!(comm, "");
}

/// T.34.61: 項目が足りない行 (stime まで届かない・状態が無い・数でない) は `None`。
#[test]
fn a_task_stat_without_enough_fields_is_none() {
    // utime (項目 14) までで stime が無い
    assert_eq!(parse_task_stat("1 (x) S 1 1 0 0 -1 0 0 0 0 0 9"), None);
    // utime の手前で切れている
    assert_eq!(parse_task_stat("1 (x) S 1 1 0 0"), None);
    // 閉じ括弧のあとが空 (状態も無い)
    assert_eq!(parse_task_stat("1 (x)"), None);
    // utime が数でない
    assert_eq!(parse_task_stat("1 (x) S 1 1 0 0 -1 0 0 0 0 0 a 2"), None);
    // 括弧の順が逆
    assert_eq!(parse_task_stat("1 )x( S 1 1 0 0 -1 0 0 0 0 0 1 2"), None);
}

#[test]
fn parses_the_syscall_file() {
    assert_eq!(parse_syscall("running\n"), Some(-1));
    assert_eq!(parse_syscall("-1 0x0 0x0\n"), Some(-1));
    assert_eq!(
        parse_syscall("73 0xffffc32a1e18 0x2 0xffffc32a1d90 0x0 0x0 0x0 0xffff 0xffff\n"),
        Some(73)
    );
    assert_eq!(parse_syscall(""), None);
}

/// `schedstat` の **2 番目** (走れるのに待たされた ns) を取ること (T15.0 (5))。
#[test]
fn parses_the_schedstat_file() {
    assert_eq!(parse_schedstat("123456 7890 42\n"), Some(7890));
    assert_eq!(parse_schedstat("0 0 0\n"), Some(0));
    // 項目が足りない / 数でないものは読めなかった扱い
    assert_eq!(parse_schedstat("123456\n"), None);
    assert_eq!(parse_schedstat(""), None);
    assert_eq!(parse_schedstat("123456 x 42\n"), None);
}

/// `/proc` が無いところを指したら `None` (`/profile` は `sampler: "off"`)。
#[test]
fn a_missing_task_directory_gives_none() {
    let mut buf = Vec::new();
    assert!(scan_tasks(std::path::Path::new("/nonexistent/task"), &mut buf).is_none());
}

/// `syscall` の無いディレクトリを差し替えると `partial` になること (T14.3 の受け入れ基準)。
#[test]
fn a_task_directory_without_syscall_is_partial() {
    let dir = std::env::temp_dir().join(format!("t143-task-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("7")).expect("作れること");
    std::fs::write(
        dir.join("7/stat"),
        "7 (conn) S 1 1 0 0 -1 4194368 0 0 0 0 5 6 0 0 20 0 8 0 100 0\n",
    )
    .expect("書けること");
    let mut buf = Vec::new();
    let scan = scan_tasks(&dir, &mut buf).expect("ディレクトリは読める");
    assert_eq!(scan.tasks.len(), 1);
    assert_eq!(scan.tasks[0].comm, "conn");
    assert_eq!(scan.tasks[0].ticks, 11);
    assert_eq!(scan.tasks[0].utime, 5, "utime は項目 14 だけ (T16.0)");
    assert_eq!(scan.tasks[0].syscall, None, "syscall が無ければ None");
    assert!(!scan.syscalls_readable, "partial に落ちること");
    // `schedstat` も無いので `run_delay_us` は `null` になる側 (T15.0 (5))
    assert_eq!(scan.tasks[0].run_delay_ns, None);
    assert!(!scan.schedstat_readable);
    // 置いてやれば読める
    std::fs::write(dir.join("7/schedstat"), "100 250 3\n").expect("書けること");
    let scan = scan_tasks(&dir, &mut buf).expect("ディレクトリは読める");
    assert_eq!(scan.tasks[0].run_delay_ns, Some(250));
    assert!(scan.schedstat_readable);
    let _ = std::fs::remove_dir_all(&dir);
}

/// スレッドの `stat` を開いたまま読む (T.32.21): 消えたスレッドの記述子は閉じ、
/// 生まれたスレッドは読み、読んだ値は開いて読むのと同じ。
#[cfg(target_os = "linux")]
#[test]
fn kept_task_files_follow_threads_that_come_and_go() {
    let root = std::path::Path::new("/proc/self/task");
    let mut files = TaskStatFiles::new(1024);
    let mut buf = Vec::new();
    let none = |_, _, _| Known::default();
    let before = scan_tasks_reusing(root, &mut buf, &mut files, none).unwrap();
    assert_eq!(files.len(), before.tasks.len());
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let h = std::thread::Builder::new()
        .name("kept-probe".into())
        .spawn(move || rx.recv().ok())
        .unwrap();
    // 名前は生まれたスレッドが自分で付けるので、見えるまで少し待つ
    let probe_tid = (0..200)
        .find_map(|_| {
            let during = scan_tasks_reusing(root, &mut buf, &mut files, none).unwrap();
            let tid = during
                .tasks
                .iter()
                .find(|t| t.comm == "kept-probe")
                .map(|t| t.tid);
            if tid.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            tid
        })
        .expect("生まれたスレッドが見える");
    assert!(files.map.contains_key(&probe_tid));
    // 開いて読むのと同じ名前と状態
    let fresh = scan_tasks(root, &mut buf).unwrap();
    let f = fresh.tasks.iter().find(|t| t.tid == probe_tid).unwrap();
    assert_eq!(f.comm, "kept-probe");
    tx.send(()).unwrap();
    h.join().unwrap();
    let after = scan_tasks_reusing(root, &mut buf, &mut files, none).unwrap();
    assert!(after.tasks.iter().all(|t| t.tid != probe_tid));
    assert!(
        !files.map.contains_key(&probe_tid),
        "消えたスレッドの記述子は閉じる"
    );
    // 上限 0 は開いたままにしない
    let mut none_kept = TaskStatFiles::new(0);
    scan_tasks_reusing(root, &mut buf, &mut none_kept, none).unwrap();
    assert!(none_kept.is_empty());
}

/// 実機では自分のスレッドが読めること (`/proc` があれば)。
#[cfg(target_os = "linux")]
#[test]
fn scans_this_process_tasks() {
    let mut buf = Vec::new();
    let scan = scan_tasks(std::path::Path::new("/proc/self/task"), &mut buf).expect("読める");
    assert!(!scan.tasks.is_empty());
    let me = std::process::id();
    assert!(scan.tasks.iter().any(|t| t.tid == me), "主スレッドが居る");
}
