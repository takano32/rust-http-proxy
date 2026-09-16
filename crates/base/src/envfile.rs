//! 環境変数の代わりに `$HOME/.env` から設定を読む。
//!
//! Pterodactyl では egg 変数を追加する権限が無いことがあるので、ボリューム直下の `.env` に
//! `KEY=VALUE` を書けば同じ効果になる。ファイルの値が実際の環境変数より優先する (パネルが
//! 渡す値を手元で上書きできるように)。

use crate::sync::RwLockExt;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock, RwLockReadGuard};

struct Loaded {
    /// 実際に読めた `.env` のパス (無ければ `None`)
    path: Option<PathBuf>,
    vars: HashMap<String, String>,
}

static LOADED: OnceLock<RwLock<Loaded>> = OnceLock::new();
/// コマンドライン引数による上書き (`.env` よりも優先する)。
static OVERRIDES: OnceLock<HashMap<String, String>> = OnceLock::new();

/// コマンドライン引数から来た上書きを覚える (起動時に 1 回だけ)。
pub fn set_overrides(vars: Vec<(String, String)>) {
    let _ = OVERRIDES.set(vars.into_iter().collect());
}

/// `$HOME/.env` の場所。`HOME` が無ければ `None`。
pub fn env_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".env"))
}

fn read() -> Loaded {
    let path = env_path();
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
}

fn loaded() -> RwLockReadGuard<'static, Loaded> {
    LOADED.get_or_init(|| RwLock::new(read())).read_locked()
}

/// `.env` を読み直し、値が変わったキー (追加・削除・変更) を名前順で返す。
pub fn reload() -> Vec<String> {
    let fresh = read();
    let lock = LOADED.get_or_init(|| RwLock::new(read()));
    let mut cur = lock.write_locked();
    let mut changed: Vec<String> = cur
        .vars
        .keys()
        .chain(fresh.vars.keys())
        .filter(|k| cur.vars.get(*k) != fresh.vars.get(*k))
        .cloned()
        .collect();
    changed.sort();
    changed.dedup();
    *cur = fresh;
    changed
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

/// 値がどの層から来たか ([`var`] と同じ優先順)。`/config` の `source` の元になる (T14.15)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarSource {
    /// コマンドライン引数による上書き
    Cli,
    /// `$HOME/.env`
    File,
    /// 実際の環境変数
    Env,
}

/// `key` の値が**どの層から来るか**を [`var`] と同じ順で調べる (どこにも無ければ `None`)。
///
/// 値そのものではなく出どころだけを返すのは、`/config` が「既定のままか、`.env` に
/// 書いたのか、パネルが渡しているのか」を見せるため。
pub fn var_source(key: &str) -> Option<VarSource> {
    if OVERRIDES.get().is_some_and(|o| o.contains_key(key)) {
        return Some(VarSource::Cli);
    }
    if loaded().vars.contains_key(key) {
        return Some(VarSource::File);
    }
    env::var(key).is_ok().then_some(VarSource::Env)
}

/// コマンドライン引数 → `$HOME/.env` → 実際の環境変数の順に探す。
pub fn var(key: &str) -> Option<String> {
    if let Some(v) = OVERRIDES.get().and_then(|o| o.get(key)) {
        return Some(v.clone());
    }
    loaded()
        .vars
        .get(key)
        .cloned()
        .or_else(|| env::var(key).ok())
}

/// 読み込んだ `.env` のパス (無ければ `None`)。
pub fn loaded_path() -> Option<PathBuf> {
    loaded().path.clone()
}

/// `.env` で与えられたキーの数 (ログ用)。
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
    fn reports_where_a_value_comes_from() {
        // 実環境にあるもの (どのテストでも必ずある)
        assert_eq!(var_source("PATH"), Some(VarSource::Env));
        // どこにも無いもの
        assert_eq!(var_source("PROXY_NO_SUCH_KEY_T1415"), None);
    }

    #[test]
    fn real_environment_wins() {
        // PATH は必ず実環境にあるので、ファイル側の値には置き換わらない
        assert_eq!(var("PATH"), env::var("PATH").ok());
    }
}
