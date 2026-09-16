//! 異常の自動検知 (`/events` の `anomaly`。T14.23)。
//!
//! 24 時間の図を人が全部読むのは手間がかかる (T14.0 は 72.7 時間ぶんを手で読んだ)。
//! ここは**「いつから」を機械に印させる**もので、履歴スレッドが 5 秒ごとに取る標本
//! ([`crate::history::Sample`]) を直近 5 分の窓に畳み、**直近 1 時間の基準値と比べて
//! 外れた瞬間**を [`crate::events`] のリングに 1 件書く。
//!
//! **費用は 0**: 判定するのは履歴スレッドの周期だけで、要求ごとの経路には 1 命令も無い。
//! 読むのも標本 1 本と原子 2 つ + 山の写真の通算だけで、システムコールは増えない。
//!
//! 判定は **5 種で固定** ([`Kind`])。閾は本文 (T14.23) のとおり:
//!
//! | 種類 | 立つ条件 |
//! |---|---|
//! | `connect_p95` | CONNECT 確立の p95 (直近 5 分) が直近 1 時間の p95 の [`CONNECT_RATIO`] 倍以上、かつ [`CONNECT_MIN_MS`] 以上 |
//! | `dns_slow` | 名前解決のミス 1 回の平均 (直近 5 分) が [`DNS_MISS_MS`] 以上 |
//! | `errors` | エラーが 5 分で [`ERRORS_MIN`] 件以上 |
//! | `active_high` | 同時接続の山が `max_conns` の [`ACTIVE_PERCENT`]% 以上 (T14.6 の写真と同じ閾。写真があればその番号) |
//! | `rejected` | `rejected_overload` / `evicted_idle` / `rejected_client_acl` が増えた |
//!
//! **同じ種類は収まるまで 1 回だけ**書く。条件を外れたまま [`CLEAR_SECS`] 秒
//! (5 分) 続いたら `cleared: <種類>` で始まる解除の 1 件を書き、また立てるようになる。
//! 説明には必ず数字 (何が・いくつ・基準値) を入れる。
//!
//! `connect_p95` だけは**立っている間は「立った瞬間の基準値」と比べる**。基準値の窓
//! (1 時間) は直近の 5 分を含むので、山が 1 時間続くと基準値そのものが山になって倍率が
//! 1 に戻り、**山の最中に解除を書いてしまう** (T14.0 の 2026-09-11 17〜23 時のような
//! 6 時間の山がこれに当たる。単体テストで実際に 2 件目が出た)。
//!
//! 窓は**自前の小さな畳み**で持つ: 直近 5 分は標本そのもの (最大 [`MAX_FINE`] 本)、
//! その外は 5 分ごとに畳んだものを 1 時間ぶん。全部で 20 KiB ほどで、`/history` の
//! リングとは別に持つ (あちらの 720 標本 × 504 B を丸ごと写すと 350 KiB になるため)。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::history::{Sample, Window};
use crate::metrics::{ERR_CAUSE_NAMES, ERR_CAUSES, Metrics};
use crate::sync::LockExt;

/// (1) CONNECT 確立の p95 が基準値の何倍で立つか。
pub const CONNECT_RATIO: f64 = 3.0;
/// (1) かつ、この ms 以上のときだけ (速いところの 3 倍は異常ではない)。
pub const CONNECT_MIN_MS: f64 = 50.0;
/// (2) 名前解決のミス 1 回の平均 (ms)。
pub const DNS_MISS_MS: f64 = 100.0;
/// (3) 5 分のエラー件数。
pub const ERRORS_MIN: u64 = 5;
/// (4) 同時接続の山が `max_conns` のこの割合 (%) 以上で立つ (T14.6 の写真と同じ閾)。
pub const ACTIVE_PERCENT: usize = 50;

/// 直近の窓 (秒)。判定はこの窓の値を [`BASE_SECS`] の窓と比べる。
pub const WINDOW_SECS: u64 = 300;
/// 基準値を取る窓 (秒)。直近の窓もこの中に入っている。
pub const BASE_SECS: u64 = 3600;
/// 条件を外れてから解除の 1 件を書くまで (秒)。
pub const CLEAR_SECS: u64 = WINDOW_SECS;

/// 直近の窓に残す標本の上限 (5 秒周期なら 60 本。時計が戻ったときの歯止め)。
const MAX_FINE: usize = 240;
/// 5 分ごとに畳んだものを残す数 (= 1 時間)。
const MAX_COARSE: usize = (BASE_SECS / WINDOW_SECS) as usize;

