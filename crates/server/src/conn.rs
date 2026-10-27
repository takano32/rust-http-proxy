//! 1 接続ぶんの状態 ([`Conn`]) と、その一生 (持ち分の番人・要求の回し・預け・後始末)。

mod connect;
mod header_read;
mod serve_one;

use super::*;

use std::time::Instant;

use connect::start_tunnel;
use header_read::Scratch;
use serve_one::serve_one;

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
    /// 今ソケットに設定してある読み取りタイムアウト (同じ値なら setsockopt を呼ばない)。
    /// `None` は「まだ何も設定していない」、`Some(Duration::ZERO)` は「無期限を設定済み」
    read_timeout: Option<std::time::Duration>,
    /// 要求行とヘッダー行の置き場。処理している間だけ持ち、預けるときはスレッドへ返す
    scratch: Option<Scratch>,
    /// アイドルのときに預ける先 (無ければ従来どおりこのスレッドがブロッキング read で待つ)
    park: Option<Arc<idle::IdleWatch>>,
    /// 接続スレッドの置き場。`/status` が上限といまの数を出すためだけに持つ
    /// (数を引くのは `/status` に来たときだけ。鍵を要求ごとに取らない)
    workers: Arc<workers::Workers>,
    /// 接続元の IP を文字列にしたもの (X-Forwarded-For と統計に毎要求要るので接続ごとに 1 回だけ作る)
    peer_ip: String,
    /// この接続で **1 回でもプロキシとして通す要求を受けたか** (T15.0 (12))。
    ///
    /// `clients[]` の同じ行の中で `requests` は「プロキシとして通した要求」なのに
    /// `rtt_ms.samples` は「閉じた接続すべて」で母数が違っていた (`/status` を引くだけの
    /// 接続でも [`Conn::finish`] が `record_client_rtt` を呼ぶので、**プロキシとしても
    /// 使っている IP** の行だけが監視のぶんで黙って積み上がる)。立てるのは
    /// `endpoints::handle` が「自分宛てではない」と決めた直後の 1 か所だけで、
    /// `finish` はこれが真のときしか RTT を入れない。
    ///
    /// **`served` では代用できない**: `*served += 1` は転送が終わってからなので、
    /// 403 も 508 も CONNECT もそこを通らないのに、そのどれもが `record_client` で行を作る。
    proxied: bool,
    /// 上限の外で受けた枠の持ち分 (T13.2)。`Some` = 「自分宛てのときだけ応える接続」で、
    /// 内部エンドポイント以外は要求を読んだあと 503 で閉じる。`Drop` で枠を返す
    overflow: Option<OverflowGuard>,
    /// `/connections` に出すこの接続の枠 (T13.4)。**登録は `Conn::new`、抹消は
    /// `ActiveGuard::drop` の 1 回ずつだけ**で、状態はこの `Arc` の原子に書く
    /// (`--lite` では `None` = 何も記録しない)
    slot: Option<Arc<recent::ConnSlot>>,
    /// 次の要求に載せる「ワーカーを待った時間」(ms。T14.3 (1))。accept と
    /// 預かり所からの起床で入り、要求 1 つに載せたら 0 に戻る
    queue_ms: u32,
    /// 預かり所がワーカーへ渡した時刻 (`--lite` では `None`。T14.3 (1))
    queued_at: Option<Instant>,
    /// 閉じた接続の個票 (`/recent`) に載せる値の積み上げ (T14.4)。
    ///
    /// **原子でも鍵でもない**: この接続を同時に触るスレッドは 1 つだけなので、要求ごとの
    /// 足し算はここで済ませ、**接続の終了で 1 回だけ** [`Conn::finish`] が枠へ移す
    tally: std::cell::Cell<recent::ConnTally>,
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
    /// この接続は終わり (閉じた理由つき。`/recent` に残す。T14.4)
    Close(recent::CloseReason),
    /// CONNECT トンネルへ移る
    Connect {
        target: String,
        prefix: Vec<u8>,
        /// ACL の判定が引いた答え (名前解決を 1 要求 1 回にする。T12.7)。
        /// IP リテラルと `PROXY_ALLOW_LOCAL=on` (判定を飛ばす) では `None`
        addrs: Option<Vec<std::net::IpAddr>>,
        /// そのホストで最後に勝った族 (T12.1)。同じ鍵取りで受け取ったもの
        preferred: Option<bool>,
        /// ここまでに測った段階 (`queue`。T14.3 (1))
        stages: metrics::StageMs,
        /// 要求行が届いた時刻 (`client_read` の起点。終わりは `tunnel::open` の入口の時計)
        read_started: Option<Instant>,
    },
}

