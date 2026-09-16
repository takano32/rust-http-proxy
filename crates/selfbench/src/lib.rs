//! 起動時の自己ベンチ (`PROXY_SELF_BENCH=on`、既定 `off`。T14.43)。
//!
//! §2 の CPU/要求 (41 us) は手元の big.LITTLE の **big コアの値**で、デプロイ先のコンテナの
//! CPU で何 us かは分からない。T14.3 の `cpu_per_request_us` は実トラフィックの値だが、
//! 0.015 req/s では 5 秒の窓に 0〜1 本しか入らず読めない。そこで、起動して待ち受けを開いた
//! 直後に **loopback だけで 3 秒** (内蔵の小さなオリジン → 自分の待ち受けへ forward 8 並列
//! 1.5 秒 + CONNECT 8 並列 1.5 秒) 回し、CPU/要求 と CPU/本 を測って `/status` の `self_bench`
//! に残す。**外へは 1 バイトも出さない** (相手は全部この プロセスの中の 127.0.0.1)。
//!
//! **本数に上限がある** ([`MAX_REQUESTS`] = 20,000 要求 / [`MAX_CONNECTS`] = 2,000 本)。
//! 秒数だけで止めると手元の big コアでは 88,000 要求・22,000 本まで行き、**`/recent` の
//! 個票 4,096 件が自分のぶんで埋まって TIME_WAIT が 44,000 残る**。上限に当たったら
//! そこで終わる (`note` に書く)。それでも自分のぶんが統計に混ざるのは困るので、
//! 回っている 3 秒だけ旗を立て ([`active`])、**自分で打った接続と要求を `/recent`・
//! `/connections`・`/hosts`・`/clients`・要求の合計から外す** ([`is_client`] / [`is_target`])。
//!
//! 測り方は §1 の `scripts/cpu-per-request.sh` と同じ「**プロセスの utime+stime の増分 ÷
//! 操作数**」だが、スクリプトと違って**打ち手 (負荷) と内蔵オリジンが同じプロセスの中にいる**
//! ので、そのぶんを引かないと 2 倍以上の数字になる。自己ベンチのスレッドは終わるときに
//! 自分の CPU を [`Charge`] で足しこみ、窓の増分からその合計を引くので、残るのは
//! **プロキシがした仕事だけ**になる。死んだスレッドの CPU もプロセスの CPU には残るので、
//! 引き算はスレッドが消えても合う。
//!
//! CPU を読むのは `/proc/<pid>/stat` ではなく `clock_gettime` の
//! `CLOCK_PROCESS_CPUTIME_ID` / `CLOCK_THREAD_CPUTIME_ID` (**同じ utime+stime を ns 刻みで**)。
//! `/proc` の値は 10 ms 刻みなので、1.5 秒の窓で 10 本前後のスレッドを引き算すると
//! 切り捨てが積もって数 % 動いてしまう (§1 の 10 秒の計測では無視できる誤差だが、ここでは効く)。
//!
//! `off` (既定) ではこのクレートの関数は 1 つも呼ばれない (`src/main.rs` の旗の分岐 1 回だけ)。
//!
//! **`crates/bench` から写したもの** (`crates/bench` は `default-members` の外なので本体から
//! 呼べない): 固定応答を返す keep-alive オリジン、すぐ閉じる sink、CONNECT を張って 200 を
//! 読むところ、応答を 1 つ読み切るところ。写したのはその 4 つだけで、ベンチ側の統計
//! (p50 / p99 / ops) は持たない (測るのは CPU/操作 だけ)。

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use proxy_base::clock;
use proxy_base::json;
use proxy_base::sync::LockExt;

/// 回す秒数 (forward と CONNECT に半分ずつ)。
pub const SECS: u64 = 3;

/// 並列数 (forward / CONNECT とも)。§1 の既定のベンチ (`--conc 8`) と揃えてある。
pub const CONC: usize = 8;

/// forward の要求数の上限 (T14.43)。**秒数より先にここへ当たったら早く終わる。**
///
/// 手元 (big コア) の 1.5 秒は 88,000 要求まで行くが、CPU/要求 を読むのに 88,000 本は
/// 要らない。要求が多いほど `/status` の合計と `/history` に自分のぶんが積もるだけなので、
/// **読める最小**で止める。
pub const MAX_REQUESTS: u64 = 20_000;

