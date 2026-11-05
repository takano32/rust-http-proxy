//! 雪像のファイル名の往復 (`snapshots` の一部。T.34.2 で `snapshots.rs` から移した)。
//!
//! 日次の 1 枚は `YYYY-MM-DD` ([`day_name`] ↔ [`day_of`])、起動と停止の 1 枚は
//! `<YYYY-MM-DDTHHMMSSZ>-start|shutdown` ([`moment_name`] ↔ [`parse_moment`])。
//! `/snapshots/<名前>` はここを通った文字列しかファイル名に使わない (`..` や `/` を書かれても
//! 置き場所の外へ出ない)。**時計もディスクも触らない純粋な関数**だけ。
//!
//! 行を動かしただけで中身は変えていない。

use super::SECS_PER_DAY;

/// UTC の日 (epoch 秒 / 86,400) を `YYYY-MM-DD` に。
pub fn day_name(day: u64) -> String {
    let (y, mo, d, ..) = crate::log::civil_from_epoch(day.saturating_mul(SECS_PER_DAY));
    format!("{:04}-{:02}-{:02}", y, mo, d)
}

/// `YYYY-MM-DD` として読める文字列か。
///
/// `/snapshots/<date>` はこれを通った文字列しかファイル名に使わない
/// (`..` や `/` を書かれても置き場所の外へ出ない)。
pub(crate) fn is_date(s: &str) -> bool {
    s.len() == 10
        && s.as_bytes().iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

/// `YYYY-MM-DD` を UTC の日に戻す (並べ替えと「最新の日付」に使う)。
pub(super) fn day_of(name: &str) -> Option<u64> {
    if !is_date(name) {
        return None;
    }
    let y: i64 = name[0..4].parse().ok()?;
    let m: i64 = name[5..7].parse().ok()?;
    let d: i64 = name[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant の days_from_civil (`log::civil_from_epoch` の逆)
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days).ok()
}

/// 起動と停止の 1 枚の種類 (T.28.20)。名前は `<YYYY-MM-DDTHHMMSSZ>-<種類>.json`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// 起動から `memory.baseline` と同じ 60 秒 (締めの「0 時間」)
    Start,
    /// 停止シグナルで状態ファイルを書く前 (締めの「前」)
    Shutdown,
}

impl Moment {
    pub(crate) const ALL: [Moment; 2] = [Moment::Start, Moment::Shutdown];

    /// 名前の終わりと `/snapshots` の `kind` に出す綴り。
    pub fn name(self) -> &'static str {
        match self {
            Moment::Start => "start",
            Moment::Shutdown => "shutdown",
        }
    }
}

/// 撮った時刻と種類から名前 (拡張子なし) を作る: `2026-10-25T203134Z-shutdown`。
///
/// 時刻の書き方は `collect-deployed.sh` の雪像 (`<UTC 時刻>-snapshot.json`) と同じ。
pub(crate) fn moment_name(t: u64, m: Moment) -> String {
    let (y, mo, d, h, mi, s) = crate::log::civil_from_epoch(t);
    format!(
        "{:04}-{:02}-{:02}T{:02}{:02}{:02}Z-{}",
        y,
        mo,
        d,
        h,
        mi,
        s,
        m.name()
    )
}

/// `<YYYY-MM-DDTHHMMSSZ>-start|shutdown` として読めれば (撮った時刻, 種類)。
///
/// `/snapshots/<名前>` はこれ (か [`is_date`]) を通った文字列しかファイル名に使わない
/// (`..` や `/` を書かれても置き場所の外へ出ない)。
pub(crate) fn parse_moment(name: &str) -> Option<(u64, Moment)> {
    let (stamp, kind) = name.split_once("Z-")?;
    let m = Moment::ALL.into_iter().find(|m| m.name() == kind)?;
    let (date, time) = stamp.split_once('T')?;
    let day = day_of(date)?;
    if time.len() != 6 || !time.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (h, mi, s): (u64, u64, u64) = (
        time[0..2].parse().ok()?,
        time[2..4].parse().ok()?,
        time[4..6].parse().ok()?,
    );
    if h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some((day * SECS_PER_DAY + h * 3600 + mi * 60 + s, m))
}
