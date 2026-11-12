//! 「その窓に閉じた接続」の分布 ([`ClosedCounts`]。`/history` の `closed`。T14.6)。
//! `recent` の一部 (T34.2 で `recent/mod.rs` から移した)。
//!
//! 足すのは [`ClosedCounts::observe`] (接続の終了で 1 回。**既に鍵の内側**)、畳むのは
//! [`ClosedCounts::merge`]、書くのは [`ClosedCounts::push_row`] ([`CLOSED_KEYS`] の順)。
//! 区間の境目 ([`LIFE_BOUNDS_SECS`] / [`BYTE_BOUNDS`]) に落とす [`bucket_of`] は純粋な関数。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `recent` の `pub use` でそのまま。

use super::RecentEntry;

/// 閉じた理由の数 ([`CLOSE_REASON_NAMES`] と同じ。T14.6)。
///
/// [`CloseReason`](super::CloseReason) は 9 種類で、`Error` だけは原因を連れている。分布では
/// **`error:*` を 1 つにまとめる** (原因別は `/status` の `errors_by_cause` と
/// `/errors` にあるので、ここで 8 + 8 = 15 列に増やさない)。**`client_dead` も
/// `shutdown` に畳む** (T14.52。標本の余白の都合で列は 8 つのまま)。
pub(crate) const CLOSE_REASONS: usize = 8;
pub const CLOSE_REASON_NAMES: [&str; CLOSE_REASONS] = [
    "client_eof",
    "server_eof",
    "idle_timeout",
    "keepalive_timeout",
    "evicted",
    "limit",
    "shutdown",
    "error",
];

/// 寿命の区間 (秒)。**12 段** (T14.6)。
///
/// `PROXY_KEEPALIVE_SECS` (15) と `PROXY_TUNNEL_IDLE_SECS` (300) を**区間の境目に
/// 置いてある**: 「その設定で切られた接続がどれだけ居るか」を読むのがこの分布の仕事なので、
/// 境目が設定とずれていると読めない。
pub const LIFE_BOUNDS_SECS: [u64; 12] = [1, 2, 5, 10, 15, 30, 60, 120, 300, 900, 3600, 21600];

/// 上り・下りバイトの区間。**12 段、1 KiB から 4 倍ずつ** (T14.6)。
///
/// 1 KiB (要求 1 本ぶんのヘッダー) から 1 GiB (10 段目) までを 4 倍刻みで覆う。
/// 最後の 1 段 (4 GiB) は「桁が違う 1 本」を上限なしの区間に落とさないための余白。
pub const BYTE_BOUNDS: [u64; 12] = [
    1 << 10,
    1 << 12,
    1 << 14,
    1 << 16,
    1 << 18,
    1 << 20,
    1 << 22,
    1 << 24,
    1 << 26,
    1 << 28,
    1 << 30,
    1 << 32,
];

/// 区間の数 (上限なしの 1 段を足す)。
pub(crate) const LIFE_BUCKETS: usize = LIFE_BOUNDS_SECS.len() + 1;
pub(crate) const BYTE_BUCKETS: usize = BYTE_BOUNDS.len() + 1;

/// 「その窓に閉じた接続」の分布 (T14.6)。**累計ではない。**
///
/// 足すのは `proxy_metrics_core::metrics::Metrics::record_closed` = 接続の終了で 1 回だけで、
/// **既に鍵の内側**なので原子操作もシステムコールも増えない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClosedCounts {
    /// 閉じた接続の数
    pub closed: u64,
    /// 閉じた理由別 ([`CLOSE_REASON_NAMES`] の並び)
    pub reasons: [u64; CLOSE_REASONS],
    /// 寿命の区間 ([`LIFE_BOUNDS_SECS`] + 上限なし)
    pub life: [u64; LIFE_BUCKETS],
    /// 上り / 下りバイトの区間 ([`BYTE_BOUNDS`] + 上限なし)
    pub up: [u64; BYTE_BUCKETS],
    pub down: [u64; BYTE_BUCKETS],
    /// 合計 (平均を出すため)
    pub life_secs_sum: u64,
    pub parked_secs_sum: u64,
    pub parks: u64,
    pub up_bytes: u64,
    pub down_bytes: u64,
}

