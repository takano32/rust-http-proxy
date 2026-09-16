//! プロファイル (`/profile`。T14.3): **1 要求 (1 本) の時間がどの段階に消えたか**を
//! メモリ上の窓に残す。
//!
//! `/status` は「ホスト別の合計」、`/history` は「時系列の合計」までしか持たないので、
//! デプロイ先で「どこで待っているか」を推定する材料が無かった (Phase 10〜11 の計測は
//! `perf` / `strace` を手元で回して得たもので、Pterodactyl では両方使えない)。
//!
//! # 持ち方
//!
//! - 窓は **5 秒 × 720 (1 時間) と 60 秒 × 1,440 (1 日)**。[`crate::history`] と同じ形だが、
//!   **`.rrd` には書かない** (メモリだけ。再起動で消えてよい)。
//! - 1 つの段階は [`crate::history::Window`] (件数・合計 ms・最大・12 段の区間)。
//!   区間は [`crate::history::WINDOW_BOUNDS_MS`] と同じ 12 段。
//! - 段階の値を運ぶのは [`crate::metrics::Detail`] の [`crate::metrics::StageMs`] で、
//!   **書くのは `Metrics::record` が既に取っている鍵の内側**。原子操作は 1 つも増えない。
//!
//! # 熱い経路の費用
//!
//! 足すのは**境目の `Instant::now()` だけ** (vDSO。システムコール 0)。CONNECT で 5 回
//! (accept / 要求行を読んだ直後 / ヘッダーを読み終えた直後 / `200` を書いた直後 /
//! 最初の中継バイト)、forward で 4 回 (accept / 要求行 / ヘッダー / オリジンへ送り終えた直後)。
//! **`--lite` では時計も読まない** ([`on`] が偽なら [`mark`] が `None` を返すだけ。T1.4)。
//!
//! 窓のメモリは 1 標本 1,352 B × (720 + 1,440) ≈ **2.9 MB**。`--lite` では標本を 1 本も
//! 作らないので 0 (環状バッファは空のまま)。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::cache::now_epoch;
use crate::history::Window;
use crate::metrics::{Detail, Metrics};
use crate::sync::LockExt;

/// 段階の窓を畳む間隔と本数 (5 秒 × 720 = 1 時間、60 秒 × 1,440 = 1 日)。
pub const RESOLUTIONS: [(u64, usize); 2] = [(5, 720), (60, 1440)];

/// 細かい方の刻み。
pub const TICK: Duration = Duration::from_secs(RESOLUTIONS[0].0);

/// CONNECT 1 本の段階 ([`Stages::connect`] の順)。
///
/// `queue` accept してからワーカーが動き出すまで / `client_read` 要求行を読んでから
/// `Host` まで読み終えるまで / `dns` 名前解決 / `connect` SYN → 確立 /
/// `first_relay` `200 Connection Established` を書いてから最初の中継バイトまで
/// (= トンネル越しの TLS 握手の往復) / `relay` 中継の合計 / `park` 預けられていた合計。
pub const CONNECT_STAGES: [&str; 7] = [
    "queue",
    "client_read",
    "dns",
    "connect",
    "first_relay",
    "relay",
    "park",
];

/// 転送した 1 要求の段階 ([`Stages::forward`] の順)。
///
/// `origin` はオリジンを掴むまで (プール命中なら 0、それ以外は `dns` + `connect`)、
/// `send` は要求をオリジンへ送り終えるまで、`ttfb` は応答ヘッダーを読み終えるまで
/// (キャッシュ HIT では 0)、`body` は本文を流し終えるまで。
pub const FORWARD_STAGES: [&str; 6] = ["queue", "client_read", "origin", "send", "ttfb", "body"];

/// 記録するかどうか。`--lite` では偽で、**時計も読まない**。
static ON: AtomicBool = AtomicBool::new(false);

