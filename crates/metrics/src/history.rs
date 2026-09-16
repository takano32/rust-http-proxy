//! ダッシュボード用の履歴: 標本を 3 つの解像度の環状バッファに残し、`/history` で
//! JSON にして返す。ブラウザ側で差分からレート (req/s, MB/s) を求める。
//!
//! - 5 秒 × 720 (1 時間)、1 分 × 1440 (1 日)、1 時間 × 720 (30 日)
//! - 粗い解像度は細かい標本から作る: 累計カウンタは窓の最後の値、ゲージ (接続数・使用量) は平均、
//!   **ゲージの山は最大値** (`active_max` / `threads_max` / `fds_max`)、
//!   **区間の値** (応答時間の分布・エラー・名前解決) は足し合わせ
//! - 状態ファイル ([`crate::persist`]) があれば各解像度をそこにも書き、起動時に読み戻す
//!
//! **累計と区間が混ざっている**のは意図したもの (T12.4 (3))。要求数やバイト数は累計を
//! 置いてブラウザ側で差分を取る (再起動をまたいでも段差が 1 つ出るだけ) が、応答時間の分布は
//! 差分が取れない (区間ごとの分位点が要る) ので、**その 5 秒に起きたぶんだけ**を置く。

use crate::sync::LockExt;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::cache::{Cache, now_epoch};
use crate::metrics::Metrics;
use crate::recent::ClosedCounts;
use crate::rrd::{Dec, Enc};

/// 記録の間隔と本数 (5 秒 × 720 = 1 時間)。
pub const INTERVAL: Duration = Duration::from_secs(5);
pub const CAPACITY: usize = 720;

/// 解像度 (秒) と本数。
pub const RESOLUTIONS: [(u64, usize); 3] = [(5, 720), (60, 1440), (3600, 720)];

/// 履歴の窓ごとの応答時間ヒストグラムの区間 (ms)。**12 段** (T12.4 (3))。
///
/// ホスト別の [`crate::metrics::LATENCY_BOUNDS_MS`] (24 段) より粗いのは、
/// こちらは 2,880 標本 × 2 系列ぶんファイルに載るため。分位点は区間内を補間し、
/// **その窓で観測した最大値で頭打ちにする** (件数が少ないとき区間の上端が出ないように)。
pub const WINDOW_BOUNDS_MS: [u64; 12] = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000];

/// 「その区間の」応答時間 (件数・合計・最大・区間ごとの件数)。累計ではない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Window {
    pub count: u64,
    pub ms_sum: u64,
    pub ms_max: u64,
    pub buckets: [u64; WINDOW_BOUNDS_MS.len() + 1],
}

impl Window {
    pub fn observe(&mut self, ms: u64) {
        self.count += 1;
        self.ms_sum += ms;
        self.ms_max = self.ms_max.max(ms);
        let idx = WINDOW_BOUNDS_MS
            .iter()
            .position(|&b| ms <= b)
            .unwrap_or(WINDOW_BOUNDS_MS.len());
        self.buckets[idx] += 1;
    }

    /// 粗い解像度へ畳むときは足し合わせる (区間の値なので平均でも最後の値でもない)。
    pub fn merge(&mut self, o: &Window) {
        self.count += o.count;
        self.ms_sum += o.ms_sum;
        self.ms_max = self.ms_max.max(o.ms_max);
        for (a, b) in self.buckets.iter_mut().zip(o.buckets.iter()) {
            *a += *b;
        }
    }

    pub fn avg_ms(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.ms_sum as f64 / self.count as f64
        }
    }

    /// 区間内を線形に補間した分位点 (ms)。最後の区間と、観測した最大値で頭打ち。
    pub fn quantile_ms(&self, q: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let rank = (q.clamp(0.0, 1.0) * self.count as f64).max(1.0);
        let mut seen = 0u64;
        for (i, &n) in self.buckets.iter().enumerate() {
            if n == 0 {
                continue;
            }
            if (seen + n) as f64 >= rank {
                let lo = if i == 0 {
                    0.0
                } else {
                    WINDOW_BOUNDS_MS[i - 1] as f64
                };
                let hi = if i < WINDOW_BOUNDS_MS.len() {
                    WINDOW_BOUNDS_MS[i] as f64
                } else {
                    (self.ms_max as f64).max(lo)
                };
                let frac = (rank - seen as f64) / n as f64;
                return (lo + (hi - lo) * frac).min(self.ms_max as f64);
            }
            seen += n;
        }
        self.ms_max as f64
    }

    fn encode(&self, e: &mut Enc) {
        e.u64(self.count).u64(self.ms_sum).u64(self.ms_max);
        for b in self.buckets {
            e.u64(b);
        }
    }

    fn decode(d: &mut Dec<'_>) -> Window {
        let mut w = Window {
            count: d.u64(),
            ms_sum: d.u64(),
            ms_max: d.u64(),
            ..Window::default()
        };
        for b in w.buckets.iter_mut() {
            *b = d.u64();
        }
        w
    }

    fn push_json(&self, out: &mut String) {
        let _ = write!(out, ",{},{},{},[", self.count, self.ms_sum, self.ms_max);
        for (i, b) in self.buckets.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", b);
        }
        out.push(']');
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sample {
    pub t: u64,
    pub requests: u64,
    pub bytes: u64,
    pub active: usize,
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    pub evictions: u64,
    pub mem_used: u64,
    pub mem_limit: u64,
    pub disk_used: u64,
    pub disk_limit: u64,
    pub rss: u64,
    /// その区間に確立した CONNECT トンネルの確立時間 (T12.4 (3)。Phase 13 の主指標)
    pub connect: Window,
    /// その区間に転送した要求の初バイトまでの時間
    pub forward: Window,
    /// その区間のエラー件数と、その原因別の内訳
    pub errors: u64,
    pub errors_by_cause: [u64; crate::metrics::ERR_CAUSES],
    /// その区間に OS へ問い合わせた名前解決の回数と、その合計 ms
    pub dns_misses: u64,
    pub dns_ms_sum: u64,
    /// プロセス全体のスレッド数 (`/proc/self/status` の `Threads`)
    pub threads: u64,
    /// 開いている記述子の数 (`/proc/self/fd`) と、その上限 (`RLIMIT_NOFILE`)
    pub fds: u64,
    pub max_fds: u64,
    /// ゲージの山。**粗い解像度では平均に畳むと山が消える**ので最大値も持つ
    pub active_max: u64,
    pub threads_max: u64,
    pub fds_max: u64,
    /// 上限に当たって暇なトンネルを追い出した回数の累計 (T13.2)。
    ///
    /// **レコードの末尾に足した** (T14.2)。標本 1 本は 63 項目 × 8 B = 504 B で、
    /// 領域の 508 B にまだ収まるので `.rrd` の版は上げていない (上げると統計が全部消える)。
    /// 版 2 で書かれた古いレコードはこの位置がゼロ埋めなので 0 として読み戻る
    pub evicted_idle: u64,
}

