//! 直近の窓の正確な分位点 (T14.31)。標本を **1,024 本そのまま**持つ。
//!
//! `/history` と `/status` の p50 / p95 は **12 段 (ホスト別は 24 段) の区間の内側を
//! 線形補間した値**で (T12.4 (3))、5〜10 ms の区間に入る限り 5.0〜10.0 のどこかを
//! 「それらしく」返す。T14.99 の判定「平常時の CONNECT 確立 p50 8.3 → 6 ms 以下」の
//! ように **1 ms 単位で読む**には、区間では足りない。
//!
//! ここは**確立時間そのもの**を、CONNECT (確立) と forward (初バイト) 別々の
//! 1,024 本の環状に持つ。直近 1,024 本ぶんの p50 / p90 / p99 / 最大が**正確に**出る。
//!
//! 3 本目の `wait` は**利用者が待つ時間** (`queue + client_read + dns + connect`。
//! T15.0 (2))。`connect` の環が `open()` の入口 (要求行を読んだ後) から測るのに対し、
//! こちらは accept してワーカーが動き出すまでの待ちと名前解決も入る。**CONNECT だけ**で、
//! `--lite` では書かない (`queue` と `client_read` が 0 なので「4 段の和」を名乗れない)。
//! `connect` の環は 1 バイトも変えていないので、Phase 12〜14 の p50 と比べる値は残る。
//!
//! **費用**: 書くのは [`crate::metrics::Metrics::record_host_detail`] が**既に取っている
//! 鍵の内側**なので、原子操作もシステムコールも鍵も増えない。増えるのは 8 バイトの
//! 書き込み 1 回と添字の +1 だけで、**時計も読まない** (ホスト別統計の `last_seen` が
//! 既に読んでいる epoch 秒を、同じ鍵の内側で使い回す)。分位点は `/status` に来たときだけ
//! 鍵の外で [`slice::select_nth_unstable`] で出す (1,024 本で数 us)。
//!
//! **単位は us** ([`u32`] = 71 分で頭打ち)。1 ms 刻みではベンチの p50 (手元の
//! `--only connect` で 0.7〜1.9 ms) と突き合わせられない。ただし **forward の初バイトは
//! 元が ms 刻み** ([`crate::metrics::Detail::first_byte_ms`]) なので、×1,000 して
//! 入るだけで細かくはならない (CONNECT の確立は `Duration` のまま来るので us が出る)。
//!
//! `.rrd` には書かない (メモリだけ、24 KiB 固定。再起動で消える)。`--lite` では書かない。

use std::fmt::Write as _;

/// 1 系統あたりに持つ標本の本数。
pub const SAMPLES: usize = 1024;

/// 標本 1 本 (8 B)。**時刻を隣に置く**のは `window_secs` (直近 1,024 本が何秒ぶんか) を
/// 出すため。値と時刻を別々の配列にすると書き込みで触る行が 2 本になるので、
/// 1 つの構造体にして 1 行で済ませる。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Slot {
    /// 確立 (CONNECT) / 初バイト (forward) / 利用者の待ち (`wait`) までの時間 (us)
    us: u32,
    /// 書いた時刻 (epoch 秒。2106 年まで)
    t: u32,
}

/// 満杯のときに使うメモリ (`/status` の `memory.rings.quantiles`)。**3 系統ぶん**
/// (T15.0 (2) で `wait` を足して 16 → 24 KiB)。
pub const BYTES: usize = 3 * SAMPLES * size_of::<Slot>();

/// 1 系統 (CONNECT の確立 / forward の初バイト / 利用者の待ち) の環状バッファ。
///
/// 満ちるまでは `push` で伸ばす (`--lite` は 1 本も書かないので**確保もしない**)。
#[derive(Debug, Default)]
pub struct Ring {
    buf: Vec<Slot>,
    /// 次に書く位置 (満ちてからは「いちばん古い標本の位置」でもある)
    next: usize,
    /// 書いた総数 (環に残っている数ではない)
    total: u64,
}

impl Ring {
    /// 標本を 1 本書く (**呼び出し側が鍵を持っている**)。
    #[inline]
    pub fn observe(&mut self, us: u32, t: u64) {
        let slot = Slot {
            us,
            t: t.min(u32::MAX as u64) as u32,
        };
        if self.buf.len() < SAMPLES {
            if self.buf.is_empty() {
                // 最初の 1 本で 1 回だけ確保する (伸ばし直しをしない)
                self.buf.reserve_exact(SAMPLES);
            }
            self.buf.push(slot);
        } else {
            self.buf[self.next] = slot;
        }
        self.next = (self.next + 1) % SAMPLES;
        self.total = self.total.saturating_add(1);
    }