/// 記録するかどうかを決める (起動時に 1 回だけ呼ぶ)。
pub fn set_enabled(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

/// 記録するか (熱い経路で読むのはこの `Relaxed` の 1 回だけ)。
#[inline]
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// 段階の境目で時計を読む。`--lite` では読まない (旗 1 つの分岐だけ)。
#[inline]
pub fn mark() -> Option<Instant> {
    on().then(Instant::now)
}

/// [`mark`] からの経過 (ms)。`None` (= `--lite`) なら 0。
#[inline]
pub fn elapsed_ms(from: Option<Instant>) -> u32 {
    match from {
        Some(t) => ms_u32(t.elapsed()),
        None => 0,
    }
}

/// `Duration` を ms の `u32` に落とす (49 日で頭打ち。段階の長さには十分)。
#[inline]
pub fn ms_u32(d: Duration) -> u32 {
    d.as_millis().min(u32::MAX as u128) as u32
}

/// その区間に観測した段階 (CONNECT 7 段 + forward 6 段)。
///
/// **[`crate::metrics::Metrics::record`] が取っている鍵の内側でだけ書く。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stages {
    pub connect: [Window; CONNECT_STAGES.len()],
    pub forward: [Window; FORWARD_STAGES.len()],
}

impl Default for Stages {
    fn default() -> Self {
        Stages {
            connect: [Window::default(); CONNECT_STAGES.len()],
            forward: [Window::default(); FORWARD_STAGES.len()],
        }
    }
}

impl Stages {
    /// CONNECT 1 本を足す ([`CONNECT_STAGES`] の順)。
    pub fn observe_connect(&mut self, d: &Detail) {
        let v = [
            d.stages.queue as u64,
            d.stages.client_read as u64,
            d.dns_ms,
            d.connect_ms,
            d.stages.first_relay as u64,
            d.stages.relay as u64,
            d.stages.park as u64,
        ];
        for (w, ms) in self.connect.iter_mut().zip(v) {
            w.observe(ms);
        }
    }

    /// 転送した 1 要求を足す ([`FORWARD_STAGES`] の順)。
    pub fn observe_forward(&mut self, d: &Detail) {
        let v = [
            d.stages.queue as u64,
            d.stages.client_read as u64,
            // プールに当たった要求は `origin_detail` を呼ばないので 0 のまま
            d.dns_ms + d.connect_ms,
            d.stages.send as u64,
            d.first_byte_ms.unwrap_or(0),
            d.stages.body as u64,
        ];
        for (w, ms) in self.forward.iter_mut().zip(v) {
            w.observe(ms);
        }
    }

    /// 粗い解像度へ畳むときは足し合わせる (区間の値なので平均でも最後の値でもない)。
    pub fn merge(&mut self, o: &Stages) {
        for (a, b) in self.connect.iter_mut().zip(o.connect.iter()) {
            a.merge(b);
        }
        for (a, b) in self.forward.iter_mut().zip(o.forward.iter()) {
            a.merge(b);
        }
    }

    /// 1 本でも観測したか (JSON を小さくするための判定)。
    pub fn is_empty(&self) -> bool {
        self.connect
            .iter()
            .chain(self.forward.iter())
            .all(|w| w.count == 0)
    }
}

/// 1 つの窓 (5 秒 または 60 秒) の中身。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sample {
    /// 窓の先頭 (epoch 秒)
    pub t: u64,
    /// その窓に処理した要求数 (`total_requests` の増分)
    pub requests: u64,
    /// その窓にプロセスが使った CPU (us。`/proc/self/stat` の utime + stime の増分)
    pub cpu_us: u64,
    pub stages: Stages,
}

impl Sample {
    /// **プロセスの CPU/要求** (us)。§2 の loopback の 41 us/要求 と同じ物差し。
    pub fn cpu_per_request_us(&self) -> Option<f64> {
        (self.requests > 0).then(|| self.cpu_us as f64 / self.requests as f64)
    }

    fn merge_into(&mut self, o: &Sample) {
        self.requests += o.requests;
        self.cpu_us += o.cpu_us;
        self.stages.merge(&o.stages);
    }

