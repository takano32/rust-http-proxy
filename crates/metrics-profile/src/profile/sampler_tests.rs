use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::profile::procstat::clock_tick;
use crate::profile::sample::Sample;
use crate::profile::threads::{SYSCALL_NRS, state_names};

/// 差し替えたディレクトリから取ると、役割ごとに CPU と状態が積まれる。
/// 2 回目は**増分だけ**が積まれること (累計をそのまま足さない)。
#[test]
fn the_sampler_accumulates_per_role_deltas() {
    let dir = std::env::temp_dir().join(format!("t143-sampler-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // `schedstat` は `"run_time_ns wait_time_ns nr_timeslices"` の 1 行 (`None` で置かない)
    let write = |tid: u32, comm: &str, ticks: u64, syscall: Option<&str>, delay_ns: Option<u64>| {
        let d = dir.join(tid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("stat"),
            format!(
                "{} ({}) S 1 1 0 0 -1 0 0 0 0 0 {} 0 0 0 20 0 8 0 100 0\n",
                tid, comm, ticks
            ),
        )
        .unwrap();
        match syscall {
            Some(v) => std::fs::write(d.join("syscall"), v).unwrap(),
            None => {
                let _ = std::fs::remove_file(d.join("syscall"));
            }
        }
        match delay_ns {
            Some(ns) => std::fs::write(d.join("schedstat"), format!("999 {} 3\n", ns)).unwrap(),
            None => {
                let _ = std::fs::remove_file(d.join("schedstat"));
            }
        }
    };
    write(1, "proxy", 10, Some("running\n"), Some(1_000));
    write(
        2,
        "conn",
        20,
        Some(format!("{} 0x0\n", SYSCALL_NRS[5]).as_str()),
        Some(5_000),
    );
    let p = Profile::default();
    let mut s = Sampler::new(dir.clone(), 1);
    s.sample(&p);
    assert_eq!(p.sampler(), SamplerState::On);
    let names = state_names();
    let accept = s.pending[0];
    let conn = s.pending[1];
    assert_eq!(accept.samples, 1);
    assert_eq!(
        accept.states[names.iter().position(|n| *n == "running").unwrap()],
        1
    );
    assert_eq!(
        conn.states[names.iter().position(|n| *n == "splice").unwrap()],
        1
    );
    let first = conn.cpu_us;
    assert!(first > 0, "はじめて見たスレッドは累計がそのまま増分");
    // 走れずに待った時間も「はじめて見たスレッド」は累計がそのまま増分。
    // **溜めは ns のまま** (us へ落とすのは `take` で 1 度だけ)
    assert_eq!(s.pending_run_delay[0], 1_000);
    assert_eq!(s.pending_run_delay[1], 5_000);
    // 2 回目: 5 tick だけ進め、待ち時間も 3,000 ns 進める
    write(
        2,
        "conn",
        25,
        Some(format!("{} 0x0\n", SYSCALL_NRS[5]).as_str()),
        Some(8_000),
    );
    s.sample(&p);
    let delta = s.pending[1].cpu_us - first;
    assert_eq!(delta, 5 * (1_000_000 / clock_tick()), "増分だけ積む");
    assert_eq!(
        s.pending_run_delay[1],
        5_000 + 3_000,
        "待ち時間も増分だけ積む"
    );
    // `syscall` が無ければ partial (ticks を進める: 休眠のまま ticks が同じスレッドは
    // 前回の値を使い回して開かないので。T.29.21)
    write(2, "conn", 26, None, Some(8_000));
    write(1, "proxy", 11, None, Some(1_000));
    s.sample(&p);
    assert_eq!(p.sampler(), SamplerState::Partial);
    assert!(s.pending[1].states[names.iter().position(|n| *n == "sleeping").unwrap()] >= 1);
    // 取り出したら 0 に戻る
    let (taken, top, run_delay) = s.take();
    assert!(taken.iter().any(|r| r.samples > 0));
    assert!(s.pending.iter().all(|r| r.samples == 0));
    // 上位のスレッドは CPU の多い順 (conn の tid 2 が先)
    assert_eq!(top[0].tid, 2);
    assert_eq!(top[0].comm_str(), "conn");
    assert_eq!(ROLES[top[0].role as usize], "conn");
    assert_eq!(top[1].tid, 1);
    assert_eq!(top[2], TopThread::default(), "残りは空の枠");
    assert_eq!(top[0].running, 0, "conn は splice の中に居た");
    assert_eq!(top[1].running, 2, "主スレッドは 1・2 回目が running");
    // 役割ごとの待ち時間 (accept 1 us / conn 8 us) が取り出せ、次の窓は 0 から
    let d = run_delay.expect("schedstat が読めている");
    assert_eq!(d[0], 1);
    assert_eq!(d[1], 8);
    assert!(s.pending_run_delay.iter().all(|v| *v == 0));
    assert!(s.pending_top.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// **休眠のまま ticks が同じスレッドは `syscall`・`schedstat` を読み直さない** (T.29.21)。
/// 走ったスレッド (ticks が進んだ / 状態が `S` でない) は毎回読む。
#[test]
fn the_sampler_skips_unchanged_sleeping_threads() {
    let dir = std::env::temp_dir().join(format!("t2921-skip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let write = |tid: u32, state: char, ticks: u64, nr: i64, delay_ns: u64| {
        let d = dir.join(tid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("stat"),
            format!("{tid} (t{tid}) {state} 1 1 0 0 -1 0 0 0 0 0 {ticks} 0 0 0 20 0 8 0 100 0\n"),
        )
        .unwrap();
        std::fs::write(d.join("syscall"), format!("{nr} 0x0\n")).unwrap();
        std::fs::write(d.join("schedstat"), format!("999 {delay_ns} 3\n")).unwrap();
    };
    let slot = |nr: i64| state_slot(Some(nr), 'S').0;
    let (a, b) = (SYSCALL_NRS[0], SYSCALL_NRS[1]);
    let o = ROLES.len() - 1; // 名前 t2・t3 は「その他」の役割
    let p = Profile::default();
    let mut s = Sampler::new(dir.clone(), 1);
    // 1 回目は全部読む
    write(2, 'S', 5, a, 1_000);
    write(3, 'R', 5, a, 1_000);
    s.sample(&p);
    // 2 回目: 中身を書き換えるが、tid 2 は休眠のまま ticks も同じ → 前回の値のまま
    write(2, 'S', 5, b, 9_000);
    write(3, 'R', 5, b, 9_000);
    s.sample(&p);
    assert_eq!(
        s.pending[o].states[slot(a)],
        2 + 1,
        "tid 2 は読み直していない"
    );
    assert_eq!(s.pending[o].states[slot(b)], 1, "tid 3 (R) は読み直した");
    assert_eq!(
        s.pending_run_delay[o],
        1_000 + 9_000,
        "tid 2 の待ちは前のまま"
    );
    // 3 回目: tid 2 の ticks が進んだ → 読み直し、待ちの増えは累計の差でまとめて入る
    write(2, 'S', 6, b, 9_000);
    s.sample(&p);
    assert_eq!(s.pending[o].states[slot(b)], 1 + 1 + 1);
    assert_eq!(
        s.pending_run_delay[o],
        9_000 + 9_000,
        "待ちの増えが消えない"
    );
    assert_eq!(p.sampler(), SamplerState::On);
    // 自分 (標本を取るスレッド) の `syscall` は開かずに `read` (読めばそう見える)
    s.self_tid = Some(3);
    std::fs::remove_file(dir.join("3").join("syscall")).unwrap();
    write(2, 'S', 7, b, 9_000);
    let reading = s.pending[o].states[slot(READ_NR)];
    s.sample(&p);
    assert_eq!(s.pending[o].states[slot(READ_NR)], reading + 1);
    assert_eq!(p.sampler(), SamplerState::On);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **1 us 未満の待ちが標本ごとに消えないこと** (T15.0 単位 3 のレビュー)。
///
/// 溜めを us で持つと 600 ns の増分が毎回 0 に落ち、窓じゅう「待たされていない」と
/// 読めてしまう。ns で積んで [`Sampler::take`] で 1 度だけ割るので 3 回で 1 us になる。
/// **名前も標本ごとに書き直す** (生まれたばかりのスレッドは親の `comm` を名乗る)。
#[test]
fn sub_microsecond_run_delays_add_up_and_the_name_is_refreshed() {
    let dir = std::env::temp_dir().join(format!("t150-subus-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let write = |comm: &str, delay_ns: u64| {
        let d = dir.join("1");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("stat"),
            format!(
                "1 ({}) R 1 1 0 0 -1 0 0 0 0 0 0 0 0 0 20 0 8 0 100 0\n",
                comm
            ),
        )
        .unwrap();
        std::fs::write(d.join("schedstat"), format!("999 {} 3\n", delay_ns)).unwrap();
    };
    let p = Profile::default();
    let mut s = Sampler::new(dir.clone(), 1);
    // 1 本目は「親の名前」、増分は 600 ns
    write("parent-name", 600);
    s.sample(&p);
    // 2・3 本目は本当の名前で、増分はそれぞれ 600 ns (合計 1,800 ns = 1 us)
    write("real-name", 1_200);
    s.sample(&p);
    write("real-name", 1_800);
    s.sample(&p);
    let (_, top, run_delay) = s.take();
    let d = run_delay.expect("schedstat が読めている");
    assert_eq!(d[0], 1, "600 ns × 3 が丸ごと消えている: {:?}", d);
    assert_eq!(
        top[0].comm_str(),
        "real-name",
        "名前がはじめて見たときのまま固まっている"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **役割ごとのユーザー空間**が utime の増分だけで積まれ、合計 (utime + stime) と
/// 別に読めること (T16.0)。行の末尾は `[user_us...],cpu_user_us`。
#[test]
fn the_user_time_is_split_per_role() {
    let dir = std::env::temp_dir().join(format!("t160-user-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let write = |utime: u64, stime: u64| {
        let d = dir.join("2");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("stat"),
            format!(
                "2 (conn) S 1 1 0 0 -1 0 0 0 0 0 {} {} 0 0 20 0 8 0 100 0\n",
                utime, stime
            ),
        )
        .unwrap();
    };
    let tick = 1_000_000 / clock_tick();
    let p = Profile::default();
    let mut s = Sampler::new(dir.clone(), 1);
    write(7, 13);
    s.sample(&p);
    // はじめて見たスレッドは累計がそのまま増分 (CPU と同じ流儀)
    assert_eq!(s.pending[1].cpu_us, 20 * tick);
    assert_eq!(s.pending[1].user_us, 7 * tick);
    // 2 回目: user 3 tick、kernel 7 tick 進める
    write(10, 20);
    s.sample(&p);
    assert_eq!(s.pending[1].cpu_us, 30 * tick);
    assert_eq!(s.pending[1].user_us, 10 * tick, "user は utime の増分だけ");
    let (threads, _, _) = s.take();
    assert_eq!(threads[1].user_us, 10 * tick);
    assert!(s.pending.iter().all(|r| r.user_us == 0), "取り出したら 0");
    // 60 秒へ畳むと足し合わせ、行の末尾に役割の順で出る
    let mut a = Sample {
        threads,
        cpu_us: 30 * tick,
        cpu_user_us: 10 * tick,
        ..Sample::default()
    };
    let b = a;
    a.merge_into(&b);
    assert_eq!(a.threads[1].user_us, 20 * tick);
    assert_eq!(a.cpu_user_us, 20 * tick);
    let mut row = String::new();
    a.push_row(&mut row);
    assert!(
        row.ends_with(&format!(
            ",null,[0,{},0,0,0,0,0,0,0],{}]",
            20 * tick,
            20 * tick
        )),
        "{}",
        row
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `schedstat` が読めない環境では `run_delay_us` が `null` になること (受け入れ基準)。
#[test]
fn the_run_delay_is_null_without_schedstat() {
    let dir = std::env::temp_dir().join(format!("t150-noschedstat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("1")).unwrap();
    std::fs::write(
        dir.join("1/stat"),
        "1 (proxy) R 1 1 0 0 -1 0 0 0 0 0 10 0 0 0 20 0 8 0 100 0\n",
    )
    .unwrap();
    let p = Profile::default();
    let mut s = Sampler::new(dir.clone(), 1);
    s.sample(&p);
    let (_, top, run_delay) = s.take();
    assert_eq!(run_delay, None, "schedstat が無ければ null");
    assert_eq!(top[0].tid, 1, "上位のスレッドは読めている");
    // JSON も `null` で書かれること (dashboard は位置で開くので数を変えない)
    let mut row = String::new();
    Sample {
        threads_top: top,
        run_delay_us: run_delay,
        ..Sample::default()
    }
    .push_row(&mut row);
    assert!(row.ends_with(",null,[0,0,0,0,0,0,0,0,0],0]"), "{}", row);
    assert!(row.contains("[[1,\"proxy\",0,"), "{}", row);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **実機で**、回り続けているスレッドが `threads_top` の先頭に出ること (T15.0 (5))。
///
/// `sample` は 2 回呼ぶ (CPU は前回との差で積むので、1 回目は「生まれてからのぶん」)。
/// 空回りの犯人を名前で指せるのがこの欄の目的なので `comm` も見る。
#[cfg(target_os = "linux")]
#[test]
fn the_busiest_thread_comes_first_in_threads_top() {
    use std::sync::Arc;

    let stop = Arc::new(AtomicBool::new(false));
    let s2 = Arc::clone(&stop);
    let spinner = thread::Builder::new()
        .name("t150-spin".into())
        .spawn(move || {
            // 最長 2 秒回し続ける (1 clock tick = 10 ms なので 200 tick ぶん)
            let until = Instant::now() + Duration::from_secs(2);
            let mut x = 0u64;
            while Instant::now() < until && !s2.load(Ordering::Relaxed) {
                for _ in 0..10_000 {
                    x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                }
            }
            std::hint::black_box(x);
        })
        .expect("spawn t150-spin");

    let p = Profile::default();
    let mut s = Sampler::for_self();
    s.sample(&p);
    thread::sleep(Duration::from_millis(500));
    s.sample(&p);
    stop.store(true, Ordering::Relaxed);
    let (_, top, run_delay) = s.take();
    assert_eq!(
        top[0].comm_str(),
        "t150-spin",
        "回り続けているスレッドが先頭に出ない: {:?}",
        top.iter()
            .map(|t| (t.tid, t.comm_str(), t.cpu_us))
            .collect::<Vec<_>>()
    );
    assert!(top[0].tid != 0 && top[0].tid != std::process::id());
    assert!(top[0].cpu_us > 0);
    assert!(top[0].running > 0, "走っている標本が 1 つも無い");
    // `schedstat` は `CONFIG_SCHEDSTATS` の無いカーネルでは**ファイルごと無い**
    // (この開発機がそれ。受け入れ基準の「読めない環境で `null`」はここで通る)
    let has_schedstat = std::path::Path::new("/proc/thread-self/schedstat").exists();
    assert_eq!(
        run_delay.is_some(),
        has_schedstat,
        "schedstat の読める / 読めないと `run_delay_us` が食い違う"
    );
    let _ = spinner.join();
}

/// 暇な秒は全スレッドを読まず、スレッドが生まれた・消えた秒と要求が来た秒は必ず読み直す (T.32.37)。
#[cfg(target_os = "linux")]
#[test]
fn the_sampler_skips_idle_seconds_but_rereads_when_threads_come_and_go() {
    let p = Profile::default();
    let mut s = Sampler::for_self();
    s.sample_unless_idle(&p, 0);
    assert_eq!(s.skipped, 0, "初回は必ず読む");
    // 同じプロセスの別の試験が CPU を使うと tick が進むので、読み飛ばしが起きるまで試す
    let skipped = (0..200).any(|_| {
        s.sample_unless_idle(&p, 0);
        if s.skipped > 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
        false
    });
    assert!(skipped, "暇な秒が 1 度も読み飛ばされない");
    // 読み飛ばした秒も 1 標本ぶん積む (状態の分布の分母が減らない)
    let samples: u64 = s.pending.iter().map(|w| w.samples).sum();
    assert!(samples > s.prev.len() as u64);
    // 要求が来た秒は読み直す
    let before = s.skipped;
    s.sample_unless_idle(&p, 1);
    assert_eq!(s.skipped, before, "要求が来た秒を読み飛ばした");
    // スレッドが生まれた秒は読み直す (新しいスレッドが前回の標本に入る)
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let h = thread::Builder::new()
        .name("t3237-born".into())
        .spawn(move || rx.recv().ok())
        .unwrap();
    let born = (0..200).find_map(|_| {
        s.sample_unless_idle(&p, 1);
        let tid =
            crate::sysinfo::scan_tasks(std::path::Path::new("/proc/self/task"), &mut Vec::new())
                .and_then(|scan| scan.tasks.into_iter().find(|t| t.comm == "t3237-born"))
                .map(|t| t.tid);
        match tid {
            Some(tid) => {
                // 名前が付いたあとの 1 回で、そのスレッドが読まれている
                s.sample_unless_idle(&p, 1);
                Some(tid)
            }
            None => {
                std::thread::sleep(Duration::from_millis(5));
                None
            }
        }
    });
    let born = born.expect("生まれたスレッドが見える");
    assert!(s.prev.contains_key(&born), "生まれたスレッドを読んでいない");
    tx.send(()).unwrap();
    h.join().unwrap();
    // 消えた秒も読み直す (消えたスレッドが前回の標本から落ちる)
    s.sample_unless_idle(&p, 1);
    assert!(!s.prev.contains_key(&born), "消えたスレッドが残っている");
}

/// `/proc` が読めない環境では `off` に落ちる (受け入れ基準)。
#[test]
fn an_unreadable_proc_turns_the_sampler_off() {
    let p = Profile::default();
    p.set_sampler(SamplerState::On);
    let mut s = Sampler::new(std::path::PathBuf::from("/nonexistent/task"), 1);
    s.sample(&p);
    assert_eq!(p.sampler(), SamplerState::Off);
    assert_eq!(p.sampler().name(), "off");
}
