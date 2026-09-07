//! 認証なしの HTTP/HTTPS(CONNECT) フォワードプロキシ本体。
//!
//! 下の層は別クレートに分けてある ([`proxy_base`] / [`proxy_cache`] / [`proxy_stats`])。
//! **外部クレートは 1 つも使っていない** — 分けているのは、1 クレートが大きいと
//! `rustc` が全部を一度に抱えてビルドの最大 RSS がそのまま増えるため
//! (実測: 18,276 行 1 クレートで 330 MB、動作環境の上限は 200 MB)。

pub mod idle;

// 下の層をこのクレートの名前空間にも出す (`rust_http_proxy::config` のような書き方が、
// 本体でも結合テストでもそのまま通るようにするため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, sync,
};
pub use proxy_blocklist::blocklist;
pub use proxy_cache::cache;
pub use proxy_config::config;
pub use proxy_endpoints::endpoints;
pub use proxy_http::{freshness, http};
pub use proxy_metrics::{history, metrics, persist, rrd};
pub use proxy_msg::{body, clientio, headers, response};
pub use proxy_net::{acl, dns, net};
pub use proxy_origin::{Upstream, origin, pool, request, tls};
pub use proxy_prom::prom;
pub use proxy_reload::reload;
pub use proxy_sys::signal;
#[cfg(target_os = "linux")]
pub use proxy_sys::sys;
pub use proxy_sysinfo::sysinfo;
pub use proxy_tunnel::tunnel;
pub use proxy_workers::workers;

