//! 固定長ペイロードの符号 ([`Enc`]) と復号 ([`Dec`])。u64 はリトルエンディアン、
//! 文字列は長さ 1 バイト + 本体の固定幅。**足りなければ 0 / 空文字列**で読む (古い版の
//! 短いレコードを、欄を末尾に足しただけで読み継ぐための約束)。

use super::STATS_RECORD;

/// 固定長ペイロードの組み立て (u64 はリトルエンディアン、文字列は長さ 1 バイト + 本体)。
pub struct Enc(pub Vec<u8>);

impl Enc {
    pub fn new() -> Self {
        Enc(Vec::with_capacity(STATS_RECORD))
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// 固定幅 `width` の文字列 (先頭 1 バイトが長さ、超える分は切る)。
    pub fn str(&mut self, s: &str, width: usize) -> &mut Self {
        let max = width - 1;
        let mut end = s.len().min(max);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push(end as u8);
        self.0.extend_from_slice(&s.as_bytes()[..end]);
        self.0.resize(self.0.len() + (max - end), 0);
        self
    }
}

impl Default for Enc {
    fn default() -> Self {
        Self::new()
    }
}

/// [`Enc`] の逆。足りなければ 0 / 空文字列。
pub struct Dec<'a>(pub &'a [u8]);

impl Dec<'_> {
    pub fn u64(&mut self) -> u64 {
        if self.0.len() < 8 {
            self.0 = &[];
            return 0;
        }
        let v = u64::from_le_bytes(self.0[..8].try_into().unwrap());
        self.0 = &self.0[8..];
        v
    }
    pub fn str(&mut self, width: usize) -> String {
        if self.0.len() < width {
            self.0 = &[];
            return String::new();
        }
        let len = (self.0[0] as usize).min(width - 1);
        let s = String::from_utf8_lossy(&self.0[1..1 + len]).into_owned();
        self.0 = &self.0[width..];
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_truncated_on_char_boundaries() {
        let mut e = Enc::new();
        e.str(&"あ".repeat(100), 16);
        assert_eq!(e.0.len(), 16);
        let mut d = Dec(&e.0);
        assert_eq!(d.str(16), "あ".repeat(5));
    }

    /// u64 はリトルエンディアンの 8 B、文字列は長さ + 本体 + ゼロ埋めで `width` ちょうど。
    #[test]
    fn numbers_are_little_endian_and_strings_are_padded_to_the_width() {
        let mut e = Enc::new();
        e.u64(0x0102_0304_0506_0708).str("ab", 6).u64(7);
        assert_eq!(
            e.0,
            [
                8, 7, 6, 5, 4, 3, 2, 1, 2, b'a', b'b', 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0
            ]
        );
        let mut d = Dec(&e.0);
        assert_eq!(d.u64(), 0x0102_0304_0506_0708);
        assert_eq!(d.str(6), "ab");
        assert_eq!(d.u64(), 7);
    }

    /// 足りなければ 0 / 空文字列で、**その後も 0 のまま** (古い短いレコードの末尾の欄の約束)。
    /// 長さの 1 バイトが幅を越えていても幅の中だけを読む (壊れたレコードでも溢れない)。
    #[test]
    fn a_short_payload_reads_as_zero_and_empty_from_then_on() {
        let bytes = [1u8, 0, 0, 0, 0, 0, 0, 0, 9, 9, 9];
        let mut d = Dec(&bytes);
        assert_eq!(d.u64(), 1);
        assert_eq!(d.u64(), 0, "3 B しか残っていない");
        assert_eq!(d.u64(), 0);
        assert_eq!(d.str(4), "");
        let mut short = Dec(&[200u8, b'x', b'y', b'z']);
        assert_eq!(short.str(4), "xyz", "長さは幅 − 1 で頭打ち");
        assert_eq!(short.u64(), 0);
    }
}
