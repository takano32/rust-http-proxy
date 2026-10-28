//! glibc の `mallinfo2(3)` でヒープの内訳を読む (T14.21)。
//!
//! RSS の内訳 (`/status` の `memory`) のうち**ヒープのぶん**を答えるためのもの。
//! `mallinfo2` は glibc 2.33 以上にしか無く、musl にも無い。無い環境で落ちないように、
//! **リンク時に解決せず `dlsym(RTLD_DEFAULT, "mallinfo2")` で実行時に探す**
//! (弱い参照 `#[linkage = "extern_weak"]` は不安定なので使えない)。
//! 見つからなければ [`malloc_info`] は `None` を返し、`/status` は `null` を出す。
//!
//! 読むのは `/status` に来たときだけ。`mallinfo2` はアリーナの鍵を順に取るので数 us
//! かかる (要求の経路からは呼ばない)。

/// `mallinfo2(3)` が返すヒープの内訳 (バイト)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MallocInfo {
    /// mmap ではない領域の合計 (`arena`)
    pub arena: u64,
    /// いま使われている合計 (`uordblks`)
    pub used: u64,
    /// 解放済みだがアロケータが手元に残している合計 (`fordblks`)
    pub free: u64,
    /// `mmap` で直接確保した領域の合計 (`hblkhd`)
    pub mmap: u64,
    /// そのうちの本数 (`hblks`)
    pub mmap_blocks: u64,
}

/// ヒープの内訳。`mallinfo2` が無い環境 (musl / glibc 2.32 以下 / Linux 以外) では `None`。
pub fn malloc_info() -> Option<MallocInfo> {
    imp::malloc_info()
}

/// `mallopt(M_ARENA_MAX)` で掛けた上限 (`PROXY_MALLOC_ARENAS`)。`0` は glibc の既定のまま。
///
/// 掛けた本人 (`main.rs`) が [`set_arena_max`] で覚えさせる。glibc に問い合わせる口が
/// 無いので、**掛けた値をそのまま出す**しかない (T5.6 で 8 に絞った経緯は README)。
pub fn arena_max() -> usize {
    ARENA_MAX.load(std::sync::atomic::Ordering::Relaxed)
}

/// `mallopt(M_ARENA_MAX)` に渡した値を覚える (起動時に 1 回)。
pub fn set_arena_max(max: usize) {
    ARENA_MAX.store(max, std::sync::atomic::Ordering::Relaxed);
}

static ARENA_MAX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 返していない空きを OS へ返す間隔 (秒。T.29.25)。
pub const TRIM_EVERY_SECS: u64 = 3600;

/// 前に [`trim_if_due`] が見た時刻 (epoch 秒。`0` = まだ見ていない)。
static LAST_TRIM_CHECK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 実際に使う間隔 (秒。**試験が短い周期で確かめられるように差し替えられる**。T.30.1)。
static TRIM_EVERY: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(TRIM_EVERY_SECS);

/// [`trim_if_due`] の間隔を差し替える (T.30.1 の試験。既定は [`TRIM_EVERY_SECS`])。
pub fn set_trim_every_secs(secs: u64) {
    TRIM_EVERY.store(secs, std::sync::atomic::Ordering::Relaxed);
}

/// いまの [`trim_if_due`] の間隔 (秒)。
pub fn trim_every_secs() -> u64 {
    TRIM_EVERY.load(std::sync::atomic::Ordering::Relaxed)
}

