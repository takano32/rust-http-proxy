use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::acl::{AclConfig, PortSet};
use crate::cache::CacheConfig;
use crate::envfile;

/// 1 接続が最悪で使う記述子の数: クライアント 1 + オリジン 1 + 素通し中のパイプ 2 (`splice`)。
pub const FDS_PER_CONN: u64 = 4;

/// 接続以外で使う記述子の予備: 待ち受け (最大 2) + epoll + inotify + 状態ファイル +
/// ブロックリストの取得 + キャッシュのディスク I/O + 標準入出力。多めに見て 64。
const NOFILE_RESERVE: u64 = 64;

/// `PROXY_MAX_CONNS=auto` の頭打ち。
///
/// 記述子が余っていてもここで止める。上限は記述子だけの歯止めではなく、
/// **fd 以外の資源 (スレッド・RSS) の歯止め**でもあるため。T2.1 で入れた既定と同じ値で、
/// 根拠は T2.3 の実測 (同時 5,000 本のアイドル接続で RSS 198 MiB。動作環境のコンテナは小さい)。
/// これを超える値が要るなら数値で明示してもらう。
pub const MAX_CONNS_CAP: usize = 4096;

/// `RLIMIT_NOFILE` の soft limit から同時接続数の上限を決める (`PROXY_MAX_CONNS=auto`)。
///
/// `(soft - 予備 64) / 4` を [`MAX_CONNS_CAP`] で頭打ちにした値。1 接続あたり最悪 4 記述子なので、
/// `ulimit -n` が 1024 の環境なら 240、4096 なら 1008 で、**`accept` が `EMFILE` で失敗する前に
/// 503 で断れる**。`0` (無制限) には決してしない (記述子切れに戻ってしまうため下限は 1)。
pub fn auto_max_conns(nofile_soft: u64) -> usize {
    let usable = nofile_soft.saturating_sub(NOFILE_RESERVE) / FDS_PER_CONN;
    usable.clamp(1, MAX_CONNS_CAP as u64) as usize
}

/// `PROXY_MAX_CONNS` の既定 (= `auto`)。`RLIMIT_NOFILE` が読めない環境では [`MAX_CONNS_CAP`]。
pub fn default_max_conns() -> usize {
    #[cfg(target_os = "linux")]
    if let Some(soft) = crate::sys::max_open_files() {
        return auto_max_conns(soft);
    }
    MAX_CONNS_CAP
}

/// `PROXY_MAX_THREADS=auto` の 1 コアあたりの本数と、その下限・上限。
///
/// 接続スレッドは**ほとんどの時間 I/O で寝ている**ので、コア数そのものでは全く足りない
/// (1 本の接続を処理しているあいだずっと 1 本要る)。一方でいくら増やしても得るものは無く、
/// 1 本あたりスタック 256 KiB と切り替えの費用がかかる。
///
/// 実測 (T10.5、`--only idle-tunnels --conc 5000`、プロキシは 4 コアに固定)。
/// 暇なトンネルは 1 本ごとに猶予 100 ms のあいだワーカーを握るので、**確立できる速さは
/// おおよそ「上限 ÷ 100 ms」**になる。上限が無いと同じ場面で 4,721 スレッドまで跳ねる。
///
/// | 上限 | 5,000 本の確立 | スレッド最大 | ピーク RSS |
/// |---|---|---|---|
/// | 64 (コア数 × 16) | 8.4 s | 68 | 26.9 MB |
/// | 128 (コア数 × 32) | 4.2 s | 132 | 26.7 MB |
/// | 192 | 3.0 s | 196 | 27.6 MB |
/// | **256 (コア数 × 64、既定)** | **2.2〜2.6 s** | **260** | **28.1 MB** |
/// | 384 | 1.8 s | 388 | 31.4 MB |
/// | 無制限 (T10.5 以前) | 1.8 s | 4,721 | 72.6 MB |
///
/// 確立の速さがほぼ元に戻り、跳ね上がりも 1 桁小さいところとして **コア数 × 64** を採った。
const THREADS_PER_CORE: usize = 64;
const MIN_MAX_THREADS: usize = 128;
const MAX_MAX_THREADS: usize = 512;

