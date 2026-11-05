use super::*;
use std::sync::mpsc;

/// `cond` が真になるまで 10 ms 刻みで最大 5 秒待つ。
///
/// 混んでいる機械ではスレッドが起きるまでに 100 ms 以上かかることがあるので、
/// 待ち時間を決め打ちしない。`#[track_caller]` を付けているので、時間切れの
/// パニックは**呼んだ側の行**を指す。
#[track_caller]
fn wait_until(cond: impl Fn() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("condition did not hold within 5s");
}

/// `cond` が 100 ms のあいだ (10 ms ごとに 10 回) 真のままであることを確かめる。
///
/// 「これ以上増えない」のような**起きないこと**を見るための待ち方。待てば真になる
/// 条件は [`wait_until`] を使う。1 回寝てから 1 回見るのと違い、途中で破れても捕まえる。
#[track_caller]
fn stays_true(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..10 {
        thread::sleep(Duration::from_millis(10));
        assert!(cond(), "{}", what);
    }
}

#[test]
fn reuses_one_thread_for_sequential_jobs() {
    let w = Arc::new(Workers::new(0));
    let (tx, rx) = mpsc::channel();
    let mut ids = Vec::new();
    for _ in 0..5 {
        let tx = tx.clone();
        w.run(Box::new(move || {
            let _ = tx.send(thread::current().id());
        }))
        .unwrap_or_else(|_| panic!("could not get a thread"));
        ids.push(rx.recv().unwrap());
        // 仕事が終わってから置き場に戻るまでの間に次を渡すと別スレッドになるので、
        // 戻ったのを見てから次へ (競合しないテストにする)
        wait_until(|| w.idle_count() == 1);
    }
    assert_eq!(ids.len(), 5);
    assert!(
        ids.windows(2).all(|p| p[0] == p[1]),
        "直列の仕事は同じスレッドが使い回される: {:?}",
        ids
    );
    assert_eq!(w.idle_count(), 1);
    assert_eq!(w.live_count(), 1, "生きているスレッドも 1 本");
}

#[test]
fn run_outside_runs_beside_a_full_pool() {
    // 上限 1 本のスレッドが塞がっていても、`run_outside` の仕事は待ち行列に積まれずに走る (T.28.0)
    let w = Arc::new(Workers::new(1));
    let (block_tx, block_rx) = mpsc::channel::<()>();
    w.run(Box::new(move || {
        let _ = block_rx.recv();
    }))
    .unwrap_or_else(|_| panic!("could not get a thread"));
    wait_until(|| w.live_count() == 1);
    let (tx, rx) = mpsc::channel();
    w.run_outside(Box::new(move || {
        let _ = tx.send(());
    }))
    .unwrap_or_else(|_| panic!("could not get a spare thread"));
    rx.recv_timeout(Duration::from_secs(5))
        .expect("上限の外のスレッドで走る");
    assert_eq!(w.queued(), 0, "待ち行列には積まない");
    assert_eq!(w.live_count(), 1, "上限の数え方は変えない");
    let _ = block_tx.send(());
}

#[test]
fn survives_a_panicking_job() {
    // 仕事がパニックしてもプールは使えるままであること
    // (そのスレッドは死に、置き場に残った送り口は次に取り出した側が捨てる)
    let w = Arc::new(Workers::new(0));
    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        w.run(Box::new(move || {
            let _ = tx.send(());
            panic!("intentional panic in a worker job");
        }))
        .unwrap_or_else(|_| panic!("could not get a thread"));
    }
    rx.recv().unwrap();
    // 席が戻るのはパニックしたスレッドが落ちきってからで、混んでいる機械では
    // 100 ms では足りない (決め打ちで待つと落ちる)。戻るまで待つ
    wait_until(|| w.live_count() == 0);

    // 次の仕事はちゃんと走る
    let (tx2, rx2) = mpsc::channel();
    for i in 0..3 {
        let tx2 = tx2.clone();
        w.run(Box::new(move || {
            let _ = tx2.send(i);
        }))
        .unwrap_or_else(|_| panic!("could not get a thread after the panic"));
        assert_eq!(rx2.recv().unwrap(), i);
    }
}