/// `/history` の 1 標本の列名 (この順で [`Sample::push_row`] が値を並べる)。
/// **キーを標本ごとに繰り返さない**ため、JSON は配列の配列にしてある (T12.4 (3))。
pub const KEYS: [&str; 32] = [
    "t",
    "requests",
    "bytes",
    "active",
    "active_max",
    "hits",
    "misses",
    "stores",
    "evictions",
    "mem_used",
    "mem_limit",
    "disk_used",
    "disk_limit",
    "rss",
    "connects",
    "connect_ms_sum",
    "connect_ms_max",
    "connect_buckets",
    "forwards",
    "forward_ms_sum",
    "forward_ms_max",
    "forward_buckets",
    "errors",
    "errors_by_cause",
    "dns_misses",
    "dns_ms_sum",
    "threads",
    "threads_max",
    "fds",
    "fds_max",
    "max_fds",
    "evicted_idle",
];

impl Sample {
    pub fn take(metrics: &Metrics, cache: &Cache) -> Self {
        let (mem_used, _) = cache.mem_usage();
        let (disk_used, _) = cache.disk_usage();
        let active = metrics.active_connections.load(Ordering::Relaxed);
        let iv = metrics.take_interval();
        // `/proc` を読むのは 5 秒の標本のときだけ (要求ごとには読まない)
        let (threads, fds, max_fds) = process_counts();
        // カーネルと cgroup の窓 (`/proc/net`・cgroup・PSI) もこの標本のときだけ進める (T14.12)
        crate::kernel::sample(now_epoch());
        Self {
            t: now_epoch(),
            requests: metrics.total_requests.load(Ordering::Relaxed),
            bytes: metrics.bytes_forwarded.load(Ordering::Relaxed),
            active,
            hits: metrics.cache_hits.load(Ordering::Relaxed),
            misses: metrics.cache_misses.load(Ordering::Relaxed),
            stores: cache.stores.load(Ordering::Relaxed),
            evictions: cache.evictions.load(Ordering::Relaxed),
            mem_used,
            mem_limit: cache.mem_capacity(),
            disk_used,
            disk_limit: cache.disk_capacity(),
            rss: cache.snapshot().rss.unwrap_or(0),
            connect: iv.connect,
            forward: iv.forward,
            errors: iv.errors,
            errors_by_cause: iv.errors_by_cause,
            dns_misses: iv.dns_misses,
            dns_ms_sum: iv.dns_ms_sum,
            threads,
            fds,
            max_fds,
            active_max: active as u64,
            threads_max: threads,
            fds_max: fds,
            evicted_idle: metrics.evicted_idle.load(Ordering::Relaxed),
        }
    }

    /// 1 標本を配列 1 行として書く ([`KEYS`] の順)。
    fn push_row(&self, out: &mut String) {
        let _ = write!(
            out,
            "[{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            self.t,
            self.requests,
            self.bytes,
            self.active,
            self.active_max,
            self.hits,
            self.misses,
            self.stores,
            self.evictions,
            self.mem_used,
            self.mem_limit,
            self.disk_used,
            self.disk_limit,
            self.rss
        );
        self.connect.push_json(out);
        self.forward.push_json(out);
        let _ = write!(out, ",{},[", self.errors);
        for (i, c) in self.errors_by_cause.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", c);
        }
        let _ = write!(
            out,
            "],{},{},{},{},{},{},{},{}]",
            self.dns_misses,
            self.dns_ms_sum,
            self.threads,
            self.threads_max,
            self.fds,
            self.fds_max,
            self.max_fds,
            self.evicted_idle
        );
    }

    /// 状態ファイルのレコード (先頭が時刻)。
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u64(self.t)
            .u64(self.requests)
            .u64(self.bytes)
            .u64(self.active as u64)
            .u64(self.hits)
            .u64(self.misses)
            .u64(self.stores)
            .u64(self.evictions)
            .u64(self.mem_used)
            .u64(self.mem_limit)
            .u64(self.disk_used)
            .u64(self.disk_limit)
            .u64(self.rss);
        self.connect.encode(&mut e);
        self.forward.encode(&mut e);
        e.u64(self.errors);
        for c in self.errors_by_cause {
            e.u64(c);
        }
        e.u64(self.dns_misses)
            .u64(self.dns_ms_sum)
            .u64(self.threads)
            .u64(self.fds)
            .u64(self.max_fds)
            .u64(self.active_max)
            .u64(self.threads_max)
            .u64(self.fds_max)
            // **末尾に足すこと** (T14.2)。前からある項目の位置が動くと、版 2 で書かれた
            // 古いレコードが別の意味で読み戻る
            .u64(self.evicted_idle);
        e.0
    }

    pub fn decode(payload: &[u8]) -> Option<Sample> {
        let mut d = Dec(payload);
        let t = d.u64();
        if t == 0 {
            return None;
        }
        let mut s = Sample {
            t,
            requests: d.u64(),
            bytes: d.u64(),
            active: d.u64() as usize,
            hits: d.u64(),
            misses: d.u64(),
            stores: d.u64(),
            evictions: d.u64(),
            mem_used: d.u64(),
            mem_limit: d.u64(),
            disk_used: d.u64(),
            disk_limit: d.u64(),
            rss: d.u64(),
            ..Sample::default()
        };
        s.connect = Window::decode(&mut d);
        s.forward = Window::decode(&mut d);
        s.errors = d.u64();
        for c in s.errors_by_cause.iter_mut() {
            *c = d.u64();
        }
        s.dns_misses = d.u64();
        s.dns_ms_sum = d.u64();
        s.threads = d.u64();
        s.fds = d.u64();
        s.max_fds = d.u64();
        s.active_max = d.u64();
        s.threads_max = d.u64();
        s.fds_max = d.u64();
        // 版 2 で書かれたレコードはここから先がゼロ埋めなので 0 になる (`Dec` は足りなければ 0)
        s.evicted_idle = d.u64();
        Some(s)
    }

    /// 窓の標本をひとつにまとめる: 累計は最後の値、ゲージは平均 (山は最大値)、
    /// 区間の値は足し合わせ、時刻は窓の先頭。
    fn downsample(window: &[Sample], t: u64) -> Sample {
        let last = window.last().copied().unwrap_or_default();
        let n = window.len().max(1) as u64;
        let avg = |f: fn(&Sample) -> u64| window.iter().map(f).sum::<u64>() / n;
        let max = |f: fn(&Sample) -> u64| window.iter().map(f).max().unwrap_or(0);
        let sum = |f: fn(&Sample) -> u64| window.iter().map(f).sum::<u64>();
        let mut connect = Window::default();
        let mut forward = Window::default();
        let mut errors_by_cause = [0u64; crate::metrics::ERR_CAUSES];
        for s in window {
            connect.merge(&s.connect);
            forward.merge(&s.forward);
            for (a, b) in errors_by_cause.iter_mut().zip(s.errors_by_cause.iter()) {
                *a += *b;
            }
        }
        Sample {
            t,
            requests: last.requests,
            bytes: last.bytes,
            active: avg(|s| s.active as u64) as usize,
            hits: last.hits,
            misses: last.misses,
            stores: last.stores,
            evictions: last.evictions,
            mem_used: avg(|s| s.mem_used),
            mem_limit: avg(|s| s.mem_limit),
            disk_used: avg(|s| s.disk_used),
            disk_limit: avg(|s| s.disk_limit),
            rss: avg(|s| s.rss),
            connect,
            forward,
            errors: sum(|s| s.errors),
            errors_by_cause,
            dns_misses: sum(|s| s.dns_misses),
            dns_ms_sum: sum(|s| s.dns_ms_sum),
            threads: avg(|s| s.threads),
            fds: avg(|s| s.fds),
            max_fds: last.max_fds,
            active_max: max(|s| s.active_max),
            threads_max: max(|s| s.threads_max),
            fds_max: max(|s| s.fds_max),
            // 累計カウンタなので窓の最後の値 (`requests` と同じ。ブラウザ側で差分を取る)
            evicted_idle: last.evicted_idle,
        }
    }
}

