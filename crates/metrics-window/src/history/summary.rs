use super::{History, RESOLUTIONS, Sample, Window};
use crate::metrics::{ERR_CAUSE_NAMES, ERR_CAUSES};
use crate::sync::LockExt;
use std::fmt::Write as _;

/// 「平常時」の閾 (T14.0): **1 時間に 300 本以上**確立した標本はバーストとして外す。
/// `scripts/snapshot-diff.py` の `BURST_PER_HOUR` と同じ値。
///
/// T.29.13 からは**下限**で、実際の閾は「これと、その期間の 1 標本の本数の中央値 ×
/// [`BURST_MEDIAN_K`] の大きい方」([`normal_limit`])。tokyo は毎時 475〜1,760 本で、300 本で切ると
/// `normal_hours_only=1` が 23 時間の全部をバーストとして外していた (`samples 0, burst_samples 25`)。
pub const BURST_PER_HOUR: u64 = 300;

/// 平常時の閾の「中央値の何倍」(T.29.13)。`scripts/snapshot_criteria.py` の phase22 の
/// `burst_median_k` (T22.1) と同じ値で、量の揃った日のいちばん多い 1 時間 (最大 / 中央値 2.0) を切らない。
pub const BURST_MEDIAN_K: u64 = 3;

/// 応答の上限 (T14.24 の受け入れ基準)。標本を返さないので実際は 1 KiB 前後。
pub const MAX_BODY: usize = 4 * 1024;

/// 要求された期間と切り方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Params {
    /// 期間の始まり (epoch 秒。0 = 残っている最古から。`since=restart` は
    /// 呼ぶ側が `now - since_start_secs` にして渡す)
    pub since: u64,
    /// 期間の終わり (epoch 秒。ふつうは今)
    pub until: u64,
    /// `?res=` (秒)。無ければ**期間の長さ**で選ぶ
    pub res: Option<u64>,
    /// バーストの標本を外す (T14.0 の「平常時」。閾は [`normal_limit`]。T.29.13)
    pub normal_hours_only: bool,
}

impl Params {
    /// 使う解像度の添字。`?res=` があればそれ、無ければ期間の長さで選ぶ:
    /// **直近 1 時間は 5 秒、1 日は 60 秒、それ以上は 3,600 秒の窓**
    /// (境目は [`RESOLUTIONS`] の 5 秒 × 720 と 60 秒 × 1,440)。
    ///
    /// **自動選択の閾は T14.32 でも変えていない**: 5 秒のリングは 6 時間ぶん
    /// ([`History::capacity`]) 持つようになったので `?res=5` と**明示すれば**
    /// 6 時間前まで 5 秒で畳めるが、書かなかったときの既定は今までどおり
    /// 「1 時間を越えたら 60 秒」にする (応答も畳む費用も今までと同じにするため)。
    pub fn res_index(&self) -> usize {
        if let Some(secs) = self.res {
            return History::index_for(secs);
        }
        let span = self.until.saturating_sub(self.since);
        RESOLUTIONS
            .iter()
            .position(|(secs, cap)| span <= secs * *cap as u64)
            .unwrap_or(RESOLUTIONS.len() - 1)
    }
}

/// その解像度で「バースト」と見なす 1 標本あたりの本数 (1 時間 300 本を割った値)。
///
/// `scripts/snapshot-diff.py` の `limit` と同じ計算 (四捨五入、下限 1)。
/// 60 秒の窓では 5 本、5 秒の窓では 1 本 — **5 秒の窓で平常時を切ると
/// 1 本でも確立した標本は全部外れる**ので、平常時を見るのは 60 秒か 1 時間の窓。
pub fn burst_limit(interval_secs: u64) -> u64 {
    ((BURST_PER_HOUR * interval_secs + 1800) / 3600).max(1)
}

