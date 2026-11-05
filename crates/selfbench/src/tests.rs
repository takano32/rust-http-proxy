//! selfbench の単体試験 (T.34.80 で lib.rs から分けた)。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use proxy_base::sync::LockExt;

use super::*;
use crate::cpu::{Charge, SB_CPU_US};
use crate::partner::count_requests;

/// 回していなければ `/status` の `self_bench` は `null`。
#[test]
fn status_is_null_before_the_first_run() {
    // このテストは `run` を呼ばない (呼ぶテストと同じプロセスだと順序に依るので、
    // 実バイナリでの確認は `tests/selfbench_test.rs` に置いてある)
    if last().is_none() {
        assert_eq!(status_json(), "null");
    }
}

/// 数字が出たときの形 (`/status` の `self_bench`)。
#[test]
fn status_json_has_the_documented_shape() {
    let r = Report {
        at: 1_700_000_000,
        forward_us: Some(45.25),
        connect_us: None,
        cores: 4,
        requests: 12_345,
        connects: 0,
        secs: 3,
        note: Some("the proxy refused all 3 CONNECTs".to_string()),
    };
    *LAST.locked() = Some(r);
    let json = status_json();
    assert!(json.contains("\"at\":1700000000"), "{}", json);
    assert!(json.contains("\"forward_us\":45.2"), "{}", json);
    assert!(json.contains("\"connect_us\":null"), "{}", json);
    assert!(json.contains("\"cores\":4"), "{}", json);
    assert!(json.contains("\"requests\":12345"), "{}", json);
    assert!(json.contains("\"connects\":0"), "{}", json);
    assert!(json.contains("\"secs\":3"), "{}", json);
    assert!(json.contains("refused all 3 CONNECTs"), "{}", json);
    *LAST.locked() = None;
}

