//! 要求行を待つ長さ ([`first_line_wait`]) と、要求行の読み ([`read_request_line`])。T.34.11

use super::*;

use super::header_read::{
    HeaderClock, Line, MAX_LINE, header_timeout, is_timeout, overload, read_line_or_idle, refuse,
};
use super::request_line::RequestLineError;

/// 断る・閉じるときに要る、この接続の素性といまの要求の番号 (借りるだけ。T.34.11)。
#[derive(Clone, Copy)]
pub(super) struct Peer<'a> {
    pub(super) client: &'a TcpStream,
    pub(super) metrics: &'a Metrics,
    /// 接続元 IP (接続ごとに 1 回作ったもの)
    pub(super) ip: &'a str,
    pub(super) conn_id: usize,
    /// この接続で処理し終えた要求の数 (`Conn::served`)
    pub(super) served: usize,
    /// 上限の外で受けた接続か (T13.2)
    pub(super) overflow: bool,
    /// その枠を使ったのが接続元ごとの上限か (断った 1 本はそちらに数える。T14.13)
    pub(super) over_client: bool,
    /// 要求行とヘッダーを読み終える期限 (`PROXY_HEADER_TIMEOUT_SECS`。T.28.0)
    pub(super) header_timeout: std::time::Duration,
}

impl Peer<'_> {
    /// この接続で要求を断る ([`refuse`] の形。WARN の文面は `warn` が持つ)。
    #[inline]
    pub(super) fn refuse(
        self,
        why: metrics::BadRequestReason,
        status: u16,
        reason: &str,
        warn: impl FnOnce(crate::log::Skipped),
    ) -> io::Result<Step> {
        refuse(
            self.client,
            self.metrics,
            self.ip,
            why,
            status,
            reason,
            warn,
        )
    }
}

/// 文法を通らなかった要求行を断る (400 / 505。`check_request_line`)。
///
/// **個票に要求行そのものは入れない** (壊れた行にも URL が載っているため)。
/// WARN は理由ごとに 1 分に 1 行 (T.28.9)。
#[cold]
pub(super) fn refuse_request_line(
    peer: Peer<'_>,
    line: &str,
    e: RequestLineError<'_>,
) -> io::Result<Step> {
    let conn_id = peer.conn_id;
    let status = e.status();
    match e {
        // メソッドが token として読めなければ `method`、読めるなら `request_line` (T14.28)。
        // 今までは応答を返さずに閉じていた。理由を数えるからには**返した状態コードを
        // 個票に正しく書く**必要があるので、414 / 431 と同じ形で 400 を返してから閉じる
        RequestLineError::Malformed => peer.refuse(
            metrics::BadRequestReason::of_request_line(line),
            status,
            "Bad Request",
            |more| {
                log_warn!(
                    Some(conn_id),
                    "malformed request line: {}{}",
                    crate::log::Clip(line.trim()),
                    more
                )
            },
        ),
        RequestLineError::Grammar => peer.refuse(
            metrics::BadRequestReason::RequestLine,
            status,
            "Bad Request",
            |more| {
                log_warn!(
                    Some(conn_id),
                    "400 Bad Request: request line is not `method target HTTP/x.y`: {}{}",
                    crate::log::Clip(line.trim()),
                    more
                )
            },
        ),
        RequestLineError::Version(version) => peer.refuse(
            metrics::BadRequestReason::RequestLine,
            status,
            "HTTP Version Not Supported",
            |more| {
                log_warn!(
                    Some(conn_id),
                    "505 HTTP Version Not Supported: {}{}",
                    crate::log::Clip(version),
                    more
                )
            },
        ),
        RequestLineError::Asterisk(method) => peer.refuse(
            metrics::BadRequestReason::BadUri,
            status,
            "Bad Request",
            |more| {
                log_warn!(
                    Some(conn_id),
                    "400 Bad Request: `*` target with {}{}",
                    crate::log::Clip(method),
                    more
                )
            },
        ),
    }
}

/// 要求行を待つ長さ: `(猶予, 読み取りタイムアウト)`。猶予が `Some` なら、空振りで預ける。
///
/// `served` はこの接続で処理し終えた要求の数、`parkable` は預け先があるか、`overflow` は
/// 上限の外で受けた接続か (T13.2)。
#[inline]
pub(super) fn first_line_wait(
    config: &Config,
    served: usize,
    parkable: bool,
    overflow: bool,
) -> (Option<std::time::Duration>, std::time::Duration) {
    // 2 回目以降で監視スレッドに預けられるなら、待つのは猶予のあいだだけ。
    // 空振りしたら「暇だ」と解釈して預ける (poll を別に呼ばずに済む)。
    // **`config.timeout` が 0 (無期限) でもここは変わらない**: 猶予は `park_grace` の
    // 長さで、`timeout` とは別物なので、空振り = 「暇だ」の判定はそのまま起きる
    // (T6.5 の預ける仕組みは無期限でも動く。結合テストで確かめてある)
    let grace =
        (served > 0 && parkable && !config.park_grace.is_zero() && !config.keepalive.is_zero())
            .then_some(config.park_grace);
    // 最初の要求は `PROXY_HEADER_TIMEOUT_SECS` と通常のタイムアウトの小さい方
    // (accept から 1 バイト目までを縛る。T.28.0)、2 回目以降は keep-alive のアイドル時間で待つ。
    // 両方 0 なら 1 要求目は無期限に待つ (= 何も送ってこない接続を閉じない。T10.6。
    // `PROXY_TIMEOUT_SECS=0` だけでは、未設定のヘッダーの締め切り (10 秒) が残る)
    let first = config::first_read_timeout(config.timeout, config.header_timeout);
    let wait = match (grace, served) {
        (Some(g), _) => g,
        // 上限の外で受けた接続は、要求行を待つ時間を切る (T13.2)。枠は `OVERFLOW_SLOTS` 本
        // しかないので、黙ったままの接続に占領させると `/status` が取れなくなる
        (None, 0) if overflow => match first {
            t if t.is_zero() => OVERFLOW_READ_TIMEOUT,
            t => t.min(OVERFLOW_READ_TIMEOUT),
        },
        (None, 0) => first,
        (None, _) => config.keepalive,
    };
    (grace, wait)
}