/// CONNECT の本数の上限。
///
/// 1 本ごとに TCP を 2 本 (クライアント側とオリジン側) 使い捨てるので、手元の 1.5 秒
/// (約 22,000 本) では **TIME_WAIT が約 44,000 残る**。2,000 本あれば CPU/本 は十分読めて、
/// 残る TIME_WAIT は約 4,000 で収まる。
pub const MAX_CONNECTS: u64 = 2_000;

/// 内蔵オリジンが返す本文の大きさ。§1 の既定 (`--body-bytes 1024`) と揃えてある。
const BODY_BYTES: usize = 1024;

/// 内蔵オリジンが「もう終わり」に気づくまでの待ち (読み取りタイムアウト)。
const ORIGIN_POLL: Duration = Duration::from_millis(50);

/// 1 回の自己ベンチの結果 (`/status` の `self_bench`)。
#[derive(Clone, Debug)]
pub struct Report {
    /// 測り終えた時刻 (epoch 秒)
    pub at: u64,
    /// forward の CPU/要求 (us)。1 本も通らなかった / CPU が読めないときは `None`
    pub forward_us: Option<f64>,
    /// CONNECT の CPU/本 (us)。同上
    pub connect_us: Option<f64>,
    /// この環境から見えるコア数 (`available_parallelism`)
    pub cores: usize,
    /// 200 が返った要求の数
    pub requests: u64,
    /// 確立できた CONNECT の本数
    pub connects: u64,
    /// 回す秒数の上限 (forward と CONNECT の合計)。**本数の上限
    /// ([`MAX_REQUESTS`] / [`MAX_CONNECTS`]) に当たると実際はもっと早く終わる**
    /// (そのときは `note` にそう書く)
    pub secs: u64,
    /// 数字が出なかった理由、または早く終わった理由 (128 バイトまで)。無ければ `None`
    pub note: Option<String>,
}

/// 最後に測った結果 (`/status` が読む)。回していなければ `None` = `null`。
static LAST: Mutex<Option<Report>> = Mutex::new(None);

/// 自己ベンチのスレッドが使った CPU の合計 (us)。窓の増分からこれを引く。
static SB_CPU_US: AtomicU64 = AtomicU64::new(0);

/// 自己ベンチが**いま回っているか** (T14.43)。回っている 3 秒の間だけ真。
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// 相手役 (内蔵オリジンと sink) のポート (`127.0.0.1` の使い捨て。`0` = 無し)。
///
/// `crates/net/src/acl.rs` にも同じ 2 つを預ける (`run` の `exempt`) が、あちらは
/// **`PROXY_ALLOW_LOCAL=off` の穴**のためで、こちらは**統計から自分のぶんを外す**ため。
/// 依存の向きが逆 (`proxy-net` → `proxy-selfbench` は無い) なので、2 か所が同じ値を持つ。
static PORTS: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

/// 自己ベンチが回っているか (**原子の読み 1 回**)。
///
/// 回っていないとき ( = 既定、そして起動 3 秒後からずっと) はこの 1 回で終わる。
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// この宛先の鍵が自己ベンチの相手役か (`/hosts` `/clients` から外すため)。
///
/// 受けるのはホスト別統計の鍵 (`http://127.0.0.1:41234` / `connect://127.0.0.1:41235`) と
/// 接続元の個票が持つ宛先 (`127.0.0.1:41235`) の 3 通り。どれも末尾が `:ポート` なので、
/// **ポートが一致して、かつホストがループバック**のときだけ真にする
/// (ポートだけの一致で外すと、たまたま同じポート番号の外向きの相手が消えてしまう)。
pub fn is_target(key: &str) -> bool {
    if !active() {
        return false;
    }
    let Some((host, port)) = key.rsplit_once(':') else {
        return false;
    };
    let Ok(port) = port.parse::<u32>() else {
        return false;
    };
    port != 0
        && PORTS.iter().any(|p| p.load(Ordering::Relaxed) == port)
        && (host.ends_with("127.0.0.1") || host.ends_with("[::1]"))
}

/// この接続元が自己ベンチの打ち手か (`/recent` `/connections` と要求の合計から外すため)。
///
/// 打ち手はこのプロセスの中にいるので、接続元は必ずループバック。**起動直後の 3 秒**に
/// ループバックから来る接続は自己ベンチのものとして扱う (同じ 3 秒に人が
/// `127.0.0.1` から繋いだら、その 1 本も個票に残らない。個票が 1 件減るだけで、
/// 統計も遮断も変わらない)。
pub fn is_client(ip: &str) -> bool {
    active() && (ip == "127.0.0.1" || ip == "::1")
}

