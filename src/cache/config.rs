//! キャッシュ設定: 上限の指定方法 (固定 / 自動) と環境変数の解釈。

use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MIB: u64 = 1024 * 1024;

/// 自動モードの既定の使用率キャップ (%)。100 = 割合では抑えず、動的マージンだけで決める。
pub const DEFAULT_TARGET_PERCENT: u8 = 100;
/// 自動モードが使えない環境 (Linux 以外など) で使う固定上限。
pub const FALLBACK_MEM: u64 = 200 * MIB;
pub const FALLBACK_DISK: u64 = 2048 * MIB;

/// 各層の上限の指定方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// 動的な安全マージンだけ残して自動で確保する。`percent` はその上に掛ける
    /// 使用率のキャップ (100 = キャップ無し)。
    Auto { percent: u8 },
    /// 固定上限 (バイト)。
    Fixed(u64),
}

impl Limit {
    /// `auto` / `auto:85` / `85%` / `2048` (MiB) を解釈する。
    pub fn parse(s: &str, default_percent: u8) -> Option<Limit> {
        let s = s.trim().to_ascii_lowercase();
        if s == "auto" {
            return Some(Limit::Auto {
                percent: default_percent,
            });
        }
        let pct = s
            .strip_prefix("auto:")
            .or_else(|| s.strip_suffix('%'))
            .map(|p| {
                p.trim()
                    .parse::<u8>()
                    .ok()
                    .filter(|p| (1..=100).contains(p))
            });
        match pct {
            Some(Some(percent)) => Some(Limit::Auto { percent }),
            Some(None) => None,
            None => s
                .parse::<u64>()
                .ok()
                .map(|mb| Limit::Fixed(mb.saturating_mul(MIB))),
        }
    }

    pub fn is_auto(&self) -> bool {
        matches!(self, Limit::Auto { .. })
    }

    pub fn target_percent(&self) -> Option<u8> {
        match self {
            Limit::Auto { percent } => Some(*percent),
            Limit::Fixed(_) => None,
        }
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Limit::Auto { percent: 100 } => write!(f, "auto (dynamic margin)"),
            Limit::Auto { percent } => write!(f, "auto (dynamic margin, cap {}%)", percent),
            Limit::Fixed(bytes) => write!(f, "{} MiB", bytes / MIB),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub enabled: bool,
    pub mem_limit: Limit,
    pub disk_limit: Limit,
    /// 予算の未使用分を先行確保する (メモリはページを実際に確保、ディスクは fallocate)。
    pub reserve: bool,
    /// システム使用量を測り直して予算を更新する間隔。0 なら起動時の 1 回だけ。
    pub probe_interval: Duration,
    /// ディスクキャッシュ格納ディレクトリ
    pub dir: PathBuf,
    /// 手動で指定する最低限の空きマージン (バイト)。0 なら動的マージンのみ
    pub mem_keep_free: u64,
    pub disk_keep_free: u64,
    /// Cache-Control も Last-Modified も無い場合の既定 TTL
    pub default_ttl: Duration,
    /// Last-Modified からの経験則: 経過時間のこの割合 (%) を TTL にする
    pub heuristic_percent: u8,
    /// 経験則 TTL の上限
    pub heuristic_max: Duration,
    /// 期限切れでも再検証できるエントリを保持しておく最長時間 (期限からの経過)
    pub max_stale: Duration,
    /// ディスク層に置く 1 オブジェクトの最大サイズ (バイト)。本文はストリーミングで書く
    pub max_object_size: u64,
    /// メモリ層に置く 1 オブジェクトの最大サイズ (バイト)
    pub mem_max_object_size: u64,
    /// コンテナのディスク割当 (バイト)。Pterodactyl のように「ディレクトリの合計サイズ」で
    /// 制限される環境向けで、指定すると `statvfs` の代わりにこの値を分母にする。
    pub disk_quota: Option<u64>,
    /// `PROXY_DISK_QUOTA_MB` (別名 `SERVER_DISK`) が明示されたか (`0` = 無制限の明示も含む)。
    pub disk_quota_set: bool,
    /// `disk_quota` が適用されるディレクトリ (通常はコンテナのボリューム = `$HOME`)。
    pub quota_root: Option<PathBuf>,
    /// コンテナのメモリ割当 (バイト)。Pterodactyl が渡す `SERVER_MEMORY` (MB)。
    pub mem_alloc: Option<u64>,
    /// Pterodactyl (Wings) 配下で動いているか (`P_SERVER_UUID` の有無)。
    pub pterodactyl: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mem_limit: Limit::Auto {
                percent: DEFAULT_TARGET_PERCENT,
            },
            disk_limit: Limit::Auto {
                percent: DEFAULT_TARGET_PERCENT,
            },
            reserve: true,
            probe_interval: Duration::from_secs(1),
            dir: env::temp_dir().join("sorahost-http-proxy-cache"),
            mem_keep_free: 0,
            disk_keep_free: 0,
            default_ttl: Duration::from_secs(300),
            heuristic_percent: 10,
            heuristic_max: Duration::from_secs(7 * 24 * 3600),
            max_stale: Duration::from_secs(30 * 24 * 3600),
            max_object_size: 4096 * MIB,
            mem_max_object_size: 32 * MIB,
            disk_quota: None,
            disk_quota_set: false,
            quota_root: None,
            mem_alloc: None,
            pterodactyl: false,
        }
    }
}