#[test]
fn runs_concurrent_jobs_on_separate_threads() {
    let w = Arc::new(Workers::new(0));
    let (start_tx, start_rx) = mpsc::channel::<()>();
    let (id_tx, id_rx) = mpsc::channel();
    let hold = Arc::new(Mutex::new(()));
    let guard = hold.locked();
    for _ in 0..4 {
        let (id_tx, hold, start_tx) = (id_tx.clone(), Arc::clone(&hold), start_tx.clone());
        w.run(Box::new(move || {
            let _ = start_tx.send(());
            let _ = id_tx.send(thread::current().id());
            // 全員が同時に走っていることを確かめるため、解放されるまで待つ
            let _held = hold.locked();
        }))
        .unwrap_or_else(|_| panic!("could not get a thread"));
    }
    for _ in 0..4 {
        start_rx.recv().unwrap();
    }
    let mut ids: Vec<_> = (0..4).map(|_| id_rx.recv().unwrap()).collect();
    drop(guard);
    ids.sort_by_key(|i| format!("{:?}", i));
    ids.dedup();
    assert_eq!(ids.len(), 4, "同時に走る仕事は別スレッド");
}

/// 上限に達したらスレッドを増やさず待たせる。**仕事は 1 つも捨てない** (T10.5)。
#[test]
fn caps_live_threads_and_queues_the_rest() {
    let w = Arc::new(Workers::new(2));
    let started = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let hold = Arc::new(Mutex::new(()));
    let guard = hold.locked();
    for _ in 0..8 {
        let (started, done, hold) = (Arc::clone(&started), Arc::clone(&done), Arc::clone(&hold));
        w.run(Box::new(move || {
            started.fetch_add(1, Ordering::SeqCst);
            let _held = hold.locked();
            done.fetch_add(1, Ordering::SeqCst);
        }))
        .unwrap_or_else(|_| panic!("could not get a thread"));
    }
    // 走れるのは上限の 2 本だけ。残りは待ち行列で待つ
    wait_until(|| started.load(Ordering::SeqCst) == 2);
    // 3 本目が走り出さないことは待っても真にならないので、しばらく見張る
    stays_true("上限を越えて走らない", || {
        started.load(Ordering::SeqCst) == 2
    });
    assert_eq!(w.live_count(), 2, "生きているスレッドは上限まで");
    assert_eq!(w.queued(), 6, "残りは待ち行列 (捨てない)");
    // 手を放せば残りも同じ 2 本で順に片づく
    drop(guard);
    wait_until(|| done.load(Ordering::SeqCst) == 8);
    assert_eq!(w.queued(), 0);
    assert!(w.live_count() <= 2, "増えていない: {}", w.live_count());
}

/// 待ち行列で待った仕事は件数と待ち時間が数えられる (`/profile` の「待ち行列」。T14.3 (3))。
///
/// 数える口はプロセス全体で 1 つなので、**差分**で見る (同じ試験バイナリの他の試験も足す)。
#[test]
fn queued_jobs_record_how_long_they_waited() {
    let before = crate::sync::queue_totals();
    let w = Arc::new(Workers::new(1));
    let done = Arc::new(AtomicUsize::new(0));
    let hold = Arc::new(Mutex::new(()));
    let guard = hold.locked();
    for _ in 0..3 {
        let (done, hold) = (Arc::clone(&done), Arc::clone(&hold));
        w.run(Box::new(move || {
            let _held = hold.locked();
            done.fetch_add(1, Ordering::SeqCst);
        }))
        .unwrap_or_else(|_| panic!("could not get a thread"));
    }
    wait_until(|| w.queued() == 2);
    // 待たせてから放す (待ち時間が 0 ms にならないように)
    thread::sleep(Duration::from_millis(20));
    drop(guard);
    wait_until(|| done.load(Ordering::SeqCst) == 3);
    let after = crate::sync::queue_totals();
    assert!(
        after[0] - before[0] >= 2,
        "待たせた 2 つが数えられること: {} -> {}",
        before[0],
        after[0]
    );
    assert!(
        after[1] >= before[1] + 20,
        "待ち時間 (ms) が足されること: {} -> {}",
        before[1],
        after[1]
    );
}

