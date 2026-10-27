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

/// UNIX 時刻 (秒) を IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`、29 字) で `buf` に書く (T.29.9)。
///
/// 応答の `Date` (RFC 9110 §6.6.1) に使う。確保はしない。
pub fn imf_fixdate(epoch: u64, buf: &mut [u8; 29]) -> &str {
    const DAYS: [&[u8; 3]; 7] = [b"Thu", b"Fri", b"Sat", b"Sun", b"Mon", b"Tue", b"Wed"];
    const MONTHS: [&[u8; 3]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    let days = epoch / 86_400;
    let secs = epoch % 86_400;
    // Howard Hinnant の civil_from_days (1970-01-01 は木曜)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as usize;
    let y = (yoe + era * 400 + i64::from(m <= 2)).clamp(0, 9999) as u64;
    let two = |v: u64| [b'0' + (v / 10 % 10) as u8, b'0' + (v % 10) as u8];
    buf[..3].copy_from_slice(DAYS[(days % 7) as usize]);
    buf[3..5].copy_from_slice(b", ");
    buf[5..7].copy_from_slice(&two(d));
    buf[7] = b' ';
    buf[8..11].copy_from_slice(MONTHS[m - 1]);
    buf[11] = b' ';
    buf[12..14].copy_from_slice(&two(y / 100));
    buf[14..16].copy_from_slice(&two(y));
    buf[16] = b' ';
    buf[17..19].copy_from_slice(&two(secs / 3600));
    buf[19] = b':';
    buf[20..22].copy_from_slice(&two(secs / 60 % 60));
    buf[22] = b':';
    buf[23..25].copy_from_slice(&two(secs % 60));
    buf[25..29].copy_from_slice(b" GMT");
    std::str::from_utf8(&buf[..]).unwrap_or("Thu, 01 Jan 1970 00:00:00 GMT")
}

/// 今の時刻の `Date: <IMF-fixdate>` の行 (行末の CRLF は付けない。35 字。T.29.9)。
pub fn date_line(buf: &mut [u8; 35]) -> &str {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    buf[..6].copy_from_slice(b"Date: ");
    let mut date = [0u8; 29];
    imf_fixdate(now, &mut date);
    buf[6..].copy_from_slice(&date);
    std::str::from_utf8(&buf[..]).unwrap_or("Date: Thu, 01 Jan 1970 00:00:00 GMT")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T.29.9: 書いた日付は `parse` で読み戻せて、元の時刻になる。
    #[test]
    fn imf_fixdate_round_trips() {
        let mut buf = [0u8; 29];
        assert_eq!(
            imf_fixdate(784_111_777, &mut buf),
            "Sun, 06 Nov 1994 08:49:37 GMT"
        );
        assert_eq!(imf_fixdate(0, &mut buf), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            imf_fixdate(1_735_689_600, &mut buf),
            "Wed, 01 Jan 2025 00:00:00 GMT"
        );
        // 閏日と年の変わり目
        assert_eq!(
            imf_fixdate(1_709_164_800, &mut buf),
            "Thu, 29 Feb 2024 00:00:00 GMT"
        );
        let mut t = 0u64;
        while t < 4_000_000_000 {
            assert_eq!(parse(imf_fixdate(t, &mut buf)), Some(t), "{}", t);
            t += 86_399 * 37;
        }
    }

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