/// プロセス全体のスレッド数 / 開いている記述子の数 / その上限。
/// **5 秒の標本のときだけ**呼ぶこと (`/proc` を 2 つ読み、ディレクトリを 1 つ数える)。
fn process_counts() -> (u64, u64, u64) {
    let threads = crate::sysinfo::process_threads().unwrap_or(0);
    let (fds, max_fds) = crate::sysinfo::process_fds().unwrap_or((0, 0));
    (threads, fds, max_fds)
}

/// 閉じた接続の分布を残す**メモリ上の窓** (5 秒 × 720 と 60 秒 × 1,440。T14.6)。
///
/// **`.rrd` の標本には足さない。** 標本 1 本の余白は 4 B しか残っていない (T14.2 (3)) ので、
/// 閉じた理由 8 種 + 寿命 13 段 + 上り 13 段 + 下り 13 段 + 合計 5 つ = 52 個の u64 は
/// どうやっても入らない。版を上げれば入るが、上げると統計が全部消える。
/// **ここは再起動で消えてよい**個票と同じ扱い (T14.4 のリングと同じ方針)。
///
/// 書くのは [`crate::metrics::Metrics::record_closed`] = 接続の終了で 1 回だけで、
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

fn push_str_array(out: &mut String, names: &[&str]) {
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", n);
    }
}

fn push_num_array(out: &mut String, v: &[u64]) {
    for (i, n) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}", n);
    }
}

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
}

impl History {
    /// 5 秒の標本を記録し、分・時の窓が閉じたらそれらも作る。
    pub fn push(&self, s: Sample) -> Pushed {
        let mut out = Pushed {
            fine: Some(s),
            ..Pushed::default()
        };
        Self::append(&self.rings[0], s, RESOLUTIONS[0].1);
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
        Self::append(&self.rings[to], agg, RESOLUTIONS[to].1);
        Some(agg)
    }

    fn append(ring: &Mutex<VecDeque<Sample>>, s: Sample, cap: usize) {
        let mut q = ring.locked();
        if q.len() >= cap {
            q.pop_front();
        }
        q.push_back(s);
    }

    /// 起動時に状態ファイルから読み戻す (`res` は 0=5 秒, 1=1 分, 2=1 時間)。
    pub fn restore(&self, res: usize, samples: impl IntoIterator<Item = Sample>) {
        let mut q = self.rings[res].locked();
        q.clear();
        q.extend(samples);
        while q.len() > RESOLUTIONS[res].1 {
            q.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.rings[0].locked().len()
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
        let res = res.min(RESOLUTIONS.len() - 1);
        let q = self.rings[res].locked();
        let mut out = String::with_capacity(256 + q.len() * 360);
        let _ = write!(out, "{{\"interval_secs\":{},\"keys\":[", RESOLUTIONS[res].0);
        for (i, k) in KEYS.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{}\"", k);
        }
        out.push_str("],\"bounds_ms\":[");
        for (i, b) in WINDOW_BOUNDS_MS.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", b);
        }
        out.push_str("],\"causes\":[");
        for (i, c) in crate::metrics::ERR_CAUSE_NAMES.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{}\"", c);
        }
        out.push_str("],\"samples\":[");
        for (i, s) in q.iter().enumerate() {
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
        // 利用者の要求が無い時間帯の名前解決と TCP 接続 (T14.10)。**別の配列**に足す
        // ので、既存の `keys` / `samples` を読む側は 1 行も変えなくてよい
        crate::canary::push_history_json(&mut out, res);
        out.push('}');
        out
    }
}

/// 定期的に記録するスレッドを起動する。記録先は `metrics.history`、`store` があれば状態ファイルにも。
pub fn spawn(
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    store: Option<Arc<crate::persist::Store>>,
) -> JoinHandle<()> {
    spawn_every(metrics, cache, store, INTERVAL)
}

