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
//!   書かない (標本の余白は 4 B しか無い。T14.2)。失敗は `/errors` に `kind: "canary"`。
//! - **IPv6 側だけの 1 本** (`PROXY_CANARY_IPV6`、既定 `on`。T14.37): 同じ周期に、同じ名前の
//!   **AAAA へ 1 本**だけ繋いでみる。デプロイ先のコンテナは IPv6 が黒穴で、
//!   「生き返ったか」を 1 分の粒度で見るための観測を別に持つ。**勝敗も族の記憶も書かない**。
//!   繋がらなかったとき・AAAA が無い名前・IPv6 を切ってあるときは `null`。
//!   T17.7 からは**この 1 本が IPv6 の探りを引き受ける**: 試している間は [`crate::net`] が
//!   利用者の経路で 600 秒に 1 回 IPv6 を先頭に戻すのをやめ (`/status` の
//!   `ipv6.probe_by` が `"canary"`)、繋がったら `v4_first` を解く。

mod probe;

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::metrics::{ErrCause, Metrics};
use crate::sync::LockExt;

pub use probe::Probe;
use probe::probe;

/// 既定の周期 (秒)。`PROXY_CANARY_SECS` で短くできる (試験用。最小 1)。
pub const SECS: u64 = 60;
/// 1 回の試行の締め切り (名前解決と接続でそれぞれ)。
pub(crate) const DEADLINE: Duration = Duration::from_secs(5);
/// 相手に既定で繋ぐポート (`PROXY_CANARY=example.com` のようにポートを省いたとき)。
pub(crate) const DEFAULT_PORT: u16 = 443;
// 窓 (`/history` の `canary` の列) は下の層 (`proxy-metrics-window`) に置いてある
// (T14.55。読むのは `history` の `/history` の組み立てで、そちらが下の層に居るため)。
// **今までの名前でここから引ける**。
pub use crate::canaryhist::{KEYS, RESOLUTIONS, push_history_json};
/// 手で並べられるホストの上限 (`PROXY_CANARY=a,b,...`)。1 周で繋ぐ本数の歯止め。
pub const MAX_HOSTS: usize = 8;
/// `auto` が「直近」と見なす窓 (秒)。この間に使われた CONNECT の宛先から選ぶ。
pub(crate) const AUTO_WINDOW_SECS: u64 = 3600;
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

/// いまの設定 (`PROXY_CANARY`)。
static MODE: Mutex<Mode> = Mutex::new(Mode::Auto);
/// 周期 (秒)。`PROXY_CANARY_SECS` (最小 1)。
static PERIOD_SECS: AtomicU64 = AtomicU64::new(SECS);
/// IPv6 側だけの 1 本を試すか (`PROXY_CANARY_IPV6`、既定 `on`。T14.37)。
static IPV6: AtomicBool = AtomicBool::new(true);
/// 最後の結果 (`/status` と `/metrics`)。
static LAST: Mutex<Option<Probe>> = Mutex::new(None);
/// 回した回数と、そのうち失敗した回数。
static RUNS: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);
/// IPv6 側の 1 本を**実際に試した**回数 (繋がっても繋がらなくても 1。T18.1)。
///
/// `ipv6_connect_ms` は負けても試さなくても `null` なので、「試したか」はここで読む。
/// 試した回だけが [`crate::net::note_canary_ipv6`] の印を更新する (= 利用者の経路の探りを止める)。
static IPV6_RUNS: AtomicU64 = AtomicU64::new(0);
/// IPv6 側を**試さなかった**回数 (T18.1)。1 回の試行 ([`probe`](fn@probe::probe)) につきどちらかが 1 増える。
///
/// 試さないのは: 名前が引けなかった、引けた答えに AAAA が無い、`PROXY_IPV6=off`、
/// `PROXY_CANARY_IPV6=off`。これが 10 分 (600 秒) ぶん続くと探りは利用者の経路に戻る。
/// **宛先が 1 つも無い周** (`off`、`auto` で直近 1 時間に CONNECT が無い) は `runs` ごと
/// 増えないので、ここにも入らない。
static IPV6_SKIPPED: AtomicU64 = AtomicU64::new(0);
/// `canary` スレッドへの送り口。**最初の [`tick`] で 1 本だけ**遅延起動する
/// (`off` のまま動くプロセスと `--lite` ではスレッドを作らない)。
static THREAD: OnceLock<Option<Sender<Msg>>> = OnceLock::new();

