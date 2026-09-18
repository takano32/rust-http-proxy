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
use crate::metrics::{Detail, ErrCause, HostOutcome, Metrics, StageMs};
use crate::net;
use crate::recent::{
    CLIENT_SIDE, CloseReason, ConnSlot, ConnTally, ORIGIN_SIDE, SIDES, STAGE_CLIENT_READ,
    STAGE_CONNECT, STAGE_DNS, STAGE_FIRST_RELAY, STAGE_QUEUE, STAGES,
};
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
    /// 確立までの内訳 (名前解決 / 接続 / 勝った族。T12.4 (2)) と段階 (T14.3 (1))
    detail: Detail,
    /// `200 Connection Established` を書き終えた時刻 (`first_relay` の起点。T14.3 (1))。
    /// `--lite` では `None` = 時計を読まない
    established: Option<Instant>,
    metrics: Arc<Metrics>,
    /// `/connections` の枠 (T13.4)。状態と運んだバイト数をここに書く。
    /// 登録と抹消は本体クレート (接続の開始と終了) の仕事で、ここは書くだけ
    slot: Option<Arc<ConnSlot>>,
    /// この接続がトンネルになるまでに預かり所にいた ms (原子の読み 1 回。T14.25)。
    ///
    /// 枠の預かり秒は**接続**のもので、HTTP の keep-alive で預けられていた分も入って
    /// いる。中継の時間から引くのは**トンネルになってからの預け**だけなので、
    /// ここで印を取って差だけを使う (同じ接続で `GET` のあとに `CONNECT` が来る場合)
    parked_ms_at_start: u64,
    /// 覗いた SNI が CONNECT のホストと食い違ったか (`PROXY_PEEK_SNI`。T14.38)。
    ///
    /// 名前そのものは覗いたその場で `/connections` の枠 ([`ConnSlot::set_sni`]) へ
    /// 書くので、ここに持つのは統計に足す旗 1 つだけ (`report` が `Detail` に載せる)
    sni_mismatch: bool,
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
    mut stages: StageMs,
    read_started: Option<Instant>,
) -> io::Result<Opened> {
    let started = Instant::now();
    // `client_read` は「要求行が届いてからここまで」(入口で読んだ時計をそのまま使うので、
    // 1 本あたりの時計は増えない。T14.3 (1))
    if let Some(t) = read_started {
        stages.client_read = crate::profile::ms_u32(started.saturating_duration_since(t));
    }
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
            let detail = detail_of(started.elapsed(), Some(ErrCause::from_io(&e)), stages);
            metrics.record_host_detail(
                &format!("connect://{}", addr_str),
                HostOutcome::Error,
                0,
                Some(started.elapsed()),
                detail,
            );
            // 個票にも 1 件残す (`/errors`。誰の・いつ・なぜ。T13.4)
            metrics.record_error(true, &addr_str, &client_ip, 502, &detail);
            // 閉じた接続の個票にも相手を残す (`/recent`。**エラーの経路だけ**。T14.4)
            if let Some(s) = &slot {
                s.failed_tunnel(&addr_str);
                let mut stage_ms = [0u64; STAGES];
                stage_ms[STAGE_DNS] = detail.dns_ms;
                stage_ms[STAGE_CONNECT] = detail.connect_ms;
                s.finish(
                    CloseReason::Error(ErrCause::from_io(&e)),
                    ConnTally {
                        up: 0,
                        down: 0,
                        status: 502,
                        stage_ms,
                        // 繋がらなかったのでカーネルに聞ける相手がいない (T14.5)
                        ..ConnTally::default()
                    },
                    0,
                );
            }
            metrics.record_client(
                &client_ip,
                HostOutcome::Error,
                0,
                // 繋がらなかったので運んだバイトは上りも下りも無い (T14.26)
                (0, 0),
                Some(started.elapsed()),
                Some(&addr_str),
            );
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
    let detail = detail_of(connect_took, None, stages);
    // `/connections` の 1 行を「CONNECT の中継中」にする (1 本につき 1 回だけ。T13.4)
    if let Some(s) = &slot {
        s.begin_tunnel(&addr_str);
    }
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    client.flush()?;
    // トンネル越しの TLS 握手の往復を測る起点 (T14.3 (1))。`--lite` では読まない
    let established = crate::profile::mark();
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
            established,
            metrics,
            parked_ms_at_start: slot.as_ref().map_or(0, |s| s.parked_ms()),
            slot,
            sni_mismatch: false,
        },
    })
}

