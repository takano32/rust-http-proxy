//! HTTP (平文) リクエストの転送とキャッシュの適用。
//!
//! 流れ: リクエスト解析 ([`request`]) → キャッシュ参照 (新鮮なら配信 [`serve`]、期限切れでも
//! バリデータ付きなら再検証用に保持) → オリジンへ転送 (プールの接続を再利用、必要なら条件付き)
//! → 応答本文を解読しつつクライアントへ自前の枠組みで配信し、同時にストリーミングで保存。
//! 304 なら保存済みの表現を延命して配信し、オリジン障害時は stale を配信する。
//!
//! 1 要求の処理 ([`handle_http_parsed`]) は段に割ってあり、段ごとにモジュールを持つ:
//!
//! - `admission`: 受け入れの判定 (純粋な計算。400 / 417 の理由と要求行の 3 要素)
//! - `lookup`: キャッシュ参照 (HIT / REFRESHING / COALESCED の配信、`only-if-cached`、入場制御と合流)
//! - `fetch`: オリジンへの転送と応答の先頭の読み (やり直し、1xx の中継、失敗時の stale)
//! - `reply`: 応答の先頭を見て決める (枠組み、再検証の 304、障害時の stale、502 の差し替え)
//! - `deliver`: 配信しつつ保存 (クライアント向けの先頭、本文の中継と splice、後始末とログ)
//!
//! 段は [`Stage`] (`Result<次の段へ渡すもの, Done>`) を返し、応答を書き終えたところで [`Done`] に
//! 戻り値を載せて抜ける。部品は `ctx` (1 要求の文脈とログ)、`upstream` (オリジンとの接続と要求の
//! 先頭)、`respond` (自前の応答)、`buffers` (中継バッファ)、`passthrough` (splice)、`serve` (保存からの
//! 配信)、`refresh` (裏の再検証)。

mod admission;
mod buffers;
mod ctx;
mod deliver;
mod fetch;
mod lookup;
#[cfg(target_os = "linux")]
mod passthrough;
mod refresh;
mod reply;
mod respond;
mod serve;
#[cfg(test)]
mod tests;
mod upstream;

pub use crate::request::{Origin, map_locations, parse_origin};
pub use crate::response::{ResponseHead, read_response_head};

use std::cell::Cell;
use std::io::{self, BufWriter};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::Upstream;
use crate::body::Framing;
use crate::cache::Cache;
use crate::clientio::ClientReader;
use crate::dns;
use crate::metrics::{BadRequestReason, Detail, Metrics, StageMs};
use crate::recent::ConnTally;
use crate::request::{RequestHeaders, parse_request_headers};
use crate::workers::Workers;
use crate::{log_debug, log_warn};
use proxy_base::minrate::GuardedWrite;

use admission::Rejection;
use buffers::CLIENT_WRITE_BUF;
use ctx::Ctx;
use respond::{write_error, write_max_forwards_zero};

