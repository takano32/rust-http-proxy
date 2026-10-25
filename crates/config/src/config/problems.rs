//! 設定を読んだときに見つけた**読めない値**と**知らない名前** (T.28.6)。
//!
//! `Config::from_env` が `parse().ok()` で黙って既定に落としていた所を、ここの
//! [`EnvProblems::num`] などに寄せた (読めなければ WARN を 1 行出して既定のまま)。

use crate::envfile;

/// 書いてあったが読めなかった値 1 つ (T.28.6)。効いているのは既定値。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidVar {
    /// 環境変数名 (`PROXY_*` / `SERVER_*`)
    pub key: &'static str,
    /// 書いてあった値 (前後の空白を落とし、[`INVALID_VALUE_CAP`] バイトで切ったもの)
    pub value: String,
    /// 読める書き方 (英語の短い句。WARN と `--check` に出す)
    pub expected: &'static str,
}

/// `.env` に書いてあるが、このプロキシが読まない名前 1 つ (T.28.6)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownVar {
    pub key: String,
    /// 綴りの近い (編集距離 [`SUGGEST_DISTANCE`] 以内の) 知っている名前
    pub suggest: Option<&'static str>,
}

/// 覚えておく値の長さの上限 (`/status` が長い値で膨らまないように)。
pub const INVALID_VALUE_CAP: usize = 64;
/// 「打ち間違いかも」と候補を添える編集距離の上限。
pub const SUGGEST_DISTANCE: usize = 2;

/// 設定を読んだときに見つけた**読めない値**と**知らない名前** (T.28.6)。
///
/// どちらも WARN を 1 行ずつ出したうえでここに残し、`--check` と `/status` の
/// `settings.invalid` / `settings.unknown` (`/config` の `reload` も同じもの) に出す。
/// それまでは `parse().ok()` で黙って既定に落ちていて、`.env` に打ち間違いを書いても
/// 誰も気づけなかった。**正しい `.env` では空のまま** (WARN も 1 行も増えない)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvProblems {
    pub invalid: Vec<InvalidVar>,
    pub unknown: Vec<UnknownVar>,
}