/// 相手役のポートを覚えて旗を立てる ([`run`] だけが呼ぶ)。
fn arm(ports: [u16; 2]) {
    for (slot, port) in PORTS.iter().zip(ports) {
        slot.store(u32::from(port), Ordering::Relaxed);
    }
    ACTIVE.store(true, Ordering::Relaxed);
}

/// 旗を下ろしてポートを忘れる。
fn disarm() {
    ACTIVE.store(false, Ordering::Relaxed);
    for slot in &PORTS {
        slot.store(0, Ordering::Relaxed);
    }
}

/// 最後の結果 (無ければ `None`)。
pub fn last() -> Option<Report> {
    LAST.locked().clone()
}

/// `/status` の `self_bench` の値。回していなければ `null`。
pub fn status_json() -> String {
    let Some(r) = last() else {
        return "null".to_string();
    };
    let us = |v: Option<f64>| match v {
        Some(v) => format!("{:.1}", v),
        None => "null".to_string(),
    };
    format!(
        "{{\"at\":{},\"forward_us\":{},\"connect_us\":{},\"cores\":{},\
         \"requests\":{},\"connects\":{},\"secs\":{},\"note\":{}}}",
        r.at,
        us(r.forward_us),
        us(r.connect_us),
        r.cores,
        r.requests,
        r.connects,
        r.secs,
        json::quote_opt(r.note.as_deref()),
    )
}

impl Report {
    /// 起動ログと `/events` に添える 1 行 (`self_bench forward 45 us, connect 140 us`)。
    pub fn summary(&self) -> String {
        let us = |v: Option<f64>| match v {
            Some(v) => format!("{:.0} us", v),
            None => "n/a".to_string(),
        };
        format!(
            "self_bench forward {}, connect {} ({} requests, {} tunnels in {}s on {} cores{})",
            us(self.forward_us),
            us(self.connect_us),
            self.requests,
            self.connects,
            self.secs,
            self.cores,
            match &self.note {
                Some(n) => format!("; {}", n),
                None => String::new(),
            }
        )
    }
}

/// 3 秒の自己ベンチを回して結果を覚え、その結果を返す。
///
/// `proxy` は**自分の待ち受け**(loopback に読み替えたもの)。`exempt` は「このループバックの
/// ポートだけは `PROXY_ALLOW_LOCAL=off` の判定から外す」という届け出で、終わったら空の
/// スライスで呼び戻す (穴は 3 秒で閉じる)。呼ぶのは `src/main.rs` の 1 か所だけ。
pub fn run(proxy: SocketAddr, exempt: &dyn Fn(&[u16])) -> Report {
    let mut report = Report {
        at: clock::now_epoch(),
        forward_us: None,
        connect_us: None,
        cores: thread::available_parallelism().map_or(0, |n| n.get()),
        requests: 0,
        connects: 0,
        secs: SECS,
        note: None,
    };
    // CPU が読めない環境 (Linux 以外、`/proc` が無い) では**数字を出さない**。
    // 引き算ができないまま出すと、打ち手とオリジンのぶんが乗った 2 倍以上の値になる
    if process_cpu_us().is_none() || thread_cpu_us().is_none() {
        return publish(
            report,
            "clock_gettime(CLOCK_PROCESS_CPUTIME_ID) is unavailable here",
        );
    }
    let half = Duration::from_millis(SECS * 1000 / 2);

    // --- forward 8 並列 ---
    let origin = match Server::origin() {
        Ok(s) => s,
        Err(e) => return publish(report, &format!("cannot start the built-in origin: {}", e)),
    };
    let sink = match Server::sink() {
        Ok(s) => s,
        Err(e) => return publish(report, &format!("cannot start the built-in sink: {}", e)),
    };
    // `PROXY_ALLOW_LOCAL=off` (既定) でも、この 2 つのポートだけは通してもらう
    let ports = [origin.addr.port(), sink.addr.port()];
    exempt(&ports);
    // 自分で打ったぶんを `/hosts` `/clients` `/recent` と要求の合計から外すための旗
    // (`is_target` / `is_client`)。**立っているのはこの 3 秒だけ**
    arm(ports);

    let target = origin.addr;
    let window = Window::start();
    let (requests, failed, fwd_capped) = spawn_load(
        "sb-fwd",
        move |deadline, budget| forward_worker(proxy, target, deadline, budget),
        half,
        MAX_REQUESTS,
    );
    // 内蔵オリジンのスレッドが自分の CPU を足し終えてから窓を閉じる
    origin.stop();
    let forward_cpu = window.end();
    report.requests = requests;
    if requests > 0 {
        report.forward_us = Some(forward_cpu as f64 / requests as f64);
    }

    // --- CONNECT 8 並列 ---
    let target = sink.addr;
    let window = Window::start();
    let (connects, refused, cnct_capped) = spawn_load(
        "sb-cnct",
        move |deadline, budget| connect_worker(proxy, target, deadline, budget),
        half,
        MAX_CONNECTS,
    );
    sink.stop();
    let connect_cpu = window.end();
    report.connects = connects;
    if connects > 0 {
        report.connect_us = Some(connect_cpu as f64 / connects as f64);
    }

    // 最後のトンネルを閉じたプロキシ側のスレッドが統計を書き終えるまで少しだけ待つ
    // (打ち手は `shutdown` した時点で次へ行くので、`/hosts` に書くのはその後になる)。
    // **CPU の窓は既に閉じてある**ので、この待ちは数字に乗らない
    thread::sleep(Duration::from_millis(50));
    // 旗と穴を閉じる (ここから先はループバック宛ても普通に 403 で、統計にも普通に載る)
    disarm();
    exempt(&[]);

    let note = match (requests, connects) {
        (0, 0) => format!(
            "the proxy refused all {} requests and all {} CONNECTs (host ACL? per-client limit?)",
            failed, refused
        ),
        (0, _) => format!("the proxy refused all {} requests (host ACL?)", failed),
        (_, 0) => format!(
            "the proxy refused all {} CONNECTs (PROXY_CONNECT_PORTS?)",
            refused
        ),
        // 上限に当たって早く終わった場合 (手元の速い CPU では普通にこうなる)。
        // 秒数より本数で止めたことが読めないと、`secs` から割った ops が合わなくなる
        (_, _) => match (fwd_capped, cnct_capped) {
            (true, true) => format!(
                "stopped early at the caps ({} requests, {} CONNECTs)",
                MAX_REQUESTS, MAX_CONNECTS
            ),
            (true, false) => format!("stopped early at the {}-request cap", MAX_REQUESTS),
            (false, true) => format!("stopped early at the {}-CONNECT cap", MAX_CONNECTS),
            (false, false) => String::new(),
        },
    };
    publish(report, &note)
}

