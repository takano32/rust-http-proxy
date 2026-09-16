//! この環境で**何が読めるか** (T14.15)。
//!
//! 統計の `null` や `partial` は「無かった」のか「読めなかった」のかで意味が違う
//! (T14.3 のスレッドの標本、T14.5 の `TCP_INFO`、T14.12 の cgroup の統計)。
//! データを読む人がそれを毎回推測しなくて済むように、**読めるかどうかを先に 1 か所で答える**。
//!
//! | 項目 | 何が読めるか | 読めないと何が `null` / `partial` になるか |
//! |---|---|---|
//! | `proc_syscall` | `/proc/self/task/<tid>/syscall` | スレッドが今どのシステムコールに居るか (T14.3) |
//! | `tcp_info` | `getsockopt(SOL_TCP, TCP_INFO)` | カーネルの RTT と再送 (T14.5) |
//! | `cgroup_cpu` | cgroup の `cpu.stat` | CPU の絞り (`nr_throttled`。T14.12) |
//! | `cgroup_pressure` | cgroup の `cpu.pressure` | PSI (隣に CPU を取られている割合。T14.12) |
//! | `ipv6_route` | `/proc/net/ipv6_route` の既定経路 | IPv6 で出られるか (T12.1 / T14.5 の読み方) |
//! | `resolver_ms` | `example.com` を 1 回引く所要 ms | リゾルバが生きているか (失敗は `null`) |
//! | `home_writable` | `$HOME` に書けるか | 状態ファイル・ブロックリストの保存 |
//!
//! **判定は起動時 1 回 + 1 時間ごと**で、要求の経路では触らない (`/status` と `/config` は
//! 覚えてある結果を読むだけ)。専用のスレッドは立てず、`.env` の監視スレッドの一巡
//! (最長 30 秒) のついでに測り直す。Linux 以外では `/proc` も cgroup も無いので
//! それぞれ `false` になる。

use std::fs;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::clock::now_epoch;
use crate::sync::RwLockExt;

/// 測り直す間隔 (起動時 1 回 + これごと)。
pub const REFRESH: Duration = Duration::from_secs(3600);

/// 名前解決の締め切り。`getaddrinfo` 自体は止められないので、別スレッドで引いて
/// こちら側だけ待つのをやめる (引けない名前は 1 回 約 2 秒かかる実測がある)。
const RESOLVER_DEADLINE: Duration = Duration::from_secs(2);

/// 測るのに使う名前。**`localhost` ではない名前**を 1 つ引いて、
/// `/etc/resolv.conf` の nameserver まで往復する時間を見る。
const PROBE_NAME: &str = "example.com";

/// この環境で読めるもの。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// `/proc/self/task/<tid>/syscall` が読める (T14.3 のスレッドの標本)
    pub proc_syscall: bool,
    /// 待ち受けソケットに `getsockopt(SOL_TCP, TCP_INFO)` が通る (T14.5 の RTT と再送)
    pub tcp_info: bool,
    /// 自分の cgroup に `cpu.stat` がある (T14.12 の CPU の絞り)
    pub cgroup_cpu: bool,
    /// 自分の cgroup に `cpu.pressure` がある (T14.12 の PSI)
    pub cgroup_pressure: bool,
    /// `/proc/net/ipv6_route` に既定経路がある (IPv6 で外へ出られる)
    pub ipv6_route: bool,
    /// 名前を 1 つ引くのにかかった ms (失敗・締め切り超過は `None`)
    pub resolver_ms: Option<u64>,
    /// `$HOME` に一時ファイルを書いて消せる (状態ファイル `.rrd` の置き場)
    pub home_writable: bool,
    /// 最後に測った時刻 (epoch 秒)
    pub checked_at: u64,
}

impl Capabilities {
    /// `resolver_ms` **以外**が全部読めるか (`--check` の終了コードはこれで決める)。
    /// 名前解決を外すのは、リゾルバが遅い / 外に出られない環境でも
    /// プロキシとしては動く (そして それ自体が測りたい数字) ため。
    pub fn all_readable(&self) -> bool {
        self.missing().is_empty()
    }

