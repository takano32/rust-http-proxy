//! 5 秒 × 720 と 60 秒 × 1,440 の **2 解像度のメモリ上の窓** ([`TwoRes`]。T34.34)。
//!
//! `closed` (T14.6) と `transfer` (T14.25) が別々に持っていた同じ約 90 行をここ 1 か所にした。
//! 型ごとに書くのは 1 窓ぶんの数え物の口 ([`Counts`]) だけ。呼び方は型の別名
//! (`crate::history::ClosedWindows` / `crate::transfer::TransferWindows`) のまま。

use crate::sync::LockExt;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;

use crate::window::RESOLUTIONS;

/// 1 窓ぶんの数え物 (`ClosedCounts` / `TransferCounts`)。
pub trait Counts: Default {
    /// `to_json_res` の文字列の先取りの 1 行ぶんの見積もり (出すバイトには効かない)
    const ROW_BYTES: usize;
    /// 閉じた 5 秒の窓を 60 秒の窓に足し込む
    fn merge(&mut self, o: &Self);
    /// 1 件も数えていないか (空の窓は残さない)
    fn is_empty(&self) -> bool;
    /// 行 1 つ (先頭が窓の始まりの時刻) を書く
    fn push_row(&self, out: &mut String, t: u64);
    /// `"keys":[` の中身から `"samples":[` までの見出しを書く (`interval_secs` の後ろ)
    fn push_head(out: &mut String);
}

#[derive(Default)]
struct State<C> {
    /// まだ閉じていない 5 秒の窓と、その始まり (5 秒に丸めた epoch)
    cur: C,
    cur_t: u64,
    /// まだ閉じていない 60 秒の窓 (閉じた 5 秒の窓を足し込む)
    min_cur: C,
    min_t: u64,
    fine: VecDeque<(u64, C)>,
    minute: VecDeque<(u64, C)>,
    /// 起動からの通算 (畳んだ本数)
    total: u64,
}

/// 2 解像度の窓 (5 秒 × 720 と 60 秒 × 1,440)。**`.rrd` には載せない** (再起動で消えてよい)。
#[derive(Default)]
pub struct TwoRes<C> {
    inner: Mutex<State<C>>,
}

impl<C: Counts> TwoRes<C> {
    pub fn new() -> TwoRes<C> {
        TwoRes::default()
    }

    /// 今の窓に 1 本を足す (鍵 1 回)。通算も 1 つ進める。
    pub(crate) fn observe_with(&self, f: impl FnOnce(&mut C)) {
        let mut w = self.inner.locked();
        w.total += 1;
        f(&mut w.cur);
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

    /// `/history` の `closed` / `transfer` (解像度の添字は [`RESOLUTIONS`] と同じ)。
    ///
    /// **1 時間の解像度では残していない** (`null`)。どちらも「いま効いている設定が長すぎるか
    /// 短すぎるか」を読むための分布で、30 日ぶんは要らない。
    pub fn to_json_res(&self, res: usize) -> String {
        if res > 1 {
            return "null".to_string();
        }
        let w = self.inner.locked();
        let ring = if res == 0 { &w.fine } else { &w.minute };
        let mut out = String::with_capacity(256 + ring.len() * C::ROW_BYTES);
        let _ = write!(out, "{{\"interval_secs\":{},\"keys\":[", RESOLUTIONS[res].0);
        C::push_head(&mut out);
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

fn push_window<C>(ring: &mut VecDeque<(u64, C)>, t: u64, c: C, cap: usize) {
    if ring.len() >= cap {
        ring.pop_front();
    }
    ring.push_back((t, c));
}

/// `"a","b",…` (括弧は書かない)。
pub(crate) fn push_str_array(out: &mut String, names: &[&str]) {
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", n);
    }
}

/// `1,2,…` (括弧は書かない)。
pub(crate) fn push_num_array(out: &mut String, v: &[u64]) {
    for (i, n) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}", n);
    }
}