/// 生きている接続スレッドの上限の既定 (`PROXY_MAX_THREADS=auto`)。
///
/// `コア数 × 64` を 128〜512 に収め、`PROXY_MAX_CONNS` があればそれも超えない
/// (受けない接続のためのスレッドは要らない)。コア数は `available_parallelism` なので、
/// `taskset` で絞られていればその数になる (使える資源に合わせる)。
/// **`0` (無制限) には決してしない** — 上限が無いと、預けた接続が一斉に切れたときに
/// スレッドが数千まで跳ねる (T8.1 で 4,621、T10.5 で 4,721 の実測)。
pub fn default_max_threads(max_conns: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let n = cores
        .saturating_mul(THREADS_PER_CORE)
        .clamp(MIN_MAX_THREADS, MAX_MAX_THREADS);
    if max_conns > 0 { n.min(max_conns) } else { n }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// 待ち受けポート
    pub port: u16,
    /// 待ち受けアドレス。空ならデュアルスタック (`[::]` + `0.0.0.0`) を自動で試す
    pub bind_addrs: Vec<IpAddr>,
    /// IPv6 を使うか (待ち受けと AAAA での接続)。既定 on
    pub ipv6: bool,
    pub acl: AclConfig,
    /// 接続・読み書きのタイムアウト (`PROXY_TIMEOUT_SECS`、既定 30 秒、`0` で無期限)。
    ///
    /// `0` は `PROXY_TUNNEL_IDLE_SECS` と同じく**無期限**の意味 (T10.6)。
    /// ソケットへ渡すときは [`proxy_base::timeout::for_socket`] で `None` に直す
    /// (`std` は `Duration::ZERO` を `InvalidInput` で断るため)。
    pub timeout: Duration,
    /// クライアント接続を keep-alive で待つアイドル時間。0 なら 1 接続 1 要求
    pub keepalive: Duration,
    /// オリジンへのアイドル接続をホストごとに何本まで保持するか。0 で再利用しない
    pub pool_per_host: usize,
    /// アイドル接続の全体上限 (`PROXY_ORIGIN_POOL_TOTAL`)。ホスト数 × per_host の歯止め
    pub pool_total: usize,
    /// アイドルな keep-alive 接続をスレッドから外し、1 本の監視スレッド (epoll) に
    /// 預けるか (`PROXY_PARK_IDLE`、既定 on)。Linux 以外では自動的に無効。
    pub park_idle: bool,
    /// 預ける前に同じスレッドで待ってみる時間 (`PROXY_PARK_GRACE_MS`)。
    ///
    /// 続けて要求が来る忙しい接続に、預ける/戻すの往復 (epoll_ctl 2 回 + ワーカーの
    /// 受け渡し) を払わせないための猶予。0 なら猶予なしで即座に預ける。
    pub park_grace: Duration,
    /// malloc のアリーナ数の上限 (`PROXY_MALLOC_ARENAS`、`0` で glibc の既定のまま)。
    ///
    /// glibc の既定は「コア数 × 8」で、スレッドごとに別のアリーナを使う。接続ごとに
    /// スレッドが増えるので、アイドル接続を多く抱えると使われないアリーナが RSS に居座る。
    /// 実測 (2,000 本のアイドル keep-alive 接続): 既定 80.5 kB/接続 → 上限 8 で 26.8 kB (-67%)。
    /// 代償は高並列でのロック競合で、実測は conc=64 の CPU/要求 +4%、conc=8 では差なし。
    pub malloc_arenas: usize,
    /// HTTPS のオリジンへ取得に行くか (システムの OpenSSL を使う)
    pub tls_enabled: bool,
    /// オリジンの証明書を検証するか
    pub tls_verify: bool,
    /// 追加の CA 証明書ファイル (PEM)。無ければシステムの CA ストア
    pub tls_ca_file: Option<PathBuf>,
    /// 名前解決の結果を保持する時間 (`PROXY_DNS_TTL_SECS`、0 で無効)
    pub dns_ttl: Duration,
    /// `/proxy.pac` で DIRECT にするホストの一覧 (`PROXY_PAC_DIRECT`、`*.example.com` 可)
    pub pac_direct: Vec<String>,
    /// ブロックリストのファイル (`PROXY_BLOCKLIST_FILE`、hosts 形式 / 1 行 1 ドメイン)
    pub blocklist_file: Option<PathBuf>,
    /// ブロックリストの URL (`PROXY_BLOCKLIST_URL`)
    pub blocklist_url: Option<String>,
    /// URL を取り直す間隔 (`PROXY_BLOCKLIST_REFRESH_SECS`)
    pub blocklist_refresh: Duration,
    /// ブロックリストの対象外にするホスト (`PROXY_BLOCKLIST_EXEMPT`、`*.example.com` 可)
    pub blocklist_exempt: Vec<String>,
    /// 統計と履歴を `$HOME/.rust-http-proxy.rrd` に残す (`PROXY_STATS_PERSIST`、既定 on)
    pub stats_persist: bool,
    /// 最速の素通しプロファイル (`PROXY_PROFILE=lite` / `--lite`)。
    /// キャッシュ・統計の永続化・ブロックリストを止め、ログを warn にする
    pub lite: bool,
    /// CONNECT を許すあて先ポート (`PROXY_CONNECT_PORTS`、既定は制限なし)
    pub connect_ports: PortSet,
    /// ループバック・リンクローカル宛てのオリジンを許すか (`PROXY_ALLOW_LOCAL`、既定 off)。
    /// 既定ではクラウドのメタデータ (`169.254.169.254`) 経由の SSRF を 403 で止める
    pub allow_local: bool,
    /// CONNECT トンネルのアイドル打ち切り時間 (`PROXY_TUNNEL_IDLE_SECS`、既定 300 秒、`0` で無期限)
    pub tunnel_idle: Duration,
    /// 同時に受ける接続数の上限 (`PROXY_MAX_CONNS`、既定 `auto`、`0` で無制限)。
    /// 超えた接続には 503 を返して閉じる (スレッドは起こさない)。`auto` の決め方は [`auto_max_conns`]
    pub max_conns: usize,
    /// 同時に生きていてよい接続スレッドの上限 (`PROXY_MAX_THREADS`、既定 `auto`、`0` で無制限)。
    ///
    /// 上限に達したら新しいスレッドを起こさず仕事を待たせる (捨てない)。
    /// **`.env` の再読込で変わる** (T11.6。`serve` が接続ごとにこの値と `Workers` の
    /// 上限を突き合わせ、食い違ったときだけ当て直す)。`auto` の決め方は [`default_max_threads`]
    pub max_threads: usize,
    pub cache: CacheConfig,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let port_str = envfile::var("SERVER_PORT").unwrap_or_else(|| "8080".to_string());
        let allow_hosts = envfile::var("PROXY_ALLOW_HOSTS");
        let deny_hosts = envfile::var("PROXY_DENY_HOSTS");
        // `0` は無期限。1 秒に切り上げないのは、切り上げると**無期限を表す手段が
        // 設定から無くなる**ため (`PROXY_TUNNEL_IDLE_SECS=0` と意味を揃えた。T10.6)
        let timeout_secs = envfile::var("PROXY_TIMEOUT_SECS")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);

        let mut cfg = Self::new(
            &port_str,
            allow_hosts.as_deref(),
            deny_hosts.as_deref(),
            Duration::from_secs(timeout_secs),
        )?;
        // lite は「既定をまとめて off にする」だけなので、後続の環境変数が上書きできる
        cfg.lite =
            envfile::var("PROXY_PROFILE").is_some_and(|v| v.trim().eq_ignore_ascii_case("lite"));
        if cfg.lite {
            cfg.stats_persist = false;
        }
        if let Some(bind) = envfile::var("PROXY_BIND") {
            cfg.bind_addrs = parse_bind_list(&bind)?;
        }
        if let Some(secs) =
            envfile::var("PROXY_KEEPALIVE_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.keepalive = Duration::from_secs(secs);
        }
        if let Some(n) =
            envfile::var("PROXY_ORIGIN_POOL").and_then(|s| s.trim().parse::<usize>().ok())
        {
            cfg.pool_per_host = n;
        }
        if let Some(n) =
            envfile::var("PROXY_ORIGIN_POOL_TOTAL").and_then(|s| s.trim().parse::<usize>().ok())
        {
            cfg.pool_total = n;
        }
        if let Some(n) =
            envfile::var("PROXY_MALLOC_ARENAS").and_then(|s| s.trim().parse::<usize>().ok())
        {
            cfg.malloc_arenas = n;
        }
        if let Some(ms) =
            envfile::var("PROXY_PARK_GRACE_MS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.park_grace = Duration::from_millis(ms);
        }
        let off = |v: String| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        };
        if let Some(v) = envfile::var("PROXY_PARK_IDLE") {
            cfg.park_idle = !off(v);
        }
        if let Some(v) = envfile::var("PROXY_IPV6") {
            cfg.ipv6 = !off(v);
        }
        if let Some(v) = envfile::var("PROXY_TLS") {
            cfg.tls_enabled = !off(v);
        }
        if let Some(v) = envfile::var("PROXY_TLS_VERIFY") {
            cfg.tls_verify = !off(v);
        }
        // `auto` (既定) は記述子の上限から決める。数値ならその値、`0` は無制限。
        // 読めない書き方は既定 (auto) のままにする
        if let Some(v) = envfile::var("PROXY_MAX_CONNS") {
            let v = v.trim();
            if v.eq_ignore_ascii_case("auto") {
                cfg.max_conns = default_max_conns();
            } else if let Ok(n) = v.parse::<usize>() {
                cfg.max_conns = n;
            }
        }
        // スレッドの上限は接続数の上限にも従うので、**PROXY_MAX_CONNS の後に**決める
        cfg.max_threads = default_max_threads(cfg.max_conns);
        if let Some(v) = envfile::var("PROXY_MAX_THREADS") {
            let v = v.trim();
            if let Ok(n) = v.parse::<usize>() {
                cfg.max_threads = n;
            }
            // `auto` と読めない書き方は既定のまま
        }
        if let Some(secs) =
            envfile::var("PROXY_TUNNEL_IDLE_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.tunnel_idle = Duration::from_secs(secs);
        }
        if let Some(v) = envfile::var("PROXY_CONNECT_PORTS") {
            cfg.connect_ports = PortSet::parse(&v);
        }
        if let Some(v) = envfile::var("PROXY_ALLOW_LOCAL") {
            cfg.allow_local = !off(v);
        }
        if let Some(v) = envfile::var("PROXY_STATS_PERSIST") {
            cfg.stats_persist = !off(v);
        }
        if let Some(path) = envfile::var("PROXY_TLS_CA_FILE").filter(|p| !p.trim().is_empty()) {
            cfg.tls_ca_file = Some(PathBuf::from(path.trim()));
        }
        if let Some(secs) =
            envfile::var("PROXY_DNS_TTL_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.dns_ttl = Duration::from_secs(secs);
        }
        let list = |v: String| -> Vec<String> {
            v.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        };
        if let Some(v) = envfile::var("PROXY_PAC_DIRECT") {
            cfg.pac_direct = list(v);
        }
        cfg.blocklist_file = envfile::var("PROXY_BLOCKLIST_FILE")
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        cfg.blocklist_url = envfile::var("PROXY_BLOCKLIST_URL")
            .map(|u| u.trim().to_string())
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"));
        if let Some(secs) =
            envfile::var("PROXY_BLOCKLIST_REFRESH_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.blocklist_refresh = Duration::from_secs(secs.max(60));
        }
        if let Some(v) = envfile::var("PROXY_BLOCKLIST_EXEMPT") {
            cfg.blocklist_exempt = list(v);
        }
        let mut cache = CacheConfig::from_env();
        if cfg.lite {
            // lite ではブロックリストの取得もキャッシュもしない (明示指定があればそちらが勝つ)
            cfg.blocklist_file = None;
            cfg.blocklist_url = None;
            if envfile::var("PROXY_CACHE_ENABLED").is_none() {
                cache.enabled = false;
            }
        }
        Ok(cfg.with_cache(cache))
    }

    /// キャッシュ設定を差し替える。
    pub fn with_cache(mut self, cache: CacheConfig) -> Self {
        self.cache = cache;
        self
    }

    pub fn new(
        port_str: &str,
        allow_hosts: Option<&str>,
        deny_hosts: Option<&str>,
        timeout: Duration,
    ) -> Result<Self, String> {
        let port: u16 = port_str
            .parse()
            .map_err(|e| format!("Invalid SERVER_PORT '{}': {}", port_str, e))?;
        let acl = AclConfig::new(allow_hosts, deny_hosts);
        let max_conns = default_max_conns();
        Ok(Self {
            port,
            bind_addrs: Vec::new(),
            ipv6: true,
            acl,
            timeout,
            keepalive: Duration::from_secs(15),
            pool_per_host: 64,
            pool_total: 256,
            park_idle: true,
            park_grace: Duration::from_millis(3),
            malloc_arenas: 8,
            tls_enabled: true,
            tls_verify: true,
            tls_ca_file: None,
            dns_ttl: Duration::from_secs(60),
            pac_direct: Vec::new(),
            blocklist_file: None,
            blocklist_url: None,
            blocklist_refresh: Duration::from_secs(86400),
            blocklist_exempt: Vec::new(),
            stats_persist: true,
            lite: false,
            connect_ports: PortSet::default(),
            allow_local: false,
            tunnel_idle: Duration::from_secs(300),
            max_conns,
            max_threads: default_max_threads(max_conns),
            cache: CacheConfig::default(),
        })
    }
}