    /// 読めなかったものの名前 (`resolver_ms` は含めない)。
    pub fn missing(&self) -> Vec<&'static str> {
        [
            ("proc_syscall", self.proc_syscall),
            ("tcp_info", self.tcp_info),
            ("cgroup_cpu", self.cgroup_cpu),
            ("cgroup_pressure", self.cgroup_pressure),
            ("ipv6_route", self.ipv6_route),
            ("home_writable", self.home_writable),
        ]
        .into_iter()
        .filter(|(_, ok)| !ok)
        .map(|(name, _)| name)
        .collect()
    }

    /// 真偽の 6 項目 (名前と値。`--check` の印字と JSON で同じ順に並べる)。
    pub fn flags(&self) -> [(&'static str, bool); 6] {
        [
            ("proc_syscall", self.proc_syscall),
            ("tcp_info", self.tcp_info),
            ("cgroup_cpu", self.cgroup_cpu),
            ("cgroup_pressure", self.cgroup_pressure),
            ("ipv6_route", self.ipv6_route),
            ("home_writable", self.home_writable),
        ]
    }

    pub fn to_json(&self) -> String {
        format!(
            "{{\"proc_syscall\":{},\"tcp_info\":{},\"cgroup_cpu\":{},\"cgroup_pressure\":{},\
             \"ipv6_route\":{},\"resolver_ms\":{},\"home_writable\":{},\"checked_at\":{}}}",
            self.proc_syscall,
            self.tcp_info,
            self.cgroup_cpu,
            self.cgroup_pressure,
            self.ipv6_route,
            self.resolver_ms
                .map(|ms| ms.to_string())
                .unwrap_or_else(|| "null".to_string()),
            self.home_writable,
            self.checked_at,
        )
    }
}

/// 最後に測った結果 (まだなら `None`)。要求の経路はここを読むだけ。
static CURRENT: RwLock<Option<Capabilities>> = RwLock::new(None);

/// 待ち受けソケットの記述子 (`TCP_INFO` を試す相手)。`-1` = まだ (`--check` では立てない)。
#[cfg(target_os = "linux")]
static LISTENER_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// `TCP_INFO` を試す相手として自分の待ち受けソケットを覚える (起動時に 1 回)。
/// プロセスが終わるまで開いたままの記述子なので、番号だけ持っておけばよい。
pub fn set_listener(listener: &std::net::TcpListener) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        LISTENER_FD.store(listener.as_raw_fd(), std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = listener;
}

/// 最後に測った結果 (まだ 1 回も測っていなければ `None`)。
pub fn current() -> Option<Capabilities> {
    *CURRENT.read_locked()
}

/// `/status` と `/config` 用。まだ測っていなければ `null`。
pub fn status_json() -> String {
    current()
        .map(|c| c.to_json())
        .unwrap_or_else(|| "null".to_string())
}

/// 測って覚える。**`/proc` などの速い判定を先に覚えてから**名前解決を測る
/// (名前解決だけは最大 [`RESOLVER_DEADLINE`] かかるので、その間も `/status` に出るように)。
pub fn refresh() -> Capabilities {
    let mut caps = probe_fast();
    *CURRENT.write_locked() = Some(caps);
    caps.resolver_ms = resolver_ms();
    caps.checked_at = now_epoch();
    *CURRENT.write_locked() = Some(caps);
    caps
}

/// 名前解決まで含めて 1 回測る (`--check` 用。覚えはしない)。
pub fn probe() -> Capabilities {
    let mut caps = probe_fast();
    caps.resolver_ms = resolver_ms();
    caps
}

/// 名前解決以外 (どれも数十 us)。
fn probe_fast() -> Capabilities {
    Capabilities {
        proc_syscall: proc_syscall_readable(),
        tcp_info: tcp_info_works(),
        cgroup_cpu: cgroup_file_here("cpu.stat"),
        cgroup_pressure: cgroup_file_here("cpu.pressure"),
        ipv6_route: has_ipv6_default_route(),
        resolver_ms: None,
        home_writable: home_writable(),
        checked_at: now_epoch(),
    }
}

/// `/proc/self/task/<自分の tid>/syscall` が読めるか (T14.3 が読む先そのもの)。
///
/// `/proc/thread-self` は `/proc/self/task/<tid>` への symlink。コンテナの seccomp や
/// `hidepid` で読めないことがあるので、**読めるかどうか**をここで答える。
fn proc_syscall_readable() -> bool {
    if readable("/proc/thread-self/syscall") {
        return true;
    }
    // `/proc/thread-self` の無い古いカーネル向け: 自分のスレッドを 1 本選んで試す
    let Ok(dir) = fs::read_dir("/proc/self/task") else {
        return false;
    };
    dir.flatten()
        .next()
        .is_some_and(|e| readable(&e.path().join("syscall").to_string_lossy()))
}

fn readable(path: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|s| !s.trim().is_empty())
}

