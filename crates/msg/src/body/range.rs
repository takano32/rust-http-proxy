//! `Range` 要求の解釈 (RFC 9110 §14)。

/// `Range` 要求の解釈結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// 両端を含むバイト範囲
    Bytes { start: u64, end: u64 },
    /// 416 を返す
    Unsatisfiable,
    /// 対応しない形 (複数範囲など) → 200 で全体を返す
    Ignore,
}

/// 単一範囲の `Range: bytes=...` を `total` バイトの表現に当てはめる (RFC 9110 §14)。
pub fn parse_range(header: &str, total: u64) -> RangeSpec {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeSpec::Ignore;
    };
    if spec.contains(',') {
        return RangeSpec::Ignore;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return RangeSpec::Ignore;
    };
    let (a, b) = (a.trim(), b.trim());
    let parsed = if a.is_empty() {
        // 末尾 n バイト
        match b.parse::<u64>() {
            Ok(0) | Err(_) => {
                return if b.is_empty() {
                    RangeSpec::Ignore
                } else {
                    RangeSpec::Unsatisfiable
                };
            }
            Ok(n) => (total.saturating_sub(n), total.saturating_sub(1)),
        }
    } else {
        let Ok(start) = a.parse::<u64>() else {
            return RangeSpec::Ignore;
        };
        let end = if b.is_empty() {
            total.saturating_sub(1)
        } else {
            match b.parse::<u64>() {
                Ok(e) if e >= start => e.min(total.saturating_sub(1)),
                _ => return RangeSpec::Ignore,
            }
        };
        (start, end)
    };
    if total == 0 || parsed.0 >= total {
        return RangeSpec::Unsatisfiable;
    }
    RangeSpec::Bytes {
        start: parsed.0,
        end: parsed.1,
    }
}