/// 要求の終わりは、読みが途中で切れても数えられる。
#[test]
fn requests_are_counted_across_reads() {
    let mut m = 0usize;
    assert_eq!(
        count_requests(&mut m, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
        1
    );
    // 2 つ続けて届いた場合
    assert_eq!(count_requests(&mut m, b"A\r\n\r\nB\r\n\r\n"), 2);
    // 境目で切れた場合 (`\r\n\r` までで 1 回目が終わる)
    assert_eq!(count_requests(&mut m, b"C\r\n\r"), 0);
    assert_eq!(count_requests(&mut m, b"\nD"), 1);
    // 途中まで一致してから崩れたら数えない
    assert_eq!(count_requests(&mut m, b"E\r\nF\r\n"), 0);
}

/// プロセスとスレッドの CPU が読めること (この環境で読めなければ数字は出さない)。
#[cfg(target_os = "linux")]
#[test]
fn cpu_of_the_process_and_of_this_thread_is_readable() {
    // スレッドを先に読む: 後で読んだプロセスの値は、先に読んだこのスレッドの値を必ず含む
    // (逆の順だと、2 回の読みの間にこのスレッドが走った分だけ `p < t` になりうる。T.28.37)
    let t = thread_cpu_us().expect("このスレッドの CPU");
    let p = process_cpu_us().expect("プロセスの CPU");
    assert!(
        p >= t,
        "プロセスの CPU は 1 本のスレッドより多い ({} < {})",
        p,
        t
    );
}

/// 自己ベンチが回っている 3 秒だけ、相手役と打ち手を統計から外せること (T14.43)。
#[test]
fn the_partners_are_recognised_only_while_the_bench_runs() {
    assert!(!active(), "回していないときは旗が下りている");
    assert!(!is_target("http://127.0.0.1:41234"));
    assert!(!is_client("127.0.0.1"));
    arm([41234, 41235]);
    assert!(is_target("http://127.0.0.1:41234"), "forward の鍵");
    assert!(is_target("connect://127.0.0.1:41235"), "CONNECT の鍵");
    assert!(is_target("127.0.0.1:41235"), "接続元の個票が持つ宛先");
    assert!(!is_target("http://127.0.0.1:41236"), "覚えていないポート");
    assert!(
        !is_target("http://example.com:41234"),
        "ポートだけの一致では外さない"
    );
    assert!(!is_target("http://example.com"), "ポートが無い鍵");
    assert!(
        is_client("127.0.0.1") && is_client("::1"),
        "打ち手は自分の中"
    );
    assert!(!is_client("203.0.113.9"), "外から来た接続元はそのまま");
    disarm();
    assert!(!is_target("http://127.0.0.1:41234"), "3 秒で閉じる");
    assert!(!is_client("127.0.0.1"));
}

/// 本数の上限は打ち手 8 本で分け合っても、超えも足りなくもしない。
#[test]
fn the_budget_is_shared_by_all_the_workers() {
    let budget = Arc::new(Budget {
        used: AtomicU64::new(0),
        cap: 100,
    });
    let mut handles = Vec::with_capacity(CONC);
    for _ in 0..CONC {
        let budget = Arc::clone(&budget);
        handles.push(thread::spawn(move || {
            let mut n = 0u64;
            while budget.take() {
                n += 1;
            }
            n
        }));
    }
    let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(total, 100, "8 本で分け合っても合計は上限ちょうど");
    assert!(budget.spent(), "上限に当たったことが読める");
}

/// 自己ベンチのスレッドの取り分は、Drop のたびに足される。
#[cfg(target_os = "linux")]
#[test]
fn a_self_bench_thread_charges_its_own_cpu() {
    let before = SB_CPU_US.load(Ordering::Relaxed);
    let h = thread::Builder::new()
        .name("sb-test".into())
        .spawn(|| {
            let _charge = Charge;
            // 1 tick (10 ms) 以上は確実に使う
            let t0 = Instant::now();
            let mut n = 0u64;
            while t0.elapsed() < Duration::from_millis(30) {
                n = n.wrapping_add(t0.elapsed().as_nanos() as u64);
            }
            n
        })
        .expect("spawn");
    let _ = h.join();
    assert!(
        SB_CPU_US.load(Ordering::Relaxed) > before,
        "スレッドの CPU が自己ベンチの取り分に入っていない"
    );
}

/// 段階の結果を手で組む (`note_for` の試験用)。
fn phase(ok: u64, bad: u64, capped: bool) -> Phase {
    Phase {
        ok,
        bad,
        capped,
        cpu_us: 0,
    }
}

/// `note` の文面は 7 通り (T.34.80)。
#[test]
fn the_note_names_each_of_its_seven_cases() {
    assert_eq!(
        note_for(&phase(0, 5, false), &phase(0, 3, false)),
        "the proxy refused all 5 requests and all 3 CONNECTs (host ACL? per-client limit?)"
    );
    assert_eq!(
        note_for(&phase(0, 5, false), &phase(9, 0, true)),
        "the proxy refused all 5 requests (host ACL?)"
    );
    assert_eq!(
        note_for(&phase(9, 0, true), &phase(0, 3, false)),
        "the proxy refused all 3 CONNECTs (PROXY_CONNECT_PORTS?)"
    );
    assert_eq!(
        note_for(&phase(9, 0, true), &phase(9, 0, true)),
        "stopped early at the caps (20000 requests, 2000 CONNECTs)"
    );
    assert_eq!(
        note_for(&phase(9, 1, true), &phase(9, 0, false)),
        "stopped early at the 20000-request cap"
    );
    assert_eq!(
        note_for(&phase(9, 0, false), &phase(9, 1, true)),
        "stopped early at the 2000-CONNECT cap"
    );
    assert_eq!(
        note_for(&phase(9, 1, false), &phase(9, 1, false)),
        "",
        "上限に当たらず全部通れば書くことは無い"
    );
}

/// 鍵の判定は旗を見ずに、渡したポートだけで決まる (T.34.80)。
#[test]
fn a_target_matches_on_a_known_port_of_a_loopback_host() {
    let ports = [41234, 41235];
    assert!(target_matches("http://127.0.0.1:41234", &ports));
    assert!(target_matches("connect://127.0.0.1:41235", &ports));
    assert!(target_matches("127.0.0.1:41235", &ports));
    assert!(target_matches("http://[::1]:41234", &ports));
    assert!(!target_matches("http://127.0.0.1:41236", &ports));
    assert!(!target_matches("http://example.com:41234", &ports));
    assert!(!target_matches("http://example.com", &ports));
    assert!(!target_matches("http://127.0.0.1:port", &ports));
    // 覚えていない欄 (0) とポート 0 は一致させない
    assert!(!target_matches("http://127.0.0.1:0", &[0, 0]));
    assert!(!target_matches("http://127.0.0.1:41234", &[0, 0]));
}

/// 1 本も通らなかった段階は CPU/操作 を出さない (0 で割らない。T.34.80)。
#[test]
fn a_phase_with_no_operations_has_no_per_op_figure() {
    let empty = Phase {
        ok: 0,
        bad: 7,
        capped: false,
        cpu_us: 1_234,
    };
    assert_eq!(empty.per_op(), None);
    assert_eq!(Phase::default().per_op(), None);
    let some = Phase {
        ok: 4,
        bad: 0,
        capped: false,
        cpu_us: 100,
    };
    assert_eq!(some.per_op(), Some(25.0));
}
