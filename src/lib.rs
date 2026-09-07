pub mod acl;
pub mod blocklist;
pub mod body;
pub mod cache;
pub mod cli;
pub mod clientio;
pub mod clock;
pub mod config;
pub mod dns;
pub mod endpoints;
pub mod envfile;
pub mod freshness;
pub mod headers;
pub mod history;
pub mod http;
pub mod httpdate;
pub mod json;
pub mod log;
pub mod metrics;
pub mod net;
pub mod origin;
pub mod persist;
pub mod pool;
pub mod prom;
pub mod reload;
pub mod rrd;
pub mod signal;
pub mod sync;
#[cfg(target_os = "linux")]
pub mod sys;
pub mod sysinfo;
pub mod tls;
pub mod tunnel;
pub mod workers;

use std::io::{self, BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use cache::Cache;
use config::Config;
use metrics::Metrics;
use pool::Pool;
use tls::TlsClient;

/// オリジンへ向かう側の共有状態 (接続プールと TLS クライアント)。
pub struct Upstream {
    pub pool: Pool,
    pub tls: Option<TlsClient>,
}

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
pub fn serve(
    listener: TcpListener,
    config_of: impl Fn() -> Arc<Config>,
    limiter: Arc<Limiter>,
    workers: Arc<workers::Workers>,
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    upstream: Arc<Upstream>,
) {
    // 待ち受けポートは接続ごとに引かない (accept したソケットのローカルポートは待ち受けと同じ)
    let local_port = listener
        .local_addr()
        .map(|a| a.port())
        .unwrap_or_else(|_| config_of().port);
    loop {
        // incoming() は accept() の戻り値のアドレスを捨てるので accept() を直接呼ぶ
        // (接続ごとの getpeername が 1 回減る)
        let (mut stream, peer) = match listener.accept() {
            Ok(v) => v,
            Err(e) => {
                log_error!(None, "accept failed: {}", e);
                continue;
            }
        };
        let cfg = config_of();
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
            let _ = stream.set_write_timeout(Some(cfg.timeout));
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
        let started = workers.run(Box::new(move || {
            struct OpenGuard(Arc<Limiter>);
            impl Drop for OpenGuard {
                fn drop(&mut self) {
                    self.0.open.fetch_sub(1, Ordering::Relaxed);
                }
            }
            let _open = OpenGuard(l);
            let accepted = Accepted { peer, local_port };
            if let Err(e) = handle_client(stream, accepted, cfg, m, c, p, conn_id) {
                if e.kind() != io::ErrorKind::UnexpectedEof
                    && e.kind() != io::ErrorKind::ConnectionReset
                    && e.kind() != io::ErrorKind::BrokenPipe
                {
                    log_error!(Some(conn_id), "{}", e);
                } else {
                    log_debug!(Some(conn_id), "connection ended: {}", e);
                }
            }
        }));
        if started.is_err() {
            limiter.open.fetch_sub(1, Ordering::Relaxed);
            log_error!(Some(conn_id), "failed to get a thread for the connection");
        }
    }
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
/// 要求を断ったあと、応答が RST で消えないように読み捨てる上限 (時間とバイト数)。
const LINGER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);
const LINGER_BYTES: usize = 64 * 1024;

thread_local! {
    /// 要求ヘッダーの行バッファ。1 接続 = 1 スレッドなので、要求ごとに `String` を作り直さず
    /// 容量ごと使い回す (解放せずに抱えておく)
    static HEADER_LINES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// スレッドから借りたヘッダー行の置き場。Drop で返すので途中で return しても失わない。
struct HeaderBuf {
    lines: Vec<String>,
    /// 今の要求で使っている行数
    used: usize,
}

impl HeaderBuf {
    fn take() -> HeaderBuf {
        HeaderBuf {
            lines: HEADER_LINES.with(|b| std::mem::take(&mut *b.borrow_mut())),
            used: 0,
        }
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

    /// 次の要求に備えて空にする。使い回すのは先頭 [`KEEP_LINES`] 本ぶんだけで、
    /// 大きく育った行は容量ごと手放す (大きなヘッダーを 1 回送られただけで、その接続の
    /// 間ずっとメモリを抱え込まないように)。
    fn recycle(&mut self) {
        self.lines.truncate(KEEP_LINES);
        for line in &mut self.lines {
            line.clear();
            line.shrink_to(KEEP_LINE_CAP);
        }
        self.used = 0;
    }

    fn headers(&self) -> &[String] {
        &self.lines[..self.used]
    }
}

impl Drop for HeaderBuf {
    fn drop(&mut self) {
        HEADER_LINES.with(|b| *b.borrow_mut() = std::mem::take(&mut self.lines));
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
pub fn handle_client(
    client: TcpStream,
    accepted: Accepted,
    config: Arc<Config>,
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    upstream: Arc<Upstream>,
    conn_id: usize,
) -> io::Result<()> {
    let started = Instant::now();
    metrics.inc_active_conn();

    struct ConnGuard(Arc<Metrics>, usize, Instant);
    impl Drop for ConnGuard {
        fn drop(&mut self) {
            self.0.dec_active_conn();
            log_debug!(
                Some(self.1),
                "connection closed after {:.1}ms (active={})",
                self.2.elapsed().as_secs_f64() * 1000.0,
                self.0
                    .active_connections
                    .load(std::sync::atomic::Ordering::Relaxed)
            );
        }
    }
    let _guard = ConnGuard(Arc::clone(&metrics), conn_id, started);

    // accept() が返したアドレスをそのまま使う (getpeername を呼ばない)
    let peer_addr = Some(net::canonical_addr(accepted.peer));
    let local_port = accepted.local_port;
    log_debug!(Some(conn_id), "accepted connection from {}", accepted.peer);
    client.set_write_timeout(Some(config.timeout))?;
    // Nagle を切る。応答ヘッダーと本文を別々に write すると delayed ACK と噛み合って
    // 1 要求あたり 40 ms 止まるため (失敗しても致命的ではないので無視する)
    let _ = client.set_nodelay(true);
    // 記述子を複製せず、バッファとストリームを分けて持つ
    // (アイドル中にバッファだけ残してストリームを手放せるようにするため)
    let mut buf = clientio::ClientBuf::new();
    let mut reader = buf.reader(&client);
    let mut served = 0usize;
    // 要求行とヘッダー行はこの接続の間ずっと使い回す (毎要求の確保をなくす)
    let mut request_line = String::new();
    let mut headers = HeaderBuf::take();
    // 今ソケットに設定してある読み取りタイムアウト (同じ値なら setsockopt を呼ばない)
    let mut read_timeout: Option<std::time::Duration> = None;

    loop {
        // 最初の要求は通常のタイムアウト、2 回目以降は keep-alive のアイドル時間で待つ
        let wait = if served == 0 {
            config.timeout
        } else {
            config.keepalive
        };
        // タイムアウトの再設定は値が変わるときだけ (setsockopt は要求ごとに効いてくる)
        if read_timeout != Some(wait) {
            client.set_read_timeout(Some(wait))?;
            read_timeout = Some(wait);
        }
        request_line.clear();
        request_line.shrink_to(KEEP_LINE_CAP);
        headers.recycle();
        match read_limited_line(&mut reader, &mut request_line) {
            Ok(None) => {
                log_warn!(
                    Some(conn_id),
                    "414 URI Too Long (request line over {} bytes)",
                    MAX_LINE
                );
                return reject(&client, 414, "URI Too Long");
            }
            Ok(Some(0)) => {
                log_debug!(Some(conn_id), "client closed ({} requests served)", served);
                return Ok(());
            }
            Ok(Some(_)) => {}
            Err(e)
                if served > 0
                    && matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::ConnectionReset
                    ) =>
            {
                log_debug!(Some(conn_id), "keep-alive idle timeout: {}", e);
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        // 要求の前の空行は読み飛ばす (RFC 9112 §2.2)
        if request_line.trim().is_empty() {
            continue;
        }
        metrics.inc_requests();
        log_trace!(Some(conn_id), "request line: {}", request_line.trim_end());

        let mut parts = request_line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
            log_warn!(
                Some(conn_id),
                "malformed request line: {:?}",
                request_line.trim()
            );
            return Ok(());
        };

        // Host の値は行の添字で覚えておき、読み終わってから借用する (複製しない)
        let mut host_line: Option<usize> = None;
        // ヘッダー全体の大きさ (要求行を含む)
        let mut header_bytes = request_line.len();
        // 本文付きの要求だけ、読み取りタイムアウトを本来の値に戻す
        let mut has_body = false;
        loop {
            let index = headers.used;
            match read_limited_line(&mut reader, headers.next())? {
                None => {
                    log_warn!(Some(conn_id), "431 Request Header Fields Too Large");
                    return reject(&client, 431, "Request Header Fields Too Large");
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
                        return reject(&client, 431, "Request Header Fields Too Large");
                    }
                }
            }
            if headers.lines[index].trim().is_empty() {
                break;
            }
            if index >= MAX_HEADER_LINES {
                log_warn!(
                    Some(conn_id),
                    "431 Request Header Fields Too Large (too many lines)"
                );
                return reject(&client, 431, "Request Header Fields Too Large");
            }
            if let Some((k, _)) = headers.lines[index].split_once(':') {
                let k = k.trim();
                if host_line.is_none() && k.eq_ignore_ascii_case("host") {
                    host_line = Some(index);
                } else if k.eq_ignore_ascii_case("content-length")
                    || k.eq_ignore_ascii_case("transfer-encoding")
                {
                    has_body = true;
                }
            }
            headers.commit();
        }
        // 要求行とヘッダーは keep-alive のアイドル時間で待っている。本文を読むならここで戻す
        if has_body && read_timeout != Some(config.timeout) {
            client.set_read_timeout(Some(config.timeout))?;
            read_timeout = Some(config.timeout);
        }
        let host_header: Option<&str> = host_line
            .and_then(|i| headers.lines[i].split_once(':'))
            .map(|(_, v)| v.trim());
        let raw_headers = headers.headers();

        // プロキシ自身のエンドポイント (/dashboard, /status, /metrics, /proxy.pac, /purge, /lookup, PURGE)
        let ep = endpoints::Endpoint {
            metrics: &metrics,
            cache: &cache,
            conn_id,
            // 実際に受けたポート (テストや複数 bind でも自分宛て判定が合うように)
            port: local_port,
            host: host_header,
            pac_direct: &config.pac_direct,
            lite: config.lite,
        };
        if endpoints::handle(&mut &client, method, target, &ep)? {
            return Ok(());
        }

        // ACL / Host Check
        let is_connect = method.eq_ignore_ascii_case("CONNECT");
        let target_host: std::borrow::Cow<'_, str> = if is_connect {
            std::borrow::Cow::Borrowed(target)
        } else {
            match http::parse_origin(target, host_header) {
                Ok(o) => std::borrow::Cow::Owned(o.host_port),
                Err(e) => {
                    log_warn!(Some(conn_id), "400 Bad Request: {}", e);
                    let _ = (&client).write_all(
                        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return Ok(());
                }
            }
        };
        let denied = if !config.acl.is_allowed(&target_host) {
            Some("ACL")
        } else if blocklist::is_blocked(net::split_host_port_ref(&target_host).0) {
            Some("blocklist")
        } else if is_connect
            && !config
                .connect_ports
                .allows(net::split_host_port_ref(&target_host).1.unwrap_or(443))
        {
            Some("CONNECT port")
        } else if !config.allow_local && acl::is_local_target(&target_host) {
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
            let client_ip = peer_addr
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|| "-".to_string());
            metrics.record_host(
                &format!("blocked://{}", net::split_host_port_ref(&target_host).0),
                metrics::HostOutcome::Blocked,
                0,
            );
            metrics.record_client(&client_ip, metrics::HostOutcome::Blocked, 0, None);
            (&client).write_all(FORBIDDEN_RESPONSE)?;
            (&client).flush()?;
            return Ok(());
        }

        if is_connect {
            // 先読みしてしまったバイト (TLS ClientHello など) はトンネルへ渡す。
            // 同時に読み取りバッファを手放す (トンネルの間は要求として読まないので、
            // アイドルのトンネルを大量に抱えるときの資源が減る)
            let prefix = reader.take_buffered();
            // 借用を終えて client を move できるようにする (ClientReader は Drop を持たない)
            #[allow(clippy::drop_non_drop)]
            drop(reader);
            return tunnel::handle_connect(
                client,
                target,
                &prefix,
                config.timeout,
                (!config.tunnel_idle.is_zero()).then_some(config.tunnel_idle),
                conn_id,
                Arc::clone(&metrics),
            );
        }

        let shared = http::Shared {
            timeout: config.timeout,
            keepalive: config.keepalive,
            conn_id,
            metrics: Arc::clone(&metrics),
            cache: Arc::clone(&cache),
            upstream: Arc::clone(&upstream),
        };
        let keep = http::handle_http_with_headers(
            &client,
            peer_addr,
            &request_line,
            raw_headers,
            &mut reader,
            &shared,
        )?;
        served += 1;
        if !keep || served >= MAX_REQUESTS_PER_CONNECTION {
            return Ok(());
        }
    }
}