/// 周期を指定して起こす版 (**結合テスト用**。本番は [`spawn`] = [`INTERVAL`])。
///
/// 山の写真 (T14.6) を撮るのがこのスレッドなので、テストで 5 秒待たずに
/// 「越えた → 1 枚撮れた」を見るための口。
pub fn spawn_every(
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    store: Option<Arc<crate::persist::Store>>,
    interval: Duration,
) -> JoinHandle<()> {
    let record = move |metrics: &Arc<Metrics>, cache: &Cache| {
        // 山の写真と、閉じた接続の分布の窓 (T14.6)。**標本より先に**撮るのは、
        // 越えてから撮るまでを 1 周期より短くするため
        metrics.take_burst_shot();
        // 下の層 (IPv4 優先の切替・圧迫・バラスト) の変わり目を出来事に 1 件 (T14.11)
        crate::events::poll(cache);
        metrics.history.closed.roll(crate::cache::now_epoch());
        let sample = Sample::take(metrics, cache);
        // 日付が変わっていたら前日の要約を 1 行残す (T14.20)。書かない設定なら原子の読み 1 回
        crate::daily::tick(metrics, &sample);
        let pushed = metrics.history.push(sample);
        // 積んだあとに、直近 5 分が直近 1 時間の基準値から外れていないかを見る (T14.23)
        crate::anomaly::check(metrics, &sample);
        if let Some(st) = &store {
            st.write_samples(&pushed);
        }
        // 利用者の要求が無い時間帯も待ちを測る (T14.10)。**ここでは測らない**
        // (名前解決と接続は `canary` スレッド 1 本の仕事で、この周期は止めない)
        crate::canary::tick(metrics);
    };
    record(&metrics, &cache);
    thread::Builder::new()
        .name("history".into())
        .spawn(move || {
            loop {
                thread::sleep(interval);
                record(&metrics, &cache);
            }
        })
        .expect("spawn history thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t: u64) -> Sample {
        Sample {
            t,
            requests: t * 2,
            active: (t % 7) as usize,
            mem_used: 100,
            ..Sample::default()
        }
    }

    #[test]
    fn keeps_the_newest_samples_in_order() {
        let h = History::default();
        for t in 0..(CAPACITY as u64 + 5) {
            h.push(sample(t));
        }
        assert_eq!(h.len(), CAPACITY);
        let json = h.to_json();
        // 標本は配列の配列で、キーは先頭に 1 回だけ (T12.4 (3))
        assert!(
            json.starts_with("{\"interval_secs\":5,\"keys\":[\"t\","),
            "{}",
            &json[..80]
        );
        assert!(json.contains("\"samples\":[[5,10,0,5,0,"), "{}", json);
        // 標本の配列の閉じ方は変えず、その**後ろ**に閉じた接続の分布が付く (T14.6)
        assert!(
            json.contains(",0,0,0,0,0,0,0,0]],\"closed\":{"),
            "{}",
            &json[json.len() - 600..]
        );
        // canary (T14.10) は**別の配列**で末尾に付く (既存の列は 1 つも動かない)
        assert!(
            json.ends_with(",\"canary\":{\"keys\":[\"t\",\"canary_dns_ms\",\"canary_connect_ms\",\"canary_host\"],\"samples\":[]}}"),
            "{}",
            &json[json.len() - 120..]
        );
        // 列の数が `KEYS` と合っていること (入れ子の配列は 1 列と数える)
        let first = &json[json.find("\"samples\":[[").unwrap() + 11..];
        let row = &first[..first.find("],[").unwrap() + 1];
        let mut depth = 0;
        let cols = 1 + row
            .chars()
            .filter(|c| {
                match c {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
                *c == ',' && depth == 1
            })
            .count();
        assert_eq!(cols, KEYS.len(), "{}", row);
    }

    #[test]
    fn minute_and_hour_windows_are_rolled_up() {
        let h = History::default();
        let mut minutes = 0;
        let mut hours = 0;
        // 2 時間 + 少し、5 秒刻み
        for i in 0..(2 * 720 + 20) {
            let p = h.push(sample(1_000_000 + i * 5));
            if p.minute.is_some() {
                minutes += 1;
            }
            if p.hour.is_some() {
                hours += 1;
            }
        }
        assert!((119..=123).contains(&minutes), "minutes {}", minutes);
        assert!((1..=3).contains(&hours), "hours {}", hours);
        let m = h.to_json_res(1);
        assert!(m.starts_with("{\"interval_secs\":60,"));
        let q = h.rings[1].lock().unwrap();
        let first = q.front().unwrap();
        assert_eq!(first.t % 60, 0, "window start is aligned");
        // 累計は窓の最後の値 (t + 55 の standard)、ゲージは平均
        assert_eq!(first.requests, (first.t + 55) * 2);
        assert_eq!(first.mem_used, 100);
        let hq = h.rings[2].lock().unwrap();
        assert_eq!(hq.front().unwrap().t % 3600, 0);
    }

    /// T12.4 (3) の受け入れ基準: **2 つの区間に別々の分位点が出る**こと。
    /// 累計しか無かった頃は「起動からの平均」しか読めず、直した前後が同じ数字に混ざっていた。
    #[test]
    fn two_intervals_keep_their_own_medians() {
        let h = History::default();
        let mut slow = sample(1_000_000);
        for _ in 0..100 {
            slow.connect.observe(257);
        }
        let mut fast = sample(1_000_005);
        for _ in 0..100 {
            fast.connect.observe(7);
        }
        h.push(slow);
        h.push(fast);
        let q = h.rings[0].lock().unwrap();
        let p50: Vec<f64> = q.iter().map(|s| s.connect.quantile_ms(0.5)).collect();
        assert_eq!(p50.len(), 2);
        assert!((250.0..=265.0).contains(&p50[0]), "{:?}", p50);
        assert!((5.0..=10.0).contains(&p50[1]), "{:?}", p50);
        // 畳むと 200 件がひとつの分布になる (件数は足し合わせ、最大は大きい方)
        drop(q);
        let agg = Sample::downsample(&[slow, fast], 1_000_000);
        assert_eq!(agg.connect.count, 200);
        assert_eq!(agg.connect.ms_max, 257);
        assert!((agg.connect.avg_ms() - 132.0).abs() < 1e-9);
    }

    /// `/history?res=5` (720 標本) が 512 KiB に収まること (T12.4 (3))。
    /// 値は「1 年動かしたあと」を想定した大きめの桁で埋める (短い数字で測ると通ってしまう)。
    #[test]
    fn a_full_hour_of_samples_fits_in_512_kib() {
        let h = History::default();
        for i in 0..(CAPACITY as u64) {
            let mut s = Sample {
                t: 1_770_000_000 + i * 5,
                requests: 12_345_678 + i,
                bytes: 987_654_321_098 + i,
                active: 240,
                hits: 1_234_567,
                misses: 2_345_678,
                stores: 345_678,
                evictions: 45_678,
                mem_used: 201_326_592,
                mem_limit: 268_435_456,
                disk_used: 2_900_000_000,
                disk_limit: 3_221_225_472,
                rss: 215_900_000,
                errors: 12_345,
                errors_by_cause: [1111, 2222, 3333, 4444, 5555, 6666, 7777, 8888],
                dns_misses: 99_999,
                dns_ms_sum: 888_888,
                threads: 128,
                fds: 1000,
                max_fds: 1024,
                active_max: 240,
                threads_max: 128,
                fds_max: 1010,
                ..Sample::default()
            };
            for b in s.connect.buckets.iter_mut() {
                *b = 999_999;
            }
            s.connect.count = 12_999_987;
            s.connect.ms_sum = 3_333_333_333;
            s.connect.ms_max = 30_000;
            s.forward = s.connect;
            h.push(s);
        }
        let json = h.to_json();
        assert_eq!(h.len(), CAPACITY);
        assert!(
            json.len() <= 512 * 1024,
            "/history?res=5 が {} B (512 KiB 超)",
            json.len()
        );
    }

    #[test]
    fn sample_encoding_round_trips() {
        let mut connect = Window::default();
        connect.observe(257);
        connect.observe(3);
        let mut forward = Window::default();
        forward.observe(9999);
        let s = Sample {
            t: 5,
            requests: 6,
            bytes: 7,
            active: 8,
            hits: 9,
            misses: 10,
            stores: 11,
            evictions: 12,
            mem_used: 13,
            mem_limit: 14,
            disk_used: 15,
            disk_limit: 16,
            rss: 17,
            connect,
            forward,
            errors: 18,
            errors_by_cause: [1, 2, 3, 4, 5, 6, 7, 8],
            dns_misses: 19,
            dns_ms_sum: 20,
            threads: 21,
            fds: 22,
            max_fds: 23,
            active_max: 24,
            threads_max: 25,
            fds_max: 26,
            evicted_idle: 27,
        };
        let enc = s.encode();
        assert!(
            enc.len() <= crate::rrd::SAMPLE_RECORD - 4,
            "{} > {}",
            enc.len(),
            crate::rrd::SAMPLE_RECORD - 4
        );
        assert_eq!(Sample::decode(&enc), Some(s));
        assert_eq!(Sample::decode(&[0u8; 104]), None);
    }

    /// 区間の分位点は区間内を補間し、その窓の最大値で頭打ちになる (T12.4 (3) の受け入れ基準)。
    #[test]
    fn window_quantiles_land_inside_the_bucket() {
        let mut a = Window::default();
        for _ in 0..100 {
            a.observe(257);
        }
        let mut b = Window::default();
        for _ in 0..100 {
            b.observe(7);
        }
        assert!(
            (250.0..=265.0).contains(&a.quantile_ms(0.5)),
            "{}",
            a.quantile_ms(0.5)
        );
        assert!(
            (5.0..=10.0).contains(&b.quantile_ms(0.5)),
            "{}",
            b.quantile_ms(0.5)
        );
        assert_eq!(a.count, 100);
        assert!((a.avg_ms() - 257.0).abs() < 1e-9);
        assert_eq!(Window::default().quantile_ms(0.5), 0.0);
    }

    /// 粗い解像度へ畳むとき、区間の値は足し合わせ、ゲージの山は最大値で残る。
    #[test]
    fn downsampling_sums_the_windows_and_keeps_the_peaks() {
        let mut a = sample(0);
        a.connect.observe(257);
        a.errors = 1;
        a.errors_by_cause[0] = 1;
        a.dns_misses = 2;
        a.active_max = 5;
        a.fds_max = 30;
        let mut b = sample(5);
        b.connect.observe(7);
        b.errors = 2;
        b.errors_by_cause[3] = 2;
        b.dns_misses = 1;
        b.active_max = 41;
        b.fds_max = 12;
        let agg = Sample::downsample(&[a, b], 0);
        assert_eq!(agg.connect.count, 2);
        assert_eq!(agg.connect.ms_max, 257);
        assert_eq!(agg.errors, 3);
        assert_eq!(agg.errors_by_cause[0], 1);
        assert_eq!(agg.errors_by_cause[3], 2);
        assert_eq!(agg.dns_misses, 3);
        // 平均に畳むと 23 になって山が消える
        assert_eq!(agg.active_max, 41);
        assert_eq!(agg.fds_max, 30);
    }

    /// `evicted_idle` は**レコードの余白に足した**ので `.rrd` の版は上がらない (T14.2 (3))。
    ///
    /// 見るのは 2 つ: (a) 63 項目が領域 (508 B) に収まっていて、まだ余白があること、
    /// (b) 版 2 で書かれた 62 項目のレコード (末尾はゼロ埋め) を読むと
    /// `evicted_idle` が 0 になり、**手前の項目は 1 つもずれない**こと。
    /// ずれるような足し方をすると、統計を捨てずに済まなくなる。
    #[test]
    fn evicted_idle_fits_in_the_record_slack_and_old_records_still_decode() {
        let mut s = sample(1_700_000_000);
        s.active_max = 240;
        s.threads_max = 128;
        s.fds_max = 1010;
        s.max_fds = 1024;
        s.evicted_idle = 7;
        let enc = s.encode();
        assert_eq!(enc.len(), 63 * 8, "63 項目 × 8 B");
        assert!(
            enc.len() <= crate::rrd::SAMPLE_RECORD - 4,
            "{} > {} (版を上げずには入らない)",
            enc.len(),
            crate::rrd::SAMPLE_RECORD - 4
        );
        assert_eq!(Sample::decode(&enc), Some(s));

        // 版 2 のレコード = 最後の 1 項目が無く、領域の残りはゼロ埋め
        let mut old = enc[..62 * 8].to_vec();
        old.resize(crate::rrd::SAMPLE_RECORD - 4, 0);
        let back = Sample::decode(&old).expect("版 2 のレコードも読める");
        assert_eq!(back.evicted_idle, 0, "無い項目は 0");
        assert_eq!(
            Sample {
                evicted_idle: 0,
                ..s
            },
            back,
            "手前の項目は 1 つもずれない"
        );
    }
}

/// 閉じた接続の分布の窓 (T14.6)。**`.rrd` には載せない**メモリ上の窓。
#[cfg(test)]
mod closed_tests {
    use super::*;
    use crate::recent::{CLOSED_KEYS, CloseReason, RecentEntry, SIDES, STAGES};

    fn sample(t: u64) -> Sample {
        Sample {
            t,
            ..Sample::default()
        }
    }

    fn closed(reason: CloseReason, secs: u64) -> RecentEntry {
        RecentEntry {
            id: 1,
            at: 1_700_000_000,
            client: "198.51.100.7".to_string(),
            target: "example.net:443".to_string(),
            connect: true,
            secs,
            requests: 0,
            up: 2048,
            down: 4096,
            reason,
            status: 0,
            parked_secs: 1,
            parks: 1,
            stage_ms: [0; STAGES],
            rtt_us: [0; SIDES],
            retrans: [0; SIDES],
        }
    }

    /// 5 秒の窓が閉じ、60 秒の窓はその足し合わせになること (境目は `/history` と同じ)。
    #[test]
    fn windows_close_on_the_same_boundaries_as_the_samples() {
        let w = ClosedWindows::new();
        // 5 秒にも 60 秒にも揃った時刻から始める (窓の始まりを読みやすくするため)
        let t0 = 1_700_000_100;
        assert_eq!(t0 % 60, 0);
        // 最初の呼び出しは「今の窓」を決めるだけ (空の窓は残さない)
        w.roll(t0 + 2);
        assert_eq!(w.counts(), (0, 0, 0));

        w.observe(&closed(CloseReason::ClientEof, 3));
        w.observe(&closed(CloseReason::IdleTimeout, 301));
        w.roll(t0 + 7);
        assert_eq!(w.counts(), (1, 0, 2), "5 秒の窓が 1 つ閉じた");

        w.observe(&closed(CloseReason::ClientEof, 1));
        // 分をまたぐ (60 秒の窓も閉じる)
        w.roll(t0 + 62);
        let (fine, minute, total) = w.counts();
        assert_eq!((fine, minute, total), (2, 1, 3));

        let json = w.to_json_res(0);
        assert!(
            json.starts_with("{\"interval_secs\":5,\"keys\":[\"t\","),
            "{}",
            json
        );
        assert!(json.contains("\"reasons\":[\"client_eof\","), "{}", json);
        assert!(
            json.contains("\"life_bounds_secs\":[1,2,5,10,15,"),
            "{}",
            json
        );
        assert!(json.contains("\"byte_bounds\":[1024,4096,"), "{}", json);
        assert!(json.contains("\"windows\":2"), "{}", json);
        assert!(json.contains("\"recorded\":3"), "{}", json);
        // 窓の始まりは 5 秒に丸めた時刻
        assert!(json.contains("\"samples\":[[1700000100,2,"), "{}", json);
        let minute_json = w.to_json_res(1);
        assert!(
            minute_json.contains("\"interval_secs\":60"),
            "{}",
            minute_json
        );
        assert!(minute_json.contains("[1700000100,3,"), "{}", minute_json);
        // 1 時間の解像度では残していない
        assert_eq!(w.to_json_res(2), "null");
        assert_eq!(CLOSED_KEYS[0], "t");
    }

    /// 件数 0 の窓は残さない (43 本/時 のプロキシで 719 本のゼロ行を作らない)。
    #[test]
    fn empty_windows_are_not_kept() {
        let w = ClosedWindows::new();
        for i in 0..100u64 {
            w.roll(1_700_000_000 + i * 5);
        }
        assert_eq!(w.counts(), (0, 0, 0));
        assert!(w.to_json_res(0).contains("\"samples\":[]"));
    }

    /// 窓は 5 秒 × 720 と 60 秒 × 1,440 で頭打ち。
    #[test]
    fn the_windows_are_capped() {
        let w = ClosedWindows::new();
        for i in 0..(RESOLUTIONS[0].1 as u64 + 50) {
            w.observe(&closed(CloseReason::ClientEof, 1));
            w.roll(1_700_000_000 + (i + 1) * 5);
        }
        let (fine, minute, total) = w.counts();
        assert_eq!(fine, RESOLUTIONS[0].1, "5 秒の窓は 720 で頭打ち");
        assert_eq!(total, RESOLUTIONS[0].1 as u64 + 50);
        assert!(minute > 0 && minute <= RESOLUTIONS[1].1);
    }

    /// `/history` は標本の**後ろに別の配列**として出す (既存の読み方を変えない)。
    #[test]
    fn history_appends_the_closed_windows_after_the_samples() {
        let h = History::default();
        h.push(sample(1_700_000_000));
        h.closed.roll(1_700_000_000);
        h.closed.observe(&closed(CloseReason::Evicted, 7));
        h.closed.roll(1_700_000_005);
        let json = h.to_json_res(0);
        // 既存の形はそのまま
        assert!(
            json.starts_with("{\"interval_secs\":5,\"keys\":[\"t\","),
            "{}",
            json
        );
        let samples_at = json.find("\"samples\":").expect("標本がある");
        let closed_at = json.find("\"closed\":").expect("分布がある");
        assert!(samples_at < closed_at, "分布は標本の後ろ");
        assert!(
            json.contains("\"closed\":{\"interval_secs\":5,"),
            "{}",
            json
        );
        assert!(json.ends_with("}}"), "{}", &json[json.len() - 40..]);
        // 1 時間の解像度は `null`。**末尾で見ない**: T14.10 の `canary` がこのうしろに
        // 付くので、`ends_with` で見ると「別の配列を足したら落ちるテスト」になる
        let hour = h.to_json_res(2);
        assert!(hour.contains(",\"closed\":null"), "{}", hour);
        assert!(hour.ends_with("}"), "{}", hour);
    }

    /// 窓が埋まったときの大きさ (1 窓 ≈ 300 B。`/history` が太る分をここで押さえておく)。
    ///
    /// デプロイ先は 43 本/時 なので、ほとんどの窓は空で**出さない**。ここで見るのは
    /// 「忙しいプロキシで 5 秒 × 720 が全部埋まったとき」の上限で、標本 (`samples`) と
    /// 同じ桁に収まっていること。60 秒 × 1,440 は同じ 1 行の 2 倍の本数になる。
    #[test]
    fn a_full_ring_of_closed_windows_stays_the_same_order_as_the_samples() {
        let w = ClosedWindows::new();
        let mut e = closed(CloseReason::ClientEof, 1234);
        e.up = 987_654_321;
        e.down = 12_345_678_901;
        e.parked_secs = 300;
        for i in 0..(RESOLUTIONS[0].1 as u64 + 5) {
            for _ in 0..200 {
                w.observe(&e);
            }
            w.roll(1_700_000_000 + (i + 1) * 5);
        }
        let json = w.to_json_res(0);
        let windows = json.matches("[1700").count();
        assert_eq!(windows, RESOLUTIONS[0].1, "{} 窓", windows);
        assert!(json.len() <= 512 * 1024, "res=5 が {} B", json.len());
        println!(
            "closed res=5 が満杯のとき: {} B ({} 窓 = 1 窓 {} B。res=60 は 1,440 窓)",
            json.len(),
            windows,
            json.len() / windows
        );
    }

    /// 窓が空なら `/history` に足すのは区間の定義ぶん (472 B) だけで、上限は変わらない。
    #[test]
    fn empty_windows_barely_grow_the_history_response() {
        let h = History::default();
        for i in 0..(CAPACITY as u64) {
            h.push(sample(1_700_000_000 + i * 5));
        }
        let json = h.to_json_res(0);
        assert!(json.len() <= 512 * 1024, "{} B", json.len());
        let tail = &json[json.find("\"closed\":").unwrap()..];
        assert!(tail.len() < 600, "空の窓で {} B", tail.len());
        println!("空の closed が /history に足す分: {} B", tail.len());
    }
}

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
pub mod summary {
    use super::{History, RESOLUTIONS, Sample, Window};
    use crate::metrics::{ERR_CAUSE_NAMES, ERR_CAUSES};
    use crate::sync::LockExt;
    use std::fmt::Write as _;

    /// 「平常時」の閾 (T14.0): **1 時間に 300 本以上**確立した標本はバーストとして外す。
    /// `scripts/snapshot-diff.py` の `BURST_PER_HOUR` と同じ値。
    pub const BURST_PER_HOUR: u64 = 300;

    /// 応答の上限 (T14.24 の受け入れ基準)。標本を返さないので実際は 1 KiB 前後。
    pub const MAX_BODY: usize = 4 * 1024;

    /// 要求された期間と切り方。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Params {
        /// 期間の始まり (epoch 秒。0 = 残っている最古から。`since=restart` は
        /// 呼ぶ側が `now - since_start_secs` にして渡す)
        pub since: u64,
        /// 期間の終わり (epoch 秒。ふつうは今)
        pub until: u64,
        /// `?res=` (秒)。無ければ**期間の長さ**で選ぶ
        pub res: Option<u64>,
        /// 1 時間 300 本以上の標本を外す (T14.0 の「平常時」)
        pub normal_hours_only: bool,
    }

    impl Params {
        /// 使う解像度の添字。`?res=` があればそれ、無ければ期間の長さで選ぶ:
        /// **直近 1 時間は 5 秒、1 日は 60 秒、それ以上は 3,600 秒の窓**
        /// (境目は環状バッファが覆う長さそのもの = 5 秒 × 720 と 60 秒 × 1,440)。
        pub fn res_index(&self) -> usize {
            if let Some(secs) = self.res {
                return History::index_for(secs);
            }
            let span = self.until.saturating_sub(self.since);
            RESOLUTIONS
                .iter()
                .position(|(secs, cap)| span <= secs * *cap as u64)
                .unwrap_or(RESOLUTIONS.len() - 1)
        }
    }

    /// その解像度で「バースト」と見なす 1 標本あたりの本数 (1 時間 300 本を割った値)。
    ///
    /// `scripts/snapshot-diff.py` の `limit` と同じ計算 (四捨五入、下限 1)。
    /// 60 秒の窓では 5 本、5 秒の窓では 1 本 — **5 秒の窓で平常時を切ると
    /// 1 本でも確立した標本は全部外れる**ので、平常時を見るのは 60 秒か 1 時間の窓。
    pub fn burst_limit(interval_secs: u64) -> u64 {
        ((BURST_PER_HOUR * interval_secs + 1800) / 3600).max(1)
    }

    /// 期間を畳んだ結果 (標本は持たない)。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Summary {
        /// 要求された期間 (`since` / `until` をそのまま返す)
        pub from: u64,
        pub to: u64,
        /// 使った解像度 (秒)
        pub interval_secs: u64,
        /// 実際に入った標本の最初と最後の時刻 (窓の始まり。標本が無ければ 0)
        pub first_t: u64,
        pub last_t: u64,
        /// 畳んだ標本の数と、平常時から外した標本の数
        pub samples: u64,
        pub burst_samples: u64,
        pub normal_hours_only: bool,
        /// 期間ぶん足し合わせた確立時間の分布 (CONNECT と forward の初バイト)
        pub connect: Window,
        pub forward: Window,
        pub errors: u64,
        pub errors_by_cause: [u64; ERR_CAUSES],
        pub dns_misses: u64,
        pub dns_ms_sum: u64,
        /// 同時接続数の山 (ゲージなので最大値で畳む)
        pub active_max: u64,
    }

    impl Summary {
        /// 標本 1 本を足す (平常時の判定は呼ぶ側で済ませてある)。
        fn add(&mut self, s: &Sample) {
            self.first_t = if self.samples == 0 {
                s.t
            } else {
                self.first_t.min(s.t)
            };
            self.last_t = self.last_t.max(s.t);
            self.samples += 1;
            self.connect.merge(&s.connect);
            self.forward.merge(&s.forward);
            self.errors += s.errors;
            for (a, b) in self
                .errors_by_cause
                .iter_mut()
                .zip(s.errors_by_cause.iter())
            {
                *a += *b;
            }
            self.dns_misses += s.dns_misses;
            self.dns_ms_sum += s.dns_ms_sum;
            self.active_max = self.active_max.max(s.active_max).max(s.active as u64);
        }

        /// 確立した CONNECT の本数。
        pub fn connects(&self) -> u64 {
            self.connect.count
        }

        /// 名前解決のミス ÷ 確立した CONNECT (T14.0 の 0.55)。
        pub fn dns_miss_per_connect(&self) -> f64 {
            if self.connect.count == 0 {
                0.0
            } else {
                self.dns_misses as f64 / self.connect.count as f64
            }
        }

        /// ミス 1 回の平均 ms (T14.0 の 11.5)。
        pub fn dns_miss_avg_ms(&self) -> f64 {
            if self.dns_misses == 0 {
                0.0
            } else {
                self.dns_ms_sum as f64 / self.dns_misses as f64
            }
        }

        /// `/history?summary=1` の本体。**必ず [`MAX_BODY`] 以下**になる (項目が固定で
        /// 標本を持たないため。桁の上限は 20 桁 × 30 項目 + 名前で 1 KiB 前後)。
        pub fn to_json(&self) -> String {
            let mut out = String::with_capacity(768);
            let _ = write!(
                out,
                "{{\"from\":{},\"to\":{},\"interval_secs\":{},\"first_t\":{},\"last_t\":{},\
                 \"samples\":{},\"burst_samples\":{},\"normal_hours_only\":{},\
                 \"connects\":{},\"p50_ms\":{:.1},\"p95_ms\":{:.1},\"avg_ms\":{:.1},\"max_ms\":{},\
                 \"forwards\":{},\"forward_p50_ms\":{:.1},\"forward_p95_ms\":{:.1},\
                 \"forward_avg_ms\":{:.1},\"forward_max_ms\":{},\
                 \"dns_misses\":{},\"dns_miss_per_connect\":{:.3},\"dns_miss_avg_ms\":{:.1},\
                 \"errors\":{},\"errors_by_cause\":[",
                self.from,
                self.to,
                self.interval_secs,
                self.first_t,
                self.last_t,
                self.samples,
                self.burst_samples,
                self.normal_hours_only,
                self.connect.count,
                self.connect.quantile_ms(0.5),
                self.connect.quantile_ms(0.95),
                self.connect.avg_ms(),
                self.connect.ms_max,
                self.forward.count,
                self.forward.quantile_ms(0.5),
                self.forward.quantile_ms(0.95),
                self.forward.avg_ms(),
                self.forward.ms_max,
                self.dns_misses,
                self.dns_miss_per_connect(),
                self.dns_miss_avg_ms(),
                self.errors,
            );
            for (i, c) in self.errors_by_cause.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{}", c);
            }
            out.push_str("],\"causes\":[");
            super::push_str_array(&mut out, &ERR_CAUSE_NAMES);
            let _ = write!(out, "],\"active_max\":{}}}", self.active_max);
            out
        }
    }

    /// 期間を切って畳む (**`/history` の標本を読むのはここだけ**)。
    ///
    /// 窓は始まりの時刻 (`t`) で切る: `since <= t <= until` の標本が入る
    /// (`scripts/snapshot-diff.py` が再起動時刻で切るのと同じ)。
    pub fn of(h: &History, p: &Params) -> Summary {
        let res = p.res_index();
        let interval = RESOLUTIONS[res].0;
        let limit = p.normal_hours_only.then(|| burst_limit(interval));
        let mut out = Summary {
            from: p.since,
            to: p.until,
            interval_secs: interval,
            normal_hours_only: p.normal_hours_only,
            ..Summary::default()
        };
        let ring = h.rings[res].locked();
        for s in ring.iter().filter(|s| s.t >= p.since && s.t <= p.until) {
            // 平常時 = 1 時間 300 本未満の標本だけ (T14.0)。外した数も返す
            if limit.is_some_and(|l| s.connect.count >= l) {
                out.burst_samples += 1;
                continue;
            }
            out.add(s);
        }
        out
    }
}

