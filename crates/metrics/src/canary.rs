//! canary — 利用者の要求が無い時間帯も「待ち」を測る (T14.10)。
//!
//! 平常時の CONNECT 確立 p50 (デプロイ先で 8.3 ms) は**利用者の要求があった時間帯だけ**の
//! 値で、深夜や利用者が居ない日は 1 点も無い。そのため「遅かったのはプロキシか、回線か、
//! 利用者の端末か」が切り分けられなかった (T14.0 の分析)。
//!
//! そこでプロキシ自身が [`SECS`] 秒に 1 回、**名前解決 → TCP 接続 → 即 `close`** だけを
//! 行い、その時間を残す。TLS も HTTP も送らないので、相手に届くのは 1 分に 1 回の
//! 名前解決と SYN / FIN だけ。
//!
//! - 回すのは **`canary` スレッド 1 本**だけ。接続 (利用者) のスレッドは 1 命令も通らない。
//!   起こすのは履歴スレッドの周期からの [`tick`] で、**履歴スレッドは待たない**
//!   (5 秒の標本の周期を canary の締め切り 5 秒が止めてはいけない)。
//! - 名前解決は [`crate::dns::resolve_uncached`] = **表を通さない**
//!   (リゾルバの実力を測るもので、表に書くと利用者の当たり外れが読めなくなる)。
//! - TCP 接続は [`crate::net::connect_resolved`] で、Happy Eyeballs と IPv4 優先の学習
//!   (T12.1) はそのまま。**同じ道で測らなければ利用者の値と比べられない。**
//! - 結果は `/status` の `canary` (最後の 1 回) と、**メモリ上の窓** (5 秒 × 720 /
//!   60 秒 × 1,440) に持ち、`/history` に `"canary"` の配列として出す。`.rrd` の標本には
//!   書かない (標本の余白は 4 B しか無い。T14.2 (3))。失敗は `/errors` に `kind: "canary"`。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::metrics::{ErrCause, Metrics};
use crate::sync::LockExt;

/// 既定の周期 (秒)。`PROXY_CANARY_SECS` で短くできる (試験用。最小 1)。
pub const SECS: u64 = 60;
/// 1 回の試行の締め切り (名前解決と接続でそれぞれ)。
pub const DEADLINE: Duration = Duration::from_secs(5);
/// 相手に既定で繋ぐポート (`PROXY_CANARY=example.com` のようにポートを省いたとき)。
pub const DEFAULT_PORT: u16 = 443;
/// 窓の解像度と本数。**履歴 ([`crate::history::RESOLUTIONS`]) の 5 秒と 1 分に揃える**
/// (`/history` の標本と同じ `t` で並ぶので、利用者の確立時間と重ねて読める)。
pub const RESOLUTIONS: [(u64, usize); 2] = [(5, 720), (60, 1440)];
/// 手で並べられるホストの上限 (`PROXY_CANARY=a,b,...`)。1 周で繋ぐ本数の歯止め。
pub const MAX_HOSTS: usize = 8;
/// `auto` が「直近」と見なす窓 (秒)。この間に使われた CONNECT の宛先から選ぶ。
pub const AUTO_WINDOW_SECS: u64 = 3600;
/// `auto` が選ぶ宛先の印 (ホスト別統計の鍵。`crates/tunnel` が付けている)。
const CONNECT_PREFIX: &str = "connect://";
/// 失敗の理由を残す長さ (バイト)。
const MAX_ERROR: usize = 80;

/// `PROXY_CANARY` の設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// 回さない
    Off,
    /// 直近 [`AUTO_WINDOW_SECS`] 秒で最も要求の多い CONNECT の宛先を毎周期選び直す
    Auto,
    /// 手で並べた宛先 (`host:port`。全部に繋ぐ)
    Hosts(Vec<String>),
}

impl Mode {
    /// `auto` / `off` / `host1,host2` を読む。空や空白だけは `auto` (既定)。
    pub fn parse(spec: &str) -> Mode {
        let spec = spec.trim();
        if spec.is_empty() || spec.eq_ignore_ascii_case("auto") {
            return Mode::Auto;
        }
        if spec.eq_ignore_ascii_case("off")
            || spec.eq_ignore_ascii_case("none")
            || spec == "0"
            || spec.eq_ignore_ascii_case("false")
        {
            return Mode::Off;
        }
        let hosts: Vec<String> = spec
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .take(MAX_HOSTS)
            .map(|h| crate::net::with_default_port(&h, DEFAULT_PORT))
            .collect();
        if hosts.is_empty() {
            Mode::Off
        } else {
            Mode::Hosts(hosts)
        }
    }

