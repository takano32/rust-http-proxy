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
    /// `cpu.stat` の `nr_periods` (CPU の期間の数、累計。T15.0 (6))。
    ///
    /// **`nr_throttled` の分母**。これが無いと「41 回絞られた」が
    /// 「41 / 8,123 = 0.5%」なのか「41 / 41 = 100%」なのか決まらない。
    pub nr_periods: Option<u64>,
    /// `cpu.stat` の `usage_usec` / `user_usec` / `system_usec` (累計 us。T16.0)。
    ///
    /// 上の 3 つと**同じファイル**から読む (探し方は変えない)。プロセスの `/proc/self/stat` と
    /// 違ってコンテナ全体 (同居するほかのプロセスも入る) のユーザー空間とカーネル側
    pub usage_usec: Option<u64>,
    pub user_usec: Option<u64>,
    pub system_usec: Option<u64>,
}

/// `cpu.stat` から読んだ行 ([`parse_cpu_stat`] の戻り)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CpuStat {
    nr_periods: Option<u64>,
    nr_throttled: Option<u64>,
    throttled_usec: Option<u64>,
    usage_usec: Option<u64>,
    user_usec: Option<u64>,
    system_usec: Option<u64>,
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
    let stat = find_up(dir, root, "cpu.stat", parse_cpu_stat).unwrap_or_default();
    CgroupCpu {
        nr_throttled: stat.nr_throttled,
        throttled_usec: stat.throttled_usec,
        // 上限は階層のどこにでも掛かるので、**いちばんきつい値**を採る
        quota_cores: tightest_quota(dir, root),
        nr_periods: stat.nr_periods,
        usage_usec: stat.usage_usec,
        user_usec: stat.user_usec,
        system_usec: stat.system_usec,
    }
}

/// `cpu.stat` から `nr_periods` / `nr_throttled` / `throttled_usec` と、
/// T16.0 で足した `usage_usec` / `user_usec` / `system_usec` を読む。
///
/// **「読めた」の判定は今までどおり絞りの 3 つだけで決める** (3 つとも無ければ `None`
/// = この階層には cpu コントローラが無い → [`find_up`] は親へ遡る)。`usage_usec` の類は
/// cpu コントローラが無くても書かれるので、判定に混ぜると遡る先が変わってしまう。
fn parse_cpu_stat(text: &str) -> Option<CpuStat> {
    let s = CpuStat {
        nr_periods: stat_field(text, "nr_periods"),
        nr_throttled: stat_field(text, "nr_throttled"),
        throttled_usec: stat_field(text, "throttled_usec"),
        usage_usec: stat_field(text, "usage_usec"),
        user_usec: stat_field(text, "user_usec"),
        system_usec: stat_field(text, "system_usec"),
    };
    (s.nr_periods.is_some() || s.nr_throttled.is_some() || s.throttled_usec.is_some()).then_some(s)
}

/// いま `cpu.stat` を実際に読んでいるファイルの道 (T15.0 (6))。
///
/// **5 秒ごとには呼ばない** — 呼ぶのは `/status` を組むときと `--check` のときだけ。
/// 自分の階層に cpu コントローラが無いと [`find_up`] は**親の値**を読むので、
/// 「絞られた 915 回」が自分のものか親のものかは、この道を見ないと決まらない。
pub fn cgroup_cpu_path() -> Option<PathBuf> {
    cgroup_v2_dir().and_then(|dir| cgroup_cpu_path_in(&dir, Path::new("/sys/fs/cgroup")))
}

/// 読む先を差し替えられる版 (テスト用)。
pub fn cgroup_cpu_path_in(dir: &Path, root: &Path) -> Option<PathBuf> {
    find_up_at(dir, root, "cpu.stat", parse_cpu_stat).map(|(path, _)| path)
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
    find_up_at(dir, root, name, parse).map(|(_, v)| v)
}

