use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::Arc;
#[cfg(not(target_os = "linux"))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(target_os = "linux"))]
use std::thread;
use std::time::{Duration, Instant};

use crate::dns;
use crate::log::{Access, access};
#[cfg(not(target_os = "linux"))]
use crate::log_trace;
use crate::metrics::{Detail, ErrCause, HostOutcome, Metrics};
use crate::net;
use crate::recent::ConnSlot;
use crate::{log_debug, log_warn};

#[cfg(target_os = "linux")]
pub use relay::{Idle, Park, expire, resume};

/// 宛先へつないで `200 Connection Established` と先読みぶんを送ったところ。
struct Opened {
    client: TcpStream,
    server: TcpStream,
    info: Info,
}

/// トンネル 1 本の素性 (アクセスログと統計に要るもの)。ソケットとは別に持ち運ぶ。
struct Info {
    conn_id: usize,
    /// `host:port` (`connect://` を付けてホスト別統計の鍵にする)
    addr_str: String,
    client_ip: String,
    /// CONNECT を受けた時刻 (アクセスログの所要時間はここから測る)
    started: Instant,
    /// ホスト別の応答時間は接続確立まで (トンネル自体の寿命は応答時間ではない)
    connect_took: Duration,
    /// 確立までの内訳 (名前解決 / 接続 / 勝った族。T12.4 (2))
    detail: Detail,
    metrics: Arc<Metrics>,
    /// `/connections` の枠 (T13.4)。状態と運んだバイト数をここに書く。
    /// 登録と抹消は本体クレート (接続の開始と終了) の仕事で、ここは書くだけ
    slot: Option<Arc<ConnSlot>>,
}

/// 宛先へつなぎ、`200` と先読みぶん (`prefix`) を送る。
/// つなげなければ 502 を書き、ログと統計を出して `Err`。
#[allow(clippy::too_many_arguments)]
fn open(
    mut client: TcpStream,
    target: &str,
    prefix: &[u8],
    timeout: Duration,
    conn_id: usize,
    metrics: Arc<Metrics>,
    client_ip: String,
    resolved: Option<&dns::Resolved<'_>>,
    slot: Option<Arc<ConnSlot>>,
) -> io::Result<Opened> {
    let started = Instant::now();
    let addr_str = net::with_default_port(target, 443);

    log_debug!(Some(conn_id), "start CONNECT {}", addr_str);

    let mut server = match connect_with_timeout(&addr_str, resolved, timeout) {
        Ok(s) => s,
        Err(e) => {
            log_warn!(
                Some(conn_id),
                "502 Bad Gateway: connect {} failed: {}",
                addr_str,
                e
            );
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
            let detail = detail_of(started.elapsed(), Some(ErrCause::from_io(&e)));
            metrics.record_host_detail(
                &format!("connect://{}", addr_str),
                HostOutcome::Error,
                0,
                Some(started.elapsed()),
                detail,
            );
            // 個票にも 1 件残す (`/errors`。誰の・いつ・なぜ。T13.4)
            metrics.record_error(true, &addr_str, &client_ip, 502, &detail);
            metrics.record_client(&client_ip, HostOutcome::Error, 0, Some(started.elapsed()));
            access(
                conn_id,
                &Access {
                    client: &client_ip,
                    method: "CONNECT",
                    target: &addr_str,
                    version: "HTTP/1.1",
                    status: "502",
                    bytes: 0,
                    duration_ms: started.elapsed().as_secs_f64() * 1000.0,
                    cache: "BYPASS",
                },
            );
            return Err(e);
        }
    };

    // ホスト別の応答時間は接続確立まで (トンネル自体の寿命は応答時間ではない)
    let connect_took = started.elapsed();
    let detail = detail_of(connect_took, None);
    // `/connections` の 1 行を「CONNECT の中継中」にする (1 本につき 1 回だけ。T13.4)
    if let Some(s) = &slot {
        s.begin_tunnel(&addr_str);
    }
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    client.flush()?;
    if !prefix.is_empty() {
        server.write_all(prefix)?;
    }
    Ok(Opened {
        client,
        server,
        info: Info {
            conn_id,
            addr_str,
            client_ip,
            started,
            connect_took,
            detail,
            metrics,
            slot,
        },
    })
}