/// 判定の種類 (**5 種で固定**。出来事の種類はどれも `anomaly` で、これは説明の頭に出る)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// CONNECT の確立が急に遅くなった
    ConnectP95,
    /// 名前解決のミス 1 回が重い
    DnsSlow,
    /// エラーが続けて出ている
    Errors,
    /// 同時接続が上限に近い
    ActiveHigh,
    /// 上限や接続元の一覧で断った / 追い出した
    Rejected,
}

/// 全種類 (README の一覧と同じ並び)。
pub const KINDS: [Kind; 5] = [
    Kind::ConnectP95,
    Kind::DnsSlow,
    Kind::Errors,
    Kind::ActiveHigh,
    Kind::Rejected,
];

impl Kind {
    /// 説明の頭に出す名前 (`cleared:` のあとに出るのもこれ)。
    pub fn name(self) -> &'static str {
        match self {
            Kind::ConnectP95 => "connect_p95",
            Kind::DnsSlow => "dns_slow",
            Kind::Errors => "errors",
            Kind::ActiveHigh => "active_high",
            Kind::Rejected => "rejected",
        }
    }
}

/// 判定に使う値 1 本 (標本 1 本ぶん)。**テストはここへ直接流し込む** (時刻も `t` で注入する)。
///
/// 前半は「その区間の値」で、畳むときに足し合わせる。後半 (`evicted_idle` 以降) は
/// **累計**で、1 本前との差だけを見る。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point {
    /// いつ (epoch 秒。標本の `t`)
    pub t: u64,
    /// その区間に確立した CONNECT の分布
    pub connect: Window,
    pub errors: u64,
    pub errors_by_cause: [u64; ERR_CAUSES],
    pub dns_misses: u64,
    pub dns_ms_sum: u64,
    /// その区間の同時接続の山
    pub active_max: u64,
    /// 追い出した数の累計 (T13.2)
    pub evicted_idle: u64,
    /// 上限で断った数の累計 (T8.5)
    pub rejected_overload: u64,
    /// 接続元の一覧で断った数の累計 (T14.18)
    pub rejected_client_acl: u64,
    /// 山の写真の通算 (T14.6)。増えていれば「この周期で 1 枚撮れた」= その番号
    pub shots: u64,
}

impl Point {
    /// 標本 1 本と、標本に入っていない累計を集める (履歴スレッドから)。
    fn take(metrics: &Metrics, s: &Sample) -> Point {
        Point {
            t: s.t,
            connect: s.connect,
            errors: s.errors,
            errors_by_cause: s.errors_by_cause,
            dns_misses: s.dns_misses,
            dns_ms_sum: s.dns_ms_sum,
            active_max: s.active_max,
            evicted_idle: s.evicted_idle,
            rejected_overload: metrics.rejected_overload.load(Ordering::Relaxed),
            rejected_client_acl: metrics.rejected_client_acl.load(Ordering::Relaxed),
            // 次に撮る番号の 1 つ前 = いままでに撮った枚数 (= 最後の 1 枚の `seq`)
            shots: metrics.bursts.next_seq().saturating_sub(1),
        }
    }

    /// 区間の値を足し合わせる (累計の欄は触らない。山は最大値)。
    fn merge(&mut self, o: &Point) {
        self.connect.merge(&o.connect);
        self.errors += o.errors;
        for (a, b) in self
            .errors_by_cause
            .iter_mut()
            .zip(o.errors_by_cause.iter())
        {
            *a += *b;
        }
        self.dns_misses += o.dns_misses;
        self.dns_ms_sum += o.dns_ms_sum;
        self.active_max = self.active_max.max(o.active_max);
    }

    /// 名前解決のミス 1 回の平均 (ms)。ミスが無ければ 0。
    fn dns_avg_ms(&self) -> f64 {
        if self.dns_misses == 0 {
            0.0
        } else {
            self.dns_ms_sum as f64 / self.dns_misses as f64
        }
    }
}

/// 立った / 解除された 1 件 (説明はそのまま `/events` の `text` になる)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fired {
    pub kind: Kind,
    /// `true` なら解除 (説明は `cleared: <種類>` で始まる)
    pub cleared: bool,
    pub text: String,
}

/// 1 種類ぶんの状態。
#[derive(Clone, Copy, Debug, Default)]
struct KindState {
    /// いま立っているか (立っている間は同じ種類を二度書かない)
    firing: bool,
    /// 立った時刻
    since: u64,
    /// 条件を外れた最初の時刻 (外れたままここから [`CLEAR_SECS`] 秒で解除)
    calm_since: Option<u64>,
    /// 立った瞬間の基準値 (`connect_p95` だけ使う。下の [`Detector::observe`] を参照)
    base_p95: f64,
}

