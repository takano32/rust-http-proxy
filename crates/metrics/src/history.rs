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
}

/// `/history` の 1 標本の列名 (この順で [`Sample::push_row`] が値を並べる)。
/// **キーを標本ごとに繰り返さない**ため、JSON は配列の配列にしてある (T12.4 (3))。
pub const KEYS: [&str; 31] = [
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
];

impl Sample {
    pub fn take(metrics: &Metrics, cache: &Cache) -> Self {
        let (mem_used, _) = cache.mem_usage();
        let (disk_used, _) = cache.disk_usage();
        let active = metrics.active_connections.load(Ordering::Relaxed);
        let iv = metrics.take_interval();
        // `/proc` を読むのは 5 秒の標本のときだけ (要求ごとには読まない)
        let (threads, fds, max_fds) = process_counts();
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
            "],{},{},{},{},{},{},{}]",
            self.dns_misses,
            self.dns_ms_sum,
            self.threads,
            self.threads_max,
            self.fds,
            self.fds_max,
            self.max_fds
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
            .u64(self.fds_max);
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
        out.push_str("]}");
        out
    }
}

/// 定期的に記録するスレッドを起動する。記録先は `metrics.history`、`store` があれば状態ファイルにも。
pub fn spawn(
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    store: Option<Arc<crate::persist::Store>>,
) -> JoinHandle<()> {
    let record = move |metrics: &Metrics, cache: &Cache| {
        let pushed = metrics.history.push(Sample::take(metrics, cache));
        if let Some(st) = &store {
            st.write_samples(&pushed);
        }
    };
    record(&metrics, &cache);
    thread::Builder::new()
        .name("history".into())
        .spawn(move || {
            loop {
                thread::sleep(INTERVAL);
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
        assert!(
            json.ends_with(",0,0,0,0,0,0,0]]}"),
            "{}",
            &json[json.len() - 60..]
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
}