use std::io::{self, BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use cache::Cache;
use config::Config;
use metrics::Metrics;

const FORBIDDEN_RESPONSE: &[u8] = b"HTTP/1.1 403 Forbidden\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Length: 13\r\n\
Connection: close\r\n\
\r\n\
403 Forbidden";

/// accept したときに分かる接続の素性 (要求ごとにも接続ごとにも引き直さない)。
#[derive(Clone, Copy)]
pub struct Accepted {
    /// 相手のアドレス (`accept` の戻り値。`getpeername` を呼ばない)
    pub peer: std::net::SocketAddr,
    /// 受けた待ち受けポート (自分宛て判定に使う。`getsockname` を呼ばない)
    pub local_port: u16,
}

/// 接続の通し番号。
static CONN_COUNTER: AtomicUsize = AtomicUsize::new(1);

/// 同時接続数の見張り (待ち受けソケット全体で 1 つ共有する)。
#[derive(Default)]
pub struct Limiter {
    open: AtomicUsize,
    /// 上限に当たったことを最後に警告した時刻 (epoch 秒)。1 分に 1 回だけ出す
    warned: AtomicUsize,
}

impl Limiter {
    pub fn new() -> Arc<Limiter> {
        Arc::new(Limiter::default())
    }

    /// 今開いている接続の数。
    pub fn open(&self) -> usize {
        self.open.load(Ordering::Relaxed)
    }
}

const OVERLOAD_RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
Retry-After: 1\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Length: 23\r\n\
Connection: close\r\n\
\r\n\
503 Service Unavailable";

/// 待ち受けソケットから接続を受け、1 本ごとにスレッドを起こす。
/// `config_of` は接続ごとに最新の設定を取り出す (`.env` の再読込に追従するため)。
#[allow(clippy::too_many_arguments)]
pub fn serve(
    listener: TcpListener,
    config_of: impl Fn() -> Arc<Config>,
    limiter: Arc<Limiter>,
    workers: Arc<workers::Workers>,
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    upstream: Arc<Upstream>,
    park: Option<Arc<idle::IdleWatch>>,
) {
    // 待ち受けポートは接続ごとに引かない (accept したソケットのローカルポートは待ち受けと同じ)
    let local_port = listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or_else(|_| config_of().port);
    // Linux では accept したソケットが待ち受けの TCP_NODELAY / SO_RCVTIMEO / SO_SNDTIMEO を
    // 引き継ぐ。待ち受けに 1 回当てておけば、接続ごとの setsockopt 3 回が要らない (T9.3)。
    // 当たらなかった環境では None のままで、従来どおり接続ごとに設定する
    let mut inherited = inherit_on_listener(&listener, config_of().timeout);
    loop {
        // incoming() は accept() の戻り値のアドレスを捨てるので accept() を直接呼ぶ
        // (接続ごとの getpeername が 1 回減る)
        let (mut stream, peer) = match listener.accept() {
            Ok(v) => v,
            // 待ち受けに載せた SO_RCVTIMEO は accept() にも効くので、接続が来ないまま
            // timeout 秒たつと WouldBlock で戻ってくる。これは異常ではないので
            // ログも待ちも無しに待ち直す (下の「その他のエラー」より必ず先に拾うこと)。
            // 割り込みと「相手が accept 前に切った」も同じくすぐ次へ
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                        | io::ErrorKind::ConnectionAborted
                ) =>
            {
                continue;
            }
            Err(e) => {
                // 記述子を使い切ったとき (EMFILE/ENFILE) は何度呼んでも同じ失敗が返る。
                // そのまま回すと 1 コアを 100% 使いながらログを溢れさせるので少し待つ
                log_error!(None, "accept failed: {}", e);
                std::thread::sleep(ACCEPT_ERROR_BACKOFF);
                continue;
            }
        };
        let cfg = config_of();
        // この接続が継承したのは「今 待ち受けに当たっている値」。.env の再読込で timeout が
        // 変わった直後だけは食い違うので、その接続は従来どおり接続ごとに設定する
        let conn_inherited = inherited.filter(|t| *t == cfg.timeout);
        if conn_inherited.is_none() && inherited.is_some() {
            // 待ち受けに当て直す (次の接続から効く)
            inherited = inherit_on_listener(&listener, cfg.timeout);
        }
        // 上限を超えたらスレッドを起こさずに 503 を返して閉じる
        let max = cfg.max_conns;
        if max > 0 && limiter.open() >= max {
            metrics
                .rejected_overload
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let now = cache::now_epoch() as usize;
            let last = limiter.warned.load(Ordering::Relaxed);
            if now.saturating_sub(last) >= 60
                && limiter
                    .warned
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                log_warn!(
                    None,
                    "connection limit reached ({} open, PROXY_MAX_CONNS={}); returning 503",
                    limiter.open(),
                    max
                );
            }
            if conn_inherited.is_none() {
                let _ = stream.set_write_timeout(Some(cfg.timeout));
            }
            let _ = stream.write_all(OVERLOAD_RESPONSE);
            let _ = stream.flush();
            continue;
        }
        let conn_id = CONN_COUNTER.fetch_add(1, Ordering::Relaxed);
        limiter.open.fetch_add(1, Ordering::Relaxed);
        let m = Arc::clone(&metrics);
        let l = Arc::clone(&limiter);
        let c = Arc::clone(&cache);
        let p = Arc::clone(&upstream);
        let w = park.clone();
        let started = workers.run(Box::new(move || {
            // 同時接続数と active_connections の持ち分は Conn が持つ (接続の寿命と一致させる)
            let accepted = Accepted { peer, local_port };
            match Conn::new(
                stream,
                accepted,
                l,
                cfg,
                m,
                c,
                p,
                w,
                conn_inherited,
                conn_id,
            ) {
                Ok(conn) => run_conn(Box::new(conn)),
                Err(e) => log_error!(Some(conn_id), "{}", e),
            }
        }));
        if started.is_err() {
            limiter.open.fetch_sub(1, Ordering::Relaxed);
            log_error!(Some(conn_id), "failed to get a thread for the connection");
        }
    }
}

/// 待ち受けソケットに「accept した接続へ引き継がせるオプション」を当てる。
/// 当たったら継承させた `timeout` を返す (当たらなければ `None` = 接続ごとに設定する)。
fn inherit_on_listener(
    listener: &TcpListener,
    timeout: std::time::Duration,
) -> Option<std::time::Duration> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        match sys::inherit_socket_options(listener.as_raw_fd(), timeout) {
            Ok(()) => return Some(timeout),
            Err(e) => log_debug!(
                None,
                "listener socket options not inherited ({}); setting them per connection",
                e
            ),
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (listener, timeout);
    None
}

