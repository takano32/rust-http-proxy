//! 標本 1 本の判定 ([`judge`] → [`Verdict`]。`slo` の一部。T34.2 で `slo.rs` から移した)。
//!
//! **時計もリングも触らない純粋な関数**で、4 つの閾 ([`Thresholds`]) を 5 秒の標本 1 本に当てる。
//! 比の 2 つは 1 時間の合計で判定する (T29.33) ので、ここで出す `bits` の 2・3 番目は参考で、
//! 達成の判定に使うのは分子と分母 (`num` / `den`) と速さの 2 ビット ([`Verdict::fast`])。
//! 宛先を 1 つ外す規則 ([`excludes`]。T30.4) もここ。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `slo` の `pub use` でそのまま。

use super::{HOUR, Thresholds};
use crate::history::Sample;
use crate::metrics::DestHour;

/// 標本 1 本の判定。**`bits` が本文の「4 ビット」を 5 秒の標本だけで見たもの** (立っている = その閾を外した)。
///
/// T29.33 から、達成の判定に使うのは**確立の 2 つ (ビット 0・1) だけ**で、比の 2 つ (ビット 2・3) は
/// 1 時間の合計 ([`Hour`](super::Hour) の `num` / `den`) で判定する。ここの 2・3 のビットと値は「その 5 秒だけで
/// 見たら」の参考 (分母が 1〜4 本の 5 秒が 96% なので、二値化した数字になる)。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Verdict {
    /// 判定できたか (**確立かエラーが 1 本以上あった**。T20.4)。偽なら分母に入れない
    pub judged: bool,
    /// 外した閾のビット (`1 << i`、`i` は [`NAMES`](super::NAMES) の位置)
    pub bits: u8,
    /// その標本の 4 つの値 (外れた時間帯の「その値」に使う)
    pub values: [f64; 4],
    /// 比の 2 つ (`error_rate`・`dns_miss_per_connect`) のこの標本の分子と分母 (T29.33)。
    /// 判定できない標本でも、試み (転送だけの 5 秒など) があれば入る
    pub num: [u64; RATES],
    pub den: [u64; RATES],
}

/// 比の閾の数 (`error_rate` と `dns_miss_per_connect`。[`NAMES`](super::NAMES) の 2・3 番目)。
pub(crate) const RATES: usize = 2;
/// 比の閾が [`NAMES`](super::NAMES) のどこから始まるか。
pub(super) const RATE_AT: usize = 2;
/// 確立の 2 つ (速さ) のビット。**5 秒の標本ごと**に判定するのはこの 2 つだけ (T29.33)。
pub(super) const SPEED_BITS: u8 = 0b0011;
/// 比を合計する窓 (秒)。`/slo` の `rate_window_secs`
pub(crate) const RATE_WINDOW_SECS: u64 = HOUR;

impl Verdict {
    /// 5 秒の標本だけで見て 4 つとも満たしたか (判定できていないときは偽)。T29.33 からは参考。
    pub fn met(&self) -> bool {
        self.judged && self.bits == 0
    }

    /// 確立の 2 つ (速さ) を満たしたか (T29.33。達成の判定に使うのはこちら)。
    pub fn fast(&self) -> bool {
        self.judged && self.bits & SPEED_BITS == 0
    }
}

/// 標本 1 本を 4 つの閾に当てる (**時計もリングも触らない**ので試験しやすい)。
pub fn judge(th: &Thresholds, s: &Sample) -> Verdict {
    let connects = s.connect.count;
    // 名前解決に失敗した試み (T28.45 の E1)。メール宛の総当たりの NXDOMAIN は
    // 「客が無い名前を頼んだ」もので、プロキシの速さとも失敗とも関係が無い
    let dns_failed = s.errors_by_cause[crate::metrics::ErrCause::Dns as usize].min(s.errors);
    let resolved_errors = s.errors - dns_failed;
    // 試み = 確立 + 転送 + エラー − 名前解決の失敗 (T20.4・T28.45)
    let attempts = (connects + s.forward.count + s.errors) - dns_failed;
    // **443・80 宛てだけ** (T30.3。利用者の決定): 443・80 以外の試みとそのエラーを引く。
    // T30.3 より前の標本はどちらも 0 = 全部のポート (旧の数え方) のまま
    let web_errors = resolved_errors - s.other_port_errors.min(resolved_errors);
    let web_attempts = attempts - s.other_port_attempts.min(attempts);
    // 名前解決のミスは失敗した試みのぶんを除く (T28.45)
    let misses = s.dns_misses.saturating_sub(s.dns_fail_misses);
    let num = [web_errors, misses];
    let den = [web_attempts, connects];
    if connects == 0 && web_errors == 0 {
        // 確立も (名前が引けたうえでの) エラーも 1 本も無い標本は「判定なし」。分位点が
        // 0 になるだけでなく、「誰も使っていない時間」を達成として数えると達成率の意味が
        // 変わる。**名前解決の失敗だけの標本もここ** (T28.45。E1 では分母が 0 になる)。
        // **443・80 以外のエラーだけの標本もここ** (T30.3。メール宛の総当たりだけの 5 秒)。
        // 比の分子と分母は持たせる (転送だけの 5 秒も 1 時間の合計の分母に入る。T29.33)
        return Verdict {
            num,
            den,
            ..Verdict::default()
        };
    }
    // **エラーだけの標本も判定する** (T20.4)。失敗した 1 件は確立の窓に入らなくなったので、
    // 確立 0 本を「判定なし」のままにすると、繋がらない CONNECT だけの時間 (宛先が全滅した
    // 時間帯も) が達成率から消える。確立が無いときは速さとミスの 3 つは 0 のまま
    // (外したことにしない) で、エラー率だけが効く
    let values = [
        // 確立の 2 つは「確立 − カーネルの RTT」の窓 (T28.45)。T29.12 から**名前解決を除いた**
        // `conn_excess` (名前解決の遅れは `dns_slow` に任せる)。**窓が空なら 0** (= 外さない):
        // T29.12 より前の標本と、RTT が 1 本も読めなかった標本 (Linux 以外) はここ
        s.conn_excess.quantile_ms(0.5),
        s.conn_excess.quantile_ms(0.95),
        ratio_of(web_errors, web_attempts),
        ratio_of(misses, connects),
    ];
    let limits = th.limits();
    let mut bits = 0u8;
    for (i, (v, lim)) in values.iter().zip(limits).enumerate() {
        if *v > lim {
            bits |= 1 << i;
        }
    }
    Verdict {
        judged: true,
        bits,
        values,
        num,
        den,
    }
}

/// `num ÷ den` (分母が 0 なら 0)。
fn ratio_of(num: u64, den: u64) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

/// その時間の `error_rate` から、いちばん多くエラーを出した宛先を外すか (T30.4。利用者の決定)。
///
/// **エラーが 2 件以上で、いちばん多い宛先がその時間のエラーの半分以上**なら外す (外すのは 1 つだけ)。
/// エラー 1 件だけの時間・エラーがばらけた時間 (4 つの宛先に 1 件ずつなど) は今までどおり。
pub(crate) fn excludes(d: &DestHour) -> bool {
    d.errors >= 2 && !d.destination.is_empty() && d.top_errors * 2 >= d.errors
}