/// 確立までの内訳を組み立てる。**呼ぶのは接続が終わった直後の 1 回だけ**
/// (thread-local を読んで 0 に戻すので、2 回呼ぶと 2 回目が空になる)。
///
/// 接続にかかった時間は「全体 − 名前解決」で出す。`net` 側に測る口を足すと
/// T12.7 (同じところを直しているタスク) と衝突するので、**引き算で済ませている**。
fn detail_of(total: Duration, cause: Option<ErrCause>) -> Detail {
    let (dns_ms, dns_misses) = crate::dns::take_resolve_cost();
    let total_ms = total.as_millis().min(u64::MAX as u128) as u64;
    Detail {
        dns_ms,
        dns_misses,
        connect_ms: total_ms.saturating_sub(dns_ms),
        family_v6: crate::dns::take_family(),
        cause,
        // CONNECT は「確立まで」がそのまま窓に入る値なので指定しない
        first_byte_ms: None,
    }
}

/// トンネルが終わったときのアクセスログと統計 (どこで終わっても 1 回だけ通る)。
fn report(o: &Info, transferred: u64) {
    o.metrics.add_bytes(transferred);
    o.metrics.record_host_detail(
        &format!("connect://{}", o.addr_str),
        HostOutcome::Bypass,
        transferred,
        Some(o.connect_took),
        o.detail,
    );
    o.metrics.record_client(
        &o.client_ip,
        HostOutcome::Bypass,
        transferred,
        Some(o.connect_took),
    );
    access(
        o.conn_id,
        &Access {
            client: &o.client_ip,
            method: "CONNECT",
            target: &o.addr_str,
            version: "HTTP/1.1",
            status: "200",
            bytes: transferred,
            duration_ms: o.started.elapsed().as_secs_f64() * 1000.0,
            cache: "BYPASS(tunnel)",
        },
    );
}

/// `prefix` はリクエストヘッダーの直後に既に読み込んでしまったバイト列 (先にサーバーへ渡す)。
///
/// 暇なトンネルを監視スレッドへ預けたい呼び出し側は [`handle_connect_parked`] を使う (Linux)。
#[allow(clippy::too_many_arguments)]
pub fn handle_connect(
    client: TcpStream,
    target: &str,
    prefix: &[u8],
    timeout: Duration,
    idle: Option<Duration>,
    conn_id: usize,
    metrics: Arc<Metrics>,
    client_ip: String,
    resolved: Option<&dns::Resolved<'_>>,
    slot: Option<Arc<ConnSlot>>,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        handle_connect_parked(
            client,
            target,
            prefix,
            timeout,
            idle,
            conn_id,
            metrics,
            client_ip,
            resolved,
            None,
            Box::new(()),
            slot,
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let Opened {
            client,
            server,
            info,
        } = open(
            client, target, prefix, timeout, conn_id, metrics, client_ip, resolved, slot,
        )?;
        let transferred = tunnel(client, server, idle)?;
        report(&info, transferred);
        Ok(())
    }
}

/// 暇になったら監視スレッドへ預けられる CONNECT (Linux)。
///
/// `park` は預け先と「預ける前に同じスレッドで待ってみる猶予」。`None` なら
/// 従来どおりこのスレッドが最後まで面倒をみる。
/// `hold` は本体クレートの持ち分 (同時接続数と `active_connections` のガード) で、
/// 中身は見ないがトンネルが終わるまで落とさずに運ぶ (預けた瞬間に数が減ると
/// `PROXY_MAX_CONNS` の意味が壊れる)。
/// `client_ip` は接続元 IP の文字列。接続を受けたときに 1 回だけ作ったものを運ぶ
/// (ここで `peer_addr()` を引き直すと、トンネル 1 本ごとに `getpeername` が 1 回増える)。
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
pub fn handle_connect_parked(
    client: TcpStream,
    target: &str,
    prefix: &[u8],
    timeout: Duration,
    idle: Option<Duration>,
    conn_id: usize,
    metrics: Arc<Metrics>,
    client_ip: String,
    resolved: Option<&dns::Resolved<'_>>,
    park: Option<(Arc<dyn Park>, Duration)>,
    hold: Box<dyn Send>,
    slot: Option<Arc<ConnSlot>>,
) -> io::Result<()> {
    let opened = open(
        client, target, prefix, timeout, conn_id, metrics, client_ip, resolved, slot,
    )?;
    // トンネルの猶予は HTTP の keep-alive より長く取る (下限 [`relay::MIN_PARK_GRACE`])
    let park = park.map(|(w, grace)| (w, grace.max(relay::MIN_PARK_GRACE)));
    relay::start(opened, idle, park, hold)
}