/// 「後でやればいい仕事」は上限に達したら積まずに返る (T11.3)。
///
/// **仕事が呼び出し元へ戻ってくること**が要点で、戻ってきた仕事を落とせば、
/// その中に入れた番人 (ここでは `Bell`) の `Drop` が走る (T9.6 と同じ性質)。
#[test]
fn try_run_gives_the_job_back_instead_of_queueing_it() {
    struct Bell(Arc<AtomicUsize>);
    impl Drop for Bell {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let w = Arc::new(Workers::new(1));
    let started = Arc::new(AtomicUsize::new(0));
    let hold = Arc::new(Mutex::new(()));
    let guard = hold.locked();
    {
        let (started, hold) = (Arc::clone(&started), Arc::clone(&hold));
        w.run(Box::new(move || {
            started.fetch_add(1, Ordering::SeqCst);
            let _held = hold.locked();
        }))
        .unwrap_or_else(|_| panic!("could not get a thread"));
    }
    wait_until(|| started.load(Ordering::SeqCst) == 1);

    // 上限は 1 で、その 1 本は塞がっている
    let dropped = Arc::new(AtomicUsize::new(0));
    let bell = Bell(Arc::clone(&dropped));
    let ran = Arc::new(AtomicUsize::new(0));
    let returned = {
        let ran = Arc::clone(&ran);
        w.try_run(Box::new(move || {
            let _bell = bell;
            ran.fetch_add(1, Ordering::SeqCst);
        }))
    };
    assert!(returned.is_err(), "上限に達したら仕事は戻ってくる");
    assert_eq!(w.queued(), 0, "待ち行列には積まない");
    assert_eq!(w.live_count(), 1, "スレッドも増えない");
    assert_eq!(dropped.load(Ordering::SeqCst), 0, "まだ落としていない");
    drop(returned);
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        1,
        "落とせば番人の Drop が走る"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 0, "仕事そのものは走らない");

    // 空きができれば `try_run` は普通に走る
    drop(guard);
    wait_until(|| w.idle_count() == 1);
    let (tx, rx) = mpsc::channel();
    w.try_run(Box::new(move || {
        let _ = tx.send(());
    }))
    .unwrap_or_else(|_| panic!("空きがあるのに走らせられなかった"));
    rx.recv().unwrap();
}

/// 上限を上げると、待たせていた仕事がその場で動き出す (T11.6)。
#[test]
fn raising_the_limit_starts_the_queued_jobs() {
    let w = Arc::new(Workers::new(1));
    let started = Arc::new(AtomicUsize::new(0));
    let hold = Arc::new(Mutex::new(()));
    let guard = hold.locked();
    for _ in 0..4 {
        let (started, hold) = (Arc::clone(&started), Arc::clone(&hold));
        w.run(Box::new(move || {
            started.fetch_add(1, Ordering::SeqCst);
            let _held = hold.locked();
        }))
        .unwrap_or_else(|_| panic!("could not get a thread"));
    }
    wait_until(|| started.load(Ordering::SeqCst) == 1);
    assert_eq!(w.queued(), 3, "上限 1 なので 3 つ待っている");

    w.set_limit(4);
    assert_eq!(w.max_threads(), 4);
    // 空いたスレッドを待たずに、待ち行列がその場で片づく
    wait_until(|| started.load(Ordering::SeqCst) == 4);
    assert_eq!(w.queued(), 0);
    assert_eq!(w.live_count(), 4);
    drop(guard);
}