/// 平常時の閾 (1 標本の本数。これ以上はバースト) を、その期間の標本の本数 `counts` から出す (T.29.13)。
///
/// `base` ([`burst_limit`]) と「本数の中央値 × [`BURST_MEDIAN_K`]」(切り上げ) の大きい方。
/// **`scripts/snapshot_criteria.py` の `normal_limit` と同じ計算** (中央値は本数が偶数なら真ん中 2 つの
/// 平均)。標本が無ければ `base`。
pub fn normal_limit(base: u64, counts: &mut [u64]) -> u64 {
    if counts.is_empty() {
        return base;
    }
    counts.sort_unstable();
    let n = counts.len();
    // 中央値 × k を切り上げる (偶数のときは (a + b) / 2 × k = (a + b) × k / 2)
    let twice = if n % 2 == 1 {
        2 * counts[n / 2]
    } else {
        counts[n / 2 - 1] + counts[n / 2]
    };
    base.max((twice * BURST_MEDIAN_K).div_ceil(2))
}

/// 期間を畳んだ結果 (標本は持たない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    /// 要求された期間 (`since` / `until` をそのまま返す)
    pub from: u64,
    pub to: u64,
    /// 使った解像度 (秒)
    pub interval_secs: u64,
    /// 実際に入った標本の最初と最後の時刻 (窓の始まり。標本が無ければ 0)
    pub first_t: u64,
    pub last_t: u64,
    /// 畳んだ標本の数と、平常時から外した標本の数
    pub samples: u64,
    pub burst_samples: u64,
    pub normal_hours_only: bool,
    /// 期間ぶん足し合わせた確立時間の分布 (CONNECT と forward の初バイト)
    pub connect: Window,
    pub forward: Window,
    pub errors: u64,
    pub errors_by_cause: [u64; ERR_CAUSES],
    pub dns_misses: u64,
    pub dns_ms_sum: u64,
    /// 同時接続数の山 (ゲージなので最大値で畳む)
    pub active_max: u64,
    /// `normal_hours_only` で使った閾 (1 標本の本数。これ以上をバーストとして外した。T.29.13)。
    /// 切っていなければ 0
    pub burst_limit: u64,
    /// 名前解決を除いた「確立 − オリジン側の RTT」の分布 (T.29.12 の `conn_excess`)。
    /// 異常検知の `connect_p95` はこちらで読む (T.29.11)。`/history?summary=1` の JSON には出さない
    pub conn_excess: Window,
    /// `dns_misses` のうち名前解決に失敗した試みが払ったぶん (標本の `dns_fail_misses`。T.32.36)。
    /// 異常検知の `dns_miss_rate` は `/slo` と同じく `(dns_misses − dns_fail_misses) ÷ 確立` で読む。
    /// JSON には出さない (`conn_excess` と同じ)
    pub dns_fail_misses: u64,
    /// 名前解決に成功した 443・80 以外の宛先へのエラー (標本の `other_port_errors`。T.32.36)。
    /// 異常検知の `errors` は `/slo` と同じく引く。JSON には出さない
    pub other_port_errors: u64,
    /// `dns_ms_sum` のうち名前解決に失敗した試みが払った ms (標本の `dns_fail_ms_sum`。T.32.16)。
    /// 異常検知の `dns_slow` は名前解決に成功したミスで読む。JSON には出さない
    pub dns_fail_ms_sum: u64,
}

impl Summary {
    /// 標本 1 本を足す (平常時の判定は呼ぶ側で済ませてある)。
    fn add(&mut self, s: &Sample) {
        self.first_t = if self.samples == 0 {
            s.t
        } else {
            self.first_t.min(s.t)
        };
        self.last_t = self.last_t.max(s.t);
        self.samples += 1;
        self.connect.merge(&s.connect);
        self.forward.merge(&s.forward);
        self.errors += s.errors;
        for (a, b) in self
            .errors_by_cause
            .iter_mut()
            .zip(s.errors_by_cause.iter())
        {
            *a += *b;
        }
        self.dns_misses += s.dns_misses;
        self.dns_ms_sum += s.dns_ms_sum;
        self.active_max = self.active_max.max(s.active_max).max(s.active as u64);
        self.conn_excess.merge(&s.conn_excess);
        self.dns_fail_misses += s.dns_fail_misses;
        self.other_port_errors += s.other_port_errors;
        self.dns_fail_ms_sum += s.dns_fail_ms_sum;
    }