impl Default for ClosedCounts {
    fn default() -> Self {
        ClosedCounts {
            closed: 0,
            reasons: [0; CLOSE_REASONS],
            life: [0; LIFE_BUCKETS],
            up: [0; BYTE_BUCKETS],
            down: [0; BYTE_BUCKETS],
            life_secs_sum: 0,
            parked_secs_sum: 0,
            parks: 0,
            up_bytes: 0,
            down_bytes: 0,
        }
    }
}

/// `bounds` の何段目か (どれにも収まらなければ最後の「上限なし」)。
fn bucket_of(bounds: &[u64], v: u64) -> usize {
    bounds.iter().position(|&b| v <= b).unwrap_or(bounds.len())
}

impl ClosedCounts {
    /// 閉じた接続 1 本を足す (`/recent` に残す 1 件と同じもの)。
    pub fn observe(&mut self, e: &RecentEntry) {
        self.closed += 1;
        self.reasons[e.reason.index()] += 1;
        self.life[bucket_of(&LIFE_BOUNDS_SECS, e.secs)] += 1;
        self.up[bucket_of(&BYTE_BOUNDS, e.up)] += 1;
        self.down[bucket_of(&BYTE_BOUNDS, e.down)] += 1;
        self.life_secs_sum = self.life_secs_sum.saturating_add(e.secs);
        self.parked_secs_sum = self.parked_secs_sum.saturating_add(e.parked_secs);
        self.parks = self.parks.saturating_add(e.parks as u64);
        self.up_bytes = self.up_bytes.saturating_add(e.up);
        self.down_bytes = self.down_bytes.saturating_add(e.down);
    }

    /// 粗い解像度へ畳むときは足し合わせる (区間の値なので平均でも最後の値でもない)。
    pub fn merge(&mut self, o: &ClosedCounts) {
        self.closed += o.closed;
        for (a, b) in self.reasons.iter_mut().zip(o.reasons.iter()) {
            *a += *b;
        }
        for (a, b) in self.life.iter_mut().zip(o.life.iter()) {
            *a += *b;
        }
        for (a, b) in self.up.iter_mut().zip(o.up.iter()) {
            *a += *b;
        }
        for (a, b) in self.down.iter_mut().zip(o.down.iter()) {
            *a += *b;
        }
        self.life_secs_sum = self.life_secs_sum.saturating_add(o.life_secs_sum);
        self.parked_secs_sum = self.parked_secs_sum.saturating_add(o.parked_secs_sum);
        self.parks = self.parks.saturating_add(o.parks);
        self.up_bytes = self.up_bytes.saturating_add(o.up_bytes);
        self.down_bytes = self.down_bytes.saturating_add(o.down_bytes);
    }

    pub fn is_empty(&self) -> bool {
        self.closed == 0
    }

    /// 1 窓を配列 1 行として書く ([`CLOSED_KEYS`] の順。先頭は窓の始まりの時刻)。
    pub fn push_row(&self, out: &mut String, t: u64) {
        use std::fmt::Write as _;
        let _ = write!(out, "[{},{},", t, self.closed);
        push_u64_array(out, &self.reasons);
        out.push(',');
        push_u64_array(out, &self.life);
        out.push(',');
        push_u64_array(out, &self.up);
        out.push(',');
        push_u64_array(out, &self.down);
        let _ = write!(
            out,
            ",{},{},{},{},{}]",
            self.life_secs_sum, self.parked_secs_sum, self.parks, self.up_bytes, self.down_bytes
        );
    }
}

/// [`ClosedCounts::push_row`] が並べる列の名前 (`/history` の `closed.keys`)。
pub const CLOSED_KEYS: [&str; 11] = [
    "t",
    "closed",
    "reasons",
    "life",
    "up",
    "down",
    "life_secs_sum",
    "parked_secs_sum",
    "parks",
    "up_bytes",
    "down_bytes",
];

fn push_u64_array(out: &mut String, v: &[u64]) {
    use std::fmt::Write as _;
    out.push('[');
    for (i, n) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}", n);
    }
    out.push(']');
}

/// 閉じた分布の単体試験 (T14.6。T34.2 で別のファイルに移した)。
#[cfg(test)]
#[path = "closed_tests.rs"]
mod tests;
