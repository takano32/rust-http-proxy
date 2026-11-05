//! 断った要求の数の読み方 (`/metrics` の系列と `/status` の `rejected_requests`。T34.110)。

/// `reason` の系列の値 (`/metrics` の `sorahost_rejected_requests_total{reason="…"}`)。
pub fn prom_value(text: &str, reason: &str) -> u64 {
    let needle = format!("sorahost_rejected_requests_total{{reason=\"{}\"}} ", reason);
    let line = text
        .lines()
        .find(|l| l.starts_with(&needle))
        .unwrap_or_else(|| panic!("{} の系列が無い", reason));
    line[needle.len()..].trim().parse().unwrap()
}

/// `/status` の `rejected_requests` から理由別の値だけを取り出す (その `{…}` の中だけを探す)。
pub fn rejected(status: &str, reason: &str) -> u64 {
    let start = status
        .find("\"rejected_requests\":{")
        .unwrap_or_else(|| panic!("rejected_requests が無い: {}", status));
    let rest = &status[start..];
    let obj = &rest[..rest.find('}').unwrap()];
    let needle = format!("\"{}\":", reason);
    let at = obj
        .find(&needle)
        .unwrap_or_else(|| panic!("{} が無い: {}", reason, obj));
    let tail = &obj[at + needle.len()..];
    tail.chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} の値が読めない: {}", reason, obj))
}