/// 確立までの内訳を組み立てる。**呼ぶのは接続が終わった直後の 1 回だけ**
/// (thread-local を読んで 0 に戻すので、2 回呼ぶと 2 回目が空になる)。
///
/// 接続にかかった時間は「全体 − 名前解決」で出す。`net` 側に測る口を足すと
/// T12.7 (同じところを直しているタスク) と衝突するので、**引き算で済ませている**。
fn detail_of(total: Duration, cause: Option<ErrCause>, stages: StageMs) -> Detail {
    let (dns_ms, dns_misses) = crate::dns::take_resolve_cost();
    let total_ms = total.as_millis().min(u64::MAX as u128) as u64;
    Detail {
        dns_ms,
        dns_misses,
        connect_ms: total_ms.saturating_sub(dns_ms),
        family_v6: crate::dns::take_family(),
        // 確立までに SYN を送り直した回数 (T14.46)。読んだのは `net` の確立点の
        // `getsockopt` 1 回で、ここは thread-local を読んで 0 に戻すだけ
        syn_retrans: crate::dns::take_syn_retrans(),
        cause,
        // CONNECT は「確立まで」がそのまま窓に入る値なので指定しない
        first_byte_ms: None,
        // `queue` と `client_read` は本体クレートが測った値 (T14.3 (1))
        stages,
        // 向き別のバイト (T14.26) は終わってからでないと分からないので `report` が入れる
        ..Detail::default()
    }
}

/// 両側のカーネルの RTT (us) と再送の通算を読む (`[クライアント側, オリジン側]`)。
///
/// **呼ぶのはトンネル 1 本の終わりだけ** (`getsockopt` 2 回)。読めなければ 0 で、
/// 個票では `null`、統計には足さない。Linux 以外はソケットを見ずに 0。
#[cfg(target_os = "linux")]
fn tcp_rtt(socks: Option<&[TcpStream; 2]>) -> ([u32; SIDES], [u32; SIDES]) {
    use std::os::fd::AsRawFd;
    let mut rtt = [0u32; SIDES];
    let mut retrans = [0u32; SIDES];
    if let Some(socks) = socks {
        for (i, s) in socks.iter().enumerate() {
            if let Some(info) = crate::sys::tcp_info(s.as_raw_fd()) {
                rtt[i] = info.rtt_us;
                retrans[i] = info.total_retrans;
            }
        }
    }
    (rtt, retrans)
}

#[cfg(not(target_os = "linux"))]
fn tcp_rtt(_socks: Option<&[TcpStream; 2]>) -> ([u32; SIDES], [u32; SIDES]) {
    ([0; SIDES], [0; SIDES])
}

