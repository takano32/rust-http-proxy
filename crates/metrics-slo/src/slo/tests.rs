//! `slo` の単体試験の共通部 (T34.2 で `slo.rs` の `mod tests` から移した。試験の名前はそのまま)。
//!
//! 判定 ([`judge`]) の試験は `judge_tests.rs`、集計と `/slo` の応答の試験は `tracker_tests.rs`。
//! 標本と 1 日・1 時間ぶんの並びを作る道具をここに置き、2 つから `use super::*` で引く。

use super::*;

#[path = "judge_tests.rs"]
mod judge;
#[path = "tracker_tests.rs"]
mod tracker;

/// 5 秒の標本 1 本。`connects` 本の確立を `ms` で置く (「確立 − RTT」の窓にも同じ `ms`。
/// 確立の 2 つの閾はそちらで判定する。T28.45)。
fn sample(t: u64, connects: u64, ms: u64) -> Sample {
    let mut s = Sample {
        t,
        ..Sample::default()
    };
    for _ in 0..connects {
        s.connect.observe(ms);
        s.conn_excess.observe(ms);
    }
    s
}

/// T14.0 の 2026-09-11 のバーストを模した 1 日 (UTC)。
///
/// 5 秒の標本を 24 時間 (720 本 × 24 = 17,280 本) 並べ、**17〜23 時の 7 時間だけ**
/// p50 / p95 とエラーが閾を越えるようにする (平常時は 1 標本 2 本を 3 ms、
/// バーストは 7 本のうち 4 本を 30 ms・3 本を 300 ms にしてエラーを 1 件)。
fn burst_day(t0: u64) -> Vec<Sample> {
    (0..24 * 720u64)
        .map(|i| {
            let t = t0 + i * 5;
            let hour = (t - t0) / HOUR;
            if (17..24).contains(&hour) {
                // 4 本が 30 ms ([25,50] の区間) と 3 本が 300 ms ([250,500] の区間) で
                // p50 = 25 + 25 × 3.5/4 = 46.875 (> 10)、p95 = 300 (> 100)。
                // エラー 1 件 ÷ 試み 8 = 0.125 (> 0.005)
                let mut s = sample(t, 0, 0);
                for _ in 0..4 {
                    s.connect.observe(30);
                    s.conn_excess.observe(30);
                }
                for _ in 0..3 {
                    s.connect.observe(300);
                    s.conn_excess.observe(300);
                }
                s.errors = 1;
                s
            } else {
                // 2 本を 3 ms → p50 = p95 = 3 ms、エラー 0、名前解決のミス 0
                sample(t, 2, 3)
            }
        })
        .collect()
}

/// 1 時間に確立 `connects` 本 (720 本の 5 秒に配る) と、名前解決に成功した試みのエラー `errors` 件。
///
/// `spread` が真ならエラーは確立の多い 5 秒に、偽なら**確立 1 本だけの 5 秒**に置く
/// (5 秒ごとに判定すると後者は 1 / 2 = 0.5 で外れる)。
fn hour_of(t0: u64, connects: u64, errors: u64, spread: bool) -> Vec<Sample> {
    let mut out: Vec<Sample> = (0..720u64).map(|i| sample(t0 + i * 5, 0, 0)).collect();
    // 確立は先頭の 5 秒から 1 本ずつ配り、余りは最後の 5 秒にまとめる
    let ones = connects.min(719);
    for s in out.iter_mut().take(ones as usize) {
        s.connect.observe(3);
        s.conn_excess.observe(1);
    }
    for _ in ones..connects {
        out[719].connect.observe(3);
        out[719].conn_excess.observe(1);
    }
    for k in 0..errors as usize {
        let at = if spread { 719 } else { k };
        out[at].errors += 1;
        out[at].errors_by_cause[crate::metrics::ErrCause::Refused as usize] += 1;
    }
    out
}

/// その時間の宛先の表 (T30.4)。
fn dest(hour: u64, errors: u64, name: &str, top_errors: u64, top_attempts: u64) -> DestHour {
    DestHour {
        hour,
        errors,
        destination: name.to_string(),
        top_errors,
        top_attempts,
    }
}