/// 期間を切ってサーバー側で畳む要約 (T14.24)。
///
/// 見るのは 4 つ: **T14.0 の表と同じ数字が出ること** (架空の標本列から p50 / p95 が
/// 手計算と一致)、平常時の切り出しがバーストを外すこと、解像度の自動選択、応答が 4 KiB 以下。
#[cfg(test)]
mod summary_tests {
    use super::summary::{Params, Summary, burst_limit, of};
    use super::*;

    /// 1 時間の標本 1 本 (時刻だけ決める)。
    fn hour(t: u64) -> Sample {
        Sample {
            t,
            ..Sample::default()
        }
    }

    /// T14.0 の「平常時」(2026-09-16 の 72.7 時間) と同じ数字になる架空の 72 時間。
    ///
    /// **p50 8.3 / p95 80.7 ms は手で置いた分布から出る値**:
    /// 4,000 本を \[2,5\] に 1,670・\[5,10\] に 500・\[25,50\] に 1,323・\[50,100\] に 500・
    /// \[100,250\] に 7 本置くと、p50 の順位 2,000 は \[5,10\] の 1,670 本目の次から
    /// 330 / 500 = 0.66 の位置 → 5 + 5 × 0.66 = **8.3**、p95 の順位 3,800 は \[50,100\] の
    /// 307 / 500 = 0.614 の位置 → 50 + 50 × 0.614 = **80.7**。
    /// 名前解決は 1 標本 38 ミス・437 ms (= 11.5 ms/ミス) で、4,000 本に対して 0.551 回/接続。
    /// バーストの 14 時間 (1 時間 350 本、500 ms、エラー 99、山 218) は平常時から外れる。
    fn t14_0_hours() -> (Vec<Sample>, u64) {
        let t0 = 1_757_000_000 - 1_757_000_000 % 3600;
        let mut normal: Vec<Sample> = (0..58).map(|i| hour(t0 + i * 3600)).collect();
        for s in normal.iter_mut() {
            s.dns_misses = 38;
            s.dns_ms_sum = 437;
            s.active = 8;
            s.active_max = 8;
        }
        let mut idx = 0usize;
        let mut in_this = 0u64;
        for (ms, n) in [(3u64, 1670u64), (8, 500), (30, 1323), (60, 500), (250, 7)] {
            for _ in 0..n {
                normal[idx].connect.observe(ms);
                in_this += 1;
                // 1 標本 69 本まで (平常時の閾 300 のはるか下)
                if in_this == 69 && idx + 1 < normal.len() {
                    idx += 1;
                    in_this = 0;
                }
            }
        }
        let mut samples = normal;
        for i in 0..14u64 {
            let mut s = hour(t0 + (58 + i) * 3600);
            for _ in 0..350 {
                s.connect.observe(500);
            }
            s.errors = if i == 0 { 99 } else { 0 };
            s.errors_by_cause[0] = s.errors;
            s.dns_misses = 350;
            s.dns_ms_sum = 35_000;
            s.active = 218;
            s.active_max = 218;
            samples.push(s);
        }
        let last = t0 + 71 * 3600;
        (samples, last)
    }

