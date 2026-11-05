//! 標本 1 本の `.rrd` のレコード ([`Sample::encode`] / [`Sample::decode`]) と、固定の欄・予備の数
//! (T34.28 で `history.rs` から移した)。

use super::*;
use crate::rrd::{Dec, Enc};

/// 標本 1 本の**固定の欄の数** (この順で [`Sample::encode`] が並べる)。
///
/// 版 3 の 1 レコードは 1,024 B ([`crate::rrd::SAMPLE_RECORD`]) で、CRC 4 B を除いた
/// 1,020 B のうち固定の欄が `SAMPLE_ITEMS × 8 B`、**残りが予備**
/// ([`SAMPLE_SPARE_ITEMS`] 項目ぶん)。
///
/// **欄を足すときの作法** (T14.14。これを守る限り `.rrd` の版は上がらない =
/// 通算の統計を捨てなくてよい):
///
/// 1. [`Sample`] の**末尾**に欄を足す (手前の欄の位置は絶対に動かさない。動かすと
///    古いレコードが別の意味で読み戻る)
/// 2. [`Sample::encode`] の末尾に `u64` を 1 つ足す (= 予備の先頭を 1 つ使う)
/// 3. [`Sample::decode`] の末尾で読む (古いレコードはそこがゼロ埋めなので 0 になる)
/// 4. この数を増やす ([`Sample::encode`] の `debug_assert` と下の `const` が見張る)
pub const SAMPLE_ITEMS: usize = 124;

/// **版 3 を始めた時点の**固定の欄の数 (T14.14 のときの [`SAMPLE_ITEMS`])。
///
/// 「予備は 60 項目ぶん持って始まった」は**版 3 の割り付けについての事実**で、
/// 欄を足すたびに減ってよい今の予備 ([`SAMPLE_SPARE_ITEMS`]) の話ではない。
/// T14.14 はそれを `SAMPLE_SPARE_ITEMS >= 60` と書き損ねていて、**4 項目を超えて
/// 足すと組み立てで落ちた** (T15.0 (10) で式を直した)。
pub(super) const V3_FIXED_ITEMS: usize = 63;

/// 予備に入る項目数。**これを使い切ったら**版を上げ、[`crate::rrd`] に変換をもう 1 本
/// 足すことになる (T14.14 の版 2 → 版 3 と同じ手順)。
pub const SAMPLE_SPARE_ITEMS: usize = ((crate::rrd::SAMPLE_RECORD - 4) - SAMPLE_ITEMS * 8) / 8;
/// 版 3 を始めた時点の予備 (64 項目)。
pub const V3_SPARE_ITEMS: usize = ((crate::rrd::SAMPLE_RECORD - 4) - V3_FIXED_ITEMS * 8) / 8;
const _: () = assert!(SAMPLE_ITEMS * 8 <= crate::rrd::SAMPLE_RECORD - 4);
const _: () = assert!(
    V3_SPARE_ITEMS >= 60,
    "T14.14: 版 3 の割り付けは 60 項目ぶんの予備を持って始まった"
);
const _: () = assert!(SAMPLE_SPARE_ITEMS >= 1, "予備を使い切った (版を上げること)");

impl Sample {
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
            // **末尾に足すこと** (T14.2)。前からある項目の位置が動くと、古いレコードが
            // 別の意味で読み戻る。足したら `SAMPLE_ITEMS` も 1 つ増やす (T14.14)
            .u64(self.evicted_idle);
        // T15.0 (10) で足した 20 項目。**`decode` と同じ順に並べること** —
        // `/history` の列数しか見ていない検査は順番の食い違いを通してしまい、
        // 統計が別の意味で読み戻る
        self.wait.encode(&mut e);
        e.u64(self.dns_warm)
            .u64(self.requests_delta)
            .u64(self.bytes_delta)
            .u64(self.active_peak)
            // T16.0 で足した 3 項目 (`decode` と同じ順)
            .u64(self.dns_warm_max)
            .u64(self.dns_warm_sum)
            .u64(self.gauge_n);
        // T28.45 で足した 17 項目 (`decode` と同じ順)
        self.excess.encode(&mut e);
        e.u64(self.dns_fail_misses);
        // T29.12 で足した 16 項目 (`decode` と同じ順)
        self.conn_excess.encode(&mut e);
        // T30.3 で足した 2 項目 (`decode` と同じ順)
        e.u64(self.other_port_attempts).u64(self.other_port_errors);
        // T32.16 で足した 1 項目 (`decode` と同じ順)
        e.u64(self.dns_fail_ms_sum);
        // T33.3 で足した 2 項目 (`decode` と同じ順)
        e.u64(self.web_dns_misses).u64(self.web_dns_ms_sum);
        debug_assert_eq!(e.0.len(), SAMPLE_ITEMS * 8, "固定の欄の数と食い違っている");
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
        // 古いレコードはここから先がゼロ埋めなので 0 になる (`Dec` は足りなければ 0)。
        // **ここから下が予備** — 欄を足すならこの位置から (T14.14)
        s.evicted_idle = d.u64();
        // T15.0 (10)。**`encode` と同じ順**。版 3 の 63 項目のレコードはここが
        // ゼロ埋めなので、新しい欄は 0 で読み戻り、手前の欄は 1 つもずれない
        s.wait = Window::decode(&mut d);
        s.dns_warm = d.u64();
        s.requests_delta = d.u64();
        s.bytes_delta = d.u64();
        s.active_peak = d.u64();
        // T16.0。T16.0 より前のレコードはここがゼロ埋めなので 0 で読み戻る
        // (畳むときは `gauge_n == 0` を `(値, 1)` として読む)
        s.dns_warm_max = d.u64();
        s.dns_warm_sum = d.u64();
        s.gauge_n = d.u64();
        // T28.45。それより前のレコードはここがゼロ埋めなので空の窓と 0 で読み戻る
        s.excess = Window::decode(&mut d);
        s.dns_fail_misses = d.u64();
        // T29.12。それより前のレコードはここがゼロ埋めなので空の窓で読み戻る
        s.conn_excess = Window::decode(&mut d);
        // T30.3。それより前のレコードはここがゼロ埋めなので 0 (= 引くものが無い = 全部のポート) で読み戻る
        s.other_port_attempts = d.u64();
        s.other_port_errors = d.u64();
        // T32.16。それより前のレコードはここがゼロ埋めなので 0 (= 引くものが無い) で読み戻る
        s.dns_fail_ms_sum = d.u64();
        // T33.3。それより前のレコードはここがゼロ埋めなので 0 で読み戻る
        s.web_dns_misses = d.u64();
        s.web_dns_ms_sum = d.u64();
        Some(s)
    }
}