/// 1 つのクライアント接続で処理する最大要求数 (keep-alive)。
const MAX_REQUESTS_PER_CONNECTION: usize = 1000;
/// 要求行・ヘッダー行 1 本の最大長と、ヘッダー行数の上限 (超えたら 414 / 431)。
const MAX_LINE: usize = 64 * 1024;
const MAX_HEADER_LINES: usize = 256;
/// 1 要求のヘッダー全体 (要求行 + ヘッダー行) の合計上限。
///
/// 1 行の上限だけだと `MAX_LINE` の行を `MAX_HEADER_LINES` 本並べられ、認証なしの
/// 開放プロキシでは正常な形の要求を数本送るだけでメモリを食い潰せる
/// (実測: 同時 8 接続で RSS 15 MiB → 272 MiB)。要求行 1 本 + 最大長のヘッダー 1 本は通る幅。
const MAX_HEADER_BYTES: usize = 128 * 1024;
/// 次の要求のために抱えておく行数と 1 行の容量。これを超えたぶんは要求ごとに解放する
/// (使い回しの利得はほぼそのままで、接続あたりの居座りを 32 KiB 程度に抑える)。
const KEEP_LINES: usize = 32;
const KEEP_LINE_CAP: usize = 1024;
/// accept が失敗したときに次の試行まで待つ時間 (記述子切れでの空回りを止める)。
const ACCEPT_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

/// 要求を断ったあと、応答が RST で消えないように読み捨てる上限 (時間とバイト数)。
const LINGER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);
const LINGER_BYTES: usize = 64 * 1024;

thread_local! {
    /// 要求行とヘッダー行の置き場。要求ごとに `String` を作り直さず容量ごと使い回す。
    ///
    /// 接続ではなく**スレッド**に置くのは、アイドルの接続を別のワーカーへ預けられるように
    /// するため (接続に持たせると、預けている間ずっと抱えることになる)。
    /// **`Drop` を持つ型をここに置かないこと。** スレッドが終わるときこの置き場自体が
    /// 破棄され、その最中に `SCRATCH.with` を呼ぶと `AccessError` で panic し、
    /// 「thread local panicked on drop」でプロセスごと abort する。
    /// 置くのは中身 (行の Vec と要求行) だけにして、[`Scratch`] は借りた側だけが持つ。
    static SCRATCH: std::cell::RefCell<(Vec<String>, String)> =
        const { std::cell::RefCell::new((Vec::new(), String::new())) };
}

/// スレッドから借りた要求行とヘッダー行の置き場。Drop で返すので途中で return しても失わない。
struct Scratch {
    lines: Vec<String>,
    /// 今の要求で使っている行数
    used: usize,
    request_line: String,
}

impl Scratch {
    /// 次の要求に備えて空にする。前の要求で大きく育った行は容量ごと手放す
    /// (使い回すのは先頭 [`KEEP_LINES`] 本 × [`KEEP_LINE_CAP`] まで。大きなヘッダーを
    /// 1 回送られただけでメモリを抱え込まないように)。
    fn reset(&mut self) {
        self.lines.truncate(KEEP_LINES);
        for line in &mut self.lines {
            line.clear();
            line.shrink_to(KEEP_LINE_CAP);
        }
        self.used = 0;
        self.request_line.clear();
        self.request_line.shrink_to(KEEP_LINE_CAP);
    }

    /// 借りる。前の要求で大きく育った行は、この時点で容量ごと手放す
    /// (使い回すのは先頭 [`KEEP_LINES`] 本 × [`KEEP_LINE_CAP`] まで。大きなヘッダーを
    /// 1 回送られただけでスレッドがメモリを抱え込まないように)。
    fn take() -> Scratch {
        let (lines, request_line) = SCRATCH.with(|b| std::mem::take(&mut *b.borrow_mut()));
        let mut me = Scratch {
            lines,
            used: 0,
            request_line,
        };
        me.reset();
        me
    }

    /// 次の行を書き込む先 (中身は空、容量は残っている)。
    fn next(&mut self) -> &mut String {
        if self.used == self.lines.len() {
            self.lines.push(String::new());
        }
        let line = &mut self.lines[self.used];
        line.clear();
        line
    }

    fn commit(&mut self) {
        self.used += 1;
    }

    fn headers(&self) -> &[String] {
        &self.lines[..self.used]
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let lines = std::mem::take(&mut self.lines);
        let request_line = std::mem::take(&mut self.request_line);
        // スレッドが終わりかけていると置き場はもう無い。with だと panic するので try_with
        let _ = SCRATCH.try_with(|b| {
            if let Ok(mut slot) = b.try_borrow_mut() {
                *slot = (lines, request_line);
            }
        });
    }
}