    fn t14_0_summary(normal_hours_only: bool) -> Summary {
        let (samples, last) = t14_0_hours();
        let h = History::default();
        h.restore(2, samples);
        of(
            &h,
            &Params {
                since: 0,
                until: last,
                res: None,
                normal_hours_only,
            },
        )
    }

    /// 受け入れ基準: 既知の標本列から p50 / p95 が**手計算と一致**する。
    #[test]
    fn the_normal_hours_of_t14_0_come_back_with_the_same_p50_and_p95() {
        let s = t14_0_summary(true);
        assert_eq!(
            (s.samples, s.burst_samples),
            (58, 14),
            "平常時 58 / 72 標本"
        );
        assert_eq!(s.interval_secs, 3600, "期間が 1 日を越えるので 1 時間の窓");
        assert_eq!(s.connects(), 4000);
        // 手計算: 5 + 5 × (2000 − 1670)/500 = 8.3、50 + 50 × (3800 − 3493)/500 = 80.7
        assert!(
            (s.connect.quantile_ms(0.5) - 8.3).abs() < 1e-9,
            "p50 {}",
            s.connect.quantile_ms(0.5)
        );
        assert!(
            (s.connect.quantile_ms(0.95) - 80.7).abs() < 1e-9,
            "p95 {}",
            s.connect.quantile_ms(0.95)
        );
        // 名前解決は T14.0 の 0.55 回/接続・ミス 1 回 11.5 ms
        assert!(
            (s.dns_miss_per_connect() - 0.551).abs() < 1e-9,
            "{}",
            s.dns_miss_per_connect()
        );
        assert!(
            (s.dns_miss_avg_ms() - 11.5).abs() < 1e-9,
            "{}",
            s.dns_miss_avg_ms()
        );
        // バーストの時間帯のエラーと山は平常時に入らない (T14.0: 平常時 0、バーストで 99)
        assert_eq!(s.errors, 0);
        assert_eq!(s.active_max, 8);
        let json = s.to_json();
        assert!(json.contains("\"p50_ms\":8.3"), "{}", json);
        assert!(json.contains("\"p95_ms\":80.7"), "{}", json);
        assert!(json.contains("\"dns_miss_per_connect\":0.551"), "{}", json);
        assert!(json.contains("\"dns_miss_avg_ms\":11.5"), "{}", json);
        assert!(json.contains("\"normal_hours_only\":true"), "{}", json);
        // 標本そのものは返さない
        assert!(!json.contains("\"samples\":["), "{}", json);
    }

