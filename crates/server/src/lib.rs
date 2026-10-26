//! 認証なしの HTTP/HTTPS(CONNECT) フォワードプロキシ本体。
//!
//! 下の層は別クレートに分けてある ([`proxy_base`] / [`proxy_cache`] / [`proxy_metrics`])。
//! **外部クレートは 1 つも使っていない** — 分けているのは、1 クレートが大きいと
//! `rustc` が全部を一度に抱えてビルドの最大 RSS がそのまま増えるため
//! (実測: 18,276 行 1 クレートで 330 MB、動作環境の上限は 120 MB)。

mod conn;
pub mod idle;

pub use conn::{Conn, Step, attach_origin_rtt, run_conn};
use conn::{OpenGuard, OverflowGuard};

// 下の層をこのクレートの名前空間にも出す (`rust_http_proxy::config` のような書き方が、
// 本体でも結合テストでもそのまま通るようにするため)。
pub use proxy_base::{
    cli, clock, envfile, httpdate, json, log, log_at, log_debug, log_error, log_info, log_trace,
    log_warn, sync, timeout, via,
};
pub use proxy_blocklist::blocklist;
pub use proxy_cache::cache;
pub use proxy_config::config;
pub use proxy_endpoints::endpoints;
pub use proxy_http::{freshness, http};
pub use proxy_metrics::{
    anomaly, canary, daily, events, history, hostseries, kernel, metrics, persist, persist_recent,
    profile, recent, rrd, slo, snapshots, trace,
};
pub use proxy_msg::{body, clientio, headers, response};
pub use proxy_net::{acl, dns, net};
pub use proxy_origin::{Upstream, origin, pool, request, tls};
pub use proxy_prom::prom;
pub use proxy_reload::reload;
pub use proxy_selfbench as selfbench;
pub use proxy_sys::signal;
#[cfg(target_os = "linux")]
pub use proxy_sys::sys;
pub use proxy_sysinfo::sysinfo;
pub use proxy_tunnel::{sni, tunnel};
pub use proxy_workers::workers;

use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cache::Cache;
use config::Config;
use metrics::Metrics;

/// 動いているバイナリの版 (`build.rs` が作る `PROXY_VERSION`)。
///
/// `CARGO_PKG_VERSION` + git の短いハッシュ (`0.18.0+144b992`、作業ツリーが汚れていれば
/// `-dirty`)。git や `.git` の無い環境でビルドしたときは `0.18.0+unknown`。
/// **デプロイ先でどのコミットが動いているかを知るため**のもので、起動ログ・`-V`・
/// `/status` の 3 か所に同じ文字列を出す (T12.6)。
pub const VERSION: &str = env!("PROXY_VERSION");

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
    /// 上限の外で受けている「自分宛てかもしれない」接続の数 (T13.2)
    overflow: AtomicUsize,
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

/// 上限に当たっていても受ける「自分宛てかもしれない」接続の本数 (T13.2)。
///
/// accept の時点では要求が読めないので、上限に当たって席も作れないときは、この本数までは
/// 受けてスレッドを起こし、**要求行と `Host` を読んでから**決める (内部エンドポイント =
/// T12.3 の自分宛て判定なら普通に応答、それ以外は 503 で閉じる)。狙いは
/// 「上限に当たっている最中でも `/status` が取れる」ことだけなので枠は小さくてよい
/// (1 本では、監視が取りに来たときに他の接続が枠を使っていると空振りする)。
const OVERFLOW_SLOTS: usize = 4;

