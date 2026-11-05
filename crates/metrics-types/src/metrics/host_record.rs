//! ホスト別 / 接続元別の 1 行の**状態ファイルの形** (`.rrd` の統計のスロット。名前 128 B + 数値)。
//!
//! 欄は**末尾に足す**だけ (前の欄の位置を動かさない)。短いレコード (古い版が書いた行) は
//! `Dec` が足りない欄を 0 で返すので、そのまま読み継げる。

use super::HostStats;

impl HostStats {
    /// 状態ファイルのレコード (名前 128 バイト + 数値)。
    pub fn encode(&self, name: &str) -> Vec<u8> {
        let mut e = crate::rrd::Enc::new();
        e.str(name, 128)
            .u64(self.last_seen)
            .u64(self.requests)
            .u64(self.hits)
            .u64(self.misses)
            .u64(self.bypass)
            .u64(self.errors)
            .u64(self.blocked)
            .u64(self.bytes)
            .u64(self.timed)
            .u64(self.duration_ms_sum)
            .u64(self.duration_ms_max);
        for b in self.buckets {
            e.u64(b);
        }
        e.u64(self.dns_ms_sum)
            .u64(self.dns_misses)
            .u64(self.connect_ms_sum)
            .u64(self.v4_wins)
            .u64(self.v6_wins);
        for c in self.errors_by_cause {
            e.u64(c);
        }
        // 欄は**末尾に足す**だけ (T14.5 の 4 欄で名前 128 B + 53 項目 = 552 B)
        e.u64(self.rtt_us_sum)
            .u64(self.rtt_us_min)
            .u64(self.rtt_samples)
            .u64(self.retrans);
        // T14.26 の 2 欄も同じ作法で末尾へ (552 → 568 B)。版 3 の 1 スロットは 640 B =
        // ペイロード 636 B なので**ここから下が予備の 68 B** (T14.14。8 項目ぶん。
        // T25.0 の 2 欄を足したので今は 52 B = 6 項目ぶん)。
        // 足しても版は上がらない (古いファイルはその位置がゼロ埋めなので 0 で読み戻る)
        e.u64(self.bytes_in).u64(self.bytes_out);
        // T25.0 の 2 欄 (DNS の失敗の内訳) も**末尾**へ (568 → 584 B)。スロットの
        // ペイロードは 636 B なので余白に収まり、**版は上がらない** (古いファイルは
        // この位置がゼロ埋めなので 0 で読み戻る)
        for c in self.dns_by_cause {
            e.u64(c);
        }
        e.0
    }