/// [`find_up`] と同じ探し方で、**読めたファイルの道も**返す (T15.0 (6))。
fn find_up_at<T>(
    dir: &Path,
    root: &Path,
    name: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Option<(PathBuf, T)> {
    let mut at = dir.to_path_buf();
    loop {
        let file = at.join(name);
        if let Ok(text) = fs::read_to_string(&file)
            && let Some(v) = parse(&text)
        {
            return Some((file, v));
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
        // `nr_throttled` の分母 (T15.0 (6))。41 / 8,123 = 0.5% と読める
        assert_eq!(cpu.nr_periods, Some(8_123));
        // ユーザー空間とカーネル側 (T16.0)。同じファイルから
        assert_eq!(cpu.usage_usec, Some(31_572_850_999));
        assert_eq!(cpu.user_usec, Some(13_527_968_994));
        assert_eq!(cpu.system_usec, Some(18_044_882_005));
        // 読んだのは自分の階層の `cpu.stat` (親の値ではない)
        assert_eq!(
            cgroup_cpu_path_in(&leaf, &root),
            Some(leaf.join("cpu.stat"))
        );
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
        assert_eq!(cpu.nr_periods, Some(8_123), "分母も同じ階層から");
        assert_eq!(cpu.quota_cores, Some(1.5), "1.5 < 4.0 (max は無制限)");
        // **どの階層を読んだか**が道で分かる (自分のではなく根の `cpu.stat`。T15.0 (6))
        assert_eq!(
            cgroup_cpu_path_in(&leaf, &root),
            Some(root.join("cpu.stat"))
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// `user_usec` の行が無い `cpu.stat` (古いカーネルの形) では、その 3 欄だけ `None` で
    /// 絞りの 3 つは今までどおり読める (T16.0)。
    #[test]
    fn a_cpu_stat_without_the_usage_lines_keeps_the_throttling() {
        let with = parse_cpu_stat(CPU_STAT).expect("読める");
        assert_eq!(with.user_usec, Some(13_527_968_994));
        assert_eq!(with.system_usec, Some(18_044_882_005));
        assert_eq!(with.usage_usec, Some(31_572_850_999));
        let without = parse_cpu_stat("nr_periods 10\nnr_throttled 2\nthrottled_usec 300\n")
            .expect("絞りの 3 つがあれば読める");
        assert_eq!(
            (
                without.nr_periods,
                without.nr_throttled,
                without.throttled_usec
            ),
            (Some(10), Some(2), Some(300))
        );
        assert_eq!(
            (without.usage_usec, without.user_usec, without.system_usec),
            (None, None, None)
        );
        // **判定は絞りの 3 つだけ**: usage の類しか無い (cpu コントローラの無い階層) なら
        // 読めなかった扱いで、親へ遡る (探し方を変えない)
        assert_eq!(
            parse_cpu_stat("usage_usec 5\nuser_usec 3\nsystem_usec 2\n"),
            None
        );
        let root = tree("usage-only");
        let leaf = root.join("user.slice/app.scope");
        fs::write(
            leaf.join("cpu.stat"),
            "usage_usec 5\nuser_usec 3\nsystem_usec 2\n",
        )
        .unwrap();
        fs::write(root.join("cpu.stat"), CPU_STAT).unwrap();
        let cpu = cgroup_cpu_in(&leaf, &root);
        assert_eq!(cpu.nr_throttled, Some(41), "今までどおり親まで遡る");
        assert_eq!(
            cpu.user_usec,
            Some(13_527_968_994),
            "同じ (親の) ファイルの値"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 読めない環境 (cgroup v1 / `/sys/fs/cgroup` が無い) では全部 `None`。
    #[test]
    fn a_missing_or_v1_cgroup_is_none_everywhere() {
        let root = tree("missing");
        let leaf = root.join("user.slice/app.scope");
        assert_eq!(cgroup_cpu_in(&leaf, &root), CgroupCpu::default());
        assert_eq!(cgroup_pressure_in(&leaf, &root), CgroupPressure::default());
        assert_eq!(cgroup_cpu_path_in(&leaf, &root), None, "読む先が無い");
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
