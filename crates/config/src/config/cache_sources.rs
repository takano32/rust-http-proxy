//! キャッシュの設定 (`PROXY_CACHE_*` / `SERVER_MEMORY` / `SERVER_DISK`) の**出どころの印**。
//! 値を読むのは [`crate::cache::CacheConfig::from_env`] (`crates/cachecfg`) で、ここは
//! 「書いてあって読めた」キーにだけ印を付け、読めない値には WARN と `invalid` を残す。

use super::{EnvProblems, Sources};
use crate::cache::config::DEFAULT_TARGET_PERCENT;
use crate::cache::{DiskQuota, Limit};
use crate::envfile;

/// キャッシュの設定は [`crate::cache::CacheConfig::from_env`] が読むので、ここでは**書いてあって読めた**
/// キーにだけ印を付ける (読み方は `crates/cachecfg` と同じ: 前後の空白を落として空でないこと、
/// 数値のものは `u64` として読めること、範囲のあるものはその中であること)。
pub(super) fn mark_cache_sources(src: &mut Sources, bad: &mut EnvProblems) {
    let raw = |k: &str| {
        envfile::var(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let num = |k: &str| raw(k).and_then(|v| v.parse::<u64>().ok());
    // 書いてあれば必ず効くもの (真偽・文字列・パス)
    for key in [
        "PROXY_CACHE_ENABLED",
        "PROXY_CACHE_RESERVE",
        "PROXY_CACHE_DIR",
        "PROXY_DISK_QUOTA_ROOT",
        "PROXY_DISK_PROBE",
        "PROXY_CACHE_ADMISSION",
    ] {
        if raw(key).is_some() {
            src.mark(key);
        }
    }
    // 読めたら印、書いてあって読めなければ WARN と `invalid` (T.28.6)
    let mut check = |key: &'static str, ok: bool, expected: &'static str| match raw(key) {
        Some(_) if ok => src.mark(key),
        Some(v) => bad.reject(key, &v, expected),
        None => {}
    };
    // 数値として読めたときだけ効くもの
    for key in [
        "PROXY_CACHE_PROBE_SECS",
        "PROXY_MEM_KEEP_FREE_MB",
        "PROXY_DISK_KEEP_FREE_MB",
        "PROXY_CACHE_TTL_SECS",
        "PROXY_CACHE_HEURISTIC_MAX_SECS",
        "PROXY_CACHE_MAX_STALE_SECS",
        "PROXY_CACHE_GRACE_SECS",
        "PROXY_STALE_WAIT_SECS",
        "PROXY_CACHE_MAX_OBJECT_MB",
        "PROXY_MEM_CACHE_MAX_OBJECT_MB",
        "PROXY_NEGATIVE_TTL_SECS",
    ] {
        check(key, num(key).is_some(), "a whole number");
    }
    // 範囲のあるもの
    for key in ["PROXY_MEM_TARGET_PERCENT", "PROXY_DISK_TARGET_PERCENT"] {
        check(
            key,
            num(key).is_some_and(|p| (1..=100).contains(&p)),
            "a number in 1..=100",
        );
    }
    check(
        "PROXY_CACHE_HEURISTIC_PERCENT",
        num("PROXY_CACHE_HEURISTIC_PERCENT").is_some_and(|p| p <= 100),
        "a number in 0..=100",
    );
    check(
        "PROXY_DISK_MAX_ENTRIES",
        num("PROXY_DISK_MAX_ENTRIES").is_some_and(|v| v > 0),
        "a whole number above 0",
    );
    // 上限は `auto` / `auto:85` / `85%` / MiB 数
    for key in ["PROXY_MEM_CACHE_MB", "PROXY_DISK_CACHE_MB"] {
        let ok = raw(key).is_some_and(|v| Limit::parse(&v, DEFAULT_TARGET_PERCENT).is_some());
        check(key, ok, "auto, auto:<percent>, <percent>% or MiB");
    }
    // ディスク割当は `PROXY_DISK_QUOTA_MB` が先で、**空でなければ**別名の `SERVER_DISK` は見ない
    let quota_key = if raw("PROXY_DISK_QUOTA_MB").is_some() {
        "PROXY_DISK_QUOTA_MB"
    } else {
        "SERVER_DISK"
    };
    let ok = raw(quota_key).is_some_and(|v| DiskQuota::parse(&v).is_some());
    check(quota_key, ok, "auto or MiB");
    // `SERVER_MEMORY=0` はパネルの「無制限」なので読めない値ではない (印は付けない)
    match num("SERVER_MEMORY") {
        Some(v) if v > 0 => src.mark("SERVER_MEMORY"),
        Some(_) => {}
        None => {
            if let Some(v) = raw("SERVER_MEMORY") {
                bad.reject("SERVER_MEMORY", &v, "a whole number of MiB");
            }
        }
    }
}
