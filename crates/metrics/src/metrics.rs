use crate::sync::LockExt;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::cache::Cache;

/// ホスト別に数える結果の分類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOutcome {
    Hit,
    Miss,
    Bypass,
    Error,
    /// ACL / ブロックリストで拒否した (403)
    Blocked,
}

impl HostOutcome {
    /// アクセスログの `cache=` の値とステータスから分類する。
    pub fn from_access(cache_state: &str, status: u16) -> Self {
        if status >= 500 {
            return HostOutcome::Error;
        }
        if cache_state.starts_with("HIT")
            || cache_state.starts_with("REVALIDATED")
            || cache_state.starts_with("REFRESHING")
            || cache_state.starts_with("COALESCED")
            || cache_state.starts_with("STALE")
        {
            HostOutcome::Hit
        } else if cache_state.starts_with("MISS") {
            HostOutcome::Miss
        } else {
            HostOutcome::Bypass
        }
    }
}

/// 接続元 IP ごとの統計を持つ上限。
pub const MAX_CLIENTS: usize = 1000;

/// 応答時間ヒストグラムの上限 (ms)。最後の区間は上限なし。
///
/// **1 ms 〜 10 s の 24 段、公比およそ 1.5** (T12.4 (1))。10 段だった頃は
/// デプロイ先の 50 ホスト中 29 (要求数で 74%) が `p50 = p95 = max` になっていた:
/// 257 ms が (250, 500] の 1 区間に全部入り、区間内を補間しても観測した最大値で
/// 頭打ちになるため。段を細かくすると同じ補間のままで分位点が意味を持つ
/// (実測: 257 ms × 90 + 290 ms × 10 の p50 が 290 → 262.5)。
///
/// 値は等比数列を整数に丸めたもので、**下の端 (1〜10 ms) はデプロイ先の
/// AAAA 無しホスト (p50 5.1 ms) が乗るところ**なので公比より細かく取ってある。
pub const LATENCY_BOUNDS_MS: [u64; 24] = [
    1, 2, 3, 4, 6, 9, 13, 20, 30, 45, 65, 95, 140, 210, 315, 470, 700, 1000, 1500, 2200, 3300,
    5000, 7500, 10000,
];

/// エラーの原因 (T12.4 (2))。デプロイ先で「エラー 12 件、原因は不明」だったのを
/// **8 つに畳んで**数える。細かく分けても読む人が増やせないので、対処が変わる粒度で切る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum ErrCause {
    /// 名前解決に失敗した
    Dns = 0,
    /// 接続を拒まれた (RST / ECONNREFUSED)
    Refused = 1,
    /// 経路が無い (ENETUNREACH / EHOSTUNREACH / EADDRNOTAVAIL)
    Unreachable = 2,
    /// 締め切りに間に合わなかった (`PROXY_TIMEOUT_SECS`)
    Timeout = 3,
    /// つないだあとで切られた (ECONNRESET / EPIPE / EOF)
    Reset = 4,
    /// TLS の握手や証明書で失敗した
    Tls = 5,
    /// 自分の `Via` が付いた要求 = ループ (508 Loop Detected。T12.3)
    Loop = 6,
    /// 上のどれでもない
    Other = 7,
}

/// [`ErrCause`] の数。
pub const ERR_CAUSES: usize = 8;

/// `/status` と `/metrics` のラベルに使う名前 ([`ErrCause`] と同じ順)。
pub const ERR_CAUSE_NAMES: [&str; ERR_CAUSES] = [
    "dns",
    "refused",
    "unreachable",
    "timeout",
    "reset",
    "tls",
    "loop",
    "other",
];

impl ErrCause {
    /// `io::Error` から原因を決める。**この判定はエラーのときにしか通らない**ので、
    /// 文字列を見るところがあっても熱い経路には乗らない。
    pub fn from_io(e: &std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        match e.kind() {
            K::ConnectionRefused => ErrCause::Refused,
            K::NetworkUnreachable | K::HostUnreachable | K::AddrNotAvailable => {
                ErrCause::Unreachable
            }
            K::TimedOut => ErrCause::Timeout,
            K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe | K::UnexpectedEof => {
                ErrCause::Reset
            }
            // `getaddrinfo` の失敗は Linux では `NotFound` にも `Uncategorized` にもなる
            // (`ToSocketAddrs` の実装依存) ので、文言も見る
            K::NotFound => ErrCause::Dns,
            _ => {
                let msg = e.to_string();
                if msg.contains("lookup address") || msg.contains("resolve host") {
                    ErrCause::Dns
                } else if msg.contains("TLS") || msg.contains("tls") || msg.contains("certificate")
                {
                    ErrCause::Tls
                } else {
                    ErrCause::Other
                }
            }
        }
    }

    pub fn name(self) -> &'static str {
        ERR_CAUSE_NAMES[self as usize]
    }
}

/// 1 要求ぶんの内訳 (T12.4 (2))。**[`Metrics::record`] が既に取っている鍵の内側で書く**ので、
/// 原子操作は 1 つも増えない。統計を持たない経路は [`Detail::default`] を渡す (全部 0)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Detail {
    /// 名前解決 (`getaddrinfo`) にかかった時間の合計 (ms)
    pub dns_ms: u64,
    /// そのうち実際に OS へ問い合わせた回数 (キャッシュに当たったら 0)
    pub dns_misses: u64,
    /// 接続 (SYN → 確立) にかかった時間 (ms)。名前解決のぶんは含まない
    pub connect_ms: u64,
    /// 確立した族 (`Some(true)` = IPv6)。分からなければ `None`
    pub family_v6: Option<bool>,
    /// エラーの原因 (エラーでなければ `None`)
    pub cause: Option<ErrCause>,
    /// 履歴の窓に入れる値 (ms)。forward は「初バイトまで」で、応答全体の時間
    /// (`took`) とは別。`None` なら `took` をそのまま使う (CONNECT の確立時間)
    pub first_byte_ms: Option<u64>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostStats {
    pub requests: u64,
    pub hits: u64,
    pub misses: u64,
    pub bypass: u64,
    pub errors: u64,
    /// ACL / ブロックリストで拒否した数
    pub blocked: u64,
    pub bytes: u64,
    /// 応答時間を記録した要求数と、その合計・最大 (ms)。CONNECT は接続確立までの時間
    pub timed: u64,
    pub duration_ms_sum: u64,
    pub duration_ms_max: u64,
    /// `LATENCY_BOUNDS_MS` の区間ごとの件数 (+ 上限なしの区間)
    pub buckets: [u64; LATENCY_BOUNDS_MS.len() + 1],
    /// 最後に要求を受けた時刻 (epoch 秒)
    pub last_seen: u64,
    /// 名前解決にかかった時間の合計 (ms) と、OS へ問い合わせた回数 (T12.4 (2))
    pub dns_ms_sum: u64,
    pub dns_misses: u64,
    /// 接続 (SYN → 確立) にかかった時間の合計 (ms)。名前解決のぶんは含まない
    pub connect_ms_sum: u64,
    /// 確立した族の内訳 (T12.1 の全体の統計と二重にならないよう、ここはホスト別)
    pub v4_wins: u64,
    pub v6_wins: u64,
    /// エラーの原因別の件数 ([`ErrCause`] の順)
    pub errors_by_cause: [u64; ERR_CAUSES],
}

