//! メモリ関連のシステム情報を `/proc` と cgroup から読む (std のみ)。

use super::proc::read_small;
use std::path::Path;

/// システム全体のメモリ量 (バイト)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfo {
    pub total: u64,
    /// カーネルの `MemAvailable` (回収可能なページキャッシュを含む見積もり)。
    pub available: u64,
    /// 最近使われた (活性な) ファイルページ。他者のホットなページキャッシュ。
    pub active_file: u64,
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
    parse_meminfo(&read_small(Path::new("/proc/meminfo"))?)
}

pub fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let mut total = None;
    let mut available = None;
    let mut free = None;
    let mut active_file = 0u64;
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
            "Active(file)" => active_file = bytes,
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
        active_file,
    })
}

/// カーネルが常に空けておこうとする量 (`vm.min_free_kbytes`, バイト)。
pub fn min_free_bytes() -> Option<u64> {
    read_small(Path::new("/proc/sys/vm/min_free_kbytes"))?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|kb| kb * 1024)
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
    parse_status_rss(&read_small(Path::new("/proc/self/status"))?)
}

pub fn parse_status_rss(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(sized_field)
}

pub fn mem_pressure() -> Option<MemPressure> {
    parse_pressure(&read_small(Path::new("/proc/pressure/memory"))?)
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
    match read_small(Path::new("/proc/self/cgroup")) {
        Some(text) => cgroup_limits_from(&text, Path::new("/sys/fs/cgroup")),
        None => Vec::new(),
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
                .and_then(|f| read_small(&dir.join(f)))
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
    read_small(path)?.trim().parse().ok()
}

fn read_stat(path: &Path, key: &str) -> Option<u64> {
    read_small(path)?.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

/// 自プロセスの常駐の内訳 (バイト。`/proc/self/status` の**同じ 1 回の読み**から。T.32.4)。
///
/// `rss` は `VmRSS`、`anon` / `file` / `shmem` は `RssAnon` / `RssFile` / `RssShmem`
/// (`rss = anon + file + shmem`)。`Rss*` はカーネル 4.5 から (それより前は `None`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessRss {
    pub rss: u64,
    pub anon: Option<u64>,
    pub file: Option<u64>,
    pub shmem: Option<u64>,
}

/// 自プロセスの常駐の内訳を今読む (T.32.4)。
pub fn process_rss_detail() -> Option<ProcessRss> {
    parse_status_rss_detail(&read_small(Path::new("/proc/self/status"))?)
}

pub fn parse_status_rss_detail(text: &str) -> Option<ProcessRss> {
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(sized_field)
    };
    Some(ProcessRss {
        rss: field("VmRSS:")?,
        anon: field("RssAnon:"),
        file: field("RssFile:"),
        shmem: field("RssShmem:"),
    })
}

/// スレッドのスタックとして常駐しているページの合計 (バイト。T.32.4)。
///
/// `/status` の `memory.stacks_estimate` は予約 (接続スレッド 256 KiB・ほか 2 MiB) の合計で、
/// 実際に触ったのは 1 本 8〜28 KiB だった (tokyo の 22.0 MB に対し常駐は 1 MB 未満)。
/// `/proc/self/maps` でスタックの写像を見つけ、`mincore(2)` で常駐の頁を数える
/// (`/proc/self/smaps` は全部の写像のページ表を歩くので 1 回 0.7〜1 ms かかった)。
/// 読むのは `/status` に来たときだけ。Linux 以外は `None`。
pub fn stacks_resident() -> Option<u64> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    let mut total = 0u64;
    for (start, end) in parse_stack_ranges(&maps) {
        total += resident_bytes(start, end)?;
    }
    Some(total)
}

/// maps (smaps の見出しの行も同じ形) から、スタックの写像の `[始まり, 終わり)` を拾う。
///
/// スタックと見なすのは主スレッドの `[stack]` と、**すぐ下に `---p` の番兵が接している
/// 名前の無い `rw-p`** (glibc の `pthread_create` が取るスタックの形: 下端に番兵を置いて上へ使う)。
/// malloc のアリーナは逆に `rw-p` の上に `---p` の予約が続くので数えない。
pub fn parse_stack_ranges(text: &str) -> Vec<(u64, u64)> {
    // 直前の写像: (終わりの番地, 名前の無い `---p` で 64 KiB 以下か)
    let mut prev: Option<(u64, bool)> = None;
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(first) = it.next() else {
            continue;
        };
        let Some((a, b)) = first.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16)) else {
            continue;
        };
        let perms = it.next().unwrap_or("");
        // offset・dev・inode の後ろが名前 (無ければ名前の無い写像)
        let name = it.nth(3).unwrap_or("");
        let guard_below = prev.is_some_and(|(e, guard)| guard && e == start);
        if name == "[stack]" || (name.is_empty() && perms == "rw-p" && guard_below) {
            out.push((start, end));
        }
        prev = Some((
            end,
            name.is_empty() && perms == "---p" && end.saturating_sub(start) <= 64 * 1024,
        ));
    }
    out
}