    /// 平常時で切らなければバーストが混ざり、**同じ期間でも数字が化ける** (T14.0 の (参考) の行)。
    #[test]
    fn without_the_normal_hours_filter_the_bursts_change_the_answer() {
        let s = t14_0_summary(false);
        assert_eq!((s.samples, s.burst_samples), (72, 0));
        assert_eq!(s.connects(), 4000 + 14 * 350);
        // 順位 4,450 は [250,500] の 450 / 4,900 の位置 = 250 + 250 × 0.0918
        assert!(
            (s.connect.quantile_ms(0.5) - 272.9591836734694).abs() < 1e-9,
            "p50 {}",
            s.connect.quantile_ms(0.5)
        );
        assert_eq!(s.errors, 99);
        assert_eq!(s.active_max, 218);
        assert!(s.dns_miss_avg_ms() > 70.0, "{}", s.dns_miss_avg_ms());
        assert!(
            s.to_json().contains("\"normal_hours_only\":false"),
            "{}",
            s.to_json()
        );
    }

    /// 期間は `?res=` があればそれ、無ければ長さで選ぶ (1 時間 → 5 秒、1 日 → 60 秒、それ以上 → 1 時間)。
    #[test]
    fn the_resolution_follows_the_span_unless_res_is_given() {
        let now = 1_757_000_000;
        let span = |secs: u64| Params {
            since: now - secs,
            until: now,
            res: None,
            normal_hours_only: false,
        };
        assert_eq!(span(300).res_index(), 0);
        assert_eq!(span(3600).res_index(), 0, "直近 1 時間は 5 秒の窓");
        assert_eq!(span(3601).res_index(), 1);
        assert_eq!(span(86_400).res_index(), 1, "1 日は 60 秒の窓");
        assert_eq!(span(86_401).res_index(), 2, "それ以上は 1 時間の窓");
        // `since` を書かなければ残っている全部 = いちばん粗い窓
        assert_eq!(
            Params {
                until: now,
                ..Params::default()
            }
            .res_index(),
            2
        );
        for (secs, want) in [(5u64, 0usize), (60, 1), (3600, 2), (7, 0)] {
            assert_eq!(
                Params {
                    res: Some(secs),
                    ..span(86_401)
                }
                .res_index(),
                want,
                "res={}",
                secs
            );
        }
        // 平常時の閾は 1 時間 300 本を解像度で割った値 (`snapshot-diff.py` と同じ)
        assert_eq!(burst_limit(3600), 300);
        assert_eq!(burst_limit(60), 5);
        assert_eq!(burst_limit(5), 1);
    }