    /// 環に残っている標本の数。
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// 鍵の内側でするのは**値を写すことだけ** (分位点は鍵を放してから出す)。
    pub fn copy(&self) -> Copied {
        let oldest = if self.buf.len() < SAMPLES {
            self.buf.first()
        } else {
            self.buf.get(self.next)
        };
        Copied {
            us: self.buf.iter().map(|s| s.us).collect(),
            oldest_t: oldest.map_or(0, |s| s.t as u64),
            total: self.total,
        }
    }
}

/// 鍵の外で分位点を出すための写し ([`Ring::copy`] が作る)。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Copied {
    us: Vec<u32>,
    oldest_t: u64,
    total: u64,
}

impl Copied {
    /// `q` パーセンタイルの位置 (0 始まり、**nearest-rank**: `ceil(n × q)` 番目)。
    ///
    /// 整数だけで出す (1〜1,024 を 1 本ずつ入れたとき p50 = 512、p90 = 922、
    /// p99 = 1,014 になる定義。受け入れ基準はこの値)。
    fn rank(n: usize, pct: usize) -> usize {
        ((n * pct).div_ceil(100)).max(1) - 1
    }

    /// 分位点を出す (`select_nth_unstable` を 3 回。1,024 本で数 us)。
    ///
    /// `now` は epoch 秒 (`window_secs` = 最古の標本からの経過秒)。
    pub fn stats(mut self, now: u64) -> Stats {
        let n = self.us.len();
        if n == 0 {
            return Stats::default();
        }
        let (k50, k90, k99) = (Self::rank(n, 50), Self::rank(n, 90), Self::rank(n, 99));
        // 小さい順位から選ぶと、次の選択は右側の部分列だけで済む
        // (`select_nth_unstable` は「その位置より左は全部以下」にして戻る)
        let p50 = *self.us.select_nth_unstable(k50).1;
        let p90 = *self.us[k50..].select_nth_unstable(k90 - k50).1;
        let p99 = *self.us[k90..].select_nth_unstable(k99 - k90).1;
        let max = self.us[k99..].iter().copied().max().unwrap_or(p99);
        Stats {
            n,
            p50_us: p50,
            p90_us: p90,
            p99_us: p99,
            max_us: max,
            window_secs: now.saturating_sub(self.oldest_t),
            total: self.total,
        }
    }
}

/// 1 系統ぶんの分位点 (`/status` の `recent_quantiles.connect` / `.forward`)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// 環に残っている標本の数 (満ちていれば 1,024)
    pub n: usize,
    pub p50_us: u32,
    pub p90_us: u32,
    pub p99_us: u32,
    pub max_us: u32,
    /// 最古の標本からの経過秒 (= この 1,024 本が何秒ぶんか)
    pub window_secs: u64,
    /// 起動から書いた総数 (環に残っている数ではない)
    pub total: u64,
}

impl Stats {
    /// `/status` に出す 1 つぶん。**ms (小数 3 桁)** で返す
    /// (`/daily` や `?summary=1` は小数 1 桁だが、ここは 1 ms 未満を読むためのもの)。
    pub fn push_json(&self, out: &mut String) {
        let ms = |us: u32| us as f64 / 1000.0;
        let _ = write!(
            out,
            "{{\"n\":{},\"p50\":{:.3},\"p90\":{:.3},\"p99\":{:.3},\"max\":{:.3},\"window_secs\":{}}}",
            self.n,
            ms(self.p50_us),
            ms(self.p90_us),
            ms(self.p99_us),
            ms(self.max_us),
            self.window_secs,
        );
    }

    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(96);
        self.push_json(&mut out);
        out
    }
}

/// 3 系統ぶん (`/status` の `recent_quantiles`)。
///
/// **ホスト表と同じ鍵の中**に置いて、要求の経路が鍵を 2 つ取らないようにしてある
/// (T14.22 の `series` と同じ置き方)。
#[derive(Debug, Default)]
pub struct Quantiles {
    /// CONNECT の確立 (`connect://` の鍵)
    pub connect: Ring,
    /// forward の初バイト
    pub forward: Ring,
    /// 利用者が待つ時間 (`queue + client_read + dns + connect`。**CONNECT だけ**。
    /// T15.0 (2))。書くのは [`crate::metrics::Metrics`] の `record` の中の 1 か所
    pub wait: Ring,
}