/// 判定そのもの。**時刻は [`Point::t`] だけを見る** (テストから注入できるように)。
pub struct Detector {
    /// 同時接続数の上限 (`PROXY_MAX_CONNS`。0 なら (4) を判定しない)
    max_conns: usize,
    /// 山と見なす本数 (T14.6 の写真と同じ閾)
    threshold: usize,
    /// 直近 [`WINDOW_SECS`] 秒の標本
    fine: VecDeque<Point>,
    /// その外側を 5 分ごとに畳んだもの (窓の始まりの時刻と値)
    coarse: VecDeque<(u64, Point)>,
    /// 1 本前の累計 (`None` = まだ 1 本も見ていない)
    last_totals: Option<[u64; 3]>,
    /// 1 本前の山の写真の通算
    last_shots: Option<u64>,
    state: [KindState; KINDS.len()],
}

impl Default for Detector {
    fn default() -> Self {
        Detector::new()
    }
}

impl Detector {
    pub fn new() -> Detector {
        Detector {
            max_conns: 0,
            threshold: 0,
            fine: VecDeque::with_capacity(64),
            coarse: VecDeque::with_capacity(MAX_COARSE + 1),
            last_totals: None,
            last_shots: None,
            state: [KindState::default(); KINDS.len()],
        }
    }

    /// 同時接続の上限と、山と見なす本数を教える ([`configure`] が渡す値)。
    pub fn set_limits(&mut self, max_conns: usize, threshold: usize) {
        self.max_conns = max_conns;
        self.threshold = threshold;
    }

    /// 標本 1 本を入れて、**変わり目 (立った / 解除された) だけ**を返す。
    pub fn observe(&mut self, p: Point) -> Vec<Fired> {
        let now = p.t;
        let totals = [p.rejected_overload, p.evicted_idle, p.rejected_client_acl];
        // 1 本目は比べる相手が無いので増分 0 (途中から見始めても古い数で立てない)
        let delta = match self.last_totals.replace(totals) {
            Some(prev) => [
                totals[0].saturating_sub(prev[0]),
                totals[1].saturating_sub(prev[1]),
                totals[2].saturating_sub(prev[2]),
            ],
            None => [0; 3],
        };
        let shot = match self.last_shots.replace(p.shots) {
            Some(prev) if p.shots > prev => Some(p.shots),
            _ => None,
        };
        self.roll(p);
        let w5 = self.fold(false);
        let base = self.fold(true);

        let p95 = w5.connect.quantile_ms(0.95);
        let p95_base = base.connect.quantile_ms(0.95);
        let peak = w5.active_max;
        // **立っている間は「立った瞬間の基準値」と比べる。** 基準値の窓 (1 時間) は
        // 直近の窓を含むので、山が 1 時間続くと基準値そのものが山になって倍率が 1 に戻り、
        // 山の最中に解除を書いてしまう (T14.0 の 17〜23 時のような 6 時間の山がこれ)。
        // 「収まるまで 1 回だけ」の「収まる」は**元の基準値に戻ること**にする
        let p95_ref = if self.state[0].firing {
            self.state[0].base_p95
        } else {
            p95_base
        };
        let hits = [
            // 直近の窓は基準値の中にも入っているので、起動直後 (1 時間ぶんが全部この
            // 5 分) は倍率がちょうど 1 になり、立たない
            p95 >= CONNECT_MIN_MS && p95_ref > 0.0 && p95 >= CONNECT_RATIO * p95_ref,
            w5.dns_avg_ms() >= DNS_MISS_MS,
            w5.errors >= ERRORS_MIN,
            (self.threshold > 0 && peak >= self.threshold as u64) || shot.is_some(),
            delta.iter().any(|&d| d > 0),
        ];

        // 説明を組むのに要るだけ (下で `self.state` を可変に借りるので先に写しておく)
        let limits = (self.max_conns, self.threshold);
        let mut out = Vec::new();
        for (i, &hit) in hits.iter().enumerate() {
            let kind = KINDS[i];
            let st = &mut self.state[i];
            if hit {
                st.calm_since = None;
                if !st.firing {
                    st.firing = true;
                    st.since = now;
                    st.base_p95 = p95_base;
                    out.push(Fired {
                        kind,
                        cleared: false,
                        text: fired_text(kind, &w5, &base, limits, delta, totals, shot),
                    });
                }
                continue;
            }
            if !st.firing {
                continue;
            }
            let calm = *st.calm_since.get_or_insert(now);
            if now.saturating_sub(calm) >= CLEAR_SECS {
                let mins = now.saturating_sub(st.since).div_ceil(60);
                st.firing = false;
                st.calm_since = None;
                out.push(Fired {
                    kind,
                    cleared: true,
                    text: cleared_text(kind, &w5, mins),
                });
            }
        }
        out
    }

