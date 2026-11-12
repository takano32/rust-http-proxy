//! 段階の窓 (CONNECT 7 段 + forward 6 段)。

use crate::metrics::Detail;
use crate::window::Window;

use super::{CONNECT_STAGES, FORWARD_STAGES};

/// その区間に観測した段階 (CONNECT 7 段 + forward 6 段)。
///
/// **`crate::metrics::Metrics::record` が取っている鍵の内側でだけ書く。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stages {
    /// **0 ms だった観測の数** (CONNECT の 7 段 → forward の 6 段の順)。
    ///
    /// 0 ms は [`Window`] の `count` と `buckets[0]` (`≤ 1 ms` の区間) にしか効かないので、
    /// 熱い経路ではこの `u32` を 1 つ増やすだけにして、読むときに足し戻す
    /// ([`Stages::folded_connect`])。**要求ごとに触る共有のバイト数が 624 B → 52 B** に減る
    /// (段階がすべて 1 ms 未満の loopback はこちらしか通らない。`Metrics::record` の鍵は
    /// 全接続スレッドが共有しているので、書く範囲が広いほどキャッシュ行が飛び交う)
    zeros: [u32; CONNECT_STAGES.len() + FORWARD_STAGES.len()],
    connect: [Window; CONNECT_STAGES.len()],
    forward: [Window; FORWARD_STAGES.len()],
}

impl Default for Stages {
    fn default() -> Self {
        Stages {
            zeros: [0; CONNECT_STAGES.len() + FORWARD_STAGES.len()],
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
        for (i, ms) in v.into_iter().enumerate() {
            if ms == 0 {
                self.zeros[i] += 1;
            } else {
                self.connect[i].observe(ms);
            }
        }
    }

    /// 転送した 1 要求を足す ([`FORWARD_STAGES`] の順)。
    pub fn observe_forward(&mut self, d: &Detail) {
        let v = [
            d.stages.queue as u64,
            d.stages.client_read as u64,
            // プールに当たった要求は `Detail::of_connect` を呼ばないので 0 のまま
            d.dns_ms + d.connect_ms,
            d.stages.send as u64,
            d.first_byte_ms.unwrap_or(0),
            d.stages.body as u64,
        ];
        for (i, ms) in v.into_iter().enumerate() {
            if ms == 0 {
                self.zeros[CONNECT_STAGES.len() + i] += 1;
            } else {
                self.forward[i].observe(ms);
            }
        }
    }

    /// 0 ms のぶんを足し戻した CONNECT の窓 (読むときに 1 回だけ組み立てる)。
    pub fn folded_connect(&self) -> [Window; CONNECT_STAGES.len()] {
        let mut out = self.connect;
        for (i, w) in out.iter_mut().enumerate() {
            fold_zeros(w, self.zeros[i]);
        }
        out
    }

    /// 0 ms のぶんを足し戻した forward の窓。
    pub fn folded_forward(&self) -> [Window; FORWARD_STAGES.len()] {
        let mut out = self.forward;
        for (i, w) in out.iter_mut().enumerate() {
            fold_zeros(w, self.zeros[CONNECT_STAGES.len() + i]);
        }
        out
    }

    /// 粗い解像度へ畳むときは足し合わせる (区間の値なので平均でも最後の値でもない)。
    pub fn merge(&mut self, o: &Stages) {
        for (a, b) in self.zeros.iter_mut().zip(o.zeros.iter()) {
            *a += *b;
        }
        for (a, b) in self.connect.iter_mut().zip(o.connect.iter()) {
            a.merge(b);
        }
        for (a, b) in self.forward.iter_mut().zip(o.forward.iter()) {
            a.merge(b);
        }
    }

    /// 1 本でも観測したか (JSON を小さくするための判定)。
    pub fn is_empty(&self) -> bool {
        self.zeros.iter().all(|z| *z == 0)
            && self
                .connect
                .iter()
                .chain(self.forward.iter())
                .all(|w| w.count == 0)
    }
}

/// 0 ms の観測を窓に足し戻す (`count` と `≤ 1 ms` の区間だけ)。
fn fold_zeros(w: &mut Window, zeros: u32) {
    if zeros == 0 {
        return;
    }
    w.count += zeros as u64;
    w.buckets[0] += zeros as u64;
}