impl Quantiles {
    /// 標本を 1 本書く (**呼び出し側が鍵を持っている**)。
    #[inline]
    pub fn observe(&mut self, connect: bool, us: u32, t: u64) {
        if connect {
            self.connect.observe(us, t);
        } else {
            self.forward.observe(us, t);
        }
    }

    /// 鍵の内側でするのは写しだけ (`connect` / `forward` / `wait` の順)。
    pub fn copy(&self) -> (Copied, Copied, Copied) {
        (self.connect.copy(), self.forward.copy(), self.wait.copy())
    }
}

/// `{"connect":{..},"forward":{..},"wait":{..}}` (`/status` の `recent_quantiles`)。
/// **`wait` は末尾に足した** (既存の鍵の順は変えない。T15.0 (2))。
pub fn to_json(connect: &Stats, forward: &Stats, wait: &Stats) -> String {
    let mut out = String::with_capacity(336);
    out.push_str("{\"connect\":");
    connect.push_json(&mut out);
    out.push_str(",\"forward\":");
    forward.push_json(&mut out);
    out.push_str(",\"wait\":");
    wait.push_json(&mut out);
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 受け入れ基準そのもの: 1〜1,024 ms を 1 本ずつ入れて p50 = 512、p99 = 1,014。
    #[test]
    fn a_thousand_samples_give_the_exact_median() {
        let mut r = Ring::default();
        for ms in 1..=SAMPLES as u32 {
            r.observe(ms * 1000, 100);
        }
        let s = r.copy().stats(160);
        assert_eq!(s.n, SAMPLES);
        assert_eq!(s.total, SAMPLES as u64);
        assert_eq!(s.p50_us, 512_000, "p50 = 512 ms");
        assert_eq!(s.p90_us, 922_000, "p90 = 922 ms");
        assert_eq!(s.p99_us, 1_014_000, "p99 = 1,014 ms");
        assert_eq!(s.max_us, 1_024_000);
        assert_eq!(s.window_secs, 60);
    }

    /// 3 本目の環 `wait` は独立していて、`connect` とも `forward` とも混ざらない
    /// (T15.0 (2))。標本ごとに `wait ≥ connect` なら**分位点も必ず** `wait ≥ connect`
    /// になる (同じ本数の順序統計量なので。結合テストの受け入れ基準はこの関係)。
    #[test]
    fn the_wait_ring_is_separate_and_never_below_the_connect_ring() {
        let mut q = Quantiles::default();
        for i in 1..=100u32 {
            let connect_us = i * 1_000;
            q.observe(true, connect_us, 50);
            // 待ちは確立に「その前の段」(queue + client_read + dns) を足したもの
            q.wait.observe(connect_us + 7_000, 50);
        }
        let (c, f, w) = q.copy();
        let (c, f, w) = (c.stats(50), f.stats(50), w.stats(50));
        assert_eq!((c.n, w.n), (100, 100), "確立と待ちは同じ本数");
        assert_eq!(f.n, 0, "forward には 1 本も入らない");
        for (a, b) in [
            (w.p50_us, c.p50_us),
            (w.p90_us, c.p90_us),
            (w.p99_us, c.p99_us),
            (w.max_us, c.max_us),
        ] {
            assert!(a >= b, "wait {} < connect {}", a, b);
        }
        assert_eq!(w.p50_us, c.p50_us + 7_000);
        assert!(w.p50_us <= w.p90_us && w.p90_us <= w.p99_us && w.p99_us <= w.max_us);
    }

    /// 入れる順を逆にしても同じ答え (環の位置と分位点は無関係)。
    #[test]
    fn the_order_of_the_samples_does_not_matter() {
        let mut r = Ring::default();
        for ms in (1..=SAMPLES as u32).rev() {
            r.observe(ms * 1000, 0);
        }
        let s = r.copy().stats(0);
        assert_eq!(s.p50_us, 512_000);
        assert_eq!(s.p99_us, 1_014_000);
    }

    /// 1,024 本を超えると古いものから落ちる (**直近**の窓であること)。
    #[test]
    fn the_ring_keeps_only_the_last_1024_samples() {
        let mut r = Ring::default();
        // 先に 1 ms を 1,024 本、そのあと 100 ms を 1,024 本
        for _ in 0..SAMPLES {
            r.observe(1_000, 10);
        }
        for _ in 0..SAMPLES {
            r.observe(100_000, 20);
        }
        let s = r.copy().stats(25);
        assert_eq!(s.n, SAMPLES);
        assert_eq!(s.total, 2 * SAMPLES as u64, "総数は落とした分も数える");
        assert_eq!(s.p50_us, 100_000, "古い 1 ms は 1 本も残っていない");
        assert_eq!(s.max_us, 100_000);
        // 最古の標本は 2 周目の先頭 (t = 20)
        assert_eq!(s.window_secs, 5);
    }

    /// 半端な本数でも順位の定義は同じ (nearest-rank)。
    #[test]
    fn a_partly_filled_ring_uses_the_same_ranks() {
        let mut r = Ring::default();
        for ms in 1..=100u32 {
            r.observe(ms * 1000, 7);
        }
        let s = r.copy().stats(7);
        assert_eq!(s.n, 100);
        assert_eq!(s.p50_us, 50_000);
        assert_eq!(s.p90_us, 90_000);
        assert_eq!(s.p99_us, 99_000);
        assert_eq!(s.max_us, 100_000);
        assert_eq!(s.window_secs, 0);
    }

    /// 1 本でも落ちない。空は全部 0。
    #[test]
    fn one_sample_and_none_at_all_are_both_safe() {
        let mut r = Ring::default();
        assert_eq!(r.copy().stats(99), Stats::default());
        assert!(r.is_empty());
        r.observe(1_234, 90);
        let s = r.copy().stats(99);
        assert_eq!(
            (s.n, s.p50_us, s.p90_us, s.p99_us, s.max_us),
            (1, 1_234, 1_234, 1_234, 1_234)
        );
        assert_eq!(s.window_secs, 9);
    }

    /// `--lite` (1 本も書かない) では確保もしない。
    #[test]
    fn an_untouched_ring_allocates_nothing() {
        let q = Quantiles::default();
        assert_eq!(q.connect.buf.capacity(), 0);
        assert_eq!(q.forward.buf.capacity(), 0);
        assert_eq!(q.wait.buf.capacity(), 0);
        // 3 系統 × 1,024 本 × 8 B (T15.0 (2) で 16 → 24 KiB)
        assert_eq!(BYTES, 24_576);
    }

    /// `/status` に出る形と大きさ。
    #[test]
    fn the_json_is_small_and_has_the_six_keys() {
        let mut q = Quantiles::default();
        q.observe(true, 1_500, 1000);
        q.observe(false, 2_500, 1000);
        // `wait` は `observe` の二択に無い (CONNECT の 1 か所だけが直に書く)
        q.wait.observe(3_500, 1000);
        let (c, f, w) = q.copy();
        let json = to_json(&c.stats(1000), &f.stats(1000), &w.stats(1000));
        assert_eq!(
            json,
            "{\"connect\":{\"n\":1,\"p50\":1.500,\"p90\":1.500,\"p99\":1.500,\"max\":1.500,\"window_secs\":0},\
             \"forward\":{\"n\":1,\"p50\":2.500,\"p90\":2.500,\"p99\":2.500,\"max\":2.500,\"window_secs\":0},\
             \"wait\":{\"n\":1,\"p50\":3.500,\"p90\":3.500,\"p99\":3.500,\"max\":3.500,\"window_secs\":0}}"
        );
        // 満杯・大きな値でも `/status` を太らせない
        let mut big = Ring::default();
        for _ in 0..SAMPLES {
            big.observe(u32::MAX, 0);
        }
        let s = big.copy().stats(u32::MAX as u64);
        let json = to_json(&s, &s, &s);
        assert!(json.len() <= 384, "{}", json.len());
    }

    /// 順位の定義 (境目の丸め方)。
    #[test]
    fn the_rank_is_the_nearest_one() {
        assert_eq!(Copied::rank(1024, 50), 511);
        assert_eq!(Copied::rank(1024, 90), 921);
        assert_eq!(Copied::rank(1024, 99), 1013);
        assert_eq!(Copied::rank(1, 99), 0);
        assert_eq!(Copied::rank(10, 50), 4);
        assert_eq!(Copied::rank(10, 99), 9);
    }
}