/// 上限の外で受けた接続が要求行を送ってくるのを待つ上限 (T13.2)。
///
/// 枠は [`OVERFLOW_SLOTS`] 本しかないので、黙ったままの接続に長く占領させない
/// (占領されると監視が `/status` を取れない)。`PROXY_TIMEOUT_SECS` の方が短ければそちら。
const OVERFLOW_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

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
    // 当たらなかった環境では None のままで、従来どおり接続ごとに設定する。
    // timeout 0 (= 無期限、T10.6) も `timeval {0, 0}` としてそのまま継承させる。
    // 読みは 1 要求目の要求行を待つ長さ (`PROXY_HEADER_TIMEOUT_SECS` との小さい方。T.28.0)、
    // 書きは `PROXY_TIMEOUT_SECS`。持つのは `(書き, 読み)` の組
    let mut inherited = inherit_on_listener(&listener, &config_of());
    // 当てる前に握手を終えて accept 待ちの列に載っている接続は、待ち受けのオプションを
    // 継承していない (Linux は握手が済んだ時点で待ち受けを写す)。起動直後に再接続してくる
    // クライアントがこれを踏むと無期限のまま扱われるので、いま列にある分を先に引き取り、
    // その接続だけ接続ごとに設定する (T.28.30)。費用は起動時と当て直したときの数回だけ
    let mut backlog = take_backlog(&listener);
    // 日付の変わり目に `/snapshot` を 1 枚組む閉包を履歴スレッドへ預ける (T14.34)。
    // 組み立ては要求で来たときと同じ関数で、**下の層 (`proxy-metrics`) から上の層を
    // 呼ばない**ためにここで預ける。書くかどうかを決めるのは `snapshots::configure`
    // (`PROXY_STATS_PERSIST=off` では呼ばれないので、預けても 1 ファイルも書かない)
    {
        let cfg = config_of();
        let workers = Arc::clone(&workers);
        let max_conns = cfg.max_conns;
        endpoints::register_snapshot(endpoints::SnapshotSource {
            metrics: Arc::clone(&metrics),
            cache: Arc::clone(&cache),
            port: local_port,
            version: VERSION,
            lite: cfg.lite,
            readonly: cfg.endpoints_readonly,
            concurrency: Box::new(move || crate::metrics::Concurrency {
                max_conns,
                max_threads: workers.max_threads(),
                live_threads: workers.live_count(),
                idle_threads: workers.idle_count(),
                queued_jobs: workers.queued(),
            }),
        });
    }
    loop {
        // incoming() は accept() の戻り値のアドレスを捨てるので accept() を直接呼ぶ
        // (接続ごとの getpeername が 1 回減る)
        let from_backlog = !backlog.is_empty();
        let accepted = match backlog.pop_front() {
            Some(v) => Ok(v),
            None => listener.accept(),
        };
        let (mut stream, peer) = match accepted {
            Ok(v) => v,
            // 待ち受けに載せた SO_RCVTIMEO は accept() にも効くので、接続が来ないまま
            // timeout 秒たつと WouldBlock で戻ってくる。これは異常ではないので
            // ログも待ちも無しに待ち直す (下の「その他のエラー」より必ず先に拾うこと)。
            // timeout 0 (= 無期限) なら accept は戻ってこないが、このループが空振りの
            // 合間にしていることは無い (設定は接続ごとに config_of() で引き直す) ので、
            // 無期限に待って構わない。
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
                // 出来事の時系列に 1 件 (**1 時間に初めて起きたときだけ**。T14.11)
                events::note_accept_error(&e);
                std::thread::sleep(ACCEPT_ERROR_BACKOFF);
                continue;
            }
        };
        let cfg = config_of();
        // 接続元の ACL (`PROXY_ALLOW_CLIENTS`。T14.18)。**要求を読まずにここで閉じる**ので、
        // 内部エンドポイントも含めて何も見せない (公開ポートで個票を出さないため)。
        // 断るのに応答は返さない — 誰が叩いているか分からない相手に、ここに何が居るかを
        // 教えないため (503 を返す上限のときとは目的が違う)。
        // T13.2 の「上限の外の枠」より**前**に置く (枠は自分宛ての要求のためのもので、
        // そもそも繋がせない相手には要らない)。**認証ではない**。
        // 既定 (空) の費用は `is_empty()` の分岐 1 回だけで、照合には入らない
        if !cfg.allow_clients.is_empty() && !cfg.allow_clients.allows(peer.ip()) {
            metrics
                .rejected_client_acl
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            log_debug!(
                None,
                "closing connection from {} (not in PROXY_ALLOW_CLIENTS)",
                peer.ip()
            );
            drop(stream);
            continue;
        }
        // 消えたクライアントを見つける (`PROXY_TCP_KEEPALIVE`、既定 `on` = 60:10:3。T14.52)。
        // 端末のスリープや回線の切断では FIN も RST も来ないので、トンネルは
        // `PROXY_TUNNEL_IDLE_SECS` (300 秒) の期限切れまで残り、閉じた理由が
        // `idle_timeout` になって「本当に暇」と区別できない。keepalive を当てておけば
        // 消えた相手は約 90 秒で `ETIMEDOUT` になり、`client_dead` として数えられる。
        //
        // 費用は**接続あたり `setsockopt` 4 回**。`TCP_NODELAY` などと違って待ち受けから
        // 継承させていないのは、`.env` で変えたときに次の接続から効くようにするため
        // (`off` なら 1 回も呼ばず、この `if` の分岐 1 回だけ)。**`--lite` でも当てる**
        // (枠が無くても「消えた相手」はトンネルの終わり方として閉じたい)
        if let Some(k) = cfg.tcp_keepalive {
            set_client_keepalive(&stream, k);
        }
        // この接続が継承したのは「今 待ち受けに当たっている値」。.env の再読込で timeout が
        // 変わった直後だけは食い違うので、その接続は従来どおり接続ごとに設定する
        let conn_inherited = inherited
            .filter(|t| !from_backlog && *t == listener_timeouts(&cfg))
            .map(|(_, read)| read);
        if conn_inherited.is_none() && inherited.is_some() && !from_backlog {
            // 待ち受けに当て直す (次の接続から効く)。その前に握手を終えた分は古い値のままなので、
            // 起動時と同じく先に引き取って接続ごとに設定する
            inherited = inherit_on_listener(&listener, &cfg);
            backlog.extend(take_backlog(&listener));
        }
        // 生きているスレッドの上限も `.env` の再読込で変わる (T11.6)。当て直すのは
        // 食い違ったときだけで、接続ごとにかかるのは `Relaxed` の読みが 1 回
        // (`PROXY_MAX_CONNS` と同じく、接続ごとに引いた設定をそのまま使う)
        if cfg.max_threads != workers.max_threads() {
            workers.set_limit(cfg.max_threads);
        }
        let mut overflow = None;
        // 接続元ごとの同時接続の上限 (`PROXY_MAX_CONNS_PER_CLIENT`。T14.13)。
        // **認証ではなく公平さの上限**で、1 人が `PROXY_MAX_CONNS` を使い切って本人が
        // 503 になるのを防ぐためのもの。既定 (0) の費用は下の分岐 1 回だけ。
        //
        // 数え方: 数えるのは `/connections` の表と同じ鍵の内側にある「接続元ごとの本数」で、
        // 引くのは `HashMap` 1 回 (240 本を数え直さない)。鍵は `Conn::new` の登録と同じ
        // 正規化した接続元 IP なので、数と表は必ず一致する。
        //
        // **自分宛ての内部エンドポイントは数えない**: accept の時点では要求が読めないので、
        // 上限に当たった接続は T13.2 と同じ「上限 + 4 本」の枠で受け、要求行を読んでから
        // 決める (自分宛てなら普通に応答、それ以外はワーカーが 503 + `Retry-After: 1`)。
        // 枠が埋まっていたらここで 503 を返して閉じる。こうすると、上限に当たっている
        // 接続元からでも `/status` が取れる (監視が消えない。T13.2 の狙いと同じ)。
        if cfg.max_conns_per_client > 0 {
            let client = net::canonical_addr(peer).ip().to_string();
            if metrics
                .conns
                .at_client_limit(&client, cfg.max_conns_per_client)
            {
                overflow = OverflowGuard::try_acquire(&limiter, true);
                if overflow.is_none() {
                    metrics.record_client_rejected(&client);
                    log_debug!(
                        None,
                        "over PROXY_MAX_CONNS_PER_CLIENT={} for {}: 503",
                        cfg.max_conns_per_client,
                        client
                    );
                    if conn_inherited.is_none() {
                        let _ = stream.set_write_timeout(timeout::for_socket(cfg.timeout));
                    }
                    let _ = stream.write_all(OVERLOAD_RESPONSE);
                    let _ = stream.flush();
                    continue;
                }
            }
        } else if metrics.conns.counting_clients() {
            // 上限を `0` に戻したら数えるのもやめる (`.env` で即時反映)
            metrics.conns.stop_counting_clients();
        }
        // 接続元 1 つの追跡 (`PROXY_TRACE_CLIENT`。T14.27)。**接続元を見るのはここだけ**で、
        // 一致した接続には下の `Conn::new` が枠 (`ConnSlot`) に旗を立てる。以降は
        // 要求ごとにその旗を読む分岐 1 回で済む (文字列の比較も設定の引き直しもしない)。
        // 既定 (空) の費用は `is_some_and` の分岐 1 回だけで、照合にも入らない。
        // `.env` で書き換えると**次に来る接続から**効く (開いている接続の旗は動かない)
        let traced = cfg
            .trace_client
            .is_some_and(|ip| ip == net::canonical_addr(peer).ip());
        // 上限に当たったときの段取り (T13.2):
        //   1. 預かり所の**暇なトンネル**を最古から 1 本閉じて席を作る (閉じるのはこのスレッド。
        //      持ち分が同期で返るので、すぐ下の `OpenGuard::acquire` がその席に座れる)
        //   2. 閉じるものが無ければ、自分宛てかもしれないぶんとして `OVERFLOW_SLOTS` 本までは
        //      受ける (要求行と `Host` を読んでから、自分宛てでなければワーカーが 503 を返す)
        //   3. それも埋まっていたら今までどおりスレッドを起こさずに 503
        let max = cfg.max_conns;
        if max > 0 && limiter.open() >= max {
            let made_room = park.as_ref().is_some_and(|w| w.evict_oldest_tunnel());
            if made_room {
                // 出来事の時系列に 1 件 (**1 時間に初めて起きたときだけ**。T14.11)
                events::note_evict(limiter.open(), max);
            }
            // 接続元ごとの上限で枠を取ってあれば、その枠をそのまま使う (二重には取らない)
            if !made_room && overflow.is_none() {
                overflow = OverflowGuard::try_acquire(&limiter, false);
            }
            if !made_room && overflow.is_none() {
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
                        "connection limit reached ({} open, PROXY_MAX_CONNS={}) and no idle tunnel to close; returning 503",
                        limiter.open(),
                        max
                    );
                }
                if conn_inherited.is_none() {
                    let _ = stream.set_write_timeout(timeout::for_socket(cfg.timeout));
                }
                let _ = stream.write_all(OVERLOAD_RESPONSE);
                let _ = stream.flush();
                continue;
            }
        }
        let conn_id = CONN_COUNTER.fetch_add(1, Ordering::Relaxed);
        // 待ち行列の段階 (`queue`) の起点 (T14.3 (1))。`--lite` では時計を読まない
        let queued_at = profile::mark();
        // 同時接続数の持ち分は番人として取って仕事へ運ぶ。`Conn::new` が途中で失敗しても、
        // 仕事がワーカーへ渡らずに落ちても、番人の `Drop` が必ず 1 回だけ返す (T9.6)
        let (open, now_open) = OpenGuard::acquire(Arc::clone(&limiter));
        // 山の写真 (T14.6): 上限の一定割合を**下から上に越えた瞬間**だけ、history スレッドに
        // 1 枚頼む (旗を立てるだけ)。**越えていない経路はこの比較 1 回で終わる** —
        // 撮らない設定 (`PROXY_BURST_PERCENT=0` / `PROXY_MAX_CONNS=0` / `--lite`) では
        // `burst_at` が `usize::MAX` なので、同じ比較がそのまま「撮らない」になる
        if now_open > cfg.burst_at {
            metrics
                .bursts
                .request(now_open, cfg.burst_at, cfg.max_conns);
        }
        let m = Arc::clone(&metrics);
        let c = Arc::clone(&cache);
        let p = Arc::clone(&upstream);
        let w = park.clone();
        // `/status` が上限とスレッド数を出すために、接続にワーカー置き場も持たせる
        // (数を引くのは `/status` に来たときだけ。要求ごとには引かない)
        let wk = Arc::clone(&workers);
        // 上限の外で受けた接続は、ワーカーが全部塞がっていても待ち行列に積まずに
        // 上限の外のスレッドで動かす (T.28.0)。上限ぶんの接続が黙ってワーカーを握って
        // いると、積んだ `/status` はそれが放されるまで動けない (T13.2 の狙いが崩れる)。
        // 本数は `OVERFLOW_SLOTS` (4 本) で縛られている
        let outside = overflow.is_some();
        let job: Box<dyn FnOnce() + Send> = Box::new(move || {
            // 同時接続数と active_connections の持ち分は Conn が持つ (接続の寿命と一致させる)
            let accepted = Accepted { peer, local_port };
            match Conn::new(
                stream,
                accepted,
                open,
                overflow,
                cfg,
                m,
                c,
                p,
                w,
                wk,
                conn_inherited,
                conn_id,
                queued_at,
                traced,
            ) {
                Ok(conn) => run_conn(Box::new(conn)),
                Err(e) => log_error!(Some(conn_id), "{}", e),
            }
        });
        let started = if outside {
            workers.run_outside(job)
        } else {
            workers.run(job)
        };
        if started.is_err() {
            // 渡せなかった仕事はここで落ちる。持ち分はその中の `OpenGuard` が返す (T9.6)
            log_error!(Some(conn_id), "failed to get a thread for the connection");
        }
    }
}