/// [`read_line_or_idle`] の結果。
enum Line {
    /// 読めた ([`read_limited_line`] の戻り値そのまま)
    Read(Option<usize>),
    /// 猶予のあいだ 1 バイトも来なかった (= 預けてよい)
    Idle,
}

/// 猶予つきで 1 行読む。
///
/// 読み取りタイムアウトを猶予の長さにしておき、空振りしたらそれを「暇だ」と解釈する。
/// `poll` を別に呼ばずに済むので、要求ごとのシステムコールが 1 回増えない。
/// 途中まで来ていたら (要求を送っている最中の細切れ) `full` まで待ち直して読み切る。
///
/// `extend` が `None` なら猶予なし。`allow_idle` が偽なら空振りでも待ち直す
/// (要求の途中では預けられないため)。
fn read_line_or_idle(
    reader: &mut clientio::ClientReader<'_>,
    line: &mut String,
    extend: Option<(&TcpStream, std::time::Duration)>,
    allow_idle: bool,
    extended: &mut bool,
) -> io::Result<Line> {
    loop {
        match read_limited_line(reader, line) {
            Ok(v) => return Ok(Line::Read(v)),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                let Some((stream, full)) = extend else {
                    return Err(e);
                };
                if *extended {
                    return Err(e);
                }
                if allow_idle && line.is_empty() {
                    return Ok(Line::Idle);
                }
                // 要求の途中: 猶予ではなく本来のアイドル時間で待ち直す
                stream.set_read_timeout(Some(full))?;
                *extended = true;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 長さ制限付きで 1 行読む。制限を超えたら `Ok(None)`。
fn read_limited_line(
    reader: &mut clientio::ClientReader<'_>,
    line: &mut String,
) -> io::Result<Option<usize>> {
    let n = reader.by_ref().take(MAX_LINE as u64).read_line(line)?;
    if n == MAX_LINE && !line.ends_with('\n') {
        return Ok(None);
    }
    Ok(Some(n))
}

fn reject(client: &TcpStream, status: u16, reason: &str) -> io::Result<()> {
    let mut client = client;
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        status, reason
    );
    client.write_all(resp.as_bytes())?;
    client.flush()?;
    // 相手がまだ送っている途中で閉じると RST になり、書いた応答ごと捨てられてクライアントは
    // 理由が分からない。少しだけ読み捨ててから閉じる (時間もバイト数も上限つきなので、
    // これ自体を居座りに使うことはできない)
    let _ = client.set_read_timeout(Some(LINGER_TIMEOUT));
    let mut sink = [0u8; 8 * 1024];
    let mut drained = 0usize;
    while drained < LINGER_BYTES {
        match client.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
    Ok(())
}

/// 1 つのクライアント接続を、keep-alive なら複数の要求にわたって処理する。
/// 1 接続ぶんの状態。ワーカーが替わっても箱ごと持ち運べるように、借用を持たない。
///
/// **`Drop` は実装しないこと。** [`Step::Connect`] で `client` だけを取り出す
/// 部分ムーブに依存している (`Drop` があると部分ムーブができない)。
pub struct Conn {
    client: TcpStream,
    /// 先読みバッファ (パイプライン化された次の要求はここに残る)
    buf: clientio::ClientBuf,
    accepted: Accepted,
    config: Arc<Config>,
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    upstream: Arc<Upstream>,
    conn_id: usize,
    /// この接続で処理した要求の数
    served: usize,
    /// 今ソケットに設定してある読み取りタイムアウト (同じ値なら setsockopt を呼ばない)
    read_timeout: Option<std::time::Duration>,
    /// 要求行とヘッダー行の置き場。処理している間だけ持ち、預けるときはスレッドへ返す
    scratch: Option<Scratch>,
    /// アイドルのときに預ける先 (無ければ従来どおりこのスレッドがブロッキング read で待つ)
    park: Option<Arc<idle::IdleWatch>>,
    /// 接続元の IP を文字列にしたもの (X-Forwarded-For と統計に毎要求要るので接続ごとに 1 回だけ作る)
    peer_ip: String,
    /// 同時接続数と `/status` の active_connections の持ち分 (接続の寿命と一致させる)
    _open: OpenGuard,
    _active: ActiveGuard,
}

/// [`serve_one`] の結果。
pub enum Step {
    /// この接続で次の要求を待つ (keep-alive)
    Next,
    /// 猶予のあいだ次の要求が来なかった。監視スレッドへ預けてスレッドを解放する
    Park,
    /// この接続は終わり
    Close,
    /// CONNECT トンネルへ移る
    Connect { target: String, prefix: Vec<u8> },
}

/// 同時接続数の持ち分。
struct OpenGuard(Arc<Limiter>);

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::Relaxed);
    }
}

