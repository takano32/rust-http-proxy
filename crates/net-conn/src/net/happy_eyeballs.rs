//! 候補列へ IPv6 優先で 250 ms ずつずらして並行に繋ぐ本体 (Happy Eyeballs, RFC 8305) と、
//! その間隔 (`PROXY_HE_STAGGER_MS`)。**要求ごとに通る熱い経路**なので、確保・鍵・`dyn` は足していない
//! (`Race` はスタックの上の構造体で、`net.rs` にあった頃のループの局所変数をまとめただけ)。

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use super::addrs::interleave_from;
use super::ipv6::{ipv6_first_for_new_host, note_ipv6_attempt, note_ipv6_loss, note_ipv6_win};

/// Happy Eyeballs で次の接続試行を始めるまでの間隔の既定 (RFC 8305 の Connection Attempt Delay)。
pub const DEFAULT_STAGGER: Duration = Duration::from_millis(250);
/// `PROXY_HE_STAGGER_MS` で受ける範囲 (ms)。外れた値は設定の側で既定に戻す (T17.6)。
///
/// 10 ms〜2 秒は RFC 8305 §5 が Connection Attempt Delay に置く下限と上限。
pub const STAGGER_RANGE_MS: std::ops::RangeInclusive<u64> = 10..=2_000;
/// いま効いている間隔 (ms)。起動時に 1 回だけ [`set_stagger`] で決める (T17.6)。
///
/// 要求の経路で読むのは `Relaxed` の load 1 回だけ (システムコールも確保も増えない)。
/// `IPV6_ENABLED` と同じ形で、`OnceLock` にしないのはテストが既定へ戻せるようにするため。
static STAGGER_MS: AtomicU64 = AtomicU64::new(DEFAULT_STAGGER.as_millis() as u64);

/// Happy Eyeballs の間隔を決める (起動時に 1 回。範囲の確かめは呼ぶ側 = 設定の仕事)。
pub fn set_stagger(d: Duration) {
    STAGGER_MS.store(d.as_millis() as u64, Ordering::Relaxed);
}

/// いま効いている Happy Eyeballs の間隔。
#[inline]
pub fn stagger() -> Duration {
    Duration::from_millis(STAGGER_MS.load(Ordering::Relaxed))
}

/// Happy Eyeballs で試す候補アドレスの上限 (T.32.14)。
///
/// 候補ごとに `thread::spawn` し、勝者が返っても敗者は締め切り (`PROXY_CONNECT_TIMEOUT_SECS`) まで
/// 記述子 1 つとスレッド 1 本を握ったまま残る (join しない・`PROXY_MAX_THREADS` に数えない)。候補数に
/// 上限が無いと、何十もの A / AAAA を返す名前への 1 接続がその数だけ記述子を使う。8 件あれば 250 ms 間隔で
/// 2 秒ぶん試せ、交互に並べたあとで切るので両方の族が 4 件ずつ残る。
pub const MAX_CANDIDATES: usize = 8;

#[cfg(test)]
thread_local! {
    /// このスレッドで Happy Eyeballs が起こした接続の試行の数 (**テストだけ**。T.32.14)
    pub(super) static HE_LAUNCHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 上流ソケットの Nagle を切る (小さな応答が delayed ACK 待ちで止まらないように)。失敗は無視。
fn nodelay(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
}

/// 確立したばかりの接続が **SYN を何回送り直したか**を控える (T14.46)。
///
/// `getsockopt(SOL_TCP, TCP_INFO)` の `tcpi_total_retrans` は「この接続で再送した通算」
/// だが、**確立した直後はまだデータを 1 バイトも送っていない**ので、読めた値は
/// SYN (3-way handshake) の再送回数そのものになる。1 回の再送は Linux の既定
/// (`tcp_syn_retries`) では 1 秒・3 秒・7 秒…と伸びるので、`connect` が 1 秒や 3 秒に
/// 飛んだ接続はここが 1 以上になる (T14.16 / T14.47 で見た 1 秒の正体)。
///
/// **費用は確立 1 本につき `getsockopt` 1 回**。要求ごとにも中継のバイトごとにも
/// 読まない (forward はプールが接続を張るときだけ通る)。読めなければ 0 =「再送なし
/// または読めなかった」で、Linux 以外はソケットを見ずに 0 のまま。
fn note_syn_retrans(stream: &TcpStream) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        if let Some(info) = proxy_sys::sys::tcp_info(stream.as_raw_fd()) {
            // 255 で頭打ち (個票の欄は u8。これ以上は「とても多い」で足りる)
            crate::dns::note_syn_retrans(info.total_retrans.min(u8::MAX as u32) as u8);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = stream;
    }
}

/// 1 つのアドレスへ接続する。`timeout` が `None` なら締め切り無し (OS 既定に任せる)。
fn connect_one(addr: &SocketAddr, timeout: Option<Duration>) -> io::Result<TcpStream> {
    match timeout {
        Some(t) => TcpStream::connect_timeout(addr, t),
        None => TcpStream::connect(addr),
    }
}