/// 待ち受けソケット (無ければその場で 1 つ作ったソケット) に `TCP_INFO` が通るか。
#[cfg(target_os = "linux")]
fn tcp_info_works() -> bool {
    use std::os::fd::AsRawFd;
    let fd = LISTENER_FD.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        return tcp_info_ok(fd);
    }
    // `--check` は待ち受けを立てないので、同じ種類のソケットを 1 つ作って試す
    std::net::TcpListener::bind(("127.0.0.1", 0)).is_ok_and(|l| tcp_info_ok(l.as_raw_fd()))
}

#[cfg(not(target_os = "linux"))]
fn tcp_info_works() -> bool {
    false
}

/// `getsockopt(SOL_TCP, TCP_INFO)` が通るか (T14.5 が読む口と同じ呼び方)。
///
/// 構造体は `linux/tcp.h` の `struct tcp_info`。カーネルは `min(len, sizeof)` しか
/// 書かないので、104 バイトの緩衝を渡せば古いカーネルでも溢れない。
#[cfg(target_os = "linux")]
fn tcp_info_ok(fd: i32) -> bool {
    const SOL_TCP: i32 = 6;
    const TCP_INFO: i32 = 11;
    unsafe extern "C" {
        fn getsockopt(fd: i32, level: i32, name: i32, val: *mut u8, len: *mut u32) -> i32;
    }
    let mut buf = [0u8; 104];
    let mut len = buf.len() as u32;
    // SAFETY: 書き込み先は 104 バイトの配列で、その大きさを `len` で渡している。
    // カーネルは `len` を超えて書かず、書けた大きさを `len` に返す
    let rc = unsafe { getsockopt(fd, SOL_TCP, TCP_INFO, buf.as_mut_ptr(), &mut len) };
    // 先頭 8 バイト (u8 の旗) すら返らないなら、読めても使い物にならない
    rc == 0 && len >= 8
}

/// 自分の cgroup (v2 → v1 の順) に `name` のファイルがあるか。
fn cgroup_file_here(name: &str) -> bool {
    match fs::read_to_string("/proc/self/cgroup") {
        Ok(text) => cgroup_file_in(&text, Path::new("/sys/fs/cgroup"), name),
        Err(_) => false,
    }
}

/// `/proc/self/cgroup` の中身と cgroup の根を与えて、`name` が見つかるかを返す
/// (テストのために分けてある。探し方は [`crate::sysinfo::mem::cgroup_limits_from`] と同じで、
/// 自分の階層から根まで辿る)。
pub fn cgroup_file_in(proc_cgroup: &str, sysfs: &Path, name: &str) -> bool {
    for line in proc_cgroup.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(_id), Some(controllers), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let root = if controllers.is_empty() {
            sysfs.to_path_buf()
        } else if controllers.split(',').any(|c| c == "cpu") {
            sysfs.join("cpu")
        } else {
            continue;
        };
        if walk_for(&root, path, name) {
            return true;
        }
    }
    false
}

/// `root/cg_path` から `root` まで遡って `name` を探す。
fn walk_for(root: &Path, cg_path: &str, name: &str) -> bool {
    let mut dir: PathBuf = root.join(cg_path.trim_start_matches('/'));
    loop {
        if dir.join(name).exists() {
            return true;
        }
        if dir == root {
            return false;
        }
        match dir.parent() {
            Some(p) if p.starts_with(root) => dir = p.to_path_buf(),
            _ => return false,
        }
    }
}

/// `/proc/net/ipv6_route` に既定経路 (`::/0`) があるか。
fn has_ipv6_default_route() -> bool {
    fs::read_to_string("/proc/net/ipv6_route").is_ok_and(|t| parse_ipv6_default_route(&t))
}

/// 1 行目の宛先が 32 桁の `0` で、次の欄 (prefix 長) が `00` なら既定経路。
pub fn parse_ipv6_default_route(text: &str) -> bool {
    text.lines().any(|line| {
        let mut f = line.split_whitespace();
        match (f.next(), f.next()) {
            (Some(dest), Some(plen)) => {
                dest.len() == 32 && dest.bytes().all(|b| b == b'0') && plen == "00"
            }
            _ => false,
        }
    })
}

/// `$HOME` に一時ファイルを書いて消せるか (状態ファイルとブロックリストの置き場)。
fn home_writable() -> bool {
    let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) else {
        return false;
    };
    let path = Path::new(&home).join(format!(".rust-http-proxy.wtest.{}", std::process::id()));
    let ok = fs::write(&path, b"ok").is_ok() && fs::read(&path).is_ok_and(|b| b == b"ok");
    let _ = fs::remove_file(&path);
    ok
}

