//! cgroup v2 の **CPU の絞り** (`cpu.stat` / `cpu.max`) と **PSI** (`*.pressure`) を読む
//! (std のみ。T14.12)。メモリ側 ([`super::mem::cgroup_mem_limits`]) と対になる。
//!
//! 動作環境 (Pterodactyl) は CPU 上限を持つので、遅いときに「自分が遅い」のか
//! 「**絞られて待たされた**」のか (`nr_throttled` / `throttled_usec`)、あるいは
//! 「隣のコンテナに CPU を取られた」のか (`cpu.pressure` の `some avg10`) が分かれる。
//!
//! 読むのは **5 秒の標本のときだけ**。cgroup v1 だけの環境・`/sys/fs/cgroup` が無い環境・
//! PSI を持たないカーネルではそれぞれ `None` を返す (呼び出し側は `null` を出す)。

use std::fs;
use std::path::{Path, PathBuf};

/// PSI の 1 資源ぶん (`some` / `full` の `avg10`)。形はメモリ側と同じなので型を共有する。
pub type Psi = super::mem::MemPressure;

/// cgroup v2 の CPU の絞り。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CgroupCpu {
    /// `cpu.stat` の `nr_throttled` (絞られた期間の数、累計)
    pub nr_throttled: Option<u64>,
    /// `cpu.stat` の `throttled_usec` (絞られて止まっていた時間、累計 us)
    pub throttled_usec: Option<u64>,
    /// `cpu.max` の quota ÷ period (= 使えるコア数)。`max` (無制限) なら `None`
    pub quota_cores: Option<f64>,
}

/// cgroup v2 の PSI 3 つ。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CgroupPressure {
    pub cpu: Option<Psi>,
    pub memory: Option<Psi>,
    pub io: Option<Psi>,
}

/// 自プロセスの cgroup v2 のディレクトリ (`/proc/self/cgroup` の `0::` 行)。
///
/// v1 しか無い環境では `None`。コンテナで cgroup 名前空間が切られていると
/// パスは `/` になるので、そのまま `/sys/fs/cgroup` を指す。
pub fn cgroup_v2_dir() -> Option<PathBuf> {
    let text = fs::read_to_string("/proc/self/cgroup").ok()?;
    v2_dir_from(&text, Path::new("/sys/fs/cgroup"))
}

pub fn v2_dir_from(proc_cgroup: &str, sysfs: &Path) -> Option<PathBuf> {
    for line in proc_cgroup.lines() {
        let mut parts = line.splitn(3, ':');
        // v2 は「階層 id が 0 で、コントローラ欄が空」の 1 行だけ (`0::/path`)
        if let (Some(_), Some(""), Some(path)) = (parts.next(), parts.next(), parts.next()) {
            return Some(sysfs.join(path.trim_start_matches('/')));
        }
    }
    None
}

/// CPU の絞り (`cpu.stat` と `cpu.max`)。読めなければ各項目が `None`。
pub fn cgroup_cpu() -> CgroupCpu {
    match cgroup_v2_dir() {
        Some(dir) => cgroup_cpu_in(&dir, Path::new("/sys/fs/cgroup")),
        None => CgroupCpu::default(),
    }
}

/// PSI (`cpu.pressure` / `memory.pressure` / `io.pressure`)。
///
/// cgroup に無ければ `/proc/pressure/*` (機械全体) に落ちる。どちらも無いカーネル
/// (`CONFIG_PSI` 無し) では `None`。
pub fn cgroup_pressure() -> CgroupPressure {
    let dir = cgroup_v2_dir();
    let root = Path::new("/sys/fs/cgroup");
    let one = |name: &str, fallback: &str| {
        dir.as_deref()
            .and_then(|d| find_up(d, root, name, super::mem::parse_pressure))
            .or_else(|| {
                fs::read_to_string(fallback)
                    .ok()
                    .as_deref()
                    .and_then(super::mem::parse_pressure)
            })
    };
    CgroupPressure {
        cpu: one("cpu.pressure", "/proc/pressure/cpu"),
        memory: one("memory.pressure", "/proc/pressure/memory"),
        io: one("io.pressure", "/proc/pressure/io"),
    }
}

/// 読む先を差し替えられる版 (テスト用)。`dir` から `root` まで遡って最初に読めたものを使う。
pub fn cgroup_cpu_in(dir: &Path, root: &Path) -> CgroupCpu {
    let stat = find_up(dir, root, "cpu.stat", |t| {
        let n = stat_field(t, "nr_throttled");
        let us = stat_field(t, "throttled_usec");
        (n.is_some() || us.is_some()).then_some((n, us))
    });
    CgroupCpu {
        nr_throttled: stat.and_then(|(n, _)| n),
        throttled_usec: stat.and_then(|(_, us)| us),
        // 上限は階層のどこにでも掛かるので、**いちばんきつい値**を採る
        quota_cores: tightest_quota(dir, root),
    }
}

/// 差し替えられる版の PSI (テスト用。`/proc/pressure` への落とし込みはしない)。
pub fn cgroup_pressure_in(dir: &Path, root: &Path) -> CgroupPressure {
    let one = |name: &str| find_up(dir, root, name, super::mem::parse_pressure);
    CgroupPressure {
        cpu: one("cpu.pressure"),
        memory: one("memory.pressure"),
        io: one("io.pressure"),
    }
}

