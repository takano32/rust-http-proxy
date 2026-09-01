//! 環境変数の代わりにファイルから設定を読む。
//!
//! Pterodactyl では egg 変数を追加する権限が無いことがあるので、ボリューム内のファイル
//! (`$HOME/sorahost-http-proxy.env`、または `PROXY_ENV_FILE` で指定) に `KEY=VALUE` を
//! 書けば同じ効果になる。実際の環境変数が優先で、ファイルはその補完。

use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::OnceLock;

const DEFAULT_NAME: &str = "sorahost-http-proxy.env";

struct Loaded {
    path: Option<PathBuf>,
    vars: HashMap<String, String>,
}

static LOADED: OnceLock<Loaded> = OnceLock::new();

fn loaded() -> &'static Loaded {
    LOADED.get_or_init(|| {
        let path = env::var_os("PROXY_ENV_FILE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|h| PathBuf::from(h).join(DEFAULT_NAME))
            });
        match path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            Some(text) => Loaded {
                path,
                vars: parse(&text),
            },
            None => Loaded {
                path: None,
                vars: HashMap::new(),
            },
        }
    })
}

/// `KEY=VALUE` 行を読む。`#` 以降はコメント、`export ` 接頭辞と引用符は外す。
pub fn parse(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let valid_key = key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid_key {
            continue;
        }
        let mut value = v.trim();
        if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
            || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
        {
            value = &value[1..value.len() - 1];
        } else if let Some(pos) = value.find(" #") {
            value = value[..pos].trim_end();
        }
        out.insert(key.to_string(), value.to_string());
    }
    out
}

/// 実際の環境変数 → env ファイルの順に探す。
pub fn var(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .or_else(|| loaded().vars.get(key).cloned())
}

/// 読み込んだ env ファイルのパス (無ければ `None`)。
pub fn loaded_path() -> Option<&'static PathBuf> {
    loaded().path.as_ref()
}

/// env ファイルで与えられたキーの数 (ログ用)。
pub fn loaded_count() -> usize {
    loaded().vars.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_env_file_syntax() {
        let vars = parse(
            "# comment\nSERVER_DISK=51200\nexport PROXY_LOG_LEVEL = debug\nPROXY_CACHE_DIR=\"/home/container/cache\"\nQUOTED='a b'\nTRAIL=value # note\nbad line\n1BAD=x\n=empty\n",
        );
        assert_eq!(vars.get("SERVER_DISK").map(String::as_str), Some("51200"));
        assert_eq!(
            vars.get("PROXY_LOG_LEVEL").map(String::as_str),
            Some("debug")
        );
        assert_eq!(
            vars.get("PROXY_CACHE_DIR").map(String::as_str),
            Some("/home/container/cache")
        );
        assert_eq!(vars.get("QUOTED").map(String::as_str), Some("a b"));
        assert_eq!(vars.get("TRAIL").map(String::as_str), Some("value"));
        assert_eq!(vars.len(), 5);
    }

    #[test]
    fn real_environment_wins() {
        // PATH は必ず実環境にあるので、ファイル側の値には置き換わらない
        assert_eq!(var("PATH"), env::var("PATH").ok());
    }
}