/// 接続をまたいで共有する状態。
pub struct Shared<'a> {
    pub timeout: Duration,
    /// オリジンへの TCP の確立だけに当てる締め切り (`PROXY_CONNECT_TIMEOUT_SECS` の実効値。T28.8)。
    /// 応答の読み書きは今までどおり [`Shared::timeout`]
    pub connect_timeout: Duration,
    /// 転送中の最小の速さ (T29.34)。応答をクライアントへ書くのと要求の本文を読むのに
    /// 1 つずつ見張り ([`proxy_base::minrate::RateGuard`]) を作る
    pub min_transfer: &'a proxy_base::minrate::MinRate,
    pub keepalive: Duration,
    pub conn_id: usize,
    pub metrics: Arc<Metrics>,
    pub cache: Arc<Cache>,
    /// 接続プールと TLS クライアント
    pub upstream: Arc<Upstream>,
    /// 接続スレッドの置き場 (裏側の再検証をこの上限の内側で走らせる。T11.3)。
    /// **借りるだけ**にしてあるのは、この構造体を要求ごとに組むため
    /// (`Arc<Workers>` を持たせると要求あたり `Arc::clone` が 1 回増える)
    pub workers: &'a Arc<Workers>,
    /// ACL の判定 (`acl::resolve_target`) が引いた答え。オリジンへ繋ぐときはこれを使い、
    /// 名前解決を 1 要求 1 回にする (T12.7)。判定を飛ばした経路では `None`
    pub resolved: Option<&'a dns::Resolved<'a>>,
    /// この接続で処理する**最後の要求**か (`Config::max_requests_per_conn` に当たった。T14.2)。
    ///
    /// 立っていたらクライアントが keep-alive を求めていても `keep_client` を倒すので、
    /// **応答に `Connection: close` が付いてから閉じる**。以前は 1,000 要求目の応答に
    /// `Connection: keep-alive` を付けたまま閉じていたので、その応答を読んだ直後に
    /// 次の要求を送ったクライアントが取りこぼしていた (T12.6 の小物)
    pub last: bool,
    /// ここまでに測った段階 (`queue`。T14.3 (1))。
    /// **本体クレートが測った値をそのまま運ぶだけ**で、ここでは時計を読まない
    pub stages: crate::metrics::StageMs,
    /// 要求行が届いた時刻 (`client_read` の起点)。終わりは**この関数の入口で既に
    /// 読んでいる `started`** を使うので、要求ごとの時計は 1 回も増えない
    pub read_started: Option<Instant>,
    /// 接続の個票 (`/recent`) に積み上げる箱 (T14.4)。`--lite` では `None`。
    ///
    /// **原子ではない**: 1 本の接続を同時に触るスレッドは 1 つだけなので、要求ごとの
    /// 積み上げは普通の変数で行い、**接続の終了で 1 回だけ**枠へ移す
    /// (要求ごとの原子操作を 1 つも増やさないため)
    pub tally: Option<&'a Cell<ConnTally>>,
    /// 追跡中の接続元か (`PROXY_TRACE_CLIENT`。T14.27)。
    ///
    /// 照合は accept で 1 回だけ済ませてあり、**要求ごとに残るのはこの旗 1 つ**。
    /// 立っていれば [`Ctx::log`] が要求行と応答の状態と段階の ms を `/trace` へ 1 行書く
    pub traced: bool,
}

/// クライアントへ書く口。応答ヘッダーと本文の先頭を 1 回の write でまとめて出す (別々に出すと
/// 1 セグメント増える)。どの経路でも最後に flush するので、次の要求の前にバッファは空になる。
type ClientWriter<'s> = BufWriter<GuardedWrite<'s>>;

/// 段の途中で応答を書き終えた印 (この要求はもう次の段へ進まない)。
///
/// 中身は [`handle_http_parsed`] の戻り値 (`Keep` / `Close` はクライアント接続を次の要求に
/// 使えるか、`Err` は書けなかった)。`io::Error` から作れるので、段の中では `?` がそのまま使える。
/// `io::Result<bool>` を包まずに 3 つの形の enum にしてある (T34.8 (d))。
enum Done {
    Keep,
    Close,
    Err(io::Error),
}

impl Done {
    #[inline]
    fn of(result: io::Result<bool>) -> Done {
        match result {
            Ok(true) => Done::Keep,
            Ok(false) => Done::Close,
            Err(e) => Done::Err(e),
        }
    }

    #[inline]
    fn into_result(self) -> io::Result<bool> {
        match self {
            Done::Keep => Ok(true),
            Done::Close => Ok(false),
            Done::Err(e) => Err(e),
        }
    }
}

impl From<io::Error> for Done {
    #[inline]
    fn from(e: io::Error) -> Done {
        Done::Err(e)
    }
}

/// 段の戻り値: 次の段へ渡すもの、または [`Done`]。
type Stage<T> = Result<T, Done>;

