//! 閉じた接続の分布の窓 ([`ClosedWindows`]。T14.6。T.34.28 で `history.rs` から移した)。

use super::*;
use crate::recent::ClosedCounts;

/// 閉じた接続の分布を残す**メモリ上の窓** (5 秒 × 720 と 60 秒 × 1,440。T14.6)。
///
/// **`.rrd` の標本には足さない。** 標本 1 本の余白は 4 B しか残っていない (T14.2 (3)) ので、
/// 閉じた理由 8 種 + 寿命 13 段 + 上り 13 段 + 下り 13 段 + 合計 5 つ = 52 個の u64 は
/// どうやっても入らない。版を上げれば入るが、上げると統計が全部消える。
/// **ここは再起動で消えてよい**個票と同じ扱い (T14.4 のリングと同じ方針)。
///
/// 書くのは `Metrics::record_closed` = 接続の終了で 1 回だけで、
/// 窓を閉じるのは history スレッド ([`ClosedWindows::roll`]) — `/history` の標本と
/// **同じ周期・同じ境目**で閉じるので、読む側は時刻で突き合わせられる。
///
/// **件数 0 の窓は残さない** (デプロイ先は 43 本/時 なので、残すと 720 本のうち
/// 719 本がゼロの行になる)。行の先頭に窓の始まりの時刻があるので、抜けていても読める。
pub struct ClosedWindows {
    inner: Mutex<ClosedState>,
}

#[derive(Default)]
struct ClosedState {
    /// まだ閉じていない 5 秒の窓と、その始まり (5 秒に丸めた epoch)
    cur: ClosedCounts,
    cur_t: u64,
    /// まだ閉じていない 60 秒の窓 (閉じた 5 秒の窓を足し込む)
    min_cur: ClosedCounts,
    min_t: u64,
    fine: VecDeque<(u64, ClosedCounts)>,
    minute: VecDeque<(u64, ClosedCounts)>,
    /// 起動からの通算 (畳んだ本数)
    total: u64,
}

impl Default for ClosedWindows {
    fn default() -> Self {
        ClosedWindows::new()
    }
}

impl ClosedWindows {
    pub fn new() -> ClosedWindows {
        ClosedWindows {
            inner: Mutex::new(ClosedState::default()),
        }
    }

    /// 閉じた接続 1 本を今の窓に足す (**接続の終了で 1 回だけ**。鍵 1 回)。
    pub fn observe(&self, e: &crate::recent::RecentEntry) {
        let mut w = self.inner.locked();
        w.total += 1;
        w.cur.observe(e);
    }

    /// 窓を閉じる (history スレッドが 5 秒ごとに呼ぶ)。
    ///
    /// `/history` の標本と同じ境目 (`now / 5 * 5`、`now / 60 * 60`) で切るので、
    /// 5 秒の窓の時刻は `/history?res=5` の `t` と、60 秒の窓は `res=60` の `t` と揃う。
    pub fn roll(&self, now: u64) {
        let mut w = self.inner.locked();
        let fine_t = (now / RESOLUTIONS[0].0) * RESOLUTIONS[0].0;
        if fine_t != w.cur_t {
            let closed = std::mem::take(&mut w.cur);
            let at = w.cur_t;
            w.cur_t = fine_t;
            if !closed.is_empty() {
                w.min_cur.merge(&closed);
                push_window(&mut w.fine, at, closed, RESOLUTIONS[0].1);
            }
        }
        let min_t = (now / RESOLUTIONS[1].0) * RESOLUTIONS[1].0;
        if min_t != w.min_t {
            let closed = std::mem::take(&mut w.min_cur);
            let at = w.min_t;
            w.min_t = min_t;
            if !closed.is_empty() {
                push_window(&mut w.minute, at, closed, RESOLUTIONS[1].1);
            }
        }
    }

    /// 2 つの窓の置き場を満杯のぶん確保して触る (**起動時に 1 回だけ**。T17.8)。
    /// 返すのは触ったバイト数。
    pub fn prefault(&self) -> usize {
        let mut w = self.inner.locked();
        crate::prefault::deque(&mut w.fine, RESOLUTIONS[0].1)
            + crate::prefault::deque(&mut w.minute, RESOLUTIONS[1].1)
    }

    /// 残してある窓の数 (5 秒 / 60 秒) と、畳んだ本数の通算。
    pub fn counts(&self) -> (usize, usize, u64) {
        let w = self.inner.locked();
        (w.fine.len(), w.minute.len(), w.total)
    }

    /// `/history` の `closed` (解像度の添字は [`RESOLUTIONS`] と同じ)。
    ///
    /// **1 時間の解像度では残していない** (`null`)。閉じた接続の分布は「いま効いている
    /// 設定が長すぎるか短すぎるか」を読むためのもので、30 日ぶんは要らない。
    pub fn to_json_res(&self, res: usize) -> String {
        use crate::recent::{BYTE_BOUNDS, CLOSE_REASON_NAMES, CLOSED_KEYS, LIFE_BOUNDS_SECS};
        if res > 1 {
            return "null".to_string();
        }
        let w = self.inner.locked();
        let ring = if res == 0 { &w.fine } else { &w.minute };
        let mut out = String::with_capacity(256 + ring.len() * 140);
        let _ = write!(out, "{{\"interval_secs\":{},\"keys\":[", RESOLUTIONS[res].0);
        push_str_array(&mut out, &CLOSED_KEYS);
        out.push_str("],\"reasons\":[");
        push_str_array(&mut out, &CLOSE_REASON_NAMES);
        out.push_str("],\"life_bounds_secs\":[");
        push_num_array(&mut out, &LIFE_BOUNDS_SECS);
        out.push_str("],\"byte_bounds\":[");
        push_num_array(&mut out, &BYTE_BOUNDS);
        out.push_str("],\"samples\":[");
        for (i, (t, c)) in ring.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            c.push_row(&mut out, *t);
        }
        let _ = write!(
            out,
            "],\"windows\":{},\"capacity\":{},\"recorded\":{}}}",
            ring.len(),
            RESOLUTIONS[res].1,
            w.total
        );
        out
    }
}

fn push_window(ring: &mut VecDeque<(u64, ClosedCounts)>, t: u64, c: ClosedCounts, cap: usize) {
    if ring.len() >= cap {
        ring.pop_front();
    }
    ring.push_back((t, c));
}

pub(super) fn push_str_array(out: &mut String, names: &[&str]) {
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", n);
    }
}

pub(super) fn push_num_array(out: &mut String, v: &[u64]) {
    for (i, n) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}", n);
    }
}