    /// `/status` に出す名前。
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Auto => "auto",
            Mode::Hosts(_) => "hosts",
        }
    }
}

/// 1 回の試行の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// いつ (epoch 秒)
    pub at: u64,
    /// 相手 (`host:port`)
    pub host: String,
    /// 名前解決にかかった ms (IP リテラルなら 0)
    pub dns_ms: u64,
    /// TCP 接続 (SYN → 確立) にかかった ms。名前解決で失敗したら 0
    pub connect_ms: u64,
    /// 失敗の理由 (成功なら `None`)
    pub error: Option<String>,
    /// 失敗の原因 (利用者のエラーと同じ物差し。成功なら `None`)
    pub cause: Option<ErrCause>,
}

impl Probe {
    /// `/status` の `canary` の中身 (波括弧なし)。
    fn push_json(&self, out: &mut String) {
        let _ = write!(
            out,
            "\"at\":{},\"host\":\"{}\",\"dns_ms\":{},\"connect_ms\":{},\"error\":",
            self.at,
            crate::json::escape(&self.host),
            self.dns_ms,
            self.connect_ms
        );
        match &self.error {
            Some(e) => {
                let _ = write!(out, "\"{}\"", crate::json::escape(e));
            }
            None => out.push_str("null"),
        }
    }
}

/// 窓の 1 行 (`/history` の `canary` の 1 標本)。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    t: u64,
    dns_ms: u64,
    connect_ms: u64,
    host: String,
}

/// `/history` の `canary` の列名 (この順で並ぶ)。
pub const KEYS: [&str; 4] = ["t", "canary_dns_ms", "canary_connect_ms", "canary_host"];

/// いまの設定 (`PROXY_CANARY`)。
static MODE: Mutex<Mode> = Mutex::new(Mode::Auto);
/// 周期 (秒)。`PROXY_CANARY_SECS` (最小 1)。
static PERIOD_SECS: AtomicU64 = AtomicU64::new(SECS);
/// 最後の結果 (`/status` と `/metrics`)。
static LAST: Mutex<Option<Probe>> = Mutex::new(None);
/// メモリ上の窓 (5 秒 × 720 / 60 秒 × 1,440)。**`.rrd` には書かない。**
static RINGS: Mutex<Option<[VecDeque<Row>; RESOLUTIONS.len()]>> = Mutex::new(None);
/// 回した回数と、そのうち失敗した回数。
static RUNS: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);
/// `canary` スレッドへの送り口。**最初の [`tick`] で 1 本だけ**遅延起動する
/// (`off` のまま動くプロセスと `--lite` ではスレッドを作らない)。
static THREAD: OnceLock<Option<Sender<Msg>>> = OnceLock::new();

/// `canary` スレッドへの用件。
enum Msg {
    /// 設定が変わったかもしれない (待ち時間を計算し直す)
    Wake,
}

/// `PROXY_CANARY` と `PROXY_CANARY_SECS` を当てる (起動時と `.env` の再読込から)。
pub fn configure(spec: &str, period: Duration) {
    *MODE.locked() = Mode::parse(spec);
    PERIOD_SECS.store(period.as_secs().max(1), Ordering::Relaxed);
    // 既にスレッドが居れば、次の待ち時間を計算し直させる (`.env` で即時反映)
    if let Some(Some(tx)) = THREAD.get() {
        let _ = tx.send(Msg::Wake);
    }
}

/// いまの設定。
pub fn mode() -> Mode {
    MODE.locked().clone()
}

/// いまの周期。
pub fn period() -> Duration {
    Duration::from_secs(PERIOD_SECS.load(Ordering::Relaxed).max(1))
}

/// 最後の結果 (`/metrics` が読む)。
pub fn last() -> Option<Probe> {
    LAST.locked().clone()
}

