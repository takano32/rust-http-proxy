//! 出来事の説明文 (立った / 解除された / 初めて見た接続元)。**数字を必ず入れる**。

use std::fmt::Write as _;

use crate::history::summary::Summary;
use crate::metrics::{ERR_CAUSE_NAMES, ERR_CAUSES, NewClient};
use crate::recent::SpinningConn;

use super::detector::CpuWindow;
use super::*;

/// ms の書き方 (10 ms 未満は小数 1 桁。「0 ms」と書かないため)。
fn ms(v: f64) -> String {
    if v < 10.0 {
        format!("{:.1}", v)
    } else {
        format!("{:.0}", v)
    }
}

/// エラーの原因の内訳 (多い順に 4 つまで。`/status` と同じ綴り)。
fn causes(counts: &[u64; ERR_CAUSES]) -> String {
    let mut idx: Vec<usize> = (0..ERR_CAUSES).filter(|&i| counts[i] > 0).collect();
    idx.sort_by_key(|&i| std::cmp::Reverse(counts[i]));
    idx.truncate(4);
    let mut s = String::new();
    for (n, &i) in idx.iter().enumerate() {
        if n > 0 {
            s.push_str(", ");
        }
        let _ = write!(s, "{} {}", ERR_CAUSE_NAMES[i], counts[i]);
    }
    s
}

/// 説明を組むのに要る「窓から読めないもの」をひとまとめにしたもの。
///
/// [`Detector::observe`] が 1 周期ぶん作り、[`fired_text`] と [`cleared_text`] が読む。
pub(super) struct Facts<'a> {
    /// (`max_conns`, 山と見なす本数)
    pub(super) limits: (usize, usize),
    /// 断った / 追い出した数の増分と累計
    pub(super) delta: [u64; 3],
    pub(super) totals: [u64; 3],
    /// この周期に撮れた山の写真の番号
    pub(super) shot: Option<u64>,
    /// 直近 5 分の CPU の絞り (T15.0 (6))
    pub(super) cpu: CpuWindow,
    /// いちばん回っているトンネル (多くて 1 本) と、閾を越えた本数、続いている秒
    pub(super) spinning: &'a [SpinningConn],
    pub(super) spinning_total: usize,
    pub(super) spin_secs: u64,
    /// `dns_slow` の閾 (ms。[`dns_miss_limit`])
    pub(super) dns_limit: f64,
}

/// 異常の説明に入れる宛先の長さ (バイト)。
///
/// 出来事 1 件は [`crate::events::MAX_TEXT`] (128 B) で切られるので、長い名前で
/// 後ろの数字を押し出さない。
const TARGET_BYTES: usize = 24;

/// 立ったときの説明 (**数字を必ず入れる**: 何が・いくつ・基準値)。
pub(super) fn fired_text(kind: Kind, w5: &Summary, base: &Summary, f: &Facts<'_>) -> String {
    let head = kind.name();
    match kind {
        Kind::ConnectP95 => connect_p95_text(head, w5, base),
        Kind::DnsSlow => dns_slow_text(head, w5, f),
        Kind::Errors => errors_text(head, w5),
        Kind::ActiveHigh => active_high_text(head, w5, f),
        Kind::Rejected => rejected_text(head, f),
        Kind::CpuThrottled => cpu_throttled_text(head, f),
        Kind::TunnelSpin => tunnel_spin_text(head, f),
        Kind::DnsMissRate => dns_miss_rate_text(head, base),
    }
}

/// (1) `connect_p95` が立ったとき。
fn connect_p95_text(head: &str, w5: &Summary, base: &Summary) -> String {
    let p95 = w5.conn_excess.quantile_ms(0.95);
    let p95_base = base.conn_excess.quantile_ms(0.95);
    // `connect-rtt` = 名前解決を除いた「確立 − RTT」(T29.11)。倍率は基準値 1 ms を下限に割る
    format!(
        "{}: connect-rtt p95 {} ms over 5m is {:.1}x the 1h baseline {} ms ({} of {} connects)",
        head,
        ms(p95),
        p95 / p95_base.max(1.0),
        ms(p95_base),
        w5.conn_excess.count,
        base.conn_excess.count
    )
}

/// (2) `dns_slow` が立ったとき。
///
/// 443・80 宛ての、名前解決に成功したミスの数字 (T32.16・T33.3)。外したぶん (443・80 以外の宛ての
/// 成功したミスと、失敗のミス) があればその回数も書く
fn dns_slow_text(head: &str, w5: &Summary, f: &Facts<'_>) -> String {
    let mut left_out = String::new();
    let other = w5.other_port_dns_misses();
    if other > 0 {
        let _ = write!(left_out, ", {} not to 443/80", other);
    }
    if w5.dns_fail_misses > 0 {
        let _ = write!(left_out, ", {} failed", w5.dns_fail_misses);
    }
    if !left_out.is_empty() {
        left_out.push_str(" not counted");
    }
    format!(
        "{}: dns miss {} ms avg over 5m (threshold {} ms; {} misses, {} ms total{})",
        head,
        ms(w5.web_dns_miss_avg_ms()),
        f.dns_limit as u64,
        w5.web_dns_misses,
        w5.web_dns_ms_sum,
        left_out,
    )
}

