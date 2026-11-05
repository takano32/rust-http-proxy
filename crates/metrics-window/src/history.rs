//! ダッシュボード用の履歴: 標本を 3 つの解像度の環状バッファに残し、`/history` で
//! JSON にして返す。ブラウザ側で差分からレート (req/s, MB/s) を求める。
//!
//! - 5 秒 × 4,320 (**6 時間**。メモリだけ。T14.32)、1 分 × 1440 (1 日)、1 時間 × 720 (30 日)
//! - 粗い解像度は細かい標本から作る: 累計カウンタは窓の最後の値、ゲージ (接続数・使用量) は平均、
//!   **ゲージの山は最大値** (`active_max` / `threads_max` / `fds_max`)、
//!   **区間の値** (応答時間の分布・エラー・名前解決) は足し合わせ
//! - 状態ファイル (`persist`。1 つ上の層) があれば各解像度をそこにも書き、起動時に読み戻す
//!
//! **累計と区間が混ざっている**のは意図したもの (T12.4 (3))。要求数やバイト数は累計を
//! 置いてブラウザ側で差分を取る (再起動をまたいでも段差が 1 つ出るだけ) が、応答時間の分布は
//! 差分が取れない (区間ごとの分位点が要る) ので、**その 5 秒に起きたぶんだけ**を置く。
//!
//! 混ざっていること自体は変えられない (既存の列の意味は変えない) ので、**どの列を
//! どう読むか**を [`KEY_KINDS`] (`/history` の `key_kinds`) で隣に出す (T15.0 (10))。
//! 通算しか無かった `requests` / `bytes` には区間の値 (`requests_delta` / `bytes_delta`) を
//! 並べて足した。

use crate::sync::LockExt;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;

// 窓そのものと解像度は下の層 (`proxy-metrics-window`) に置いてある (T14.55)。
// **今までの名前でここから引ける** (`crate::history::Window` / `RESOLUTIONS` …)。
pub use crate::window::{CAPACITY, DEFAULT_N, INTERVAL, RESOLUTIONS, WINDOW_BOUNDS_MS, Window};

// 標本・符号・閉じた窓は割ってある (T.34.28)。**今までの名前でここから引ける**
// (`crate::history::Sample` / `KEYS` / `SAMPLE_ITEMS` / `ClosedWindows` …)。
// 標本 1 本と列の名前・読み方、1 行の書き出しと畳み方
mod sample;
// `.rrd` のレコード (`encode` / `decode`) と固定の欄・予備の数
mod codec;
// 閉じた接続の分布の窓
mod closed;

pub use closed::ClosedWindows;
use closed::{push_num_array, push_str_array};
pub use codec::{SAMPLE_ITEMS, SAMPLE_SPARE_ITEMS, V3_SPARE_ITEMS};
pub use sample::{KEY_KINDS, KEYS, Sample};

/// 1 回の記録で各解像度に加わった標本 (状態ファイルへの書込用)。
#[derive(Debug, Default, Clone, Copy)]
pub struct Pushed {
    pub fine: Option<Sample>,
    pub minute: Option<Sample>,
    pub hour: Option<Sample>,
}

#[derive(Default)]
pub struct History {
    rings: [Mutex<VecDeque<Sample>>; 3],
    /// 閉じた接続の分布 (`/history` の `closed`。T14.6)。**`.rrd` には載せない**
    pub closed: ClosedWindows,
    /// 転送速度と半閉じの分布 (`/history` の `transfer`。T14.25)。**`.rrd` には載せない**
    pub transfer: crate::transfer::TransferWindows,
}

impl History {
    /// 5 秒の標本を記録し、分・時の窓が閉じたらそれらも作る。
    pub fn push(&self, s: Sample) -> Pushed {
        let mut out = Pushed {
            fine: Some(s),
            ..Pushed::default()
        };
        Self::append(&self.rings[0], s, Self::capacity(0));
        out.minute = self.roll(0, 1, s.t);
        if out.minute.is_some() {
            out.hour = self.roll(1, 2, s.t);
        }
        out
    }

    /// `from` の標本から、`to` の解像度で前の窓が閉じていればひとつ作る。
    fn roll(&self, from: usize, to: usize, now: u64) -> Option<Sample> {
        let step = RESOLUTIONS[to].0;
        let window_start = (now / step) * step;
        let last_to = self.rings[to].locked().back().map(|s| s.t);
        // 直前の窓 [window_start - step, window_start) がまだ無ければ作る
        let prev = window_start.checked_sub(step)?;
        if last_to.is_some_and(|t| t >= prev) {
            return None;
        }
        let src = self.rings[from].locked();
        let window: Vec<Sample> = src
            .iter()
            .filter(|s| s.t >= prev && s.t < window_start)
            .copied()
            .collect();
        drop(src);
        if window.is_empty() {
            return None;
        }
        let agg = Sample::downsample(&window, prev);
        Self::append(&self.rings[to], agg, Self::capacity(to));
        Some(agg)
    }

