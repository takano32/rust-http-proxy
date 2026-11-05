//! ホスト別 (接続元別にも使う) の通算 ([`HostStats`]): 1 要求を数える・区間ごとの応答時間の
//! 分布と分位点・カーネルの RTT・捨てる行の畳み込み。状態ファイルの形は `host_record`、
//! `/status` の行の JSON は `host_json` にある。

use std::time::Duration;

use super::{DNS_CAUSES, Detail, ERR_CAUSES, HostOutcome};

/// 応答時間ヒストグラムの上限 (ms)。最後の区間は上限なし。
///
/// **1 ms 〜 10 s の 24 段、公比およそ 1.5** (T12.4 (1))。10 段だった頃は
/// デプロイ先の 50 ホスト中 29 (要求数で 74%) が `p50 = p95 = max` になっていた:
/// 257 ms が (250, 500] の 1 区間に全部入り、区間内を補間しても観測した最大値で
/// 頭打ちになるため。段を細かくすると同じ補間のままで分位点が意味を持つ
/// (実測: 257 ms × 90 + 290 ms × 10 の p50 が 290 → 262.5)。
///
/// 値は等比数列を整数に丸めたもので、**下の端 (1〜10 ms) はデプロイ先の
/// AAAA 無しホスト (p50 5.1 ms) が乗るところ**なので公比より細かく取ってある。
pub const LATENCY_BOUNDS_MS: [u64; 24] = [
    1, 2, 3, 4, 6, 9, 13, 20, 30, 45, 65, 95, 140, 210, 315, 470, 700, 1000, 1500, 2200, 3300,
    5000, 7500, 10000,
];

