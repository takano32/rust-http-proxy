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
//! `off` (既定) ではこのクレートの関数は 1 つも呼ばれない (`crates/run/src/startup.rs` の旗の分岐 1 回だけ)。
//!
//! **`crates/bench` から写したもの** (`crates/bench` は `default-members` の外なので本体から
//! 呼べない): 固定応答を返す keep-alive オリジン、すぐ閉じる sink、CONNECT を張って 200 を
//! 読むところ、応答を 1 つ読み切るところ。写したのはその 4 つだけで、ベンチ側の統計
//! (p50 / p99 / ops) は持たない (測るのは CPU/操作 だけ)。

use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use proxy_base::clock;
use proxy_base::json;
use proxy_base::sync::LockExt;

mod cpu;
mod load;
mod partner;
#[cfg(test)]
mod tests;

use cpu::{Window, process_cpu_us, thread_cpu_us};
use load::{Budget, connect_worker, forward_worker, spawn_load};
use partner::Server;

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

/// 1 回の自己ベンチの結果 (`/status` の `self_bench`)。
#[derive(Clone, Debug)]
pub struct Report {
    /// 測り終えた時刻 (epoch 秒)
    pub(crate) at: u64,
    /// forward の CPU/要求 (us)。1 本も通らなかった / CPU が読めないときは `None`
    pub(crate) forward_us: Option<f64>,
    /// CONNECT の CPU/本 (us)。同上
    pub(crate) connect_us: Option<f64>,
    /// この環境から見えるコア数 (`available_parallelism`)
    pub(crate) cores: usize,
    /// 200 が返った要求の数
    pub(crate) requests: u64,
    /// 確立できた CONNECT の本数
    pub(crate) connects: u64,
    /// 回す秒数の上限 (forward と CONNECT の合計)。**本数の上限
    /// ([`MAX_REQUESTS`] / [`MAX_CONNECTS`]) に当たると実際はもっと早く終わる**
    /// (そのときは `note` にそう書く)
    pub(crate) secs: u64,
    /// 数字が出なかった理由、または早く終わった理由 (128 バイトまで)。無ければ `None`
    pub(crate) note: Option<String>,
}

/// 最後に測った結果 (`/status` が読む)。回していなければ `None` = `null`。
static LAST: Mutex<Option<Report>> = Mutex::new(None);

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
    let ports = [
        PORTS[0].load(Ordering::Relaxed),
        PORTS[1].load(Ordering::Relaxed),
    ];
    target_matches(key, &ports)
}

/// [`is_target`] の判定の中身 (旗も静的な `PORTS` も読まない純粋な関数。T34.80)。
fn target_matches(key: &str, ports: &[u32]) -> bool {
    let Some((host, port)) = key.rsplit_once(':') else {
        return false;
    };
    let Ok(port) = port.parse::<u32>() else {
        return false;
    };
    port != 0 && ports.contains(&port) && (host.ends_with("127.0.0.1") || host.ends_with("[::1]"))
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
/// スライスで呼び戻す (穴は 3 秒で閉じる)。呼ぶのは `crates/run/src/startup.rs` の 1 か所だけ。
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
    let forward = measure_phase(
        "sb-fwd",
        move |deadline, budget| forward_worker(proxy, target, deadline, budget),
        origin,
        half,
        MAX_REQUESTS,
    );
    report.requests = forward.ok;
    report.forward_us = forward.per_op();

    // --- CONNECT 8 並列 ---
    let target = sink.addr;
    let connect = measure_phase(
        "sb-cnct",
        move |deadline, budget| connect_worker(proxy, target, deadline, budget),
        sink,
        half,
        MAX_CONNECTS,
    );
    report.connects = connect.ok;
    report.connect_us = connect.per_op();

    // 最後のトンネルを閉じたプロキシ側のスレッドが統計を書き終えるまで少しだけ待つ
    // (打ち手は `shutdown` した時点で次へ行くので、`/hosts` に書くのはその後になる)。
    // **CPU の窓は既に閉じてある**ので、この待ちは数字に乗らない
    thread::sleep(Duration::from_millis(50));
    // 旗と穴を閉じる (ここから先はループバック宛ても普通に 403 で、統計にも普通に載る)
    disarm();
    exempt(&[]);

    publish(report, &note_for(&forward, &connect))
}

/// 1 つの段階 (forward か CONNECT) の結果 (T34.80)。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Phase {
    /// 通った数 (200 が返った要求 / 確立できた CONNECT)
    ok: u64,
    /// 断られた数
    bad: u64,
    /// 本数の上限に当たって早く終わったか
    capped: bool,
    /// この段階のあいだにプロキシが使った CPU (us)
    cpu_us: u64,
}

impl Phase {
    /// 1 操作あたりの CPU (us)。1 本も通らなかったら `None`。
    fn per_op(&self) -> Option<f64> {
        (self.ok > 0).then(|| self.cpu_us as f64 / self.ok as f64)
    }
}

/// 打ち手 `CONC` 本で `work` を回し、窓を開け閉めして 1 つの段階を測る。
///
/// 相手役 (`partner`) は**打ち手が全部終わってから止め**、そのスレッドが自分の CPU を
/// 足し終えてから窓を閉じる。
fn measure_phase<F>(name: &'static str, work: F, partner: Server, dur: Duration, cap: u64) -> Phase
where
    F: Fn(Instant, &Budget) -> (u64, u64) + Send + Sync + 'static,
{
    let window = Window::start();
    let (ok, bad, capped) = spawn_load(name, work, dur, cap);
    // 内蔵オリジンのスレッドが自分の CPU を足し終えてから窓を閉じる
    partner.stop();
    let cpu_us = window.end();
    Phase {
        ok,
        bad,
        capped,
        cpu_us,
    }
}

/// `note` の文面 (純粋な関数。T34.80)。何も書くことがなければ空。
fn note_for(forward: &Phase, connect: &Phase) -> String {
    match (forward.ok, connect.ok) {
        (0, 0) => format!(
            "the proxy refused all {} requests and all {} CONNECTs (host ACL? per-client limit?)",
            forward.bad, connect.bad
        ),
        (0, _) => format!("the proxy refused all {} requests (host ACL?)", forward.bad),
        (_, 0) => format!(
            "the proxy refused all {} CONNECTs (PROXY_CONNECT_PORTS?)",
            connect.bad
        ),
        // 上限に当たって早く終わった場合 (手元の速い CPU では普通にこうなる)。
        // 秒数より本数で止めたことが読めないと、`secs` から割った ops が合わなくなる
        (_, _) => match (forward.capped, connect.capped) {
            (true, true) => format!(
                "stopped early at the caps ({} requests, {} CONNECTs)",
                MAX_REQUESTS, MAX_CONNECTS
            ),
            (true, false) => format!("stopped early at the {}-request cap", MAX_REQUESTS),
            (false, true) => format!("stopped early at the {}-CONNECT cap", MAX_CONNECTS),
            (false, false) => String::new(),
        },
    }
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