/// (3) `errors` が立ったとき。
fn errors_text(head: &str, w5: &Summary) -> String {
    let by = causes(&w5.errors_by_cause);
    // 数は判定に使った数 (`/slo` と同じ数え方。T32.36)。外したもの (名前解決の失敗と
    // 443・80 以外の宛先のエラー) があればその件数も書く (無ければ文面は今までどおり)。
    // 原因の内訳は標本の 8 つのまま (外したものも入る)
    let dns = w5.dns_failed_errors();
    let other = w5.other_port_errors.min(w5.errors - dns);
    let mut left_out = String::new();
    if dns > 0 {
        let _ = write!(left_out, "; {} dns not counted", dns);
    }
    if other > 0 {
        let _ = write!(left_out, "; {} not to 443/80", other);
    }
    format!(
        "{}: {} errors in 5m (threshold {}{}){}{}",
        head,
        w5.web_errors(),
        ERRORS_MIN,
        left_out,
        if by.is_empty() { "" } else { ": " },
        by
    )
}

/// (4) `active_high` が立ったとき。
fn active_high_text(head: &str, w5: &Summary, f: &Facts<'_>) -> String {
    let (max_conns, threshold) = f.limits;
    let shot = f.shot;
    // 標本 (5 秒ごと) の山が閾に届いていないのに立つのは、**写真の方が先に
    // 気づいた**とき (accept の経路は越えた瞬間に旗を立てるが、標本はその 5 秒の
    // 断面しか見ない)。そのときは「写真が越えた」と書く (山の数字が食い違って見えないように)
    let sampled = w5.active_max;
    if threshold > 0 && sampled >= threshold as u64 {
        let mut s = format!("{}: active connections peaked at {} in 5m", head, sampled);
        if max_conns > 0 {
            let _ = write!(
                s,
                " of max_conns {} ({}%, threshold {})",
                max_conns,
                sampled.saturating_mul(100) / max_conns as u64,
                threshold
            );
        }
        if let Some(seq) = shot {
            let _ = write!(s, ", burst shot #{}", seq);
        }
        return s;
    }
    format!(
        "{}: burst shot #{} crossed the threshold {} of max_conns {} (sampled peak {} in 5m)",
        head,
        shot.unwrap_or(0),
        threshold,
        max_conns,
        sampled
    )
}

/// (5) `rejected` が立ったとき。
fn rejected_text(head: &str, f: &Facts<'_>) -> String {
    let (delta, totals) = (f.delta, f.totals);
    let mut s = format!("{}:", head);
    for (name, d, total) in [
        ("rejected_overload", delta[0], totals[0]),
        ("evicted_idle", delta[1], totals[1]),
        ("rejected_client_acl", delta[2], totals[2]),
    ] {
        if d > 0 {
            let _ = write!(s, " {} +{} (total {}),", name, d, total);
        }
    }
    s.pop();
    s
}

/// (7) `cpu_throttled` が立ったとき。CPU の上限で絞られ続けている (T15.0 (6))。**割合**で書く
fn cpu_throttled_text(head: &str, f: &Facts<'_>) -> String {
    format!(
        "{}: {:.0}% of {} cpu periods throttled in {}m (threshold {:.0}%; quota {:.2} cores, used {:.2}{})",
        head,
        f.cpu.ratio().unwrap_or(0.0) * 100.0,
        f.cpu.periods,
        f.cpu.secs.div_ceil(60),
        CPU_THROTTLED * 100.0,
        f.cpu.quota_cores,
        f.cpu.cores(),
        f.cpu.top(),
    )
}

/// (8) `tunnel_spin` が立ったとき。起こされても進まないトンネルが回り続けている (T15.0 (6))
fn tunnel_spin_text(head: &str, f: &Facts<'_>) -> String {
    let mut s = format!("{}:", head);
    match f.spinning.first() {
        Some(c) => {
            let _ = write!(
                s,
                " conn#{} spun {} times/tick for {}s ({}, age {}s, half_closed {})",
                c.id,
                c.spins_delta,
                f.spin_secs,
                crate::recent::clip(&c.target, TARGET_BYTES),
                c.age_secs,
                c.half_closed.unwrap_or("no"),
            );
        }
        // 本数だけ分かって個票が取れない周期 (起こりえないが黙らない)
        None => s.push_str(" a tunnel is spinning"),
    }
    if f.spinning_total > 1 {
        let _ = write!(s, ", {} tunnels", f.spinning_total);
    }
    s
}