/// `/status` の active_connections の持ち分。
struct ActiveGuard {
    metrics: Arc<Metrics>,
    conn_id: usize,
    started: Instant,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.metrics.dec_active_conn();
        log_debug!(
            Some(self.conn_id),
            "connection closed after {:.1}ms (active={})",
            self.started.elapsed().as_secs_f64() * 1000.0,
            self.metrics
                .active_connections
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }
}

impl Conn {
    /// accept した接続から作る。
    /// 同時接続数と active_connections はここから数え始める。
    ///
    /// `inherited` が `Some(t)` なら、待ち受けから `TCP_NODELAY` / `SO_RCVTIMEO` / `SO_SNDTIMEO`
    /// (いずれも `t`) を引き継いでいるので、接続ごとの `setsockopt` は 1 回も要らない (T9.3)。
    #[allow(clippy::too_many_arguments)]
    fn new(
        client: TcpStream,
        accepted: Accepted,
        limiter: Arc<Limiter>,
        config: Arc<Config>,
        metrics: Arc<Metrics>,
        cache: Arc<Cache>,
        upstream: Arc<Upstream>,
        park: Option<Arc<idle::IdleWatch>>,
        inherited: Option<std::time::Duration>,
        conn_id: usize,
    ) -> io::Result<Conn> {
        metrics.inc_active_conn();
        let active = ActiveGuard {
            metrics: Arc::clone(&metrics),
            conn_id,
            started: Instant::now(),
        };
        log_debug!(Some(conn_id), "accepted connection from {}", accepted.peer);
        if inherited.is_none() {
            client.set_write_timeout(Some(config.timeout))?;
            // Nagle を切る。応答ヘッダーと本文を別々に write すると delayed ACK と噛み合って
            // 1 要求あたり 40 ms 止まるため (失敗しても致命的ではないので無視する)
            let _ = client.set_nodelay(true);
        }
        Ok(Conn {
            client,
            buf: clientio::ClientBuf::new(),
            accepted,
            config,
            metrics,
            cache,
            upstream,
            conn_id,
            served: 0,
            // 継承していれば読み取りタイムアウトはもう載っている (1 要求目の setsockopt が省ける)
            read_timeout: inherited,
            scratch: None,
            park,
            peer_ip: net::canonical_addr(accepted.peer).ip().to_string(),
            _open: OpenGuard(limiter),
            _active: active,
        })
    }

    /// 先読みしたバイトが残っているか (残っていたら次の要求を待ってはいけない)。
    pub fn has_buffered(&self) -> bool {
        self.buf.has_buffered()
    }

    /// クライアント側のソケット記述子 (epoll に入れるときの鍵)。
    pub fn client_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.client.as_raw_fd()
    }

    /// ログ用の接続番号。
    pub fn id(&self) -> usize {
        self.conn_id
    }

    /// 要求と要求の間に抱えている資源を手放す (別のワーカーへ預ける前に呼ぶ)。
    pub fn release_idle_buffers(&mut self) {
        self.scratch = None;
        self.buf.release();
    }
}

/// 1 つのクライアント接続を、keep-alive なら複数の要求にわたって最後まで処理する。
fn pump(mut conn: Box<Conn>) -> io::Result<()> {
    loop {
        match serve_one(&mut conn)? {
            // 猶予 0 の設定では読みにも行かず、その場で預ける
            Step::Next if conn.park.is_some() && conn.config.park_grace.is_zero() => {
                match park_now(conn) {
                    Ok(()) => return Ok(()),
                    Err(back) => conn = back,
                }
            }
            Step::Next => continue,
            // 猶予のあいだ次の要求が来なかった
            Step::Park => match park_now(conn) {
                // 預けられた: このスレッドは解放される (続きは監視スレッドが起こす)
                Ok(()) => return Ok(()),
                Err(back) => conn = back,
            },
            Step::Close => return Ok(()),
            Step::Connect { target, prefix } => {
                // client だけ取り出してトンネルへ渡す (残りのフィールドは
                // この式が終わってから落ちるので、持ち分は最後まで立ったまま)
                let conn = *conn;
                let (timeout, idle) = (
                    conn.config.timeout,
                    (!conn.config.tunnel_idle.is_zero()).then_some(conn.config.tunnel_idle),
                );
                let (conn_id, metrics) = (conn.conn_id, Arc::clone(&conn.metrics));
                return tunnel::handle_connect(
                    conn.client,
                    &target,
                    &prefix,
                    timeout,
                    idle,
                    conn_id,
                    metrics,
                );
            }
        }
    }
}