/// `PROXY_BIND` のカンマ区切りアドレス (`::`, `0.0.0.0`, `127.0.0.1`, `[::1]`)。空なら自動。
pub fn parse_bind_list(s: &str) -> Result<Vec<IpAddr>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .map_err(|e| format!("Invalid PROXY_BIND entry '{}': {}", item, e))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_port() {
        let cfg = Config::new("9090", None, None, Duration::from_secs(10)).unwrap();
        assert_eq!(cfg.port, 9090);
        assert!(cfg.bind_addrs.is_empty());
        assert!(cfg.ipv6, "IPv6 on by default");
        assert_eq!(cfg.timeout, Duration::from_secs(10));
        assert_eq!(cfg.keepalive, Duration::from_secs(15));
        assert_eq!(cfg.pool_per_host, 64);
        assert_eq!(cfg.pool_total, 256);
        assert_eq!(cfg.malloc_arenas, 8);
        assert_eq!(cfg.max_conns, default_max_conns());
        assert_eq!(cfg.tunnel_idle, Duration::from_secs(300));
        assert!(cfg.connect_ports.is_empty() && !cfg.allow_local);
    }

    #[test]
    fn test_auto_max_conns() {
        // 1 接続 4 記述子 + 予備 64。記述子切れ (accept の EMFILE) より先に 503 で断れる値
        assert_eq!(auto_max_conns(256), 48);
        assert_eq!(auto_max_conns(1024), 240);
        assert_eq!(auto_max_conns(4096), 1008);
        // 記述子が余っていても頭打ち (fd 以外の資源の歯止め)
        assert_eq!(auto_max_conns(524_288), MAX_CONNS_CAP);
        assert_eq!(auto_max_conns(1_048_576), MAX_CONNS_CAP);
        assert_eq!(auto_max_conns(u64::MAX), MAX_CONNS_CAP);
        // 予備にも足りない極端な環境でも 0 (= 無制限) にはしない
        assert_eq!(auto_max_conns(64), 1);
        assert_eq!(auto_max_conns(0), 1);
        // 既定はこの計算そのもので、上限を超えない
        assert!((1..=MAX_CONNS_CAP).contains(&default_max_conns()));
    }

    #[test]
    fn test_default_max_threads() {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let want = (cores * THREADS_PER_CORE).clamp(MIN_MAX_THREADS, MAX_MAX_THREADS);
        // 接続数に余裕があればコア数から決まる
        assert_eq!(default_max_threads(0), want, "無制限のときもスレッドは有限");
        assert_eq!(default_max_threads(MAX_CONNS_CAP), want);
        // 接続数の上限の方が小さければそちらに従う (受けない接続のスレッドは要らない)
        assert_eq!(default_max_threads(16), 16);
        assert_eq!(default_max_threads(1), 1);
        // 何があっても無制限にはしない
        assert!(default_max_threads(0) > 0 && default_max_threads(0) <= MAX_MAX_THREADS);
        let cfg = Config::new("8080", None, None, Duration::from_secs(30)).unwrap();
        assert_eq!(cfg.max_threads, default_max_threads(cfg.max_conns));
    }

    #[test]
    fn test_bind_list() {
        let list = parse_bind_list(" ::, 0.0.0.0 ,[::1]").unwrap();
        assert_eq!(list.len(), 3);
        assert!(list[0].is_ipv6() && list[1].is_ipv4() && list[2].is_loopback());
        assert!(parse_bind_list("nope").is_err());
        assert!(parse_bind_list("").unwrap().is_empty());
    }

    #[test]
    fn test_cache_defaults() {
        let cfg = Config::new("8080", None, None, Duration::from_secs(30)).unwrap();
        assert!(cfg.cache.mem_limit.is_auto());
        assert!(cfg.cache.disk_limit.is_auto());
        assert_eq!(cfg.cache.mem_limit.target_percent(), Some(100));
        assert!(cfg.cache.enabled);
        assert_eq!(cfg.cache.reserve, proxy_cache::cache::Reserve::Staged);
    }

    #[test]
    fn test_invalid_port() {
        assert!(Config::new("invalid", None, None, Duration::from_secs(30)).is_err());
        assert!(Config::new("99999", None, None, Duration::from_secs(30)).is_err());
    }
}
