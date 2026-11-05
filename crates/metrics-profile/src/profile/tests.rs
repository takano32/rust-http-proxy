use super::procstat::*;
use super::rings::MAX_UNKNOWN;
use super::sample::*;
use super::threads::*;
use super::*;
use crate::metrics::{Detail, StageMs};

fn detail(stages: StageMs, dns: u64, connect: u64, ttfb: Option<u64>) -> Detail {
    Detail {
        dns_ms: dns,
        connect_ms: connect,
        first_byte_ms: ttfb,
        stages,
        ..Detail::default()
    }
}

#[test]
fn a_connect_lands_in_all_seven_stages() {
    let mut s = Stages::default();
    s.observe_connect(&detail(
        StageMs {
            queue: 1,
            client_read: 2,
            first_relay: 30,
            relay: 400,
            park: 5000,
            ..StageMs::default()
        },
        6,
        9,
        None,
    ));
    let got: Vec<u64> = s.folded_connect().iter().map(|w| w.ms_sum).collect();
    assert_eq!(got, vec![1, 2, 6, 9, 30, 400, 5000]);
    assert!(s.folded_connect().iter().all(|w| w.count == 1));
    assert!(s.folded_forward().iter().all(|w| w.count == 0));
}

#[test]
fn a_forward_request_lands_in_all_six_stages() {
    let mut s = Stages::default();
    s.observe_forward(&detail(
        StageMs {
            queue: 1,
            client_read: 2,
            send: 3,
            body: 40,
            ..StageMs::default()
        },
        6,
        9,
        Some(20),
    ));
    let got: Vec<u64> = s.folded_forward().iter().map(|w| w.ms_sum).collect();
    // origin = dns + connect
    assert_eq!(got, vec![1, 2, 15, 3, 20, 40]);
}

/// 粗い窓へは足し合わせる (件数も区間も)。
#[test]
fn merging_adds_up() {
    let mut a = Stages::default();
    let mut b = Stages::default();
    a.observe_connect(&detail(StageMs::default(), 3, 0, None));
    b.observe_connect(&detail(StageMs::default(), 7, 0, None));
    a.merge(&b);
    assert_eq!(a.folded_connect()[2].count, 2);
    assert_eq!(a.folded_connect()[2].ms_sum, 10);
    assert_eq!(a.folded_connect()[2].ms_max, 7);
    // 0 ms の段階も件数だけは残り、`≤ 1 ms` の区間に入っている
    assert_eq!(a.folded_connect()[0].count, 2);
    assert_eq!(a.folded_connect()[0].buckets[0], 2);
    assert_eq!(a.folded_connect()[0].ms_sum, 0);
}

/// 1 分の窓は 5 秒の標本 12 本から作られ、環状バッファは上限で古いものを捨てる。
#[test]
fn the_minute_window_is_built_from_the_five_second_ones() {
    let p = Profile::default();
    let base = 1_800_000_000u64; // 分の頭 (60 で割り切れる)
    for i in 0..12 {
        let mut st = Stages::default();
        st.observe_connect(&detail(StageMs::default(), 10, 0, None));
        p.push(Sample {
            t: base + i * 5,
            requests: 2,
            cpu_us: 100,
            stages: st,
            ..Sample::default()
        });
    }
    // まだ次の分に入っていないので 1 分の窓はできない
    assert_eq!(p.len(1), 0);
    p.push(Sample {
        t: base + 60,
        ..Sample::default()
    });
    assert_eq!(p.len(1), 1);
    let minute = p.recent_totals(1, 10).stages;
    assert_eq!(minute.folded_connect()[2].count, 12);
    assert_eq!(minute.folded_connect()[2].ms_sum, 120);
}

/// 上限を超えたら古い標本から捨てる。
#[test]
fn the_ring_keeps_only_the_capacity() {
    let p = Profile::default();
    for i in 0..(RESOLUTIONS[0].1 as u64 + 10) {
        p.push(Sample {
            t: 1_000_000 + i * 5,
            ..Sample::default()
        });
    }
    assert_eq!(p.len(0), RESOLUTIONS[0].1);
}