impl HostStats {
    fn count(&mut self, outcome: HostOutcome, bytes: u64, took: Option<Duration>, detail: Detail) {
        self.requests += 1;
        self.bytes += bytes;
        self.last_seen = crate::cache::now_epoch();
        match outcome {
            HostOutcome::Hit => self.hits += 1,
            HostOutcome::Miss => self.misses += 1,
            HostOutcome::Bypass => self.bypass += 1,
            HostOutcome::Error => self.errors += 1,
            HostOutcome::Blocked => self.blocked += 1,
        }
        if let Some(d) = took {
            self.observe(d);
        }
        self.add_detail(detail);
    }

    /// 内訳を足す (鍵の内側。全部 0 の [`Detail::default`] でも同じ道を通る)。
    fn add_detail(&mut self, d: Detail) {
        self.dns_ms_sum += d.dns_ms;
        self.dns_misses += d.dns_misses;
        self.connect_ms_sum += d.connect_ms;
        match d.family_v6 {
            Some(true) => self.v6_wins += 1,
            Some(false) => self.v4_wins += 1,
            None => {}
        }
        if let Some(c) = d.cause {
            self.errors_by_cause[c as usize] += 1;
        }
    }

    /// 状態ファイルのレコード (名前 128 バイト + 数値)。
    pub fn encode(&self, name: &str) -> Vec<u8> {
        let mut e = crate::rrd::Enc::new();
        e.str(name, 128)
            .u64(self.last_seen)
            .u64(self.requests)
            .u64(self.hits)
            .u64(self.misses)
            .u64(self.bypass)
            .u64(self.errors)
            .u64(self.blocked)
            .u64(self.bytes)
            .u64(self.timed)
            .u64(self.duration_ms_sum)
            .u64(self.duration_ms_max);
        for b in self.buckets {
            e.u64(b);
        }
        e.u64(self.dns_ms_sum)
            .u64(self.dns_misses)
            .u64(self.connect_ms_sum)
            .u64(self.v4_wins)
            .u64(self.v6_wins);
        for c in self.errors_by_cause {
            e.u64(c);
        }
        e.0
    }

    pub fn decode(payload: &[u8]) -> Option<(String, HostStats)> {
        let mut d = crate::rrd::Dec(payload);
        let name = d.str(128);
        if name.is_empty() {
            return None;
        }
        let mut s = HostStats {
            last_seen: d.u64(),
            requests: d.u64(),
            hits: d.u64(),
            misses: d.u64(),
            bypass: d.u64(),
            errors: d.u64(),
            blocked: d.u64(),
            bytes: d.u64(),
            timed: d.u64(),
            duration_ms_sum: d.u64(),
            duration_ms_max: d.u64(),
            ..HostStats::default()
        };
        for b in s.buckets.iter_mut() {
            *b = d.u64();
        }
        s.dns_ms_sum = d.u64();
        s.dns_misses = d.u64();
        s.connect_ms_sum = d.u64();
        s.v4_wins = d.u64();
        s.v6_wins = d.u64();
        for c in s.errors_by_cause.iter_mut() {
            *c = d.u64();
        }
        Some((name, s))
    }

    fn observe(&mut self, d: Duration) {
        let ms = d.as_millis().min(u64::MAX as u128) as u64;
        self.timed += 1;
        self.duration_ms_sum += ms;
        self.duration_ms_max = self.duration_ms_max.max(ms);
        let idx = LATENCY_BOUNDS_MS
            .iter()
            .position(|&b| ms <= b)
            .unwrap_or(LATENCY_BOUNDS_MS.len());
        self.buckets[idx] += 1;
    }

    pub fn avg_ms(&self) -> f64 {
        if self.timed == 0 {
            0.0
        } else {
            self.duration_ms_sum as f64 / self.timed as f64
        }
    }

    /// 区間内を線形に補間した分位点 (ms)。最後の区間は最大値で頭打ち。
    pub fn quantile_ms(&self, q: f64) -> f64 {
        if self.timed == 0 {
            return 0.0;
        }
        let rank = (q.clamp(0.0, 1.0) * self.timed as f64).max(1.0);
        let mut seen = 0u64;
        for (i, &n) in self.buckets.iter().enumerate() {
            if n == 0 {
                continue;
            }
            if (seen + n) as f64 >= rank {
                let lo = if i == 0 {
                    0.0
                } else {
                    LATENCY_BOUNDS_MS[i - 1] as f64
                };
                let hi = if i < LATENCY_BOUNDS_MS.len() {
                    LATENCY_BOUNDS_MS[i] as f64
                } else {
                    (self.duration_ms_max as f64).max(lo)
                };
                let frac = (rank - seen as f64) / n as f64;
                // 観測した最大値は超えない (件数が少ないとき区間の上端が出ないように)
                return (lo + (hi - lo) * frac).min(self.duration_ms_max as f64);
            }
            seen += n;
        }
        self.duration_ms_max as f64
    }

    pub fn error_rate(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.errors as f64 / self.requests as f64
        }
    }
}

/// `/status` の JSON に埋め込む、上の層が用意する部品。
///
/// 既定 (`Default`) は `"null"`。単体テストのように上の層が居ないときに使う。
pub struct StatusExtras<'a> {
    /// `.env` の再読込の状態 (`reload::status_json()`)
    pub settings: &'a str,
    /// ブロックリストの状態 (`blocklist::status_json()`)
    pub blocklist: &'a str,
    /// 状態ファイルの状態 (`persist::status_json()`)
    pub state_file: &'a str,
    /// 動いているバイナリの版 (本体クレートの `VERSION`。`0.1.0+144b992` の形)
    pub version: &'a str,
    /// 上限といまのスレッドの数 ([`Concurrency`])
    pub concurrency: Concurrency,
    /// `hosts[]` の上位 50 をどの鍵で切り出すか (`/status?sort=`。T13.3)
    pub sort: HostSort,
}