/// ホスト別統計の上限。超えた分は `other` にまとめる。
pub const MAX_HOSTS: usize = 1000;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostStats {
    pub requests: u64,
    pub hits: u64,
    pub misses: u64,
    pub bypass: u64,
    pub errors: u64,
    /// ACL / ブロックリストで拒否した数
    pub blocked: u64,
    pub bytes: u64,
    /// 応答時間を記録した要求数と、その合計・最大 (ms)。CONNECT は接続確立までの時間
    pub timed: u64,
    pub duration_ms_sum: u64,
    pub duration_ms_max: u64,
    /// `LATENCY_BOUNDS_MS` の区間ごとの件数 (+ 上限なしの区間)
    pub buckets: [u64; LATENCY_BOUNDS_MS.len() + 1],
    /// 最後に要求を受けた時刻 (epoch 秒)
    pub last_seen: u64,
    /// 名前解決にかかった時間の合計 (ms) と、OS へ問い合わせた回数 (T12.4 (2))
    pub dns_ms_sum: u64,
    pub dns_misses: u64,
    /// 接続 (SYN → 確立) にかかった時間の合計 (ms)。名前解決のぶんは含まない
    pub connect_ms_sum: u64,
    /// 確立した族の内訳 (T12.1 の全体の統計と二重にならないよう、ここはホスト別)
    pub v4_wins: u64,
    pub v6_wins: u64,
    /// エラーの原因別の件数 ([`ErrCause`](super::ErrCause) の順)
    pub errors_by_cause: [u64; ERR_CAUSES],
    /// 名前解決の失敗の内訳 ([`DnsCause`](super::DnsCause) の順。T25.0)。`errors_by_cause` の
    /// **うしろに足す 2 列**で、先頭 8 列の意味は動かさない。`.rrd` には
    /// [`HostStats::encode`] の**末尾**に書く (前の欄の位置を動かさない)
    pub dns_by_cause: [u64; DNS_CAUSES],
    /// カーネルの平滑化 RTT (`TCP_INFO`) の標本 (T14.5)。**要求ごとではなく接続の
    /// 終わりに 1 本 1 回**なので `timed` とは数が合わない。ホスト別はオリジン側、
    /// 接続元別 (`ClientStats`) はクライアント側の値が入る
    pub rtt_us_sum: u64,
    pub rtt_us_min: u64,
    pub rtt_samples: u64,
    /// その接続たちが再送したセグメントの通算 (`tcpi_total_retrans`)
    pub retrans: u64,
    /// 向き別の転送バイト (T14.26)。`bytes_in` = クライアント → オリジン (上り)、
    /// `bytes_out` = オリジン → クライアント (下り)。**ここまでが `.rrd` に残る欄**
    /// ([`HostStats::encode`] の並びと同じ順)。
    ///
    /// CONNECT は `bytes == bytes_in + bytes_out` だが、**forward の `bytes` は
    /// 今までどおり応答のぶんだけ**なので (欄の意味は変えない)、上りを足すと
    /// `bytes_in + bytes_out` の方が大きくなる
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// ホスト別の時系列 (`crate::hostseries`) の枠の番号。**直近 1 時間の要求数で
    /// 上位 16 に居る間だけ** `Some` で、入れ替えるのは history スレッド (T14.22)。
    /// 要求の経路はこの旗を見るだけ。`.rrd` には書かない欄なので
    /// [`HostStats::encode`] / [`HostStats::decode`] は 1 バイトも変えていない
    pub series_slot: Option<u8>,
    /// CONNECT のホストと SNI が食い違った本数 (`PROXY_PEEK_SNI`。T14.38)。
    /// **`.rrd` には書かない** (スロットの余白は 4 B しか無い。T14.26) ので
    /// 再起動で 0 に戻る = `series_slot` と同じ扱い。合計は `/status` の `sni_mismatches`
    pub sni_mismatch: u64,
    /// そのホストへの接続が **SYN を送り直した回数の合計** (T14.46)。
    /// **`.rrd` には書かない** ([`HostStats::sni_mismatch`] と同じ扱い。スロットの
    /// 余白は 4 B しか無い) ので再起動で 0 に戻る。合計は `/status` の `syn_retrans_total`。
    ///
    /// [`HostStats::retrans`] (T14.5) は**接続の終わり**に読んだ通算 (データの再送を含む)、
    /// こちらは**確立の直後**なので SYN の再送だけ = 「繋ぐのに何回やり直したか」
    pub syn_retrans: u64,
    /// `/slo` の `error_rate` が 1 つの宛先に偏ったエラーを外すための、その時間の数 (T.30.4)。
    /// **`.rrd` には書かない** (メモリだけ。`/slo` の集計も再起動で消えるので揃う)。
    /// 書くのは `proxy-metrics-core` の `Metrics` の**エラーの経路と時間の区切り**だけ
    pub web_hour: WebHour,
}

/// ホスト 1 つの「いまの UTC の 1 時間」の、443・80 宛てのエラーと試行を出すための欄 (T.30.4)。
///
/// **成功の経路では 1 つも触らない**: この時間の成功の本数は、時間の区切りで写した
/// `timed` (応答時間を測った要求の通算) と `timed_errors` (そのうちエラー) からの差で出す
/// (成功 = Δ`timed` − Δ`timed_errors`)。エラーの経路だけが `timed_errors` を足す。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WebHour {
    /// 応答時間を測ったエラーの通算 (起動から。エラーの経路で足す)
    pub timed_errors: u64,
    /// 時間の区切りで写した [`HostStats::timed`] と `timed_errors`
    pub base_timed: u64,
    pub base_timed_errors: u64,
    // その時間のエラーの数は T.32.10 からホスト別統計から切り離した小さな表
    // (`proxy-metrics-core` の `web_dest::DestTable`) が持つ (表が満杯でも宛先を数えるため)
}