impl EnvProblems {
    /// 前後の空白を落として空でない値 (空は「書いていない」と同じに扱う。パネルは空の変数も渡す)。
    pub(super) fn present(key: &str) -> Option<String> {
        envfile::var(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// 数として読めればその値、読めなければ WARN を 1 行出して `None` (= 既定のまま)。
    pub(super) fn num<T: std::str::FromStr>(&mut self, key: &'static str) -> Option<T> {
        let v = Self::present(key)?;
        match v.parse::<T>() {
            Ok(n) => Some(n),
            Err(_) => {
                self.reject(key, &v, "a whole number");
                None
            }
        }
    }

    /// 読めなかった値を WARN 1 行と一緒に覚える。
    pub(super) fn reject(&mut self, key: &'static str, raw: &str, expected: &'static str) {
        crate::log_warn!(
            None,
            "{}={:?} is not {}; using the default",
            key,
            raw.trim(),
            expected
        );
        self.note(key, raw, expected);
    }

    /// 覚えるだけ (WARN は呼び出し側が別の文面で出しているもの)。
    pub(super) fn note(&mut self, key: &'static str, raw: &str, expected: &'static str) {
        let raw = raw.trim();
        let mut cut = raw.len().min(INVALID_VALUE_CAP);
        while !raw.is_char_boundary(cut) {
            cut -= 1;
        }
        self.invalid.push(InvalidVar {
            key,
            value: raw[..cut].to_string(),
            expected,
        });
    }

    /// `.env` の名前のうち `PROXY_*` / `SERVER_*` で `known` に無いものを WARN 1 行ずつで覚える。
    ///
    /// 見るのは `.env` だけ (実際の環境変数はパネルやベンチの道具が `SERVER_IP` や
    /// `PROXY_CPUS` のような別の名前を渡すので、見ると正しい設定でも WARN が出る)。
    pub(super) fn find_unknown(&mut self, known: &[&'static str]) {
        for key in envfile::file_keys() {
            if !(key.starts_with("PROXY_") || key.starts_with("SERVER_"))
                || known.contains(&key.as_str())
            {
                continue;
            }
            let suggest = suggest(&key, known);
            match suggest {
                Some(s) => crate::log_warn!(
                    None,
                    "unknown setting {} in .env is ignored (did you mean {}?)",
                    key,
                    s
                ),
                None => crate::log_warn!(None, "unknown setting {} in .env is ignored", key),
            }
            self.unknown.push(UnknownVar { key, suggest });
        }
    }

    pub fn is_empty(&self) -> bool {
        self.invalid.is_empty() && self.unknown.is_empty()
    }

    /// `settings.invalid` の JSON (`[{"key":…,"value":…}]`)。
    pub fn invalid_json(&self) -> String {
        let items: Vec<String> = self
            .invalid
            .iter()
            .map(|v| {
                format!(
                    "{{\"key\":\"{}\",\"value\":{}}}",
                    v.key,
                    crate::json::quote(&v.value)
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    /// `settings.unknown` の JSON (`[{"key":…,"suggest":…|null}]`)。
    pub fn unknown_json(&self) -> String {
        let items: Vec<String> = self
            .unknown
            .iter()
            .map(|u| {
                format!(
                    "{{\"key\":{},\"suggest\":{}}}",
                    crate::json::quote(&u.key),
                    crate::json::quote_opt(u.suggest)
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    }
}

/// `key` にいちばん近い知っている名前 (編集距離 [`SUGGEST_DISTANCE`] 以内。同じ距離なら先に並ぶ方)。
fn suggest(key: &str, known: &[&'static str]) -> Option<&'static str> {
    known
        .iter()
        .map(|k| (edit_distance(key, k), *k))
        .filter(|(d, _)| *d <= SUGGEST_DISTANCE)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// 2 つの名前の編集距離 (挿入・削除・置換 1 回を 1 と数える。名前は ASCII なのでバイトで比べる)。
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    // 名前は前置きを外したもの (`check-docs.sh` の (b) がコードの `"PROXY_…"` を README の表と突き合わせるため)
    const KNOWN: &[&str] = &["TIMEOUT_SECS", "KEEPALIVE_SECS", "PORT"];

    #[test]
    fn edit_distance_counts_one_edit_as_one() {
        assert_eq!(edit_distance("TIMEOUT_SECS", "TIMEOUT_SECS"), 0);
        // 1 字抜け・1 字違い・1 字多い
        assert_eq!(edit_distance("TIMOUT_SECS", "TIMEOUT_SECS"), 1);
        assert_eq!(edit_distance("TIMEOUT_SECZ", "TIMEOUT_SECS"), 1);
        assert_eq!(edit_distance("TIMEOUTT_SECS", "TIMEOUT_SECS"), 1);
        assert_eq!(edit_distance("", "ABC"), 3);
    }

    #[test]
    fn suggests_only_close_names() {
        assert_eq!(suggest("TIMOUT_SECS", KNOWN), Some("TIMEOUT_SECS"));
        assert_eq!(suggest("KEEPALIVE_SEC", KNOWN), Some("KEEPALIVE_SECS"));
        assert_eq!(suggest("FOO_BAR", KNOWN), None);
    }

    #[test]
    fn invalid_values_are_cut_and_quoted() {
        let mut p = EnvProblems::default();
        let long = format!(" {} ", "x".repeat(100));
        p.note("PROXY_TIMEOUT_SECS", &long, "a whole number");
        p.note("PROXY_KEEPALIVE_SECS", "a\"b", "a whole number");
        assert_eq!(p.invalid[0].value.len(), INVALID_VALUE_CAP);
        let want = format!(
            "[{{\"key\":\"PROXY_TIMEOUT_SECS\",\"value\":\"{}\"}},{{\"key\":\"PROXY_KEEPALIVE_SECS\",\"value\":\"a\\\"b\"}}]",
            "x".repeat(INVALID_VALUE_CAP)
        );
        assert_eq!(p.invalid_json(), want);
        p.unknown.push(UnknownVar {
            key: "FOO_NAME".to_string(),
            suggest: None,
        });
        assert_eq!(
            p.unknown_json(),
            "[{\"key\":\"FOO_NAME\",\"suggest\":null}]"
        );
        assert!(!p.is_empty());
        assert!(EnvProblems::default().is_empty());
        assert_eq!(EnvProblems::default().invalid_json(), "[]");
    }
}
