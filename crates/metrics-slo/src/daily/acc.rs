//! 1 日ぶんの積み上げ ([`Acc`]) と、その 1 行。

use std::fmt::Write as _;

use crate::history::{Sample, Window};

use super::{MAX_FLOAT, MAX_LINE, MAX_NUM, SECS_PER_DAY, SEED_RES};

/// 標本から読む累計 (日の境目で差を取る)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Cum {
    pub(super) requests: u64,
    pub(super) bytes: u64,
    pub(super) evicted_idle: u64,
    /// 山の写真の通し番号 (捨てた分も含む通算。T14.6)
    pub(super) bursts: u64,
}

impl Cum {
    pub(super) fn take(s: &Sample, bursts: u64) -> Cum {
        Cum {
            requests: s.requests,
            bytes: s.bytes,
            evicted_idle: s.evicted_idle,
            bursts,
        }
    }
}

/// 1 日ぶんの積み上げ。
#[derive(Debug, Clone, Default)]
pub(super) struct Acc {
    /// UTC の日 (epoch 秒 / 86,400)
    pub(super) day: u64,
    pub(super) first_t: u64,
    pub(super) last_t: u64,
    pub(super) samples: u64,
    pub(super) start: Cum,
    pub(super) end: Cum,
    pub(super) connect: Window,
    pub(super) errors: u64,
    pub(super) dns_misses: u64,
    pub(super) dns_ms_sum: u64,
    /// `dns_misses` のうち名前解決に失敗した試みのぶん (標本の `dns_fail_misses`。T.32.17)
    pub(super) dns_fail_misses: u64,
    pub(super) active_max: u64,
    pub(super) rss_max: u64,
    pub(super) rss_sum: u64,
    pub(super) rss_n: u64,
    /// 組み直した前のプロセスのぶん (T.32.18): 見ていた秒 (60 秒の行の数 × 60)、要求とバイト
    /// (行の `requests_delta` / `bytes_delta` の和)、追い出し (行どうしの通算の差の和)
    pub(super) seed_secs: u64,
    pub(super) seed_requests: u64,
    pub(super) seed_bytes: u64,
    pub(super) seed_evicted: u64,
    /// その日に動いた版の並び (組み直したときだけ。`None` なら書く側の今の版)
    pub(super) versions: Option<String>,
}

impl Acc {
    pub(super) fn start_at(day: u64, t: u64, cum: Cum) -> Acc {
        Acc {
            day,
            first_t: t,
            last_t: t,
            start: cum,
            end: cum,
            ..Acc::default()
        }
    }

    pub(super) fn add(&mut self, s: &Sample, cum: Cum) {
        self.samples += 1;
        self.last_t = self.last_t.max(s.t);
        self.end = cum;
        self.connect.merge(&s.connect);
        self.errors += s.errors;
        self.dns_misses += s.dns_misses;
        self.dns_ms_sum += s.dns_ms_sum;
        self.dns_fail_misses += s.dns_fail_misses;
        self.active_max = self.active_max.max(s.active_max).max(s.active as u64);
        // RSS が読めない環境 (`/proc` が無い) では 0 が並ぶので平均に混ぜない
        if s.rss > 0 {
            self.rss_max = self.rss_max.max(s.rss);
            self.rss_sum = self.rss_sum.saturating_add(s.rss);
            self.rss_n += 1;
        }
    }

    /// 前のプロセスの 60 秒の行 1 本を足す (T.32.18)。区間の値は 5 秒の標本と同じく足し合わせ、
    /// 標本の数は行に畳んだ 5 秒の本数 (`gauge_n`。古い行は 1)、RSS の平均はその本数で重みを付ける。
    /// `prev_evicted` は 1 本前の行の `evicted_idle` (通算なので差を足す。減ったら再起動の段差で 0)
    pub(super) fn add_seed(&mut self, s: &Sample, prev_evicted: Option<u64>) {
        let n = s.gauge_n.max(1);
        self.samples += n;
        self.seed_secs += crate::history::RESOLUTIONS[SEED_RES].0;
        self.seed_requests += s.requests_delta;
        self.seed_bytes += s.bytes_delta;
        if let Some(p) = prev_evicted {
            self.seed_evicted += s.evicted_idle.saturating_sub(p);
        }
        self.connect.merge(&s.connect);
        self.errors += s.errors;
        self.dns_misses += s.dns_misses;
        self.dns_ms_sum += s.dns_ms_sum;
        self.dns_fail_misses += s.dns_fail_misses;
        self.active_max = self.active_max.max(s.active_max).max(s.active as u64);
        if s.rss > 0 {
            self.rss_max = self.rss_max.max(s.rss);
            self.rss_sum = self.rss_sum.saturating_add(s.rss.saturating_mul(n));
            self.rss_n += n;
        }
    }