    /// 名前解決に成功した試みのミスが払った ms (`dns_ms_sum − dns_fail_ms_sum`。T.32.16)。
    pub fn resolved_dns_ms_sum(&self) -> u64 {
        self.dns_ms_sum.saturating_sub(self.dns_fail_ms_sum)
    }

    /// 名前解決に成功したミス 1 回の平均 ms (T.32.16。`dns_slow` が読む)。
    pub fn resolved_dns_miss_avg_ms(&self) -> f64 {
        match self.resolved_dns_misses() {
            0 => 0.0,
            n => self.resolved_dns_ms_sum() as f64 / n as f64,
        }
    }

    /// 名前解決に失敗したエラー (原因 `dns` の全部。`nxdomain` も `dns_temp` も。T.32.36)。
    pub fn dns_failed_errors(&self) -> u64 {
        self.errors_by_cause[crate::metrics::ErrCause::Dns as usize].min(self.errors)
    }

    /// `/slo` の `error_rate` の分子と同じ数え方のエラー (T.32.36。利用者の決定 (a)):
    /// **エラー − 名前解決の失敗 − 443・80 以外の宛先のエラー**。T.30.3 より前の標本は
    /// `other_port_errors` が 0 なので、名前解決の失敗だけを引いた数になる。
    pub fn web_errors(&self) -> u64 {
        let resolved = self.errors - self.dns_failed_errors();
        resolved - self.other_port_errors.min(resolved)
    }

    /// 名前解決に成功した試みが払ったミス (`dns_misses − dns_fail_misses`。T.32.36)。
    pub fn resolved_dns_misses(&self) -> u64 {
        self.dns_misses.saturating_sub(self.dns_fail_misses)
    }

    /// `/slo` の `dns_miss_per_connect` と同じ数え方の率 (名前解決に成功したミス ÷ 確立。T.32.36)。
    pub fn resolved_dns_miss_per_connect(&self) -> f64 {
        if self.connect.count == 0 {
            0.0
        } else {
            self.resolved_dns_misses() as f64 / self.connect.count as f64
        }
    }

    /// 確立した CONNECT の本数。
    pub fn connects(&self) -> u64 {
        self.connect.count
    }

    /// 名前解決のミス ÷ 確立した CONNECT (T14.0 の 0.55)。
    pub fn dns_miss_per_connect(&self) -> f64 {
        if self.connect.count == 0 {
            0.0
        } else {
            self.dns_misses as f64 / self.connect.count as f64
        }
    }

    /// ミス 1 回の平均 ms (T14.0 の 11.5)。
    pub fn dns_miss_avg_ms(&self) -> f64 {
        if self.dns_misses == 0 {
            0.0
        } else {
            self.dns_ms_sum as f64 / self.dns_misses as f64
        }
    }

    /// `/history?summary=1` の本体 ([`Summary::to_json_with`] に何も添えない版)。
    pub fn to_json(&self) -> String {
        self.to_json_with(None)
    }