/// `canary` スレッドへの用件。
enum Msg {
    /// 設定が変わったかもしれない (待ち時間を計算し直す)
    Wake,
}

/// `/status` の `canary` を組む口を下の層 (`proxy-metrics-core` の `push_env`) に預ける
/// (T15.12。`kernel::set_state_file_errors` と同じ形)。
///
/// canary はこのクレートに居て、`/status` を組むのは 1 つ下の層なので下からは呼べない。
/// **預ける前は canary が `off` のときと 1 バイト同じ文字列**
/// ([`crate::metrics::CANARY_STATUS_UNSET`]) が出るので、預け忘れても鍵は変わらない。
/// 呼ぶのは canary に用がある 2 か所 ([`configure`] と [`tick`]) で、2 度目からは
/// 何もしない (`OnceLock`)。
fn install() {
    crate::metrics::set_canary_status(status_json);
}

/// `PROXY_CANARY` / `PROXY_CANARY_SECS` / `PROXY_CANARY_IPV6` を当てる
/// (起動時と `.env` の再読込から)。
pub fn configure(spec: &str, period: Duration, ipv6: bool) {
    install();
    *MODE.locked() = Mode::parse(spec);
    PERIOD_SECS.store(period.as_secs().max(1), Ordering::Relaxed);
    IPV6.store(ipv6, Ordering::Relaxed);
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

/// IPv6 側だけの 1 本を試すか (`PROXY_CANARY_IPV6`)。
pub fn ipv6_enabled() -> bool {
    IPV6.load(Ordering::Relaxed)
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
    install();
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
    auto_target_at(metrics, crate::cache::now_epoch())
}

/// [`auto_target`] の本体 (時刻を受ける。T34.45)。
fn auto_target_at(metrics: &Metrics, now: u64) -> Option<String> {
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

/// 窓に 1 行足す (下の層の環状バッファへ。T14.55)。
fn push_row(probe: &Probe) {
    crate::canaryhist::push(
        probe.at,
        probe.dns_ms,
        probe.connect_ms,
        &probe.host,
        probe.ipv6_connect_ms,
    );
}

/// `/status` の `"canary"` 要素 (組み立ては `/status` のときだけ)。
pub fn status_json() -> String {
    let mut out = String::with_capacity(224);
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
        None => out.push_str(
            "\"at\":0,\"host\":\"\",\"dns_ms\":0,\"connect_ms\":0,\"ipv6_connect_ms\":null,\"error\":null",
        ),
    }
    // IPv6 側の 1 本を試した回数と試さなかった回数 (通算。末尾に足すだけ。T18.1)
    let _ = write!(
        out,
        ",\"ipv6_runs\":{},\"ipv6_skipped\":{}}}",
        IPV6_RUNS.load(Ordering::Relaxed),
        IPV6_SKIPPED.load(Ordering::Relaxed),
    );
    out
}

/// 試験用: 窓と最後の結果と数を空に戻す (同じプロセスで 2 度目を測るため)。
#[cfg(test)]
fn reset() {
    *MODE.locked() = Mode::Auto;
    PERIOD_SECS.store(SECS, Ordering::Relaxed);
    IPV6.store(true, Ordering::Relaxed);
    *LAST.locked() = None;
    crate::canaryhist::clear();
    RUNS.store(0, Ordering::Relaxed);
    FAILURES.store(0, Ordering::Relaxed);
    IPV6_RUNS.store(0, Ordering::Relaxed);
    IPV6_SKIPPED.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests;