impl Default for StatusExtras<'_> {
    fn default() -> Self {
        StatusExtras {
            settings: "null",
            blocklist: "null",
            state_file: "null",
            version: "unknown",
            concurrency: Concurrency::default(),
            sort: HostSort::Requests,
        }
    }
}

/// 上限と、いまの接続スレッドの数 (`/status` 用)。
///
/// `auto` で決まった上限を**起動ログを見なくても確かめられる**ようにするためのもの
/// (`PROXY_MAX_CONNS` は T8.5、`PROXY_MAX_THREADS` と待ち行列は T10.5 のやり残し)。
/// 値を決めるのは上の層 (`Config` と `Workers`) で、ここは受け取って並べるだけ。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Concurrency {
    /// 同時接続数の上限 (`PROXY_MAX_CONNS` が `auto` なら決まった値。`0` で無制限)
    pub max_conns: usize,
    /// 生きていてよい接続スレッドの上限 (`PROXY_MAX_THREADS`。`0` で無制限)
    pub max_threads: usize,
    /// いま生きている接続スレッドの数
    pub live_threads: usize,
    /// そのうち空き置き場に積んである数 (仕事を待っているスレッド)
    pub idle_threads: usize,
    /// 上限に達して待たせている仕事の数 (捨てていない)
    pub queued_jobs: usize,
}

/// `/status` の `hosts[]` から上位 50 を切り出す鍵 (`?sort=`。T13.3)。
///
/// **切り出す鍵だけ**を変えるもので、JSON の形も件数も変わらない。要求数の上位 50 には
/// 「悪いホスト」が出てこないのが動機で、デプロイ後 58.6 時間の実測では
/// **エラー 99 件のうち 80 件 (名前解決の失敗) を抱えたホストが 1 つも上位 50 に居なかった**
/// (上位 50 のエラーは全部 0)。`clients[]` は要求数順のまま (接続元には内訳が無いので、
/// 名前解決や確立の鍵で並べても全部 0 の同点になるだけ)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostSort {
    /// 要求数の多い順 (既定。今までの順)
    #[default]
    Requests,
    /// エラー件数の多い順
    Errors,
    /// 名前解決に費やした合計 (`dns_ms_sum`) の大きい順
    Dns,
    /// 応答 (CONNECT は確立) の平均 (`avg_ms`) の遅い順
    Slow,
}

impl HostSort {
    /// `?sort=` の値から決める。**知らない値は既定 (`requests`) に倒す**
    /// (綴り違いで 400 を返すより、今までどおりの応答を返す方が監視の口として安全)。
    pub fn from_param(v: &str) -> Self {
        match v {
            "errors" => HostSort::Errors,
            "dns" => HostSort::Dns,
            "slow" => HostSort::Slow,
            _ => HostSort::Requests,
        }
    }
}

/// ホスト別統計の上限。超えた分は `other` にまとめる。
pub const MAX_HOSTS: usize = 1000;

/// 「その区間の」合計 (T12.4 (3))。[`Metrics::take_interval`] が読んで 0 に戻す。
///
/// **ホスト別統計と同じ鍵の内側に置いてある**のがこの構造体の要点で、
/// 全体の合計を別の `AtomicU64` で持つと 1 要求あたり十数回の原子操作が増える
/// (熱い経路に測る側の費用を乗せない。T12.4 の注意)。
#[derive(Debug, Clone, Copy, Default)]
pub struct Interval {
    /// CONNECT の確立時間 (Phase 13 の主指標)
    pub connect: crate::history::Window,
    /// 転送した要求の初バイトまでの時間
    pub forward: crate::history::Window,
    pub errors: u64,
    pub errors_by_cause: [u64; ERR_CAUSES],
    pub dns_misses: u64,
    pub dns_ms_sum: u64,
}

/// ホスト別統計の表と、区間の合計。1 つの鍵で守る。
#[derive(Default)]
struct HostTable {
    map: HashMap<String, HostStats>,
    /// 起動からの累計 (`/metrics` のヒストグラム用)
    total: Interval,
    /// 直近の標本以降 (`take_interval` が読んで 0 に戻す)
    interval: Interval,
}

pub struct Metrics {
    pub start_time: Instant,
    pub total_requests: AtomicU64,
    pub active_connections: AtomicUsize,
    /// アイドルなまま監視スレッド (epoll) に預けている接続数と、その監視が生きているか
    pub parked_connections: AtomicUsize,
    /// そのうち CONNECT トンネルの数 (両方向とも暇なもの。T8.1)
    pub parked_tunnels: AtomicUsize,
    pub park_watcher_alive: AtomicBool,
    /// 同時接続数の上限に当たって 503 で断った数
    pub rejected_overload: AtomicU64,
    /// 上限に当たったときに、席を作るために閉じた暇なトンネルの数 (T13.2)
    pub evicted_idle: AtomicU64,
    pub bytes_forwarded: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    /// オリジンへ新規に張った接続数と、プールから再利用した回数
    pub origin_new: AtomicU64,
    pub origin_reused: AtomicU64,
    /// ダッシュボード用の履歴 (`history::spawn` が記録)
    pub history: crate::history::History,
    /// 直近のエラーの個票 (`/errors`。T13.4)。**書くのはエラーの経路だけ**なので、
    /// 成功の熱い経路はこのリングを 1 度も触らない
    pub errors: crate::recent::ErrorRing,
    /// ホスト (`scheme://host:port`) ごとの統計と、区間の合計
    hosts: Mutex<HostTable>,
    /// 接続元 IP ごとの統計 (上位 `MAX_CLIENTS`、あふれた分は "other")
    clients: Mutex<HashMap<String, HostStats>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            total_requests: AtomicU64::new(0),
            active_connections: AtomicUsize::new(0),
            parked_connections: AtomicUsize::new(0),
            parked_tunnels: AtomicUsize::new(0),
            park_watcher_alive: AtomicBool::new(false),
            rejected_overload: AtomicU64::new(0),
            evicted_idle: AtomicU64::new(0),
            bytes_forwarded: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            origin_new: AtomicU64::new(0),
            origin_reused: AtomicU64::new(0),
            history: crate::history::History::default(),
            errors: crate::recent::ErrorRing::new(),
            hosts: Mutex::new(HostTable::default()),
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// ホスト別に 1 要求を数える (応答時間なし)。
    pub fn record_host(&self, host: &str, outcome: HostOutcome, bytes: u64) {
        self.record(host, outcome, bytes, None, Detail::default());
    }

    /// ホスト別に 1 要求と応答時間を数える。
    pub fn record_host_timed(&self, host: &str, outcome: HostOutcome, bytes: u64, took: Duration) {
        self.record(host, outcome, bytes, Some(took), Detail::default());
    }