    pub fn decode(payload: &[u8]) -> Option<(String, HostStats)> {
        let mut d = crate::rrd::Dec(payload);
        let name = d.str(128);
        if name.is_empty() {
            return None;
        }
        let mut s = HostStats {
            last_seen: d.u64(),
            requests: d.u64(),
            hits: d.u64(),
            misses: d.u64(),
            bypass: d.u64(),
            errors: d.u64(),
            blocked: d.u64(),
            bytes: d.u64(),
            timed: d.u64(),
            duration_ms_sum: d.u64(),
            duration_ms_max: d.u64(),
            ..HostStats::default()
        };
        for b in s.buckets.iter_mut() {
            *b = d.u64();
        }
        s.dns_ms_sum = d.u64();
        s.dns_misses = d.u64();
        s.connect_ms_sum = d.u64();
        s.v4_wins = d.u64();
        s.v6_wins = d.u64();
        for c in s.errors_by_cause.iter_mut() {
            *c = d.u64();
        }
        // 短いレコード (T14.5 より前に書かれた行、版 2 から詰め直した行) は
        // ここで尽きて 0 が返る
        s.rtt_us_sum = d.u64();
        s.rtt_us_min = d.u64();
        s.rtt_samples = d.u64();
        s.retrans = d.u64();
        // 同じく T14.26 より前のファイルはここで尽きて 0 が返る (向きの分からない
        // 昔の転送量は `bytes` にだけ入っている)。**ここから下が予備** — 欄を足す
        // ならこの位置から (T14.14)
        s.bytes_in = d.u64();
        s.bytes_out = d.u64();
        // ここから T25.0 の 2 欄 (DNS の失敗の内訳)。T25.0 より前に書かれた行は
        // ここで尽きて 0 が返る (= `nxdomain` / `dns_temp` は「分からない」)
        for c in s.dns_by_cause.iter_mut() {
            *c = d.u64();
        }
        Some((name, s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全部の欄に違う値を入れて往復させる。`.rrd` に書かない欄 (`series_slot` ほか) は戻らない。
    /// 名前 128 B + 57 項目 × 8 B = 584 B で、版 3 のスロット (636 B) に 52 B の余白が残る。
    #[test]
    fn a_row_round_trips_and_the_memory_only_columns_stay_out_of_the_record() {
        let mut s = HostStats {
            requests: 1,
            hits: 2,
            misses: 3,
            bypass: 4,
            errors: 5,
            blocked: 6,
            bytes: 7,
            timed: 8,
            duration_ms_sum: 9,
            duration_ms_max: 10,
            last_seen: 11,
            dns_ms_sum: 12,
            dns_misses: 13,
            connect_ms_sum: 14,
            v4_wins: 15,
            v6_wins: 16,
            errors_by_cause: [17, 18, 19, 20, 21, 22, 23, 24],
            dns_by_cause: [25, 26],
            rtt_us_sum: 27,
            rtt_us_min: 28,
            rtt_samples: 29,
            retrans: 30,
            bytes_in: 31,
            bytes_out: 32,
            series_slot: Some(3),
            sni_mismatch: 33,
            syn_retrans: 34,
            ..HostStats::default()
        };
        for (i, b) in s.buckets.iter_mut().enumerate() {
            *b = 100 + i as u64;
        }
        let enc = s.encode("connect://a.example:443");
        assert_eq!(enc.len(), 128 + 57 * 8);
        assert_eq!(crate::rrd::STATS_RECORD - 4 - enc.len(), 52, "残りの余白");
        let (name, back) = HostStats::decode(&enc).expect("名前がある");
        assert_eq!(name, "connect://a.example:443");
        s.series_slot = None;
        s.sni_mismatch = 0;
        s.syn_retrans = 0;
        assert_eq!(back, s, "メモリだけの欄は 0 / None で戻る");
        // 名前が空のレコード (空のスロット) は行にならない
        assert_eq!(HostStats::decode(&[0u8; 640]), None);
    }

    /// 古い版の短い行は、無い欄が 0 で戻り、前の欄は 1 つもずれない (T14.5 / T14.26 / T25.0)。
    #[test]
    fn a_short_record_reads_the_missing_columns_as_zero() {
        let s = HostStats {
            requests: 40,
            timed: 3,
            duration_ms_max: 900,
            errors_by_cause: [1, 0, 0, 2, 0, 0, 0, 0],
            dns_by_cause: [1, 0],
            rtt_us_sum: 50_100,
            rtt_us_min: 20_000,
            rtt_samples: 2,
            retrans: 2,
            bytes_in: 1034,
            bytes_out: 2068,
            ..HostStats::default()
        };
        let enc = s.encode("h");
        // T25.0 より前 (DNS の内訳が無い)
        let (_, back) = HostStats::decode(&enc[..enc.len() - 16]).unwrap();
        assert_eq!(back.dns_by_cause, [0, 0]);
        assert_eq!((back.bytes_in, back.bytes_out), (1034, 2068));
        // T14.26 より前 (向き別のバイトも無い)
        let (_, back) = HostStats::decode(&enc[..enc.len() - 32]).unwrap();
        assert_eq!((back.bytes_in, back.bytes_out), (0, 0));
        assert_eq!((back.rtt_us_sum, back.rtt_samples), (50_100, 2));
        // T14.5 より前 (RTT の 4 欄も無い)
        let (_, back) = HostStats::decode(&enc[..enc.len() - 64]).unwrap();
        assert_eq!(
            (
                back.rtt_us_sum,
                back.rtt_us_min,
                back.rtt_samples,
                back.retrans
            ),
            (0, 0, 0, 0)
        );
        assert_eq!(back.requests, 40, "前の欄はそのまま読める");
        assert_eq!(back.errors_by_cause, s.errors_by_cause);
    }
}