/// 応答を書き終えた (`keep` はクライアント接続を次の要求に使えるか)。
fn done<T>(keep: bool) -> Stage<T> {
    Err(if keep { Done::Keep } else { Done::Close })
}

/// 配信の結果 (`serve_cached` などの戻り値) をそのまま [`handle_http_parsed`] の戻り値にする。
fn finished<T>(result: io::Result<bool>) -> Stage<T> {
    Err(Done::of(result))
}

/// 1 要求ぶんの文脈。入口で決まったこと (要求行・枠組み・要求先) と、段をまたいで使う参照を
/// まとめて持つ。読む口と書く口 (`reader` / `client`) は借用の都合で段の引数として別に渡す。
///
/// 要求行のメソッド・正規化 URL・プールの鍵・接続番号・`metrics`・`req`・`head_only` は [`Ctx`] にあるので
/// ここには重ねて持たない (`x.ctx.url` のように読む。組み立ての写しを減らす。T34.8)。
struct Exchange<'a> {
    shared: &'a Shared<'a>,
    cache: &'a Cache,
    client_ip: Option<&'a str>,
    /// 転送するヘッダー (`Max-Forwards` を 1 減らしたときは差し替えた写し)
    raw_headers: &'a [String],
    http11: bool,
    is_get: bool,
    /// 要求の本文の枠組み
    req_framing: Framing,
    origin: Origin,
    /// 接続先 (正規化 URL・プールの鍵と同じ 1 本の `String` から借りる)
    server_addr: &'a str,
    ctx: Ctx<'a>,
}

/// 1 リクエストを処理する。戻り値はクライアント接続を次の要求に使えるか。
pub fn handle_http_with_headers(
    client: &TcpStream,
    client_ip: Option<&str>,
    request_line: &str,
    raw_headers: &[String],
    reader: &mut ClientReader<'_>,
    shared: &Shared<'_>,
) -> io::Result<bool> {
    let req = parse_request_headers(raw_headers, shared.conn_id);
    handle_http_parsed(
        client,
        client_ip,
        request_line,
        raw_headers,
        &req,
        reader,
        shared,
    )
}

/// [`handle_http_with_headers`] の、ヘッダーを読み終えた (`parse_request_headers` 済みの) 版。
/// 受け付けの層はヘッダーを 1 回だけなめてここへ渡す (T28.15)。
pub fn handle_http_parsed(
    client: &TcpStream,
    client_ip: Option<&str>,
    request_line: &str,
    raw_headers: &[String],
    req: &RequestHeaders,
    reader: &mut ClientReader<'_>,
    shared: &Shared<'_>,
) -> io::Result<bool> {
    match forward(
        client,
        client_ip,
        request_line,
        raw_headers,
        req,
        reader,
        shared,
    ) {
        Ok(keep) => Ok(keep),
        Err(done) => done.into_result(),
    }
}