impl HostStats {
    /// `now` は epoch 秒。**呼ぶ側が 1 回だけ読んだ値**を渡す (直近の標本 (T14.31) と
    /// `last_seen` で同じ値を使い回すため。壁時計を読む回数は今までと変わらない)。
    ///
    /// **呼ぶのは上の層 (`proxy-metrics-core` の `Metrics`) だけ** (T14.55 でクレートを
    /// 割るまでは同じクレートの中の `fn` だった)。
    pub fn count(
        &mut self,
        now: u64,
        outcome: HostOutcome,
        bytes: u64,
        took: Option<Duration>,
        detail: &Detail,
    ) {
        self.requests += 1;
        self.bytes += bytes;
        self.last_seen = now;
        match outcome {
            HostOutcome::Hit => self.hits += 1,
            HostOutcome::Miss => self.misses += 1,
            HostOutcome::Bypass => self.bypass += 1,
            HostOutcome::Error => self.errors += 1,
            HostOutcome::Blocked => self.blocked += 1,
        }
        if let Some(d) = took {
            self.observe(d);
        }
        self.add_detail(detail);
    }

    /// 内訳を足す (鍵の内側。全部 0 の [`Detail::default`] でも同じ道を通る)。
    fn add_detail(&mut self, d: &Detail) {
        self.dns_ms_sum += d.dns_ms;
        self.dns_misses += d.dns_misses;
        self.connect_ms_sum += d.connect_ms;
        match d.family_v6 {
            Some(true) => self.v6_wins += 1,
            Some(false) => self.v4_wins += 1,
            None => {}
        }
        if let Some(c) = d.cause {
            self.errors_by_cause[c as usize] += 1;
        }
        // 名前解決の失敗の内訳 (T25.0)。鍵の内側の足し算 1 回で、`dns` の合計
        // (`errors_by_cause[ErrCause::Dns as usize]`) は今までどおり別に数える
        // (`dns == nxdomain + dns_temp` が保たれる)
        if let Some(c) = d.dns_cause {
            self.dns_by_cause[c as usize] += 1;
        }
        // 向き別のバイト (T14.26)。既に数えてあるものを 2 つに分けて運んできただけなので、
        // ここで増えるのは足し算 2 回だけ (鍵も原子操作もシステムコールも増えない)
        self.bytes_in += d.bytes_in;
        self.bytes_out += d.bytes_out;
        // CONNECT のホストと SNI の食い違い (T14.38)。**旗は `tunnel::report` が
        // 立てたもの**で、ここは鍵の内側の足し算 1 回 (メモリだけの欄)
        if d.sni_mismatch {
            self.sni_mismatch += 1;
        }
        // 確立までの SYN の再送 (T14.46)。**読んだのは `net` の確立点**で、
        // ここは鍵の内側の足し算 1 回 (メモリだけの欄)
        self.syn_retrans += d.syn_retrans as u64;
    }

    /// カーネルの RTT と再送を 1 標本足す (**接続の終わりに 1 回だけ**。T14.5)。
    /// 呼び出し側が既に鍵を取っているので、原子操作もシステムコールも増えない。
    /// **呼ぶのは上の層 (`proxy-metrics-core` の `Metrics`) だけ** (T14.55 でクレートを
    /// 割るまでは同じクレートの中の `fn` だった)。
    pub fn observe_rtt(&mut self, rtt_us: u32, retrans: u32) {
        let us = rtt_us as u64;
        self.rtt_us_sum = self.rtt_us_sum.saturating_add(us);
        self.rtt_samples += 1;
        if self.rtt_us_min == 0 || us < self.rtt_us_min {
            self.rtt_us_min = us;
        }
        self.retrans = self.retrans.saturating_add(retrans as u64);
    }

