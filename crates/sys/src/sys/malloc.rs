//! glibc の `mallopt(3)`: アリーナ数の上限と、mmap の閾の固定。**どちらもスレッドを作る前に 1 回だけ。**

use std::ffi::c_int;

unsafe extern "C" {
    fn mallopt(param: c_int, value: c_int) -> c_int;
}

/// glibc の `M_ARENA_MAX` (malloc.h の定義値)。
const M_ARENA_MAX: c_int = -8;

/// malloc のアリーナ数に上限を掛ける。**スレッドを作る前に 1 回だけ呼ぶこと。**
///
/// glibc は既定でコア数の 8 倍までアリーナを作り、スレッドごとに別のアリーナを使う。
/// 接続ごとにスレッドが増えるこのプロキシでは、アイドル接続を多数抱えたときに
/// 使われないままのアリーナが RSS に居座る。戻り値は成功したか。
pub fn limit_malloc_arenas(max: c_int) -> bool {
    // SAFETY: 定数のパラメータ番号と値を渡すだけ。失敗は 0 で返る。
    unsafe { mallopt(M_ARENA_MAX, max) == 1 }
}

/// glibc の `M_TRIM_THRESHOLD` / `M_MMAP_THRESHOLD` (malloc.h の定義値)。
const M_TRIM_THRESHOLD: c_int = -1;
const M_MMAP_THRESHOLD: c_int = -3;

/// glibc の mmap の閾を `bytes` に固定し、trim の閾をその 4 倍にする (T.32.3)。
/// **スレッドを作る前に 1 回だけ呼ぶこと。** 戻り値は 2 つとも受け付けられたか。
///
/// glibc は既定で「動く閾」を使い、mmap で取った塊を free するたびに閾をその大きさまで
/// (動く閾の上限は 32 MiB) 上げ、trim の閾を 2 倍にする。上がった後は大きな一時の確保 (重い口の
/// `String`、キャッシュの本文) がアリーナから取られ、主でないアリーナの天辺の空きは
/// `free` でも `malloc_trim(0)` でも返らない。どちらかを `mallopt` で決めると
/// `mp_.no_dyn_threshold` が立ち、閾はそれきり動かない。
pub fn fix_malloc_mmap_threshold(bytes: usize) -> bool {
    let (mmap, trim) = mmap_thresholds(bytes);
    // SAFETY: 定数のパラメータ番号と値を渡すだけ。失敗は 0 で返る。
    unsafe { mallopt(M_MMAP_THRESHOLD, mmap) == 1 && mallopt(M_TRIM_THRESHOLD, trim) == 1 }
}

/// `mallopt` に渡す 2 つの値 (mmap の閾と、その 4 倍の trim の閾。`c_int` に収める)。
pub(super) fn mmap_thresholds(bytes: usize) -> (c_int, c_int) {
    let mmap = bytes.min(c_int::MAX as usize) as c_int;
    let trim = bytes.saturating_mul(4).min(c_int::MAX as usize) as c_int;
    (mmap, trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// trim の閾は mmap の閾の 4 倍で、どちらも `c_int` で頭打ち (T.32.3)。
    #[test]
    fn the_trim_threshold_is_four_times_the_mmap_threshold_and_both_fit_an_int() {
        assert_eq!(mmap_thresholds(256 * 1024), (256 * 1024, 1024 * 1024));
        assert_eq!(mmap_thresholds(0), (0, 0));
        assert_eq!(
            mmap_thresholds(1 << 30),
            (1 << 30, c_int::MAX),
            "4 倍が溢れれば trim だけ頭打ち"
        );
        assert_eq!(mmap_thresholds(usize::MAX), (c_int::MAX, c_int::MAX));
    }
}