/// `0` に戻すと無制限 (T10.5 以前の動き) に戻る (T11.6)。
#[test]
fn setting_the_limit_to_zero_means_unlimited() {
    let w = Arc::new(Workers::new(1));
    let started = Arc::new(AtomicUsize::new(0));
    let hold = Arc::new(Mutex::new(()));
    let guard = hold.locked();
    for _ in 0..6 {
        let (started, hold) = (Arc::clone(&started), Arc::clone(&hold));
        w.run(Box::new(move || {
            started.fetch_add(1, Ordering::SeqCst);
            let _held = hold.locked();
        }))
        .unwrap_or_else(|_| panic!("could not get a thread"));
    }
    wait_until(|| w.queued() == 5);
    w.set_limit(0);
    wait_until(|| started.load(Ordering::SeqCst) == 6);
    assert_eq!(w.queued(), 0);
    drop(guard);
}

/// 上限を下げても走っている仕事は殺さず、終わったスレッドから 1 本ずつ減る (T11.6)。
#[test]
fn lowering_the_limit_lets_the_running_jobs_finish() {
    let w = Arc::new(Workers::new(4));
    let started = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let hold = Arc::new(Mutex::new(()));
    let guard = hold.locked();
    let job = |w: &Arc<Workers>| {
        let (started, done, hold) = (Arc::clone(&started), Arc::clone(&done), Arc::clone(&hold));
        w.run(Box::new(move || {
            started.fetch_add(1, Ordering::SeqCst);
            let _held = hold.locked();
            done.fetch_add(1, Ordering::SeqCst);
        }))
        .unwrap_or_else(|_| panic!("could not get a thread"));
    };
    for _ in 0..4 {
        job(&w);
    }
    wait_until(|| started.load(Ordering::SeqCst) == 4);

    w.set_limit(1);
    assert_eq!(w.live_count(), 4, "走っているスレッドは殺さない");
    assert_eq!(
        started.load(Ordering::SeqCst),
        4,
        "走っている仕事は止まらない"
    );
    // 生きている数が新しい上限を超えているので、次の仕事は待たされる
    job(&w);
    assert_eq!(w.queued(), 1);

    drop(guard);
    wait_until(|| done.load(Ordering::SeqCst) == 5);
    // 仕事を終えたスレッドは空き置き場へ戻らずに終わり、上限まで縮む
    wait_until(|| w.live_count() <= 1);
    assert!(w.idle_count() <= 1, "空きも上限まで: {}", w.idle_count());
}

/// 上限が 1 のとき、走っている仕事がパニックしても待ち行列が片づくこと (T10.5)。
#[test]
fn queued_jobs_survive_a_panicking_job() {
    let w = Arc::new(Workers::new(1));
    let (tx, rx) = mpsc::channel();
    let started = Arc::new(AtomicUsize::new(0));
    let hold = Arc::new(Mutex::new(()));
    let guard = hold.locked();
    {
        let (tx, started, hold) = (tx.clone(), Arc::clone(&started), Arc::clone(&hold));
        w.run(Box::new(move || {
            started.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(0);
            // 後続が待ち行列に入るまで走り続け、そのうえでパニックする
            let _held = hold.locked();
            panic!("intentional panic in a worker job");
        }))
        .unwrap_or_else(|_| panic!("could not get a thread"));
    }
    rx.recv().unwrap();
    for i in 1..=2 {
        let (tx, started) = (tx.clone(), Arc::clone(&started));
        w.run(Box::new(move || {
            started.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(i);
        }))
        .unwrap_or_else(|_| panic!("could not get a thread"));
    }
    assert_eq!(w.queued(), 2, "上限 1 なので 2 つとも待たされる");
    // パニックしたスレッドが抱えていた席は戻り、待ち行列は代わりのスレッドが片づける
    drop(guard);
    let mut got = vec![rx.recv().unwrap(), rx.recv().unwrap()];
    got.sort();
    assert_eq!(got, vec![1, 2], "待たせた仕事は捨てられない");
    assert_eq!(started.load(Ordering::SeqCst), 3);
}
