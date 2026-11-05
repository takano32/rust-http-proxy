//! 固定長リングの共通の計算 (`recent` の一部。T.34.2 で `errors.rs` から移し、添字の式を集めた)。
//!
//! 閉じた接続・山の写真・エラー・出来事・追跡の 5 本のリングはどれも同じ形: `Vec` を使った分だけ
//! 伸ばし、上限の件数で満杯になったら `next` の位置から古いものを上書きする。**新しい順に読む添字**
//! ([`newest_at`])、**1 件書く** ([`push_wrapping`])、**ファイルにまだ書いていない件の取り出し**
//! ([`drain_unwritten`]) の式を 1 か所に置き、ここで直に試験する。どれも `#[inline]` の小さな関数で、
//! 接続の終了で 1 回通る `RecentRing::push` の費用は変わらない (同じ式を書いていた所を呼び出しに
//! 置き換えただけ)。
//!
//! T.34.49 で、5 本が同じ 5 欄 (置き場・次に書く位置・通算・書いた所・読み戻した件数) を別々に
//! 持っていたのを [`FixedRing`] 1 つにまとめた。各リングの内側は `Mutex<FixedRing<T, CAP>>`。

/// 新しい方から `i` 番目 (`0` = 最新) が `buf` のどこに居るか。
///
/// 満杯になる前は `next == 0` のままなので末尾が最新、満杯になってからは `next` の 1 つ手前が最新。
/// `len` は `buf.len()` (1 以上で呼ぶ)、`cap` はリングの上限、`next` は次に書く位置。
#[inline]
pub(crate) fn newest_at(len: usize, cap: usize, next: usize, i: usize) -> usize {
    let start = if len < cap { len } else { next };
    (start + len - 1 - i) % len
}

/// 1 件書く (満杯なら最も古いものを上書きし、`next` を 1 つ進める)。
#[inline]
pub(crate) fn push_wrapping<T>(buf: &mut Vec<T>, next: &mut usize, cap: usize, item: T) {
    if buf.len() < cap {
        buf.push(item);
        return;
    }
    let at = *next;
    buf[at] = item;
    *next = (at + 1) % cap;
}

/// リングの「まだファイルに書いていない件」を**古い順**で取り出す (T14.9)。
///
/// 印を位置ではなく**通算の件数** (`total`) にしてあるのは、リングが古いものを
/// 上書きするため: 位置で覚えると 1 周したときに「どこまで書いたか」が分からなくなる。
/// `max` を越える分は**古い方から落とす** (1 周期の書き込みを 64 KiB に抑えるため。
/// 落とした件数を 2 つ目に返す。メモリのリングには全部残っている)。
pub(crate) fn drain_unwritten<T: Clone>(
    buf: &[T],
    next: usize,
    cap: usize,
    total: u64,
    written: &mut u64,
    max: usize,
) -> (Vec<T>, u64) {
    let len = buf.len();
    let pending = total.saturating_sub(*written).min(len as u64) as usize;
    *written = total;
    if pending == 0 {
        return (Vec::new(), 0);
    }
    let take = pending.min(max);
    // 満杯になる前は先頭が最古、満杯になってからは次に書く位置が最古
    let start = if len < cap { 0 } else { next };
    let out = ((len - take)..len)
        .map(|i| buf[(start + i) % len].clone())
        .collect();
    (out, (pending - take) as u64)
}

/// 固定長リング 1 本 (T.34.49): 置き場・次に書く位置・通算・ファイルに書いた所・読み戻した件数。
///
/// 閉じた接続・山の写真・エラー・出来事・追跡の 5 本が同じ 5 欄と同じ操作を持っていたので、
/// ここに 1 つにまとめた。上限の件数は型の `CAP` (定数なので、前の `push_wrapping(…, MAX_*, …)` と
/// 同じ命令になる)。鍵は持たない (各リングが `Mutex<FixedRing<…>>` で包む)。
pub(crate) struct FixedRing<T, const CAP: usize> {
    buf: Vec<T>,
    /// 次に書く位置 (`buf` が満杯になってからだけ意味を持つ)
    next: usize,
    /// 起動からの通算 (捨てた分も含む)
    total: u64,
    /// **ファイルに書いた所までの通算** (T14.9。印の数え方は呼ぶ側が決める)
    written: u64,
    /// 起動時に状態ファイルから読み戻した件数 (`"restored"`)
    restored: usize,
}

impl<T, const CAP: usize> Default for FixedRing<T, CAP> {
    fn default() -> Self {
        FixedRing::new()
    }
}

impl<T, const CAP: usize> FixedRing<T, CAP> {
    /// 空のリング (1 バイトも確保しない。`static` にも置ける)。
    pub(crate) const fn new() -> Self {
        FixedRing {
            buf: Vec::new(),
            next: 0,
            total: 0,
            written: 0,
            restored: 0,
        }
    }

    /// 1 件書いて通算を 1 進める (満杯なら最も古いものを上書きする)。
    #[inline]
    pub(crate) fn push(&mut self, item: T) {
        self.total += 1;
        self.write(item);
    }

    /// 通算を進めずに 1 件書く (エラーのリングは通算と行を別に数える。T20.3)。
    #[inline]
    pub(crate) fn write(&mut self, item: T) {
        push_wrapping(&mut self.buf, &mut self.next, CAP, item);
    }

