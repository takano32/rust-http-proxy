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
}

#[cfg(test)]
mod tests {
    use super::*;

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
