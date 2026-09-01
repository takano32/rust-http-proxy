//! HTTP 日付 (RFC 9110 §5.6.7) の解析。
//!
//! IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`)、旧 RFC 850 (`Sunday, 06-Nov-94 08:49:37 GMT`)、
//! asctime (`Sun Nov  6 08:49:37 1994`) の 3 形式を受け付け、UNIX 時刻 (秒) を返す。

/// 解析できなければ `None`。1970 年より前も `None`。
pub fn parse(s: &str) -> Option<u64> {
    let tokens: Vec<&str> = s
        .split([' ', ',', '\t'])
        .filter(|t| !t.is_empty())
        .collect();
    match tokens.as_slice() {
        // IMF-fixdate: Sun, 06 Nov 1994 08:49:37 GMT
        [_wkday, dd, mon, yyyy, time, _tz] if yyyy.len() == 4 => civil(yyyy, mon, dd, time),
        // 曜日無しの寛容な形
        [dd, mon, yyyy, time, _tz] if yyyy.len() == 4 => civil(yyyy, mon, dd, time),
        // asctime: Sun Nov  6 08:49:37 1994
        [_wkday, mon, dd, time, yyyy] if yyyy.len() == 4 => civil(yyyy, mon, dd, time),
        // RFC 850: Sunday, 06-Nov-94 08:49:37 GMT
        [_wkday, dmy, time, _tz] => {
            let mut p = dmy.split('-');
            let (dd, mon, yy) = (p.next()?, p.next()?, p.next()?);
            let year = match yy.len() {
                2 => {
                    let y: u32 = yy.parse().ok()?;
                    // RFC 9110: 2 桁年は 20 世紀末を境に解釈する
                    if y >= 70 { 1900 + y } else { 2000 + y }
                }
                4 => yy.parse().ok()?,
                _ => return None,
            };
            civil(&year.to_string(), mon, dd, time)
        }
        _ => None,
    }
}

fn civil(year: &str, mon: &str, day: &str, time: &str) -> Option<u64> {
    let y: i64 = year.parse().ok()?;
    let m = month(mon)?;
    let d: u32 = day.parse().ok()?;
    let mut hms = time.split(':');
    let h: u32 = hms.next()?.parse().ok()?;
    let mi: u32 = hms.next()?.parse().ok()?;
    let s: u32 = hms.next()?.parse().ok()?;
    if !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    epoch_from_civil(y, m, d, h, mi, s)
}

fn month(s: &str) -> Option<u32> {
    const NAMES: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = s.to_ascii_lowercase();
    NAMES
        .iter()
        .position(|n| *n == lower.as_str())
        .map(|i| i as u32 + 1)
}

/// 暦日時 (UTC) から UNIX 時刻 (秒)。1970 年より前は `None`。
pub fn epoch_from_civil(y: i64, m: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<u64> {
    // Howard Hinnant の days_from_civil
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h as i64 * 3600 + mi as i64 * 60 + s as i64;
    u64::try_from(secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_three_formats() {
        let expected = 784_111_777;
        assert_eq!(parse("Sun, 06 Nov 1994 08:49:37 GMT"), Some(expected));
        assert_eq!(parse("Sunday, 06-Nov-94 08:49:37 GMT"), Some(expected));
        assert_eq!(parse("Sun Nov  6 08:49:37 1994"), Some(expected));
        assert_eq!(parse("06 Nov 1994 08:49:37 GMT"), Some(expected));
        assert_eq!(parse("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse("Wed, 01 Jan 2025 00:00:00 GMT"), Some(1_735_689_600));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse("0"), None);
        assert_eq!(parse("-1"), None);
        assert_eq!(parse("Sun, 32 Nov 1994 08:49:37 GMT"), None);
        assert_eq!(parse("Sun, 06 Foo 1994 08:49:37 GMT"), None);
        assert_eq!(parse("Sun, 06 Nov 1969 08:49:37 GMT"), None);
    }

    #[test]
    fn civil_round_trip_matches_log_module() {
        let epoch = 1_788_288_860;
        let (y, mo, d, h, mi, s) = crate::log::civil_from_epoch(epoch);
        assert_eq!(epoch_from_civil(y, mo, d, h, mi, s), Some(epoch));
    }
}