    /// 捨てる行の通算を `self` (`other` の行) に足す (T.32.10。満杯の表から古い行を捨てるとき)。
    ///
    /// 通算の欄を全部足すので、表を舐めて足す合計 (`/metrics` の `errors_total{cause="nxdomain"}`
    /// など) は行を捨てても減らない。最大・最小・最後の時刻は大きい方 / 小さい方。メモリだけの欄の
    /// うち `series_slot` と `web_hour` (その時間の `/slo` の数え方) は足さない。
    pub fn absorb(&mut self, o: &HostStats) {
        self.requests += o.requests;
        self.hits += o.hits;
        self.misses += o.misses;
        self.bypass += o.bypass;
        self.errors += o.errors;
        self.blocked += o.blocked;
        self.bytes += o.bytes;
        self.timed += o.timed;
        self.duration_ms_sum += o.duration_ms_sum;
        self.duration_ms_max = self.duration_ms_max.max(o.duration_ms_max);
        for (a, b) in self.buckets.iter_mut().zip(o.buckets) {
            *a += b;
        }
        self.last_seen = self.last_seen.max(o.last_seen);
        self.dns_ms_sum += o.dns_ms_sum;
        self.dns_misses += o.dns_misses;
        self.connect_ms_sum += o.connect_ms_sum;
        self.v4_wins += o.v4_wins;
        self.v6_wins += o.v6_wins;
        for (a, b) in self.errors_by_cause.iter_mut().zip(o.errors_by_cause) {
            *a += b;
        }
        for (a, b) in self.dns_by_cause.iter_mut().zip(o.dns_by_cause) {
            *a += b;
        }
        self.rtt_us_sum = self.rtt_us_sum.saturating_add(o.rtt_us_sum);
        if o.rtt_us_min != 0 && (self.rtt_us_min == 0 || o.rtt_us_min < self.rtt_us_min) {
            self.rtt_us_min = o.rtt_us_min;
        }
        self.rtt_samples += o.rtt_samples;
        self.retrans = self.retrans.saturating_add(o.retrans);
        self.bytes_in += o.bytes_in;
        self.bytes_out += o.bytes_out;
        self.sni_mismatch += o.sni_mismatch;
        self.syn_retrans += o.syn_retrans;
    }

    /// RTT の平均 (ms)。標本が無ければ `None` (Linux 以外・`--lite` では出ない)。
    pub fn rtt_avg_ms(&self) -> Option<f64> {
        (self.rtt_samples > 0).then(|| self.rtt_us_sum as f64 / self.rtt_samples as f64 / 1000.0)
    }

    /// RTT の最小 (ms)。標本が無ければ `None`。
    pub fn rtt_min_ms(&self) -> Option<f64> {
        (self.rtt_samples > 0).then(|| self.rtt_us_min as f64 / 1000.0)
    }

    fn observe(&mut self, d: Duration) {
        let ms = d.as_millis().min(u64::MAX as u128) as u64;
        self.timed += 1;
        self.duration_ms_sum += ms;
        self.duration_ms_max = self.duration_ms_max.max(ms);
        let idx = LATENCY_BOUNDS_MS
            .iter()
            .position(|&b| ms <= b)
            .unwrap_or(LATENCY_BOUNDS_MS.len());
        self.buckets[idx] += 1;
    }

    pub fn avg_ms(&self) -> f64 {
        if self.timed == 0 {
            0.0
        } else {
            self.duration_ms_sum as f64 / self.timed as f64
        }
    }

    /// 区間内を線形に補間した分位点 (ms)。最後の区間は最大値で頭打ち。
    pub fn quantile_ms(&self, q: f64) -> f64 {
        if self.timed == 0 {
            return 0.0;
        }
        let rank = (q.clamp(0.0, 1.0) * self.timed as f64).max(1.0);
        let mut seen = 0u64;
        for (i, &n) in self.buckets.iter().enumerate() {
            if n == 0 {
                continue;
            }
            if (seen + n) as f64 >= rank {
                let lo = if i == 0 {
                    0.0
                } else {
                    LATENCY_BOUNDS_MS[i - 1] as f64
                };
                let hi = if i < LATENCY_BOUNDS_MS.len() {
                    LATENCY_BOUNDS_MS[i] as f64
                } else {
                    (self.duration_ms_max as f64).max(lo)
                };
                let frac = (rank - seen as f64) / n as f64;
                // 観測した最大値は超えない (件数が少ないとき区間の上端が出ないように)
                return (lo + (hi - lo) * frac).min(self.duration_ms_max as f64);
            }
            seen += n;
        }
        self.duration_ms_max as f64
    }