    fn append(ring: &Mutex<VecDeque<Sample>>, s: Sample, cap: usize) {
        let mut q = ring.locked();
        if q.len() >= cap {
            q.pop_front();
        }
        q.push_back(s);
    }

    /// 3 つの解像度の標本と `closed` / `transfer` の窓の置き場を、満杯のぶん確保して触る
    /// (**起動時に 1 回だけ**。T17.8)。返すのは触ったバイト数。
    ///
    /// 触っても件数は増えないので、`memory.rings_used.history` (件数 × 1 本) は変わらない。
    /// 読み戻した標本はそのまま残る (状態ファイルを読む前でも後でも呼べる)。
    pub fn prefault(&self) -> usize {
        let rings: usize = (0..RESOLUTIONS.len())
            .map(|res| crate::prefault::deque(&mut self.rings[res].locked(), Self::capacity(res)))
            .sum();
        rings + self.closed.prefault() + self.transfer.prefault()
    }

    /// 起動時に状態ファイルから読み戻す (`res` は 0=5 秒, 1=1 分, 2=1 時間)。
    pub fn restore(&self, res: usize, samples: impl IntoIterator<Item = Sample>) {
        let mut q = self.rings[res].locked();
        q.clear();
        q.extend(samples);
        while q.len() > Self::capacity(res) {
            q.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.rings[0].locked().len()
    }

    /// その解像度の `since <= t < until` の標本を古い順に `f` へ渡す (**鍵の内側で舐めるだけ**で写さない。
    /// 60 秒の 1,440 行を写すと 1.4 MB の確保になる)。`/daily` が起動したときに今日の積み上げを
    /// 読み戻した `history["60"]` から組み直すのに使う (T.32.18)。`f` の中で `History` に触らないこと
    pub fn for_each_in(&self, res: usize, since: u64, until: u64, mut f: impl FnMut(&Sample)) {
        let q = self.rings[res].locked();
        for s in q.iter().filter(|s| s.t >= since && s.t < until) {
            f(s);
        }
    }

    /// その解像度の標本数 (`res` は 0=5 秒, 1=1 分, 2=1 時間)。
    ///
    /// [`Self::len`] は 5 秒しか返さないので、3 つ足した「いま埋まっているぶん」
    /// (`/status` の `memory.rings_used.history`) を出すのに要る (T15.0 (13))。
    pub fn len_res(&self, res: usize) -> usize {
        self.rings[res.min(RESOLUTIONS.len() - 1)].locked().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 解像度 (秒) から添字を引く。合わなければ 5 秒。
    pub fn index_for(secs: u64) -> usize {
        RESOLUTIONS
            .iter()
            .position(|(s, _)| *s == secs)
            .unwrap_or(0)
    }

    /// **メモリ上の**リングが残す本数。5 秒だけ [`CAPACITY`] (6 時間。T14.32) で、
    /// 1 分と 1 時間は [`RESOLUTIONS`] のまま (`.rrd` の本数と同じ)。
    pub const fn capacity(res: usize) -> usize {
        if res == 0 {
            CAPACITY
        } else {
            RESOLUTIONS[res].1
        }
    }

    /// `?n=` を書かないときに返す本数。**5 秒だけ [`DEFAULT_N`] (720 = 1 時間)** で
    /// 頭打ちにして、応答の大きさを今までどおりに保つ (T14.32)。
    pub const fn default_n(res: usize) -> usize {
        if res == 0 {
            DEFAULT_N
        } else {
            RESOLUTIONS[res].1
        }
    }

    /// `?n=` の値を丸める (書いていなければ [`Self::default_n`]、上は
    /// [`Self::capacity`] で頭打ち。`/errors?n=` などと同じ作法で下は 1)。
    pub fn clamp_n(res: usize, n: Option<usize>) -> usize {
        n.unwrap_or_else(|| Self::default_n(res))
            .clamp(1, Self::capacity(res))
    }

    /// `{"interval_secs":N,"keys":[...],"bounds_ms":[...],"samples":[[...],...]}` (古い順)。
    ///
    /// **標本は配列の配列**にしてある (T12.4 (3))。項目が 13 から 31 に増えたので、
    /// 標本ごとにキーを繰り返すと `/history?res=5` (720 標本) が 1 MB を超える。
    /// キーは `keys` に 1 回だけ出し、`connect_buckets` / `forward_buckets` /
    /// `errors_by_cause` はその位置に入れ子の配列で入る。
    ///
    /// 末尾の `closed` は閉じた接続の分布 ([`ClosedWindows`]。T14.6)。**標本とは別の配列**で、
    /// `res=5|60` のときだけ中身がある (1 時間では `null`)。
    pub fn to_json(&self) -> String {
        self.to_json_res(0)
    }

    pub fn to_json_res(&self, res: usize) -> String {
        self.to_json_res_n(res, None)
    }

    /// `?n=` で**新しい方から何本返すか**を決める版 (T14.32)。
    ///
    /// 5 秒のリングは 6 時間ぶん ([`CAPACITY`]) 持つが、`n` を書かなければ今までどおり
    /// 720 本 ([`DEFAULT_N`]) しか返さない (応答の大きさを変えないため)。効くのは
    /// **標本の配列だけ**で、隣に並ぶ `closed` / `transfer` / `canary` / `kernel` の窓は
    /// もともと 720 本までしか持っていないのでそのまま出す。
    pub fn to_json_res_n(&self, res: usize, n: Option<usize>) -> String {
        let res = res.min(RESOLUTIONS.len() - 1);
        let n = Self::clamp_n(res, n);
        let q = self.rings[res].locked();
        // 古い方を落として新しい方から n 本
        let skip = q.len().saturating_sub(n);
        let mut out = String::with_capacity(256 + (q.len() - skip) * 360);
        // 応答の形の版は**いちばん先頭の鍵** (T14.49)。`closed` / `transfer` / `canary` /
        // `kernel` は**この応答の中の配列**なので、版を持つのはここ 1 か所だけ
        out.push_str(crate::metrics::SCHEMA_HEAD);
        push_header(&mut out, res);
        for (i, s) in q.iter().skip(skip).enumerate() {
            if i > 0 {
                out.push(',');
            }
            s.push_row(&mut out);
        }
        drop(q);
        // 閉じた接続の分布は**標本の後ろに別の配列**で出す (T14.6)。既存の `keys` /
        // `samples` の形と読み方は 1 つも変えない。1 時間の解像度では残していないので `null`
        out.push_str("],\"closed\":");
        out.push_str(&self.closed.to_json_res(res));
        // 転送速度と半閉じの分布は `closed` の隣に**別の配列**で出す (T14.25)。
        // `closed` の `keys` も標本の形も 1 つも変えていない
        out.push_str(",\"transfer\":");
        out.push_str(&self.transfer.to_json_res(res));
        // 利用者の要求が無い時間帯の名前解決と TCP 接続 (T14.10)。**別の配列**に足す
        // ので、既存の `keys` / `samples` を読む側は 1 行も変えなくてよい
        crate::canaryhist::push_history_json(&mut out, res);
        out.push('}');
        out
    }
}

/// `/history` の見出し (`interval_secs`・`keys`・`key_kinds`・`bounds_ms`・`causes`) と `samples` の口
/// (T.34.28 で [`History::to_json_res_n`] から出した)。版の鍵 (`SCHEMA_HEAD`) の直後に書く。
fn push_header(out: &mut String, res: usize) {
    let _ = write!(out, "\"interval_secs\":{},\"keys\":[", RESOLUTIONS[res].0);
    push_str_array(out, &KEYS);
    // 列の**読み方**を同じ長さで隣に出す (T15.0 (10))。綴り (`keys`) は 1 つも
    // 変えていないので、読む側は添字でも名前でも今までどおり引ける
    out.push_str("],\"key_kinds\":[");
    push_str_array(out, &KEY_KINDS);
    out.push_str("],\"bounds_ms\":[");
    push_num_array(out, &WINDOW_BOUNDS_MS);
    out.push_str("],\"causes\":[");
    push_str_array(out, &crate::metrics::ERR_CAUSE_NAMES);
    out.push_str("],\"samples\":[");
}

#[cfg(test)]
mod tests;

/// 標本 1 本の `.rrd` のレコードと予備 (T.34.28 で `tests.rs` から割った)。
#[cfg(test)]
mod codec_tests;

/// 閉じた接続の分布の窓 (T14.6)。**`.rrd` には載せない**メモリ上の窓。
#[cfg(test)]
mod closed_tests;

/// 期間を切ってサーバー側で畳む要約 (`/history?since=&until=&summary=1`。T14.24)。
///
/// T14.0 と T14.17 は `/history` を丸ごと取って手元で切って集計していた (284 KB を取って
/// 5 行を得る)。同じ畳み方をサーバー側でやれば、調査ページの「起動から」「前のデプロイと
/// 比べる」が **1 要求**で済む。**標本そのものは返さない**ので応答は 1 KiB 前後。
///
/// 求め方は `scripts/snapshot-diff.py` の `aggregate()` と**同じ**にしてある:
/// 区間の値 (確立時間の 12 段のヒストグラム・エラー・名前解決) は期間ぶん**足し合わせ**、
/// ゲージの山は**最大値**、分位点は足し合わせたヒストグラムを [`Window::quantile_ms`] で
/// 補間する (区間内は一様、その期間の最大値で頭打ち)。同じ期間を切れば同じ数字が出る。
///
/// **費用 0**: 読むのは `/history` に来たときだけで、要求の経路には何も足していない。
pub mod summary;

/// 期間を切ってサーバー側で畳む要約 (T14.24)。
///
/// 見るのは 4 つ: **T14.0 の表と同じ数字が出ること** (架空の標本列から p50 / p95 が
/// 手計算と一致)、平常時の切り出しがバーストを外すこと、解像度の自動選択、応答が 4 KiB 以下。
#[cfg(test)]
mod summary_tests;
