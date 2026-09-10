//! 起動ごとに変わる印を付けた `Via` の値。転送する要求に足し、受けた要求に自分の印が
//! あればループなので `508 Loop Detected` で閉じる (T12.3)。
//!
//! 印を**起動ごとの乱数**にしてあるのは、rust-http-proxy を 2 段に並べた正当な構成
//! (別プロセス = 別の印) を誤検出しないため。同じプロセスの別ポートは同じ印になるので、
//! 「自分の別の待ち受けへ転送した」形のループは 1 段で止まる。
//!
//! 乱数は外部クレートを足さずに作る (`RandomState` はプロセスごとに無作為な種を持つ)。
//! 行は**起動時に 1 回だけ**組み立て、熱い経路 (`headers::write_request_headers`) は
//! `&'static str` をそのまま書く (要求ごとに `String` を作らない)。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// `1.1 rust-http-proxy/xxxxxxxx` (`Via` の値そのもの)。
static TOKEN: OnceLock<String> = OnceLock::new();
/// `Via: 1.1 rust-http-proxy/xxxxxxxx\r\n` (転送する要求にそのまま書く 1 行)。
static LINE: OnceLock<String> = OnceLock::new();

/// 8 桁 16 進の印を作る。`RandomState` の種 + 時刻 + スタックのアドレスを混ぜる。
fn mark() -> String {
    let anchor = 0u8;
    let mut h = RandomState::new().build_hasher();
    h.write_usize(&anchor as *const u8 as usize);
    h.write_u64(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
            .unwrap_or(0),
    );
    h.write_u32(std::process::id());
    let v = h.finish();
    format!("{:08x}", (v ^ (v >> 32)) as u32)
}

/// この起動の `Via` の値 (`1.1 rust-http-proxy/xxxxxxxx`)。
pub fn token() -> &'static str {
    TOKEN.get_or_init(|| format!("1.1 rust-http-proxy/{}", mark()))
}

/// 転送する要求に足す 1 行 (CRLF 込み)。熱い経路はこれをそのまま書く。
pub fn line() -> &'static str {
    LINE.get_or_init(|| format!("Via: {}\r\n", token()))
}

/// `Via` の値に自分の印があるか (ループの検出)。値は `,` 区切りで複数の中継が並ぶ。
#[inline]
pub fn is_self(value: &str) -> bool {
    value.contains(token())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_is_eight_hex_and_stable_within_a_process() {
        let t = token();
        let hex = t.rsplit('/').next().unwrap();
        assert_eq!(hex.len(), 8, "{}", t);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "{}", t);
        assert_eq!(t, token(), "起動中は変わらない");
        assert_eq!(line(), format!("Via: {}\r\n", t));
    }

    #[test]
    fn detects_only_our_own_mark() {
        assert!(is_self(token()));
        assert!(is_self(&format!("1.1 other-proxy, {}", token())));
        // 別プロセス (別の印) は誤検出しない
        assert!(!is_self("1.1 rust-http-proxy/00000000"));
        assert!(!is_self("1.1 rust-http-proxy"));
        assert!(!is_self(""));
    }

    #[test]
    fn marks_differ_between_boots() {
        // 同じプロセスでも作り直せば違う値になる (種が毎回変わる)
        let a = mark();
        let b = mark();
        assert_ne!(a, b);
    }
}