/// 同時接続数の持ち分。
///
/// **数えるのも返すのもこの型だけ** (T9.6)。`serve` が accept した直後に
/// [`OpenGuard::acquire`] で取り、`Conn` へ運ぶ。途中で `Conn::new` が失敗しても、
/// 仕事がワーカーに渡らずに落ちても、`Drop` が必ず 1 回だけ返す。
/// (以前は `serve` が `fetch_add` して `Conn` の `Drop` が返す形だったので、
/// `Conn::new` の `?` で抜けると持ち分が戻らず、積もると恒久的に 503 になった。)
pub(crate) struct OpenGuard(Arc<Limiter>);

impl OpenGuard {
    /// 持ち分を 1 つ取る (返すのは `Drop`)。
    ///
    /// 2 つ目の戻り値は**取ったあとの本数**。山の写真 (T14.6) が「閾を下から上に越えた
    /// 瞬間」を知るために要るが、`fetch_add` が返す値を使うので**原子操作は増えない**
    /// (同じ瞬間に複数のスレッドが accept しても、越えた 1 本だけがこの値を受け取る)。
    pub(crate) fn acquire(limiter: Arc<Limiter>) -> (OpenGuard, usize) {
        let open = limiter.open.fetch_add(1, Ordering::Relaxed) + 1;
        (OpenGuard(limiter), open)
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 上限の外の枠 ([`OVERFLOW_SLOTS`]) の持ち分 (T13.2)。
///
/// `OpenGuard` と同じく**取るのも返すのもこの型だけ**。accept した接続へ運び、
/// `Conn` が落ちる (= 接続が閉じる) ときに `Drop` が枠を返す。
pub(crate) struct OverflowGuard {
    limiter: Arc<Limiter>,
    /// この枠を使った理由が**接続元ごとの上限**か (T14.13)。自分宛てでなかったときに
    /// 数える先が違うだけ (`rejected_per_client` / `rejected_overload`)
    per_client: bool,
}

impl OverflowGuard {
    /// 空いていれば枠を 1 つ取る。埋まっていれば `None` (呼び出し側は 503)。
    ///
    /// 待ち受けが複数 (デュアルスタック) だと accept するスレッドも複数なので、
    /// 「見てから増やす」の間に割り込まれないように CAS で取る。
    pub(crate) fn try_acquire(limiter: &Arc<Limiter>, per_client: bool) -> Option<OverflowGuard> {
        let mut taken = limiter.overflow.load(Ordering::Relaxed);
        while taken < OVERFLOW_SLOTS {
            match limiter.overflow.compare_exchange_weak(
                taken,
                taken + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(OverflowGuard {
                        limiter: Arc::clone(limiter),
                        per_client,
                    });
                }
                Err(now) => taken = now,
            }
        }
        None
    }
}

impl Drop for OverflowGuard {
    fn drop(&mut self) {
        self.limiter.overflow.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 捨てるオリジン接続のカーネルの RTT をホスト別統計に足す配線 (T14.5)。
///
/// **通るのは「期限切れ」と「相手が閉じていた」を捨てるときだけ**で、使い回せた接続
/// (熱い経路) では 1 度も呼ばれない。挿すのはプールを作った直後の 1 回きり。
/// 鍵はプールの鍵 (`scheme://host:port`) で、ホスト別統計の鍵と同じもの。
pub fn attach_origin_rtt(pool: &mut pool::Pool, metrics: Arc<Metrics>) {
    pool.on_discard(Box::new(move |host, tcp| {
        let (rtt_us, retrans) = tcp_rtt(tcp);
        metrics.record_host_rtt(host, rtt_us, retrans);
    }));
}

/// いまのスレッドの番号 (`gettid`。T15.0 (4))。Linux 以外は `0` = 「分からない」。
///
/// システムコールを引くのは**スレッドの一生に 1 回**だけで (`sys::gettid` が
/// スレッドローカルに控える)、ここは 2 回目からは読み出し 1 回。
#[cfg(target_os = "linux")]
fn current_tid() -> u32 {
    sys::gettid()
}

#[cfg(not(target_os = "linux"))]
fn current_tid() -> u32 {
    0
}

/// ソケット 1 本のカーネルの RTT (us) と再送の通算 (`getsockopt` 1 回。T14.5)。
/// 読めなければ 0 (個票では `null`、統計には足さない)。Linux 以外は聞かない。
#[cfg(target_os = "linux")]
fn tcp_rtt(tcp: &TcpStream) -> (u32, u32) {
    use std::os::fd::AsRawFd;
    match sys::tcp_info(tcp.as_raw_fd()) {
        Some(i) => (i.rtt_us, i.total_retrans),
        None => (0, 0),
    }
}

#[cfg(not(target_os = "linux"))]
fn tcp_rtt(_tcp: &TcpStream) -> (u32, u32) {
    (0, 0)
}

/// `/status` の active_connections の持ち分。
struct ActiveGuard {
    metrics: Arc<Metrics>,
    conn_id: usize,
    started: Instant,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        // `/connections` からも外し、閉じた接続の個票を 1 件残す (`/recent`。T14.4)。
        // 登録と抹消は接続の開始と終了で 1 回ずつ (T13.4)。トンネルへ移った接続も、
        // この番人を一緒に運んでいるので必ずここを通る
        self.metrics.record_closed(self.conn_id as u64);
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
    pub(crate) fn new(
        client: TcpStream,
        accepted: Accepted,
        open: OpenGuard,
        overflow: Option<OverflowGuard>,
        config: Arc<Config>,
        metrics: Arc<Metrics>,
        cache: Arc<Cache>,
        upstream: Arc<Upstream>,
        park: Option<Arc<idle::IdleWatch>>,
        workers: Arc<workers::Workers>,
        inherited: Option<std::time::Duration>,
        conn_id: usize,
        queued_at: Option<Instant>,
        traced: bool,
    ) -> io::Result<Conn> {
        metrics.inc_active_conn();
        let active_started = Instant::now();
        let active = ActiveGuard {
            metrics: Arc::clone(&metrics),
            conn_id,
            started: active_started,
        };
        log_debug!(Some(conn_id), "accepted connection from {}", accepted.peer);
        if inherited.is_none() {
            // 0 は無期限 (T10.6)。std は `Duration::ZERO` を断るので `None` に直す
            client.set_write_timeout(timeout::for_socket(config.timeout))?;
            // Nagle を切る。応答ヘッダーと本文を別々に write すると delayed ACK と噛み合って
            // 1 要求あたり 40 ms 止まるため (失敗しても致命的ではないので無視する)
            let _ = client.set_nodelay(true);
        }
        let peer_ip = net::canonical_addr(accepted.peer).ip().to_string();
        // `/connections` に登録する (接続の開始で 1 回だけ。`--lite` では `None`)
        let slot = metrics
            .conns
            .register(conn_id as u64, &peer_ip, active.started);
        // 追跡の旗 (`PROXY_TRACE_CLIENT` に一致した接続だけ。T14.27)。照合は accept で
        // 済ませてあるので、ここは原子 1 回の書き込み。**`--lite` は枠が無いので
        // 旗も立たない** = 追跡しない
        if traced && let Some(s) = &slot {
            s.mark_traced();
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
            workers,
            peer_ip,
            // プロキシとして通す要求はまだ 1 つも来ていない (T15.0 (12))
            proxied: false,
            overflow,
            slot,
            // accept からワーカーが動き出すまで (T14.3 (1))。時計は accept で
            // 読んだ 1 回だけで、終わりは番人が既に読んでいる `started` を使う
            queue_ms: queued_at.map_or(0, |t| {
                profile::ms_u32(active_started.saturating_duration_since(t))
            }),
            queued_at: None,
            tally: std::cell::Cell::new(recent::ConnTally::default()),
            _open: open,
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

    /// クライアントのソケットに溜まっているエラーを 1 回だけ取る (T14.52)。
    ///
    /// TCP keepalive (`PROXY_TCP_KEEPALIVE`) が尽きるとカーネルが `sk_err` を
    /// `ETIMEDOUT` にするので、「相手が黙って消えた」と「相手がふつうに切った」を
    /// ここで見分けられる (`ECONNRESET` などはそのまま原因になる)。
    /// `getsockopt(SO_ERROR)` を 1 回引き、**取ると消える**ので、呼ぶのは
    /// 預かり所が `EPOLLERR` で起こされた枝だけ (接続の経路では呼ばない)。
    pub fn take_client_error(&self) -> Option<io::Error> {
        self.client.take_error().ok().flatten()
    }

    /// `/connections` に出す状態を書く (原子 1 回。`--lite` では何もしない。T13.4)。
    pub fn set_state(&self, state: recent::ConnState) {
        if let Some(slot) = &self.slot {
            slot.set_state(state);
        }
    }

    /// `/connections` の枠 (預かり所が状態を書くために借りる)。
    pub fn slot(&self) -> Option<&Arc<recent::ConnSlot>> {
        self.slot.as_ref()
    }

    /// いま受け持っているスレッドの番号を枠へ書く (原子 1 回。`--lite` では何もしない。
    /// T15.0 (4))。**呼ぶのはワーカーがこの接続を取ったときだけ**で、要求ごとには触らない。
    pub fn set_tid(&self) {
        if let Some(slot) = &self.slot {
            slot.set_tid(current_tid());
        }
    }

    /// ワーカーの待ち行列へ入れる直前に呼ぶ (`queue` の段階の起点。T14.3 (1))。
    /// `--lite` では時計を読まない。
    pub fn mark_queued(&mut self) {
        self.queued_at = profile::mark();
    }

    /// 閉じた接続の個票 (`/recent`) に載せる値を枠へ移す (**接続の終了で 1 回だけ**。T14.4)。
    ///
    /// リングへ書くのは持ち分 (`ActiveGuard`) が落ちるときで、ここは「何を書くか」を
    /// 決めるだけ。理由は先着優先なので、外から閉じられた (追い出し・監視の停止) ものは
    /// そちらの理由が残る。
    pub fn finish(&self, reason: recent::CloseReason) {
        if let Some(slot) = &self.slot {
            // クライアント側のカーネルの RTT と再送を 1 回だけ読む
            // (`getsockopt` 1 回 / 接続。要求ごとには読まない。`--lite` は枠が無いので
            // ここへ来ない。オリジン側は接続プールが捨てるときに読む。T14.5)
            let (rtt_us, retrans) = tcp_rtt(&self.client);
            let mut tally = self.tally.get();
            tally.rtt_us[recent::CLIENT_SIDE] = rtt_us;
            tally.retrans[recent::CLIENT_SIDE] = retrans;
            slot.finish(reason, tally, self.served as u32);
            // 接続元の表 (`clients[]`) に入れるのは**プロキシとして使われた接続だけ** (T15.0 (12))。
            // `/status` を引くだけの接続を混ぜると、同じ行の `requests` (プロキシとして通した
            // 要求) と `rtt_ms.samples` (閉じた接続すべて) で母数が食い違う。個票 (`/recent`) の
            // `rtt_ms` は接続 1 本ごとの値なので、こちらは今までどおり全部に入れる
            if self.proxied {
                self.metrics
                    .record_client_rtt(&self.peer_ip, rtt_us, retrans);
            }
        }
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
        let step = match serve_one(&mut conn) {
            Ok(step) => step,
            Err(e) => {
                // 入出力で落ちた接続も個票に残す (原因つき。T14.4)
                conn.finish(recent::CloseReason::error_from_io(&e));
                return Err(e);
            }
        };
        match step {
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
            Step::Close(why) => {
                conn.finish(why);
                return Ok(());
            }
            Step::Connect {
                target,
                prefix,
                addrs,
                preferred,
                stages,
                read_started,
            } => {
                // 要るものだけ取り出してトンネルへ渡す (`Conn` に `Drop` は無いので
                // 分解できる)。**持ち分 (`_open` / `_active`) も一緒に渡すこと**:
                // トンネルは暇なときに監視スレッドへ預けられるので、ここで落とすと
                // 預けた瞬間に同時接続数が減って `PROXY_MAX_CONNS` の意味が壊れる
                let Conn {
                    client,
                    config,
                    metrics,
                    conn_id,
                    park,
                    peer_ip,
                    slot,
                    _open,
                    _active,
                    ..
                } = *conn;
                // CONNECT のオリジン接続だけは専用の締め切りを使う (`PROXY_CONNECT_TIMEOUT_SECS`。
                // 未設定なら 10 秒と `config.timeout` の小さい方が入っている。T15.6)
                let timeout = config.connect_timeout;
                let idle = (!config.tunnel_idle.is_zero()).then_some(config.tunnel_idle);
                let hold: Box<dyn Send> = Box::new((_open, _active));
                // 判定で引いた答えをそのまま接続に使う (T12.7)。鍵はこの CONNECT の
                // ホスト名 (`target` の借用で足りるので複製しない)
                let resolved = addrs.map(|addrs| {
                    dns::Resolved::new(net::split_host_port_ref(&target).0, addrs, preferred)
                });
                return start_tunnel(
                    client,
                    &target,
                    &prefix,
                    timeout,
                    idle,
                    // 書き側の最小の速さ (T.29.34)
                    config.min_transfer,
                    conn_id,
                    metrics,
                    // 接続元 IP は接続ごとに 1 回作ってある。ここで渡さないと
                    // トンネル側が `peer_addr()` を引き直す (`getpeername` が 1 本ごとに 1 回)
                    peer_ip,
                    resolved.as_ref(),
                    park,
                    config.park_grace,
                    hold,
                    // `/connections` の枠をそのままトンネルへ運ぶ (T13.4)。
                    // 抹消するのは `hold` の中の `ActiveGuard` なので、寿命は一致する
                    slot,
                    stages,
                    read_started,
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
        conn.set_state(recent::ConnState::Reading);
        return Err(conn);
    }
    let Some(watch) = conn.park.clone() else {
        return Err(conn);
    };
    // 預けに行く (断られたら下で `reading` に戻す)
    conn.set_state(recent::ConnState::Parked);
    let deadline = Instant::now() + conn.config.keepalive;
    conn.release_idle_buffers();
    match watch.park(conn, deadline) {
        Ok(()) => Ok(()),
        Err(mut conn) => {
            // 預かってもらえなかった (監視スレッドが死んだ、この記述子が epoll に
            // 入らない)。この接続は以後スレッドで待つ側に固定する。そうしないと
            // 要求のたびに猶予で空振りして預け直そうとして空回りする
            conn.park = None;
            conn.set_state(recent::ConnState::Reading);
            Err(conn)
        }
    }
}

/// 1 つの接続を最後まで面倒みる。ワーカースレッドに渡す仕事の中身。
/// 監視スレッドから戻ってきた接続もここに入る。
pub fn run_conn(mut conn: Box<Conn>) {
    let conn_id = conn.conn_id;
    // 預かり所から戻ってきた接続が待ち行列に居た時間 (T14.3 (1))。
    // accept の分は `Conn::new` が入れてあるので、ここで読むのは起床した接続だけ
    if let Some(t) = conn.queued_at.take() {
        conn.queue_ms = profile::ms_u32(t.elapsed());
    }
    // ワーカーが取った (預かり所から戻ってきた接続もここを通る。T13.4)
    conn.set_state(recent::ConnState::Serving);
    // いま受け持っているスレッドの番号 (T15.0 (4))。**accept 直後の 1 本目も、
    // 預かり所から起こされた 1 本も、ワーカーのスレッドで通るのはここ**なので、
    // 書き先は 1 か所で足りる (`Conn::new` の登録はこのすぐ手前・同じスレッド、
    // `IdleWatch::resume_conn` は監視スレッドなのでそこで書くと監視の番号になる)。
    // CONNECT のトンネルが預かり所から戻る道だけは別のスレッドなので、
    // そちらは `tunnel::resume` が書く
    conn.set_tid();
    if let Err(e) = pump(conn) {
        if tunnel::is_reported(&e) {
            // 繋がらなかった CONNECT: tunnel が 502 を書き、WARN 1 行と `/errors` の個票まで
            // 出してある。同じ失敗を ERROR で出し直さない (T.28.4)
            log_debug!(Some(conn_id), "connection ended: {}", e);
        } else if e.kind() != io::ErrorKind::UnexpectedEof
            && e.kind() != io::ErrorKind::ConnectionReset
            && e.kind() != io::ErrorKind::BrokenPipe
        {
            log_error!(Some(conn_id), "{}", e);
        } else {
            log_debug!(Some(conn_id), "connection ended: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同時接続数の持ち分は、`Conn::new` が途中で失敗しても戻ること (T9.6)。
    ///
    /// 以前は `serve` が `open` を直接 `fetch_add` し、返すのは `Conn` の
    /// `_open: OpenGuard` の `Drop` だった。`Conn::new` は番人を作る**前**に
    /// `set_write_timeout(...)?` を通るので、ここで失敗すると持ち分が誰にも
    /// 戻されず、積もると `PROXY_MAX_CONNS` に達して恒久的に 503 になる。
    ///
    /// **失敗のさせ方は T10.6 で変えた**。以前は `PROXY_TIMEOUT_SECS=0` が
    /// `set_write_timeout(Some(ZERO))` を必ず失敗させたのでそれを使っていたが、
    /// 0 は「無期限」(= `None`) になったので成功する。代わりにソケットでない
    /// 記述子 (パイプ) を渡して `setsockopt` を `ENOTSOCK` で失敗させる。
    #[cfg(unix)]
    #[test]
    fn open_slot_comes_back_when_conn_setup_fails() {
        use std::os::fd::OwnedFd;

        let limiter = Limiter::new();
        // ソケットでない記述子。`set_write_timeout` が ENOTSOCK で失敗する
        let (reader, _writer) = std::io::pipe().unwrap();
        let stream = TcpStream::from(OwnedFd::from(reader));

        let config = Arc::new(
            config::Config::new("8080", None, None, std::time::Duration::from_secs(5)).unwrap(),
        );
        let metrics = Arc::new(metrics::Metrics::new());
        let cache = Arc::new(Cache::new(config.cache.clone()));
        let upstream = Arc::new(Upstream {
            pool: pool::Pool::new(8, std::time::Duration::from_secs(60)),
            tls: None,
        });

        let (open, now_open) = OpenGuard::acquire(Arc::clone(&limiter));
        assert_eq!(now_open, 1, "取ったあとの本数が返る");
        assert_eq!(limiter.open(), 1, "持ち分を取ったら 1");
        let err = Conn::new(
            stream,
            Accepted {
                peer: "127.0.0.1:1".parse().unwrap(),
                local_port: 8080,
            },
            open,
            None,
            config,
            metrics,
            cache,
            upstream,
            None,
            Arc::new(workers::Workers::new(0)),
            None,
            1,
            None,
            false,
        );
        assert!(err.is_err(), "ソケットでなければ Conn::new は失敗する");
        drop(err);
        assert_eq!(limiter.open(), 0, "失敗しても持ち分は戻る");
    }

    /// `PROXY_TIMEOUT_SECS=0` は「無期限」なので `Conn::new` は成功すること (T10.6)。
    ///
    /// 以前はここで `set_write_timeout(Some(Duration::ZERO))` が必ず失敗し、
    /// **この設定では 1 本も代理できなかった**。
    #[test]
    fn zero_timeout_sets_no_timeout_instead_of_failing() {
        let limiter = Limiter::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = TcpStream::connect(addr).unwrap();
        let (stream, peer) = listener.accept().unwrap();

        let config =
            Arc::new(config::Config::new("8080", None, None, std::time::Duration::ZERO).unwrap());
        let metrics = Arc::new(metrics::Metrics::new());
        let cache = Arc::new(Cache::new(config.cache.clone()));
        let upstream = Arc::new(Upstream {
            pool: pool::Pool::new(8, std::time::Duration::from_secs(60)),
            tls: None,
        });

        let (open, _) = OpenGuard::acquire(Arc::clone(&limiter));
        let conn = Conn::new(
            stream,
            Accepted {
                peer,
                local_port: addr.port(),
            },
            open,
            None,
            config,
            metrics,
            cache,
            upstream,
            None,
            Arc::new(workers::Workers::new(0)),
            // 継承していない経路 (Linux 以外・継承に失敗した環境) をわざと通す
            None,
            1,
            None,
            false,
        )
        .expect("timeout 0 は無期限なので Conn::new は成功する");
        assert_eq!(
            conn.client.write_timeout().unwrap(),
            None,
            "0 は無期限として設定される"
        );
        assert_eq!(limiter.open(), 1);
        drop(conn);
        assert_eq!(limiter.open(), 0);
    }

    /// ワーカーに渡せなかった仕事が落ちるときも持ち分が戻ること (T9.6)。
    ///
    /// `Workers::run` は失敗すると仕事 (`Box<dyn FnOnce>`) を呼び出し元へ返す。
    /// `serve` はそれを落とすだけなので、番人が仕事の中に入っていることが返却の条件。
    #[test]
    fn open_slot_comes_back_when_the_job_is_dropped() {
        let limiter = Limiter::new();
        let (open, _) = OpenGuard::acquire(Arc::clone(&limiter));
        // `let _ = open;` だと RFC 2229 の分離キャプチャで閉包が捕まえないので `drop` で使う
        let job: Box<dyn FnOnce() + Send> = Box::new(move || drop(open));
        assert_eq!(limiter.open(), 1, "仕事が持ち分を抱えている");
        drop(job);
        assert_eq!(limiter.open(), 0, "呼ばれずに落ちても戻る");
    }
}