/// 件数 0 の段階は `0` 1 文字で書く (静かな窓を小さくする)。
#[test]
fn empty_stages_are_written_as_a_single_zero() {
    let mut row = String::new();
    Sample {
        t: 7,
        ..Sample::default()
    }
    .push_row(&mut row);
    // 上位のスレッドが 1 本も無い窓も `0` 1 文字、`schedstat` が読めなければ `null`
    assert_eq!(
        row,
        "[7,0,0,[0,0,0,0,0,0,0],[0,0,0,0,0,0],[0,0,0,0,0,0,0,0,0],[0,0,0,0],[0,0,0],0,null,[0,0,0,0,0,0,0,0,0],0]"
    );
}

/// 予算に入らない古い標本は落とし、`truncated` を立てる。
#[test]
fn rows_are_cut_to_the_budget_newest_first() {
    let p = Profile::default();
    for i in 0..50u64 {
        p.push(Sample {
            t: 1_000_000 + i * 5,
            requests: i,
            ..Sample::default()
        });
    }
    let (all, shown, total, cut) = p.rows_within(0, 1 << 20);
    assert_eq!(total, 50);
    assert_eq!(shown, 50);
    assert!(!cut);
    assert!(all.starts_with("[1000000,0,"), "{}", &all[..24]);
    let (small, shown2, _, cut2) = p.rows_within(0, 120);
    assert!(shown2 < 50 && shown2 > 0, "書けた件数: {}", shown2);
    assert!(cut2, "予算に入り切らなければ打ち切る");
    assert!(small.len() <= 120);
    // 残るのは**新しい方**
    assert!(small.contains("[1000245,49,"), "{}", small);
}

/// `rows_within_page` は「新しい方から `offset` 本飛ばして `n` 本」(T15.0 (11))。
///
/// 環は時刻で引けないので頁は本数で切る。縛るのは 3 つ: **返す並びは古い順**のまま、
/// 頁を継いでいくと**全部が重複なく揃う**、`n` で切れたぶんは打ち切りではない
/// (バイト数で切れたときだけ `cut`)。
#[test]
fn rows_can_be_paged_from_the_newest_end() {
    let p = Profile::default();
    for i in 0..50u64 {
        p.push(Sample {
            t: 1_000_000 + i * 5,
            requests: i,
            ..Sample::default()
        });
    }
    // 1 頁目 = いちばん新しい 10 本 (t は 1000200..1000245)。並びは古い順
    let (page1, shown, total, cut) = p.rows_within_page(0, 1 << 20, 10, 0);
    assert_eq!(
        (shown, total, cut),
        (10, 50, false),
        "n で切るのは打ち切りではない"
    );
    assert!(page1.starts_with("[1000200,40,"), "{}", &page1[..24]);
    assert!(page1.contains("[1000245,49,"), "{}", page1);
    assert!(
        !page1.contains("[1000195,39,"),
        "11 本目が入っている: {}",
        page1
    );

    // 2 頁目 = その次の 10 本
    let (page2, shown2, _, _) = p.rows_within_page(0, 1 << 20, 10, 10);
    assert_eq!(shown2, 10);
    assert!(page2.starts_with("[1000150,30,"), "{}", &page2[..24]);
    assert!(page2.contains("[1000195,39,"), "{}", page2);

    // 5 頁で 50 本が重複なく揃う (1 標本の目印は先頭の `[t,requests,`。
    // 入れ子の配列は 0 埋めの窓なので、この綴りとは当たらない)
    let mut seen: Vec<u64> = Vec::new();
    for page in 0..5 {
        let (rows, n, _, _) = p.rows_within_page(0, 1 << 20, 10, page * 10);
        assert_eq!(n, 10, "{} 頁目", page + 1);
        for i in 0..50u64 {
            if rows.contains(&format!("[{},{},", 1_000_000 + i * 5, i)) {
                seen.push(i);
            }
        }
    }
    assert_eq!(seen.len(), 50, "同じ標本が 2 つの頁に出た: {:?}", seen);
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 50, "欠けがある");

    // 環の外を指したら空 (エラーにはしない)
    let (empty, shown3, total3, cut3) = p.rows_within_page(0, 1 << 20, 10, 50);
    assert_eq!((empty.as_str(), shown3, total3, cut3), ("", 0, 50, false));

    // 予算はページ 1 枚に効く (飛ばしたぶんは数えない)
    let (tight, shown4, _, cut4) = p.rows_within_page(0, 120, 10, 10);
    assert!(shown4 < 10 && shown4 > 0, "書けた件数: {}", shown4);
    assert!(cut4, "バイト数で切れたら打ち切り");
    assert!(tight.contains("[1000195,39,"), "{}", tight);

    // `rows_within` は今までどおり (= 頁を切らない呼び方と同じ)
    let (all, shown5, _, _) = p.rows_within(0, 1 << 20);
    let (same, shown6, _, _) = p.rows_within_page(0, 1 << 20, usize::MAX, 0);
    assert_eq!((all, shown5), (same, shown6));
}

