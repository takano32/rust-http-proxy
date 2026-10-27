//! 「これだけは空けておく」安全マージンを動的に決めるコントローラ。
//!
//! 固定の割合や絶対量ではなく、観測から決める:
//!
//! - **変動幅**: 他プロセスの使用量 (全体の使用量 − 自分の保持分) が直近の窓の中で
//!   1 プローブ間にどれだけ動いたかの最大値 × 2。次のプローブまでに他者が伸びても
//!   吸収できる量
//! - **床**: カーネルや FS が健全に動くための最低限 (呼び出し側が渡す)
//! - **活性キャッシュ**: 他者が実際に使っているページキャッシュなど、奪うと性能が落ちる量
//! - **バックオフ**: 圧迫 (PSI / ENOSPC) を観測したら倍々に増やし、平穏が続けば徐々に減らす
//!
//! マージン = これらの最大値 (と手動の最低値)。

use std::collections::VecDeque;

/// 変動幅を見る窓の長さ (秒)。プローブ回数には [`Margin::with_interval`] が直す
/// (T.29.22 でプローブの既定を 1 → 5 秒にしたので、回数で持つと窓が 5 倍に延びる)。
const WINDOW_SECS: u64 = 60;
/// 変動幅に掛ける安全係数。
const VOLATILITY_FACTOR: u64 = 2;
/// 平穏がこの秒数続くごとにバックオフを減衰させる。
const DECAY_SECS: u64 = 30;
/// 圧迫の連続観測でバックオフを再び倍にするまでの最短間隔 (秒)。
/// PSI の avg10 は 10 秒平均なので、1 回の圧迫で毎秒倍々にならないようにする。
const ESCALATE_SECS: u64 = 10;
/// バックオフの上限 (全体の 1/2)。ゼロまで縮退させない。
const BACKOFF_CAP_DIVISOR: u64 = 2;

#[derive(Debug, Clone)]
pub struct Margin {
    /// 窓の長さ・減衰・倍にする間隔 (どれもプローブ回数。秒をプローブの間隔で割ったもの)
    window: usize,
    decay_every: u64,
    escalate_every: u64,
    history: VecDeque<u64>,
    backoff: u64,
    calm_ticks: u64,
    since_escalation: u64,
    manual_floor: u64,
}

impl Margin {
    /// 1 秒ごとにプローブする前提の版。
    pub fn new(manual_floor: u64) -> Self {
        Self::with_interval(manual_floor, 1)
    }

    /// `interval_secs` 秒ごとにプローブする前提の版 (窓・減衰・倍にする間隔を秒で保つ)。
    pub fn with_interval(manual_floor: u64, interval_secs: u64) -> Self {
        let every = |secs: u64| (secs / interval_secs.max(1)).max(1);
        // 隣り合う差を取るので窓は 2 標本以上
        let window = every(WINDOW_SECS).max(2) as usize;
        let escalate_every = every(ESCALATE_SECS);
        Self {
            window,
            decay_every: every(DECAY_SECS),
            escalate_every,
            history: VecDeque::with_capacity(window + 1),
            backoff: 0,
            calm_ticks: 0,
            since_escalation: escalate_every,
            manual_floor,
        }
    }

    /// 他者の使用量を 1 サンプル記録する。
    pub fn observe(&mut self, others_used: u64) {
        if self.history.len() >= self.window {
            self.history.pop_front();
        }
        self.history.push_back(others_used);
    }