#[cfg(target_os = "linux")]
fn resident_bytes(start: u64, end: u64) -> Option<u64> {
    use std::ffi::{c_int, c_long, c_void};
    unsafe extern "C" {
        fn mincore(addr: *mut c_void, length: usize, vec: *mut u8) -> c_int;
        fn sysconf(name: c_int) -> c_long;
    }
    /// glibc / musl の Linux の `_SC_PAGESIZE`
    const SC_PAGESIZE: c_int = 30;
    // SAFETY: 引数を取らない問い合わせ。
    let page = unsafe { sysconf(SC_PAGESIZE) };
    let page = u64::try_from(page).ok().filter(|p| *p > 0)?;
    let len = end.checked_sub(start)?;
    let mut vec = vec![0u8; len.div_ceil(page) as usize];
    // SAFETY: `start` は maps に出ていた写像の頁境界、`vec` は頁の数だけある。写像が
    // その間に外れていれば ENOMEM で -1 が返るだけ (その写像は数えない)。
    let r = unsafe { mincore(start as *mut c_void, len as usize, vec.as_mut_ptr()) };
    if r != 0 {
        return Some(0);
    }
    Some(vec.iter().filter(|v| *v & 1 == 1).count() as u64 * page)
}

#[cfg(not(target_os = "linux"))]
fn resident_bytes(_: u64, _: u64) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_status_rss_detail_from_one_read() {
        let text = "Name:\tx\nVmRSS:\t   26000 kB\nRssAnon:\t   17000 kB\nRssFile:\t    9000 kB\nRssShmem:\t       0 kB\n";
        let m = parse_status_rss_detail(text).unwrap();
        assert_eq!(m.rss, 26_000 * 1024);
        assert_eq!(m.anon, Some(17_000 * 1024));
        assert_eq!(m.file, Some(9_000 * 1024));
        assert_eq!(m.shmem, Some(0));
        let old = parse_status_rss_detail("VmRSS: 10 kB\n").unwrap();
        assert_eq!((old.rss, old.anon, old.file), (10 * 1024, None, None));
        assert!(parse_status_rss_detail("RssAnon: 1 kB\n").is_none());
    }

    /// 番兵が下に接した `rw-p` と `[stack]` だけを拾い、アリーナ (`rw-p` の上に `---p`) と
    /// 名前のある写像は拾わない (T.32.4。smaps の見出しの行も同じ形)。
    #[test]
    fn finds_only_thread_stacks_in_maps() {
        let text = "\
7f0000000000-7f0000001000 ---p 00000000 00:00 0 
Rss:                   0 kB
7f0000001000-7f0000041000 rw-p 00000000 00:00 0 
Size:                256 kB
Rss:                  12 kB
7f1000000000-7f1000100000 rw-p 00000000 00:00 0 
Rss:                 900 kB
7f1000100000-7f1004000000 ---p 00000000 00:00 0 
Rss:                   0 kB
7f1004000000-7f1004200000 rw-p 00000000 00:00 0 
Rss:                 500 kB
7f2000000000-7f2000010000 r-xp 00000000 08:01 1234                       /usr/lib/libc.so.6
Rss:                  64 kB
7ffd00000000-7ffd00021000 rw-p 00000000 00:00 0                          [stack]
Rss:                  20 kB
";
        assert_eq!(
            parse_stack_ranges(text),
            vec![
                (0x7f0000001000, 0x7f0000041000),
                (0x7ffd00000000, 0x7ffd00021000)
            ]
        );
    }

    /// この試験のプロセス自身で: 内訳の和が VmRSS に合い、スタックの常駐が見つかる。
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_rss_detail_adds_up() {
        let m = process_rss_detail().unwrap();
        if let (Some(a), Some(f), Some(s)) = (m.anon, m.file, m.shmem) {
            assert!((a + f + s).abs_diff(m.rss) <= 4096, "{:?}", m);
        }
        let stacks = std::thread::spawn(|| stacks_resident().unwrap())
            .join()
            .unwrap();
        assert!(stacks > 0 && stacks < m.rss, "{} / {:?}", stacks, m);
    }

    #[test]
    fn parses_meminfo() {
        let text = "MemTotal:        6792156 kB\nMemFree:          791672 kB\nMemAvailable:    2393856 kB\nBuffers:              16 kB\nActive(file):     617388 kB\n";
        let m = parse_meminfo(text).unwrap();
        assert_eq!(m.total, 6_792_156 * 1024);
        assert_eq!(m.available, 2_393_856 * 1024);
        assert_eq!(m.active_file, 617_388 * 1024);
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