/// 結果を覚えて返す (`note` が空なら `None` として覚える)。
fn publish(mut report: Report, note: &str) -> Report {
    report.note = (!note.is_empty()).then(|| clip(note, 128));
    *LAST.locked() = Some(report.clone());
    report
}

/// `max` バイトを超えたら末尾に `…` を付けて切る (`crates/metrics` の `recent::clip` と同じ作法)。
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max - 3;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ------------------------------------------------------------------ CPU の窓

/// 1 つの段階のあいだに**プロキシが**使った CPU を測る窓。
///
/// プロセス全体の CPU の増分から、自己ベンチのスレッドの取り分 ([`SB_CPU_US`]) と、
/// **窓を開け閉めしているこのスレッド自身**の取り分 (スレッドを起こす / 待つ費用) を引く。
struct Window {
    proc_us: u64,
    self_us: u64,
    own_us: u64,
}

impl Window {
    fn start() -> Window {
        Window {
            proc_us: process_cpu_us().unwrap_or(0),
            self_us: SB_CPU_US.load(Ordering::Relaxed),
            own_us: thread_cpu_us().unwrap_or(0),
        }
    }

    /// 窓を閉じて「プロキシ側の CPU (us)」を返す。**自己ベンチのスレッドを全部止めてから**
    /// 呼ぶこと (止める前に呼ぶと、まだ足していない取り分がプロキシ側に乗る)。
    fn end(self) -> u64 {
        let proc_delta = process_cpu_us().unwrap_or(0).saturating_sub(self.proc_us);
        let self_delta = SB_CPU_US
            .load(Ordering::Relaxed)
            .saturating_sub(self.self_us);
        let own_delta = thread_cpu_us().unwrap_or(0).saturating_sub(self.own_us);
        proc_delta
            .saturating_sub(self_delta)
            .saturating_sub(own_delta)
    }
}

/// 自己ベンチのスレッドの取り分を数える番人。**Drop で自分の CPU を足す**ので、
/// 途中で return してもパニックしても取り分が抜けない。
struct Charge;

impl Drop for Charge {
    fn drop(&mut self) {
        if let Some(us) = thread_cpu_us() {
            SB_CPU_US.fetch_add(us, Ordering::Relaxed);
        }
    }
}