    /// `[t,requests,cpu_us,[connect...],[forward...]]`。
    /// **件数 0 の段階は `0` 1 文字**で書く (静かな窓を小さくするため)。
    fn push_row(&self, out: &mut String) {
        let _ = write!(out, "[{},{},{},[", self.t, self.requests, self.cpu_us);
        push_windows(out, &self.connect_windows());
        out.push_str("],[");
        push_windows(out, &self.forward_windows());
        out.push_str("]]");
    }

    fn connect_windows(&self) -> [Window; CONNECT_STAGES.len()] {
        self.stages.connect
    }

    fn forward_windows(&self) -> [Window; FORWARD_STAGES.len()] {
        self.stages.forward
    }
}

/// 窓の並びを JSON に書く (件数 0 は `0`、それ以外は `[count,sum,max,[buckets]]`)。
fn push_windows(out: &mut String, ws: &[Window]) {
    for (i, w) in ws.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if w.count == 0 {
            out.push('0');
            continue;
        }
        let _ = write!(out, "[{},{},{},[", w.count, w.ms_sum, w.ms_max);
        for (j, b) in w.buckets.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", b);
        }
        out.push_str("]]");
    }
}

/// 段階の窓の環状バッファ (5 秒 × 720 と 60 秒 × 1,440)。
#[derive(Default)]
pub struct Profile {
    rings: [Mutex<VecDeque<Sample>>; 2],
}

impl Profile {
    /// 5 秒の標本を足し、1 分の窓が閉じていればそれも作る。
    pub fn push(&self, s: Sample) {
        Self::append(&self.rings[0], s, RESOLUTIONS[0].1);
        self.roll(s.t);
    }

    /// 直前の 1 分の窓がまだ無ければ、5 秒の標本から作る ([`crate::history::History`] と同じ形)。
    fn roll(&self, now: u64) {
        let step = RESOLUTIONS[1].0;
        let window_start = (now / step) * step;
        let Some(prev) = window_start.checked_sub(step) else {
            return;
        };
        if self.rings[1].locked().back().is_some_and(|s| s.t >= prev) {
            return;
        }
        let src = self.rings[0].locked();
        let window: Vec<Sample> = src
            .iter()
            .filter(|s| s.t >= prev && s.t < window_start)
            .copied()
            .collect();
        drop(src);
        if window.is_empty() {
            return;
        }
        let mut agg = Sample {
            t: prev,
            ..Sample::default()
        };
        for s in &window {
            agg.merge_into(s);
        }
        Self::append(&self.rings[1], agg, RESOLUTIONS[1].1);
    }

    fn append(ring: &Mutex<VecDeque<Sample>>, s: Sample, cap: usize) {
        let mut q = ring.locked();
        if q.len() >= cap {
            q.pop_front();
        }
        q.push_back(s);
    }

    /// 解像度 (秒) から添字を引く。合わなければ 5 秒。
    pub fn index_for(secs: u64) -> usize {
        RESOLUTIONS
            .iter()
            .position(|(s, _)| *s == secs)
            .unwrap_or(0)
    }

    /// その解像度の標本数。
    pub fn len(&self, res: usize) -> usize {
        self.rings[res.min(1)].locked().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len(0) == 0
    }

    /// 直近 5 分ぶん (5 秒 × 60) の段階を足し合わせる (画面の積み上げ用)。
    pub fn recent(&self, res: usize, n: usize) -> Stages {
        let q = self.rings[res.min(1)].locked();
        let mut out = Stages::default();
        for s in q.iter().skip(q.len().saturating_sub(n)) {
            out.merge(&s.stages);
        }
        out
    }

    /// **新しい順に** `budget` バイトまで書けるだけ集め、古い順に並べて返す。
    /// 返すのは (JSON の並び, 全体の件数, 打ち切ったか)。
    pub fn rows_within(&self, res: usize, budget: usize) -> (String, usize, bool) {
        let q = self.rings[res.min(1)].locked();
        let total = q.len();
        let mut rows: Vec<String> = Vec::new();
        let mut used = 0usize;
        let mut cut = false;
        for s in q.iter().rev() {
            let mut row = String::with_capacity(160);
            s.push_row(&mut row);
            if used + row.len() + 1 > budget {
                cut = true;
                break;
            }
            used += row.len() + 1;
            rows.push(row);
        }
        drop(q);
        rows.reverse();
        (rows.join(","), total, cut)
    }
}