    /// 窓を進める: 直近 5 分に入れ、こぼれた分は 5 分ごとに畳み、1 時間より古いものは捨てる。
    fn roll(&mut self, p: Point) {
        let now = p.t;
        self.fine.push_back(p);
        while self
            .fine
            .front()
            .is_some_and(|f| f.t + WINDOW_SECS <= now || self.fine.len() > MAX_FINE)
        {
            let Some(old) = self.fine.pop_front() else {
                break;
            };
            let key = (old.t / WINDOW_SECS) * WINDOW_SECS;
            match self.coarse.back_mut() {
                Some((k, acc)) if *k == key => acc.merge(&old),
                _ => self.coarse.push_back((key, old)),
            }
        }
        while self
            .coarse
            .front()
            .is_some_and(|(k, _)| k + BASE_SECS <= now || self.coarse.len() > MAX_COARSE)
        {
            self.coarse.pop_front();
        }
    }

    /// 窓を 1 つに畳む (`base` が真なら畳んだ分も足す = 直近 1 時間)。
    fn fold(&self, base: bool) -> Point {
        let mut w = Point::default();
        for f in &self.fine {
            w.merge(f);
        }
        if base {
            for (_, c) in &self.coarse {
                w.merge(c);
            }
        }
        w
    }

    /// いま立っている種類 (テストと、途中経過を見るため)。
    pub fn firing(&self) -> Vec<Kind> {
        KINDS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.state[*i].firing)
            .map(|(_, k)| *k)
            .collect()
    }
}

/// ms の書き方 (10 ms 未満は小数 1 桁。「0 ms」と書かないため)。
fn ms(v: f64) -> String {
    if v < 10.0 {
        format!("{:.1}", v)
    } else {
        format!("{:.0}", v)
    }
}

/// エラーの原因の内訳 (多い順に 4 つまで。`/status` と同じ綴り)。
fn causes(counts: &[u64; ERR_CAUSES]) -> String {
    let mut idx: Vec<usize> = (0..ERR_CAUSES).filter(|&i| counts[i] > 0).collect();
    idx.sort_by_key(|&i| std::cmp::Reverse(counts[i]));
    idx.truncate(4);
    let mut s = String::new();
    for (n, &i) in idx.iter().enumerate() {
        if n > 0 {
            s.push_str(", ");
        }
        let _ = write!(s, "{} {}", ERR_CAUSE_NAMES[i], counts[i]);
    }
    s
}

/// 立ったときの説明 (**数字を必ず入れる**: 何が・いくつ・基準値)。
fn fired_text(
    kind: Kind,
    w5: &Point,
    base: &Point,
    limits: (usize, usize),
    delta: [u64; 3],
    totals: [u64; 3],
    shot: Option<u64>,
) -> String {
    let (max_conns, threshold) = limits;
    let head = kind.name();
    match kind {
        Kind::ConnectP95 => {
            let p95 = w5.connect.quantile_ms(0.95);
            let p95_base = base.connect.quantile_ms(0.95);
            format!(
                "{}: connect p95 {} ms over 5m is {:.1}x the 1h baseline {} ms ({} of {} connects)",
                head,
                ms(p95),
                p95 / p95_base.max(f64::MIN_POSITIVE),
                ms(p95_base),
                w5.connect.count,
                base.connect.count
            )
        }
        Kind::DnsSlow => format!(
            "{}: dns miss {} ms avg over 5m (threshold {} ms; {} misses, {} ms total)",
            head,
            ms(w5.dns_avg_ms()),
            DNS_MISS_MS as u64,
            w5.dns_misses,
            w5.dns_ms_sum
        ),
        Kind::Errors => {
            let by = causes(&w5.errors_by_cause);
            format!(
                "{}: {} errors in 5m (threshold {}){}{}",
                head,
                w5.errors,
                ERRORS_MIN,
                if by.is_empty() { "" } else { ": " },
                by
            )
        }
        Kind::ActiveHigh => {
            // 標本 (5 秒ごと) の山が閾に届いていないのに立つのは、**写真の方が先に
            // 気づいた**とき (accept の経路は越えた瞬間に旗を立てるが、標本はその 5 秒の
            // 断面しか見ない)。そのときは「写真が越えた」と書く (山の数字が食い違って見えないように)
            let sampled = w5.active_max;
            if threshold > 0 && sampled >= threshold as u64 {
                let mut s = format!("{}: active connections peaked at {} in 5m", head, sampled);
                if max_conns > 0 {
                    let _ = write!(
                        s,
                        " of max_conns {} ({}%, threshold {})",
                        max_conns,
                        sampled.saturating_mul(100) / max_conns as u64,
                        threshold
                    );
                }
                if let Some(seq) = shot {
                    let _ = write!(s, ", burst shot #{}", seq);
                }
                return s;
            }
            format!(
                "{}: burst shot #{} crossed the threshold {} of max_conns {} (sampled peak {} in 5m)",
                head,
                shot.unwrap_or(0),
                threshold,
                max_conns,
                sampled
            )
        }
        Kind::Rejected => {
            let mut s = format!("{}:", head);
            for (name, d, total) in [
                ("rejected_overload", delta[0], totals[0]),
                ("evicted_idle", delta[1], totals[1]),
                ("rejected_client_acl", delta[2], totals[2]),
            ] {
                if d > 0 {
                    let _ = write!(s, " {} +{} (total {}),", name, d, total);
                }
            }
            s.pop();
            s
        }
    }
}