/// このプロセスが使った CPU (us)。`/proc/<pid>/stat` の utime + stime と同じ量。
fn process_cpu_us() -> Option<u64> {
    cpu_us(CLOCK_PROCESS_CPUTIME_ID)
}

/// **呼んだスレッドが**使った CPU (us)。
fn thread_cpu_us() -> Option<u64> {
    cpu_us(CLOCK_THREAD_CPUTIME_ID)
}

/// `clock_gettime(2)` の時計の番号 (Linux)。
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;

/// `clock_gettime` で CPU 時間を us で読む。読めない環境では `None`
/// (呼び出し側は数字を出さずに `note` を書く)。
///
/// 外部クレートは使わない (TASKS.md §0)。`crates/sys/src/sys.rs` と同じく
/// `unsafe extern "C"` で宣言し、64 ビットの Linux 以外では「無い」と出す
/// (`timespec` の中身が 64 ビットであることに寄りかかっているため)。
#[cfg(all(
    target_os = "linux",
    any(target_arch = "aarch64", target_arch = "x86_64")
))]
fn cpu_us(clock: i32) -> Option<u64> {
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }
    unsafe extern "C" {
        fn clock_gettime(clk: i32, tp: *mut Timespec) -> i32;
    }
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: 書き込み先は この呼び出しの間だけ有効ならよい `timespec` 1 つ。失敗は -1 で返る
    if unsafe { clock_gettime(clock, &raw mut ts) } != 0 {
        return None;
    }
    Some(ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000)
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "aarch64", target_arch = "x86_64")
)))]
fn cpu_us(_clock: i32) -> Option<u64> {
    None
}

// ------------------------------------------------------------------ 打ち手

/// 打ち手 `CONC` 本が分け合う「あと何回やってよいか」(T14.43 の上限)。
///
/// 秒数だけで止めると、速い CPU では 88,000 要求 / 22,000 本まで行ってしまう
/// ([`MAX_REQUESTS`] / [`MAX_CONNECTS`] の説明)。**原子 1 つを 8 本で取り合う**が、
/// 1 要求に 1 回なので、要求そのものの数十 us に比べれば無視できる。
struct Budget {
    used: AtomicU64,
    cap: u64,
}

impl Budget {
    /// 1 回ぶん取る (取れたら真)。
    fn take(&self) -> bool {
        self.used.fetch_add(1, Ordering::Relaxed) < self.cap
    }

    /// 上限に当たって止めたか (全部が終わってから読む)。
    fn spent(&self) -> bool {
        self.used.load(Ordering::Relaxed) >= self.cap
    }
}