/// `/proc/<pid>/stat` の comm に空白と括弧が入っていても utime / stime を読める。
#[test]
fn the_stat_line_is_parsed_after_the_last_paren() {
    let mut line = String::from("42 (weird ) name) S 1 42 42 0 -1 4194304 0 0 0 0 ");
    line.push_str("111 222 0 0 20 0 8 0 100 0 0");
    let us = parse_stat_cpu_us(&line).expect("読めること");
    assert_eq!(us, (111 + 222) * (1_000_000 / clock_tick()));
    // ユーザー空間は utime (項目 14) だけ。合計は `parse_stat_cpu_us` と同じ (T16.0)
    let (total, user) = parse_stat_cpu_split_us(&line).expect("読めること");
    assert_eq!(total, us);
    assert_eq!(user, 111 * (1_000_000 / clock_tick()));
    assert_eq!(parse_stat_cpu_split_us("42 (x) S 1"), None);
    assert_eq!(parse_stat_cpu_split_us("括弧が無い"), None);
}

#[cfg(target_os = "linux")]
#[test]
fn this_process_has_used_some_cpu() {
    assert!(process_cpu_us().is_some());
}

/// 役割は主スレッド (tid == pid) とスレッド名で決まり、知らない名前は `other`。
#[test]
fn roles_come_from_the_thread_name() {
    assert_eq!(ROLES[role_of(7, 7, "rust-http-proxy")], "accept");
    assert_eq!(ROLES[role_of(9, 7, "conn")], "conn");
    assert_eq!(ROLES[role_of(9, 7, "idle-watch")], "idle-watch");
    assert_eq!(ROLES[role_of(9, 7, "profile-sample")], "profile-sample");
    assert_eq!(ROLES[role_of(9, 7, "env-reload")], "other");
    // 主スレッドの名前は実行ファイル名なので、名前より tid を先に見る
    assert_eq!(ROLES[role_of(7, 7, "conn")], "accept");
}

/// 状態は「走行中 / システムコール名 / 休眠 / その他」に落ちる。
#[test]
fn states_map_to_syscall_names() {
    let names = state_names();
    assert_eq!(names[state_slot(Some(-1), 'R').0], "running");
    assert_eq!(names[state_slot(None, 'R').0], "running");
    assert_eq!(names[state_slot(None, 'S').0], "sleeping");
    assert_eq!(names[state_slot(None, 'Z').0], "other");
    // 表にある番号は名前で、無い番号は `other` + 番号を返す
    let nr = SYSCALL_NRS[2]; // ppoll
    assert_eq!(names[state_slot(Some(nr), 'S').0], "ppoll");
    assert_eq!(state_slot(Some(99_999), 'S'), (NSTATES - 1, Some(99_999)));
    assert_eq!(names.len(), NSTATES);
    assert_eq!(names[1], "recvfrom");
}

/// **T.29.14**: x86_64 の `poll` (7) と `epoll_wait` (232) は `ppoll` / `epoll_pwait` の枠に数え、
/// `unknown_syscalls` に落ちない (状態の数は変えない)。
#[cfg(target_arch = "x86_64")]
#[test]
fn x86_64_poll_and_epoll_wait_are_named() {
    let names = state_names();
    assert_eq!(names[state_slot(Some(7), 'S').0], "ppoll");
    assert_eq!(state_slot(Some(7), 'S').1, None);
    assert_eq!(names[state_slot(Some(232), 'S').0], "epoll_pwait");
    assert_eq!(state_slot(Some(232), 'S').1, None);
    // 枠の位置は名前の表と合っている
    for (nr, i) in SYSCALL_ALIASES {
        assert!(!SYSCALL_NRS.contains(&nr), "{} は表と重なる", nr);
        assert_eq!(names[1 + i], ["ppoll", "epoll_pwait"][(nr == 232) as usize]);
    }
}