/// 解除の説明 (**`cleared: <種類>` で始める**。いまの値も入れる)。
fn cleared_text(kind: Kind, w5: &Point, mins: u64) -> String {
    let head = format!("cleared: {} after {}m", kind.name(), mins);
    match kind {
        Kind::ConnectP95 => format!(
            "{} (connect p95 {} ms over 5m, {} connects)",
            head,
            ms(w5.connect.quantile_ms(0.95)),
            w5.connect.count
        ),
        Kind::DnsSlow => format!(
            "{} (dns miss {} ms avg over 5m, {} misses)",
            head,
            ms(w5.dns_avg_ms()),
            w5.dns_misses
        ),
        Kind::Errors => format!("{} ({} errors in 5m)", head, w5.errors),
        Kind::ActiveHigh => format!("{} (active peaked at {} in 5m)", head, w5.active_max),
        Kind::Rejected => format!("{} (no new rejects for {}m)", head, CLEAR_SECS / 60),
    }
}

/// 同時接続数の上限と、山と見なす本数 ([`configure`] が入れる)。
static MAX_CONNS: AtomicUsize = AtomicUsize::new(0);
static THRESHOLD: AtomicUsize = AtomicUsize::new(0);

/// 判定の入れ物。**触るのは履歴スレッドだけ** (5 秒に 1 回)。
static DETECTOR: Mutex<Option<Detector>> = Mutex::new(None);

/// 同時接続の山の閾を教える (`src/main.rs` の起動時に 1 回)。
///
/// `burst_at` は T14.6 の写真と同じ閾 (`PROXY_MAX_CONNS × PROXY_BURST_PERCENT`)。
/// 写真を撮らない設定 (`PROXY_BURST_PERCENT=0` と `--lite`) では [`usize::MAX`] が
/// 来るので、そのときだけ本文どおりの [`ACTIVE_PERCENT`]% を自分で当てる。
pub fn configure(max_conns: usize, burst_at: usize) {
    let threshold = if burst_at == 0 || burst_at == usize::MAX {
        max_conns.saturating_mul(ACTIVE_PERCENT) / 100
    } else {
        burst_at
    };
    MAX_CONNS.store(max_conns, Ordering::Relaxed);
    THRESHOLD.store(threshold, Ordering::Relaxed);
}

/// 標本 1 本を判定して、変わり目だけ `/events` に 1 件書く (**履歴スレッドから**)。
///
/// 要求の経路には 1 命令も足さない。ここで増えるのは原子の読み 2 つ・山の写真の
/// 通算 1 つ・直近 5 分の畳み (5 秒周期なら 60 本 × 21 個の加算) だけ。
pub fn check(metrics: &Metrics, sample: &Sample) {
    let fired = {
        let mut guard = DETECTOR.locked();
        let d = guard.get_or_insert_with(Detector::new);
        d.set_limits(
            MAX_CONNS.load(Ordering::Relaxed),
            THRESHOLD.load(Ordering::Relaxed),
        );
        d.observe(Point::take(metrics, sample))
    };
    // 書くのは鍵の外 (出来事のリングと判定の鍵を同時に握らない)
    for f in fired {
        crate::events::push(crate::events::EventKind::Anomaly, &f.text);
    }
}

