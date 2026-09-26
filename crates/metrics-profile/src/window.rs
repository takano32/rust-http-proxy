//! 時系列の窓そのもの — 記録の間隔と解像度、1 区間ぶんの応答時間 ([`Window`])。
//!
//! **ここに置いてあるのは層の都合**: 段階の窓 ([`crate::profile`]) と転送の窓
//! ([`crate::transfer`]) が同じものを使い、それらを抱える [`crate::history`] は
//! 1 つ上の層に居る (T14.55 でクレートを割ったときに切り出した)。
//! 元の場所は `history` なので、そちらから今までの名前で引ける。

use std::fmt::Write as _;
use std::time::Duration;

use crate::rrd::{Dec, Enc};

/// 記録の間隔。
pub const INTERVAL: Duration = Duration::from_secs(5);

/// **メモリ上の** 5 秒のリングが残す本数 (5 秒 × 4,320 = **6 時間**。T14.32)。
///
/// 1 時間 (720 本) だったのを 6 時間にしたのは、バーストが数時間続く (T14.0 の 09-11 は
/// 17〜23 時) のに 5 秒の解像度が 1 時間しか無く、翌朝には 60 秒に畳んだ鈍った山
/// (`active_max` は最大で残るが p95 は足し合わせで丸くなる) しか読めなかったため。
///
/// **伸ばしたのはメモリだけ**: `.rrd` に書くのは今までどおり最新 720 本
/// ([`crate::rrd::Layout`] の `history_fine` は 720 固定) で、読み戻した 720 本は
/// このリングの末尾に入る。`history::Sample` 504 B × 4,320 = **2.08 MiB** (満杯のとき)。
pub const CAPACITY: usize = 4320;

/// 解像度 (秒) と**`.rrd` に書く本数**。
///
/// メモリ上の 5 秒のリングだけは [`CAPACITY`] (6 時間) まで伸びる (T14.32) ので、
/// ここの 720 が効くのは **`.rrd` の領域**・**閉じた接続と転送の窓**
/// (`history::ClosedWindows` / [`crate::transfer::TransferWindows`])・
/// **`?res=` を書かないときの解像度の自動選択の閾** (1 時間までは 5 秒。[`summary::Params`])
/// の 3 つ。メモリ上の本数が要るところは [`History::capacity`] を使うこと。
pub const RESOLUTIONS: [(u64, usize); 3] = [(5, 720), (60, 1440), (3600, 720)];

/// `/history?res=5` が `n=` を書かないときに返す本数 (T14.32)。
///
/// リングは 6 時間ぶん持つが、**既定の応答の大きさは今までどおり 1 時間ぶん**にする
/// (ダッシュボードも `/snapshot` も `scripts/` も既定で読むため)。遡りたいときだけ
/// `?res=5&n=4320` と書く。
pub const DEFAULT_N: usize = RESOLUTIONS[0].1;

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

    /// **呼ぶのは 1 つ上の層の `history::Sample` だけ** (T14.55 でクレートを割るまでは
    /// 同じクレートの中の `fn` だった)。
    pub fn encode(&self, e: &mut Enc) {
        e.u64(self.count).u64(self.ms_sum).u64(self.ms_max);
        for b in self.buckets {
            e.u64(b);
        }
    }

    /// **呼ぶのは 1 つ上の層の `history::Sample` だけ** (T14.55)。
    pub fn decode(d: &mut Dec<'_>) -> Window {
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

    /// **呼ぶのは 1 つ上の層の `history::Sample` だけ** (T14.55)。
    pub fn push_json(&self, out: &mut String) {
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