    pub fn error_rate(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.errors as f64 / self.requests as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{DnsCause, ErrCause};
    use super::*;

    /// 応答時間は「それ以下の最初の区間」に入る (境目は区間に含む。10 s 超は上限なしの段)。
    #[test]
    fn observe_puts_the_response_time_in_the_first_bucket_it_fits() {
        let mut s = HostStats::default();
        for (ms, want) in [
            (0u64, 0usize),
            (1, 0),
            (2, 1),
            (10, 6),
            (1000, 17),
            (10_000, 23),
            (10_001, 24),
        ] {
            s.observe(Duration::from_millis(ms));
            assert_eq!(s.buckets[want], 1, "{ms} ms は {want} 段目");
            s.buckets[want] = 0;
        }
        assert_eq!(s.timed, 7);
        assert_eq!(s.duration_ms_max, 10_001);
        assert_eq!(s.duration_ms_sum, 21_014);
        assert!((s.avg_ms() - 21_014.0 / 7.0).abs() < 1e-9);
    }

    /// 結果は 5 つの列に 1 つずつ入り、内訳は鍵の内側で足し込まれる。
    #[test]
    fn count_sorts_each_outcome_into_its_column_and_adds_the_detail() {
        let mut s = HostStats::default();
        let d = Detail {
            dns_ms: 12,
            dns_misses: 1,
            connect_ms: 245,
            family_v6: Some(true),
            cause: Some(ErrCause::Refused),
            dns_cause: Some(DnsCause::DnsTemp),
            bytes_in: 10,
            bytes_out: 20,
            sni_mismatch: true,
            syn_retrans: 2,
            ..Detail::default()
        };
        for (i, o) in [
            HostOutcome::Hit,
            HostOutcome::Miss,
            HostOutcome::Bypass,
            HostOutcome::Error,
            HostOutcome::Blocked,
        ]
        .into_iter()
        .enumerate()
        {
            s.count(1_000 + i as u64, o, 100, None, &d);
        }
        assert_eq!(
            (s.requests, s.hits, s.misses, s.bypass, s.errors, s.blocked),
            (5, 1, 1, 1, 1, 1)
        );
        assert_eq!((s.bytes, s.bytes_in, s.bytes_out), (500, 50, 100));
        assert_eq!(s.last_seen, 1_004, "最後に数えた時刻");
        assert_eq!(s.timed, 0, "応答時間なしでは区間に入らない");
        assert_eq!(
            (s.dns_ms_sum, s.dns_misses, s.connect_ms_sum),
            (60, 5, 1225)
        );
        assert_eq!((s.v4_wins, s.v6_wins), (0, 5));
        assert_eq!(s.errors_by_cause[ErrCause::Refused as usize], 5);
        assert_eq!(s.dns_by_cause, [0, 5]);
        assert_eq!((s.sni_mismatch, s.syn_retrans), (5, 10));
        s.count(
            2_000,
            HostOutcome::Hit,
            0,
            Some(Duration::from_millis(3)),
            &Detail::default(),
        );
        assert_eq!((s.timed, s.buckets[2]), (1, 1));
    }

    /// 捨てる行の畳み込み (T.32.10): 通算は足し、最大は大きい方、RTT の最小は 0 を無視した小さい方、
    /// `series_slot` と `web_hour` は触らない。
    #[test]
    fn absorb_adds_the_totals_and_keeps_the_extremes() {
        let mut other = HostStats {
            requests: 10,
            duration_ms_max: 50,
            last_seen: 100,
            rtt_us_min: 0,
            rtt_samples: 0,
            series_slot: Some(3),
            ..HostStats::default()
        };
        other.buckets[1] = 2;
        other.web_hour.timed_errors = 7;
        let mut gone = HostStats {
            requests: 5,
            errors: 2,
            duration_ms_max: 900,
            last_seen: 50,
            rtt_us_sum: 40_000,
            rtt_us_min: 20_000,
            rtt_samples: 2,
            retrans: 3,
            errors_by_cause: [1, 1, 0, 0, 0, 0, 0, 0],
            dns_by_cause: [1, 0],
            series_slot: Some(9),
            sni_mismatch: 1,
            ..HostStats::default()
        };
        gone.buckets[1] = 1;
        gone.buckets[5] = 4;
        gone.web_hour.timed_errors = 99;
        other.absorb(&gone);
        assert_eq!((other.requests, other.errors), (15, 2));
        assert_eq!(other.duration_ms_max, 900, "最大は大きい方");
        assert_eq!(other.last_seen, 100, "最後の時刻は新しい方");
        assert_eq!(
            (other.rtt_us_min, other.rtt_samples, other.retrans),
            (20_000, 2, 3)
        );
        assert_eq!((other.buckets[1], other.buckets[5]), (3, 4));
        assert_eq!(other.errors_by_cause[..2], [1, 1]);
        assert_eq!(other.dns_by_cause, [1, 0]);
        assert_eq!(other.sni_mismatch, 1);
        assert_eq!(other.series_slot, Some(3), "枠の旗は足さない");
        assert_eq!(other.web_hour.timed_errors, 7, "その時間の数え方は足さない");
        // 相手の最小が 0 (標本なし) なら自分の最小を残す
        let mut a = HostStats {
            rtt_us_min: 5,
            ..HostStats::default()
        };
        a.absorb(&HostStats::default());
        assert_eq!(a.rtt_us_min, 5);
    }

    /// T12.4 (1) の形: 257 ms × 90 + 290 ms × 10 で p50 < p95、最大値で頭打ち、空は 0。
    #[test]
    fn the_quantile_interpolates_inside_the_bucket_and_caps_at_the_max() {
        let mut s = HostStats::default();
        for _ in 0..90 {
            s.observe(Duration::from_millis(257));
        }
        for _ in 0..10 {
            s.observe(Duration::from_millis(290));
        }
        let (p50, p95) = (s.quantile_ms(0.5), s.quantile_ms(0.95));
        assert!((250.0..=265.0).contains(&p50), "p50 {p50}");
        assert!((280.0..=290.0).contains(&p95), "p95 {p95}");
        assert!(p50 < p95);
        assert_eq!(s.quantile_ms(1.0), 290.0, "最大値で頭打ち");
        assert_eq!(
            s.quantile_ms(0.0),
            s.quantile_ms(0.001),
            "q は 1 件目より下に行かない"
        );
        assert_eq!(HostStats::default().quantile_ms(0.5), 0.0);
        assert_eq!(HostStats::default().avg_ms(), 0.0);
    }

    /// RTT は接続の終わりの標本: 平均と最小 (ms)、標本が無ければ `None`。エラー率は要求数で割る。
    #[test]
    fn rtt_and_error_rate_are_none_or_zero_without_samples() {
        let mut s = HostStats::default();
        assert_eq!((s.rtt_avg_ms(), s.rtt_min_ms()), (None, None));
        assert_eq!(s.error_rate(), 0.0);
        s.observe_rtt(30_100, 2);
        s.observe_rtt(20_000, 0);
        assert_eq!(s.rtt_avg_ms(), Some(25.05));
        assert_eq!(s.rtt_min_ms(), Some(20.0));
        assert_eq!((s.rtt_samples, s.retrans), (2, 2));
        s.requests = 4;
        s.errors = 1;
        assert!((s.error_rate() - 0.25).abs() < 1e-9);
    }
}
