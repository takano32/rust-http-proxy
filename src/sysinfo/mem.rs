//! メモリ関連のシステム情報を `/proc` と cgroup から読む (std のみ)。

use std::fs;
use std::path::Path;

/// システム全体のメモリ量 (バイト)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfo {
    pub total: u64,
    /// カーネルの `MemAvailable` (回収可能なページキャッシュを含む見積もり)。
    pub available: u64,
}

/// cgroup によるメモリ制限 (バイト)。
///
/// `usage` は Docker / Pterodactyl (Wings) がパネルに表示するのと同じ
/// 「usage − inactive_file」で、回収可能な非アクティブなページキャッシュを除いた値。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CgroupMem {
    pub limit: u64,
    pub usage: u64,
    /// cgroup v2 の `memory.pressure` (あれば)。
    pub pressure: Option<MemPressure>,
}

/// `/proc/pressure/memory` の `avg10` (直近 10 秒でメモリ待ちが発生した時間の割合, %)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemPressure {
    pub some_avg10: f64,
    pub full_avg10: f64,
}

pub fn mem_info() -> Option<MemInfo> {
    parse_meminfo(&fs::read_to_string("/proc/meminfo").ok()?)
}

pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut total = None;
    let mut available = None;
    let mut free = None;
    let mut reclaimable = 0u64;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(bytes) = sized_field(rest) else {
            continue;
        };
        match key.trim() {
            "MemTotal" => total = Some(bytes),
            "MemAvailable" => available = Some(bytes),
            "MemFree" => free = Some(bytes),
            "Buffers" | "Cached" | "SReclaimable" => reclaimable += bytes,
            _ => {}
        }
    }
    let total = total?;
    // MemAvailable が無い古いカーネル向けの近似
    let available = available.or_else(|| free.map(|f| f + reclaimable))?;
    Some(MemInfo {
        total,
        available: available.min(total),
    })
}

/// `"   123456 kB"` / `"123456"` 形式の値をバイトに変換する。
fn sized_field(s: &str) -> Option<u64> {
    let mut it = s.split_whitespace();
    let n: u64 = it.next()?.parse().ok()?;
    Some(match it.next() {
        Some("kB") | Some("KB") | Some("kb") => n.saturating_mul(1024),
        _ => n,
    })
}

/// 自プロセスの常駐セットサイズ (バイト)。
pub fn process_rss() -> Option<u64> {
    parse_status_rss(&fs::read_to_string("/proc/self/status").ok()?)
}

pub fn parse_status_rss(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(sized_field)
}

pub fn mem_pressure() -> Option<MemPressure> {
    parse_pressure(&fs::read_to_string("/proc/pressure/memory").ok()?)
}

pub fn parse_pressure(text: &str) -> Option<MemPressure> {
    let mut some = None;
    let mut full = None;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(kind) = it.next() else {
            continue;
        };
        let avg10 = it
            .find_map(|f| f.strip_prefix("avg10="))
            .and_then(|v| v.parse::<f64>().ok());
        match kind {
            "some" => some = avg10,
            "full" => full = avg10,
            _ => {}
        }
    }
    Some(MemPressure {
        some_avg10: some?,
        full_avg10: full.unwrap_or(0.0),
    })
}

/// 自プロセスが属する cgroup (v2 / v1) を根まで辿り、メモリ制限が掛かっている階層をすべて返す。
/// コンテナ内で「ホストの 90%」を取りにいって OOM で殺されるのを防ぐための情報。
pub fn cgroup_mem_limits() -> Vec<CgroupMem> {
    match fs::read_to_string("/proc/self/cgroup") {
        Ok(text) => cgroup_limits_from(&text, Path::new("/sys/fs/cgroup")),
        Err(_) => Vec::new(),
    }
}

struct CgroupFiles {
    limit: &'static str,
    usage: &'static str,
    stat: &'static str,
    inactive_key: &'static str,
    pressure: Option<&'static str>,
}

const V2: CgroupFiles = CgroupFiles {
    limit: "memory.max",
    usage: "memory.current",
    stat: "memory.stat",
    inactive_key: "inactive_file",
    pressure: Some("memory.pressure"),
};

const V1: CgroupFiles = CgroupFiles {
    limit: "memory.limit_in_bytes",
    usage: "memory.usage_in_bytes",
    stat: "memory.stat",
    inactive_key: "total_inactive_file",
    pressure: None,
};

/// v1 は無制限だと巨大な値 (2^63 付近) を返すので、それ未満だけを制限とみなす。
const UNLIMITED: u64 = 1 << 62;