    /// [`record_host_timed`](Self::record_host_timed) に内訳を添えた版 (T12.4 (2))。
    /// 内訳は**この関数が取る鍵の内側**でしか触らないので、原子操作は増えない。
    pub fn record_host_detail(
        &self,
        host: &str,
        outcome: HostOutcome,
        bytes: u64,
        took: Option<Duration>,
        detail: Detail,
    ) {
        self.record(host, outcome, bytes, took, detail);
    }

    /// エラー 1 件を個票のリングに写す (`/errors`。T13.4)。
    ///
    /// **エラーを返す経路からだけ呼ぶこと。** 集計 (`/status`) では「どのホストで何件」
    /// までしか分からず、デプロイ先で 2 秒かかって失敗した名前解決の**相手と時刻**が
    /// 読めなかった (T13.0)。原因の分からないエラー (`detail.cause` が `None`) は
    /// 書かない — 原因なしの行が並んでも読む人の手が増えないため。
    pub fn record_error(
        &self,
        connect: bool,
        target: &str,
        client: &str,
        status: u16,
        detail: &Detail,
    ) {
        let Some(cause) = detail.cause else {
            return;
        };
        self.errors.push(crate::recent::ErrorEntry::new(
            connect,
            target,
            client,
            status,
            cause,
            detail.dns_ms,
            detail.connect_ms,
        ));
    }

    fn record(
        &self,
        host: &str,
        outcome: HostOutcome,
        bytes: u64,
        took: Option<Duration>,
        detail: Detail,
    ) {
        let mut hosts = self.hosts.locked();
        let hosts = &mut *hosts;
        // 全体の合計も同じ鍵の内側で足す (原子操作を増やさない)
        for iv in [&mut hosts.total, &mut hosts.interval] {
            iv.dns_misses += detail.dns_misses;
            iv.dns_ms_sum += detail.dns_ms;
            if outcome == HostOutcome::Error {
                iv.errors += 1;
            }
            if let Some(c) = detail.cause {
                iv.errors_by_cause[c as usize] += 1;
            }
            if let Some(d) = took {
                let ms = detail
                    .first_byte_ms
                    .unwrap_or_else(|| d.as_millis().min(u64::MAX as u128) as u64);
                // CONNECT のホスト別統計の鍵は `connect://` で始まる (`tunnel::report`)。
                // 前綴りを見るだけで済むので、呼び出し側に旗を持たせない
                if host.starts_with("connect://") {
                    iv.connect.observe(ms);
                } else if !host.starts_with("blocked://") && !host.starts_with("loop://") {
                    iv.forward.observe(ms);
                }
            }
        }
        // 既にある行はキーを作り直さない (毎要求の String 確保をなくす)
        if let Some(stats) = hosts.map.get_mut(host) {
            stats.count(outcome, bytes, took, detail);
            return;
        }
        let key = if hosts.map.len() >= MAX_HOSTS {
            "other".to_string()
        } else {
            host.to_string()
        };
        hosts
            .map
            .entry(key)
            .or_default()
            .count(outcome, bytes, took, detail);
    }

    /// 直近の標本以降の合計を読み、0 に戻す ([`crate::history::Sample::take`] だけが呼ぶ)。
    pub fn take_interval(&self) -> Interval {
        let mut hosts = self.hosts.locked();
        std::mem::take(&mut hosts.interval)
    }

    /// 起動からの累計 (`/metrics` の全体のヒストグラム用)。
    pub fn totals(&self) -> Interval {
        self.hosts.locked().total
    }

    /// 接続元 IP ごとに 1 要求を数える。
    pub fn record_client(
        &self,
        client: &str,
        outcome: HostOutcome,
        bytes: u64,
        took: Option<Duration>,
    ) {
        let mut clients = self.clients.locked();
        if let Some(stats) = clients.get_mut(client) {
            stats.count(outcome, bytes, took, Detail::default());
            return;
        }
        let key = if clients.len() >= MAX_CLIENTS {
            "other".to_string()
        } else {
            client.to_string()
        };
        clients
            .entry(key)
            .or_default()
            .count(outcome, bytes, took, Detail::default());
    }

