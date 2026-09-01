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

/// 変動幅を見る窓の長さ (プローブ回数)。
const WINDOW: usize = 60;
/// 変動幅に掛ける安全係数。
const VOLATILITY_FACTOR: u64 = 2;
/// 平穏がこの回数続くごとにバックオフを減衰させる。
const DECAY_EVERY: u64 = 30;

#[derive(Debug, Clone)]
pub struct Margin {
    history: VecDeque<u64>,
    backoff: u64,
    calm_ticks: u64,
    manual_floor: u64,
}

impl Margin {
    pub fn new(manual_floor: u64) -> Self {
        Self {
            history: VecDeque::with_capacity(WINDOW + 1),
            backoff: 0,
            calm_ticks: 0,
            manual_floor,
        }
    }

    /// 他者の使用量を 1 サンプル記録する。
    pub fn observe(&mut self, others_used: u64) {
        if self.history.len() >= WINDOW {
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
    pub fn on_pressure(&mut self, initial: u64, total: u64) {
        self.calm_ticks = 0;
        self.backoff = self.backoff.saturating_mul(2).max(initial).min(total);
    }

    /// 圧迫のないプローブが 1 回あった。しばらく続けばバックオフを減衰させる。
    pub fn on_calm(&mut self) {
        if self.backoff == 0 {
            return;
        }
        self.calm_ticks += 1;
        if self.calm_ticks.is_multiple_of(DECAY_EVERY) {
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
        for _ in 0..WINDOW {
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
    fn backoff_doubles_under_pressure_and_decays_when_calm() {
        let mut m = Margin::new(0);
        m.on_pressure(100 * MIB, 1000 * MIB);
        assert_eq!(m.backoff(), 100 * MIB);
        m.on_pressure(100 * MIB, 1000 * MIB);
        assert_eq!(m.backoff(), 200 * MIB);
        for _ in 0..10 {
            m.on_pressure(100 * MIB, 1000 * MIB);
        }
        assert_eq!(m.backoff(), 1000 * MIB, "capped at total");
        for _ in 0..DECAY_EVERY {
            m.on_calm();
        }
        assert_eq!(m.backoff(), 750 * MIB);
        assert_eq!(m.keep_free(0, 0), 750 * MIB);
    }
}