/// 1 標本ぶんの「前回との差」を取るための覚え書き。
struct Tick {
    requests: u64,
    cpu_us: u64,
}

/// `/proc/self/stat` の utime + stime (us)。読めなければ `None`。
pub fn process_cpu_us() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    parse_stat_cpu_us(&text)
}

/// `/proc/<pid>/stat` の utime (14) + stime (15) を us で返す。
///
/// **comm は括弧で囲まれていて空白も括弧も含みうる**ので、最後の `)` から後ろを数える。
pub fn parse_stat_cpu_us(text: &str) -> Option<u64> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut it = rest.split_whitespace();
    // 最後の ')' の次は state (3 番目の項目) なので、utime は 11 個先
    let utime: u64 = it.nth(11)?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    Some((utime + stime) * (1_000_000 / clock_tick()))
}

/// `sysconf(_SC_CLK_TCK)`。Linux では実質いつも 100。
fn clock_tick() -> u64 {
    #[cfg(target_os = "linux")]
    {
        const SC_CLK_TCK: i32 = 2;
        unsafe extern "C" {
            fn sysconf(name: i32) -> i64;
        }
        // SAFETY: 引数も戻り値も整数だけの読み取り専用の問い合わせ
        let v = unsafe { sysconf(SC_CLK_TCK) };
        if v > 0 {
            return v as u64;
        }
    }
    100
}

/// 段階の窓を畳むスレッドを起こす (`profile-sample`)。
///
/// **`--lite` では呼ばない** (窓を 1 本も作らない)。
pub fn spawn(metrics: std::sync::Arc<Metrics>) -> JoinHandle<()> {
    thread::Builder::new()
        .name("profile-sample".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut prev = Tick {
                requests: metrics.total_requests.load(Ordering::Relaxed),
                cpu_us: process_cpu_us().unwrap_or(0),
            };
            loop {
                thread::sleep(TICK);
                tick(&metrics, &mut prev);
            }
        })
        .expect("spawn profile-sample thread")
}