/// **履歴スレッドの周期から 1 回だけ呼ぶ** (T14.10)。
///
/// ここでするのは「`canary` スレッドが居なければ起こす」「設定が変わったことを伝える」
/// だけで、**名前解決も接続もしない** (5 秒の標本の周期を 5 秒の締め切りで止めない)。
/// `off` のときはスレッドも作らない。
pub fn tick(metrics: &Arc<Metrics>) {
    if matches!(*MODE.locked(), Mode::Off) && THREAD.get().is_none() {
        return;
    }
    let tx = THREAD.get_or_init(|| spawn(Arc::clone(metrics)));
    if let Some(tx) = tx {
        let _ = tx.send(Msg::Wake);
    }
}

/// `canary` スレッドを 1 本起こす (起こせなければ `None` = 以後 canary は回らない)。
fn spawn(metrics: Arc<Metrics>) -> Option<Sender<Msg>> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let started = thread::Builder::new()
        .name("canary".into())
        .spawn(move || canary_loop(&rx, &metrics));
    match started {
        Ok(_) => Some(tx),
        Err(e) => {
            crate::log_warn!(None, "canary: cannot spawn its thread: {}", e);
            None
        }
    }
}

/// `canary` スレッドの本体。**周期の眠りは `recv_timeout`** なので、設定が変わったら
/// すぐ起きて計算し直す (`dns-refresh` と同じ形。T14.1)。
fn canary_loop(rx: &mpsc::Receiver<Msg>, metrics: &Metrics) {
    // 起こされた直後に 1 回測る (再起動のあと 1 分待たない)
    let mut next = Instant::now();
    loop {
        let wait = next.saturating_duration_since(Instant::now());
        if wait.is_zero() {
            run_round(metrics);
            next = Instant::now() + period();
            continue;
        }
        match rx.recv_timeout(wait) {
            // 設定が変わったかもしれない: 縮んだ周期をその場で当てる
            Ok(Msg::Wake) => next = next.min(Instant::now() + period()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// 1 周ぶん (対象を選び、1 ホストずつ測って残す)。
fn run_round(metrics: &Metrics) {
    for target in targets(metrics) {
        let probe = probe(&target);
        record(&probe, metrics);
    }
}

/// この周期で測る宛先 (`off` と、`auto` で選べるものが無いときは空)。
fn targets(metrics: &Metrics) -> Vec<String> {
    match &*MODE.locked() {
        Mode::Off => Vec::new(),
        Mode::Hosts(hosts) => hosts.clone(),
        // `auto` は 1 周期ごとに選び直す (相手が変われば次の 60 秒から新しい相手)
        Mode::Auto => auto_target(metrics).into_iter().collect(),
    }
}

/// `auto` の宛先: 直近 [`AUTO_WINDOW_SECS`] 秒に使われた **CONNECT の宛先**のうち、
/// 要求の最も多いもの (`/status` の上位ホストの先頭)。1 件も無ければ `None` = 何もしない。
fn auto_target(metrics: &Metrics) -> Option<String> {
    let now = crate::cache::now_epoch();
    metrics
        .hosts_sorted_by(crate::metrics::HostSort::Requests)
        .into_iter()
        .find(|(host, stats)| {
            host.starts_with(CONNECT_PREFIX)
                && stats.requests > 0
                && stats.last_seen > 0
                && now.saturating_sub(stats.last_seen) <= AUTO_WINDOW_SECS
        })
        .map(|(host, _)| host[CONNECT_PREFIX.len()..].to_string())
}

/// 1 ホストを測る: **名前解決 (表を通さない) → TCP 接続 → 即 `close`**。
fn probe(target: &str) -> Probe {
    let at = crate::cache::now_epoch();
    let (host, port) = crate::net::split_host_port_ref(target);
    let port = port.unwrap_or(DEFAULT_PORT);
    let started = Instant::now();
    let addrs = match crate::dns::resolve_uncached(host, port) {
        Ok(a) => a,
        Err(e) => {
            return Probe {
                at,
                host: target.to_string(),
                dns_ms: ms(started.elapsed()),
                connect_ms: 0,
                error: Some(clip(&e.to_string())),
                // 名前解決で終わったのだから原因は `dns` (文言から当てない)
                cause: Some(ErrCause::Dns),
            };
        }
    };
    let dns_ms = ms(started.elapsed());
    // IPv6 を切ってあるときは利用者の経路と同じく A レコードだけを試す
    let addrs: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|ip| crate::net::ipv6_enabled() || ip.is_ipv4())
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    if addrs.is_empty() {
        return Probe {
            at,
            host: target.to_string(),
            dns_ms,
            connect_ms: 0,
            error: Some("no address for this family".to_string()),
            cause: Some(ErrCause::Dns),
        };
    }
    let started = Instant::now();
    // 握れたら**その場で捨てる** (`drop` = FIN)。TLS も HTTP も送らない
    let (error, cause) = match crate::net::connect_resolved(host, addrs, DEADLINE) {
        Ok(_stream) => (None, None),
        // 原因は `io::Error` から決める (利用者のエラーと同じ `refused` / `timeout` …)
        Err(e) => (Some(clip(&e.to_string())), Some(ErrCause::from_io(&e))),
    };
    Probe {
        at,
        host: target.to_string(),
        dns_ms,
        connect_ms: ms(started.elapsed()),
        error,
        cause,
    }
}

/// 結果を残す: 最後の 1 回・窓・(失敗なら) `/errors` の個票 1 件。
fn record(probe: &Probe, metrics: &Metrics) {
    RUNS.fetch_add(1, Ordering::Relaxed);
    if let Some(e) = &probe.error {
        FAILURES.fetch_add(1, Ordering::Relaxed);
        // 原因は利用者のエラーと同じ物差し (`dns` / `refused` / `timeout` …)。
        // **集計 (`errors_by_cause`) には足さない** (canary は利用者の要求ではない)
        let cause = probe.cause.unwrap_or(ErrCause::Other);
        metrics.record_canary_error(&probe.host, cause, probe.dns_ms, probe.connect_ms);
        crate::log_warn!(
            None,
            "canary: {} failed after dns {} ms + connect {} ms: {}",
            probe.host,
            probe.dns_ms,
            probe.connect_ms,
            e
        );
    }
    push_row(probe);
    *LAST.locked() = Some(probe.clone());
}

/// 窓に 1 行足す (同じ窓に 2 回入ったら**新しい方で置き換える**)。
fn push_row(probe: &Probe) {
    let mut guard = RINGS.locked();
    let rings = guard.get_or_insert_with(Default::default);
    for (ring, (step, cap)) in rings.iter_mut().zip(RESOLUTIONS) {
        let t = (probe.at / step) * step;
        let row = Row {
            t,
            dns_ms: probe.dns_ms,
            connect_ms: probe.connect_ms,
            host: probe.host.clone(),
        };
        match ring.back_mut() {
            Some(back) if back.t == t => *back = row,
            _ => {
                if ring.len() >= cap {
                    ring.pop_front();
                }
                ring.push_back(row);
            }
        }
    }
}

/// `/status` の `"canary"` 要素 (組み立ては `/status` のときだけ)。
pub fn status_json() -> String {
    let mut out = String::with_capacity(192);
    let _ = write!(
        out,
        "{{\"mode\":\"{}\",\"secs\":{},\"runs\":{},\"failures\":{},",
        mode().name(),
        PERIOD_SECS.load(Ordering::Relaxed),
        RUNS.load(Ordering::Relaxed),
        FAILURES.load(Ordering::Relaxed),
    );
    match last() {
        Some(p) => p.push_json(&mut out),
        // まだ 1 回も回っていない (`off`、起動直後、履歴スレッドが無い)
        None => out.push_str("\"at\":0,\"host\":\"\",\"dns_ms\":0,\"connect_ms\":0,\"error\":null"),
    }
    out.push('}');
    out
}

/// `/history` の応答に `,"canary":{"keys":[...],"samples":[[...]]}` を足す (T14.10)。
///
/// **既存の `keys` / `samples` の形は変えない** (読む側を壊さないため、別の配列にする)。
/// `res` は [`crate::history::RESOLUTIONS`] の添字で、1 時間の解像度 (2) には窓を
/// 持たないので空の配列を出す。
pub fn push_history_json(out: &mut String, res: usize) {
    out.push_str(",\"canary\":{\"keys\":[");
    for (i, k) in KEYS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", k);
    }
    out.push_str("],\"samples\":[");
    let guard = RINGS.locked();
    if let Some(ring) = guard.as_ref().and_then(|rings| rings.get(res)) {
        for (i, r) in ring.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "[{},{},{},\"{}\"]",
                r.t,
                r.dns_ms,
                r.connect_ms,
                crate::json::escape(&r.host)
            );
        }
    }
    drop(guard);
    out.push_str("]}");
}