/// `dir` から `root` まで遡り、最初に `parse` が答えたものを返す。
///
/// コンテナでは自分の cgroup に cpu コントローラが有効でないことがある
/// (その場合は親の値が効いている) ので、1 段だけ見て諦めない。
fn find_up<T>(dir: &Path, root: &Path, name: &str, parse: impl Fn(&str) -> Option<T>) -> Option<T> {
    let mut at = dir.to_path_buf();
    loop {
        if let Ok(text) = fs::read_to_string(at.join(name))
            && let Some(v) = parse(&text)
        {
            return Some(v);
        }
        if at == root {
            return None;
        }
        match at.parent() {
            Some(p) if p.starts_with(root) => at = p.to_path_buf(),
            _ => return None,
        }
    }
}

/// 階層のどこかに掛かっている `cpu.max` のうち、いちばんきついコア数。
fn tightest_quota(dir: &Path, root: &Path) -> Option<f64> {
    let mut at = dir.to_path_buf();
    let mut best: Option<f64> = None;
    loop {
        if let Ok(text) = fs::read_to_string(at.join("cpu.max"))
            && let Some(cores) = parse_cpu_max(&text)
        {
            best = Some(best.map_or(cores, |b: f64| b.min(cores)));
        }
        if at == root {
            return best;
        }
        match at.parent() {
            Some(p) if p.starts_with(root) => at = p.to_path_buf(),
            _ => return best,
        }
    }
}

/// `"200000 100000"` → 2.0 コア。`"max 100000"` (無制限) は `None`。
pub fn parse_cpu_max(text: &str) -> Option<f64> {
    let mut it = text.split_whitespace();
    let quota: f64 = it.next()?.parse().ok()?;
    let period: f64 = it.next().unwrap_or("100000").parse().ok()?;
    (period > 0.0 && quota > 0.0).then(|| quota / period)
}

/// `"名前 値"` が 1 行ずつ並ぶ形式 (`cpu.stat` / `memory.stat`) から 1 つ取る。
fn stat_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// この機械の `cpu.stat` の実物の形 (**数値は架空**)。
    const CPU_STAT: &str = "\
usage_usec 31572850999
user_usec 13527968994
system_usec 18044882005
nr_periods 8123
nr_throttled 41
throttled_usec 1234567
nr_bursts 0
burst_usec 0
";

    /// 同じく `cpu.pressure` (数値は架空)。
    const CPU_PRESSURE: &str = "\
some avg10=43.02 avg60=29.74 avg300=12.64 total=1375653890
full avg10=2.99 avg60=2.10 avg300=0.90 total=166527343
";

    /// テストごとに別のツリーを作る (**同じプロセスで並列に走る**ので、
    /// 1 つの場所を使い回すと片方の後片付けがもう片方の読みを壊す)。
    fn tree(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("shp-test-cgcpu-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("user.slice/app.scope")).unwrap();
        root
    }

    #[test]
    fn reads_the_throttling_and_the_quota_from_the_real_shape() {
        let root = tree("real");
        let leaf = root.join("user.slice/app.scope");
        fs::write(leaf.join("cpu.stat"), CPU_STAT).unwrap();
        fs::write(leaf.join("cpu.max"), "200000 100000\n").unwrap();
        fs::write(leaf.join("cpu.pressure"), CPU_PRESSURE).unwrap();

        let cpu = cgroup_cpu_in(&leaf, &root);
        assert_eq!(cpu.nr_throttled, Some(41));
        assert_eq!(cpu.throttled_usec, Some(1_234_567));
        assert_eq!(cpu.quota_cores, Some(2.0));
        let psi = cgroup_pressure_in(&leaf, &root).cpu.unwrap();
        assert_eq!(psi.some_avg10, 43.02);
        assert_eq!(psi.full_avg10, 2.99);
        let _ = fs::remove_dir_all(&root);
    }

    /// 自分の階層に cpu コントローラが無くても、親に掛かっている上限は効いている。
    /// 上限は**いちばんきつい**ものを採る。
    #[test]
    fn walks_up_for_the_stats_and_takes_the_tightest_quota() {
        let root = tree("walk");
        let leaf = root.join("user.slice/app.scope");
        fs::write(root.join("cpu.stat"), CPU_STAT).unwrap();
        fs::write(root.join("cpu.max"), "max 100000\n").unwrap();
        fs::write(root.join("user.slice/cpu.max"), "400000 100000\n").unwrap();
        fs::write(leaf.join("cpu.max"), "150000 100000\n").unwrap();

        let cpu = cgroup_cpu_in(&leaf, &root);
        assert_eq!(cpu.nr_throttled, Some(41), "親の cpu.stat まで遡る");
        assert_eq!(cpu.quota_cores, Some(1.5), "1.5 < 4.0 (max は無制限)");
        let _ = fs::remove_dir_all(&root);
    }

    /// 読めない環境 (cgroup v1 / `/sys/fs/cgroup` が無い) では全部 `None`。
    #[test]
    fn a_missing_or_v1_cgroup_is_none_everywhere() {
        let root = tree("missing");
        let leaf = root.join("user.slice/app.scope");
        assert_eq!(cgroup_cpu_in(&leaf, &root), CgroupCpu::default());
        assert_eq!(cgroup_pressure_in(&leaf, &root), CgroupPressure::default());
        // v1 だけの `/proc/self/cgroup` には `0::` の行が無い
        assert!(v2_dir_from("3:cpu,cpuacct:/app\n8:memory:/app\n", &root).is_none());
        assert_eq!(
            v2_dir_from("0::/user.slice/app.scope\n", &root),
            Some(leaf.clone())
        );
        // cgroup 名前空間の中では `0::/` (= 根がそのまま自分の cgroup)
        assert_eq!(v2_dir_from("0::/\n", &root), Some(root.clone()));
        assert_eq!(parse_cpu_max("max 100000\n"), None);
        assert_eq!(parse_cpu_max("50000 100000\n"), Some(0.5));
        assert_eq!(parse_cpu_max("\n"), None);
        let _ = fs::remove_dir_all(&root);
    }
}