    /// 窓は始まりの時刻で切る (`since` / `until` はそのまま返す = `since=restart` の確認に使う)。
    #[test]
    fn the_period_is_cut_at_since_and_until() {
        let h = History::default();
        let t0 = 1_757_000_000;
        let mut samples = Vec::new();
        for i in 0..10u64 {
            let mut s = Sample {
                t: t0 + i * 5,
                ..Sample::default()
            };
            s.connect.observe(10 + i);
            samples.push(s);
        }
        h.restore(0, samples);
        let s = of(
            &h,
            &Params {
                since: t0 + 10,
                until: t0 + 25,
                res: Some(5),
                normal_hours_only: false,
            },
        );
        assert_eq!((s.from, s.to), (t0 + 10, t0 + 25), "要求された期間を返す");
        assert_eq!((s.first_t, s.last_t), (t0 + 10, t0 + 25), "入った標本の端");
        assert_eq!((s.samples, s.connects()), (4, 4));
        // 期間の外の標本は 1 本も入らない
        let none = of(
            &h,
            &Params {
                since: t0 + 1000,
                until: t0 + 2000,
                res: Some(5),
                normal_hours_only: false,
            },
        );
        assert_eq!((none.samples, none.connects()), (0, 0));
        assert!(
            none.to_json().contains("\"p50_ms\":0.0"),
            "{}",
            none.to_json()
        );
    }

    /// 受け入れ基準: 応答 4 KiB 以下 (1 年動かしたあとの桁で埋めても)。
    #[test]
    fn the_summary_response_fits_in_4_kib() {
        let h = History::default();
        let mut s = Sample {
            t: 1_757_000_000,
            active: 240,
            active_max: 240,
            errors: 12_345_678,
            errors_by_cause: [
                111_111_111,
                222_222_222,
                333_333_333,
                444_444_444,
                555_555_555,
                666_666_666,
                777_777_777,
                888_888_888,
            ],
            dns_misses: 99_999_999,
            dns_ms_sum: 888_888_888_888,
            ..Sample::default()
        };
        for b in s.connect.buckets.iter_mut() {
            *b = 999_999_999;
        }
        s.connect.count = 12_999_998_987;
        s.connect.ms_sum = 3_333_333_333_333;
        s.connect.ms_max = 30_000;
        s.forward = s.connect;
        h.restore(2, vec![s]);
        let json = of(
            &h,
            &Params {
                since: 0,
                until: u64::MAX,
                res: Some(3600),
                // 平常時で切ると 1 時間 300 本以上のこの標本は外れて 0 が並ぶので、
                // **大きい桁がそのまま載る**方 (切らない) で上限を見る
                normal_hours_only: false,
            },
        )
        .to_json();
        assert!(
            json.len() <= crate::history::summary::MAX_BODY,
            "/history?summary=1 が {} B (4 KiB 超)",
            json.len()
        );
        println!("summary の大きさ: {} B\n{}", json.len(), json);
    }
}