/// `CONC` 本のスレッドで `work` を `dur` のあいだ (または `cap` 回まで) 回し、
/// (成功, 失敗, 上限に当たったか) を返す。
fn spawn_load<F>(name: &'static str, work: F, dur: Duration, cap: u64) -> (u64, u64, bool)
where
    F: Fn(Instant, &Budget) -> (u64, u64) + Send + Sync + 'static,
{
    let deadline = Instant::now() + dur;
    let work = Arc::new(work);
    let budget = Arc::new(Budget {
        used: AtomicU64::new(0),
        cap,
    });
    let mut handles: Vec<JoinHandle<(u64, u64)>> = Vec::with_capacity(CONC);
    for _ in 0..CONC {
        let work = Arc::clone(&work);
        let budget = Arc::clone(&budget);
        let spawned = thread::Builder::new().name(name.into()).spawn(move || {
            let _charge = Charge;
            work(deadline, &budget)
        });
        if let Ok(h) = spawned {
            handles.push(h);
        }
    }
    let (mut ok, mut bad) = (0u64, 0u64);
    for h in handles {
        if let Ok((o, b)) = h.join() {
            ok += o;
            bad += b;
        }
    }
    (ok, bad, budget.spent())
}

/// keep-alive の forward を回す 1 本ぶん (200 が返った数と、それ以外の数)。
fn forward_worker(
    proxy: SocketAddr,
    origin: SocketAddr,
    deadline: Instant,
    budget: &Budget,
) -> (u64, u64) {
    let request = format!("GET http://{0}/ HTTP/1.1\r\nHost: {0}\r\n\r\n", origin).into_bytes();
    let (mut ok, mut bad) = (0u64, 0u64);
    let mut conn: Option<(TcpStream, BufReader<TcpStream>)> = None;
    let mut line = String::new();
    // **秒数の判定が先**。上限を取るのは「まだ時間がある」ときだけ (時間切れで
    // 抜けた回まで上限を食うと、当たっていないのに当たったことになる)
    while Instant::now() < deadline && budget.take() {
        if conn.is_none() {
            let Ok(sock) = TcpStream::connect(proxy) else {
                bad += 1;
                return (ok, bad);
            };
            let _ = sock.set_nodelay(true);
            let Ok(clone) = sock.try_clone() else {
                bad += 1;
                return (ok, bad);
            };
            conn = Some((sock, BufReader::new(clone)));
        }
        let Some((sock, reader)) = conn.as_mut() else {
            break;
        };
        match sock
            .write_all(&request)
            .and_then(|()| read_response(reader, &mut line))
        {
            Ok((200, keep)) => {
                ok += 1;
                if !keep {
                    conn = None;
                }
            }
            // 403 (ACL) や 503 (上限) はここに来る。数えるだけで CPU の割り算には使わない
            Ok((_, _)) => {
                bad += 1;
                conn = None;
            }
            Err(_) => {
                bad += 1;
                conn = None;
            }
        }
    }
    (ok, bad)
}

/// CONNECT を張っては閉じる 1 本ぶん (確立できた本数と、断られた数)。
fn connect_worker(
    proxy: SocketAddr,
    sink: SocketAddr,
    deadline: Instant,
    budget: &Budget,
) -> (u64, u64) {
    let (mut ok, mut bad) = (0u64, 0u64);
    while Instant::now() < deadline && budget.take() {
        match open_tunnel(proxy, sink) {
            Ok(sock) => {
                ok += 1;
                let _ = sock.shutdown(Shutdown::Both);
            }
            Err(_) => {
                bad += 1;
                // 断られ続けるなら待っても無駄なので、少し休んでから次 (ここで回り続けて
                // CPU を焼くと、測っているものが「403 を返す費用」になってしまう)
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
    (ok, bad)
}

/// 応答を 1 つ読み切る。戻り値は (状態コード, この接続を使い続けてよいか)。
fn read_response(reader: &mut BufReader<TcpStream>, line: &mut String) -> io::Result<(u16, bool)> {
    let mut status = 0u16;
    let mut length: Option<u64> = None;
    let mut keep = true;
    let mut first = true;
    loop {
        line.clear();
        if reader.read_line(line)? == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        if first {
            first = false;
            status = line
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().ok();
            } else if k.eq_ignore_ascii_case("connection") && v.trim().eq_ignore_ascii_case("close")
            {
                keep = false;
            }
        }
    }
    let mut body = [0u8; 16 * 1024];
    let mut left = length.unwrap_or(0);
    while left > 0 {
        let want = left.min(body.len() as u64) as usize;
        reader.read_exact(&mut body[..want])?;
        left -= want as u64;
    }
    Ok((status, keep))
}

/// CONNECT を張り、`200` を読み切ったソケットを返す。
///
/// `BufReader` を残さないのは `crates/bench` の `open_tunnel_bare` と同じ理由
/// (CONNECT の応答のあとには何も続かないので、1 バイトずつ読んでも取りこぼさない)。
fn open_tunnel(proxy: SocketAddr, target: SocketAddr) -> io::Result<TcpStream> {
    let mut sock = TcpStream::connect(proxy)?;
    sock.set_nodelay(true)?;
    let req = format!("CONNECT {0} HTTP/1.1\r\nHost: {0}\r\n\r\n", target);
    sock.write_all(req.as_bytes())?;
    let mut head = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if sock.read(&mut byte)? == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        head.push(byte[0]);
        if head.len() > 4096 {
            return Err(io::Error::other("CONNECT response header too long"));
        }
    }
    if !head.starts_with(b"HTTP/1.1 200") {
        return Err(io::Error::other("CONNECT was refused"));
    }
    Ok(sock)
}

// ------------------------------------------------------------------ 相手役

/// 自分の中の相手役 (forward の固定応答オリジン / CONNECT の sink)。
///
/// **どちらも `127.0.0.1` の使い捨てポート**で、外からは何も来ない前提。
/// 止めるときは旗を立ててから自分で 1 本繋いで `accept` を起こす。
struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    acceptor: Option<JoinHandle<()>>,
    conns: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Server {
    /// 固定応答を返す keep-alive オリジン (forward の相手)。
    fn origin() -> io::Result<Server> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let conns: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        // 応答は 1 回の `write_all` で出す。`no-store` なのでキャッシュには入らない
        // (§1 のベンチの既定と同じ条件 = 毎回オリジンまで行く)
        let body = vec![b'x'; BODY_BYTES];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let response = Arc::new(response);
        let acceptor = {
            let stop = Arc::clone(&stop);
            let conns = Arc::clone(&conns);
            thread::Builder::new()
                .name("sb-origin".into())
                .spawn(move || {
                    let _charge = Charge;
                    for stream in listener.incoming() {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let Ok(stream) = stream else { continue };
                        let response = Arc::clone(&response);
                        let stop = Arc::clone(&stop);
                        let spawned =
                            thread::Builder::new()
                                .name("sb-conn".into())
                                .spawn(move || {
                                    let _charge = Charge;
                                    serve_origin(stream, &response, &stop);
                                });
                        if let Ok(h) = spawned {
                            conns.locked().push(h);
                        }
                    }
                })?
        };
        Ok(Server {
            addr,
            stop,
            acceptor: Some(acceptor),
            conns,
        })
    }

    /// 繋がれたらすぐ閉じる相手 (CONNECT の sink)。1 スレッドだけで受ける。
    fn sink() -> io::Result<Server> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let acceptor = {
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("sb-sink".into())
                .spawn(move || {
                    let _charge = Charge;
                    for stream in listener.incoming() {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        if let Ok(s) = stream {
                            let _ = s.shutdown(Shutdown::Both);
                        }
                    }
                })?
        };
        Ok(Server {
            addr,
            stop,
            acceptor: Some(acceptor),
            conns: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// 相手役を止め、**スレッドが自分の CPU を足し終えるまで待つ**。
    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // `accept` で寝ているスレッドを起こすためだけの 1 本 (旗が立っているので受けたら抜ける)
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.acceptor.take() {
            let _ = h.join();
        }
        let handles: Vec<JoinHandle<()>> = self.conns.locked().drain(..).collect();
        for h in handles {
            let _ = h.join();
        }
    }
}

/// 要求の終わり (空行) を数えては固定応答を返す。**行の中身は読まない** (固定応答なので要らない)。
///
/// 読み取りタイムアウトを置いてあるのは、止めるときに気づくため
/// (プロキシはオリジンへの接続をプールに持ったまま次の要求を待つので、閉じてくれない)。
fn serve_origin(mut stream: TcpStream, response: &[u8], stop: &AtomicBool) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(ORIGIN_POLL));
    let mut buf = [0u8; 2048];
    let mut matched = 0usize;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                for _ in 0..count_requests(&mut matched, &buf[..n]) {
                    if stream.write_all(response).is_err() {
                        return;
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// `data` の中に「ヘッダーの終わり」(`\r\n\r\n`) がいくつあるかを数える。
///
/// `matched` は**前の呼び出しからの続き**(何バイトまで一致していたか)。読みが途中で
/// 切れても数え落とさないように、呼ぶ側が持ち回る。
fn count_requests(matched: &mut usize, data: &[u8]) -> usize {
    const END: &[u8; 4] = b"\r\n\r\n";
    let mut hits = 0usize;
    for &b in data {
        if b == END[*matched] {
            *matched += 1;
            if *matched == END.len() {
                *matched = 0;
                hits += 1;
            }
        } else {
            *matched = usize::from(b == b'\r');
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回していなければ `/status` の `self_bench` は `null`。
    #[test]
    fn status_is_null_before_the_first_run() {
        // このテストは `run` を呼ばない (呼ぶテストと同じプロセスだと順序に依るので、
        // 実バイナリでの確認は `tests/selfbench_test.rs` に置いてある)
        if last().is_none() {
            assert_eq!(status_json(), "null");
        }
    }

    /// 数字が出たときの形 (`/status` の `self_bench`)。
    #[test]
    fn status_json_has_the_documented_shape() {
        let r = Report {
            at: 1_700_000_000,
            forward_us: Some(45.25),
            connect_us: None,
            cores: 4,
            requests: 12_345,
            connects: 0,
            secs: 3,
            note: Some("the proxy refused all 3 CONNECTs".to_string()),
        };
        *LAST.locked() = Some(r);
        let json = status_json();
        assert!(json.contains("\"at\":1700000000"), "{}", json);
        assert!(json.contains("\"forward_us\":45.2"), "{}", json);
        assert!(json.contains("\"connect_us\":null"), "{}", json);
        assert!(json.contains("\"cores\":4"), "{}", json);
        assert!(json.contains("\"requests\":12345"), "{}", json);
        assert!(json.contains("\"connects\":0"), "{}", json);
        assert!(json.contains("\"secs\":3"), "{}", json);
        assert!(json.contains("refused all 3 CONNECTs"), "{}", json);
        *LAST.locked() = None;
    }

    /// 要求の終わりは、読みが途中で切れても数えられる。
    #[test]
    fn requests_are_counted_across_reads() {
        let mut m = 0usize;
        assert_eq!(
            count_requests(&mut m, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
            1
        );
        // 2 つ続けて届いた場合
        assert_eq!(count_requests(&mut m, b"A\r\n\r\nB\r\n\r\n"), 2);
        // 境目で切れた場合 (`\r\n\r` までで 1 回目が終わる)
        assert_eq!(count_requests(&mut m, b"C\r\n\r"), 0);
        assert_eq!(count_requests(&mut m, b"\nD"), 1);
        // 途中まで一致してから崩れたら数えない
        assert_eq!(count_requests(&mut m, b"E\r\nF\r\n"), 0);
    }

    /// プロセスとスレッドの CPU が読めること (この環境で読めなければ数字は出さない)。
    #[cfg(target_os = "linux")]
    #[test]
    fn cpu_of_the_process_and_of_this_thread_is_readable() {
        let p = process_cpu_us().expect("プロセスの CPU");
        let t = thread_cpu_us().expect("このスレッドの CPU");
        assert!(
            p >= t,
            "プロセスの CPU は 1 本のスレッドより多い ({} < {})",
            p,
            t
        );
    }

    /// 自己ベンチが回っている 3 秒だけ、相手役と打ち手を統計から外せること (T14.43)。
    #[test]
    fn the_partners_are_recognised_only_while_the_bench_runs() {
        assert!(!active(), "回していないときは旗が下りている");
        assert!(!is_target("http://127.0.0.1:41234"));
        assert!(!is_client("127.0.0.1"));
        arm([41234, 41235]);
        assert!(is_target("http://127.0.0.1:41234"), "forward の鍵");
        assert!(is_target("connect://127.0.0.1:41235"), "CONNECT の鍵");
        assert!(is_target("127.0.0.1:41235"), "接続元の個票が持つ宛先");
        assert!(!is_target("http://127.0.0.1:41236"), "覚えていないポート");
        assert!(
            !is_target("http://example.com:41234"),
            "ポートだけの一致では外さない"
        );
        assert!(!is_target("http://example.com"), "ポートが無い鍵");
        assert!(
            is_client("127.0.0.1") && is_client("::1"),
            "打ち手は自分の中"
        );
        assert!(!is_client("203.0.113.9"), "外から来た接続元はそのまま");
        disarm();
        assert!(!is_target("http://127.0.0.1:41234"), "3 秒で閉じる");
        assert!(!is_client("127.0.0.1"));
    }

    /// 本数の上限は打ち手 8 本で分け合っても、超えも足りなくもしない。
    #[test]
    fn the_budget_is_shared_by_all_the_workers() {
        let budget = Arc::new(Budget {
            used: AtomicU64::new(0),
            cap: 100,
        });
        let mut handles = Vec::with_capacity(CONC);
        for _ in 0..CONC {
            let budget = Arc::clone(&budget);
            handles.push(thread::spawn(move || {
                let mut n = 0u64;
                while budget.take() {
                    n += 1;
                }
                n
            }));
        }
        let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(total, 100, "8 本で分け合っても合計は上限ちょうど");
        assert!(budget.spent(), "上限に当たったことが読める");
    }

    /// 自己ベンチのスレッドの取り分は、Drop のたびに足される。
    #[cfg(target_os = "linux")]
    #[test]
    fn a_self_bench_thread_charges_its_own_cpu() {
        let before = SB_CPU_US.load(Ordering::Relaxed);
        let h = thread::Builder::new()
            .name("sb-test".into())
            .spawn(|| {
                let _charge = Charge;
                // 1 tick (10 ms) 以上は確実に使う
                let t0 = Instant::now();
                let mut n = 0u64;
                while t0.elapsed() < Duration::from_millis(30) {
                    n = n.wrapping_add(t0.elapsed().as_nanos() as u64);
                }
                n
            })
            .expect("spawn");
        let _ = h.join();
        assert!(
            SB_CPU_US.load(Ordering::Relaxed) > before,
            "スレッドの CPU が自己ベンチの取り分に入っていない"
        );
    }
}