    /// 要約の 1 行 (改行は付けない)。**必ず [`MAX_LINE`] バイト以下**になる
    /// (数は 15 桁・小数は 6 桁 + 1 位・版は [`MAX_VERSION`](super::MAX_VERSION) 文字で頭打ち)。
    ///
    /// `rate_hours` は `/slo` の比の 2 つを 1 時間の合計で判定した `[満たした時間, 外した時間]`
    /// (T.29.33。その日のうちこのプロセスが動いていた時間だけ)。
    pub(super) fn line(&self, version: &str, rate_hours: [[u32; 2]; crate::slo::RATES]) -> String {
        let (y, mo, d, ..) = crate::log::civil_from_epoch(self.day.saturating_mul(SECS_PER_DAY));
        let connects = self.connect.count;
        let dns_per = if connects == 0 {
            0.0
        } else {
            self.dns_misses as f64 / connects as f64
        };
        // 名前解決に成功したミス ÷ 確立 (`/slo` の `dns_miss_per_connect` と同じ数え方。T.32.17)。
        // `dns_per_connect` は今までどおり失敗した試みのミスも入れた値で、読む側の互換のため残す
        let resolved_per = if connects == 0 {
            0.0
        } else {
            self.dns_misses.saturating_sub(self.dns_fail_misses) as f64 / connects as f64
        };
        let dns_ms = if self.dns_misses == 0 {
            0.0
        } else {
            self.dns_ms_sum as f64 / self.dns_misses as f64
        };
        let rss_avg = self.rss_sum.checked_div(self.rss_n).unwrap_or(0);
        let mut out = String::with_capacity(MAX_LINE);
        let _ = write!(
            out,
            "{{\"day\":\"{:04}-{:02}-{:02}\",\"t\":{},\"secs\":{},\"samples\":{},\
             \"requests\":{},\"bytes\":{},\"connects\":{},\
             \"connect_p50_ms\":{:.1},\"connect_p95_ms\":{:.1},\
             \"dns_misses\":{},\"dns_per_connect\":{:.3},\"dns_miss_ms\":{:.1},\
             \"errors\":{},\"bursts\":{},\"active_max\":{},\"evicted_idle\":{},\
             \"rss_max\":{},\"rss_avg\":{},\"version\":\"{}\",\
             \"slo_rate_hours\":{{\"error_rate\":[{},{}],\"dns_miss_per_connect\":[{},{}]}},\
             \"dns_fail_misses\":{},\"dns_resolved_per_connect\":{:.3}}}",
            y,
            mo,
            d,
            num(self.day.saturating_mul(SECS_PER_DAY)),
            num(self
                .seed_secs
                .saturating_add(self.last_t.saturating_sub(self.first_t))),
            num(self.samples),
            num(self
                .seed_requests
                .saturating_add(self.end.requests.saturating_sub(self.start.requests))),
            num(self
                .seed_bytes
                .saturating_add(self.end.bytes.saturating_sub(self.start.bytes))),
            num(connects),
            flt(self.connect.quantile_ms(0.5)),
            flt(self.connect.quantile_ms(0.95)),
            num(self.dns_misses),
            flt(dns_per),
            flt(dns_ms),
            num(self.errors),
            num(self.end.bursts.saturating_sub(self.start.bursts)),
            num(self.active_max),
            num(self.seed_evicted.saturating_add(
                self.end
                    .evicted_idle
                    .saturating_sub(self.start.evicted_idle)
            )),
            num(self.rss_max),
            num(rss_avg),
            self.versions.as_deref().unwrap_or(version),
            rate_hours[0][0].min(99),
            rate_hours[0][1].min(99),
            rate_hours[1][0].min(99),
            rate_hours[1][1].min(99),
            // 末尾に足した欄 (T.32.17)。無い古い行は読む側が `dns_per_connect` に落ちる
            num(self.dns_fail_misses),
            flt(resolved_per),
        );
        out
    }
}

fn num(v: u64) -> u64 {
    v.min(MAX_NUM)
}

fn flt(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(0.0, MAX_FLOAT)
    } else {
        0.0
    }
}