impl CacheConfig {
    pub fn from_env() -> Self {
        Self::from_lookup(|k| env::var(k).ok())
    }

    /// 環境変数相当の値を `lookup` から読んで設定を組み立てる (テストしやすいよう分離)。
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let d = Self::default();
        let get = |k: &str| {
            lookup(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let num = |k: &str| get(k).and_then(|v| v.parse::<u64>().ok());
        let flag = |k: &str, default: bool| {
            get(k)
                .map(|v| {
                    !matches!(
                        v.to_ascii_lowercase().as_str(),
                        "0" | "false" | "off" | "no"
                    )
                })
                .unwrap_or(default)
        };
        let percent = |k: &str| {
            num(k)
                .filter(|p| (1..=100).contains(p))
                .map(|p| p as u8)
                .unwrap_or(DEFAULT_TARGET_PERCENT)
        };
        let limit = |k: &str, pct_key: &str| {
            let pct = percent(pct_key);
            get(k)
                .and_then(|v| Limit::parse(&v, pct))
                .unwrap_or(Limit::Auto { percent: pct })
        };

        let pterodactyl = get("P_SERVER_UUID").is_some();
        let home = get("HOME").map(PathBuf::from);
        let dir = get("PROXY_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| resolve_default_dir(pterodactyl));

        Self {
            enabled: flag("PROXY_CACHE_ENABLED", true),
            mem_limit: limit("PROXY_MEM_CACHE_MB", "PROXY_MEM_TARGET_PERCENT"),
            disk_limit: limit("PROXY_DISK_CACHE_MB", "PROXY_DISK_TARGET_PERCENT"),
            reserve: flag("PROXY_CACHE_RESERVE", true),
            probe_interval: num("PROXY_CACHE_PROBE_SECS")
                .map(Duration::from_secs)
                .unwrap_or(d.probe_interval),
            dir,
            mem_keep_free: num("PROXY_MEM_KEEP_FREE_MB")
                .map(|v| v.saturating_mul(MIB))
                .unwrap_or(d.mem_keep_free),
            disk_keep_free: num("PROXY_DISK_KEEP_FREE_MB")
                .map(|v| v.saturating_mul(MIB))
                .unwrap_or(d.disk_keep_free),
            default_ttl: num("PROXY_CACHE_TTL_SECS")
                .map(Duration::from_secs)
                .unwrap_or(d.default_ttl),
            heuristic_percent: num("PROXY_CACHE_HEURISTIC_PERCENT")
                .filter(|p| (0..=100).contains(p))
                .map(|p| p as u8)
                .unwrap_or(d.heuristic_percent),
            heuristic_max: num("PROXY_CACHE_HEURISTIC_MAX_SECS")
                .map(Duration::from_secs)
                .unwrap_or(d.heuristic_max),
            max_stale: num("PROXY_CACHE_MAX_STALE_SECS")
                .map(Duration::from_secs)
                .unwrap_or(d.max_stale),
            max_object_size: num("PROXY_CACHE_MAX_OBJECT_MB")
                .map(|v| v.saturating_mul(MIB))
                .unwrap_or(d.max_object_size),
            mem_max_object_size: num("PROXY_MEM_CACHE_MAX_OBJECT_MB")
                .map(|v| v.saturating_mul(MIB))
                .unwrap_or(d.mem_max_object_size),
            disk_quota: num("PROXY_DISK_QUOTA_MB")
                .or_else(|| num("SERVER_DISK"))
                .filter(|&v| v > 0)
                .map(|v| v.saturating_mul(MIB)),
            disk_quota_set: num("PROXY_DISK_QUOTA_MB")
                .or_else(|| num("SERVER_DISK"))
                .is_some(),
            quota_root: get("PROXY_DISK_QUOTA_ROOT").map(PathBuf::from).or(home),
            mem_alloc: num("SERVER_MEMORY")
                .filter(|&v| v > 0)
                .map(|v| v.saturating_mul(MIB)),
            pterodactyl,
        }
    }

    /// テスト用: キャッシュ無効設定。
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// テスト用: 固定上限・先行確保なし・バックグラウンド更新なし。
    pub fn fixed(mem_bytes: u64, disk_bytes: u64, dir: PathBuf) -> Self {
        Self {
            enabled: true,
            mem_limit: Limit::Fixed(mem_bytes),
            disk_limit: Limit::Fixed(disk_bytes),
            reserve: false,
            probe_interval: Duration::ZERO,
            dir,
            ..Self::default()
        }
    }
}

/// 既定のキャッシュディレクトリ。書き込める最初の候補を選ぶ。
///
/// 1. `/var/cache/sorahost-http-proxy` (root で動かす場合)
/// 2. `$XDG_CACHE_HOME/sorahost-http-proxy` または `~/.cache/sorahost-http-proxy`
/// 3. `$TMPDIR/sorahost-http-proxy-cache` (tmpfs のことが多いので最後の手段)
///
/// Pterodactyl ではコンテナが起動ごとに作り直され `/home/container` だけが残るので、
/// 2 を最優先にする。
pub fn resolve_default_dir(pterodactyl: bool) -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(xdg) = env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(xdg).join("sorahost-http-proxy"));
    } else if let Some(home) = env::var_os("HOME").filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(home).join(".cache/sorahost-http-proxy"));
    }
    if cfg!(unix) {
        let var_cache = PathBuf::from("/var/cache/sorahost-http-proxy");
        if pterodactyl {
            candidates.push(var_cache);
        } else {
            candidates.insert(0, var_cache);
        }
    }
    candidates
        .into_iter()
        .find(|c| is_writable_dir(c.as_path()))
        .unwrap_or_else(|| CacheConfig::default().dir)
}