pub fn cgroup_limits_from(proc_cgroup: &str, sysfs: &Path) -> Vec<CgroupMem> {
    let mut out = Vec::new();
    for line in proc_cgroup.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(_id), Some(controllers), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if controllers.is_empty() {
            walk_limits(sysfs, path, &V2, &mut out);
        } else if controllers.split(',').any(|c| c == "memory") {
            walk_limits(&sysfs.join("memory"), path, &V1, &mut out);
        }
    }
    out
}

fn walk_limits(root: &Path, cg_path: &str, files: &CgroupFiles, out: &mut Vec<CgroupMem>) {
    let mut dir = root.join(cg_path.trim_start_matches('/'));
    loop {
        if let Some(limit) = read_number(&dir.join(files.limit)).filter(|&l| l < UNLIMITED) {
            let usage = read_number(&dir.join(files.usage)).unwrap_or(0);
            let inactive = read_stat(&dir.join(files.stat), files.inactive_key).unwrap_or(0);
            let pressure = files
                .pressure
                .and_then(|f| fs::read_to_string(dir.join(f)).ok())
                .and_then(|t| parse_pressure(&t));
            out.push(CgroupMem {
                limit,
                usage: usage.saturating_sub(inactive),
                pressure,
            });
        }
        if dir == root {
            break;
        }
        match dir.parent() {
            Some(p) if p.starts_with(root) => dir = p.to_path_buf(),
            _ => break,
        }
    }
}

fn read_number(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_stat(path: &Path, key: &str) -> Option<u64> {
    fs::read_to_string(path).ok()?.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meminfo() {
        let text = "MemTotal:        6792156 kB\nMemFree:          791672 kB\nMemAvailable:    2393856 kB\nBuffers:              16 kB\n";
        let m = parse_meminfo(text).unwrap();
        assert_eq!(m.total, 6_792_156 * 1024);
        assert_eq!(m.available, 2_393_856 * 1024);
    }

    #[test]
    fn meminfo_without_available_falls_back_to_free_plus_cache() {
        let text = "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 200 kB\nSReclaimable: 10 kB\n";
        let m = parse_meminfo(text).unwrap();
        assert_eq!(m.available, 360 * 1024);
        assert!(parse_meminfo("MemFree: 1 kB\n").is_none());
    }

    #[test]
    fn parses_rss_and_pressure() {
        assert_eq!(
            parse_status_rss("Name:\tx\nVmRSS:\t  4096 kB\n"),
            Some(4096 * 1024)
        );
        let p = parse_pressure(
            "some avg10=12.50 avg60=3.00 avg300=1.00 total=1\nfull avg10=0.75 avg60=0.00 avg300=0.00 total=2\n",
        )
        .unwrap();
        assert_eq!(p.some_avg10, 12.5);
        assert_eq!(p.full_avg10, 0.75);
    }

    #[test]
    fn walks_cgroup_hierarchy_for_limits() {
        let root = std::env::temp_dir().join("shp-test-cgroup");
        let _ = fs::remove_dir_all(&root);
        let leaf = root.join("system.slice/proxy.service");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(leaf.join("memory.max"), "max\n").unwrap();
        let parent = root.join("system.slice");
        fs::write(parent.join("memory.max"), "1073741824\n").unwrap();
        fs::write(parent.join("memory.current"), "536870912\n").unwrap();
        fs::write(
            parent.join("memory.stat"),
            "anon 100\nfile 300000000\ninactive_file_x 5\ninactive_file 268435456\n",
        )
        .unwrap();
        fs::write(
            parent.join("memory.pressure"),
            "some avg10=1.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        )
        .unwrap();

        let limits = cgroup_limits_from("0::/system.slice/proxy.service\n", &root);
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].limit, 1 << 30);
        assert_eq!(limits[0].usage, 536_870_912 - 268_435_456);
        assert_eq!(limits[0].pressure.unwrap().some_avg10, 1.0);

        // v1 の無制限値は無視される
        let v1 = root.join("memory/app");
        fs::create_dir_all(&v1).unwrap();
        fs::write(v1.join("memory.limit_in_bytes"), "9223372036854771712\n").unwrap();
        assert!(cgroup_limits_from("3:cpu,memory:/app\n", &root).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_live_meminfo() {
        let m = mem_info().expect("/proc/meminfo should be readable");
        assert!(m.total > 0 && m.available <= m.total);
        assert!(process_rss().unwrap_or(0) > 0);
    }
}
