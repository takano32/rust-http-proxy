//! 環境変数の小さな読み手 ([`Env`])。`CacheConfig::from_lookup` の規則を 1 つずつ持つ
//! (T34.85 で `from_lookup` の 5 つの閉包から出した)。
//!
//! 読み元は `lookup` で受ける (ふだんは `envfile::var`、試験では表)。どの読みも
//! **前後の空白を落とし、空なら書いていないのと同じ**に扱う。

use std::time::Duration;

use super::{DEFAULT_TARGET_PERCENT, Limit, MIB};

/// `lookup` を包んで、値の読み方 (数・旗・割合・秒・MiB・上限) を揃える。
pub(super) struct Env<F> {
    lookup: F,
}

impl<F: Fn(&str) -> Option<String>> Env<F> {
    pub(super) fn new(lookup: F) -> Self {
        Env { lookup }
    }

    /// 値 (前後の空白を落とす。空なら `None`)。
    pub(super) fn get(&self, k: &str) -> Option<String> {
        (self.lookup)(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// 符号なしの整数 (読めなければ `None`)。
    pub(super) fn num(&self, k: &str) -> Option<u64> {
        self.get(k).and_then(|v| v.parse::<u64>().ok())
    }

    /// 旗。`0` / `false` / `off` / `no` (大文字小文字を問わない) だけが偽で、ほかの値は
    /// 全部真。書いていなければ `default`。
    pub(super) fn flag(&self, k: &str, default: bool) -> bool {
        self.get(k)
            .map(|v| {
                !matches!(
                    v.to_ascii_lowercase().as_str(),
                    "0" | "false" | "off" | "no"
                )
            })
            .unwrap_or(default)
    }

    /// 0 より大きい整数 (0 と読めない値は `None` = 書いていないのと同じ)。
    pub(super) fn positive(&self, k: &str) -> Option<u64> {
        self.num(k).filter(|&v| v > 0)
    }

    /// 目標の割合 (1〜100)。範囲外・読めない・書いていないは [`DEFAULT_TARGET_PERCENT`]。
    pub(super) fn percent(&self, k: &str) -> u8 {
        self.percent_from(k, 1, DEFAULT_TARGET_PERCENT)
    }

    /// `min`〜100 の割合 (範囲外・読めない・書いていないは `default`)。
    /// 発見的な鮮度の割合は 0 も通すので、下の端を受ける。
    pub(super) fn percent_from(&self, k: &str, min: u64, default: u8) -> u8 {
        self.num(k)
            .filter(|p| (min..=100).contains(p))
            .map(|p| p as u8)
            .unwrap_or(default)
    }

    /// 秒 (読めなければ `default`)。
    pub(super) fn secs(&self, k: &str, default: Duration) -> Duration {
        self.num(k).map(Duration::from_secs).unwrap_or(default)
    }

    /// MiB で書いた量をバイトにする (読めなければ `default`。桁あふれは上で止める)。
    pub(super) fn mib(&self, k: &str, default: u64) -> u64 {
        self.num(k)
            .map(|v| v.saturating_mul(MIB))
            .unwrap_or(default)
    }

    /// 層の上限 (`k` は `auto` / MB 数など、`pct_key` はその割合)。読めなければ `auto`。
    pub(super) fn limit(&self, k: &str, pct_key: &str) -> Limit {
        let pct = self.percent(pct_key);
        self.get(k)
            .and_then(|v| Limit::parse(&v, pct))
            .unwrap_or(Limit::Auto { percent: pct })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 つの鍵 `K` に `v` を入れた読み手。
    fn one(v: &'static str) -> Env<impl Fn(&str) -> Option<String>> {
        Env::new(move |k: &str| (k == "K").then(|| v.to_string()))
    }

    /// 空白だけの値と空の値は、書いていないのと同じ (T34.85)。
    #[test]
    fn blank_values_are_unset() {
        assert_eq!(one("  x  ").get("K"), Some("x".to_string()));
        assert_eq!(one("   ").get("K"), None);
        assert_eq!(one("").get("K"), None);
        assert_eq!(one("x").get("OTHER"), None);
    }

    /// 旗の 8 形: 偽の綴り 4 つ (大文字でも) と、真になるそれ以外 (T34.85)。
    #[test]
    fn a_flag_is_false_only_for_four_spellings() {
        for v in ["0", "false", "off", "no", "OFF", " No "] {
            assert!(!one(v).flag("K", true), "{:?}", v);
        }
        for v in ["1", "true", "on", "yes", "enabled"] {
            assert!(one(v).flag("K", false), "{:?}", v);
        }
        // 書いていない・空は既定
        assert!(one("").flag("K", true));
        assert!(!one("").flag("K", false));
    }

    /// 割合は 1〜100 だけ通し、範囲外と読めない値は既定に倒す (T34.85)。
    #[test]
    fn a_percent_outside_one_to_hundred_falls_back() {
        assert_eq!(one("1").percent("K"), 1);
        assert_eq!(one("100").percent("K"), 100);
        assert_eq!(one("0").percent("K"), DEFAULT_TARGET_PERCENT);
        assert_eq!(one("101").percent("K"), DEFAULT_TARGET_PERCENT);
        assert_eq!(one("-5").percent("K"), DEFAULT_TARGET_PERCENT);
        assert_eq!(one("half").percent("K"), DEFAULT_TARGET_PERCENT);
        // 下の端を 0 にした割合 (発見的な鮮度) は 0 を通す
        assert_eq!(one("0").percent_from("K", 0, 10), 0);
        assert_eq!(one("101").percent_from("K", 0, 10), 10);
        // 0 より大きい数だけ
        assert_eq!(one("0").positive("K"), None);
        assert_eq!(one("5").positive("K"), Some(5));
    }

    /// 読めない数は書いていないのと同じで既定に倒す。MiB は桁あふれで止める (T34.85)。
    #[test]
    fn unreadable_numbers_fall_back_to_the_default() {
        let d = Duration::from_secs(7);
        assert_eq!(one("10").secs("K", d), Duration::from_secs(10));
        assert_eq!(one("ten").secs("K", d), d);
        assert_eq!(one("1.5").secs("K", d), d);
        assert_eq!(one("3").mib("K", 9), 3 * MIB);
        assert_eq!(one("x").mib("K", 9), 9);
        assert_eq!(one("18446744073709551615").mib("K", 9), u64::MAX);
        assert_eq!(one(" 42 ").num("K"), Some(42));
    }

    /// 上限は読めなければ `auto` (その割合で)、読めればその形 (T34.85)。
    #[test]
    fn a_limit_falls_back_to_auto_at_its_percent() {
        let env = Env::new(|k: &str| match k {
            "MB" => Some("garbage".to_string()),
            "PCT" => Some("70".to_string()),
            "FIXED" => Some("256".to_string()),
            _ => None,
        });
        assert_eq!(env.limit("MB", "PCT"), Limit::Auto { percent: 70 });
        assert_eq!(env.limit("FIXED", "PCT"), Limit::Fixed(256 * MIB));
        assert_eq!(
            env.limit("MISSING", "MISSING"),
            Limit::Auto {
                percent: DEFAULT_TARGET_PERCENT
            }
        );
    }
}
