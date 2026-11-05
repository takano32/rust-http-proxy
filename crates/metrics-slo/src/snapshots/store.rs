//! 置き場所にあるファイルの一覧と間引き (`snapshots` の一部。T.34.2 で `snapshots.rs` から移した)。
//!
//! 日次の 1 枚 ([`existing`] / [`newest_day`] / [`prune`]) と起動・停止の 1 枚 ([`existing_moments`] /
//! [`prune_moments`]) を名前の形で見分ける。**何を消すかの判断** ([`stale_moments`]) はディスクを
//! 触らない関数に出してあり、単体試験はそこへ直に並びを渡して書ける。
//!
//! 行を動かしただけで中身は変えていない。

use std::fs;
use std::path::Path;

use super::names::{Moment, day_of, is_date, parse_moment};
use super::{EXT, SECS_PER_DAY};
use crate::log_warn;

/// 置いてある日付 (古い順)。`<YYYY-MM-DD>.json` だけを見る。
pub(super) fn existing(dir: &Path) -> Vec<String> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut days: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let date = name.strip_suffix(EXT)?;
            is_date(date).then(|| date.to_string())
        })
        .collect();
    // 名前が `YYYY-MM-DD` なので、文字列の並びがそのまま日付の並び
    days.sort_unstable();
    days
}

/// いちばん新しいファイルの日 (無ければ 0)。
pub(super) fn newest_day(dir: &Path) -> u64 {
    existing(dir)
        .last()
        .and_then(|d| day_of(d))
        .unwrap_or_default()
}

/// `days` 個を超えたぶんを**古い方から**消す。消した数を返す。
pub(super) fn prune(dir: &Path, days: usize) -> usize {
    let all = existing(dir);
    let over = all.len().saturating_sub(days);
    let mut dropped = 0;
    for date in all.into_iter().take(over) {
        let path = dir.join(format!("{}{}", date, EXT));
        match fs::remove_file(&path) {
            Ok(()) => dropped += 1,
            Err(e) => log_warn!(
                None,
                "daily snapshot {}: cannot drop: {}",
                path.display(),
                e
            ),
        }
    }
    dropped
}

/// 置いてある起動と停止の 1 枚 (名前の時刻の古い順)。`(名前, 撮った時刻, 種類)`。
pub(super) fn existing_moments(dir: &Path) -> Vec<(String, u64, Moment)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<(String, u64, Moment)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(EXT)?;
            let (t, m) = parse_moment(stem)?;
            Some((stem.to_string(), t, m))
        })
        .collect();
    v.sort_unstable_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    v
}

/// `m` の種類のうち、`now` から `days` 日より古いものと、新しい方から `days` 枚を越えたものを消す
/// (T.28.20)。消した数を返す。**別の種類と日次の 1 枚には触らない**。
pub(super) fn prune_moments(dir: &Path, days: usize, m: Moment, now: u64) -> usize {
    let mine: Vec<(String, u64)> = existing_moments(dir)
        .into_iter()
        .filter(|(_, _, k)| *k == m)
        .map(|(name, t, _)| (name, t))
        .collect();
    let mut dropped = 0;
    for name in stale_moments(&mine, days, now) {
        let path = dir.join(format!("{}{}", name, EXT));
        match fs::remove_file(&path) {
            Ok(()) => dropped += 1,
            Err(e) => log_warn!(None, "snapshot {}: cannot drop: {}", path.display(), e),
        }
    }
    dropped
}

/// 消す名前 (古い順に並んだ同じ種類の `(名前, 撮った時刻)` から): `now` から `days` 日より古いものと、
/// 新しい方から `days` 枚を越えたぶん (T.28.20)。**時計もディスクも触らない。**
fn stale_moments(mine: &[(String, u64)], days: usize, now: u64) -> Vec<&str> {
    let oldest = now.saturating_sub(days as u64 * SECS_PER_DAY);
    let over = mine.len().saturating_sub(days);
    mine.iter()
        .enumerate()
        .filter(|(i, (_, t))| *i < over || *t < oldest)
        .map(|(_, (name, _))| name.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 消すのは「`days` 日より古い」か「新しい方から `days` 枚を越えた」もの (T.28.20。T.34.2 で直に押さえる)。
    #[test]
    fn stale_moments_drop_the_old_and_the_surplus() {
        let day = SECS_PER_DAY;
        let now = 10 * day;
        let mine: Vec<(String, u64)> = [1u64, 6, 8, 9, 10]
            .into_iter()
            .map(|d| (format!("m{}", d), d * day))
            .collect();
        // 3 日より古いのは m1・m6 (7 日前 = ちょうど境目は残る)、枚数の 3 を越えるのは古い方から 2 枚
        assert_eq!(stale_moments(&mine, 3, now), vec!["m1", "m6"]);
        // 日数では消えないが、枚数で古い方から 1 枚
        assert_eq!(stale_moments(&mine[1..], 3, 9 * day), vec!["m6"]);
        // 日数で消えるものが枚数より多ければそちらが効く
        assert_eq!(
            stale_moments(&mine, 4, 20 * day),
            vec!["m1", "m6", "m8", "m9", "m10"]
        );
        // 0 日 = 全部消す、何も無ければ何も消さない
        assert_eq!(stale_moments(&mine, 0, now).len(), 5);
        assert!(stale_moments(&[], 3, now).is_empty());
    }
}