/// 番号の表は名前と同じ長さで、同じ番号が 2 つ無いこと。
#[test]
fn the_syscall_table_is_consistent() {
    assert_eq!(SYSCALL_NRS.len(), SYSCALL_NAMES.len());
    for (i, a) in SYSCALL_NRS.iter().enumerate() {
        for b in SYSCALL_NRS.iter().skip(i + 1) {
            assert_ne!(a, b, "番号が重複している: {}", a);
        }
    }
}

/// 空の窓は `threads_top` が `0` 1 文字、`run_delay_us` は配列のまま。
#[test]
fn an_empty_window_writes_a_bare_zero() {
    let mut row = String::new();
    Sample {
        run_delay_us: Some([0; ROLES.len()]),
        ..Sample::default()
    }
    .push_row(&mut row);
    assert!(
        row.ends_with(",0,[0,0,0,0,0,0,0,0,0],[0,0,0,0,0,0,0,0,0],0]"),
        "{}",
        row
    );
}

/// 60 秒へ畳むとき、上位のスレッドは **tid を鍵に**足して上位 8 に切り直す。
#[test]
fn the_top_threads_merge_by_tid() {
    let t = |tid: u32, cpu_us: u64| TopThread {
        tid,
        cpu_us,
        running: 1,
        role: 1,
        comm: TopThread::comm_bytes("conn"),
    };
    let mut a = Sample {
        threads_top: [
            t(1, 10),
            t(2, 5),
            t(3, 4),
            t(4, 3),
            t(5, 2),
            t(6, 1),
            t(7, 1),
            t(8, 1),
        ],
        run_delay_us: Some([1; ROLES.len()]),
        ..Sample::default()
    };
    let b = Sample {
        threads_top: [
            t(9, 9),
            t(2, 100),
            t(3, 1),
            t(4, 1),
            t(5, 1),
            t(6, 1),
            t(7, 1),
            t(8, 1),
        ],
        run_delay_us: Some([2; ROLES.len()]),
        ..Sample::default()
    };
    a.merge_into(&b);
    // tid 2 は 5 + 100 = 105 で首位、同じ tid が 2 度出ない
    assert_eq!(a.threads_top[0].tid, 2);
    assert_eq!(a.threads_top[0].cpu_us, 105);
    assert_eq!(a.threads_top[0].running, 2);
    assert_eq!(a.threads_top[1].tid, 1);
    let mut tids: Vec<u32> = a.threads_top.iter().map(|x| x.tid).collect();
    tids.sort_unstable();
    tids.dedup();
    assert_eq!(tids.len(), TOP_THREADS, "8 本とも別のスレッド");
    assert_eq!(a.run_delay_us, Some([3; ROLES.len()]));
    // 片方しか読めていない窓は読めた方をそのまま残す
    let mut c = Sample::default();
    c.merge_into(&b);
    assert_eq!(c.run_delay_us, Some([2; ROLES.len()]));
}

#[test]
fn parses_the_thread_count_and_ticks_of_proc_self_stat() {
    let line = "42 (a b) c) S 1 42 42 0 -1 4194560 100 0 0 0 7 5 0 0 20 0 13 0 100 0\n";
    assert_eq!(parse_stat_threads_ticks(line), Some((13, 12)));
    assert_eq!(parse_stat_threads_ticks("42 (x) S 1"), None);
}

/// 1 標本の大きさ (窓のメモリ = これ × (720 + 1,440))。
///
/// **数字を doc に書き写さない**ため、知りたいときはこのテストを `-- --nocapture` で
/// 回す。縛るのは上限だけ (増えたことに気づけるように)。
#[test]
fn a_sample_stays_small_enough_for_the_rings() {
    let bytes = std::mem::size_of::<Sample>();
    eprintln!(
        "size_of::<Sample>() = {} B、満杯の窓 = {} B ({:.2} MiB)",
        bytes,
        capacity_bytes(),
        capacity_bytes() as f64 / 1_048_576.0
    );
    assert!(bytes <= 4096, "1 標本が大きすぎる: {} B", bytes);
    assert_eq!(sample_bytes(), bytes);
    assert_eq!(
        capacity_bytes(),
        (RESOLUTIONS[0].1 + RESOLUTIONS[1].1) * bytes
    );
}