/// 接続を監視スレッドへ預けてこのスレッドを解放する。
/// 預けられたら `Ok(())`、このまま同じスレッドで待つなら `Err(conn)`。
///
/// 「暇かどうか」の判定は済んでいる前提 ([`Step::Park`] で来るか、猶予 0 の設定)。
fn park_now(mut conn: Box<Conn>) -> Result<(), Box<Conn>> {
    // 先読み済みのバイトがあるなら待つ必要が無い (パイプライン化された次の要求)
    if conn.has_buffered() {
        return Err(conn);
    }
    // keep-alive を切っている設定なら、預けても期限切れで閉じるだけ
    if conn.config.keepalive.is_zero() {
        return Err(conn);
    }
    let Some(watch) = conn.park.clone() else {
        return Err(conn);
    };
    let deadline = Instant::now() + conn.config.keepalive;
    conn.release_idle_buffers();
    match watch.park(conn, deadline) {
        Ok(()) => Ok(()),
        Err(mut conn) => {
            // 預かってもらえなかった (監視スレッドが死んだ、この記述子が epoll に
            // 入らない)。この接続は以後スレッドで待つ側に固定する。そうしないと
            // 要求のたびに猶予で空振りして預け直そうとして空回りする
            conn.park = None;
            Err(conn)
        }
    }
}

/// 1 つの接続を最後まで面倒みる。ワーカースレッドに渡す仕事の中身。
/// 監視スレッドから戻ってきた接続もここに入る。
pub fn run_conn(conn: Box<Conn>) {
    let conn_id = conn.conn_id;
    if let Err(e) = pump(conn) {
        if e.kind() != io::ErrorKind::UnexpectedEof
            && e.kind() != io::ErrorKind::ConnectionReset
            && e.kind() != io::ErrorKind::BrokenPipe
        {
            log_error!(Some(conn_id), "{}", e);
        } else {
            log_debug!(Some(conn_id), "connection ended: {}", e);
        }
    }
}

