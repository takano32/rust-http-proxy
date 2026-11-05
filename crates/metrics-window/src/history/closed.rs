//! 閉じた接続の分布の窓 ([`ClosedWindows`]。T14.6。T.34.28 で `history.rs` から移した)。
//!
//! 窓の扱い (閉じる・触る・数える・配列を書く) は `transfer` と共通の [`TwoRes`] にある (T.34.34)。
//! ここに残すのは 1 窓ぶんの数え物 ([`ClosedCounts`]) の口と、1 本を足す `observe` だけ。

use crate::recent::ClosedCounts;
use crate::twores::{Counts, TwoRes, push_num_array, push_str_array};

/// 閉じた接続の分布を残す**メモリ上の窓** (5 秒 × 720 と 60 秒 × 1,440。T14.6)。
///
/// **`.rrd` の標本には足さない。** 標本 1 本の余白は 4 B しか残っていない (T14.2 (3)) ので、
/// 閉じた理由 8 種 + 寿命 13 段 + 上り 13 段 + 下り 13 段 + 合計 5 つ = 52 個の u64 は
/// どうやっても入らない。版を上げれば入るが、上げると統計が全部消える。
/// **ここは再起動で消えてよい**個票と同じ扱い (T14.4 のリングと同じ方針)。
///
/// 書くのは `Metrics::record_closed` = 接続の終了で 1 回だけで、
/// 窓を閉じるのは history スレッド ([`TwoRes::roll`]) — `/history` の標本と
/// **同じ周期・同じ境目**で閉じるので、読む側は時刻で突き合わせられる。
///
/// **件数 0 の窓は残さない** (デプロイ先は 43 本/時 なので、残すと 720 本のうち
/// 719 本がゼロの行になる)。行の先頭に窓の始まりの時刻があるので、抜けていても読める。
pub type ClosedWindows = TwoRes<ClosedCounts>;

impl TwoRes<ClosedCounts> {
    /// 閉じた接続 1 本を今の窓に足す (**接続の終了で 1 回だけ**。鍵 1 回)。
    pub fn observe(&self, e: &crate::recent::RecentEntry) {
        self.observe_with(|c| c.observe(e));
    }
}

impl Counts for ClosedCounts {
    const ROW_BYTES: usize = 140;

    fn merge(&mut self, o: &Self) {
        ClosedCounts::merge(self, o);
    }

    fn is_empty(&self) -> bool {
        ClosedCounts::is_empty(self)
    }

    fn push_row(&self, out: &mut String, t: u64) {
        ClosedCounts::push_row(self, out, t);
    }

    fn push_head(out: &mut String) {
        use crate::recent::{BYTE_BOUNDS, CLOSE_REASON_NAMES, CLOSED_KEYS, LIFE_BOUNDS_SECS};
        push_str_array(out, &CLOSED_KEYS);
        out.push_str("],\"reasons\":[");
        push_str_array(out, &CLOSE_REASON_NAMES);
        out.push_str("],\"life_bounds_secs\":[");
        push_num_array(out, &LIFE_BOUNDS_SECS);
        out.push_str("],\"byte_bounds\":[");
        push_num_array(out, &BYTE_BOUNDS);
        out.push_str("],\"samples\":[");
    }
}
