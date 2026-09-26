//! 記録のリングの置き場を**起動時にまとめて確保して触る** (T17.8)。
//!
//! リングは満ちるまで伸びるので、RSS は起動から数時間〜1 日かけて天井まで上がっていく。
//! その増え方は「リングが埋まっただけ」なのに、毎回リークでないことを読み解く手間が要った
//! (§4 の `malloc_trim` の項)。起動時に容量ぶんを確保して 1 ページに 1 バイトずつ触って
//! おけば、**起動直後から RSS が天井の近くに居る**ので、そこから増えたらそのまま信号になる。
//!
//! - 触るのは**まだ要素の入っていない置き場** (`Vec::spare_capacity_mut`) だけ。
//!   入っている要素にも件数 (`len`) にも触らないので、`/status` の `memory.rings_used`
//!   (件数 × 1 件の大きさ) は 1 バイトも変わらない
//! - 呼ぶかどうかは呼び出し側が決める (`--lite` と Linux 以外では呼ばない。
//!   `--lite` は「使わないときに一切コストがかからない」の対象)
//! - 速さは変わらない: 確保の回数が起動時の 1 回に減るだけで、要求の経路は同じ
//!   (満ちる前に `push` が伸ばし直す分の複写も無くなる)

use std::collections::VecDeque;

/// 触る間隔。4 KiB より大きいページの機械でも、4 KiB ごとに触れば全部のページに当たる。
const PAGE: usize = 4096;

/// `v` の置き場を `cap` 件ぶんまで確保し (足りないぶんだけ、ちょうど)、要素の入っていない
/// 置き場を 1 ページに 1 バイトずつ触る。返すのは触った置き場のバイト数。
///
/// 中身と `len` は変えない (読み戻した件はそのまま残る)。既に `cap` 件以上の置き場が
/// あれば確保はしない (触るのは空いている所だけ)。
pub fn vec<T>(v: &mut Vec<T>, cap: usize) -> usize {
    if cap > v.len() {
        v.reserve_exact(cap - v.len());
    }
    let spare = v.spare_capacity_mut();
    let bytes = std::mem::size_of_val(spare);
    touch(spare.as_mut_ptr().cast::<u8>(), bytes);
    bytes
}

/// [`vec`] の `VecDeque` 版。`Vec` に移して確保と触りを済ませ、戻す。
///
/// `VecDeque` → `Vec` は確保し直さず (要素が置き場の途中から始まっていれば並べ直すだけ)、
/// `Vec` → `VecDeque` は O(1) で置き場をそのまま引き継ぐ (どちらも std が約束している)。
/// 起動時に 1 回呼ぶだけなので、並べ直しが起きても数 us。
pub fn deque<T>(q: &mut VecDeque<T>, cap: usize) -> usize {
    let mut v = Vec::from(std::mem::take(q));
    let bytes = vec(&mut v, cap);
    *q = VecDeque::from(v);
    bytes
}

/// `p` から `len` バイトを 1 ページに 1 バイトずつ書く (最後のバイトも書く)。
fn touch(p: *mut u8, len: usize) {
    if len == 0 {
        return;
    }
    let mut off = 0;
    while off < len {
        // SAFETY: `p .. p + len` は呼び出し側が `&mut` で借りている `Vec` の空いた置き場
        // (`spare_capacity_mut` = `MaybeUninit`) で、`off < len` なので範囲の内側。
        // 書くのは初期化していない置き場なので、要素の不変条件に触れない。
        // `volatile` にしたのは、あとで読まれない書き込みとして最適化で消されないため
        unsafe { std::ptr::write_volatile(p.add(off), 0) };
        off += PAGE;
    }
    // 置き場の終わりがページの途中なら、最後のページにも当てる
    // SAFETY: 同上 (`len - 1 < len`)
    unsafe { std::ptr::write_volatile(p.add(len - 1), 0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vec_gets_exactly_the_capacity_and_keeps_its_contents() {
        let mut v: Vec<u64> = vec![1, 2, 3];
        let bytes = vec(&mut v, 1000);
        assert_eq!(v, [1, 2, 3], "中身は変えない");
        assert!(v.capacity() >= 1000);
        assert_eq!(bytes, (v.capacity() - 3) * 8);
        // 既に足りていれば確保し直さない
        let cap = v.capacity();
        let ptr = v.as_ptr();
        vec(&mut v, 10);
        assert_eq!((v.capacity(), v.as_ptr()), (cap, ptr));
    }

    #[test]
    fn a_deque_keeps_its_order_and_does_not_grow_past_the_capacity_when_filled() {
        let mut q: VecDeque<u32> = VecDeque::new();
        // 置き場の途中から始まる形にしてから触る (並べ直しが要る側)
        q.extend([9, 9, 1, 2]);
        q.pop_front();
        q.pop_front();
        deque(&mut q, 4320);
        assert_eq!(q, [1, 2], "順番と中身は変えない");
        let cap = q.capacity();
        assert!(cap >= 4320);
        let before = q.as_slices().0.as_ptr();
        for i in 0..(cap - 2) as u32 {
            q.push_back(i);
        }
        assert_eq!(q.capacity(), cap, "満ちるまで伸ばし直さない");
        assert_eq!(q.as_slices().0.as_ptr(), before, "置き場は起動時のまま");
    }

    #[test]
    fn zero_sized_and_empty_rings_touch_nothing() {
        let mut v: Vec<()> = Vec::new();
        assert_eq!(vec(&mut v, 100), 0);
        let mut q: VecDeque<u8> = VecDeque::new();
        assert_eq!(deque(&mut q, 0), 0);
    }
}