/// 名前解決して接続する (IPv6 / IPv4 を Happy Eyeballs で並行に試す)。
/// `resolved` は ACL の判定が引いた答え。あればここでは解決しない (T12.7)。
pub fn connect_with_timeout(
    addr_str: &str,
    resolved: Option<&dns::Resolved<'_>>,
    timeout: Duration,
) -> io::Result<TcpStream> {
    net::connect_with(addr_str, resolved, timeout)
}

/// 双方向にデータを中継し、転送した合計バイト数を返す (Linux 以外)。
///
/// `idle` 秒だけ双方向とも動きが無ければ閉じる (`None` で無期限)。
/// Linux では 1 スレッドで `poll(2)` を回して `splice(2)` でカーネル内をコピーする
/// ([`relay`]。接続あたりのスレッドが 3 本から 1 本に減り、ユーザー空間へのコピーも無くなる)。
#[cfg(not(target_os = "linux"))]
pub fn tunnel(client: TcpStream, server: TcpStream, idle: Option<Duration>) -> io::Result<u64> {
    copy_both_ways(client, server, idle)
}

/// 2 スレッドで双方向に `io::copy` する従来版 (Linux 以外)。
#[cfg(not(target_os = "linux"))]
fn copy_both_ways(
    mut client: TcpStream,
    mut server: TcpStream,
    idle: Option<Duration>,
) -> io::Result<u64> {
    let mut client_clone = client.try_clone()?;
    let mut server_clone = server.try_clone()?;
    // poll が無いので、アイドル打ち切りは読み取りタイムアウトで代用する
    for s in [&client, &server, &client_clone, &server_clone] {
        let _ = s.set_read_timeout(idle);
        let _ = s.set_write_timeout(None);
    }
    let total = Arc::new(AtomicU64::new(0));

    let up = Arc::clone(&total);
    let t1 = thread::spawn(move || {
        let n = io::copy(&mut client, &mut server).unwrap_or(0);
        up.fetch_add(n, Ordering::Relaxed);
        let _ = server.shutdown(std::net::Shutdown::Write);
        n
    });

    let down = Arc::clone(&total);
    let t2 = thread::spawn(move || {
        let n = io::copy(&mut server_clone, &mut client_clone).unwrap_or(0);
        down.fetch_add(n, Ordering::Relaxed);
        let _ = client_clone.shutdown(std::net::Shutdown::Write);
        n
    });

    let sent = t1.join().unwrap_or(0);
    let received = t2.join().unwrap_or(0);
    log_trace!(None, "tunnel finished: {}B up / {}B down", sent, received);
    Ok(total.load(Ordering::Relaxed))
}

/// Linux の 1 スレッド中継 (`poll` + `splice`) と、暇なときの預け入れ。
#[cfg(target_os = "linux")]
mod relay {
    use std::io::{self, Read, Write};
    use std::net::{Shutdown, TcpStream};
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{Info, Opened, report};
    use crate::log_trace;
    use crate::recent::{ConnSlot, ConnState};
    use crate::sys::{self, POLLERR, POLLHUP, POLLIN, POLLOUT, Pipe, PollFd};