/// 覚えている状態を捨てる (テスト用)。
pub fn reset() {
    *DETECTOR.locked() = None;
    MAX_CONNS.store(0, Ordering::Relaxed);
    THRESHOLD.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 5 秒周期の標本 1 本。
    fn point(t: u64) -> Point {
        Point {
            t,
            ..Point::default()
        }
    }

    /// `n` 本の CONNECT が `ms` ミリ秒で確立した標本。
    fn connects(t: u64, n: usize, ms: u64) -> Point {
        let mut p = point(t);
        for _ in 0..n {
            p.connect.observe(ms);
        }
        p
    }

    /// 平常時を `secs` 秒ぶん流す (5 秒ごとに 10 ms の CONNECT が 2 本)。
    fn calm(d: &mut Detector, from: u64, secs: u64) -> (u64, Vec<Fired>) {
        let mut out = Vec::new();
        let mut t = from;
        while t < from + secs {
            out.extend(d.observe(connects(t, 2, 10)));
            t += 5;
        }
        (t, out)
    }

    /// (1) 確立の p95 が基準値の 3 倍以上かつ 50 ms 以上。
    #[test]
    fn a_slow_connect_p95_fires_once_and_clears() {
        let mut d = Detector::new();
        // 1 時間の平常時 (基準値 = 10 ms)
        let (mut t, quiet) = calm(&mut d, 0, BASE_SECS);
        assert!(quiet.is_empty(), "平常時に立ってはいけない: {:?}", quiet);
        // 10 分のバースト (250 ms)
        let mut fired = Vec::new();
        while t < BASE_SECS + 600 {
            fired.extend(d.observe(connects(t, 2, 250)));
            t += 5;
        }
        assert_eq!(fired.len(), 1, "立つのは 1 回だけ: {:?}", fired);
        assert!(!fired[0].cleared);
        assert_eq!(fired[0].kind, Kind::ConnectP95);
        assert!(
            fired[0].text.starts_with("connect_p95: connect p95 "),
            "{}",
            fired[0].text
        );
        assert!(fired[0].text.contains("1h baseline"), "{}", fired[0].text);
        // 平常時に戻して 15 分 (窓から抜けるのに 5 分 + 外れたまま 5 分)
        let (_, back) = calm(&mut d, t, 900);
        assert_eq!(back.len(), 1, "解除も 1 回だけ: {:?}", back);
        assert!(back[0].cleared);
        assert!(
            back[0].text.starts_with("cleared: connect_p95 after "),
            "{}",
            back[0].text
        );
        assert!(d.firing().is_empty());
    }

    /// (2) 名前解決のミス 1 回の平均が 100 ms 以上。
    #[test]
    fn a_slow_dns_miss_fires_once_and_clears() {
        let mut d = Detector::new();
        let (mut t, _) = calm(&mut d, 0, 60);
        let mut fired = Vec::new();
        for _ in 0..12 {
            let mut p = connects(t, 2, 10);
            p.dns_misses = 3;
            p.dns_ms_sum = 3 * 140;
            fired.extend(d.observe(p));
            t += 5;
        }
        assert_eq!(fired.len(), 1, "{:?}", fired);
        assert_eq!(fired[0].kind, Kind::DnsSlow);
        assert_eq!(
            fired[0].text,
            "dns_slow: dns miss 140 ms avg over 5m (threshold 100 ms; 3 misses, 420 ms total)"
        );
        let (_, back) = calm(&mut d, t, 900);
        assert_eq!(back.len(), 1, "{:?}", back);
        assert!(back[0].cleared);
        assert!(
            back[0].text.contains("dns miss 0.0 ms avg over 5m"),
            "{}",
            back[0].text
        );
    }

    /// (3) エラーが 5 分で 5 件以上。
    #[test]
    fn errors_in_five_minutes_fire_once_and_clear() {
        let mut d = Detector::new();
        let (mut t, _) = calm(&mut d, 0, 60);
        let mut fired = Vec::new();
        // 1 標本に 3 件ずつ 2 回 = 6 件 (2 回目で越える)
        for _ in 0..2 {
            let mut p = point(t);
            p.errors = 3;
            p.errors_by_cause[0] = 2;
            p.errors_by_cause[4] = 1;
            fired.extend(d.observe(p));
            t += 5;
        }
        assert_eq!(fired.len(), 1, "{:?}", fired);
        assert_eq!(fired[0].kind, Kind::Errors);
        assert_eq!(
            fired[0].text,
            "errors: 6 errors in 5m (threshold 5): dns 4, reset 2"
        );
        // 窓から抜けるのに 5 分 + 外れたまま 5 分
        let (_, back) = calm(&mut d, t, 900);
        assert_eq!(back.len(), 1, "{:?}", back);
        assert_eq!(back[0].kind, Kind::Errors);
        assert!(
            back[0].text.ends_with("(0 errors in 5m)"),
            "{}",
            back[0].text
        );
    }

    /// (4) 同時接続の山が `max_conns` の 50% 以上。
    #[test]
    fn a_high_active_peak_fires_once_and_clears() {
        let mut d = Detector::new();
        d.set_limits(240, 120);
        let (mut t, _) = calm(&mut d, 0, 60);
        let mut fired = Vec::new();
        for active in [195, 218, 201] {
            let mut p = connects(t, 2, 10);
            p.active_max = active;
            fired.extend(d.observe(p));
            t += 5;
        }
        assert_eq!(fired.len(), 1, "{:?}", fired);
        assert_eq!(fired[0].kind, Kind::ActiveHigh);
        assert_eq!(
            fired[0].text,
            "active_high: active connections peaked at 195 in 5m of max_conns 240 (81%, threshold 120)"
        );
        let (_, back) = calm(&mut d, t, 900);
        assert_eq!(back.len(), 1, "{:?}", back);
        assert!(
            back[0].text.starts_with("cleared: active_high after "),
            "{}",
            back[0].text
        );
    }

    /// (4) 写真が撮れていればその番号を持つ (T14.6 の `seq`)。
    #[test]
    fn a_new_burst_shot_carries_its_number() {
        let mut d = Detector::new();
        d.set_limits(240, 120);
        let mut p = point(0);
        p.shots = 6;
        assert!(d.observe(p).is_empty(), "1 本目は比べる相手が無い");
        let mut p = point(5);
        p.shots = 7;
        p.active_max = 130;
        let fired = d.observe(p);
        assert_eq!(fired.len(), 1);
        assert_eq!(
            fired[0].text,
            "active_high: active connections peaked at 130 in 5m of max_conns 240 (54%, threshold 120), burst shot #7"
        );
    }

    /// 5 秒の標本が山を見逃しても写真が気づく (accept の経路は越えた瞬間に旗を立てる)。
    /// そのときは「写真が越えた」と書く (標本の山と数字が食い違って見えないように)。
    #[test]
    fn a_spike_between_two_samples_is_written_as_the_photo() {
        let mut d = Detector::new();
        d.set_limits(4, 2);
        assert!(d.observe(point(0)).is_empty());
        let mut p = point(5);
        p.shots = 1;
        p.active_max = 1;
        let fired = d.observe(p);
        assert_eq!(fired.len(), 1);
        assert_eq!(
            fired[0].text,
            "active_high: burst shot #1 crossed the threshold 2 of max_conns 4 (sampled peak 1 in 5m)"
        );
    }

    /// (5) 断った / 追い出した数が増えた。
    #[test]
    fn growing_reject_counters_fire_once_and_clear() {
        let mut d = Detector::new();
        let (mut t, _) = calm(&mut d, 0, 60);
        let mut fired = Vec::new();
        let mut p = connects(t, 2, 10);
        p.rejected_overload = 12;
        p.evicted_idle = 3;
        fired.extend(d.observe(p));
        t += 5;
        // 同じ山でもう 1 回増えても 2 件目は書かない
        let mut p = connects(t, 2, 10);
        p.rejected_overload = 20;
        p.evicted_idle = 3;
        fired.extend(d.observe(p));
        t += 5;
        assert_eq!(fired.len(), 1, "{:?}", fired);
        assert_eq!(fired[0].kind, Kind::Rejected);
        assert_eq!(
            fired[0].text,
            "rejected: rejected_overload +12 (total 12), evicted_idle +3 (total 3)"
        );
        // 増えなくなってから 5 分で解除 (累計は 20 / 3 のまま)
        let mut back = Vec::new();
        while t < 60 + 700 {
            let mut p = connects(t, 2, 10);
            p.rejected_overload = 20;
            p.evicted_idle = 3;
            back.extend(d.observe(p));
            t += 5;
        }
        assert_eq!(back.len(), 1, "{:?}", back);
        assert_eq!(
            back[0].text,
            "cleared: rejected after 6m (no new rejects for 5m)"
        );
    }

    /// 5 種が同時に立っても、それぞれ 1 回ずつ (受け入れ基準)。
    #[test]
    fn each_of_the_five_fires_once_and_clears_once() {
        let mut d = Detector::new();
        d.set_limits(240, 120);
        let (mut t, _) = calm(&mut d, 0, BASE_SECS);
        let mut fired = Vec::new();
        let start = t;
        while t < start + 600 {
            let mut p = connects(t, 4, 300);
            p.dns_misses = 2;
            p.dns_ms_sum = 2 * 250;
            p.errors = 3;
            p.errors_by_cause[0] = 3;
            p.active_max = 218;
            p.rejected_overload = (t - start) / 5;
            fired.extend(d.observe(p));
            t += 5;
        }
        let mut kinds: Vec<&str> = fired.iter().map(|f| f.kind.name()).collect();
        kinds.sort_unstable();
        assert_eq!(
            kinds,
            [
                "active_high",
                "connect_p95",
                "dns_slow",
                "errors",
                "rejected"
            ],
            "5 種が 1 回ずつ: {:?}",
            fired
        );
        assert!(fired.iter().all(|f| !f.cleared));
        assert!(
            fired
                .iter()
                .all(|f| f.text.len() <= crate::events::MAX_TEXT)
        );
        // 収まったら 5 種とも解除が 1 回ずつ
        let (_, back) = calm(&mut d, t, 1200);
        let mut kinds: Vec<&str> = back.iter().map(|f| f.kind.name()).collect();
        kinds.sort_unstable();
        assert_eq!(
            kinds,
            [
                "active_high",
                "connect_p95",
                "dns_slow",
                "errors",
                "rejected"
            ],
            "解除も 1 回ずつ: {:?}",
            back
        );
        assert!(back.iter().all(|f| f.cleared));
        assert!(back.iter().all(|f| f.text.len() <= crate::events::MAX_TEXT));
        assert!(d.firing().is_empty());
    }

    /// 静かなプロセスでは 1 件も書かない (2 時間)。
    #[test]
    fn a_quiet_process_writes_nothing() {
        let mut d = Detector::new();
        d.set_limits(240, 120);
        let (t, out) = calm(&mut d, 0, 2 * BASE_SECS);
        assert!(out.is_empty(), "{:?}", out);
        // 標本が 1 本も無い (要求が来ていない) 時間帯も同じ
        let mut t = t;
        let mut out = Vec::new();
        for _ in 0..720 {
            out.extend(d.observe(point(t)));
            t += 5;
        }
        assert!(out.is_empty(), "{:?}", out);
    }

    /// 窓は直近 5 分 (標本そのもの) と直近 1 時間 (5 分ごとに畳んだもの)。
    #[test]
    fn the_windows_keep_five_minutes_and_one_hour() {
        let mut d = Detector::new();
        let (t, _) = calm(&mut d, 0, 2 * BASE_SECS);
        assert_eq!(d.fine.len(), (WINDOW_SECS / 5) as usize, "直近 5 分ぶん");
        assert!(d.coarse.len() <= MAX_COARSE, "{}", d.coarse.len());
        let w5 = d.fold(false);
        let base = d.fold(true);
        assert_eq!(w5.connect.count, 2 * WINDOW_SECS / 5);
        assert!(
            (base.connect.count as i64 - (2 * BASE_SECS / 5) as i64).abs() <= 2 * 60,
            "1 時間ぶん ±5 分: {}",
            base.connect.count
        );
        assert!(t > 0);
    }

    #[test]
    fn the_five_kinds_have_distinct_names() {
        let mut names: Vec<&str> = KINDS.iter().map(|k| k.name()).collect();
        assert_eq!(names.len(), 5);
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 5, "名前が重なっている");
    }

    /// 閾は `PROXY_BURST_PERCENT` と同じ本数。写真を撮らない設定なら本文の 50%。
    #[test]
    fn the_threshold_follows_the_burst_photo() {
        reset();
        configure(240, 120);
        assert_eq!(THRESHOLD.load(Ordering::Relaxed), 120);
        configure(240, usize::MAX);
        assert_eq!(THRESHOLD.load(Ordering::Relaxed), 120, "50% を自分で当てる");
        configure(0, usize::MAX);
        assert_eq!(
            THRESHOLD.load(Ordering::Relaxed),
            0,
            "上限なしなら判定しない"
        );
        reset();
    }

    /// 履歴スレッドの口 ([`check`]) が `/events` に `anomaly` で 1 件書く。
    ///
    /// 出来事のリングは静的に 1 本なので、[`crate::events`] のテストと同じ鍵で 1 つずつ通す。
    #[test]
    fn check_writes_one_anomaly_event() {
        let _g = crate::events::TEST_LOCK.locked();
        crate::events::clear();
        reset();
        let metrics = Metrics::new();
        let mut sample = Sample {
            t: 1_789_251_465,
            ..Sample::default()
        };
        check(&metrics, &sample);
        assert!(crate::events::is_empty(), "1 本目では立たない");
        metrics
            .rejected_client_acl
            .fetch_add(7, std::sync::atomic::Ordering::Relaxed);
        sample.t += 5;
        check(&metrics, &sample);
        let (events, _) = crate::events::select(0, 10);
        assert_eq!(events.len(), 1, "{:?}", events);
        assert_eq!(events[0].kind, crate::events::EventKind::Anomaly);
        assert_eq!(events[0].text, "rejected: rejected_client_acl +7 (total 7)");
        // 同じ種類は収まるまで 1 回だけ
        sample.t += 5;
        check(&metrics, &sample);
        assert_eq!(crate::events::len(), 1);
        crate::events::clear();
        reset();
    }
}
