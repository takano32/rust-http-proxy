//! 文字列を決まったバイト数に切り詰める [`clip`] (`recent` の一部。T.34.2 で `errors.rs` から移した)。
//!
//! 個票 (エラー・接続元・宛先・SNI)、出来事の説明、追跡のパス、異常の説明の宛先 — 固定長の
//! リングに文字列を残す所はどれもこれ 1 つを通す (文字の途中で切らず、切ったら `…` を付ける)。

/// 文字列を `max` バイト以内に切る (文字の途中で切らない。切ったら `…` を付ける)。
pub fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // `…` は UTF-8 で 3 バイトなので、その手前までに収める
    let room = max.saturating_sub(3);
    let mut end = room;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(max);
    out.push_str(&s[..end]);
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_does_not_split_a_character() {
        assert_eq!(clip("abc", 8), "abc");
        assert_eq!(clip("abcdefghij", 8), "abcde…");
        // 3 バイト文字の途中で切らない
        assert_eq!(clip("日本語です", 8), "日…");
        assert!(clip("日本語です", 8).len() <= 8);
    }
}