    /// 要求数の多い順に並べた接続元別統計。
    pub fn clients_sorted(&self) -> Vec<(String, HostStats)> {
        let clients = self.clients.locked();
        let mut v: Vec<(String, HostStats)> = clients
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        v.sort_by(|a, b| b.1.requests.cmp(&a.1.requests).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// 起動時に状態ファイルから読み戻す (今の値が空のときだけ)。
    pub fn restore(&self, hosts: Vec<(String, HostStats)>, clients: Vec<(String, HostStats)>) {
        let mut h = self.hosts.locked();
        if h.map.is_empty() {
            h.map.extend(hosts);
        }
        let mut c = self.clients.locked();
        if c.is_empty() {
            c.extend(clients);
        }
    }

    /// 要求数の多い順に並べたホスト別統計。
    pub fn hosts_sorted(&self) -> Vec<(String, HostStats)> {
        self.hosts_sorted_by(HostSort::Requests)
    }

    /// 鍵を選んで並べたホスト別統計 (T13.3)。`/status?sort=` が上位 50 を切り出すのに使う。
    ///
    /// **同点は要求数 → 名前で崩す**ので、どの鍵でも順序は 1 つに決まる (テストが順序で書ける)。
    /// 名前解決だけは合計が同じときに問い合わせた回数を先に見る: 手元の loopback では
    /// ミス 1 回が 1 ms 未満で `dns_ms_sum` が 0 に丸まる (`dns::take_resolve_cost`) ため、
    /// 合計だけでは「名前で引いているホスト」と「IP リテラル」の区別が付かない。
    pub fn hosts_sorted_by(&self, sort: HostSort) -> Vec<(String, HostStats)> {
        let hosts = self.hosts.locked();
        let mut v: Vec<(String, HostStats)> = hosts
            .map
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        v.sort_by(|a, b| {
            let tie = b.1.requests.cmp(&a.1.requests).then_with(|| a.0.cmp(&b.0));
            match sort {
                HostSort::Requests => tie,
                HostSort::Errors => b.1.errors.cmp(&a.1.errors).then(tie),
                HostSort::Dns => {
                    b.1.dns_ms_sum
                        .cmp(&a.1.dns_ms_sum)
                        .then_with(|| b.1.dns_misses.cmp(&a.1.dns_misses))
                        .then(tie)
                }
                HostSort::Slow => b.1.avg_ms().total_cmp(&a.1.avg_ms()).then(tie),
            }
        });
        v
    }

    pub fn inc_requests(&self) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_active_conn(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_active_conn(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn add_bytes(&self, bytes: u64) {
        self.bytes_forwarded.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn inc_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// プールから再利用できた割合 (`reused / (new + reused)`)。
    pub fn pool_hit_ratio(&self) -> f64 {
        let new = self.origin_new.load(Ordering::Relaxed);
        let reused = self.origin_reused.load(Ordering::Relaxed);
        match new + reused {
            0 => 0.0,
            total => reused as f64 / total as f64,
        }
    }

    pub fn inc_origin_conn(&self, reused: bool) {
        if reused {
            self.origin_reused.fetch_add(1, Ordering::Relaxed);
        } else {
            self.origin_new.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn to_json(&self) -> String {
        self.to_json_with_cache(None, StatusExtras::default())
    }

    /// キャッシュ統計と、上の層が用意した部品を含めた `/status` 用 JSON を生成する。
    ///
    /// `settings` / `blocklist` / `state_file` を**呼び出し側から受け取る**のは、
    /// ここで `reload::status_json()` のように直接呼ぶと、下の層 (指標) が上の層を
    /// 呼ぶ形になって依存が輪になるため。`/status` を組み立てるのは `proxy-endpoints`
    /// の仕事で、ここはその部品を並べるだけにする。
    pub fn to_json_with_cache(&self, cache: Option<&Cache>, extra: StatusExtras<'_>) -> String {
        let uptime = self.start_time.elapsed().as_secs();
        let requests = self.total_requests.load(Ordering::Relaxed);
        let active = self.active_connections.load(Ordering::Relaxed);
        let bytes = self.bytes_forwarded.load(Ordering::Relaxed);

        let cache_json = match cache {
            Some(c) => c.to_json(),
            None => "null".to_string(),
        };

        // 上位 50 を切り出す鍵だけが `?sort=` で変わる (JSON の形は変わらない。T13.3)
        let all_hosts = self.hosts_sorted_by(extra.sort);
        // ホスト別統計は `.rrd` で再起動をまたいで通算されるので、**いつからの通算か**を出す
        // (`total_requests` は起動から、`hosts[]` は通算という窓の混在が読めなかった)
        let restored_since = all_hosts
            .iter()
            .map(|(_, s)| s.last_seen)
            .filter(|&t| t > 0)
            .min()
            .unwrap_or(0);
        // `/proc` を読むのはこのパスに来たときだけ (要求ごとには読まない)
        let threads = crate::sysinfo::process_threads().unwrap_or(0);
        let (fds, max_fds) = crate::sysinfo::process_fds().unwrap_or((0, 0));
        let hosts_json: Vec<String> = all_hosts
            .into_iter()
            .take(50)
            .map(|(h, s)| {
                format!(
                    "{{\"host\":\"{}\",{}}}",
                    crate::json::escape(&h),
                    stats_json(&s, true)
                )
            })
            .collect();
        let clients_json: Vec<String> = self
            .clients_sorted()
            .into_iter()
            .take(50)
            .map(|(c, s)| {
                format!(
                    "{{\"client\":\"{}\",{}}}",
                    crate::json::escape(&c),
                    stats_json(&s, false)
                )
            })
            .collect();
        format!(
            concat!(
                "{{\"status\":\"ok\",\"version\":\"{}\",\"uptime_secs\":{},\"total_requests\":{},",
                // 窓の目印 (T12.4 (4)): `since_start_secs` から下は起動から、
                // `restored_since` は `hosts[]` / `clients[]` が何時からの通算か (epoch 秒、0 = 無し)
                "\"since_start_secs\":{},\"restored_since\":{},",
                "\"threads\":{},\"fds\":{},\"max_fds\":{},",
                "\"active_connections\":{},\"max_conns\":{},",
                "\"parked_connections\":{},\"parked_tunnels\":{},",
                "\"parking\":{},",
                "\"live_threads\":{},\"idle_threads\":{},\"queued_jobs\":{},\"max_threads\":{},",
                "\"rejected_overload\":{},\"evicted_idle\":{},\"bytes_forwarded\":{},",
                "\"cache_hits\":{},\"cache_misses\":{},",
                "\"origin_connections\":{{\"new\":{},\"reused\":{},\"pool_hit_ratio\":{:.4}}},",
                "\"hosts\":[{}],\"clients\":[{}],",
                "\"log_level\":\"{}\",\"settings\":{},\"dns\":{},\"ipv6\":{},\"blocklist\":{},\"state_file\":{},\"cache\":{}}}"
            ),
            crate::json::escape(extra.version),
            uptime,
            requests,
            uptime,
            restored_since,
            threads,
            fds,
            max_fds,
            active,
            extra.concurrency.max_conns,
            self.parked_connections.load(Ordering::Relaxed),
            self.parked_tunnels.load(Ordering::Relaxed),
            self.park_watcher_alive.load(Ordering::Relaxed),
            extra.concurrency.live_threads,
            extra.concurrency.idle_threads,
            extra.concurrency.queued_jobs,
            extra.concurrency.max_threads,
            self.rejected_overload.load(Ordering::Relaxed),
            self.evicted_idle.load(Ordering::Relaxed),
            bytes,
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_misses.load(Ordering::Relaxed),
            self.origin_new.load(Ordering::Relaxed),
            self.origin_reused.load(Ordering::Relaxed),
            self.pool_hit_ratio(),
            hosts_json.join(","),
            clients_json.join(","),
            crate::log::current_level().as_str().trim(),
            extra.settings,
            crate::dns::status_json(),
            crate::net::ipv6_status_json(),
            extra.blocklist,
            extra.state_file,
            cache_json
        )
    }
}

/// ホスト別 / 接続元別に共通の統計フィールド (先頭・末尾の波括弧なし)。
///
/// `detail` はホスト別だけ (T12.4 (2))。接続元別には名前解決も接続も族も無いので、
/// 全部 0 の列を 50 行ぶん並べても `/status` が太るだけになる。
fn stats_json(s: &HostStats, detail: bool) -> String {
    let mut out = format!(
        "\"requests\":{},\"hits\":{},\"misses\":{},\"bypass\":{},\"errors\":{},\"blocked\":{},\"bytes\":{},\"timed\":{},\"avg_ms\":{:.1},\"p50_ms\":{:.1},\"p95_ms\":{:.1},\"max_ms\":{},\"last_seen\":{}",
        s.requests,
        s.hits,
        s.misses,
        s.bypass,
        s.errors,
        s.blocked,
        s.bytes,
        s.timed,
        s.avg_ms(),
        s.quantile_ms(0.5),
        s.quantile_ms(0.95),
        s.duration_ms_max,
        s.last_seen
    );
    if detail {
        let _ = write!(
            out,
            ",\"dns_ms_sum\":{},\"dns_misses\":{},\"connect_ms_sum\":{},\"v4_wins\":{},\"v6_wins\":{},\"errors_by_cause\":[",
            s.dns_ms_sum, s.dns_misses, s.connect_ms_sum, s.v4_wins, s.v6_wins
        );
        for (i, c) in s.errors_by_cause.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", c);
        }
        out.push(']');
    }
    out
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics() {
        let metrics = Metrics::new();
        metrics.inc_requests();
        metrics.inc_active_conn();
        metrics.add_bytes(1024);

        let json = metrics.to_json();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"total_requests\":1"));
        assert!(json.contains("\"active_connections\":1"));
        assert!(json.contains("\"bytes_forwarded\":1024"));

        metrics.inc_cache_hit();
        metrics.inc_cache_miss();
        let json_c = metrics.to_json();
        assert!(json_c.contains("\"cache_hits\":1"));
        assert!(json_c.contains("\"cache_misses\":1"));
        assert!(json_c.contains("\"cache\":null"));

        metrics.dec_active_conn();
        let json2 = metrics.to_json();
        assert!(json2.contains("\"active_connections\":0"));
        // 上の層が渡さないときは 0 (= 無制限・数えていない) で出る
        assert!(json2.contains("\"max_conns\":0"));
        assert!(
            json2.contains(
                "\"live_threads\":0,\"idle_threads\":0,\"queued_jobs\":0,\"max_threads\":0"
            )
        );
    }

    /// 版は上の層から渡ったものがそのまま `/status` に出ること (T12.6)。
    #[test]
    fn the_version_from_the_upper_layer_lands_in_the_status_json() {
        let m = Metrics::new();
        let json = m.to_json_with_cache(
            None,
            StatusExtras {
                version: "0.1.0+deadbee",
                ..StatusExtras::default()
            },
        );
        assert!(json.contains("\"version\":\"0.1.0+deadbee\""), "{}", json);
        // 渡されなければ "unknown" (git の無い環境でビルドしたときと同じ見え方)
        assert!(
            m.to_json().contains("\"version\":\"unknown\""),
            "{}",
            m.to_json()
        );
    }

    /// 上限といまのスレッド数は上の層から渡ったものがそのまま出ること (T10.7)。
    #[test]
    fn capacity_from_the_upper_layer_lands_in_the_status_json() {
        let m = Metrics::new();
        let json = m.to_json_with_cache(
            None,
            StatusExtras {
                concurrency: Concurrency {
                    max_conns: 48,
                    max_threads: 256,
                    live_threads: 7,
                    idle_threads: 3,
                    queued_jobs: 2,
                },
                ..StatusExtras::default()
            },
        );
        assert!(json.contains("\"max_conns\":48"), "{}", json);
        assert!(
            json.contains(
                "\"live_threads\":7,\"idle_threads\":3,\"queued_jobs\":2,\"max_threads\":256"
            ),
            "{}",
            json
        );
    }

    #[test]
    fn clients_and_blocked_are_counted() {
        let m = Metrics::new();
        m.record_client(
            "10.0.0.1",
            HostOutcome::Hit,
            100,
            Some(Duration::from_millis(30)),
        );
        m.record_client("10.0.0.1", HostOutcome::Blocked, 0, None);
        m.record_client("10.0.0.2", HostOutcome::Bypass, 5, None);
        m.record_host("blocked://ads.example", HostOutcome::Blocked, 0);
        let clients = m.clients_sorted();
        assert_eq!(clients[0].0, "10.0.0.1");
        assert_eq!(clients[0].1.requests, 2);
        assert_eq!(clients[0].1.blocked, 1);
        assert_eq!(clients[0].1.timed, 1);
        assert!(clients[0].1.last_seen > 0);
        let enc = clients[0].1.encode("10.0.0.1");
        assert!(enc.len() <= crate::rrd::STATS_RECORD - 4);
        let (name, back) = HostStats::decode(&enc).unwrap();
        assert_eq!(name, "10.0.0.1");
        assert_eq!(back, clients[0].1);
        let json = m.to_json();
        assert!(json.contains("\"clients\":[{\"client\":\"10.0.0.1\",\"requests\":2"));
        assert!(json.contains("\"blocked\":1"));
        assert!(json.contains("\"host\":\"blocked://ads.example\",\"requests\":1,\"hits\":0,\"misses\":0,\"bypass\":0,\"errors\":0,\"blocked\":1"));
    }

    #[test]
    fn test_host_stats() {
        let m = Metrics::new();
        m.record_host(
            "http://a:80",
            HostOutcome::from_access("HIT(memory) age=1s", 200),
            10,
        );
        m.record_host(
            "http://a:80",
            HostOutcome::from_access("MISS stored ttl=1s", 200),
            20,
        );
        m.record_host("http://b:80", HostOutcome::from_access("BYPASS", 200), 5);
        m.record_host("http://b:80", HostOutcome::from_access("MISS", 502), 0);
        let hosts = m.hosts_sorted();
        assert_eq!(hosts[0].0, "http://a:80");
        assert!(hosts[0].1.last_seen > 0);
        assert_eq!(
            HostStats {
                last_seen: 0,
                ..hosts[0].1.clone()
            },
            HostStats {
                requests: 2,
                hits: 1,
                misses: 1,
                bypass: 0,
                errors: 0,
                bytes: 30,
                ..Default::default()
            }
        );
        assert_eq!(hosts[1].1.errors, 1);
        assert_eq!(hosts[1].1.bypass, 1);
        let json = m.to_json();
        assert!(
            json.contains("\"hosts\":[{\"host\":\"http://a:80\",\"requests\":2"),
            "{}",
            json
        );
        for i in 0..(MAX_HOSTS + 5) {
            m.record_host(&format!("http://h{}:80", i), HostOutcome::Hit, 1);
        }
        let hosts = m.hosts_sorted();
        assert!(hosts.len() <= MAX_HOSTS + 1);
        assert!(hosts.iter().any(|(h, _)| h == "other"));
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;

    #[test]
    fn quantiles_interpolate_within_buckets() {
        let m = Metrics::new();
        for ms in [5, 20, 40, 80, 200, 400, 800, 2000, 4000, 9000] {
            m.record_host_timed(
                "http://a:80",
                HostOutcome::Miss,
                0,
                Duration::from_millis(ms),
            );
        }
        let (_, s) = &m.hosts_sorted()[0];
        assert_eq!(s.timed, 10);
        assert_eq!(s.duration_ms_max, 9000);
        assert!((s.avg_ms() - 1654.5).abs() < 0.01);
        // 24 段では 1 件ずつ別の区間に入る: p50 は 200 ms の入る (140, 210] の上端
        assert!(
            (s.quantile_ms(0.5) - 210.0).abs() < 1e-6,
            "{}",
            s.quantile_ms(0.5)
        );
        let p95 = s.quantile_ms(0.95);
        assert!(p95 > 5000.0 && p95 <= 9000.0, "{}", p95);
        assert_eq!(s.quantile_ms(1.0), 9000.0);
        assert_eq!(HostStats::default().quantile_ms(0.5), 0.0);
    }

    /// T12.4 (1) の受け入れ基準: **区間が 10 段だった頃は 257 ms が (250, 500] に全部入り、
    /// 補間しても観測した最大値で頭打ちになって p50 = p95 = max = 290 になっていた**。
    /// 24 段では 257 ms と 290 ms が (210, 315] に入り、区間内の補間が意味を持つ。
    #[test]
    fn the_deployed_shape_of_257ms_gets_a_real_median() {
        let m = Metrics::new();
        for _ in 0..90 {
            m.record_host_timed(
                "connect://www.dlsite.com:443",
                HostOutcome::Bypass,
                0,
                Duration::from_millis(257),
            );
        }
        for _ in 0..10 {
            m.record_host_timed(
                "connect://www.dlsite.com:443",
                HostOutcome::Bypass,
                0,
                Duration::from_millis(290),
            );
        }
        let (_, s) = &m.hosts_sorted()[0];
        let (p50, p95) = (s.quantile_ms(0.5), s.quantile_ms(0.95));
        assert!((250.0..=265.0).contains(&p50), "p50 {}", p50);
        assert!(p95 >= 280.0, "p95 {}", p95);
        assert!(p50 < p95, "p50 {} p95 {}", p50, p95);
    }

    /// 同じく T12.4 (1): AAAA の無いホスト (デプロイ先の p50 5.1 ms) が潰れないこと。
    #[test]
    fn a_five_millisecond_host_keeps_a_five_millisecond_median() {
        let m = Metrics::new();
        for _ in 0..100 {
            m.record_host_timed(
                "connect://discord.com:443",
                HostOutcome::Bypass,
                0,
                Duration::from_millis(5),
            );
        }
        let (_, s) = &m.hosts_sorted()[0];
        let p50 = s.quantile_ms(0.5);
        assert!((4.0..=6.0).contains(&p50), "p50 {}", p50);
    }

    /// 区間は 1 ms から 10 s まで単調増加で、公比はおよそ 1.5。
    #[test]
    fn the_bounds_are_a_geometric_ladder() {
        assert_eq!(LATENCY_BOUNDS_MS.len(), 24);
        assert_eq!(LATENCY_BOUNDS_MS[0], 1);
        assert_eq!(LATENCY_BOUNDS_MS[23], 10_000);
        for w in LATENCY_BOUNDS_MS.windows(2) {
            assert!(w[0] < w[1], "{:?}", w);
        }
        // 5 ms より上は公比 1.33〜1.55 に収まっている (下の端はもっと細かい)
        for w in LATENCY_BOUNDS_MS.windows(2).skip(4) {
            let r = w[1] as f64 / w[0] as f64;
            assert!((1.3..=1.6).contains(&r), "{:?} -> {}", w, r);
        }
    }

    /// `io::Error` から 8 つの原因へ畳めること (T12.4 (2))。
    #[test]
    fn io_errors_fold_into_eight_causes() {
        use std::io::{Error, ErrorKind};
        let c = |k: ErrorKind| ErrCause::from_io(&Error::new(k, "x"));
        assert_eq!(c(ErrorKind::ConnectionRefused), ErrCause::Refused);
        assert_eq!(c(ErrorKind::NetworkUnreachable), ErrCause::Unreachable);
        assert_eq!(c(ErrorKind::HostUnreachable), ErrCause::Unreachable);
        assert_eq!(c(ErrorKind::TimedOut), ErrCause::Timeout);
        assert_eq!(c(ErrorKind::ConnectionReset), ErrCause::Reset);
        assert_eq!(c(ErrorKind::BrokenPipe), ErrCause::Reset);
        assert_eq!(c(ErrorKind::NotFound), ErrCause::Dns);
        assert_eq!(c(ErrorKind::PermissionDenied), ErrCause::Other);
        // `getaddrinfo` の失敗は種別が付かないことがあるので文言も見る
        assert_eq!(
            ErrCause::from_io(&Error::other("failed to lookup address information: x")),
            ErrCause::Dns
        );
        assert_eq!(
            ErrCause::from_io(&Error::other("TLS handshake failed")),
            ErrCause::Tls
        );
        assert_eq!(ErrCause::Loop.name(), "loop");
        assert_eq!(ERR_CAUSE_NAMES.len(), ERR_CAUSES);
    }

    /// 内訳はホスト別の行と区間の合計の両方に乗る (T12.4 (2) / (3))。
    #[test]
    fn the_breakdown_lands_on_the_host_row_and_the_interval() {
        let m = Metrics::new();
        m.record_host_detail(
            "connect://a:443",
            HostOutcome::Bypass,
            0,
            Some(Duration::from_millis(257)),
            Detail {
                dns_ms: 12,
                dns_misses: 1,
                connect_ms: 245,
                family_v6: Some(false),
                cause: None,
                first_byte_ms: None,
            },
        );
        m.record_host_detail(
            "connect://a:443",
            HostOutcome::Error,
            0,
            Some(Duration::from_millis(300)),
            Detail {
                family_v6: Some(true),
                cause: Some(ErrCause::Refused),
                ..Detail::default()
            },
        );
        let (host, s) = &m.hosts_sorted()[0];
        assert_eq!(host, "connect://a:443");
        assert_eq!((s.dns_ms_sum, s.dns_misses, s.connect_ms_sum), (12, 1, 245));
        assert_eq!((s.v4_wins, s.v6_wins), (1, 1));
        assert_eq!(s.errors_by_cause[ErrCause::Refused as usize], 1);
        // `connect://` の鍵は CONNECT の窓へ入る
        let iv = m.totals();
        assert_eq!(iv.connect.count, 2);
        assert_eq!(iv.forward.count, 0);
        assert_eq!(iv.errors, 1);
        assert_eq!(iv.errors_by_cause[ErrCause::Refused as usize], 1);
        assert_eq!((iv.dns_misses, iv.dns_ms_sum), (1, 12));
        // 区間は読むと 0 に戻る
        assert_eq!(m.take_interval().connect.count, 2);
        assert_eq!(m.take_interval().connect.count, 0);
        assert_eq!(m.totals().connect.count, 2, "累計は残る");
        // forward は初バイトの値が窓に入る (応答全体の時間ではない)
        m.record_host_detail(
            "http://b:80",
            HostOutcome::Miss,
            0,
            Some(Duration::from_millis(900)),
            Detail {
                first_byte_ms: Some(7),
                ..Detail::default()
            },
        );
        let iv = m.take_interval();
        assert_eq!(iv.forward.count, 1);
        assert_eq!(iv.forward.ms_max, 7);
        // ホスト別の応答時間はこれまでどおり応答全体
        let b = m
            .hosts_sorted()
            .into_iter()
            .find(|(h, _)| h == "http://b:80")
            .unwrap()
            .1;
        assert_eq!(b.duration_ms_max, 900);
        // `/status` にはホスト別だけ内訳が出る (接続元別には出ない)
        let json = m.to_json();
        assert!(json.contains("\"v6_wins\":1"), "{}", json);
        assert!(json.contains("\"errors_by_cause\":["), "{}", json);
    }

    /// `/status?sort=` が上位を切り出す鍵だけを変えること (T13.3)。
    ///
    /// 鍵ごとに先頭が入れ替わり、**JSON の形は変わらない** (件数もキーもそのまま)。
    #[test]
    fn the_sort_key_only_changes_which_hosts_are_cut_out() {
        let m = Metrics::new();
        // 要求は多いが健全なホスト
        for _ in 0..10 {
            m.record_host_detail(
                "connect://busy:443",
                HostOutcome::Bypass,
                0,
                Some(Duration::from_millis(5)),
                Detail::default(),
            );
        }
        // 名前解決に時間を払っているホスト (要求は 2 件)
        for _ in 0..2 {
            m.record_host_detail(
                "connect://slow-dns:443",
                HostOutcome::Bypass,
                0,
                Some(Duration::from_millis(60)),
                Detail {
                    dns_ms: 50,
                    dns_misses: 1,
                    connect_ms: 10,
                    ..Detail::default()
                },
            );
        }
        // エラーだけのホスト (要求 1 件)
        m.record_host_detail(
            "connect://broken:443",
            HostOutcome::Error,
            0,
            None,
            Detail {
                cause: Some(ErrCause::Dns),
                dns_misses: 1,
                ..Detail::default()
            },
        );
        // 遠いホスト (1 件だけだが平均が飛び抜けて遅い)
        m.record_host_detail(
            "connect://far-away:443",
            HostOutcome::Bypass,
            0,
            Some(Duration::from_millis(900)),
            Detail {
                connect_ms: 900,
                ..Detail::default()
            },
        );
        let first = |sort| m.hosts_sorted_by(sort)[0].0.clone();
        assert_eq!(first(HostSort::Requests), "connect://busy:443");
        assert_eq!(first(HostSort::Errors), "connect://broken:443");
        assert_eq!(first(HostSort::Dns), "connect://slow-dns:443");
        assert_eq!(first(HostSort::Slow), "connect://far-away:443");
        // 知らない値と空は既定 (要求数順) に倒れる
        for v in ["", "requests", "REQUESTS", "errors?", "なにか"] {
            assert_eq!(
                m.hosts_sorted_by(HostSort::from_param(v))[0].0,
                "connect://busy:443",
                "{}",
                v
            );
        }
        // 同点は要求数 → 名前で崩すので、どの鍵でも順序は 1 つに決まる
        assert_eq!(
            m.hosts_sorted_by(HostSort::Errors),
            m.hosts_sorted_by(HostSort::Errors)
        );
        // JSON の形は鍵で変わらない (件数もキーも同じ。先頭のホストだけが違う)
        let of = |sort| {
            m.to_json_with_cache(
                None,
                StatusExtras {
                    sort,
                    ..StatusExtras::default()
                },
            )
        };
        let (a, b) = (of(HostSort::Requests), of(HostSort::Errors));
        assert_eq!(a.matches("\"host\":").count(), 4);
        assert_eq!(
            a.matches("\"host\":").count(),
            b.matches("\"host\":").count()
        );
        assert_eq!(
            a.matches("\"errors_by_cause\":[").count(),
            b.matches("\"errors_by_cause\":[").count()
        );
        assert!(
            a.contains("\"hosts\":[{\"host\":\"connect://busy:443\""),
            "{}",
            a
        );
        assert!(
            b.contains("\"hosts\":[{\"host\":\"connect://broken:443\""),
            "{}",
            b
        );
    }

    /// `/status` の応答は上位 50 ホスト + 上位 50 接続元でも 64 KiB に収まること (T13.3)。
    ///
    /// 監視が 5 秒ごとに引く口なので、太らせない。長い名前 (RFC の上限に近い 200 バイト) を
    /// 並べた最悪に近い形で測る。
    #[test]
    fn the_status_json_stays_under_64_kib() {
        let m = Metrics::new();
        for i in 0..200 {
            let host = format!("connect://{}{:03}.example.net:443", "n".repeat(180), i);
            m.record_host_detail(
                &host,
                HostOutcome::Error,
                u64::MAX / 2,
                Some(Duration::from_millis(1234)),
                Detail {
                    dns_ms: 9999,
                    dns_misses: 7,
                    connect_ms: 8888,
                    family_v6: Some(true),
                    cause: Some(ErrCause::Dns),
                    first_byte_ms: None,
                },
            );
            m.record_client(
                &format!("2001:db8:{:04x}:{:04x}::{:04x}", i, i, i),
                HostOutcome::Error,
                u64::MAX / 2,
                Some(Duration::from_millis(1234)),
            );
        }
        for sort in [
            HostSort::Requests,
            HostSort::Errors,
            HostSort::Dns,
            HostSort::Slow,
        ] {
            let json = m.to_json_with_cache(
                None,
                StatusExtras {
                    sort,
                    ..StatusExtras::default()
                },
            );
            assert!(json.len() <= 64 * 1024, "{} バイト", json.len());
        }
    }

    #[test]
    fn error_rate_counts_all_requests() {
        let m = Metrics::new();
        m.record_host("http://a:80", HostOutcome::Error, 0);
        m.record_host("http://a:80", HostOutcome::Hit, 0);
        m.record_host("http://a:80", HostOutcome::Hit, 0);
        m.record_host("http://a:80", HostOutcome::Hit, 0);
        let (_, s) = &m.hosts_sorted()[0];
        assert!((s.error_rate() - 0.25).abs() < 1e-9);
        assert_eq!(s.timed, 0);
    }
}