    /// 1 回の splice / read で動かす最大バイト数 (パイプ容量と同じ)。
    const CHUNK: usize = 1 << 20;
    /// 無期限 (`PROXY_TUNNEL_IDLE_SECS=0`) のトンネルを預けるときの「遠い期限」。
    /// 預かり所は期限の早い順に並べた集合で待つので、期限そのものは必ず要る。
    const FOREVER: Duration = Duration::from_secs(365 * 86400);
    /// 預ける前に同じスレッドで待ってみる猶予の下限 (`PROXY_PARK_GRACE_MS` が短くてもこれ以上)。
    ///
    /// トンネルの預け入れは HTTP の keep-alive より往復が高い (記述子 2 本の epoll 出し入れ、
    /// 中継パイプの作り直し、ワーカーの受け渡し) 一方、預けたいのは「秒〜分の単位で暇な
    /// トンネル」なので、100 ms 待って損はない。実測 (`--only connect`、短命トンネル):
    /// 猶予 3 ms だと CPU/本 175.2 → 190.2 us (+8.6%)、p99 1.709 → 2.263 ms (+32%) で、
    /// 「閉じられる直前に預けて、すぐ起こされる」ぶんを丸ごと払っていた。
    pub(super) const MIN_PARK_GRACE: Duration = Duration::from_millis(100);

    /// スレッドごとに使い回す中継パイプの数 (トンネル 1 本が両方向で 2 本使う)。
    const POOLED_PIPES: usize = 2;