    /// 窓の中で隣り合うサンプル間の差の最大値。
    pub fn volatility(&self) -> u64 {
        self.history
            .iter()
            .zip(self.history.iter().skip(1))
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0)
    }

    pub fn backoff(&self) -> u64 {
        self.backoff
    }

    /// 圧迫を観測した。`initial` は最初のバックオフ量 (例: 全体の 5%)。
    /// 倍にするのは [`ESCALATE_SECS`] 秒に 1 度まで、上限は全体の半分。
    pub fn on_pressure(&mut self, initial: u64, total: u64) {
        self.calm_ticks = 0;
        self.since_escalation += 1;
        if self.backoff > 0 && self.since_escalation < self.escalate_every {
            return;
        }
        self.since_escalation = 0;
        self.backoff = self
            .backoff
            .saturating_mul(2)
            .max(initial)
            .min(total / BACKOFF_CAP_DIVISOR);
    }

    /// 圧迫のないプローブが 1 回あった。しばらく続けばバックオフを減衰させる。
    pub fn on_calm(&mut self) {
        self.since_escalation = self.since_escalation.saturating_add(1);
        if self.backoff == 0 {
            return;
        }
        self.calm_ticks += 1;
        if self.calm_ticks.is_multiple_of(self.decay_every) {
            self.backoff = self.backoff * 3 / 4;
        }
    }

    /// 現在のマージン (バイト)。
    pub fn keep_free(&self, floor: u64, hot: u64) -> u64 {
        self.manual_floor
            .max(floor)
            .max(hot)
            .max(self.volatility().saturating_mul(VOLATILITY_FACTOR))
            .max(self.backoff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn volatility_tracks_largest_step_in_window() {
        let mut m = Margin::new(0);
        assert_eq!(m.keep_free(0, 0), 0);
        for v in [100, 120, 90, 300, 310] {
            m.observe(v * MIB);
        }
        assert_eq!(m.volatility(), 210 * MIB);
        assert_eq!(m.keep_free(0, 0), 420 * MIB);
        // 窓から溢れれば忘れる
        for _ in 0..WINDOW_SECS {
            m.observe(310 * MIB);
        }
        assert_eq!(m.volatility(), 0);
    }

    #[test]
    fn floors_and_hot_cache_win_when_larger() {
        let mut m = Margin::new(50 * MIB);
        m.observe(0);
        m.observe(10 * MIB);
        assert_eq!(m.keep_free(30 * MIB, 0), 50 * MIB, "manual floor");
        assert_eq!(m.keep_free(80 * MIB, 0), 80 * MIB, "kernel floor");
        assert_eq!(m.keep_free(0, 200 * MIB), 200 * MIB, "hot cache");
    }

    #[test]
    fn backoff_escalates_once_per_episode_and_decays_when_calm() {
        let mut m = Margin::new(0);
        m.on_pressure(100 * MIB, 1000 * MIB);
        assert_eq!(m.backoff(), 100 * MIB);
        // 圧迫が続いても 10 回未満では倍にならない
        for _ in 0..5 {
            m.on_pressure(100 * MIB, 1000 * MIB);
        }
        assert_eq!(m.backoff(), 100 * MIB);
        for _ in 0..ESCALATE_SECS {
            m.on_pressure(100 * MIB, 1000 * MIB);
        }
        assert_eq!(m.backoff(), 200 * MIB);
        for _ in 0..(ESCALATE_SECS * 10) {
            m.on_pressure(100 * MIB, 1000 * MIB);
        }
        assert_eq!(m.backoff(), 500 * MIB, "capped at half of total");
        for _ in 0..DECAY_SECS {
            m.on_calm();
        }
        assert_eq!(m.backoff(), 375 * MIB);
        assert_eq!(m.keep_free(0, 0), 375 * MIB);
    }

    /// プローブが 5 秒ごとでも窓・減衰・倍にする間隔は**秒で**同じ (T.29.22)。
    #[test]
    fn counts_follow_the_probe_interval() {
        let mut m = Margin::with_interval(0, 5);
        assert_eq!((m.window, m.decay_every, m.escalate_every), (12, 6, 2));
        m.observe(0);
        m.observe(100 * MIB);
        // 60 秒 = 12 標本で忘れる
        for _ in 0..12 {
            m.observe(100 * MIB);
        }
        assert_eq!(m.volatility(), 0);
        m.on_pressure(100 * MIB, 1000 * MIB);
        m.on_pressure(100 * MIB, 1000 * MIB);
        assert_eq!(m.backoff(), 100 * MIB, "10 秒 = 2 回未満では倍にならない");
        m.on_pressure(100 * MIB, 1000 * MIB);
        assert_eq!(m.backoff(), 200 * MIB);
        for _ in 0..6 {
            m.on_calm();
        }
        assert_eq!(m.backoff(), 150 * MIB, "30 秒 = 6 回の平穏で 3/4");
        // 窓より長い間隔でも 2 標本は持つ
        assert_eq!(Margin::with_interval(0, 3600).window, 2);
    }
}