/// 要求 1 つを段の順に通す: 受け入れ → 要求先 → キャッシュ参照 → オリジンへ転送 → 応答の先頭 → 配信。
#[inline]
fn forward(
    client: &TcpStream,
    client_ip: Option<&str>,
    request_line: &str,
    raw_headers: &[String],
    req: &RequestHeaders,
    reader: &mut ClientReader<'_>,
    shared: &Shared<'_>,
) -> Stage<bool> {
    let started = Instant::now();
    let conn_id = shared.conn_id;
    // 読まないクライアントへの書きを見張る (T29.34)。測るのは大きな書きだけなので
    // (`GuardedWrite`)、1 回で書き終わる小さな応答には時計を 1 度も足さない
    let client = &mut BufWriter::with_capacity(
        CLIENT_WRITE_BUF,
        GuardedWrite::new(client, shared.min_transfer, conn_id),
    );
    let cache: &Cache = &shared.cache;
    let metrics: &Metrics = &shared.metrics;

    // ---- 受け入れの判定 (理由と順は `admission`) ----
    let line = match admission::check(req, request_line) {
        Ok(line) => line,
        Err(rej) => {
            return Err(refuse(
                client,
                metrics,
                client_ip,
                conn_id,
                request_line,
                &rej,
            ));
        }
    };
    let (method, target, version, http11) = (line.method, line.target, line.version, line.http11);
    let keep_client = admission::keep_client(req, http11, shared.last, shared.keepalive.is_zero());
    let req_framing = admission::request_framing(req);
    let send_continue = admission::wants_continue(req, http11, req_framing);
    // メソッドは大小を区別する (RFC 9110 §9.1。T32.8)。`get` を GET として扱うと、オリジンへは
    // `get` のまま届いて 501・405 が返り、それが GET の鍵に保存されて他の利用者の `GET` に配られていた
    // (キャッシュの毒)。小文字は未知のメソッドとして素通しし、保存も参照もしない
    let is_get = method == "GET";
    let head_only = method == "HEAD";
    // `Max-Forwards` (RFC 9110 §7.6.2。TRACE と OPTIONS だけ): 0 なら転送せずに自分で応え、
    // そうでなければ 1 減らして転送する (T27.8)。今まではどちらもそのまま転送していた。
    // 値が読めないときは今までどおり触らない。**ほかのメソッドには 1 命令も足さない**
    let trace = method == "TRACE";
    let mut decremented: Option<Vec<String>> = None;
    if trace || method == "OPTIONS" {
        match admission::max_forwards(raw_headers) {
            Some((_, Some(0))) => {
                log_debug!(
                    Some(conn_id),
                    "Max-Forwards: 0 on {}, answering here",
                    method
                );
                let _ = write_max_forwards_zero(client, trace, request_line, raw_headers);
                return done(false);
            }
            Some((i, Some(n))) => {
                decremented = Some(admission::decrement_max_forwards(raw_headers, i, n));
            }
            _ => {}
        }
    }
    let raw_headers: &[String] = decremented.as_deref().unwrap_or(raw_headers);

    // ---- 要求先 ----
    let origin = match parse_origin(target, req.host.as_deref()) {
        // 空のパスの `OPTIONS` (`OPTIONS http://h:P`) はサーバー全体への問い合わせなので、オリジンへは
        // `OPTIONS *` を送る (RFC 9112 §3.2.4。T29.9)。今までは `/` を補って `OPTIONS /` にしていた。
        // ほかのメソッドには文字列の比較 1 回だけ
        Ok(mut o) => {
            if method == "OPTIONS" && crate::request::has_empty_path(target) {
                o.path = "*".to_string();
            }
            o
        }
        Err(e) => {
            // `Host` が無い (`no_host`) のか絶対 URI が壊れている (`bad_uri`) のか (T14.28)
            if let Some(more) = metrics.record_bad_request(
                BadRequestReason::of_target(method, target),
                client_ip.unwrap_or("-"),
                400,
            ) {
                log_warn!(Some(conn_id), "400 Bad Request: {}{}", e, more);
            }
            let _ = write_error(client, 400, "Bad Request");
            return done(false);
        }
    };
    // URL・プールキー・接続先は 1 本の String から借りる (要求ごとの組み立てを 1 回に)
    let located = origin.locate();
    let (server_addr, url, pool_key) = (located.server_addr(), located.url(), located.pool_key());
    log_debug!(Some(conn_id), "start {} {} {}", method, url, version);
    let ctx = Ctx {
        client_ip: client_ip.unwrap_or("-"),
        method,
        url,
        version,
        started,
        conn_id,
        metrics,
        req,
        keep_client,
        head_only,
        mapped: origin.mapped,
        pool_key,
        // `queue` は本体クレートが測った値、`client_read` は「要求行が届いてから
        // `started` まで」(入口で読んだ時計をそのまま使う。T14.3 (1))
        detail: Detail {
            stages: StageMs {
                client_read: shared.read_started.map_or(0, |t| {
                    crate::profile::ms_u32(started.saturating_duration_since(t))
                }),
                ..shared.stages
            },
            ..Detail::default()
        },
        up_bytes: 0,
        tally: shared.tally,
        traced: shared.traced,
    };
    let mut x = Exchange {
        shared,
        cache,
        client_ip,
        raw_headers,
        http11,
        is_get,
        req_framing,
        origin,
        server_addr,
        ctx,
    };

    // ---- キャッシュ参照 ----
    let mut cs = lookup::CacheState::new(cache.config());
    lookup::consult(&mut x, client, reader, &mut cs)?;
    // ---- オリジンへ転送 ----
    let mut fetched = fetch::fetch(&mut x, client, reader, &mut cs, send_continue)?;
    // ---- 応答の先頭を見て決める ----
    let facts = reply::examine_head(&mut x, client, &mut cs, &mut fetched)?;
    // ---- 配信しつつ保存 ----
    deliver::deliver(&mut x, client, &mut cs, fetched, facts)
}