    thread_local! {
        /// 使い終わった中継パイプの置き場 (スレッドごと)。
        ///
        /// 短命なトンネルは 1 本あたり `pipe2` 2 回・`fcntl(F_SETPIPE_SZ)` 2 回・
        /// `close` 4 回を払っていた (`--only connect` の実測でシステムコール 29.03 回/本 のうち 8 回)。
        /// 空になったパイプはスレッドに残しておき、次のトンネルが使い回す。
        /// **`Drop` を持つ [`Pipe`] を置くので、出し入れは必ず `try_with` で行うこと**
        /// (スレッドの終了中に `with` を呼ぶと `AccessError` で panic し、
        /// 「thread local panicked on drop」でプロセスごと abort する)。
        static PIPES: std::cell::RefCell<Vec<Pipe>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// 置き場から 1 本取る。無ければ作る (容量を広げるのは作るときだけ)。
    fn take_pipe() -> io::Result<Pipe> {
        if let Ok(Some(pipe)) = PIPES.try_with(|c| c.borrow_mut().pop()) {
            return Ok(pipe);
        }
        let pipe = Pipe::new()?;
        pipe.set_capacity(CHUNK as i32);
        Ok(pipe)
    }

    /// 空のパイプを置き場へ返す。置き場が一杯 (かスレッドの終了中) なら閉じる。
    fn give_pipe(pipe: Pipe) {
        let _ = PIPES.try_with(move |c| {
            let mut pool = c.borrow_mut();
            if pool.len() < POOLED_PIPES {
                pool.push(pipe);
            }
        });
    }

    /// 片方向の中継。データはパイプ (splice) か、使えなければ中間バッファに置く。
    /// 最初にデータが動くまで作らない (アイドルのトンネルは資源を持たない)。
    enum Relay {
        Unset,
        Pipe(Pipe),
        Buf(Vec<u8>),
    }

    struct Dir {
        /// `socks` の添字 (0 = クライアント, 1 = サーバー)
        src: usize,
        dst: usize,
        relay: Relay,
        /// まだ相手に渡していないバイト数
        pending: usize,
        /// バッファ方式で次に書き出す位置
        offset: usize,
        /// poll が「読める」と言った (最初は分からないので待つ側から始める)
        readable: bool,
        src_eof: bool,
        done: bool,
        moved: u64,
    }

    impl Dir {
        fn new(src: usize, dst: usize) -> Dir {
            Dir {
                src,
                dst,
                relay: Relay::Unset,
                pending: 0,
                offset: 0,
                readable: false,
                src_eof: false,
                done: false,
                moved: 0,
            }
        }

        /// 送信元から中継バッファへ移す。`Ok(0)` は EOF。
        fn fill(&mut self, socks: &[TcpStream; 2]) -> io::Result<usize> {
            if matches!(self.relay, Relay::Unset) {
                self.relay = match take_pipe() {
                    Ok(p) => Relay::Pipe(p),
                    Err(_) => Relay::Buf(vec![0u8; 64 * 1024]),
                };
            }
            match &mut self.relay {
                Relay::Unset => unreachable!("just initialised"),
                Relay::Pipe(pipe) => {
                    match sys::splice_move(socks[self.src].as_raw_fd(), pipe.write_fd, CHUNK) {
                        Ok(n) => Ok(n),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => Err(e),
                        // splice が使えないソケット (EINVAL など) はバッファ方式へ落として次周で読む
                        Err(_) => {
                            self.relay = Relay::Buf(vec![0u8; 64 * 1024]);
                            self.offset = 0;
                            Err(io::Error::from(io::ErrorKind::WouldBlock))
                        }
                    }
                }
                Relay::Buf(buf) => {
                    let n = (&socks[self.src]).read(buf)?;
                    self.offset = 0;
                    Ok(n)
                }
            }
        }

        /// 中継バッファから送信先へ移す。
        fn drain(&mut self, socks: &[TcpStream; 2]) -> io::Result<usize> {
            match &mut self.relay {
                Relay::Unset => Ok(0),
                Relay::Pipe(pipe) => {
                    sys::splice_move(pipe.read_fd, socks[self.dst].as_raw_fd(), self.pending)
                }
                Relay::Buf(buf) => {
                    let end = self.offset + self.pending;
                    (&socks[self.dst]).write(&buf[self.offset..end])
                }
            }
        }

        /// 中継の置き場を手放す。パイプは**空のときだけ**スレッドの置き場へ返す。
        ///
        /// 中身が残っているパイプを使い回すと、次のトンネルに**他人のバイト**が流れる。
        /// `pending` はパイプに入っていてまだ渡していないバイト数なので、これが 0 の
        /// ときだけ返す (渡せなくなったときは呼び出し側が `Relay::Unset` にして閉じる)。
        fn drop_relay(&mut self) {
            if let Relay::Pipe(pipe) = std::mem::replace(&mut self.relay, Relay::Unset)
                && self.pending == 0
            {
                give_pipe(pipe);
            }
            self.offset = 0;
        }

        /// 「今すぐ動かすものが無く、まだ両方向とも生きている」か (預けてよいかの判定)。
        fn quiet(&self) -> bool {
            self.pending == 0 && !self.src_eof && !self.done
        }
    }

    /// [`Idle::run_until_idle`] の結果。
    enum Outcome {
        /// トンネルは終わった (運んだ合計は `Idle::transferred` にある)
        Done,
        /// 両方向とも暇になった (預けてよい)
        Idle,
    }

    /// 暇になったトンネルの預け先 (実体は本体クレートの `IdleWatch`)。
    ///
    /// 依存の向きが 本体 → `proxy-tunnel` なので、預け先そのものは型として受け取れない。
    /// 預かれたら `Ok(())`、断られたら `Err(idle)` で戻ってきて、呼び出し側が続ける。
    pub trait Park: Send + Sync {
        fn park(&self, idle: Box<Idle>) -> Result<(), Box<Idle>>;
    }

    /// トンネル 1 本ぶんの、持ち運べる状態。
    ///
    /// 落ちるとソケットが閉じ、アクセスログと統計が 1 回だけ出る ([`Drop`])。
    /// どのワーカーで終わっても、預かり所が期限切れで落としても同じ場所を通る。
    pub struct Idle {
        socks: [TcpStream; 2],
        dirs: [Dir; 2],
        /// 双方向に運んだ合計バイト数 (預けても引き継ぐ)
        transferred: u64,
        /// 無通信で打ち切るまでの時間 (`None` で無期限)
        idle: Option<Duration>,
        /// 預け先と、預ける前に同じスレッドで待ってみる猶予 (`None` なら預けない)
        park: Option<(Arc<dyn Park>, Duration)>,
        /// アクセスログと統計に要る素性
        info: Info,
        /// 本体クレートの持ち分 (同時接続数と `active_connections`)。中身は見ない
        _hold: Box<dyn Send>,
    }

    impl Drop for Idle {
        fn drop(&mut self) {
            // 空のパイプはスレッドの置き場へ返す (次のトンネルが pipe2 と fcntl を省ける)
            for d in self.dirs.iter_mut() {
                d.drop_relay();
            }
            report(&self.info, self.transferred);
        }
    }

    impl Idle {
        /// ログ用の接続番号。
        pub fn id(&self) -> usize {
            self.info.conn_id
        }

        /// epoll に入れる記述子 (クライアントとサーバーの 2 本)。
        pub fn fds(&self) -> [RawFd; 2] {
            [self.socks[0].as_raw_fd(), self.socks[1].as_raw_fd()]
        }

        /// `/connections` の枠 (預かり所が状態を書くために借りる。T13.4)。
        pub fn slot(&self) -> Option<&Arc<ConnSlot>> {
            self.info.slot.as_ref()
        }

        /// 預かるときの期限。作れなければ `None` (預けない)。
        pub fn deadline(&self) -> Option<Instant> {
            // 無期限のトンネルも、期限の集合に入れるために遠い期限を置く
            Instant::now().checked_add(self.idle.unwrap_or(FOREVER))
        }

        /// 以後は預けない (断られたトンネルが猶予のたびに預け直そうとして空回りしないように)。
        pub fn no_park(&mut self) {
            self.park = None;
        }

        /// 預ける前に中継の資源を手放す。
        ///
        /// 預けるのは両方向とも `pending == 0` のときだけなので、パイプの中身は空。
        /// 方向あたり記述子 2 本 (splice が使えない相手では 64 KiB のバッファ) を暇な間ずっと
        /// 抱えないようにする。戻ってきたら遅延生成のまま作り直す。
        fn release(&mut self) {
            for d in self.dirs.iter_mut() {
                d.drop_relay();
                d.readable = false;
            }
            // ここまでに運んだバイト数を `/connections` に見せる (預ける直前の 1 回)
            if let Some(s) = &self.info.slot {
                s.set_bytes(self.transferred);
            }
        }

        /// 動かせるだけ動かして、終わるか暇になるまで回す。
        ///
        /// 預け先があるときは「猶予のあいだ `poll` が空振りしたら [`Outcome::Idle`]」で戻る
        /// (打ち切りの期限は預かり所が見る)。預け先が無いときは従来どおり、
        /// アイドル打ち切りまでこのスレッドで待つ。
        fn run_until_idle(&mut self) -> Outcome {
            let conn_id = self.info.conn_id;
            let timeout_ms: i32 = match self.idle {
                Some(d) => d.as_millis().min(i32::MAX as u128) as i32,
                None => -1,
            };
            let grace_ms: Option<i32> = self
                .park
                .as_ref()
                .map(|(_, g)| g.as_millis().min(i32::MAX as u128) as i32);
            let socks = &self.socks;
            let slot = self.info.slot.as_ref();
            let dirs = &mut self.dirs;
            let transferred = &mut self.transferred;

            loop {
                let mut progressed = false;
                for d in dirs.iter_mut() {
                    if d.done {
                        continue;
                    }
                    // 送信元 → 中継 (読めると分かってから中継バッファを用意する)
                    if !d.src_eof && d.pending == 0 && d.readable {
                        match d.fill(socks) {
                            Ok(0) => {
                                d.src_eof = true;
                                progressed = true;
                            }
                            Ok(n) => {
                                d.pending = n;
                                d.offset = 0;
                                progressed = true;
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                d.readable = false
                            }
                            Err(_) => {
                                d.src_eof = true;
                                progressed = true;
                            }
                        }
                    }
                    // 中継 → 送信先
                    while d.pending > 0 {
                        match d.drain(socks) {
                            Ok(0) => break,
                            Ok(n) => {
                                d.pending -= n;
                                d.offset += n;
                                d.moved += n as u64;
                                *transferred += n as u64;
                                progressed = true;
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            // 送信先が閉じた: この方向は終わり、相手にも伝える
                            Err(_) => {
                                d.pending = 0;
                                // パイプに残ったぶんはもう渡せない。置き場へ返さずに閉じる
                                // (返すと次のトンネルに他人のバイトが混ざる)
                                d.relay = Relay::Unset;
                                d.src_eof = true;
                                progressed = true;
                                break;
                            }
                        }
                    }
                    if d.src_eof && d.pending == 0 {
                        let _ = socks[d.dst].shutdown(Shutdown::Write);
                        d.done = true;
                        progressed = true;
                    }
                }
                if dirs.iter().all(|d| d.done) {
                    break;
                }
                if progressed {
                    continue;
                }

                // どちらも動かなかったので待つ
                let mut events = [0i16; 2];
                for d in dirs.iter() {
                    if d.done {
                        continue;
                    }
                    if !d.src_eof && d.pending == 0 {
                        events[d.src] |= POLLIN;
                    }
                    if d.pending > 0 {
                        events[d.dst] |= POLLOUT;
                    }
                }
                let mut fds = [
                    PollFd::new(socks[0].as_raw_fd(), events[0]),
                    PollFd::new(socks[1].as_raw_fd(), events[1]),
                ];
                // 両方向とも暇なら、待つのは猶予のあいだだけ。空振りしたら預ける。
                // **片方向だけ EOF (half-close) のトンネルは預けない**: 残った方向は
                // 「相手に shutdown を伝えて閉じる」までが仕事で、そこまで預かり所に
                // 持たせると起こし方が 2 通りになる。寿命も短いので単純さを採る
                // ここで止まる = 今の合計が落ち着いた値。`/connections` に見せるのは
                // この 1 回だけで、バイトごとにも splice ごとにも書かない (T13.4)
                if let Some(s) = slot {
                    s.set_bytes(*transferred);
                }
                let parkable = grace_ms.is_some() && dirs.iter().all(Dir::quiet);
                let wait_ms = match grace_ms {
                    Some(g) if parkable => g,
                    _ => timeout_ms,
                };
                match sys::poll_fds(&mut fds, wait_ms) {
                    Ok(0) if parkable => return Outcome::Idle,
                    Ok(0) if wait_ms >= 0 => {
                        log_trace!(Some(conn_id), "tunnel idle timeout after {}ms", wait_ms);
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log_trace!(Some(conn_id), "tunnel poll failed: {}", e);
                        break;
                    }
                }
                for d in dirs.iter_mut() {
                    // HUP / ERR でも read して EOF を確かめる
                    if fds[d.src].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
                        d.readable = true;
                    }
                }
            }

            log_trace!(
                Some(conn_id),
                "tunnel finished: {}B up / {}B down",
                dirs[0].moved,
                dirs[1].moved
            );
            Outcome::Done
        }
    }

    /// 中継を始める。暇になったら預け、預けられなければこのスレッドで続ける。
    pub(super) fn start(
        opened: Opened,
        idle: Option<Duration>,
        park: Option<(Arc<dyn Park>, Duration)>,
        hold: Box<dyn Send>,
    ) -> io::Result<()> {
        let Opened {
            client,
            server,
            info,
        } = opened;
        client.set_nonblocking(true)?;
        server.set_nonblocking(true)?;
        drive(Box::new(Idle {
            socks: [client, server],
            dirs: [Dir::new(0, 1), Dir::new(1, 0)],
            transferred: 0,
            idle,
            park,
            info,
            _hold: hold,
        }));
        Ok(())
    }

    /// 預かっていたトンネルをワーカーで再開する (事象が来て起こされたとき)。
    pub fn resume(idle: Box<Idle>) {
        if let Some(s) = idle.slot() {
            s.set_state(ConnState::Relaying);
        }
        drive(idle);
    }

    /// 期限切れで引き上げたトンネルを閉じる (`poll` が 0 を返したときと同じログと統計)。
    pub fn expire(idle: Box<Idle>) {
        log_trace!(Some(idle.id()), "tunnel idle timeout while parked");
        // 落ちるとソケットが閉じ、アクセスログと統計が出る
        drop(idle);
    }

    /// 暇になるまで回し、暇になったら預ける。
    ///
    /// 終わるとここで `Idle` が落ちる (= どのワーカーで終わっても、アクセスログと統計は
    /// 同じ場所を 1 回だけ通る)。
    fn drive(mut idle: Box<Idle>) {
        loop {
            match idle.run_until_idle() {
                Outcome::Done => return,
                Outcome::Idle => {
                    let Some((watch, _)) = idle.park.clone() else {
                        // 預け先が無ければ run_until_idle は Idle を返さない。
                        // 念のため、空回りせずに閉じる
                        return;
                    };
                    idle.release();
                    match watch.park(idle) {
                        // 預けられた: このスレッドは解放される (続きは監視スレッドが起こす)
                        Ok(()) => return,
                        Err(mut back) => {
                            // 預かってもらえなかった (監視スレッドが死んだ、記述子が epoll に
                            // 入らない)。以後はこのスレッドが最後まで面倒をみる
                            back.no_park();
                            idle = back;
                        }
                    }
                }
            }
        }
    }
}