/// 要求行を 1 行読む (猶予つき)。読めたら `None` (中身は `line`)。
///
/// 預ける・閉じる・断る (414 / 408 / 503) なら、`serve_one` がそのまま返す結果を `Some` で返す。
/// 読めた普通の要求は比較 1 回で抜け、ほかの形は [`end_of_first_line`] (冷たい側) が受ける。
#[inline]
pub(super) fn read_request_line(
    reader: &mut clientio::ClientReader<'_>,
    line: &mut String,
    extend: Option<std::time::Duration>,
    extended: &mut bool,
    clock: &mut HeaderClock<'_>,
    peer: Peer<'_>,
) -> Option<io::Result<Step>> {
    match read_line_or_idle(reader, line, extend, true, extended, clock) {
        Ok(Line::Read(Some(n))) if n > 0 => None,
        r => Some(end_of_first_line(r, clock, peer)),
    }
}

/// 要求行が読めなかったとき (預ける・閉じる・断る) の 1 歩。
#[cold]
#[inline(never)]
fn end_of_first_line(
    read: io::Result<Line>,
    clock: &HeaderClock<'_>,
    peer: Peer<'_>,
) -> io::Result<Step> {
    let Peer {
        client,
        metrics,
        ip: peer_ip,
        conn_id,
        served,
        overflow,
        over_client,
        header_timeout: limit,
    } = peer;
    match read {
        Ok(Line::Idle) => Ok(Step::Park),
        // 理由別に 1 件数えて個票にも残す (`/errors`。T14.28)。**断る経路だけ**を通るので
        // 通した要求には 1 命令も足していない。WARN は理由ごとに 1 分に 1 行 (T.28.9)
        Ok(Line::Read(None)) => refuse(
            client,
            metrics,
            peer_ip,
            metrics::BadRequestReason::RequestLine,
            414,
            "URI Too Long",
            |more| {
                log_warn!(
                    Some(conn_id),
                    "414 URI Too Long (request line over {} bytes){}",
                    MAX_LINE,
                    more
                )
            },
        ),
        Ok(Line::Read(Some(0))) => {
            log_debug!(Some(conn_id), "client closed ({} requests served)", served);
            Ok(Step::Close(recent::CloseReason::ClientEof))
        }
        // 読めた行は `read_request_line` が先に抜けている
        Ok(Line::Read(Some(_))) => unreachable!("a read line returns before this"),
        // 1 バイト目から期限を越えた (1 バイトずつの trickle。2 要求目以降も)
        Err(e) if is_timeout(&e) && clock.expired() => {
            header_timeout(client, metrics, peer_ip, conn_id, limit)
        }
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
            Ok(Step::Close(recent::CloseReason::KeepaliveTimeout))
        }
        // 上限の外で受けたのに何も送ってこない接続 (T13.2)。自分宛てか分からないまま
        // 枠を握らせておけないので、待つのをやめて 503 で閉じる
        Err(e)
            if overflow
                && matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
        {
            Ok(overload(
                client,
                metrics,
                conn_id,
                "a silent connection",
                false,
                over_client.then_some(peer_ip),
            ))
        }
        // 1 要求目の 1 バイト目も来ないまま締め切りを過ぎた = 先張りの接続 (T.33.0)。
        // 応答を読まない相手なので 408 は返さずに黙って閉じ、数は `silent_closes` だけに
        // (`rejected_requests` にも `/errors` にも積まない)。0.32.39 の tokyo の 48.3 時間の 408 の
        // 3,415 件は宛先も要求も無いもので、1 つの接続元から波 (20.5 秒おき・88 秒おき) で来ていた。
        // 1 バイトでも送ってから止まった接続 (本物の遅い要求) は下の 408 のまま
        Err(e) if served == 0 && !clock.started && clock.is_header_timeout(&e, 0) => {
            metrics.silent_closes.fetch_add(1, Ordering::Relaxed);
            log_debug!(
                Some(conn_id),
                "closed a silent connection (no byte within {} s)",
                limit.as_secs_f64()
            );
            Ok(Step::Close(recent::CloseReason::IdleTimeout))
        }
        // 1 要求目の途中で黙った・期限切れ (T.28.0)
        Err(e) if clock.is_header_timeout(&e, served) => {
            header_timeout(client, metrics, peer_ip, conn_id, limit)
        }
        Err(e) => Err(e),
    }
}