/// 1 標本ぶんを窓へ (`spawn` のループの中身。テストからも呼ぶ)。
fn tick(metrics: &Metrics, prev: &mut Tick) {
    let requests = metrics.total_requests.load(Ordering::Relaxed);
    let cpu_us = process_cpu_us().unwrap_or(0);
    let s = Sample {
        t: now_epoch(),
        requests: requests.saturating_sub(prev.requests),
        cpu_us: cpu_us.saturating_sub(prev.cpu_us),
        stages: metrics.take_stages(),
    };
    *prev = Tick { requests, cpu_us };
    metrics.profile.push(s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{Detail, StageMs};

    fn detail(stages: StageMs, dns: u64, connect: u64, ttfb: Option<u64>) -> Detail {
        Detail {
            dns_ms: dns,
            connect_ms: connect,
            first_byte_ms: ttfb,
            stages,
            ..Detail::default()
        }
    }

    #[test]
    fn a_connect_lands_in_all_seven_stages() {
        let mut s = Stages::default();
        s.observe_connect(&detail(
            StageMs {
                queue: 1,
                client_read: 2,
                first_relay: 30,
                relay: 400,
                park: 5000,
                ..StageMs::default()
            },
            6,
            9,
            None,
        ));
        let got: Vec<u64> = s.connect.iter().map(|w| w.ms_sum).collect();
        assert_eq!(got, vec![1, 2, 6, 9, 30, 400, 5000]);
        assert!(s.connect.iter().all(|w| w.count == 1));
        assert!(s.forward.iter().all(|w| w.count == 0));
    }

    #[test]
    fn a_forward_request_lands_in_all_six_stages() {
        let mut s = Stages::default();
        s.observe_forward(&detail(
            StageMs {
                queue: 1,
                client_read: 2,
                send: 3,
                body: 40,
                ..StageMs::default()
            },
            6,
            9,
            Some(20),
        ));
        let got: Vec<u64> = s.forward.iter().map(|w| w.ms_sum).collect();
        // origin = dns + connect
        assert_eq!(got, vec![1, 2, 15, 3, 20, 40]);
    }

    /// 粗い窓へは足し合わせる (件数も区間も)。
    #[test]
    fn merging_adds_up() {
        let mut a = Stages::default();
        let mut b = Stages::default();
        a.observe_connect(&detail(StageMs::default(), 3, 0, None));
        b.observe_connect(&detail(StageMs::default(), 7, 0, None));
        a.merge(&b);
        assert_eq!(a.connect[2].count, 2);
        assert_eq!(a.connect[2].ms_sum, 10);
        assert_eq!(a.connect[2].ms_max, 7);
    }

    /// 1 分の窓は 5 秒の標本 12 本から作られ、環状バッファは上限で古いものを捨てる。
    #[test]
    fn the_minute_window_is_built_from_the_five_second_ones() {
        let p = Profile::default();
        let base = 1_800_000_000u64; // 分の頭 (60 で割り切れる)
        for i in 0..12 {
            let mut st = Stages::default();
            st.observe_connect(&detail(StageMs::default(), 10, 0, None));
            p.push(Sample {
                t: base + i * 5,
                requests: 2,
                cpu_us: 100,
                stages: st,
            });
        }
        // まだ次の分に入っていないので 1 分の窓はできない
        assert_eq!(p.len(1), 0);
        p.push(Sample {
            t: base + 60,
            ..Sample::default()
        });
        assert_eq!(p.len(1), 1);
        let minute = p.recent(1, 10);
        assert_eq!(minute.connect[2].count, 12);
        assert_eq!(minute.connect[2].ms_sum, 120);
    }

    /// 上限を超えたら古い標本から捨てる。
    #[test]
    fn the_ring_keeps_only_the_capacity() {
        let p = Profile::default();
        for i in 0..(RESOLUTIONS[0].1 as u64 + 10) {
            p.push(Sample {
                t: 1_000_000 + i * 5,
                ..Sample::default()
            });
        }
        assert_eq!(p.len(0), RESOLUTIONS[0].1);
    }

    /// 件数 0 の段階は `0` 1 文字で書く (静かな窓を小さくする)。
    #[test]
    fn empty_stages_are_written_as_a_single_zero() {
        let mut row = String::new();
        Sample {
            t: 7,
            ..Sample::default()
        }
        .push_row(&mut row);
        assert_eq!(row, "[7,0,0,[0,0,0,0,0,0,0],[0,0,0,0,0,0]]");
    }

    /// 予算に入らない古い標本は落とし、`truncated` を立てる。
    #[test]
    fn rows_are_cut_to_the_budget_newest_first() {
        let p = Profile::default();
        for i in 0..50u64 {
            p.push(Sample {
                t: 1_000_000 + i * 5,
                requests: i,
                ..Sample::default()
            });
        }
        let (all, total, cut) = p.rows_within(0, 1 << 20);
        assert_eq!(total, 50);
        assert!(!cut);
        assert!(all.starts_with("[1000000,0,"), "{}", &all[..24]);
        let (small, _, cut2) = p.rows_within(0, 120);
        assert!(cut2, "予算に入り切らなければ打ち切る");
        assert!(small.len() <= 120);
        // 残るのは**新しい方**
        assert!(small.contains("[1000245,49,"), "{}", small);
    }

    /// `/proc/<pid>/stat` の comm に空白と括弧が入っていても utime / stime を読める。
    #[test]
    fn the_stat_line_is_parsed_after_the_last_paren() {
        let mut line = String::from("42 (weird ) name) S 1 42 42 0 -1 4194304 0 0 0 0 ");
        line.push_str("111 222 0 0 20 0 8 0 100 0 0");
        let us = parse_stat_cpu_us(&line).expect("読めること");
        assert_eq!(us, (111 + 222) * (1_000_000 / clock_tick()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_has_used_some_cpu() {
        assert!(process_cpu_us().is_some());
    }
}