/// [`trim_if_due`] の通算 (`/status` の `memory.trim`。T.30.1)。
///
/// 返した量は info の 1 行にしか出ていなかったので、`/log` の 1,000 行が warn で埋まると
/// 返したかどうかが見えなかった (tokyo の 0.29.36)。書くのは `history` スレッドだけ
/// (1 時間に 1 回)、読むのは `/status` に来たときだけ。
static TRIM_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TRIM_SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TRIM_LAST_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TRIM_LAST_RETURNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TRIM_TOTAL_RETURNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TRIM_LAST_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [`trim_if_due`] の通算 (T.30.1)。`last_*` は 1 回も返していなければ `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrimStats {
    /// `malloc_trim(0)` を呼んだ回数
    pub count: u64,
    /// 間隔は来たが `heap_free <= heap_used` で呼ばなかった回数
    pub skipped: u64,
    /// 最後に呼んだ時刻 (epoch 秒)
    pub last_at: Option<u64>,
    /// 最後の 1 回で返した量 (RSS の減り。バイト)
    pub last_returned_bytes: Option<u64>,
    /// 返した量の通算 (バイト)
    pub total_returned_bytes: u64,
    /// 最後の 1 回の `malloc_trim(0)` にかかった時間 (us)
    pub last_us: Option<u64>,
}

/// いまの [`TrimStats`]。`history` スレッドが動かない (`--lite` / `PROXY_STATS_PERSIST=off`)
/// なら 0 と `None` のまま。
pub fn trim_stats() -> TrimStats {
    use std::sync::atomic::Ordering::Relaxed;
    let count = TRIM_COUNT.load(std::sync::atomic::Ordering::Acquire);
    let some = |v: u64| (count > 0).then_some(v);
    TrimStats {
        count,
        skipped: TRIM_SKIPPED.load(Relaxed),
        last_at: some(TRIM_LAST_AT.load(Relaxed)),
        last_returned_bytes: some(TRIM_LAST_RETURNED.load(Relaxed)),
        total_returned_bytes: TRIM_TOTAL_RETURNED.load(Relaxed),
        last_us: some(TRIM_LAST_US.load(Relaxed)),
    }
}

impl TrimStats {
    /// `/status` の `memory.trim` の JSON (鍵の並びは固定)。
    pub fn to_json(&self) -> String {
        let opt = |v: Option<u64>| v.map_or_else(|| "null".to_string(), |x| x.to_string());
        format!(
            concat!(
                "{{\"count\":{},\"skipped\":{},\"last_at\":{},\"last_returned_bytes\":{},",
                "\"total_returned_bytes\":{},\"last_us\":{},\"every_secs\":{}}}"
            ),
            self.count,
            self.skipped,
            opt(self.last_at),
            opt(self.last_returned_bytes),
            self.total_returned_bytes,
            opt(self.last_us),
            trim_every_secs()
        )
    }
}

/// **1 時間に 1 回** (試験は [`set_trim_every_secs`] で縮める)、解放済みなのにアロケータが抱えている空き (`heap_free`) が使っている量
/// (`heap_used`) を超えていれば `malloc_trim(0)` で OS へ返す (T.29.25)。`history` スレッドが
/// 5 秒ごとに呼ぶ (間隔に満たなければ原子の読み 1 回で戻る)。
///
/// glibc の `free` は**ヒープの天辺の空きしか返さない**ので、間に使用中の塊が 1 つでも残ると、
/// その下の空きページは RSS に載ったままになる (0.28.48 の 24 時間で RSS 65.8 MB のうち
/// heap_free 18.3 MB)。`malloc_trim(0)` は全アリーナの空きの塊の中のページも `madvise` で返す。
/// 返したページは**空きの塊のまま**なので `heap_free` (`mallinfo2` の `fordblks`) は減らず、
/// 減るのは RSS。返した量は RSS の差で見る (戻り値。返さなかったら `None`)。
///
/// 起動直後は見ない (最初の呼び出しは時刻を覚えるだけ)。`malloc_trim` が無い環境
/// (musl / Linux 以外) では何もしない。
pub fn trim_if_due(now_secs: u64) -> Option<Trimmed> {
    use std::sync::atomic::Ordering;
    let last = LAST_TRIM_CHECK.load(Ordering::Relaxed);
    if last == 0 {
        LAST_TRIM_CHECK.store(now_secs, Ordering::Relaxed);
        return None;
    }
    if now_secs.saturating_sub(last) < trim_every_secs() {
        return None;
    }
    LAST_TRIM_CHECK.store(now_secs, Ordering::Relaxed);
    let m = malloc_info()?;
    if m.free <= m.used {
        // 呼ばなかった回も数える (返すものが無かったのか、見ていないのかを分ける。T.30.1)
        TRIM_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let t = trim()?;
    // 通算は `history` スレッドだけが書く (T.30.1)。`last_at` を最後に書くのは、
    // `/status` が `count` を読んでから `last_*` を読むとき、0 時刻を拾わないため
    TRIM_LAST_RETURNED.store(t.released(), Ordering::Relaxed);
    TRIM_TOTAL_RETURNED.fetch_add(t.released(), Ordering::Relaxed);
    TRIM_LAST_US.store(t.took_us, Ordering::Relaxed);
    TRIM_LAST_AT.store(now_secs, Ordering::Relaxed);
    TRIM_COUNT.fetch_add(1, Ordering::Release);
    let msg = format!(
        "returned free heap to the OS (malloc_trim, {} us): rss {} -> {} MiB, heap used {} MiB / free {} MiB",
        t.took_us,
        t.rss_before >> 20,
        t.rss_after >> 20,
        m.used >> 20,
        m.free >> 20
    );
    // 1 MiB 以上返したときだけ info (返すものが無い時間は 1 時間に 1 行も出さない)
    if t.released() >= 1 << 20 {
        crate::log_info!(None, "{}", msg);
    } else {
        crate::log_debug!(None, "{}", msg);
    }
    Some(t)
}

/// [`trim`] の前後の RSS (バイト) と、`malloc_trim` にかかった時間。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trimmed {
    pub rss_before: u64,
    pub rss_after: u64,
    /// `malloc_trim(0)` の 1 回 (us)
    pub took_us: u64,
}

impl Trimmed {
    /// 返した量 (RSS の減り。増えていれば 0)。
    pub fn released(&self) -> u64 {
        self.rss_before.saturating_sub(self.rss_after)
    }
}

/// いますぐ `malloc_trim(0)` を呼ぶ (前後の RSS を返す)。`malloc_trim` が無ければ `None`。
///
/// 全アリーナの鍵を順に取って空きの塊を歩くので、ヒープが大きいと ms 単位かかる
/// (要求の経路からは呼ばない)。
pub fn trim() -> Option<Trimmed> {
    let rss_before = super::mem::process_rss().unwrap_or(0);
    let t0 = std::time::Instant::now();
    if !imp::malloc_trim() {
        return None;
    }
    let took_us = t0.elapsed().as_micros().min(u64::MAX as u128) as u64;
    let rss_after = super::mem::process_rss().unwrap_or(0);
    Some(Trimmed {
        rss_before,
        rss_after,
        took_us,
    })
}

#[cfg(target_os = "linux")]
mod imp {
    use super::MallocInfo;
    use std::ffi::{c_char, c_void};
    use std::sync::OnceLock;

    /// glibc 2.33 以上の `struct mallinfo2` (全フィールド `size_t`)。
    ///
    /// 古い `mallinfo` は同じ並びの `int` 版で、2 GiB を超えると桁があふれる。
    /// あふれた値を出すくらいなら `null` の方がましなので、`mallinfo2` だけを見る。
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MallInfo2 {
        arena: usize,
        ordblks: usize,
        smblks: usize,
        hblks: usize,
        hblkhd: usize,
        usmblks: usize,
        fsmblks: usize,
        uordblks: usize,
        fordblks: usize,
        keepcost: usize,
    }

    type FnMallInfo2 = unsafe extern "C" fn() -> MallInfo2;

    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    /// `RTLD_DEFAULT` (glibc も musl も `NULL`): 読み込み済みのものから既定の順で探す。
    const RTLD_DEFAULT: *mut c_void = std::ptr::null_mut();

    /// 解決は 1 回だけ (見つからなければ `None` を覚える)。
    fn resolve() -> Option<FnMallInfo2> {
        static F: OnceLock<Option<FnMallInfo2>> = OnceLock::new();
        *F.get_or_init(|| {
            // SAFETY: 有効な C 文字列と `RTLD_DEFAULT` を渡すだけ。無ければ null が返る。
            let p = unsafe { dlsym(RTLD_DEFAULT, c"mallinfo2".as_ptr()) };
            if p.is_null() {
                return None;
            }
            // SAFETY: glibc の `struct mallinfo2 mallinfo2(void)` と同じ型に変換する
            // (引数なし・`size_t` 10 個の構造体を返す)。
            Some(unsafe { std::mem::transmute::<*mut c_void, FnMallInfo2>(p) })
        })
    }

    /// glibc の `int malloc_trim(size_t pad)` (T.29.25)。
    type FnMallocTrim = unsafe extern "C" fn(pad: usize) -> i32;

    /// `malloc_trim` も glibc にしか無い (musl に無い) ので、`mallinfo2` と同じく実行時に探す。
    fn resolve_trim() -> Option<FnMallocTrim> {
        static F: OnceLock<Option<FnMallocTrim>> = OnceLock::new();
        *F.get_or_init(|| {
            // SAFETY: 有効な C 文字列と `RTLD_DEFAULT` を渡すだけ。無ければ null が返る。
            let p = unsafe { dlsym(RTLD_DEFAULT, c"malloc_trim".as_ptr()) };
            if p.is_null() {
                return None;
            }
            // SAFETY: glibc の `int malloc_trim(size_t pad)` と同じ型に変換する。
            Some(unsafe { std::mem::transmute::<*mut c_void, FnMallocTrim>(p) })
        })
    }

    /// `malloc_trim(0)` を呼ぶ。見つからなければ `false`。
    pub fn malloc_trim() -> bool {
        let Some(f) = resolve_trim() else {
            return false;
        };
        // SAFETY: 余白 0 を渡すだけ。アロケータの鍵を取り、空きページを OS へ返す。
        unsafe { f(0) };
        true
    }

    pub fn malloc_info() -> Option<MallocInfo> {
        let f = resolve()?;
        // SAFETY: 引数を取らず、アロケータの統計を返すだけ (副作用はアリーナの鍵だけ)。
        let m = unsafe { f() };
        Some(MallocInfo {
            arena: m.arena as u64,
            used: m.uordblks as u64,
            free: m.fordblks as u64,
            mmap: m.hblkhd as u64,
            mmap_blocks: m.hblks as u64,
        })
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::MallocInfo;

    pub fn malloc_info() -> Option<MallocInfo> {
        None
    }

    pub fn malloc_trim() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 天辺に使用中の塊を 1 つ残して下の 64 MiB を解放すると、`free` だけでは RSS が減らず、
    /// `trim` で減る (T.29.25)。`malloc_trim` が無い環境は `None` が正しい。
    #[cfg(target_os = "linux")]
    #[test]
    fn trim_returns_free_pages_below_a_live_block() {
        // mmap の閾値 (既定 128 KiB) より小さい塊で 64 MiB を**触って**確保する
        let mut blocks: Vec<Vec<u8>> = (0..4096)
            .map(|i| vec![(i % 251) as u8 + 1; 16 << 10])
            .collect();
        let keep = vec![7u8; 16 << 10];
        let peak = super::super::mem::process_rss().unwrap();
        blocks.clear();
        blocks.shrink_to_fit();
        let Some(t) = trim() else {
            return;
        };
        assert!(
            t.released() >= 32 << 20,
            "trim で RSS が減らない: 山 {} MiB、前 {} → 後 {} MiB",
            peak >> 20,
            t.rss_before >> 20,
            t.rss_after >> 20
        );
        assert_eq!(keep[0], 7);
    }

    /// 1 時間に 1 回だけ見る (最初は時刻を覚えるだけ)。
    #[test]
    fn trim_if_due_waits_for_the_interval() {
        LAST_TRIM_CHECK.store(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(trim_if_due(1_000), None, "最初は覚えるだけ");
        assert_eq!(trim_if_due(1_000 + TRIM_EVERY_SECS - 1), None);
        let _ = trim_if_due(1_000 + TRIM_EVERY_SECS);
        assert_eq!(
            LAST_TRIM_CHECK.load(std::sync::atomic::Ordering::Relaxed),
            1_000 + TRIM_EVERY_SECS,
            "間隔が来たら見た時刻を進める"
        );
        assert_eq!(trim_if_due(1_000 + TRIM_EVERY_SECS + 5), None);
    }

    /// この機械 (glibc) では読めて、辻褄が合っていること。
    ///
    /// 読めない環境 (musl / 古い glibc / Linux 以外) では `None` が正しいので、
    /// **`None` を失敗にはしない** (この関数の仕事は「無ければ `null`」)。
    #[test]
    fn mallinfo2_is_consistent_when_it_is_there() {
        let Some(m) = malloc_info() else {
            return;
        };
        // 1 バイトも確保していないプロセスは無い
        assert!(m.used > 0, "{:?}", m);
        // 使用中 + 空きはアリーナの中に収まる (mmap のぶんは別勘定)
        assert!(m.used + m.free <= m.arena + m.mmap, "{:?}", m);
        // 64 MiB は必ず mmap で取られる (glibc の閾値の上限は 32 MiB) ので、
        // 他の試験が並行して解放していても増分は消えない。
        // **触らない** (calloc の 0 ページなので RSS は増えない)
        let big = vec![0u8; 64 << 20];
        let after = malloc_info().unwrap();
        assert!(
            after.used + after.mmap >= m.used + m.mmap + (32 << 20),
            "{:?} -> {:?}",
            m,
            after
        );
        drop(big);
    }

    #[test]
    fn arena_max_is_what_we_set() {
        assert_eq!(arena_max(), 0);
        set_arena_max(8);
        assert_eq!(arena_max(), 8);
        set_arena_max(0);
    }
}