/// (9) `dns_miss_rate` が立ったとき。名前解決のミスの**率** (T15.0 (9))。窓は 1 時間
///
/// 名前解決に成功したミスで数える (T32.36)。失敗のぶんを外していればその回数も書く
fn dns_miss_rate_text(head: &str, base: &Summary) -> String {
    format!(
        "{}: dns misses {:.2}/connect over 1h (threshold {:.2}; {} misses, {} connects{})",
        head,
        base.resolved_dns_miss_per_connect(),
        DNS_MISS_RATE,
        base.resolved_dns_misses(),
        base.connects(),
        if base.dns_fail_misses > 0 {
            format!(", {} failed not counted", base.dns_fail_misses)
        } else {
            String::new()
        },
    )
}

/// 解除の説明 (**`cleared: <種類>` で始める**。いまの値も入れる)。
pub(super) fn cleared_text(
    kind: Kind,
    w5: &Summary,
    base: &Summary,
    f: &Facts<'_>,
    mins: u64,
) -> String {
    let head = format!("cleared: {} after {}m", kind.name(), mins);
    match kind {
        Kind::ConnectP95 => format!(
            "{} (connect-rtt p95 {} ms over 5m, {} connects)",
            head,
            ms(w5.conn_excess.quantile_ms(0.95)),
            w5.conn_excess.count
        ),
        Kind::DnsSlow => format!(
            "{} (dns miss {} ms avg over 5m, {} misses)",
            head,
            ms(w5.web_dns_miss_avg_ms()),
            w5.web_dns_misses
        ),
        Kind::Errors => format!("{} ({} errors in 5m)", head, w5.web_errors()),
        Kind::ActiveHigh => format!("{} (active peaked at {} in 5m)", head, w5.active_max),
        Kind::Rejected => format!("{} (no new rejects for {}m)", head, CLEAR_SECS / 60),
        Kind::CpuThrottled => format!(
            "{} ({:.0}% of {} cpu periods throttled in 5m)",
            head,
            f.cpu.ratio().unwrap_or(0.0) * 100.0,
            f.cpu.periods
        ),
        Kind::TunnelSpin => format!("{} (no tunnel spinning for {}m)", head, CLEAR_SECS / 60),
        Kind::DnsMissRate => format!(
            "{} (dns misses {:.2}/connect over 1h, {} connects)",
            head,
            base.resolved_dns_miss_per_connect(),
            base.connects()
        ),
    }
}

/// **規則 6** の説明 (T14.54)。接続元・要求数・最初の宛先の種類・`User-Agent` の順。
///
/// `User-Agent` を最後に置いてあるのは、[`crate::events::MAX_TEXT`] (128 B) で切られる
/// ときに**接続元と数字を残す**ため。
pub(super) fn new_client_text(c: &NewClient) -> String {
    let mut s = format!("new_client: {} first seen ({} req", c.client, c.requests);
    match c.port {
        // `ports` は出た順なので、先頭が最初の宛先のポート (443 / 80 / それ以外)
        Some(p) => {
            let _ = write!(
                s,
                ", first target port {} ({})",
                p,
                if c.literal { "literal" } else { "name" }
            );
        }
        // `User-Agent` だけ先に読めていて、要求がまだ数えられていない瞬間
        None => s.push_str(", no target yet"),
    }
    match &c.agent {
        Some(a) => {
            let _ = write!(
                s,
                ", agent \"{}\")",
                crate::recent::clip(a, NEW_CLIENT_AGENT_BYTES)
            );
        }
        None => s.push_str(", no agent)"),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 ms 未満は小数 1 桁 (「0 ms」と書かない)、10 ms からは整数。
    #[test]
    fn ms_keeps_one_decimal_below_ten() {
        assert_eq!(ms(0.04), "0.0");
        assert_eq!(ms(9.94), "9.9");
        assert_eq!(ms(9.96), "10.0");
        assert_eq!(ms(10.0), "10");
        assert_eq!(ms(123.6), "124");
    }

    /// 原因は多い順に 4 つまで (同じ数なら表の順)、0 件の原因は書かない。
    #[test]
    fn causes_lists_the_four_largest() {
        let n = ERR_CAUSE_NAMES;
        let mut counts = [0u64; ERR_CAUSES];
        assert_eq!(causes(&counts), "");
        counts[1] = 3;
        assert_eq!(causes(&counts), format!("{} 3", n[1]));
        counts[0] = 1;
        counts[2] = 5;
        counts[3] = 3;
        counts[4] = 2;
        assert_eq!(
            causes(&counts),
            format!("{} 5, {} 3, {} 3, {} 2", n[2], n[1], n[3], n[4])
        );
    }
}