/// `used_bytes` は**いま入っているぶん**なので、標本を積むと増える (T15.0 (13))。
///
/// 「起動直後と 1 時間後で増える」は待てないので、`push` を 2 回して見る。
/// 満杯の見積もり ([`capacity_bytes`]) は動かない。
#[test]
fn used_bytes_grows_with_the_samples_and_never_passes_the_capacity() {
    let p = Profile::default();
    assert_eq!(p.used_bytes(), 0, "`--lite` と起動直後は 1 本も無い");
    p.push(Sample {
        t: 1_700_000_000,
        requests: 1,
        ..Sample::default()
    });
    let one = p.used_bytes();
    assert_eq!(one, sample_bytes(), "5 秒の環に 1 本");
    p.push(Sample {
        t: 1_700_000_005,
        requests: 2,
        ..Sample::default()
    });
    assert!(p.used_bytes() > one, "{} -> {}", one, p.used_bytes());
    assert!(
        p.used_bytes() <= capacity_bytes(),
        "{} > {}",
        p.used_bytes(),
        capacity_bytes()
    );
}

/// 表に無いシステムコール番号は `sys_N` 用に控える (上限 [`MAX_UNKNOWN`] 種)。
#[test]
fn unknown_syscall_numbers_are_kept() {
    let p = Profile::default();
    for _ in 0..3 {
        p.note_unknown_syscall(999);
    }
    p.note_unknown_syscall(998);
    for i in 0..MAX_UNKNOWN as i64 {
        p.note_unknown_syscall(500 + i);
    }
    let v = p.unknown_syscalls();
    assert_eq!(v[0], (999, 3));
    assert_eq!(v.len(), MAX_UNKNOWN);
}

/// 窓の並びは件数 0 を `0`、それ以外を `[count,sum,max,[13 段の区間]]` で書く。
#[test]
fn push_windows_writes_zero_for_an_empty_window_and_the_buckets_otherwise() {
    let mut w = crate::window::Window::default();
    w.observe(3);
    let mut out = String::new();
    push_windows(&mut out, &[crate::window::Window::default(), w]);
    assert_eq!(out, "0,[1,3,3,[0,0,1,0,0,0,0,0,0,0,0,0,0]]");
    let mut empty = String::new();
    push_windows(&mut empty, &[]);
    assert_eq!(empty, "");
}

/// `merge_top` は空の枠 (`tid == 0`) を飛ばし、同じ tid は足して新しい方の名前と役割を採る。
#[test]
fn merge_top_adds_by_tid_and_keeps_the_newer_name() {
    let t = |tid: u32, cpu_us: u64, comm: &str, role: u8| TopThread {
        tid,
        cpu_us,
        running: 1,
        role,
        comm: TopThread::comm_bytes(comm),
    };
    let mut a = [TopThread::default(); TOP_THREADS];
    let mut b = [TopThread::default(); TOP_THREADS];
    a[0] = t(5, 10, "parent", 0);
    a[1] = t(6, 3, "conn", 1);
    b[0] = t(5, 1, "child", 2);
    b[1] = t(7, 20, "conn", 1);
    let m = merge_top(&a, &b);
    assert_eq!((m[0].tid, m[0].cpu_us), (7, 20));
    assert_eq!((m[1].tid, m[1].cpu_us, m[1].running), (5, 11, 2));
    assert_eq!(m[1].comm_str(), "child", "名前は新しい方");
    assert_eq!(m[1].role, 2, "役割も新しい方");
    assert_eq!((m[2].tid, m[2].cpu_us), (6, 3));
    assert!(m[3..].iter().all(|x| *x == TopThread::default()));
    // CPU が同じなら tid の小さい順
    let mut c = [TopThread::default(); TOP_THREADS];
    c[0] = t(9, 4, "x", 0);
    c[1] = t(8, 4, "y", 0);
    let m = merge_top(&c, &[TopThread::default(); TOP_THREADS]);
    assert_eq!((m[0].tid, m[1].tid), (8, 9));
}

/// 状態の名前は `running`・システムコール名 (表の順)・`sleeping`・`other` の順に並ぶ。
#[test]
fn state_names_are_running_then_syscalls_then_sleeping_and_other() {
    let names = state_names();
    assert_eq!(names.len(), NSTATES);
    assert_eq!(names[0], "running");
    assert_eq!(&names[1..1 + SYSCALL_NAMES.len()], &SYSCALL_NAMES[..]);
    assert_eq!(names[NSTATES - 2], "sleeping");
    assert_eq!(names[NSTATES - 1], "other");
}