/// トンネルが終わったときのアクセスログと統計 (どこで終わっても 1 回だけ通る)。
///
/// `up` はクライアント → 宛先、`down` は宛先 → クライアントのバイト数。
/// `reason` は閉じた理由 (`/recent`。T14.4) で、外から閉じられた (追い出し・監視の停止)
/// ときは枠に先に書いてある理由が勝つ。
fn report(
    o: &Info,
    up: u64,
    down: u64,
    reason: CloseReason,
    socks: Option<&[TcpStream; 2]>,
    half_close: Option<Duration>,
    stall_ms: [u32; SIDES],
) {
    let transferred = up.saturating_add(down);
    // トンネルの寿命 (時計はここで 1 回だけ読み、アクセスログ・T14.3 の段階・
    // T14.25 の中継の時間で使い回す)
    let alive = o.started.elapsed();
    // 中継の合計 = 生きていた時間 − 確立まで − 預けられていた時間 (T14.3 (1))。
    // **時計は足さない** (アクセスログが読む `alive` をそのまま使う)
    let mut detail = o.detail;
    if crate::profile::on() {
        let total = alive.as_millis().min(u64::MAX as u128) as u64;
        let head =
            o.connect_took.as_millis().min(u64::MAX as u128) as u64 + detail.stages.park as u64;
        detail.stages.relay = total.saturating_sub(head).min(u32::MAX as u64) as u32;
    }
    // カーネルの RTT と再送を**両側 1 本ずつ** (`getsockopt` 2 回。トンネル 1 本の
    // 終わりだけで、中継のバイトごとにも要求ごとにも読まない。T14.5)
    let (rtt_us, retrans) = tcp_rtt(socks);
    // 閉じた接続の個票に 1 件ぶんの値を載せる (原子は接続の終わりのここだけ。T14.4)。
    // 実際にリングへ書くのは本体クレートの `ActiveGuard::drop` (= この直後)。
    // **段階は上で組み立てた `detail`** (T14.3 の `first_relay` まで入っている) から取る
    if let Some(s) = &o.slot {
        let mut stage_ms = [0u64; STAGES];
        stage_ms[STAGE_DNS] = detail.dns_ms;
        stage_ms[STAGE_CONNECT] = detail.connect_ms;
        stage_ms[STAGE_QUEUE] = detail.stages.queue as u64;
        stage_ms[STAGE_CLIENT_READ] = detail.stages.client_read as u64;
        stage_ms[STAGE_FIRST_RELAY] = detail.stages.first_relay as u64;
        s.finish(
            reason,
            ConnTally {
                up,
                down,
                status: 0,
                stage_ms,
                rtt_us,
                retrans,
                // 確立までの SYN の再送 (T14.46)。確立の直後に読んだ値をそのまま運ぶ
                syn_retrans: detail.syn_retrans,
                // 中継が書けるのを待った ms (T14.42)。中継のループで数え終えた
                // ものをそのまま運ぶだけで、ここでは時計も割り算も無い
                stall_ms,
            },
            0,
        );
        // 速さと半閉じの分布に 1 本足す (T14.25)。**中継の時間 = 寿命 − 確立まで −
        // 預かり所にいた時間** (預けは利用者が待っていない時間なので中継ではない。
        // T14.3 の `stages.relay` と同じ引き算だが、あちらは `/profile` が止まっていると
        // 0 なので、ここは枠の預かり秒 (T14.4) から出す = プロファイルの設定に依らない)。
        // ここは既に「個票を残す接続」(= `--lite` ではない) の内側で、時計も上で 1 回
        // 読んだものを使うので、足すのは引き算と割り算 1 回ずつと窓の鍵 1 回だけ
        let parked = s.parked_ms().saturating_sub(o.parked_ms_at_start);
        let relay = alive
            .saturating_sub(o.connect_took)
            .saturating_sub(Duration::from_millis(parked));
        o.metrics
            .history
            .transfer
            .observe(transferred, relay, half_close, stall_ms);
        // 接続元 1 つの追跡 (`/trace`。T14.27)。**旗が立っているトンネルだけ** 1 行書く。
        // 立っていない本数の費用はこの分岐 1 回だけで、宛先・段階の ms・閉じた理由・
        // 寿命はすぐ上で既に組んだものをそのまま渡す (時計も確保も増やさない)
        if s.traced() {
            crate::trace::push(crate::trace::Line {
                conn_id: o.conn_id,
                client: &o.client_ip,
                method: "CONNECT",
                target: &o.addr_str,
                version: "HTTP/1.1",
                status: 200,
                took_ms: alive.as_millis().min(u64::MAX as u128) as u64,
                bytes: transferred,
                stage_ms,
                reason: Some(reason),
            });
        }
    }
    // 向き別のバイト (T14.26)。**数え直しはしていない**: 中継が方向ごとに持っている
    // `up` / `down` (すぐ上で個票に渡したのと同じ値) をホスト別統計の鍵の内側へ
    // 運ぶだけなので、足し算 2 回のほかに費用は無い。`--lite` (枠が無い = 上の
    // `if let` を通らない) でもホスト別統計は生きているので、ここは枠の外に置く
    detail.bytes_in = up;
    detail.bytes_out = down;
    // CONNECT のホストと SNI の食い違い (T14.38)。旗は中継の入口で 1 回だけ立ててあり、
    // ここはホスト別統計が既に取る鍵の内側へ運ぶだけ (原子もシステムコールも増えない)
    detail.sni_mismatch = o.sni_mismatch;
    // 自己ベンチ (T14.43) のトンネルは合計にも足さない (`/hosts` と同じ理由)
    if !crate::selfbench::is_target(&o.addr_str) {
        o.metrics.add_bytes(transferred);
    }
    let host_key = format!("connect://{}", o.addr_str);
    o.metrics.record_host_detail(
        &host_key,
        HostOutcome::Bypass,
        transferred,
        Some(o.connect_took),
        detail,
    );
    o.metrics.record_client(
        &o.client_ip,
        HostOutcome::Bypass,
        transferred,
        (up, down),
        Some(o.connect_took),
        // 接続元の個票に宛先の種類とポートを数える (`/clients`。T14.7)。
        // トンネル 1 本の終わりに 1 回だけで、鍵は record_client のものをそのまま使う
        Some(&o.addr_str),
    );
    // オリジン側の RTT はホスト別、クライアント側は接続元別へ (どちらも行が
    // できたあと = 上の 2 つより後に呼ぶこと。読めなければ鍵も取らない。T14.5)
    o.metrics
        .record_host_rtt(&host_key, rtt_us[ORIGIN_SIDE], retrans[ORIGIN_SIDE]);
    o.metrics
        .record_client_rtt(&o.client_ip, rtt_us[CLIENT_SIDE], retrans[CLIENT_SIDE]);
    access(
        o.conn_id,
        &Access {
            client: &o.client_ip,
            method: "CONNECT",
            target: &o.addr_str,
            version: "HTTP/1.1",
            status: "200",
            bytes: transferred,
            duration_ms: alive.as_secs_f64() * 1000.0,
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
    stages: StageMs,
    read_started: Option<Instant>,
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
            stages,
            read_started,
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let Opened {
            client,
            server,
            info,
        } = open(
            client,
            target,
            prefix,
            timeout,
            conn_id,
            metrics,
            client_ip,
            resolved,
            slot,
            stages,
            read_started,
        )?;
        let (up, down) = tunnel(client, server, idle)?;
        // Linux 以外は片方向ずつ `io::copy` するだけなので、どちらが先に EOF を出したかは
        // 分からない (`/recent` の理由は `shutdown` になる)
        // Linux 以外は `io::copy` が終わった時点でソケットを手放しているので読めない
        // 半閉じ (片側 EOF) がいつ起きたかも分からない (T14.25)
        // 詰まりの向き (T14.42) も `io::copy` の中で待つので数えられない (両方 0)
        report(
            &info,
            up,
            down,
            CloseReason::Shutdown,
            None,
            None,
            [0; SIDES],
        );
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
    stages: StageMs,
    read_started: Option<Instant>,
) -> io::Result<()> {
    let opened = open(
        client,
        target,
        prefix,
        timeout,
        conn_id,
        metrics,
        client_ip,
        resolved,
        slot,
        stages,
        read_started,
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

/// 双方向にデータを中継し、(上り, 下り) のバイト数を返す (Linux 以外)。
///
/// `idle` 秒だけ双方向とも動きが無ければ閉じる (`None` で無期限)。
/// Linux では 1 スレッドで `poll(2)` を回して `splice(2)` でカーネル内をコピーする
/// ([`relay`]。接続あたりのスレッドが 3 本から 1 本に減り、ユーザー空間へのコピーも無くなる)。
#[cfg(not(target_os = "linux"))]
pub fn tunnel(
    client: TcpStream,
    server: TcpStream,
    idle: Option<Duration>,
) -> io::Result<(u64, u64)> {
    copy_both_ways(client, server, idle)
}

/// 2 スレッドで双方向に `io::copy` する従来版 (Linux 以外)。
#[cfg(not(target_os = "linux"))]
fn copy_both_ways(
    mut client: TcpStream,
    mut server: TcpStream,
    idle: Option<Duration>,
) -> io::Result<(u64, u64)> {
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
    let _ = total.load(Ordering::Relaxed);
    Ok((sent, received))
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
    use crate::recent::{CLIENT_SIDE, CloseReason, ConnSlot, ConnState, ORIGIN_SIDE, SIDES};
    use crate::sys::{self, POLLERR, POLLHUP, POLLIN, POLLOUT, Pipe, PollFd};

    /// 1 回の splice / read で動かす最大バイト数 (パイプ容量と同じ)。
    const CHUNK: usize = 1 << 20;

    /// 「相手が黙って消えた」= `ETIMEDOUT` か (T14.52)。
    ///
    /// TCP keepalive (`PROXY_TCP_KEEPALIVE`) が尽きると、カーネルはそのソケットの
    /// 保留エラーを `ETIMEDOUT` にする。以後の `splice` / `read` / `write` / `poll` は
    /// この errno で返るので、閉じた理由を `client_dead` と書ける。
    /// `SO_RCVTIMEO` / `SO_SNDTIMEO` の締め切りは Linux では `EAGAIN` (= `WouldBlock`)
    /// なので**ここには混ざらない**。
    fn went_away(e: &io::Error) -> bool {
        e.kind() == io::ErrorKind::TimedOut
    }

    /// `poll` に渡す記述子。**関心 (`events`) が無いなら `-1`** (T15.5)。
    ///
    /// `poll(2)` は負の記述子を無視して `revents` を 0 にする。関心が 0 の記述子を
    /// そのまま渡すと、`events` に関係なく返る `POLLERR` / `POLLHUP` で
    /// 「誰も面倒を見ない起床」が起きて空回りになる (呼ぶ側の注記を見よ)。
    fn poll_fd(sock: &TcpStream, events: i16) -> RawFd {
        if events == 0 { -1 } else { sock.as_raw_fd() }
    }

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
                        // 相手が消えた (T14.52): これは「splice が使えない」ではなく
                        // 「接続が死んだ」なので、64 KiB の緩衝を確保して読み直さずに
                        // そのまま上へ返す (呼び出し側が `client_dead` と書く)
                        Err(e) if went_away(&e) => Err(e),
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
        /// `200` を書いてから最初の中継バイトまで (ms。T14.3 (1))
        first_relay_ms: u32,
        /// いま預けられているなら、預けた時刻 (T14.3 (1))
        parked_at: Option<Instant>,
        /// 預けられていた合計 (ms)
        parked_ms: u32,
        /// 中継の中で決まった閉じた理由 (アイドル打ち切り / `poll` の失敗。T14.4)。
        /// 外から閉じられたとき (追い出し・監視の停止) は枠に直接書いてあるので
        /// ここは `None` のままで、`ConnSlot::finish` の先着優先でそちらが勝つ
        close: Option<CloseReason>,
        /// 先に EOF を出したのはどちらと、それがいつか (`0` = クライアント、`1` = 宛先)。
        ///
        /// 側は閉じた理由 (T14.4)、時刻は**半閉じから反対側が閉じるまで**の分布 (T14.25)。
        /// 書くのは `get_or_insert_with` の中 = **トンネル 1 本につき多くて 1 回**で、
        /// バイトを動かす道 (splice の往復) には 1 命令も足していない
        first_eof: Option<(usize, Instant)>,
        /// まだ SNI を覗いていないか (`PROXY_PEEK_SNI`。T14.38)。
        ///
        /// 立っているのは「個票の枠があり (= `--lite` ではない)、宛先が 443
        /// (または試験用の口) の CONNECT」だけで、**最初にクライアント側が読めた
        /// ときに 1 回覗いて倒す** (トンネル 1 本に `recv(MSG_PEEK)` は多くて 1 回)
        peek_sni: bool,
        /// 書けるのを待った合計 (us。`[クライアント側, オリジン側]`。T14.42)。
        ///
        /// 添字は `socks` の添字と同じで、`0` は「**クライアントへ**書けなくて待った」
        /// (= 利用者の下り回線か端末が読んでいない)、`1` は「**オリジンへ**書けなくて
        /// 待った」(= オリジンか利用者の上りが詰まっている)。**時計を読むのは
        /// `poll` で書けるのを待ちに入る回だけ**で、64 KiB ごとにも splice ごとにも
        /// 読まない (詰まらない中継は 1 回も読まない)。ms ではなく us で積むのは、
        /// 1 ms に満たない待ちを何度も繰り返すトンネルで切り捨てが積み上がらないように
        stall_us: [u64; SIDES],
        /// 本体クレートの持ち分 (同時接続数と `active_connections`)。中身は見ない
        _hold: Box<dyn Send>,
    }

    /// 積んだ us を個票の ms にする (**時計は読まない**。四捨五入は T14.25 と同じ)。
    fn stall_ms(us: [u64; SIDES]) -> [u32; SIDES] {
        let mut out = [0u32; SIDES];
        for (o, v) in out.iter_mut().zip(us) {
            *o = ((v + 500) / 1000).min(u32::MAX as u64) as u32;
        }
        out
    }

    impl Drop for Idle {
        fn drop(&mut self) {
            // 空のパイプはスレッドの置き場へ返す (次のトンネルが pipe2 と fcntl を省ける)
            for d in self.dirs.iter_mut() {
                d.drop_relay();
            }
            // 段階 (T14.3 (1))。`relay` は `report` が引き算で出す
            self.info.detail.stages.first_relay = self.first_relay_ms;
            self.info.detail.stages.park = self.parked_ms;
            // 閉じた理由 (T14.4): 中継の中で決まっていればそれ、決まっていなければ
            // **先に EOF を出した側**。どちらも無ければプロキシ側の都合 (`shutdown`)
            let reason = self.close.unwrap_or(match self.first_eof {
                Some((0, _)) => CloseReason::ClientEof,
                Some(_) => CloseReason::ServerEof,
                None => CloseReason::Shutdown,
            });
            // 半閉じで終わったトンネルだけ、半閉じから反対側が閉じる (= いま) までを
            // 数える (T14.25)。両側とも EOF を出さずに終わったものは `None`
            let half_close = self.first_eof.map(|(_, at)| at.elapsed());
            // `dirs[0]` はクライアント → 宛先 (上り)、`dirs[1]` は宛先 → クライアント (下り)
            // `socks` はまだ生きている (落ちるのはこの関数を抜けたあと)。T14.5
            report(
                &self.info,
                self.dirs[0].moved,
                self.dirs[1].moved,
                reason,
                Some(&self.socks),
                half_close,
                // 中継の詰まりの向き (T14.42)。中継のループで積んだ us を ms に丸めるだけ
                stall_ms(self.stall_us),
            );
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
            self.parked_at = None;
        }

        /// 預かり所から戻ってきた (預けられていた時間を足す。T14.3 (1))。
        fn unpark(&mut self) {
            if let Some(t) = self.parked_at.take() {
                self.parked_ms = self
                    .parked_ms
                    .saturating_add(crate::profile::ms_u32(t.elapsed()));
            }
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
            // 預けた時刻 (T14.3 (1))。預かってもらえなければ `no_park` が消す
            self.parked_at = crate::profile::mark();
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
            let established = self.info.established;
            let dirs = &mut self.dirs;
            let transferred = &mut self.transferred;
            let first_relay = &mut self.first_relay_ms;
            // 閉じた理由に使う 2 つ (T14.4)。どちらも 1 本の終わりに 1 回書くだけ
            let close = &mut self.close;
            let first_eof = &mut self.first_eof;
            // SNI を覗く枠 (T14.38)。`Info` の別々の欄なので `slot` と一緒に借りられる
            let peek_sni = &mut self.peek_sni;
            let sni_mismatch = &mut self.info.sni_mismatch;
            let addr_str = &self.info.addr_str;
            // 書けるのを待った時間 (T14.42)。預けても引き継ぐので `Idle` の欄
            let stall_us = &mut self.stall_us;

            loop {
                let mut progressed = false;
                for d in dirs.iter_mut() {
                    if d.done {
                        continue;
                    }
                    // 送信元 → 中継 (読めると分かってから中継バッファを用意する)
                    if !d.src_eof && d.pending == 0 && d.readable {
                        // CONNECT の最初のバイトから SNI を覗く (T14.38)。**`200` を
                        // 書いたあと、最初の中継の前に 1 回だけ** `recv(MSG_PEEK)` で、
                        // バイトは消費しないので下の `fill` (splice) はそのまま通る。
                        // 壊れていれば `None` で中継は続く (ここで閉じない)
                        if *peek_sni && d.src == 0 {
                            *peek_sni = false;
                            let mut buf = [0u8; crate::sni::PEEK_LEN];
                            if let Ok(n) = sys::peek(socks[0].as_raw_fd(), &mut buf)
                                && let Some(name) = crate::sni::parse_client_hello(&buf[..n])
                            {
                                if let Some(s) = slot {
                                    s.set_sni(name);
                                }
                                *sni_mismatch = crate::sni::differs(addr_str, name);
                            }
                        }
                        match d.fill(socks) {
                            Ok(0) => {
                                d.src_eof = true;
                                first_eof.get_or_insert_with(|| (d.src, Instant::now()));
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
                            Err(ref e) => {
                                // 消えたクライアント (T14.52)。keepalive が尽きた
                                // ソケットは `ETIMEDOUT` で返るので、`client_eof`
                                // (ふつうに切った) と区別して記録する。オリジン側の
                                // `ETIMEDOUT` は「相手が消えた」ではあってもクライアント
                                // ではないので、従来どおり EOF として扱う
                                if d.src == CLIENT_SIDE && went_away(e) {
                                    *close = Some(CloseReason::ClientDead);
                                }
                                d.src_eof = true;
                                first_eof.get_or_insert_with(|| (d.src, Instant::now()));
                                progressed = true;
                            }
                        }
                    }
                    // 中継 → 送信先
                    while d.pending > 0 {
                        match d.drain(socks) {
                            Ok(0) => break,
                            Ok(n) => {
                                // 最初の中継バイト (T14.3 (1))。比較 1 回で、時計を読むのは
                                // 1 本につき 1 回だけ (`--lite` では `established` が `None`)
                                if *transferred == 0
                                    && let Some(t) = established
                                {
                                    *first_relay = crate::profile::ms_u32(t.elapsed());
                                }
                                d.pending -= n;
                                d.offset += n;
                                d.moved += n as u64;
                                *transferred += n as u64;
                                progressed = true;
                            }
                            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            // 送信先が閉じた: この方向は終わり、相手にも伝える
                            Err(ref e) => {
                                // 書こうとした先が消えたクライアントだったとき (T14.52)
                                if d.dst == CLIENT_SIDE && went_away(e) {
                                    *close = Some(CloseReason::ClientDead);
                                }
                                // 先に手を引いたのは**送信先**の側 (T14.4)
                                first_eof.get_or_insert_with(|| (d.dst, Instant::now()));
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
                // 相手が消えたトンネルは、残った向き (オリジン → クライアント) を
                // 待たずにここで閉じる (T14.52)。書き先が死んでいるのでオリジンから
                // 来るバイトはもう誰にも渡せず、待てば `PROXY_TUNNEL_IDLE_SECS`
                // (300 秒) までこのトンネルが席を占め続ける
                if matches!(close, Some(CloseReason::ClientDead)) {
                    log_trace!(Some(conn_id), "tunnel closed: the client went away");
                    break;
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
                // **関心の無い記述子は `poll` に渡さない** (T15.5)。`poll(2)` は
                // `events` が 0 でも `POLLERR` / `POLLHUP` を必ず返すので、渡すと
                // 「誰も面倒を見ない起床」が生まれる: 上の輪は `readable` を立てるだけで、
                // その記述子を読む向きが `done` なら輪の先頭で飛ばされ、反対の向きは
                // 自分の送信元しか見ていないので 1 バイトも進まない (`progressed = false`)。
                // `poll` が 0 を返さないのでアイドル打ち切りにも永久に当たらず、
                // **1 本のトンネルが 1 スレッドを回し続ける** (2026-09-18 のデプロイ先は
                // この形のトンネル 2 本で CPU 割り当て 0.5 コアを 27 時間食い切っていた。
                // `mtalk.google.com:5228`、齢 25.4 時間、通算 9 kB、0 bps)。
                //
                // 負の記述子は `poll(2)` が無視して `revents` を 0 にするので、これで
                // **`poll` が返した起床は必ず、その記述子への `fill` か `drain` の
                // システムコール 1 回につながる**という不変条件が立つ (関心が `POLLIN`
                // なら `fill` が EOF かエラーを返して進み、`POLLOUT` なら `drain` が
                // 書けるかエラーを返して進む)。どの向きも `done` でなければ `POLLIN` か
                // `POLLOUT` を 1 つは立てる (すぐ上の輪) ので、両方 0 になるのは
                // 「全部 `done`」= この手前で輪を抜けるときだけ。死んだ半閉じの
                // トンネルは今までどおりアイドル打ち切り (`PROXY_TUNNEL_IDLE_SECS`) で閉じる
                let mut fds = [
                    PollFd::new(poll_fd(&socks[0], events[0]), events[0]),
                    PollFd::new(poll_fd(&socks[1], events[1]), events[1]),
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
                // 中継の詰まりの向き (T14.42)。**書けなくて待ちに入る回だけ**時計を
                // 読む: `events` に `POLLOUT` が立つのは、直前の `drain` (splice) が
                // `EAGAIN` で止まって `pending` が残っている方向だけなので、
                // **書けば必ず入る相手 (loopback) では `Instant::now()` を 1 度も
                // 呼ばない** — ここの比較 1 回で終わる。64 KiB ごとでも splice ごとでも
                // なく「待ちに入る回」なので、`--lite` でも同じように読む
                // (T14.25 の「最初の EOF の 1 回」と同じ扱い)
                let stall_from =
                    ((events[CLIENT_SIDE] | events[ORIGIN_SIDE]) & POLLOUT != 0).then(Instant::now);
                let polled = sys::poll_fds(&mut fds, wait_ms);
                // 待ち終わったのでもう 1 回読み、**書けなかった向きだけ**に積む。
                // 両方詰まっていれば両方に積む (どちらも「その間書けなかった」ため)
                if let Some(t) = stall_from {
                    let waited = t.elapsed().as_micros().min(u64::MAX as u128) as u64;
                    for (side, ev) in events.iter().enumerate() {
                        if ev & POLLOUT != 0 {
                            stall_us[side] = stall_us[side].saturating_add(waited);
                        }
                    }
                }
                match polled {
                    Ok(0) if parkable => return Outcome::Idle,
                    Ok(0) if wait_ms >= 0 => {
                        log_trace!(Some(conn_id), "tunnel idle timeout after {}ms", wait_ms);
                        *close = Some(CloseReason::IdleTimeout);
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log_trace!(Some(conn_id), "tunnel poll failed: {}", e);
                        // `poll` が `ETIMEDOUT` を返すことはまず無いが、返ってきたなら
                        // 「相手が消えた」で間違いない (T14.52。比較 1 回)
                        *close = Some(if went_away(&e) {
                            CloseReason::ClientDead
                        } else {
                            CloseReason::Error(crate::metrics::ErrCause::from_io(&e))
                        });
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
        // 覗くかどうかは**トンネル 1 本につきここで 1 回**決める (中継のループでは
        // 旗を見るだけ)。個票の枠が無い `--lite` と、443 以外のポート (TLS とは
        // 限らない) では覗かない。T14.38
        let peek_sni = info.slot.is_some()
            && crate::sni::peek_on(
                crate::net::split_host_port_ref(&info.addr_str)
                    .1
                    .unwrap_or(0),
            );
        drive(Box::new(Idle {
            socks: [client, server],
            dirs: [Dir::new(0, 1), Dir::new(1, 0)],
            transferred: 0,
            idle,
            park,
            info,
            first_relay_ms: 0,
            parked_at: None,
            parked_ms: 0,
            close: None,
            first_eof: None,
            peek_sni,
            stall_us: [0; SIDES],
            _hold: hold,
        }));
        Ok(())
    }

    /// 預かっていたトンネルをワーカーで再開する (事象が来て起こされたとき)。
    pub fn resume(mut idle: Box<Idle>) {
        idle.unpark();
        if let Some(s) = idle.slot() {
            s.set_state(ConnState::Relaying);
        }
        drive(idle);
    }

    /// 期限切れで引き上げたトンネルを閉じる (`poll` が 0 を返したときと同じログと統計)。
    pub fn expire(mut idle: Box<Idle>) {
        idle.unpark();
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