    /// 通算だけを 1 進める (同じ行にまとめた 1 件。T20.3)。
    #[inline]
    pub(crate) fn count(&mut self) {
        self.total += 1;
    }

    /// 覚えている件数。
    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    /// 起動からの通算 (捨てた分も含む)。
    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// 起動時に読み戻した件数。
    pub(crate) fn restored(&self) -> usize {
        self.restored
    }

    /// **新しい順**に並べた中身 (`0` 番目が最新)。
    pub(crate) fn newest(&self) -> impl Iterator<Item = &T> + '_ {
        let len = self.buf.len();
        (0..len).map(move |i| &self.buf[newest_at(len, CAP, self.next, i)])
    }

    /// 新しい方から `i` 番目 (`i < len()` で呼ぶ) を書き換える口 (エラーの行をまとめる。T20.3)。
    pub(crate) fn newest_mut(&mut self, i: usize) -> &mut T {
        let len = self.buf.len();
        &mut self.buf[newest_at(len, CAP, self.next, i)]
    }

    /// まだファイルに書いていない件を古い順で取り出す。印は `mark` (通算の件数。ふつうは
    /// [`FixedRing::total`]、エラーは行の通算) まで進める。
    pub(crate) fn take_unwritten(&mut self, mark: u64, max: usize) -> (Vec<T>, u64)
    where
        T: Clone,
    {
        drain_unwritten(&self.buf, self.next, CAP, mark, &mut self.written, max)
    }

    /// 置き場を `CAP` 件ぶん確保して触る (**起動時に 1 回だけ**。T17.8)。返すのは触ったバイト数。
    pub(crate) fn prefault(&mut self) -> usize {
        crate::prefault::vec(&mut self.buf, CAP)
    }

    /// 読み戻しを終える: 読み戻した件は書き直さないので印を `mark` に合わせ、件数を覚える。
    pub(crate) fn finish_restore(&mut self, mark: u64, n: usize) {
        self.written = mark;
        self.restored = n;
    }

    /// 空に戻す (試験用の口が呼ぶ)。置き場は確保したまま。
    pub(crate) fn clear(&mut self) {
        self.buf.clear();
        self.next = 0;
        self.total = 0;
        self.written = 0;
        self.restored = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 満杯になる前は末尾が最新、満杯になってからは `next` の 1 つ手前が最新。
    #[test]
    fn newest_at_walks_backwards_from_the_latest_entry() {
        // 3 件入って満杯 (上限 5) でない: 新しい順は 2, 1, 0
        assert_eq!(
            (0..3).map(|i| newest_at(3, 5, 0, i)).collect::<Vec<_>>(),
            vec![2, 1, 0]
        );
        // 満杯 (上限 3) で次に書く位置が 1 = 位置 0 が最新、位置 1 が最古
        assert_eq!(
            (0..3).map(|i| newest_at(3, 3, 1, i)).collect::<Vec<_>>(),
            vec![0, 2, 1]
        );
        // 1 件だけ
        assert_eq!(newest_at(1, 3, 0, 0), 0);
    }

    /// 上限まで伸び、そこからは最も古い位置を上書きして `next` が 1 周する。
    #[test]
    fn push_wrapping_fills_then_overwrites_the_oldest() {
        let (mut buf, mut next) = (Vec::new(), 0usize);
        for i in 0..5u32 {
            push_wrapping(&mut buf, &mut next, 3, i);
        }
        assert_eq!(buf, vec![3, 4, 2]);
        assert_eq!(next, 2);
        push_wrapping(&mut buf, &mut next, 3, 5);
        assert_eq!(
            (buf, next),
            (vec![3, 4, 5], 0),
            "3 件目を書いたら先頭に戻る"
        );
    }

    /// まだ書いていない件は古い順。印は通算で持つので 1 周しても数え違えず、`max` を越えるぶんは
    /// 古い方から落として件数だけ返す。
    #[test]
    fn drain_unwritten_returns_the_oldest_pending_items_and_moves_the_mark() {
        // 満杯になる前: 5 件のうち 2 件 (位置 3・4) が未書き込み
        let buf: Vec<u32> = (0..5).collect();
        let mut written = 3u64;
        assert_eq!(
            drain_unwritten(&buf, 0, 8, 5, &mut written, usize::MAX),
            (vec![3, 4], 0)
        );
        assert_eq!(written, 5);
        assert_eq!(
            drain_unwritten(&buf, 0, 8, 5, &mut written, usize::MAX),
            (Vec::<u32>::new(), 0),
            "印が進んだ"
        );
        // 1 周したあと: 上限 4 のリングに 6 件書き (`next` = 2、中身は [4, 5, 2, 3])、3 件が未書き込み
        let buf = vec![4u32, 5, 2, 3];
        let mut written = 3u64;
        assert_eq!(
            drain_unwritten(&buf, 2, 4, 6, &mut written, usize::MAX),
            (vec![3, 4, 5], 0)
        );
        // 未書き込みがリングの長さを越えていれば (書けないまま 1 周した) 残っている分だけ
        let mut written = 0u64;
        assert_eq!(
            drain_unwritten(&buf, 2, 4, 6, &mut written, usize::MAX),
            (vec![2, 3, 4, 5], 0)
        );
        // `max` を越えるぶんは古い方から落として件数を返す
        let mut written = 0u64;
        assert_eq!(
            drain_unwritten(&buf, 2, 4, 6, &mut written, 1),
            (vec![5], 3)
        );
        assert_eq!(written, 6);
    }
}