/// 待ち受けソケットに「accept した接続へ引き継がせるオプション」を当てる。
/// 当たったら継承させた `timeout` を返す (当たらなければ `None` = 接続ごとに設定する)。
///
/// `timeout` が `Duration::ZERO` (= 無期限) でも当たる。Linux の `timeval {0, 0}` が
/// 「タイムアウト無し」そのものなので、無期限のときこそ継承させた方が安い
/// (継承しないと接続ごとに `set_write_timeout(None)` / `set_nodelay` / `set_read_timeout(None)`
/// の 3 回を払う。実測: 1 接続 1 要求 で setsockopt 3.00 → 0.00 回/接続)。
fn inherit_on_listener(
    listener: &TcpListener,
    cfg: &Config,
) -> Option<(std::time::Duration, std::time::Duration)> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let (write, read) = listener_timeouts(cfg);
        match sys::inherit_socket_options(listener.as_raw_fd(), read, write) {
            Ok(()) => return Some((write, read)),
            Err(e) => log_debug!(
                None,
                "listener socket options not inherited ({}); setting them per connection",
                e
            ),
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (listener, cfg);
    None
}

/// いま accept 待ちの列に載っている接続を、待たずに全部引き取る (T.28.30)。
///
/// 待ち受けを一時だけ非ブロッキングにして `WouldBlock` まで accept する。accept した接続は
/// 非ブロッキングを引き継がない (`accept4` に `SOCK_NONBLOCK` を渡していない)。
/// 非ブロッキングにできなければ何も引き取らない (今までどおり)。
fn take_backlog(
    listener: &TcpListener,
) -> std::collections::VecDeque<(TcpStream, std::net::SocketAddr)> {
    let mut out = std::collections::VecDeque::new();
    if listener.set_nonblocking(true).is_err() {
        return out;
    }
    loop {
        match listener.accept() {
            Ok(v) => out.push_back(v),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if let Err(e) = listener.set_nonblocking(false) {
        log_error!(None, "could not make the listener blocking again: {}", e);
    }
    out
}

/// 待ち受けに継承させる `(書き, 読み)` のタイムアウト (T.28.0)。
///
/// 書きは `PROXY_TIMEOUT_SECS`、読みは 1 要求目の要求行の 1 バイト目を待つ長さ
/// ([`config::first_read_timeout`])。読みを継承させておけば、黙った接続を
/// `PROXY_HEADER_TIMEOUT_SECS` で切るのに接続ごとの `setsockopt` が要らない。
fn listener_timeouts(cfg: &Config) -> (std::time::Duration, std::time::Duration) {
    (
        cfg.timeout,
        config::first_read_timeout(cfg.timeout, cfg.header_timeout),
    )
}

/// accept したクライアント側のソケットに TCP keepalive を当てる (T14.52)。
///
/// `setsockopt` 4 回 (`SO_KEEPALIVE` / `TCP_KEEPIDLE` / `TCP_KEEPINTVL` / `TCP_KEEPCNT`)。
/// 失敗しても接続は続ける (当たらなければ「消えた相手」が今までどおり
/// `idle_timeout` になるだけで、中継そのものには関係がない) が、**毎接続 warn を
/// 出すと溢れる**ので `debug` に落とす。Linux 以外では何もしない。
fn set_client_keepalive(stream: &TcpStream, k: config::TcpKeepalive) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        if let Err(e) = sys::set_keepalive(stream.as_raw_fd(), k.idle_secs, k.intvl_secs, k.count) {
            log_debug!(
                None,
                "could not set TCP keepalive on the client socket: {}",
                e
            );
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (stream, k);
}

/// accept が失敗したときに次の試行まで待つ時間 (記述子切れでの空回りを止める)。
const ACCEPT_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);