/// 名前を 1 つ引いてかかった ms (締め切り [`RESOLVER_DEADLINE`]、失敗は `None`)。
///
/// `getaddrinfo` には締め切りが無いので、**引くのは別のスレッド**にして待つ側だけ諦める。
/// 諦めたスレッドは答えが来たときに自分で終わる (送り先が閉じているので何もしない)。
/// 名前解決の表 (`crate::dns`) は通さない — 測りたいのは OS のリゾルバそのもの。
fn resolver_ms() -> Option<u64> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("dns-probe".into())
        .spawn(move || {
            let start = Instant::now();
            let ok = (PROBE_NAME, 80)
                .to_socket_addrs()
                .is_ok_and(|mut a| a.next().is_some());
            let _ = tx.send(ok.then(|| start.elapsed().as_millis() as u64));
        })
        .ok()?;
    rx.recv_timeout(RESOLVER_DEADLINE).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_ipv6_default_route() {
        // 実物の形 (この機械の `/proc/net/ipv6_route`。アドレスは架空)
        let text = "\
20010db8000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000000 00000000 00000001     eth0
00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000009 00000000 00450003     eth0
";
        assert!(parse_ipv6_default_route(text));
        // 既定経路の無い環境 (リンクローカルだけ)
        let only_link = text.lines().next().unwrap();
        assert!(!parse_ipv6_default_route(only_link));
        assert!(!parse_ipv6_default_route(""));
    }

    #[test]
    fn walks_the_cgroup_hierarchy_for_a_file() {
        let root = std::env::temp_dir().join(format!("shp-caps-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("app/inner")).unwrap();
        // 上の階層にだけ `cpu.stat` がある (Pterodactyl のような入れ子)
        fs::write(root.join("app/cpu.stat"), "nr_throttled 0\n").unwrap();
        assert!(cgroup_file_in("0::/app/inner\n", &root, "cpu.stat"));
        assert!(!cgroup_file_in("0::/app/inner\n", &root, "cpu.pressure"));
        // v1 (コントローラ名のディレクトリの下)
        fs::create_dir_all(root.join("cpu/app")).unwrap();
        fs::write(root.join("cpu/app/cpu.stat"), "nr_throttled 0\n").unwrap();
        assert!(cgroup_file_in("4:cpu,cpuacct:/app\n", &root, "cpu.stat"));
        // 関係のないコントローラの行は見ない
        assert!(!cgroup_file_in("5:memory:/app\n", &root, "cpu.stat"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_json_has_all_seven_items() {
        let caps = Capabilities {
            proc_syscall: true,
            tcp_info: true,
            cgroup_cpu: false,
            cgroup_pressure: false,
            ipv6_route: true,
            resolver_ms: Some(9),
            home_writable: true,
            checked_at: 1_700_000_000,
        };
        let json = caps.to_json();
        for key in [
            "proc_syscall",
            "tcp_info",
            "cgroup_cpu",
            "cgroup_pressure",
            "ipv6_route",
            "resolver_ms",
            "home_writable",
        ] {
            assert!(json.contains(key), "{} が無い: {}", key, json);
        }
        assert!(json.contains("\"resolver_ms\":9"), "{}", json);
        assert!(!caps.all_readable());
        assert_eq!(caps.missing(), vec!["cgroup_cpu", "cgroup_pressure"]);
        // 名前解決が失敗していても終了コードには効かない
        let ok = Capabilities {
            cgroup_cpu: true,
            cgroup_pressure: true,
            resolver_ms: None,
            ..caps
        };
        assert!(ok.all_readable());
        assert!(ok.to_json().contains("\"resolver_ms\":null"));
    }

    /// 実機で測れること (この機械は全部読める。`--check` が 0 で終わるのと同じ判定)。
    #[cfg(target_os = "linux")]
    #[test]
    fn probes_this_machine() {
        let caps = probe_fast();
        assert!(caps.proc_syscall, "/proc/thread-self/syscall が読めない");
        assert!(caps.tcp_info, "getsockopt(TCP_INFO) が通らない");
        assert!(caps.checked_at > 0);
        // 覚えた結果が読めること (要求の経路はこれを読むだけ)
        assert!(current().is_none() || current().is_some());
        let stored = refresh();
        assert_eq!(current(), Some(stored));
        assert!(status_json().contains("\"proc_syscall\":"));
    }
}