/// **1 つのアドレスへ繋ぐだけ** (canary の IPv6 側。T14.37)。
///
/// Happy Eyeballs も、勝敗の記録 ([`super::ipv6_status_json`] の `attempts` / `wins` / `losses`) も、
/// ホストごとの族の記憶 ([`crate::dns::remember_family`]) も**動かさない**。
/// 結果を `v4_first` の判定に渡すのは canary が別に [`super::note_canary_ipv6`] を呼んだとき (T17.7)。
pub fn connect_addr(addr: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    connect_one(addr, proxy_base::timeout::for_socket(timeout))
}

/// 候補列に Happy Eyeballs で接続する本体。
///
/// `timeout` が `Duration::ZERO` なら**締め切りを置かない** (`PROXY_TIMEOUT_SECS=0` = 無期限。
/// T10.6)。`TcpStream::connect_timeout` は 0 を `InvalidInput` で断るので、そのときは
/// 素の `connect` を使い、OS 既定の接続タイムアウト (Linux はおよそ 130 秒) に任せる。
///
/// `preferred` はこのホストで最後に勝った族。`None` のときは全体の状態で決める
/// (IPv6 が 1 度も勝っていなければ IPv4 を先頭に。T12.1)。
pub(super) fn connect_candidates(
    host: &str,
    addrs: Vec<SocketAddr>,
    preferred: Option<bool>,
    timeout: Duration,
) -> io::Result<TcpStream> {
    if addrs.len() == 1 {
        // 候補が 1 つなら Happy Eyeballs は要らない (この経路は T12.1 でも変えていない)
        return connect_single(&addrs[0], timeout);
    }

    let mut addrs = interleave_from(addrs, preferred.unwrap_or_else(ipv6_first_for_new_host));
    // 候補は先頭の [`MAX_CANDIDATES`] 件で切る (T.32.14)。交互に並べたあとで切るので両方の族が残る
    addrs.truncate(MAX_CANDIDATES);
    let mut race = Race::new(&addrs, timeout, stagger(), connect_one);
    let (index, stream) = race.run()?;
    note_winner(
        host,
        &stream,
        addrs[index].is_ipv6(),
        race.tried_v6,
        preferred,
    );
    Ok(stream)
}

/// 候補が 1 つのときの接続 (並べ替えも全体の勝敗の記録も要らない)。
fn connect_single(addr: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    let v6 = addr.is_ipv6();
    connect_one(addr, proxy_base::timeout::for_socket(timeout)).inspect(|s| {
        nodelay(s);
        // 確立した族を控える (thread-local への書き込み 1 回。T12.4 (2))
        crate::dns::note_family(v6);
        // SYN を何回送り直したか (`getsockopt` 1 回。T14.46)
        note_syn_retrans(s);
    })
}

/// 締め切りで諦めたときの札 (`TimedOut`。`ErrCause::from_io` が `timeout` に数える)。
fn attempts_timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "connection attempts timed out")
}

/// 勝った候補の後始末: Nagle を切り、SYN の再送と族を控え、全体の勝敗とホストごとの記憶を書く。
fn note_winner(
    host: &str,
    stream: &TcpStream,
    won_v6: bool,
    tried_v6: bool,
    preferred: Option<bool>,
) {
    nodelay(stream);
    // SYN を何回送り直したか (`getsockopt` 1 回。T14.46)。**勝った候補だけ**
    // 読む (捨てる候補のぶんは数えない = 確立 1 本につきちょうど 1 回)
    note_syn_retrans(stream);
    // ホスト別の内訳 (`v4_wins` / `v6_wins`) に使う (T12.4 (2))
    crate::dns::note_family(won_v6);
    if tried_v6 {
        if won_v6 {
            note_ipv6_win();
        } else {
            note_ipv6_loss();
        }
    }
    // ホストごとの記憶は**答えが変わるときだけ**書く (毎回書くと鍵を取ることになる)
    if preferred != Some(won_v6) {
        crate::dns::remember_family(host, won_v6);
    }
}

/// 1 つのアドレスへ繋ぐ関数の形 ([`Race`] が試行ごとのスレッドで呼ぶ)。本番は [`connect_one`]、
/// 試験は締め切りと間隔を確かめるための作り物を渡す (T.34.79。総称なので単相化され、`dyn` にはならない)。
trait Connect<S>: Fn(&SocketAddr, Option<Duration>) -> io::Result<S> + Copy + Send + 'static {}
impl<S, F> Connect<S> for F where
    F: Fn(&SocketAddr, Option<Duration>) -> io::Result<S> + Copy + Send + 'static
{
}

