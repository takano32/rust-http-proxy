//! ASCII だけを見る文字列の切り分け。
//!
//! HTTP のヘッダー行は ASCII なので、`str::split_once(char)` (`CharSearcher` を組んでから
//! `memchr` を呼ぶ) や `str::trim` (Unicode の空白判定 = 1 文字ずつ復号する) を通す必要がない。
//! 20〜40 バイトの行を要求ごとにヘッダーの本数だけなめる経路では、この初期化のぶんが効いてくる。
//! 空白の除去は標準の [`str::trim_ascii`] で足りるので、ここには分割だけ置く。

/// 最初に現れた `byte` の位置で 1 回だけ割る ([`str::split_once`] の ASCII 版)。
///
/// `byte` は ASCII (0x00〜0x7F) であること。UTF-8 では ASCII バイトが多バイト文字の
/// 途中に現れないので、見つかった位置は必ず文字境界になる。
#[inline]
pub fn split_once(s: &str, byte: u8) -> Option<(&str, &str)> {
    debug_assert!(byte.is_ascii(), "ASCII 以外だと文字境界で割れない");
    let i = s.as_bytes().iter().position(|b| *b == byte)?;
    Some((&s[..i], &s[i + 1..]))
}

/// 最後に現れた `byte` の位置で 1 回だけ割る ([`str::rsplit_once`] の ASCII 版)。
#[inline]
pub fn rsplit_once(s: &str, byte: u8) -> Option<(&str, &str)> {
    debug_assert!(byte.is_ascii(), "ASCII 以外だと文字境界で割れない");
    let i = s.as_bytes().iter().rposition(|b| *b == byte)?;
    Some((&s[..i], &s[i + 1..]))
}

/// `byte` が現れる回数。
#[inline]
pub fn count(s: &str, byte: u8) -> usize {
    s.as_bytes().iter().filter(|b| **b == byte).count()
}

/// token に使える字 (RFC 9110 §5.6.2 の tchar) の表。ヘッダー 1 行ごとに名前の全部の字を見るので、
/// 1 字 1 回の表引きにしてある (T.29.8)。
const TCHAR: [bool; 256] = {
    let mut t = [false; 256];
    let mut i = 0;
    while i < 256 {
        let b = i as u8;
        t[i] = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            );
        i += 1;
    }
    t
};

/// 空でない token か (ヘッダーの名前・メソッド。T.29.8。T.32.15 で `proxy-origin` から移した)。
#[inline]
pub fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| TCHAR[b as usize])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 標準の `split_once` / `rsplit_once` と同じ結果になること。
    #[test]
    fn matches_the_standard_library() {
        for s in [
            "a:b",
            ":b",
            "a:",
            "a",
            "",
            "Host: example.com:8080",
            "日本語: あり:なし",
            "::1",
        ] {
            assert_eq!(split_once(s, b':'), s.split_once(':'), "{:?}", s);
            assert_eq!(rsplit_once(s, b':'), s.rsplit_once(':'), "{:?}", s);
            assert_eq!(count(s, b':'), s.matches(':').count(), "{:?}", s);
        }
    }
}