/// 受け入れの判定で断った要求に、理由を数えて (WARN は理由ごとに 1 分に 1 行。T28.9) 400 / 417 を書く。
///
/// 理由別に 1 件数えて個票にも残す (T14.28)。**個票に要求行そのものは入れない**。
fn refuse(
    client: &mut ClientWriter<'_>,
    metrics: &Metrics,
    client_ip: Option<&str>,
    conn_id: usize,
    request_line: &str,
    rej: &Rejection<'_>,
) -> Done {
    let (status, reason) = rej.status();
    let more = match rej.reason(request_line) {
        Some(why) => metrics.record_bad_request(why, client_ip.unwrap_or("-"), status),
        None => {
            // `rejected_requests` の理由には無いが、これも本数は相手が決めるので 1 分に 1 行 (T28.9)
            static EXPECT_WARN: crate::log::Throttle = crate::log::Throttle::new();
            EXPECT_WARN.pass(crate::log::WARN_EVERY_SECS)
        }
    };
    if let Some(more) = more {
        match rej {
            Rejection::ObsFold => log_warn!(
                Some(conn_id),
                "400 Bad Request: obsolete line folding in request headers{}",
                more
            ),
            Rejection::BadHeader => log_warn!(
                Some(conn_id),
                "400 Bad Request: malformed request header (name not a token, no colon, or CR/LF/NUL in the value){}",
                more
            ),
            Rejection::RequestLine => log_warn!(
                Some(conn_id),
                "malformed request line: {}{}",
                crate::log::Clip(request_line.trim()),
                more
            ),
            Rejection::ContentLength => log_warn!(
                Some(conn_id),
                "400 Bad Request: invalid or conflicting Content-Length{}",
                more
            ),
            Rejection::TransferEncoding {
                te_invalid,
                version,
            } => log_warn!(
                Some(conn_id),
                "400 Bad Request: Transfer-Encoding {} ({}){}",
                if *te_invalid {
                    "does not end in a single chunked"
                } else {
                    "in an HTTP/1.0 request"
                },
                version,
                more
            ),
            Rejection::DupHost => log_warn!(
                Some(conn_id),
                "400 Bad Request: duplicate Host header{}",
                more
            ),
            Rejection::ExpectUnknown => log_warn!(
                Some(conn_id),
                "417 Expectation Failed: unsupported Expect in the request{}",
                more
            ),
            Rejection::ChunkedAndLength => log_warn!(
                Some(conn_id),
                "400 Bad Request: both Content-Length and Transfer-Encoding: chunked{}",
                more
            ),
            // server の要求行の判定 (T34.142) と同じ文面 (T36.15)
            Rejection::Method(method) => log_warn!(
                Some(conn_id),
                "400 Bad Request: method is not a token: {}{}",
                crate::log::Clip(method),
                more
            ),
        }
    }
    let _ = write_error(client, status, reason);
    Done::Close
}