/// ms に丸める (0.5 ms 以上は 1 ms。手元の loopback は 0 ms になる)。
fn ms(d: Duration) -> u64 {
    (d.as_micros() as u64 + 500) / 1000
}

fn clip(s: &str) -> String {
    crate::recent::clip(s, MAX_ERROR)
}

/// 試験用: 窓と最後の結果と数を空に戻す (同じプロセスで 2 度目を測るため)。
#[cfg(test)]
fn reset() {
    *MODE.locked() = Mode::Auto;
    PERIOD_SECS.store(SECS, Ordering::Relaxed);
    *LAST.locked() = None;
    *RINGS.locked() = None;
    RUNS.store(0, Ordering::Relaxed);
    FAILURES.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{Detail, HostOutcome};
    use std::net::TcpListener;

    /// この単体テストたちは**同じプロセスの静的な状態**を触るので直列にする。
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn the_setting_is_read_as_auto_off_or_a_list_of_hosts() {
        assert_eq!(Mode::parse("auto"), Mode::Auto);
        assert_eq!(Mode::parse(" AUTO "), Mode::Auto);
        assert_eq!(Mode::parse(""), Mode::Auto);
        assert_eq!(Mode::parse("off"), Mode::Off);
        assert_eq!(Mode::parse("OFF"), Mode::Off);
        // ポートを省いたら 443、書いてあればそのまま。大小は潰す
        assert_eq!(
            Mode::parse("Example.com, b.example.net:8443"),
            Mode::Hosts(vec![
                "example.com:443".to_string(),
                "b.example.net:8443".to_string()
            ])
        );
        // 並べられるのは MAX_HOSTS まで
        let many = (0..20)
            .map(|i| format!("h{}.example.net", i))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            match Mode::parse(&many) {
                Mode::Hosts(h) => h.len(),
                _ => 0,
            },
            MAX_HOSTS
        );
        // 区切りだけ書いたら回さない
        assert_eq!(Mode::parse(",, ,"), Mode::Off);
    }

    #[test]
    fn auto_picks_the_busiest_connect_target_seen_in_the_last_hour() {
        let _s = SERIAL.locked();
        let m = Metrics::new();
        // forward の宛先は選ばない (canary は CONNECT の確立を測るもの)
        m.record_host("http://a.example.net:80", HostOutcome::Miss, 10);
        m.record_host("http://a.example.net:80", HostOutcome::Miss, 10);
        m.record_host("http://a.example.net:80", HostOutcome::Miss, 10);
        m.record_host_detail(
            "connect://b.example.net:443",
            HostOutcome::Bypass,
            1,
            None,
            Detail::default(),
        );
        m.record_host_detail(
            "connect://c.example.net:443",
            HostOutcome::Bypass,
            1,
            None,
            Detail::default(),
        );
        m.record_host_detail(
            "connect://c.example.net:443",
            HostOutcome::Bypass,
            1,
            None,
            Detail::default(),
        );
        assert_eq!(
            auto_target(&m).as_deref(),
            Some("c.example.net:443"),
            "要求の多い CONNECT の宛先が選ばれる"
        );
        // 何も通っていないプロキシでは選ばない (= 何もしない)
        assert_eq!(auto_target(&Metrics::new()), None);
    }

    #[test]
    fn a_probe_measures_a_real_connect_and_a_refused_one() {
        let _s = SERIAL.locked();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let ok = probe(&format!("127.0.0.1:{}", port));
        assert_eq!(ok.error, None, "{:?}", ok);
        assert_eq!(ok.host, format!("127.0.0.1:{}", port));
        assert!(ok.at > 1_700_000_000, "{:?}", ok);
        // IP リテラルは名前解決が要らない (表も通らない)
        assert_eq!(ok.dns_ms, 0, "{:?}", ok);

        drop(listener);
        let dead = probe(&format!("127.0.0.1:{}", port));
        assert!(dead.error.is_some(), "{:?}", dead);
    }

    #[test]
    fn the_windows_keep_one_row_per_slot_and_the_json_has_its_own_array() {
        let _s = SERIAL.locked();
        reset();
        let sample = |at: u64, dns: u64, conn: u64| Probe {
            at,
            host: "a.example.net:443".to_string(),
            dns_ms: dns,
            connect_ms: conn,
            error: None,
            cause: None,
        };
        // 同じ 5 秒の窓に 2 回入ったら新しい方だけが残り、1 分の窓もひとつ
        push_row(&sample(1_000_000, 3, 8));
        push_row(&sample(1_000_002, 4, 9));
        // 次の 5 秒の窓 (1 分の窓は同じ)
        push_row(&sample(1_000_007, 5, 10));
        let mut fine = String::new();
        push_history_json(&mut fine, 0);
        assert!(
            fine.starts_with(",\"canary\":{\"keys\":[\"t\",\"canary_dns_ms\",\"canary_connect_ms\",\"canary_host\"],\"samples\":[["),
            "{}",
            fine
        );
        assert!(
            fine.contains("[1000000,4,9,\"a.example.net:443\"],[1000005,5,10,"),
            "5 秒の窓は 2 行で、同じ窓は上書き: {}",
            fine
        );
        let mut minute = String::new();
        push_history_json(&mut minute, 1);
        assert_eq!(
            minute.matches("a.example.net").count(),
            1,
            "1 分の窓は 1 行: {}",
            minute
        );
        assert!(minute.contains("[999960,5,10,"), "{}", minute);
        // 1 時間の解像度は窓を持たない (空の配列)
        let mut hour = String::new();
        push_history_json(&mut hour, 2);
        assert!(hour.ends_with("\"samples\":[]}"), "{}", hour);
        reset();
    }

    #[test]
    fn the_status_json_has_the_last_result_and_a_failure_lands_in_the_error_ring() {
        let _s = SERIAL.locked();
        reset();
        let m = Metrics::new();
        // まだ 1 回も回っていない
        let empty = status_json();
        assert!(empty.contains("\"mode\":\"auto\""), "{}", empty);
        assert!(empty.contains("\"runs\":0"), "{}", empty);
        assert!(empty.contains("\"at\":0"), "{}", empty);

        record(
            &Probe {
                at: 1_789_000_000,
                host: "a.example.net:443".to_string(),
                dns_ms: 7,
                connect_ms: 9,
                error: None,
                cause: None,
            },
            &m,
        );
        let json = status_json();
        assert!(json.contains("\"runs\":1"), "{}", json);
        assert!(json.contains("\"failures\":0"), "{}", json);
        assert!(json.contains("\"host\":\"a.example.net:443\""), "{}", json);
        assert!(json.contains("\"dns_ms\":7,\"connect_ms\":9"), "{}", json);
        assert!(json.contains("\"error\":null"), "{}", json);
        assert!(m.errors.is_empty(), "成功は個票に残さない");

        record(
            &Probe {
                at: 1_789_000_060,
                host: "a.example.net:443".to_string(),
                dns_ms: 12,
                connect_ms: 0,
                error: Some("Connection refused (os error 111)".to_string()),
                cause: Some(ErrCause::Refused),
            },
            &m,
        );
        let json = status_json();
        assert!(json.contains("\"failures\":1"), "{}", json);
        assert!(json.contains("\"error\":\"Connection"), "{}", json);
        let (entries, total) = m.errors.recent(10);
        assert_eq!(total, 1);
        assert_eq!(entries.len(), 1);
        let one = entries[0].to_json();
        assert!(one.contains("\"kind\":\"canary\""), "{}", one);
        assert!(one.contains("\"target\":\"a.example.net:443\""), "{}", one);
        assert!(one.contains("\"dns_ms\":12"), "{}", one);
        // 集計 (`/status` の errors_by_cause) には足さない
        assert_eq!(m.totals().errors_by_cause.iter().sum::<u64>(), 0);
        assert!(one.contains("\"cause\":\"refused\""), "{}", one);
        reset();
    }

    #[test]
    fn off_means_no_target_at_all() {
        let _s = SERIAL.locked();
        reset();
        let m = Metrics::new();
        m.record_host_detail(
            "connect://a.example.net:443",
            HostOutcome::Bypass,
            1,
            None,
            Detail::default(),
        );
        configure("off", Duration::from_secs(1));
        assert!(targets(&m).is_empty());
        configure("auto", Duration::from_secs(1));
        assert_eq!(targets(&m), vec!["a.example.net:443".to_string()]);
        configure("x.example.net,y.example.net:8443", Duration::from_secs(1));
        assert_eq!(
            targets(&m),
            vec![
                "x.example.net:443".to_string(),
                "y.example.net:8443".to_string()
            ]
        );
        // 周期は最小 1 秒
        configure("off", Duration::from_secs(0));
        assert_eq!(period(), Duration::from_secs(1));
        reset();
    }
}