/// 要求を 1 つ処理する。次に何をするかを返す。
fn serve_one(conn: &mut Conn) -> io::Result<Step> {
    let (conn_id, local_port) = (conn.conn_id, conn.accepted.local_port);
    // 要求行とヘッダー行の置き場。持っていなければ今のスレッドから借りる
    if conn.scratch.is_none() {
        conn.scratch = Some(Scratch::take());
    }
    let Conn {
        client,
        buf,
        scratch,
        config,
        metrics,
        cache,
        upstream,
        served,
        read_timeout,
        park,
        peer_ip,
        ..
    } = conn;
    let peer_ip: &str = peer_ip;
    let scratch = scratch.as_mut().expect("just set");
    scratch.reset();

    // 2 回目以降で監視スレッドに預けられるなら、待つのは猶予のあいだだけ。
    // 空振りしたら「暇だ」と解釈して預ける (poll を別に呼ばずに済む)
    let grace = (*served > 0
        && park.is_some()
        && !config.park_grace.is_zero()
        && !config.keepalive.is_zero())
    .then_some(config.park_grace);
    // 最初の要求は通常のタイムアウト、2 回目以降は keep-alive のアイドル時間で待つ
    let wait = match (grace, *served) {
        (Some(g), _) => g,
        (None, 0) => config.timeout,
        (None, _) => config.keepalive,
    };
    // タイムアウトの再設定は値が変わるときだけ (setsockopt は要求ごとに効いてくる)
    if *read_timeout != Some(wait) {
        client.set_read_timeout(Some(wait))?;
        *read_timeout = Some(wait);
    }
    // 猶予で待っている間に要求が届き始めたら、本来のアイドル時間まで待ち直す
    let extend = grace.map(|_| (&*client, config.keepalive));
    let mut extended = false;
    let mut reader = buf.reader(client);
    match read_line_or_idle(
        &mut reader,
        &mut scratch.request_line,
        extend,
        true,
        &mut extended,
    ) {
        Ok(Line::Idle) => return Ok(Step::Park),
        Ok(Line::Read(None)) => {
            log_warn!(
                Some(conn_id),
                "414 URI Too Long (request line over {} bytes)",
                MAX_LINE
            );
            reject(client, 414, "URI Too Long")?;
            return Ok(Step::Close);
        }
        Ok(Line::Read(Some(0))) => {
            log_debug!(Some(conn_id), "client closed ({} requests served)", *served);
            return Ok(Step::Close);
        }
        Ok(Line::Read(Some(_))) => {}
        Err(e)
            if *served > 0
                && matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::ConnectionReset
                ) =>
        {
            log_debug!(Some(conn_id), "keep-alive idle timeout: {}", e);
            return Ok(Step::Close);
        }
        Err(e) => return Err(e),
    }
    // 要求の前の空行は読み飛ばす (RFC 9112 §2.2)
    if scratch.request_line.trim().is_empty() {
        return Ok(Step::Next);
    }
    metrics.inc_requests();
    log_trace!(
        Some(conn_id),
        "request line: {}",
        scratch.request_line.trim_end()
    );

    let request_line = std::mem::take(&mut scratch.request_line);
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        log_warn!(
            Some(conn_id),
            "malformed request line: {:?}",
            request_line.trim()
        );
        scratch.request_line = request_line;
        return Ok(Step::Close);
    };

    // Host の値は行の添字で覚えておき、読み終わってから借用する (複製しない)
    let mut host_line: Option<usize> = None;
    // ヘッダー全体の大きさ (要求行を含む)
    let mut header_bytes = request_line.len();
    // 本文付きの要求だけ、読み取りタイムアウトを本来の値に戻す
    let mut has_body = false;
    loop {
        let index = scratch.used;
        // 要求の途中なので、空振りしても預けない (待ち直す)
        let line =
            match read_line_or_idle(&mut reader, scratch.next(), extend, false, &mut extended)? {
                Line::Read(v) => v,
                Line::Idle => unreachable!("allow_idle is false"),
            };
        match line {
            None => {
                log_warn!(Some(conn_id), "431 Request Header Fields Too Large");
                reject(client, 431, "Request Header Fields Too Large")?;
                return Ok(Step::Close);
            }
            Some(0) => break,
            Some(n) => {
                header_bytes += n;
                if header_bytes > MAX_HEADER_BYTES {
                    log_warn!(
                        Some(conn_id),
                        "431 Request Header Fields Too Large (headers over {} bytes)",
                        MAX_HEADER_BYTES
                    );
                    reject(client, 431, "Request Header Fields Too Large")?;
                    return Ok(Step::Close);
                }
            }
        }
        if scratch.lines[index].trim().is_empty() {
            break;
        }
        if index >= MAX_HEADER_LINES {
            log_warn!(
                Some(conn_id),
                "431 Request Header Fields Too Large (too many lines)"
            );
            reject(client, 431, "Request Header Fields Too Large")?;
            return Ok(Step::Close);
        }
        if let Some((k, _)) = scratch.lines[index].split_once(':') {
            let k = k.trim();
            if host_line.is_none() && k.eq_ignore_ascii_case("host") {
                host_line = Some(index);
            } else if k.eq_ignore_ascii_case("content-length")
                || k.eq_ignore_ascii_case("transfer-encoding")
            {
                has_body = true;
            }
        }
        scratch.commit();
    }
    // 要求行とヘッダーは keep-alive のアイドル時間で待っている。本文を読むならここで戻す
    if has_body && *read_timeout != Some(config.timeout) {
        client.set_read_timeout(Some(config.timeout))?;
        *read_timeout = Some(config.timeout);
    }
    let host_header: Option<&str> = host_line
        .and_then(|i| scratch.lines[i].split_once(':'))
        .map(|(_, v)| v.trim());
    let raw_headers = scratch.headers();

    // プロキシ自身のエンドポイント (/dashboard, /status, /metrics, /proxy.pac, /purge, /lookup, PURGE)
    let ep = endpoints::Endpoint {
        metrics,
        cache,
        conn_id,
        // 実際に受けたポート (テストや複数 bind でも自分宛て判定が合うように)
        port: local_port,
        host: host_header,
        pac_direct: &config.pac_direct,
        lite: config.lite,
    };
    if endpoints::handle(&mut &*client, method, target, &ep)? {
        return Ok(Step::Close);
    }

    // ACL / Host Check
    let is_connect = method.eq_ignore_ascii_case("CONNECT");
    // 判定に要るのはホストだけなので、要求行から借りる (String を 2 本作らない)
    let target_host: &str = if is_connect {
        target
    } else {
        match request::target_host(target, host_header) {
            Ok(h) => h,
            Err(e) => {
                log_warn!(Some(conn_id), "400 Bad Request: {}", e);
                let _ = (&*client).write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return Ok(Step::Close);
            }
        }
    };
    let (bare_host, host_port) = net::split_host_port_ref(target_host);
    let denied = if !config.acl.is_allowed(target_host) {
        Some("ACL")
    } else if blocklist::is_blocked(bare_host) {
        Some("blocklist")
    } else if is_connect && !config.connect_ports.allows(host_port.unwrap_or(443)) {
        Some("CONNECT port")
    } else if !config.allow_local && acl::is_local_target(target_host) {
        // クラウドのメタデータ (169.254.169.254) 経由の SSRF を止める
        Some("local address")
    } else {
        None
    };
    if let Some(why) = denied {
        log_warn!(
            Some(conn_id),
            "403 Forbidden ({} blocked host: {})",
            why,
            target_host
        );
        metrics.record_host(
            &format!("blocked://{}", bare_host),
            metrics::HostOutcome::Blocked,
            0,
        );
        metrics.record_client(peer_ip, metrics::HostOutcome::Blocked, 0, None);
        (&*client).write_all(FORBIDDEN_RESPONSE)?;
        (&*client).flush()?;
        return Ok(Step::Close);
    }

    if is_connect {
        // 先読みしてしまったバイト (TLS ClientHello など) はトンネルへ渡す。
        // 同時に読み取りバッファを手放す (トンネルの間は要求として読まないので、
        // アイドルのトンネルを大量に抱えるときの資源が減る)
        let prefix = reader.take_buffered();
        return Ok(Step::Connect {
            target: target.to_string(),
            prefix,
        });
    }

    let shared = http::Shared {
        timeout: config.timeout,
        keepalive: config.keepalive,
        conn_id,
        metrics: Arc::clone(metrics),
        cache: Arc::clone(cache),
        upstream: Arc::clone(upstream),
    };
    let keep = http::handle_http_with_headers(
        client,
        Some(peer_ip),
        &request_line,
        raw_headers,
        &mut reader,
        &shared,
    )?;
    scratch.request_line = request_line;
    *served += 1;
    if !keep || *served >= MAX_REQUESTS_PER_CONNECTION {
        Ok(Step::Close)
    } else {
        Ok(Step::Next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 置き場を借りたスレッドが終わっても落ちないこと。
    ///
    /// スレッドローカルの破棄中にその置き場自身を触ると、std は
    /// 「thread local panicked on drop」で**プロセスごと abort** する。
    /// 回帰するとこのテストはテスト失敗ではなくテストバイナリの異常終了になる。
    #[test]
    fn scratch_survives_the_thread_that_borrowed_it() {
        let h = std::thread::spawn(|| {
            let mut scratch = Scratch::take();
            scratch.request_line.push_str("GET / HTTP/1.1\r\n");
            scratch.next().push_str("Host: example.com\r\n");
            scratch.commit();
            assert_eq!(scratch.headers().len(), 1);
            // ここで置き場はスレッドへ戻る。このあとスレッドが終わり、置き場が破棄される
        });
        h.join().expect("the worker thread must exit cleanly");
    }

    /// 借りて返すと、行の容量は使い回されるが中身は残らない。
    #[test]
    fn scratch_is_recycled_but_cleared() {
        let mut first = Scratch::take();
        first.next().push_str("X-A: 1");
        first.commit();
        drop(first);
        let second = Scratch::take();
        assert_eq!(second.headers().len(), 0, "前の要求の行は見えない");
        assert!(!second.lines.is_empty(), "容量は使い回す");
        assert!(second.lines[0].is_empty());
    }
}