/// 1 回の接続の中で走っている試行の集まり (`connect_candidates` のループの状態)。
///
/// `S` は繋いだもの (本番は `TcpStream`)、`F` は 1 つのアドレスへ繋ぐ関数 (T.34.79)。
struct Race<'a, S, F> {
    addrs: &'a [SocketAddr],
    /// 全体の締め切り (`timeout` が 0 なら無し)
    deadline: Option<Instant>,
    /// 間隔は 1 回の接続の中では変わらないので、最初に 1 回だけ読む
    stagger: Duration,
    connect: F,
    tx: mpsc::Sender<(usize, io::Result<S>)>,
    rx: mpsc::Receiver<(usize, io::Result<S>)>,
    /// 起動した試行の数と、まだ結果の返っていない数
    launched: usize,
    pending: usize,
    /// IPv6 の候補を 1 本でも起動したか (勝敗を数えるのはこのとき)
    tried_v6: bool,
    /// 次の試行を始めてよい時刻
    next_launch: Instant,
    /// 最後に返ってきた失敗
    last_err: Option<io::Error>,
    /// 締め切りで抜けたか (T15.0 (3))
    timed_out: bool,
}

impl<'a, S: Send + 'static, F: Connect<S>> Race<'a, S, F> {
    fn new(addrs: &'a [SocketAddr], timeout: Duration, stagger: Duration, connect: F) -> Self {
        let (tx, rx) = mpsc::channel();
        Race {
            addrs,
            deadline: (!timeout.is_zero()).then(|| Instant::now() + timeout),
            stagger,
            connect,
            tx,
            rx,
            launched: 0,
            pending: 0,
            tried_v6: false,
            next_launch: Instant::now(),
            last_err: None,
            timed_out: false,
        }
    }

    /// 候補を間隔をずらして起こし、最初に繋がったもの (候補の添字と繋いだもの) を返す。
    /// 全部が失敗したら最後に返ってきた失敗、締め切りを過ぎたら `TimedOut`。
    fn run(&mut self) -> io::Result<(usize, S)> {
        loop {
            let now = Instant::now();
            if self.should_launch(now) {
                self.launch(now);
            }
            match self.wait(now) {
                Ok((index, Ok(stream))) => return Ok((index, stream)),
                Ok((_, Err(e))) => {
                    self.pending -= 1;
                    self.last_err = Some(e);
                    if self.pending == 0 && self.launched == self.addrs.len() {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.deadline.is_some_and(|d| Instant::now() >= d) {
                        // 締め切りで抜けたか (T15.0 (3))。立てるのは**この締め切りの `break` だけ**で、
                        // 「全候補が失敗した」の `break` では立てない (そのときの `last_err` は本物の理由)
                        self.timed_out = true;
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        // 締め切りで抜けたときは「終わった理由」を返す (T15.0 (3))。ここで `last_err`
        // (先に返ってきた 1 本の `ECONNREFUSED` 等) を返すと、30 秒黙って待った接続が
        // `/errors` にも `/hosts` の `errors_by_cause` にも `refused` として残り、
        // 原因別の集計がそのまま狂う (`ErrCause::from_io` はこの 1 つの札しか見ない)
        if self.timed_out {
            return Err(attempts_timed_out());
        }
        Err(self.last_err.take().unwrap_or_else(attempts_timed_out))
    }

    /// 次の候補を起こすか: 候補が残っていて、走っているものが無いか間隔が過ぎた。
    fn should_launch(&self, now: Instant) -> bool {
        self.launched < self.addrs.len() && (self.pending == 0 || now >= self.next_launch)
    }

    /// 次の候補へのスレッドを 1 本起こす。
    fn launch(&mut self, now: Instant) {
        let addr = self.addrs[self.launched];
        let index = self.launched;
        let tx = self.tx.clone();
        let connect = self.connect;
        let remaining = self.deadline.map(|d| {
            d.saturating_duration_since(now)
                .max(Duration::from_millis(1))
        });
        if addr.is_ipv6() && !self.tried_v6 {
            self.tried_v6 = true;
            note_ipv6_attempt();
        }
        #[cfg(test)]
        HE_LAUNCHED.set(HE_LAUNCHED.get() + 1);
        thread::spawn(move || {
            let _ = tx.send((index, connect(&addr, remaining)));
        });
        self.launched += 1;
        self.pending += 1;
        self.next_launch = now + self.stagger;
    }

    /// 次の結果を待つ。締め切りが無いときは、まだ launch していないぶんの間隔だけ待ち、
    /// 全部 launch したあとは結果が出るまで待つ (無期限)。
    fn wait(&self, now: Instant) -> Result<(usize, io::Result<S>), RecvTimeoutError> {
        let wait = if self.launched < self.addrs.len() {
            Some(self.next_launch.saturating_duration_since(now))
        } else {
            self.deadline.map(|d| d.saturating_duration_since(now))
        };
        match wait {
            Some(w) => self.rx.recv_timeout(w),
            None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        }
    }
}

/// `Race` の締め切りと間隔の単体試験 (T.34.79。作り物の接続関数で、網には出ない)。
#[cfg(test)]
mod race_tests;
