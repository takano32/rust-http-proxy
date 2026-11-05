//! CPU の窓 (T34.80 で lib.rs から分けた)。
//!
//! プロセスの CPU の増分から、自己ベンチのスレッドの取り分 ([`Charge`]) と窓を開け閉め
//! するスレッド自身の取り分を引き、**プロキシがした仕事だけ**を残す。

use std::sync::atomic::{AtomicU64, Ordering};

/// 自己ベンチのスレッドが使った CPU の合計 (us)。窓の増分からこれを引く。
pub(crate) static SB_CPU_US: AtomicU64 = AtomicU64::new(0);

/// 1 つの段階のあいだに**プロキシが**使った CPU を測る窓。
///
/// プロセス全体の CPU の増分から、自己ベンチのスレッドの取り分 ([`SB_CPU_US`]) と、
/// **窓を開け閉めしているこのスレッド自身**の取り分 (スレッドを起こす / 待つ費用) を引く。
pub(crate) struct Window {
    proc_us: u64,
    self_us: u64,
    own_us: u64,
}

impl Window {
    pub(crate) fn start() -> Window {
        Window {
            proc_us: process_cpu_us().unwrap_or(0),
            self_us: SB_CPU_US.load(Ordering::Relaxed),
            own_us: thread_cpu_us().unwrap_or(0),
        }
    }

    /// 窓を閉じて「プロキシ側の CPU (us)」を返す。**自己ベンチのスレッドを全部止めてから**
    /// 呼ぶこと (止める前に呼ぶと、まだ足していない取り分がプロキシ側に乗る)。
    pub(crate) fn end(self) -> u64 {
        let proc_delta = process_cpu_us().unwrap_or(0).saturating_sub(self.proc_us);
        let self_delta = SB_CPU_US
            .load(Ordering::Relaxed)
            .saturating_sub(self.self_us);
        let own_delta = thread_cpu_us().unwrap_or(0).saturating_sub(self.own_us);
        proc_delta
            .saturating_sub(self_delta)
            .saturating_sub(own_delta)
    }
}

/// 自己ベンチのスレッドの取り分を数える番人。**Drop で自分の CPU を足す**ので、
/// 途中で return してもパニックしても取り分が抜けない。
pub(crate) struct Charge;

impl Drop for Charge {
    fn drop(&mut self) {
        if let Some(us) = thread_cpu_us() {
            SB_CPU_US.fetch_add(us, Ordering::Relaxed);
        }
    }
}

/// このプロセスが使った CPU (us)。`/proc/<pid>/stat` の utime + stime と同じ量。
pub(crate) fn process_cpu_us() -> Option<u64> {
    cpu_us(CLOCK_PROCESS_CPUTIME_ID)
}

/// **呼んだスレッドが**使った CPU (us)。
pub(crate) fn thread_cpu_us() -> Option<u64> {
    cpu_us(CLOCK_THREAD_CPUTIME_ID)
}

/// `clock_gettime(2)` の時計の番号 (Linux)。
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;

/// `clock_gettime` で CPU 時間を us で読む。読めない環境では `None`
/// (呼び出し側は数字を出さずに `note` を書く)。
///
/// 外部クレートは使わない (TODO.md §0)。`crates/sys/src/sys.rs` と同じく
/// `unsafe extern "C"` で宣言し、64 ビットの Linux 以外では「無い」と出す
/// (`timespec` の中身が 64 ビットであることに寄りかかっているため)。
#[cfg(all(
    target_os = "linux",
    any(target_arch = "aarch64", target_arch = "x86_64")
))]
fn cpu_us(clock: i32) -> Option<u64> {
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }
    unsafe extern "C" {
        fn clock_gettime(clk: i32, tp: *mut Timespec) -> i32;
    }
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: 書き込み先は この呼び出しの間だけ有効ならよい `timespec` 1 つ。失敗は -1 で返る
    if unsafe { clock_gettime(clock, &raw mut ts) } != 0 {
        return None;
    }
    Some(ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000)
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "aarch64", target_arch = "x86_64")
)))]
fn cpu_us(_clock: i32) -> Option<u64> {
    None
}