fn is_writable_dir(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".write-test");
    let ok = fs::write(&probe, b"").is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn parses_limit_forms() {
        assert_eq!(
            Limit::parse("auto", 100),
            Some(Limit::Auto { percent: 100 })
        );
        assert_eq!(Limit::parse("auto", 90), Some(Limit::Auto { percent: 90 }));
        assert_eq!(
            Limit::parse(" AUTO:75 ", 90),
            Some(Limit::Auto { percent: 75 })
        );
        assert_eq!(Limit::parse("80%", 90), Some(Limit::Auto { percent: 80 }));
        assert_eq!(Limit::parse("200", 90), Some(Limit::Fixed(200 * MIB)));
        assert_eq!(Limit::parse("auto:0", 90), None);
        assert_eq!(Limit::parse("auto:101", 90), None);
        assert_eq!(Limit::parse("lots", 90), None);
        assert_eq!(Limit::Fixed(3 * MIB).to_string(), "3 MiB");
        assert!(Limit::Auto { percent: 90 }.to_string().contains("90%"));
        assert!(Limit::Auto { percent: 100 }.to_string().contains("dynamic"));
    }

    #[test]
    fn defaults_are_auto_with_reserve() {
        let d = CacheConfig::default();
        assert_eq!(d.mem_limit, Limit::Auto { percent: 100 });
        assert_eq!(d.disk_limit, Limit::Auto { percent: 100 });
        assert!(d.reserve && d.enabled);
        assert_eq!(d.probe_interval, Duration::from_secs(1));
        assert_eq!(d.mem_keep_free, 0);
    }

    #[test]
    fn env_lookup_overrides() {
        let vars: HashMap<&str, &str> = [
            ("PROXY_MEM_CACHE_MB", "256"),
            ("PROXY_DISK_CACHE_MB", "auto"),
            ("PROXY_DISK_TARGET_PERCENT", "70"),
            ("PROXY_CACHE_RESERVE", "off"),
            ("PROXY_CACHE_PROBE_SECS", "10"),
            ("PROXY_CACHE_DIR", "/tmp/shp-cfg-test"),
            ("PROXY_CACHE_TTL_SECS", "5"),
            ("PROXY_CACHE_MAX_OBJECT_MB", "1"),
            ("PROXY_CACHE_ENABLED", "yes"),
            ("PROXY_DISK_QUOTA_MB", "10240"),
            ("SERVER_MEMORY", "1024"),
            ("P_SERVER_UUID", "abc"),
            ("HOME", "/home/container"),
        ]
        .into_iter()
        .collect();
        let cfg = CacheConfig::from_lookup(|k| vars.get(k).map(|v| v.to_string()));
        assert_eq!(cfg.mem_limit, Limit::Fixed(256 * MIB));
        assert_eq!(cfg.disk_limit, Limit::Auto { percent: 70 });
        assert!(!cfg.reserve && cfg.enabled);
        assert_eq!(cfg.probe_interval, Duration::from_secs(10));
        assert_eq!(cfg.dir, PathBuf::from("/tmp/shp-cfg-test"));
        assert_eq!(cfg.default_ttl, Duration::from_secs(5));
        assert_eq!(cfg.max_object_size, MIB);
        assert_eq!(cfg.mem_max_object_size, 32 * MIB);
        assert_eq!(cfg.mem_keep_free, 0);
        assert_eq!(cfg.heuristic_percent, 10);
        assert_eq!(cfg.disk_quota, Some(10240 * MIB));
        assert!(cfg.disk_quota_set);
        assert_eq!(cfg.quota_root, Some(PathBuf::from("/home/container")));
        assert_eq!(cfg.mem_alloc, Some(1024 * MIB));
        assert!(cfg.pterodactyl);

        let bad: HashMap<&str, &str> = [
            ("PROXY_MEM_CACHE_MB", "garbage"),
            ("PROXY_MEM_TARGET_PERCENT", "0"),
            ("PROXY_CACHE_DIR", "/tmp/shp-cfg-test"),
            ("PROXY_DISK_QUOTA_MB", "0"),
            ("PROXY_DISK_KEEP_FREE_MB", "512"),
        ]
        .into_iter()
        .collect();
        let cfg = CacheConfig::from_lookup(|k| bad.get(k).map(|v| v.to_string()));
        assert_eq!(cfg.mem_limit, Limit::Auto { percent: 100 });
        assert_eq!(cfg.disk_quota, None);
        assert!(cfg.disk_quota_set, "0 は「無制限」の明示");
        assert_eq!(cfg.disk_keep_free, 512 * MIB);
        assert_eq!(cfg.mem_alloc, None);
        assert!(!cfg.pterodactyl);
    }
}