    /// `/history?summary=1` の本体。**必ず [`MAX_BODY`] 以下**になる (項目が固定で
    /// 標本を持たないため。桁の上限は 20 桁 × 30 項目 + 名前で 1 KiB 前後)。
    ///
    /// `recent` は直近 1,024 本の正確な分位点 (T14.31) を組んだ文字列。**`p50_ms` と
    /// 混ぜずに別の鍵 `recent_quantiles` で末尾に添える**: こちらは期間 (`from`〜`to`) を
    /// 畳んだ区間の補間、あちらは期間に関わらず直近 1,024 本の実測なので、
    /// **同じ数字ではない** (期間が短いほど食い違う)。
    pub fn to_json_with(&self, recent: Option<&str>) -> String {
        let mut out = String::with_capacity(768);
        // 応答の形の版は**いちばん先頭の鍵** (T14.49)
        out.push_str(crate::metrics::SCHEMA_HEAD);
        let _ = write!(
            out,
            "\"from\":{},\"to\":{},\"interval_secs\":{},\"first_t\":{},\"last_t\":{},\
             \"samples\":{},\"burst_samples\":{},\"normal_hours_only\":{},\
             \"connects\":{},\"p50_ms\":{:.1},\"p95_ms\":{:.1},\"avg_ms\":{:.1},\"max_ms\":{},\
             \"forwards\":{},\"forward_p50_ms\":{:.1},\"forward_p95_ms\":{:.1},\
             \"forward_avg_ms\":{:.1},\"forward_max_ms\":{},\
             \"dns_misses\":{},\"dns_miss_per_connect\":{:.3},\"dns_miss_avg_ms\":{:.1},\
             \"errors\":{},\"errors_by_cause\":[",
            self.from,
            self.to,
            self.interval_secs,
            self.first_t,
            self.last_t,
            self.samples,
            self.burst_samples,
            self.normal_hours_only,
            self.connect.count,
            self.connect.quantile_ms(0.5),
            self.connect.quantile_ms(0.95),
            self.connect.avg_ms(),
            self.connect.ms_max,
            self.forward.count,
            self.forward.quantile_ms(0.5),
            self.forward.quantile_ms(0.95),
            self.forward.avg_ms(),
            self.forward.ms_max,
            self.dns_misses,
            self.dns_miss_per_connect(),
            self.dns_miss_avg_ms(),
            self.errors,
        );
        for (i, c) in self.errors_by_cause.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", c);
        }
        out.push_str("],\"causes\":[");
        super::push_str_array(&mut out, &ERR_CAUSE_NAMES);
        let _ = write!(out, "],\"active_max\":{}", self.active_max);
        // 末尾に足した鍵 (T.29.13): 平常時を切った閾
        let _ = write!(out, ",\"burst_limit\":{}", self.burst_limit);
        if let Some(q) = recent {
            let _ = write!(out, ",\"recent_quantiles\":{}", q);
        }
        out.push('}');
        out
    }
}

/// 期間を切って畳む (**`/history` の標本を読むのはここだけ**)。
///
/// 窓は標本の時刻 (`t`) で切る: `since <= t <= until` の標本が入る
/// (`scripts/snapshot-diff.py` が再起動時刻で切るのと同じ)。`t` の意味は解像度で違う:
/// **5 秒の標本は取った時刻 (区間の終わり。秒の切り捨て)**、60 秒と 3,600 秒の標本は
/// 窓の始まり ([`History::push`] が閉じた窓の頭の時刻で作る)。
pub fn of(h: &History, p: &Params) -> Summary {
    let res = p.res_index();
    let interval = RESOLUTIONS[res].0;
    let ring = h.rings[res].locked();
    let in_window = |s: &&Sample| s.t >= p.since && s.t <= p.until;
    // 平常時 = 「1 時間 300 本と、その期間の 1 標本の本数の中央値 × 3 の大きい方」未満の標本 (T.29.13)
    let limit = p.normal_hours_only.then(|| {
        let mut counts: Vec<u64> = ring
            .iter()
            .filter(in_window)
            .map(|s| s.connect.count)
            .collect();
        normal_limit(burst_limit(interval), &mut counts)
    });
    let mut out = Summary {
        from: p.since,
        to: p.until,
        interval_secs: interval,
        normal_hours_only: p.normal_hours_only,
        burst_limit: limit.unwrap_or(0),
        ..Summary::default()
    };
    for s in ring.iter().filter(in_window) {
        // 平常時の標本だけ (T14.0 / T.29.13)。外した数も返す
        if limit.is_some_and(|l| s.connect.count >= l) {
            out.burst_samples += 1;
            continue;
        }
        out.add(s);
    }
    out
}
