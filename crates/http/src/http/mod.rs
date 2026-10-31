//! HTTP (平文) リクエストの転送とキャッシュの適用。
//!
//! 流れ: リクエスト解析 ([`request`]) → キャッシュ参照 (新鮮なら配信 [`serve`]、期限切れでも
//! バリデータ付きなら再検証用に保持) → オリジンへ転送 (プールの接続を再利用、必要なら条件付き)
//! → 応答本文を解読しつつクライアントへ自前の枠組みで配信し、同時にストリーミングで保存。
//! 304 なら保存済みの表現を延命して配信し、オリジン障害時は stale を配信する。

mod refresh;
mod serve;
#[cfg(test)]
mod tests;

pub use crate::request::{Origin, map_locations, parse_origin};
pub use crate::response::{ResponseHead, read_response_head};

use std::borrow::Cow;
use std::cell::Cell;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::Upstream;

use crate::body::{self, BodyReader, Framing};
use crate::cache::{
    Cache, CacheKey, CacheSource, CachedResponse, FetchOutcome, FetchTicket, cache_key_variant,
    now_epoch,
};
use crate::dns;
use crate::freshness;
use crate::headers;
use crate::log::{Access, access};
use crate::metrics::{BadRequestReason, Detail, ErrCause, Failure, HostOutcome, Metrics, StageMs};
use crate::origin::{self, OriginStream};
use crate::recent::{
    ConnTally, STAGE_CLIENT_READ, STAGE_CONNECT, STAGE_DNS, STAGE_FIRST_BYTE, STAGE_QUEUE, STAGES,
};
use crate::sync::LockExt;
use crate::workers::Workers;
use crate::{log_debug, log_trace, log_warn};
use proxy_base::minrate::{GuardedWrite, Side};

use crate::request::{RequestHeaders, parse_request_headers};
use serve::{can_serve_stale, serve_cached};

/// 本文を運ぶときの 1 回ぶんの大きさ。
///
/// 64 KiB より 32 KiB の方が速い (実測、大コア固定・3 回とも): 本文 64 KiB で
/// CPU/要求 93.5 → 85.9 us (-8%)、本文 1 MiB で 750.6 → 666.7 us (-11%)、
/// 主力の 1 KiB 経路は 47.6 → 46.5 us で退行なし、アイドル接続あたりのメモリも同じ。
/// キャッシュ階層に収まりやすいためと思われる。
pub(super) const COPY_BUF_SIZE: usize = 32 * 1024;

/// クライアントへ書くときのバッファ。**必ず [`COPY_BUF_SIZE`] より大きくすること。**
/// 同じ大きさだと std の BufWriter が「容量以上の write はバッファを空けてから直書き」
/// する経路に入り、ヘッダーだけが単独の sendto になる (実測: 本文 1 MiB で sendto が
/// 9 → 42.7 回/要求)。
pub(super) const CLIENT_WRITE_BUF: usize = 2 * COPY_BUF_SIZE;

/// オリジンから読むときのバッファ。**必ず [`COPY_BUF_SIZE`] 以下にすること。**
/// これより大きいと body::BodyReader の「要求が容量以上なら内部バッファを迂回して直読み」
/// が効かず、本文をまるごと余計に memcpy する (実測: 本文 1 MiB で user CPU +60%)。
pub(super) const ORIGIN_READ_BUF: usize = COPY_BUF_SIZE;

// 上の 2 つの不変条件をコンパイル時に固定する (定数を片方だけ触ったときに気付けるように)
const _: () = assert!(CLIENT_WRITE_BUF > COPY_BUF_SIZE);
const _: () = assert!(ORIGIN_READ_BUF <= COPY_BUF_SIZE);

/// 本文の中継バッファのプロセス共用プール。
///
/// スレッドローカルに置くと、アイドルな keep-alive 接続が「使っていない 64 KiB」を
/// スレッドごとに抱え続ける (実測: アイドル 1 接続あたり 109.6 kB のうち約 67 kB がこれ)。
/// 借りている間だけ持つ形にすると、同時に本文を運んでいる数ぶんしか要らない。
static COPY_POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// プールに置いておく上限。これを超えたぶんは返さずに解放する
/// (同時に本文を運ぶ数がこれを超えるのは一時的な山なので、抱え続けない)。
const COPY_POOL_MAX: usize = 64;

/// スレッドから借りた中継バッファ。Drop で返すので途中で return しても失わない。
pub(super) struct CopyBuf(Vec<u8>);

impl CopyBuf {
    pub(super) fn take() -> CopyBuf {
        let mut buf = COPY_POOL.locked().pop().unwrap_or_default();
        if buf.len() < COPY_BUF_SIZE {
            buf.resize(COPY_BUF_SIZE, 0);
        }
        CopyBuf(buf)
    }
}

impl std::ops::Deref for CopyBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl std::ops::DerefMut for CopyBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

impl Drop for CopyBuf {
    fn drop(&mut self) {
        let mut pool = COPY_POOL.locked();
        if pool.len() < COPY_POOL_MAX {
            pool.push(std::mem::take(&mut self.0));
        }
    }
}
/// Content-Length のある応答本文を `splice(2)` でオリジンからクライアントへ素通しする。
///
/// 本文を書き換えず、キャッシュにも保存しない応答なら、ユーザー空間に持ち上げる必要がない。
/// CONNECT トンネルで既に使っている仕掛けと同じ (カーネル内でパイプを経由して移す)。
#[cfg(target_os = "linux")]
pub(super) mod passthrough {
    use std::io;
    use std::os::fd::RawFd;
    use std::sync::Mutex;

    use crate::sync::LockExt;
    use crate::sys::{self, Pipe};

    /// パイプの容量 = 1 回の splice で動かす上限。
    ///
    /// 1 MiB にすると `fs.pipe-user-pages-soft` (既定 16384 ページ = 64 MiB) に当たって
    /// 64 本しか作れず、それ以降は黙って既定の 64 KiB に落ちる。256 KiB なら 256 本まで
    /// 同じ容量が行き渡る。
    const PIPE_CAPACITY: usize = 256 * 1024;
    /// 使い回すために置いておくパイプの上限 (1 本あたりカーネル側のメモリを持つ)。
    const POOL_MAX: usize = 32;

    static POOL: Mutex<Vec<Pipe>> = Mutex::new(Vec::new());

    /// プールから借りたパイプ。Drop で返す。
    struct Borrowed(Option<Pipe>);

    impl Borrowed {
        fn take() -> io::Result<Borrowed> {
            if let Some(p) = POOL.locked().pop() {
                return Ok(Borrowed(Some(p)));
            }
            let p = Pipe::new_blocking()?;
            p.set_capacity(PIPE_CAPACITY as i32);
            Ok(Borrowed(Some(p)))
        }
    }

    impl Drop for Borrowed {
        fn drop(&mut self) {
            if let Some(p) = self.0.take() {
                let mut pool = POOL.locked();
                if pool.len() < POOL_MAX {
                    pool.push(p);
                }
            }
        }
    }

    /// `remaining` バイトを運ぶ。戻り値は (運んだバイト数, 最後まで届いたか)。
    ///
    /// 途中で失敗したらパイプに残ったバイトは失われるので、呼び出し側はこの接続を
    /// プールに戻してはいけない (`Err` を返した場合)。
    ///
    /// `guard` が切る設定のとき、`to` (クライアント) は**呼ぶ側が待たない形にしてある**
    /// (T.29.34)。待つソケットへの splice は 1 回の呼び出しの中でページごとに締め切りを
    /// 待ち直すので、読まない相手には 1 回で 20 秒以上止まり、見張りが判定できない
    /// (実測 22 秒で 95 KB)。待たない splice が `EAGAIN` なら `poll` で書けるのを待ち、
    /// 待った時間を見張りに積む。`poll` は窓の残りより長く眠らず、無通信の締め切り
    /// `timeout` (`0` は無期限) を書けないまま越えたら `WouldBlock` で返す (前と同じ扱い)。
    pub fn relay(
        from: RawFd,
        to: RawFd,
        remaining: u64,
        guard: &mut proxy_base::minrate::RateGuard,
        timeout: std::time::Duration,
    ) -> io::Result<(u64, bool)> {
        let borrowed = Borrowed::take()?;
        let pipe = borrowed.0.as_ref().expect("just taken");
        let mut left = remaining;
        let mut moved = 0u64;
        while left > 0 {
            let want = left.min(PIPE_CAPACITY as u64) as usize;
            let n = sys::splice_block(from, pipe.write_fd, want)?;
            if n == 0 {
                // オリジンが Content-Length より早く閉じた
                return Ok((moved, false));
            }
            let mut out = n;
            while out > 0 {
                let w = if guard.is_on() {
                    to_client(pipe.read_fd, to, out, guard, timeout)?
                } else {
                    sys::splice_block(pipe.read_fd, to, out)?
                };
                if w == 0 {
                    return Ok((moved, false));
                }
                out -= w;
                moved += w as u64;
                left -= w as u64;
            }
        }
        Ok((moved, true))
    }

    /// 待たないクライアントへ 1 回ぶん運ぶ (書けなければ `poll` で待つ。[`relay`] を見よ)。
    fn to_client(
        pipe: RawFd,
        to: RawFd,
        len: usize,
        guard: &mut proxy_base::minrate::RateGuard,
        timeout: std::time::Duration,
    ) -> io::Result<usize> {
        let mut stuck = std::time::Duration::ZERO;
        loop {
            match sys::splice_block(pipe, to, len) {
                Ok(w) => {
                    guard.note(std::time::Duration::ZERO, w as u64)?;
                    return Ok(w);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
            if !timeout.is_zero() && stuck >= timeout {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            let mut wait = guard.left().unwrap_or(timeout);
            if !timeout.is_zero() {
                wait = wait.min(timeout - stuck);
            }
            let ms = wait.as_millis().clamp(1, i32::MAX as u128) as i32;
            let mut fds = [sys::PollFd::new(to, sys::POLLOUT)];
            let t0 = std::time::Instant::now();
            let ready = sys::poll_fds(&mut fds, ms)?;
            let waited = t0.elapsed();
            guard.note(waited, 0)?;
            if ready == 0 {
                stuck += waited;
            }
        }
    }
}

/// この大きさ以上の本文だけ [`passthrough`] に回す。これより小さいと、応答ヘッダーを
/// 読んだ時点でだいたい手元のバッファに入っているので、splice の往復のぶんかえって遅い
/// (実測: 64 KiB では約 171 → 179 us/要求)。
#[cfg(target_os = "linux")]
const SPLICE_MIN_BYTES: u64 = 128 * 1024;

/// オリジンがこのステータスを返したら stale を配信する (RFC 5861 stale-if-error 相当)。
const STALE_ON_STATUS: &[u16] = &[500, 502, 503, 504];

/// 接続をまたいで共有する状態。
pub struct Shared<'a> {
    pub timeout: Duration,
    /// オリジンへの TCP の確立だけに当てる締め切り (`PROXY_CONNECT_TIMEOUT_SECS` の実効値。T.28.8)。
    /// 応答の読み書きは今までどおり [`Shared::timeout`]
    pub connect_timeout: Duration,
    /// 転送中の最小の速さ (T.29.34)。応答をクライアントへ書くのと要求の本文を読むのに
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

/// アクセスログと配信に必要なリクエストの文脈。
struct Ctx<'a> {
    client_ip: &'a str,
    method: &'a str,
    url: &'a str,
    version: &'a str,
    started: Instant,
    conn_id: usize,
    metrics: &'a Metrics,
    req: &'a RequestHeaders,
    /// 応答後もクライアント接続を維持できるか
    keep_client: bool,
    head_only: bool,
    /// マッピング形式の要求 (Location を書き換える)
    mapped: bool,
    /// ホスト別統計のキー (`scheme://host:port`)
    pool_key: &'a str,
    /// 1 要求の内訳 (名前解決 / 接続 / 勝った族 / エラーの原因。T12.4 (2))。
    /// オリジンへ行った経路だけが埋める (キャッシュ HIT では全部 0)
    detail: Detail,
    /// クライアントから受けたバイト (要求のヘッダーと本文)。接続の個票の「上り」(T14.4)
    up_bytes: u64,
    /// 接続の個票に積み上げる箱 (T14.4)。`--lite` では `None`
    tally: Option<&'a Cell<ConnTally>>,
    /// 追跡中の接続元か (`PROXY_TRACE_CLIENT`。T14.27)。立っていなければ分岐 1 回で飛ばす
    traced: bool,
}

impl Ctx<'_> {
    fn log(&self, status: u16, bytes: u64, cache: &str) {
        let outcome = HostOutcome::from_access(cache, status);
        // 経過時間は 1 回だけ引く (clock_gettime は要求ごとに効いてくる)
        let took = self.started.elapsed();
        // 本文を流した時間 = 全体 − 初バイトまで (T14.3 (1))。**時計は足さない**
        // (ここまでに読んだ 2 つの差で出す。キャッシュ HIT は初バイトが無いので全部が本文)
        let mut detail = self.detail;
        if crate::profile::on() {
            let total = took.as_millis().min(u64::MAX as u128) as u64;
            detail.stages.body = total.saturating_sub(detail.first_byte_ms.unwrap_or(0)) as u32;
        }
        // 接続の個票 (`/recent`) の積み上げ (T14.4)。箱の中の足し算だけで、原子も鍵も無い。
        // **段階は T14.3 の `StageMs` からも入れる** (`first_relay` は CONNECT だけなので 0)
        if let Some(cell) = self.tally {
            let mut t = cell.get();
            let mut stage_ms = [0u64; STAGES];
            stage_ms[STAGE_DNS] = detail.dns_ms;
            stage_ms[STAGE_CONNECT] = detail.connect_ms;
            stage_ms[STAGE_FIRST_BYTE] = detail.first_byte_ms.unwrap_or(0);
            stage_ms[STAGE_QUEUE] = detail.stages.queue as u64;
            stage_ms[STAGE_CLIENT_READ] = detail.stages.client_read as u64;
            t.add_request(status, self.up_bytes, bytes, stage_ms, detail.syn_retrans);
            cell.set(t);
            // 接続元 1 つの追跡 (`/trace`。T14.27)。**旗が立っている接続だけ** 1 行書く。
            // 立っていない要求の費用はこの分岐 1 回だけで、段階の ms も所要時間も
            // すぐ上で既に組んだものをそのまま渡す (時計も確保も増やさない)。
            // ここは個票の決まり (パスを入れない) の**唯一の例外**で、追跡中の
            // 1 接続元に限り URL を先頭 256 B まで残す
            if self.traced {
                crate::trace::push(crate::trace::Line {
                    conn_id: self.conn_id,
                    client: self.client_ip,
                    method: self.method,
                    target: self.url,
                    version: self.version,
                    status,
                    took_ms: took.as_millis().min(u64::MAX as u128) as u64,
                    bytes,
                    stage_ms,
                    // 閉じた理由は接続 1 本のものなので CONNECT だけ (T14.4 の `/recent` にある)
                    reason: None,
                });
            }
        }
        // 向き別のバイト (T14.26)。**新しい計数はしていない**: 上りは要求本文
        // (個票が既に使っている `up_bytes`)、下りはこの応答のバイト (アクセスログに
        // 出すのと同じ値) をそのまま運ぶだけ。ホスト別統計の鍵の内側で足す
        detail.bytes_in = self.up_bytes;
        detail.bytes_out = bytes;
        self.metrics
            .record_host_detail(self.pool_key, outcome, bytes, Some(took), detail);
        self.metrics
            // 宛先はホスト別統計と同じ鍵 (`scheme://host:port`)。接続元の個票 (`/clients`) は
            // この鍵からホストとポートを取る (T14.7)
            .record_client(
                self.client_ip,
                outcome,
                bytes,
                (self.up_bytes, bytes),
                Some(took),
                Some(self.pool_key),
            );
        let mut digits = [0u8; 5];
        access(
            self.conn_id,
            &Access {
                client: self.client_ip,
                method: self.method,
                target: self.url,
                version: self.version,
                status: decimal(&mut digits, status as u64),
                bytes,
                duration_ms: took.as_secs_f64() * 1000.0,
                cache,
            },
        );
    }

    /// エラーを返したときのログと統計。個票のリング (`/errors`) にも 1 件残す (T13.4)。
    ///
    /// [`Ctx::log`] と分けてあるのは、**エラーの経路からしか個票を書かない**ため
    /// (成功の熱い経路には 1 命令も足さない)。
    fn log_failure(&self, status: u16) {
        self.metrics
            .record_error(false, self.pool_key, self.client_ip, status, &self.detail);
        self.log(status, 0, "ERROR");
    }

    fn connection_line(&self) -> &'static str {
        if self.keep_client {
            "Connection: keep-alive"
        } else {
            "Connection: close"
        }
    }
}

/// 10 進の数字を確保せずに書く (`to_string` / `format!` は `String` を 1 本作る)。
fn decimal(buf: &mut [u8; 5], mut v: u64) -> &str {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 || i == 0 {
            break;
        }
    }
    // 数字しか書いていないので UTF-8 として必ず妥当
    std::str::from_utf8(&buf[i..]).unwrap_or("0")
}

/// `Content-Length: N` を確保せずに組み立てる (`buf` は呼び出し側のスタック)。
fn content_length_line(buf: &mut [u8; 40], n: u64) -> &str {
    const NAME: &[u8] = b"Content-Length: ";
    buf[..NAME.len()].copy_from_slice(NAME);
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    let mut v = n;
    loop {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let end = NAME.len() + (digits.len() - i);
    buf[NAME.len()..end].copy_from_slice(&digits[i..]);
    std::str::from_utf8(&buf[..end]).unwrap_or("Content-Length: 0")
}

/// オリジンとの接続 (バッファ付き)。
type OriginConn = BufReader<OriginStream>;

/// 1 リクエストを処理する。戻り値はクライアント接続を次の要求に使えるか。
pub fn handle_http_with_headers(
    client: &TcpStream,
    client_ip: Option<&str>,
    request_line: &str,
    raw_headers: &[String],
    reader: &mut crate::clientio::ClientReader<'_>,
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
/// 受け付けの層はヘッダーを 1 回だけなめてここへ渡す (T.28.15)。
pub fn handle_http_parsed(
    client: &TcpStream,
    client_ip: Option<&str>,
    request_line: &str,
    raw_headers: &[String],
    req: &RequestHeaders,
    reader: &mut crate::clientio::ClientReader<'_>,
    shared: &Shared<'_>,
) -> io::Result<bool> {
    let started = Instant::now();
    // 応答ヘッダーと本文の先頭を 1 回の write でまとめて出す (別々に出すと 1 セグメント増える)。
    // どの経路でも最後に flush するので、次の要求の前にバッファは空になる
    let conn_id = shared.conn_id;
    // 読まないクライアントへの書きを見張る (T.29.34)。測るのは大きな書きだけなので
    // (`GuardedWrite`)、1 回で書き終わる小さな応答には時計を 1 度も足さない
    let client = &mut BufWriter::with_capacity(
        CLIENT_WRITE_BUF,
        GuardedWrite::new(client, shared.min_transfer, conn_id),
    );
    let cache: &Cache = &shared.cache;
    let metrics: &Metrics = &shared.metrics;
    // RFC 9112 §5.2: 折り返した (obs-fold) ヘッダーは、中継する側が 400 で断るか
    // SP 1 個に置き換えるかを選べる。置き換えると値が黙って変わるので**断る** (T27.2)。
    // 継続行を捨てていた今までは、オリジンに `X-A: 1` だけが届いていた
    if req.obs_fold {
        // WARN は理由ごとに 1 分に 1 行 (T.28.9。本数は `/status` の `rejected_requests`)
        if let Some(more) =
            metrics.record_bad_request(BadRequestReason::ObsFold, client_ip.unwrap_or("-"), 400)
        {
            log_warn!(
                Some(conn_id),
                "400 Bad Request: obsolete line folding in request headers{}",
                more
            );
        }
        let _ = write_error(client, 400, "Bad Request");
        return Ok(false);
    }

    // 名前とコロンの間の空白 (RFC 9112 §5.1) と値の CR / LF / NUL (RFC 9110 §5.5) も断る (T27.5)。
    // `Content-Length : 5` をこちらは長さとして使い、空白つきのまま転送していた。値の CR は
    // そのまま届き、CR を改行と読むオリジンには別のヘッダー (`Transfer-Encoding` …) に見えていた
    if req.bad_header {
        if let Some(more) =
            metrics.record_bad_request(BadRequestReason::BadHeader, client_ip.unwrap_or("-"), 400)
        {
            log_warn!(
                Some(conn_id),
                "400 Bad Request: malformed request header (name not a token, no colon, or CR/LF/NUL in the value){}",
                more
            );
        }
        let _ = write_error(client, 400, "Bad Request");
        return Ok(false);
    }

    let mut parts = request_line.split_ascii_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        // 理由別に 1 件数えて個票にも残す (T14.28)。**個票に要求行そのものは入れない**。
        // 本体クレートが同じ判定を先にしているのでここへは普通は来ないが、数える口は
        // 400 を返す場所ごとに置く
        if let Some(more) = metrics.record_bad_request(
            BadRequestReason::of_request_line(request_line),
            client_ip.unwrap_or("-"),
            400,
        ) {
            log_warn!(
                Some(conn_id),
                "malformed request line: {}{}",
                crate::log::Clip(request_line.trim()),
                more
            );
        }
        let _ = write_error(client, 400, "Bad Request");
        return Ok(false);
    };
    // `Content-Length` の形が壊れている要求は、本文の切れ目が決まらないので 400 で断る (T19.1)。
    // ここで断らないと、判定 (数字でない → 本文なし) と転送 (生の行をそのまま) が食い違い、
    // オリジンが本文を待ち続けるか、後段が別の長さで読む (要求の密輸)。**無言で閉じない**
    if req.content_length_invalid {
        if let Some(more) =
            metrics.record_bad_request(BadRequestReason::BodyFraming, client_ip.unwrap_or("-"), 400)
        {
            log_warn!(
                Some(conn_id),
                "400 Bad Request: invalid or conflicting Content-Length{}",
                more
            );
        }
        let _ = write_error(client, 400, "Bad Request");
        return Ok(false);
    }
    let version = parts.next().unwrap_or("HTTP/1.0");
    let http11 = version == "HTTP/1.1";
    // `Transfer-Encoding` の最後がちょうど 1 回の `chunked` でない要求 (RFC 9112 §6.3) と、
    // HTTP/1.0 の要求の `Transfer-Encoding` (§6.1 は「枠組みが壊れているとして扱う」) は 400 (T27.6)。
    // 今までは `xchunked`・`gzip` を本文なしとしてヘッダーだけ転送し、本文はクライアントの
    // 次の要求として読まれていた (`xchunked` と `Content-Length` が両方あると CL で読んで両方を転送した)
    if req.te_invalid || (req.te_present && !http11) {
        if let Some(more) =
            metrics.record_bad_request(BadRequestReason::BodyFraming, client_ip.unwrap_or("-"), 400)
        {
            log_warn!(
                Some(conn_id),
                "400 Bad Request: Transfer-Encoding {} ({}){}",
                if req.te_invalid {
                    "does not end in a single chunked"
                } else {
                    "in an HTTP/1.0 request"
                },
                version,
                more
            );
        }
        let _ = write_error(client, 400, "Bad Request");
        return Ok(false);
    }
    // `Host` が 2 本以上ある要求は RFC 9110 §7.2 が 400 を求めている (T19.2)。
    // どの `Host` を信じるかで前段と後段が食い違うと、宛先の付け替え (要求の密輸) になる。
    // **通る要求には 1 命令も足さない**: 印は 2 本目を見たときしか立たない
    if req.dup_host {
        if let Some(more) =
            metrics.record_bad_request(BadRequestReason::DupHost, client_ip.unwrap_or("-"), 400)
        {
            log_warn!(
                Some(conn_id),
                "400 Bad Request: duplicate Host header{}",
                more
            );
        }
        let _ = write_error(client, 400, "Bad Request");
        return Ok(false);
    }
    // `100-continue` 以外の `Expect` は満たせないので 417 (RFC 9110 §10.1.1。T.28.3)。
    // 本文が続くかもしれないので閉じる。印は `Expect` の行があるときしか立たない
    if req.expect_unknown {
        // `rejected_requests` の理由には無いが、これも本数は相手が決めるので 1 分に 1 行 (T.28.9)
        static EXPECT_WARN: crate::log::Throttle = crate::log::Throttle::new();
        if let Some(more) = EXPECT_WARN.pass(crate::log::WARN_EVERY_SECS) {
            log_warn!(
                Some(conn_id),
                "417 Expectation Failed: unsupported Expect in the request{}",
                more
            );
        }
        let _ = write_error(client, 417, "Expectation Failed");
        return Ok(false);
    }
    let keep_client = !shared.last
        && !shared.keepalive.is_zero()
        && if http11 {
            !req.connection_close
        } else {
            req.connection_keep_alive
        };
    let req_framing = if req.chunked {
        // `Content-Length` と `Transfer-Encoding: chunked` が両方ある要求は、前段と後段で
        // 本文の切れ目が食い違う (要求の密輸)。RFC 9112 §6.1 は「中継してはならない」と
        // 書いているので 400 で断る (T14.28)。**この比較が走るのは `chunked` が真のとき
        // だけ**なので、本文の無い要求と `Content-Length` だけの要求 (= 成功の経路) には
        // 分岐も比較も 1 つも増えない
        if req.content_length.is_some() {
            if let Some(more) = metrics.record_bad_request(
                BadRequestReason::BodyFraming,
                client_ip.unwrap_or("-"),
                400,
            ) {
                log_warn!(
                    Some(conn_id),
                    "400 Bad Request: both Content-Length and Transfer-Encoding: chunked{}",
                    more
                );
            }
            let _ = write_error(client, 400, "Bad Request");
            return Ok(false);
        }
        Framing::Chunked
    } else {
        // `Content-Length: 0` は本文が無いのと同じ (T.29.5)。`Length(0)` のままだと「本文つきの GET」として
        // キャッシュを素通り (`cache BYPASS`) していた。行そのものは今までどおりオリジンへ転送する
        // (`raw_headers` から書くので、ここで畳んでも消えない)
        match req.content_length {
            None | Some(0) => Framing::None,
            Some(n) => Framing::Length(n),
        }
    };
    // `Expect: 100-continue` の本文つきの要求には、本文を読む前に `100 Continue` を 1 回返す
    // (RFC 9110 §10.1.1。T.28.3)。今までは自分でも返さず、オリジンの 100 も中継しないので、
    // 100 を厳密に待つクライアントは締め切りまで本文を送らず、こちらは本文を待って止まっていた。
    // HTTP/1.0 の要求の 100-continue は無視する (§10.1.1)
    let mut send_continue = req.expect_continue
        && http11
        && matches!(req_framing, Framing::Chunked | Framing::Length(1..));
    // メソッドは大小を区別する (RFC 9110 §9.1。T.32.8)。`get` を GET として扱うと、オリジンへは
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
        let found = raw_headers.iter().enumerate().find_map(|(i, l)| {
            let (k, v) = proxy_base::ascii::split_once(l, b':')?;
            k.trim_ascii()
                .eq_ignore_ascii_case("max-forwards")
                .then(|| (i, v.trim_ascii().parse::<u64>().ok()))
        });
        match found {
            Some((_, Some(0))) => {
                log_debug!(
                    Some(conn_id),
                    "Max-Forwards: 0 on {}, answering here",
                    method
                );
                let _ = write_max_forwards_zero(client, trace, request_line, raw_headers);
                return Ok(false);
            }
            Some((i, Some(n))) => {
                let mut v = raw_headers.to_vec();
                v[i] = format!("Max-Forwards: {}\r\n", n - 1);
                decremented = Some(v);
            }
            _ => {}
        }
    }
    let raw_headers: &[String] = decremented.as_deref().unwrap_or(raw_headers);

    let origin = match parse_origin(target, req.host.as_deref()) {
        // 空のパスの `OPTIONS` (`OPTIONS http://h:P`) はサーバー全体への問い合わせなので、オリジンへは
        // `OPTIONS *` を送る (RFC 9112 §3.2.4。T.29.9)。今までは `/` を補って `OPTIONS /` にしていた。
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
            return Ok(false);
        }
    };
    // URL・プールキー・接続先は 1 本の String から借りる (要求ごとの組み立てを 1 回に)
    let located = origin.locate();
    let (server_addr, url, pool_key) = (located.server_addr(), located.url(), located.pool_key());
    log_debug!(Some(conn_id), "start {} {} {}", method, url, version);
    let mut ctx = Ctx {
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

    // ---- キャッシュ参照 ----
    let cfg = cache.config();
    // キャッシュ無効時は鍵の生成も鮮度判定もしない (素通しの経路を最短にする)
    let cache_on = cache.enabled();
    let (key, client_no_store, mut force_revalidate, only_if_cached, now) = if cache_on {
        let variant = freshness::accept_encoding_variant(req.accept_encoding.as_deref());
        (
            cache_key_variant("GET", url, &variant),
            freshness::has_directive(&req.cache_control, "no-store"),
            freshness::has_directive(&req.cache_control, "no-cache")
                || freshness::directive_value(&req.cache_control, "max-age") == Some(0),
            // `only-if-cached` (RFC 9111 §5.2.1.7。T.28.10 (c)): 保存から返せなければ 504
            freshness::has_directive(&req.cache_control, "only-if-cached"),
            now_epoch(),
        )
    } else {
        (CacheKey(0), false, false, false, 0)
    };
    // 本文付きの GET は本文でも意味が変わりうるのでキャッシュしない
    let lookup_allowed = cache_on
        && (is_get || head_only)
        && !req.authorization
        && !client_no_store
        && req_framing == Framing::None;
    let store_allowed = lookup_allowed && is_get && req.range.is_none();
    let client_conditional = req.if_none_match.is_some() || req.if_modified_since.is_some();

    let mut stale: Option<(CachedResponse, CacheSource)> = None;
    // クライアント自身の条件付き要求のときの、期限切れの保存済みの表現 (T.29.6)
    let mut client_cond_entry: Option<CachedResponse> = None;
    if lookup_allowed {
        if let Some((entry, source)) = cache.get(key, conn_id) {
            cache.remember_variant(url, key);
            // 要求の `max-age=N` (N > 0) と `min-fresh=N` (RFC 9111 §5.2.1.1・§5.2.1.3。T.28.10 (c)):
            // 保存から N 秒より古い・期限まで N 秒ない表現は、そのままでは返さず再検証する
            // (`max-age=0` と同じ扱い)。今までは見ずに HIT にしていた。`Cache-Control` の無い要求は
            // 空文字の判定 1 回だけ
            if !force_revalidate
                && !req.cache_control.is_empty()
                && client_wants_fresher(&req.cache_control, &entry, now)
            {
                force_revalidate = true;
            }
            if entry.is_fresh(now) && !force_revalidate {
                body::drain(reader, req_framing)?;
                let ttl_left = entry.ttl_left(now);
                return serve_cached(client, entry, source, "HIT", ttl_left, &ctx);
            }
            // クライアント自身の条件付き要求は、そのままオリジンに判断させる
            if !client_conditional {
                // 期限切れ直後 (grace 内) なら、すぐ返して裏で再検証する (stale-while-revalidate)。
                // 既定の grace は新鮮だった期間のある表現だけに使い、max-age=0 (毎回再検証) の表現は
                // オリジンが stale-while-revalidate を明示したときだけ対象にする
                let head = freshness::parse_cached_head(&entry.head);
                let default_grace = if entry.meta.expires_at > entry.meta.stored_at {
                    cfg.grace.as_secs()
                } else {
                    0
                };
                let grace = default_grace.max(
                    head.cache_control
                        .as_deref()
                        .and_then(|cc| freshness::directive_value(cc, "stale-while-revalidate"))
                        .unwrap_or(0),
                );
                let stale_for = now.saturating_sub(entry.meta.expires_at);
                if !force_revalidate
                    && grace > 0
                    && entry.meta.validators
                    && head.may_serve_stale()
                    && stale_for <= grace
                    && refresh::spawn(
                        shared,
                        &origin,
                        key,
                        url,
                        entry.head.clone(),
                        req.accept_encoding.clone(),
                    )
                {
                    body::drain(reader, req_framing)?;
                    cache.stale_served.fetch_add(1, Ordering::Relaxed);
                    return serve_cached(client, entry, source, "REFRESHING", 0, &ctx);
                }
                stale = Some((entry, source));
            } else if entry.meta.validators {
                // オリジンの 304 で保存済みの表現も延ばせるように持っておく (T.29.6)。
                // `stale` には入れない (条件はクライアントのものをそのまま送り、stale も配らない)
                client_cond_entry = Some(entry);
            }
        }
    } else if cache_on {
        log_debug!(
            Some(conn_id),
            "cache BYPASS (method={} auth={} cc='{}')",
            method,
            req.authorization,
            req.cache_control
        );
    }

    // `only-if-cached` で保存から返せなかった (無い・古い・保存できない要求): オリジンへは行かずに 504
    // (RFC 9111 §5.2.1.7。T.28.10 (c))。今まではオリジンへ行っていた
    if only_if_cached {
        body::drain(reader, req_framing)?;
        log_debug!(
            Some(conn_id),
            "only-if-cached and no usable stored response: 504"
        );
        write_error(client, 504, "Gateway Timeout")?;
        ctx.log(504, 0, "MISS");
        return Ok(false);
    }

    // 保存されないと分かっている鍵 (T11.9) は 1 回だけ見て、入場制御と合流の両方に使う (T.28.13 (c))。
    // その鍵は前に見ているので、入場制御に通しても「2 回目」で通る (門番の表を引かずに済ませる)
    let known_unstored = store_allowed && stale.is_none() && !cache.may_coalesce(key, now);

    // ---- 入場制御: 層が埋まっているとき、初めて見た URL は保存しない (2 回目から) ----
    let store_allowed = store_allowed && (stale.is_some() || known_unstored || cache.admit(key));

    // ---- 同時ミスの合流: 同じキーを誰かが取得中なら、その保存完了を待ってキャッシュから返す ----
    // **保存されないと分かっている URL では合流しない** (T11.9)。待っても保存されないので、
    // 起きてから結局自分でオリジンへ行くことになり、待ち損 (`futex` 1.76 回/要求) だけが残る。
    // 初めて見る URL では今までどおり合流する
    let mut leader = None;
    if store_allowed && stale.is_none() && !known_unstored {
        match cache.begin_fetch(key) {
            FetchTicket::Leader(guard) => leader = Some(guard),
            FetchTicket::Follower(inflight) => {
                log_debug!(
                    Some(conn_id),
                    "cache WAIT: another request is fetching key={}",
                    key
                );
                // `shared.timeout` が 0 (無期限) のときはここだけ「待たない」意味に
                // なるが、待たなければ自分で取りに行くだけなので安全側。
                // 先頭のスレッドを無期限に待つ形にはしない (相手が固まると連鎖する)
                if inflight.wait(shared.timeout) == Some(FetchOutcome::Stored)
                    && let Some((entry, source)) = cache.get(key, conn_id)
                    && entry.is_fresh(now_epoch())
                {
                    body::drain(reader, req_framing)?;
                    cache.coalesced.fetch_add(1, Ordering::Relaxed);
                    let ttl_left = entry.ttl_left(now_epoch());
                    return serve_cached(client, entry, source, "COALESCED", ttl_left, &ctx);
                }
                // 保存されなかった・間に合わなかった: 自分で取りに行く
            }
        }
    }

    // ---- オリジンへ転送 ----
    let conditional_lines = stale
        .as_ref()
        .map(|(entry, _)| {
            freshness::conditional_headers(&freshness::parse_cached_head(&entry.head))
        })
        .unwrap_or_default();
    // 転送するヘッダーは Vec<String> を経由せず、そのままバイト列に書く
    let mut request_head =
        build_request_head(method, &origin, raw_headers, client_ip, &conditional_lines);
    log_trace!(
        Some(conn_id),
        "origin request head:\n{}",
        String::from_utf8_lossy(&request_head).trim_end()
    );
    // 期限切れの表現が手元にあるなら、オリジンを長く待たずに stale を返す
    // (`shared.timeout` の 0 は「無期限」なので、素の `min` だと 0 秒で諦める方に
    // 化ける。`timeout::shorter` は無期限の側を必ず負けさせる。T10.6)
    let mut origin_timeout = if stale.is_some() {
        proxy_base::timeout::shorter(shared.timeout, cfg.stale_wait)
    } else {
        shared.timeout
    };

    // 本文の無い冪等な要求だけ、再利用した接続が死んでいたときに 1 回やり直す
    let retryable = req_framing == Framing::None && (is_get || head_only);
    // 1xx を中継したか。中継したらクライアントはもう応答の先頭を受け取っているので、
    // プールの接続を掴み直して同じ 1xx をもう一度出さない (やり直さない)
    let mut interim_sent = false;
    // オリジンが要求の本文を読み切らずに閉じたか (T.29.7)。立っていれば応答は渡すが、
    // クライアントの本文の残りが読めていないので、クライアントともオリジンとも続けない
    let mut body_cut = false;
    let mut attempt = 0;
    let (mut server, head, status, request_body_bytes) = loop {
        attempt += 1;
        // 接続にかかった時間は「オリジンを掴むまで − 名前解決」で出す。時計を読むのは
        // ここで 1 回だけで、プールから再利用できたときは 2 回目を読まない (熱い経路)
        let acquire_started = Instant::now();
        // この時計より**前に**払われた名前解決 (入口の ACL) を控える (T15.0 (1))。
        // thread-local の読み 1 回で、時計もシステムコールも増えない
        let dns_before = crate::dns::peek_resolve_cost().0;
        let (mut server, reused) = match acquire_origin(
            &shared.upstream,
            origin_timeout,
            shared.connect_timeout,
            conn_id,
            &origin,
            pool_key,
            shared.resolved,
        ) {
            Ok(v) => v,
            Err(e) => {
                // 名前解決が締め切りを過ぎた (または上限で断った) ときは 504 (T.32.11)
                let (code, reason) = if crate::dns::is_timeout(&e) {
                    (504, "Gateway Timeout")
                } else {
                    (502, "Bad Gateway")
                };
                // 理由ごとに 1 分に 1 行 (T.29.15。本数は `/errors` と `/hosts` が持つ)
                if let Some(more) = shared.metrics.origin_warn(Failure::from_io(&e)) {
                    log_warn!(
                        Some(conn_id),
                        "{} {}: connect {} failed: {}{}",
                        code,
                        reason,
                        server_addr,
                        e,
                        more
                    );
                }
                // 入口で測った `client_read` は `ctx.detail` にしか無いので、
                // `shared.stages` ではなくこちらを引き継ぐ (T17.4)
                ctx.detail = origin_detail(
                    acquire_started,
                    Some(Failure::from_io(&e)),
                    ctx.detail.stages,
                    dns_before,
                );
                if let Some((entry, source)) = stale.take()
                    && !force_revalidate
                    && can_serve_stale(&entry)
                {
                    body::drain(reader, req_framing)?;
                    cache.stale_served.fetch_add(1, Ordering::Relaxed);
                    return serve_cached(client, entry, source, "STALE", 0, &ctx);
                }
                write_error(client, code, reason)?;
                // 原因つきで 1 件数える (デプロイ先の「エラー 12 件、原因は不明」を無くす)
                ctx.log_failure(code);
                return Ok(false);
            }
        };
        if !reused {
            ctx.detail = origin_detail(acquire_started, None, ctx.detail.stages, dns_before);
        }
        metrics.inc_origin_conn(reused);
        let head_sent = server.get_mut().write_all(&request_head);
        // 本文のある要求はやり直さない (`retryable` が偽) ので、100 は高々 1 回。
        // クライアントへ書けなければここで終わる (オリジンの誤りと混ぜない)
        if head_sent.is_ok() && send_continue {
            send_continue = false;
            client.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
            client.flush()?;
        }
        let mut body_sent = 0u64;
        let sent = head_sent
            .and_then(|_| {
                // 本文を送ってこないクライアントを見張る (T.29.34。見張りは本文のある要求だけが作る)
                forward_request_body(
                    reader,
                    server.get_mut(),
                    req_framing,
                    &mut body_sent,
                    shared.min_transfer,
                    conn_id,
                )
            })
            .and_then(|n| server.get_mut().flush().map(|_| n));
        // 本文を送る途中でオリジンが閉じた (`EPIPE` / `ECONNRESET`): 先に応答 (413 など) を返して
        // 閉じたのかもしれないので、読み残しの応答を読みに行く (RFC 9112 §9.5。T.29.7)。
        // 読めなければ今までどおり 502。本文の無い要求は対象外 (死んだ接続のやり直しに回す)。
        // 正常な要求にはシステムコールを 1 つも足さない (失敗したときだけ通る)
        let sent = match sent {
            Err(e)
                if req_framing.has_body()
                    && !is_client_body_error(&e)
                    && matches!(
                        e.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ) =>
            {
                log_debug!(
                    Some(conn_id),
                    "origin {} stopped reading the request body after {}B ({}); reading its response",
                    pool_key,
                    body_sent,
                    e
                );
                body_cut = true;
                Ok(body_sent)
            }
            other => other,
        };
        // 要求をオリジンへ送り終えた境目 (T14.3 (1))。ここが forward に足す唯一の時計
        let sent_at = crate::profile::mark();
        // 1xx (100 と 101 を除く) はヘッダーだけクライアントへ中継し、最終応答を読み続ける (T19.6)。
        // HTTP/1.0 のクライアントへは送らない (RFC 9110 §15.2 の MUST NOT。1.0 のクライアントは
        // 103 を最終応答と読む。T.29.3): 読み捨てて最終応答だけを返す
        let result = sent.and_then(|n| {
            crate::response::read_head_forwarding(&mut server, |head, status| {
                if !http11 {
                    log_trace!(
                        Some(conn_id),
                        "dropping interim {} response for an HTTP/1.0 client",
                        status
                    );
                    return Ok(());
                }
                interim_sent = true;
                forward_interim(client, head, status, conn_id)
            })
            .map(|h| (h, n))
        });
        match result {
            // 408 (Request Timeout) と 421 (Misdirected Request) は、再利用した接続に
            // 積まれていた応答か、この接続では処理できない印。冪等ならやり直す (RFC 9110 §15.5.20)
            Ok(((_, status), _))
                if reused
                    && retryable
                    && !interim_sent
                    && attempt == 1
                    && (status == 408 || status == 421) =>
            {
                log_debug!(
                    Some(conn_id),
                    "pooled connection to {} returned {}, retrying on a fresh one",
                    pool_key,
                    status
                );
                continue;
            }
            // 再検証の 304 の validator が保存済みと「あって違う」(RFC 9111 §4.3.4。T.32.9): その 304 で
            // 保存済みを延ばすと、古い本文に新しい `ETag` を付けて配り続ける (下流は新しい `ETag` で
            // 304 をもらい続ける)。項目を消し、条件なしで 1 回だけ取り直す。この接続は使い回さない
            // (304 の枠組みを読む前なので)。`stale` を外すので 2 回目は起きない。本文の無い GET だけ
            Ok(((head, 304), _))
                if stale.as_ref().is_some_and(|(entry, _)| {
                    not_modified_conflicts(
                        &freshness::parse_cached_head(&entry.head),
                        &headers::response_pairs(&head),
                    )
                }) =>
            {
                log_debug!(
                    Some(conn_id),
                    "304 from {} names another representation: dropped the stored entry, refetching",
                    pool_key
                );
                cache.remove(key);
                stale = None;
                drop(server);
                request_head = build_request_head(method, &origin, raw_headers, client_ip, &[]);
                origin_timeout = shared.timeout;
                attempt = 0;
                continue;
            }
            Ok(((head, status), n)) => {
                if origin_timeout != shared.timeout {
                    let _ = server.get_ref().set_timeouts(shared.timeout);
                }
                // 「forward の初バイト」= オリジンの応答ヘッダーを読み終えたところ
                // (本文の転送は相手と回線の都合なので、待ちの物差しにはこちらを使う)
                ctx.detail.first_byte_ms =
                    Some(ctx.started.elapsed().as_millis().min(u64::MAX as u128) as u64);
                // 要求を送るのにかかった時間 = 「送り終えるまで」− 名前解決 − 接続 (T14.3 (1))
                if let Some(at) = sent_at {
                    let acquire_and_send =
                        crate::profile::ms_u32(at.saturating_duration_since(acquire_started));
                    let origin_ms = (ctx.detail.dns_ms + ctx.detail.connect_ms) as u32;
                    ctx.detail.stages.send = acquire_and_send.saturating_sub(origin_ms);
                }
                break (server, head, status, n);
            }
            Err(e)
                if reused
                    && retryable
                    && !interim_sent
                    && attempt == 1
                    && is_stale_conn_error(&e) =>
            {
                log_debug!(
                    Some(conn_id),
                    "pooled connection to {} was stale ({}), retrying",
                    pool_key,
                    e
                );
                continue;
            }
            // クライアントの本文が壊れている (chunk の大きさが読めない など)。オリジンの誤りではない
            // ので 502 ではなく 400 で返して閉じる (T27.8)。オリジンへの接続はここで捨てる (プールに戻さない)
            Err(e) if is_client_body_error(&e) => {
                if let Some(more) = metrics.record_bad_request(
                    BadRequestReason::BodyFraming,
                    client_ip.unwrap_or("-"),
                    400,
                ) {
                    log_warn!(Some(conn_id), "400 Bad Request: {}{}", e, more);
                }
                let _ = write_error(client, 400, "Bad Request");
                return Ok(false);
            }
            Err(e) => {
                // オリジンの読み書きはブロッキング + `SO_RCVTIMEO` / `SO_SNDTIMEO` なので、
                // Linux では締め切りが切れると `WouldBlock` (EAGAIN) が返る。そのまま出すと
                // "Resource temporarily unavailable (os error 11)" になって締め切りと読めないので、
                // 言い換える (原因の数え方は `ErrCause::from_io` が `timeout` に寄せる。T.28.5)
                // 応答を読む途中の失敗も同じ分け方 (T25.0)。DNS はここまで来ないが、
                // `cause` と `dns_cause` を 1 か所で決めておくと組み合わせがずれない
                let failure = Failure::from_io(&e);
                // WARN は理由ごとに 1 分に 1 行 (T.29.15)
                if let Some(more) = shared.metrics.origin_warn(failure) {
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) {
                        log_warn!(
                            Some(conn_id),
                            "origin response timed out after {} s{}",
                            origin_timeout.as_secs_f64(),
                            more
                        );
                    } else {
                        log_warn!(
                            Some(conn_id),
                            "failed to read origin response: {}{}",
                            e,
                            more
                        );
                    }
                }
                ctx.detail.cause = Some(failure.cause);
                ctx.detail.dns_cause = failure.dns;
                if let Some((entry, source)) = stale.take()
                    && !force_revalidate
                    && can_serve_stale(&entry)
                {
                    cache.stale_served.fetch_add(1, Ordering::Relaxed);
                    return serve_cached(client, entry, source, "STALE", 0, &ctx);
                }
                write_error(client, 502, "Bad Gateway")?;
                ctx.log_failure(502);
                return Ok(false);
            }
        }
    };
    // 本文の途中でオリジンが返した応答 (T.29.7): クライアントの本文の残りは読んでいないので、
    // 応答に `Connection: close` を付けて閉じる (次の要求の頭に本文の続きが来てしまう)
    if body_cut {
        ctx.keep_client = false;
    }
    if request_body_bytes > 0 {
        log_debug!(
            Some(conn_id),
            "forwarded request body {}B",
            request_body_bytes
        );
    }
    // trace が切れていれば応答の先頭をもう 1 回なめない (T.28.12)
    if crate::log::enabled(crate::log::Level::Trace) {
        for (k, v) in headers::response_lines(&head) {
            log_trace!(Some(conn_id), "res header  {}: {}", k, v);
        }
    }

    // 枠組みも `Connection: close` も生の先頭から直接見る (組を作らない)。
    // ついでに `Content-Length` を検査する (T19.5)。重複して不一致・数字でない・負・
    // u64 に収まらない桁数は、そのまま通すと「名乗った長さと実際に送るバイトが食い違う」
    // 応答になるので、オリジンの応答として扱わず 502 に差し替える (この接続は再利用しない)
    // 枠組みと `Connection: close` を 1 回の走査で (T.28.12)
    // `Age` もこの 1 回で拾う (T.29.4。下の `age_secs`)
    let scan = Framing::scan_response_head(status, head_only, &head);
    let (framing, conn_close, scan_date) = (scan.framing, scan.close, scan.date);
    // 素通しの応答に付け直す `Age` (T.29.4)。`Age` は end-to-end (RFC 9110 §7.6・9111 §4.2.3) なのに、
    // 枠組みの名前と一緒に [`headers::write_response_head`] が無条件に落としていた (HIT にだけ付いた)。
    // 値は保存するときの `policy.age` と同じ `max(受けた Age, now − Date)`。**オリジンが `Age` を
    // 付けてきた応答だけ**に付ける (`Date` だけの応答に足すと、時計のずれがそのまま `Age` に出る)。
    // 読めない値は今までどおり落とす。`Date` を読むのは `Age` のある応答だけ
    // 304 (クライアントの条件付き要求の中継) と 101 は対象外
    let age_secs = scan
        .age
        .filter(|_| status >= 200 && status != 304)
        .and_then(|v| v.trim_ascii().parse::<u64>().ok())
        .map(|age| {
            let date = headers::response_value(&head, "date").and_then(crate::httpdate::parse);
            age.max(date.map_or(0, |d| now_epoch().saturating_sub(d)))
        });
    let framing = match framing {
        Ok(framing) => framing,
        Err(bad) => {
            if let Some(more) = ctx.metrics.origin_warn(Failure::of(ErrCause::Other)) {
                log_warn!(
                    Some(conn_id),
                    "502 Bad Gateway: bad Content-Length {:?} from {}{}",
                    bad.0,
                    pool_key,
                    more
                );
            }
            // 失敗の数え方 (`/status` の `errors`・`errors_by_cause`) に乗せる。
            // 原因は既存の 8 つのうち「上のどれでもない」を使う (配列は伸ばさない)
            ctx.detail.cause = Some(ErrCause::Other);
            write_error(client, 502, "Bad Gateway")?;
            ctx.log_failure(502);
            return Ok(false);
        }
    };
    // 頼んでいない 101 (RFC 9110 §15.2.2: クライアントが `Upgrade` で挙げていないプロトコルへ切り替えては
    // ならない。T.29.9)。今まではそのまま返していた。この接続はもう HTTP ではないので使い回さない
    if status == 101 && !req.upgrade {
        log_warn!(
            Some(conn_id),
            "502 Bad Gateway: unsolicited 101 Switching Protocols from {}",
            pool_key
        );
        ctx.detail.cause = Some(ErrCause::Other);
        write_error(client, 502, "Bad Gateway")?;
        ctx.log_failure(502);
        return Ok(false);
    }
    let origin_reusable =
        head.starts_with(b"HTTP/1.1") && framing != Framing::Close && !conn_close && !body_cut;
    // 最後が chunked でない `Transfer-Encoding` の応答 (T.28.1)。閉じるまでを本文にして返すが、
    // 区切りがオリジンと食い違っているかもしれないので保存しない (接続は上で使い回さない)。
    // 見分けるのは閉じるまでの応答のときだけ (CL / chunked の熱い経路では先頭をなめ直さない)
    //
    // 符号化はほどかずに渡すので、TE の行もクライアントへ伝える (T.28.35。下の `client_framing`)
    let unframed_te = (framing == Framing::Close)
        .then(|| Framing::response_transfer_encoding_line(&head))
        .flatten();
    if unframed_te.is_some() {
        log_debug!(
            Some(conn_id),
            "response Transfer-Encoding does not end in chunked: reading until close, not caching"
        );
    }

    // クライアントの条件付き要求に対するオリジンの 304 (T.29.6): 304 はそのまま中継するが、
    // 304 の validator が保存済みの表現と一致すれば、保存済みの表現も延ばして先頭を書き直す
    // (RFC 9111 §4.3.4)。今までは延ばさなかったので、続く素の GET が古い先頭のまま
    // `REFRESHING` でオリジンへ行っていた。validator が違う 304 では触らない
    if status == 304
        && let Some(entry) = client_cond_entry.take()
    {
        let cached_head = freshness::parse_cached_head(&entry.head);
        let pairs_304 = headers::response_pairs(&head);
        if not_modified_selects(&cached_head, &pairs_304) {
            if freshness::not_modified_forbids_storing(&pairs_304) {
                cache.remove(key);
            } else {
                let p = freshness::revalidated_policy(&pairs_304, &cached_head, cfg, now);
                match headers::merge_not_modified(&entry.head, &head) {
                    Some(new_head) => {
                        cache.refresh_with_head(key, url, &new_head, p.ttl, p.age, conn_id);
                    }
                    None => {
                        cache.refresh(key, p.ttl, p.age, conn_id);
                    }
                }
            }
            log_debug!(
                Some(conn_id),
                "client-conditional 304 from {}: refreshed the stored entry",
                pool_key
            );
        }
    }

    // 304: 保存済みの表現がまだ有効。延命して配信する
    if status == 304
        && let Some((entry, source)) = stale.take()
    {
        let cached_head = freshness::parse_cached_head(&entry.head);
        let pairs_304 = headers::response_pairs(&head);
        let mut entry = entry;
        // 304 が保存を禁じた (`no-store`・`private`・`Vary: Cookie` など): 延命せず項目を消し、
        // この要求にだけ 304 のヘッダーを載せた写しを配る (T.29.0)。次の要求はオリジンへ行く
        if freshness::not_modified_forbids_storing(&pairs_304) {
            cache.remove(key);
            if let Some(new_head) = headers::merge_not_modified(&entry.head, &head) {
                entry.replace_head(new_head);
            }
            if let Some(served) = headers::with_set_cookies(&entry.head, &head) {
                entry.replace_head(served);
            }
            entry.meta.stored_at = now_epoch();
            entry.meta.expires_at = entry.meta.stored_at;
            if origin_reusable {
                shared.upstream.pool.put(pool_key, server, shared.timeout);
            }
            return serve_cached(client, entry, source, "REVALIDATED", 0, &ctx);
        }
        let p = freshness::revalidated_policy(&pairs_304, &cached_head, cfg, now);
        // 304 のヘッダーで保存済みの先頭を書き直して置き直し、配るのも新しい先頭にする
        // (RFC 9111 §3.2・§4.3.4。T.28.2)。今までは期限だけ延ばし、配る先頭は古いままだった
        let expires_at = match headers::merge_not_modified(&entry.head, &head) {
            Some(new_head) => {
                let expires_at =
                    cache.refresh_with_head(key, url, &new_head, p.ttl, p.age, conn_id);
                entry.replace_head(new_head);
                expires_at
            }
            None => cache.refresh(key, p.ttl, p.age, conn_id),
        };
        // 配る `Age` も置き直した時刻から数える (`refresh` と同じく `now - 304 の Age`。T.28.34)。
        // 今までは手元の写しの古い `stored_at` のままで、保存から N 秒たって再検証した直後の応答が
        // `Age: N` になっていた (次の HIT からは新しい値)
        entry.meta.stored_at = now_epoch().saturating_sub(p.age);
        entry.meta.expires_at = expires_at;
        // 304 の `Set-Cookie` は保存した先頭へは写さず (T.29.0)、再検証したこの要求にだけ渡す
        if let Some(served) = headers::with_set_cookies(&entry.head, &head) {
            entry.replace_head(served);
        }
        if origin_reusable {
            shared.upstream.pool.put(pool_key, server, shared.timeout);
        }
        let ttl_left = p.ttl.as_secs().saturating_sub(p.age);
        return serve_cached(client, entry, source, "REVALIDATED", ttl_left, &ctx);
    }
    // オリジン障害: stale を配信 (禁止されていなければ)。この接続は再利用しない
    if STALE_ON_STATUS.contains(&status)
        && !force_revalidate
        && let Some((entry, source)) = stale.take()
        && can_serve_stale(&entry)
    {
        log_debug!(
            Some(conn_id),
            "origin returned {}: serving stale entry",
            status
        );
        cache.stale_served.fetch_add(1, Ordering::Relaxed);
        return serve_cached(client, entry, source, "STALE", 0, &ctx);
    }
    if lookup_allowed {
        metrics.inc_cache_miss();
    }

    // 最後が chunked でない TE を HTTP/1.0 のクライアントへは送れない (RFC 9112 §6.1: TE を送ってよいのは
    // HTTP/1.1 以上を名乗った要求への応答だけ)。符号化をほどけないので、TE を落として符号化済みの
    // バイトを本文として渡す (T.28.35 より前の振る舞い) 代わりに 502 にする (この接続は再利用しない)
    if !http11 && let Some(te) = unframed_te.as_deref() {
        if let Some(more) = ctx.metrics.origin_warn(Failure::of(ErrCause::Other)) {
            log_warn!(
                Some(conn_id),
                "502 Bad Gateway: {:?} from {} cannot be relayed to an HTTP/1.0 client{}",
                te,
                pool_key,
                more
            );
        }
        ctx.detail.cause = Some(ErrCause::Other);
        write_error(client, 502, "Bad Gateway")?;
        ctx.log_failure(502);
        return Ok(false);
    }

    // ---- 配信しつつ保存 ----
    let policy = if store_allowed && unframed_te.is_none() {
        freshness::response_policy_lines(
            status,
            headers::response_lines(&head),
            req.cookie,
            cfg,
            now,
        )
    } else {
        None
    };
    if policy.is_none() && stale.is_some() && (200..400).contains(&status) {
        // 新しい表現が届いたのに保存できないので、古い表現も捨てる (4xx/5xx では残す)
        cache.remove(key);
    }
    // unsafe メソッドへの成功応答は対象 URL (と Location 先) のキャッシュを無効化する (RFC 9111 §4.4)
    if !is_get && !head_only && (200..400).contains(&status) && cache_on {
        cache.invalidate(url, conn_id);
        for name in ["location", "content-location"] {
            if let Some(target) = headers::response_value(&head, name)
                && let Ok(o) = parse_origin(target, None)
                && o.scheme == origin.scheme
                && o.server_addr() == server_addr
            {
                cache.invalidate(&o.url(), conn_id);
            }
        }
    }
    // 保存するのは元の Location のまま (配信時に必要なら書き換える)。
    // マッピング形式でなければ String を 1 本ずつ作らずに直接バイト列へ書く。
    // **保存しないなら組み立てもしない**: 素通しの経路では、この 1 本のために
    // 応答ヘッダーをもう一度なめて Vec を育て直していた (要求ごとに確保 5 回)
    let cached_head = policy.is_some().then(|| {
        let mut out = Vec::with_capacity(head.len() + 64);
        headers::write_response_head(&mut out, &head, None, &[], &[] as &[&str]);
        out
    });
    let sanitized = origin.mapped.then(|| {
        let mut h = headers::sanitize_response_head(&head);
        map_locations(&mut h.lines);
        h
    });
    // クライアント向けの枠組み: 長さが分かればそのまま、分からなければ HTTP/1.1 には再 chunk。
    // 最後が chunked でない TE の応答は、オリジンの TE の行を残して閉じるまでを本文にする (T.28.35。
    // `gzip, chunked` に包み直してもよいが、バイト列をオリジンのまま渡す方を選んだ)
    let client_framing = match framing {
        Framing::None | Framing::Length(_) => framing,
        Framing::Close if unframed_te.is_some() => Framing::Close,
        Framing::Chunked | Framing::Close => {
            if http11 {
                Framing::Chunked
            } else {
                Framing::Close
            }
        }
    };
    if client_framing == Framing::Close {
        ctx.keep_client = false;
    }
    // 足すヘッダー行は借用のまま持つ (`Vec<String>` と `format!` を要求ごとに作らない)
    let mut cl_buf = [0u8; 40];
    // HEAD のときだけ確保する (それ以外は 1 バイトも作らない)
    let cl_owned;
    let mut age_buf = [0u8; 32];
    let mut date_buf = [0u8; 35];
    let mut extra: [&str; 5] = [""; 5];
    let mut n_extra = 0usize;
    match client_framing {
        Framing::Length(len) => {
            extra[0] = content_length_line(&mut cl_buf, len);
            n_extra = 1;
        }
        Framing::Chunked => {
            extra[0] = "Transfer-Encoding: chunked";
            n_extra = 1;
        }
        Framing::Close if let Some(te) = unframed_te.as_deref() => {
            extra[0] = te;
            n_extra = 1;
        }
        Framing::None if head_only => {
            // HEAD: GET と同じヘッダーを返す (本文は無い)。TE があれば GET と同じく、最後が chunked なら
            // `Transfer-Encoding: chunked`、そうでなければオリジンの並びを 1 行で (HTTP/1.1 のクライアントだけ。
            // T.28.36)。TE が無ければ `Content-Length`
            let te = if http11 {
                Framing::response_transfer_encoding_line(&head)
            } else {
                None
            };
            if let Some(te) = te {
                let chunked = te
                    .rsplit(',')
                    .next()
                    .is_some_and(|t| t.trim_ascii().eq_ignore_ascii_case("chunked"));
                cl_owned = if chunked {
                    "Transfer-Encoding: chunked".to_string()
                } else {
                    te
                };
                extra[0] = &cl_owned;
                n_extra = 1;
            } else if let Some(v) = headers::response_value(&head, "content-length") {
                cl_owned = format!("Content-Length: {}", v);
                extra[0] = &cl_owned;
                n_extra = 1;
            }
        }
        _ => {}
    }
    // 応答にも自分の `Via` を足す (RFC 9110 §7.6.3。T.28.10 (b))。保存する先頭 (`cached_head`) には入れない
    // `Date` の無い応答には自分で付ける (RFC 9110 §6.6.1: 時計を持つ送り手は付けなければならない。T.29.9)。
    // 有無は枠組みと同じ 1 回の走査で見ているので、`Date` のある応答 (ほとんど全部) には何も足さない
    if !scan_date {
        extra[n_extra] = crate::httpdate::date_line(&mut date_buf);
        n_extra += 1;
    }
    if let Some(age) = age_secs {
        extra[n_extra] = serve::age_line(&mut age_buf, age);
        n_extra += 1;
    }
    extra[n_extra] = crate::via::field();
    n_extra += 1;
    extra[n_extra] = ctx.connection_line();
    n_extra += 1;
    let extra = &extra[..n_extra];
    let client_head = match &sanitized {
        Some(h) => h.assemble(extra),
        None => {
            let mut out = Vec::with_capacity(head.len() + 96);
            headers::write_response_head(&mut out, &head, None, &[], extra);
            out
        }
    };
    let expected = match framing {
        Framing::Length(n) => cached_head
            .as_ref()
            .map(|h| n.saturating_add(h.len() as u64)),
        _ => None,
    };
    let mut sink =
        policy.map(|p| cache.begin_store(key, url, p.ttl, p.age, p.validators, expected, conn_id));
    if let (Some(s), Some(h)) = (sink.as_mut(), cached_head.as_ref()) {
        s.write(h);
    }

    // ---- 小さな本文は、ヘッダーを出す前に読み切る (T19.5) ----
    // オリジンが名乗った `Content-Length` より短い本文で閉じたとき、**ヘッダーをまだ
    // 出していなければ 502 に差し替えられる**。出したあとでは差し替えられない (接続を
    // 閉じるだけ) ので、手元のバッファ ([`CopyBuf`]、32 KiB) に収まる本文だけ先に読む。
    //
    // 読む量も書く量も今までと同じで、変わるのはヘッダーを出す順番だけ
    // (正常な応答のバイト列は 1 バイトも変わらない)。**代償は初バイト**: この大きさの
    // 本文では、ヘッダーがクライアントに届くのが本文を読み終えた後ろになる。上限を
    // 中継バッファと同じ 32 KiB にしてあるのは、この待ちを「ふつうの小さな応答が
    // 1 パケットで届く範囲」に収めるため (それを超える本文は今までどおり流しながら運ぶ)
    let mut pre_buf: Option<CopyBuf> = None;
    let mut early = 0usize;
    if !head_only
        && client_framing == framing
        && let Framing::Length(total) = framing
        && total as usize <= COPY_BUF_SIZE
    {
        let buf = pre_buf.get_or_insert_with(CopyBuf::take);
        let mut body = BodyReader::new(&mut server, framing);
        while early < total as usize {
            match body.read(&mut buf[early..total as usize]) {
                Ok(0) => break,
                Ok(n) => early += n,
                Err(ref e) if is_eof_like(e) => {
                    log_debug!(Some(conn_id), "origin body ended early: {}", e);
                    break;
                }
                Err(e) => {
                    if let Some(s) = sink.take() {
                        s.abort();
                    }
                    return Err(e);
                }
            }
        }
        if early < total as usize {
            // 名乗った長さに足りないままオリジンが閉じた。**まだ 1 バイトも出していない**ので、
            // 中途半端な `Content-Length` を残さず 502 に差し替える (この接続は再利用しない)
            if let Some(s) = sink.take() {
                s.abort();
            }
            // 原因は既存の 8 つ (EOF で切られた = `reset`)
            ctx.detail.cause = Some(ErrCause::Reset);
            if let Some(more) = ctx.metrics.origin_warn(Failure::of(ErrCause::Reset)) {
                log_warn!(
                    Some(conn_id),
                    "502 Bad Gateway: origin body ended at {}B of {}B{}",
                    early,
                    total,
                    more
                );
            }
            write_error(client, 502, "Bad Gateway")?;
            ctx.log_failure(502);
            return Ok(false);
        }
        if let Some(s) = sink.as_mut() {
            s.write(&buf[..early]);
        }
    }

    client.write_all(&client_head)?;
    let mut body_bytes = early as u64;
    let mut clean = true;
    if let Some(buf) = pre_buf.as_ref()
        && early > 0
    {
        // 先に読み切った本文 (ヘッダーのすぐ後ろに置く)
        client.write_all(&buf[..early])?;
    }

    // ---- 書き換えも保存もしない本文は splice(2) でカーネル内を運ぶ ----
    let mut spliced = false;
    #[cfg(target_os = "linux")]
    if early == 0
        && sink.is_none()
        && !head_only
        && client_framing == framing
        && let Framing::Length(total) = framing
        && let OriginStream::Plain(origin_tcp) = server.get_ref()
        && total.saturating_sub(server.buffer().len() as u64) >= SPLICE_MIN_BYTES
    {
        use std::os::fd::AsRawFd;
        let (origin_fd, client_fd) = (
            origin_tcp.as_raw_fd(),
            client.get_ref().get_ref().as_raw_fd(),
        );
        // 応答ヘッダーを読むときに一緒に読んでしまった本文の先頭は自分で書く
        // (ヘッダーと同じ 1 回の sendto に載る)
        let buffered = server.buffer().len().min(total as usize);
        if buffered > 0 {
            let prefix = server.buffer()[..buffered].to_vec();
            client.write_all(&prefix)?;
            server.consume(buffered);
            body_bytes += buffered as u64;
        }
        client.flush()?;
        // 最小の速さを見るときだけ、運ぶ間クライアントを待たない形にする (T.29.34)
        let nonblocking = client.get_ref().is_on();
        if nonblocking {
            client.get_ref().get_ref().set_nonblocking(true)?;
        }
        let relayed = passthrough::relay(
            origin_fd,
            client_fd,
            total - buffered as u64,
            client.get_mut().guard(),
            shared.timeout,
        );
        if nonblocking {
            client.get_ref().get_ref().set_nonblocking(false)?;
        }
        let (moved, ok) = relayed?;
        body_bytes += moved;
        clean = ok;
        spliced = true;
        log_trace!(Some(conn_id), "spliced {}B of body to the client", moved);
    }

    if !spliced && early == 0 {
        let buf: &mut [u8] = pre_buf.get_or_insert_with(CopyBuf::take);
        let mut body = BodyReader::new(&mut server, framing);
        loop {
            let n = match body.read(&mut *buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(ref e) if is_eof_like(e) => {
                    log_debug!(Some(conn_id), "origin body ended early: {}", e);
                    clean = false;
                    break;
                }
                // chunk の枠が壊れている・行が長すぎる (T.29.1)。先頭はもう出ているので、途中で切れた
                // 本文と同じに扱う (終端の chunk を出さずに閉じる)。本数はオリジンが決めるので 1 分に 1 行
                Err(ref e) if e.kind() == io::ErrorKind::InvalidData => {
                    static FRAMING_WARN: crate::log::Throttle = crate::log::Throttle::new();
                    if let Some(more) = FRAMING_WARN.pass(crate::log::WARN_EVERY_SECS) {
                        log_warn!(
                            Some(conn_id),
                            "origin body from {} cut: {}{}",
                            pool_key,
                            e,
                            more
                        );
                    }
                    clean = false;
                    break;
                }
                Err(e) => {
                    if let Some(s) = sink.take() {
                        s.abort();
                    }
                    return Err(e);
                }
            };
            if client_framing == Framing::Chunked {
                body::write_chunk(client, &buf[..n])?;
            } else {
                client.write_all(&buf[..n])?;
            }
            body_bytes += n as u64;
            if let Some(s) = sink.as_mut() {
                s.write(&buf[..n]);
            }
        }
        clean = clean && body.finished_cleanly();
    }
    if clean && client_framing == Framing::Chunked {
        body::write_last_chunk(client)?;
    }
    client.flush()?;

    let cache_state: Cow<'static, str> = if clean {
        if origin_reusable {
            shared.upstream.pool.put(pool_key, server, shared.timeout);
        }
        match (policy, sink) {
            (Some(p), Some(s)) => {
                let out = s.finish();
                if out.memory || out.disk {
                    if let Some(guard) = leader.take() {
                        guard.complete(FetchOutcome::Stored);
                    }
                    // 保存できたときだけ組み立てる (素通しの経路では確保しない)
                    Cow::Owned(format!("MISS stored ttl={}s", p.ttl.as_secs()))
                } else {
                    Cow::Borrowed("MISS")
                }
            }
            _ if !lookup_allowed => Cow::Borrowed("BYPASS"),
            _ => Cow::Borrowed("MISS"),
        }
    } else {
        // 途中で切れた本文はクライアントにもそれと分かる形 (終端チャンク無し / 短い本文) で伝わる
        if let Some(s) = sink {
            s.abort();
        }
        ctx.keep_client = false;
        Cow::Borrowed("MISS truncated")
    };

    // 保存されなかった場合は待っている要求に自分で取りに行かせる (Drop でも通知される)
    if let Some(guard) = leader.take() {
        // この鍵は保存されないと分かったので覚えておく (次の同時ミスは合流させない。T11.9)。
        // 途中で切れた転送は一時的な失敗なので覚えない (次はうまくいくかもしれない)
        if clean {
            cache.remember_not_stored(key, now);
        }
        guard.complete(FetchOutcome::NotStored);
    }
    let total = client_head.len() as u64 + body_bytes;
    // 起動時の自己ベンチ (T14.43) が自分で打った要求のバイトは合計に足さない
    // (`/status` の `bytes_forwarded` と `/history` の `bytes` に 20 MB の山が立つため)。
    // 費用は自己ベンチが回っていないときの原子の読み 1 回
    if !proxy_metrics::selfbench::is_target(ctx.pool_key) {
        metrics.add_bytes(total + request_body_bytes);
    }
    // 個票の「上り」は要求の本文ぶん (ヘッダーぶんは本体クレートが足す。T14.4)
    ctx.up_bytes = request_body_bytes;
    ctx.log(status, total, &cache_state);
    Ok(ctx.keep_client)
}

/// クライアントから受けたヘッダーをそのまま使って、オリジンへの要求の先頭を組み立てる。
/// [`request_head`] と同じ形だが、中間の `Vec<String>` を作らない。
pub(super) fn build_request_head(
    method: &str,
    origin: &Origin,
    raw_headers: &[String],
    client_ip: Option<&str>,
    conditional: &[String],
) -> Vec<u8> {
    let mut head = Vec::with_capacity(256 + raw_headers.iter().map(|h| h.len()).sum::<usize>());
    head.extend_from_slice(method.as_bytes());
    head.extend_from_slice(b" ");
    head.extend_from_slice(origin.path.as_bytes());
    head.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    head.extend_from_slice(origin.host_port.as_bytes());
    head.extend_from_slice(b"\r\n");
    headers::write_request_headers(&mut head, raw_headers, client_ip);
    for line in conditional {
        head.extend_from_slice(line.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    head
}

/// オリジンへの要求の先頭 (要求行 + Host + 転送するヘッダー + 条件付きヘッダー + 空行)。
/// Host はオリジンのものに差し替える (マッピング形式ではプロキシ宛ての Host が来る)。
/// 裏の再検証 ([`refresh`]) のように、転送するヘッダーを自前で作る経路で使う。
pub(super) fn request_head(
    method: &str,
    origin: &Origin,
    forwarded: &[String],
    conditional: &[String],
) -> Vec<u8> {
    let mut head = format!("{} {} HTTP/1.1\r\n", method, origin.path).into_bytes();
    head.extend_from_slice(format!("Host: {}\r\n", origin.host_port).as_bytes());
    for h in forwarded {
        head.extend_from_slice(h.as_bytes());
    }
    for line in conditional {
        head.extend_from_slice(line.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    head
}

/// プールにあれば再利用し、無ければ接続する (HTTPS なら TLS まで)。戻り値の bool は再利用したか。
/// `timeout` は接続と最初の応答を待つ時間 (stale があるときは短くする)。
///
/// プールから取り出すときは**どの要求でも**生存確認 (`recv(MSG_PEEK|MSG_DONTWAIT)` 1 回) をする
/// (T.29.2)。T26.0 で GET / HEAD (やり直せる要求) だけ省いていたが、省くと「前の応答を読み切った
/// あとにオリジンが遅れて送ったバイト」を次の要求の応答として読んでしまう (手元で: `GET /a` の
/// `CL: 2` の 0.3 秒後に届いた `HTTP/1.1 200 … X-Injected: yes … EVIL` が次の `GET /b` の応答になり、
/// その次は `HIT`)。覗けば読み残しがある接続は捨てる。覗いてから読むまでの窓は塞げない (README)。
pub(super) fn acquire_origin(
    upstream: &Upstream,
    timeout: Duration,
    connect_timeout: Duration,
    conn_id: usize,
    origin: &Origin,
    pool_key: &str,
    resolved: Option<&dns::Resolved<'_>>,
) -> io::Result<(OriginConn, bool)> {
    if let Some(server) = upstream.pool.get(pool_key, timeout) {
        log_debug!(Some(conn_id), "reusing pooled connection to {}", pool_key);
        return Ok((server, true));
    }
    log_debug!(Some(conn_id), "connecting to origin {}", pool_key);
    // TCP の確立だけは短い締め切り (`PROXY_CONNECT_TIMEOUT_SECS`)。`timeout` の方が短ければ
    // (stale を持っていて待たない回) そちらに合わせる。0 (無期限) は必ず負ける (T.28.8)
    let stream = origin::connect_within(
        origin.scheme,
        &origin.server_addr(),
        &origin.host(),
        proxy_base::timeout::shorter(connect_timeout, timeout),
        timeout,
        upstream.tls.as_ref(),
        resolved,
    )?;
    Ok((BufReader::with_capacity(ORIGIN_READ_BUF, stream), false))
}

/// 要求の `max-age=N` / `min-fresh=N` を、保存済みの表現がそのままでは満たさないか (T.28.10 (c))。
///
/// `max-age`: 保存からの経過 (`now - stored_at`) が N 秒を越える。`min-fresh`: 期限までが N 秒に足りない。
/// 値の読めない指示は見ない (今までどおり)。`max-age=0` は呼ぶ側が先に `no-cache` と同じに扱っている。
fn client_wants_fresher(cache_control: &str, entry: &CachedResponse, now: u64) -> bool {
    let age = now.saturating_sub(entry.meta.stored_at);
    freshness::directive_value(cache_control, "max-age").is_some_and(|n| age > n)
        || freshness::directive_value(cache_control, "min-fresh")
            .is_some_and(|n| entry.ttl_left(now) < n)
}

/// 304 が保存済みの表現を指しているか (RFC 9111 §4.3.4 の「更新する保存済みの応答を選ぶ」の簡易版。T.29.6)。
///
/// 304 に `ETag` があれば保存済みの `ETag` と一致すること (弱い印 `W/` も含めて文字どおり)。
/// 無ければ `Last-Modified` が一致すること。どちらも無い 304 は選ばない (別の表現かもしれない)。
fn not_modified_selects(cached: &freshness::CachedHead, pairs_304: &[(String, String)]) -> bool {
    let value = |name: &str| {
        pairs_304
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim_ascii())
    };
    if let Some(etag) = value("etag") {
        return cached.etag.as_deref().map(str::trim_ascii) == Some(etag);
    }
    match value("last-modified") {
        Some(lm) => cached.last_modified.as_deref().map(str::trim_ascii) == Some(lm),
        None => false,
    }
}

/// 再検証の 304 が保存済みとは別の表現を指しているか (RFC 9111 §4.3.4。T.32.9)。
///
/// 304 の `ETag` が保存済みの `ETag` と違う (比べるのは弱い印 `W/` を外して)、または `ETag` で比べられず
/// `Last-Modified` が両方にあって違うときに真。validator の無い 304 と、比べる相手が保存済みに無いときは偽
/// (今までどおり延ばす)。[`not_modified_selects`] より甘い: こちらは「延ばしてはいけない」の判定
fn not_modified_conflicts(cached: &freshness::CachedHead, pairs_304: &[(String, String)]) -> bool {
    let value = |name: &str| {
        pairs_304
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim_ascii())
    };
    let weak = |v: &str| v.strip_prefix("W/").unwrap_or(v).to_string();
    if let (Some(got), Some(have)) = (value("etag"), cached.etag.as_deref()) {
        return weak(got) != weak(have.trim_ascii());
    }
    match (value("last-modified"), cached.last_modified.as_deref()) {
        (Some(got), Some(have)) => got != have.trim_ascii(),
        _ => false,
    }
}

/// クライアントのリクエスト本文をオリジンへ同じ枠組みで転送する。戻り値は本文のバイト数。
///
/// `total` には送れたぶんを積む (途中で失敗しても、そこまでのバイト数が残る。T.29.7)。
/// 読む 1 回ごとに見張り (`rate`) で待ちを測る (本文を送ってこない相手を切る。T.29.34)。
fn forward_request_body(
    reader: &mut crate::clientio::ClientReader<'_>,
    server: &mut OriginStream,
    framing: Framing,
    total: &mut u64,
    rate: &proxy_base::minrate::MinRate,
    conn_id: usize,
) -> io::Result<u64> {
    match framing {
        Framing::None | Framing::Close => Ok(0),
        Framing::Length(_) => {
            // io::copy はスタックの 8 KiB で読むため、512 KiB の本文で recvfrom が 66 回出る。
            // chunked 側と同じ 64 KiB の使い回しバッファで読むと 8 回で済む
            let mut body = BodyReader::new(reader, framing);
            let mut buf = CopyBuf::take();
            let mut guard = rate.guard(Side::RequestBody, conn_id);
            loop {
                let n = guard
                    .time(|| body.read(&mut buf))
                    .map_err(client_body_error)?;
                if n == 0 {
                    break;
                }
                server.write_all(&buf[..n])?;
                *total += n as u64;
            }
            Ok(*total)
        }
        Framing::Chunked => {
            let mut body = BodyReader::new(reader, framing);
            let mut buf = CopyBuf::take();
            let mut guard = rate.guard(Side::RequestBody, conn_id);
            loop {
                let n = guard
                    .time(|| body.read(&mut buf))
                    .map_err(client_body_error)?;
                if n == 0 {
                    break;
                }
                body::write_chunk(server, &buf[..n])?;
                *total += n as u64;
            }
            body::write_last_chunk(server)?;
            Ok(*total)
        }
    }
}

/// `Max-Forwards: 0` の TRACE / OPTIONS に自分で応える (RFC 9110 §7.6.2・§9.3.7・§9.3.8。T27.8)。
///
/// OPTIONS は `200` で本文なし。TRACE は `200` + `Content-Type: message/http` で、受け取った要求行と
/// ヘッダーをそのまま返す。ただし `Cookie`・`Authorization`・`Proxy-Authorization` の行は返さない
/// (§9.3.8 は「機微な値を含む行を除いてよい」。TRACE で認証情報を読み出す手口を塞ぐ)。返したら閉じる
fn write_max_forwards_zero(
    client: &mut impl Write,
    trace: bool,
    request_line: &str,
    raw_headers: &[String],
) -> io::Result<()> {
    let mut body = Vec::new();
    if trace {
        body.extend_from_slice(request_line.trim_end().as_bytes());
        body.extend_from_slice(b"\r\n");
        for line in raw_headers {
            let name =
                proxy_base::ascii::split_once(line, b':').map_or("", |(k, _)| k.trim_ascii());
            if ["cookie", "authorization", "proxy-authorization"]
                .iter()
                .any(|n| name.eq_ignore_ascii_case(n))
            {
                continue;
            }
            body.extend_from_slice(line.trim_end().as_bytes());
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"\r\n");
    }
    let mut date = [0u8; 35];
    let head = format!(
        "HTTP/1.1 200 OK\r\n{}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
        crate::httpdate::date_line(&mut date),
        if trace {
            "Content-Type: message/http\r\n"
        } else {
            ""
        },
        body.len()
    );
    client.write_all(head.as_bytes())?;
    client.write_all(&body)?;
    client.flush()
}

/// クライアントの本文を読む側で起きた誤り (T27.8)。オリジンとの読み書きの失敗 (502) と分けて、
/// chunk の大きさが読めない (`0x5`・桁あふれ) などの**クライアントの誤り**を 400 で返すための印。
#[derive(Debug)]
struct ClientBodyError(io::Error);

impl std::fmt::Display for ClientBodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "client request body: {}", self.0)
    }
}

impl std::error::Error for ClientBodyError {}

fn client_body_error(e: io::Error) -> io::Error {
    io::Error::new(e.kind(), ClientBodyError(e))
}

/// `e` がクライアントの本文を読む側の誤りか ([`client_body_error`] で包んだもの)。
fn is_client_body_error(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<ClientBodyError>())
}

/// 中間応答 (1xx) をクライアントへ中継する。**ヘッダー部だけで、本文は持たない** (T19.6)。
///
/// RFC 9110 §15.2 は、自分が起こした要求でない限り 1xx を中継することを求めている。
/// 整形は最終応答と同じ [`headers::write_response_head`] を通す — hop-by-hop
/// (`Connection` が名指ししたものを含む) と枠組みのヘッダーを落とし、obs-fold は SP 1 個に
/// 畳む。足す行は無い (`Content-Length` / `Connection` は 1xx に付けない)。
///
/// **キャッシュには載せない**し、`/recent` とホスト別の統計にも数えない (数えるのは
/// 最終応答の 1 本だけ)。
fn forward_interim(
    client: &mut impl Write,
    head: &[u8],
    status: u16,
    conn_id: usize,
) -> io::Result<()> {
    let mut out = Vec::with_capacity(head.len() + 16);
    headers::write_response_head(&mut out, head, None, &[], &[] as &[&str]);
    client.write_all(&out)?;
    // 最終応答を読み終えるまで抱えない: クライアントは 103 を見てから先の資源を取りに行く
    client.flush()?;
    log_trace!(
        Some(conn_id),
        "forwarded interim {} response ({}B)",
        status,
        out.len()
    );
    Ok(())
}

/// 自前の誤りの応答。T.29.9 から `Date` も付ける (RFC 9110 §6.6.1)。
fn write_error(client: &mut impl Write, status: u16, reason: &str) -> io::Result<()> {
    let mut date = [0u8; 35];
    let resp = format!(
        "HTTP/1.1 {} {}\r\n{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        status,
        reason,
        crate::httpdate::date_line(&mut date)
    );
    client.write_all(resp.as_bytes())?;
    client.flush()
}

/// オリジンを掴むまでの内訳を組み立てる (T12.4 (2))。**接続を試した直後の 1 回だけ**呼ぶ
/// (thread-local を読んで 0 に戻すので、2 回呼ぶと 2 回目が空になる)。
///
/// **`dns_before` は `acquire_started` より前に既に払われていた名前解決 (ms)**
/// (T15.0 (1)。トンネル側の `detail_of` と同じ引き算)。入口の ACL
/// (`PROXY_ALLOW_LOCAL=false` が既定) がここより前に名前を引くので、それを引かずに
/// 「全体 − 名前解決」をすると接続の段が 0 に潰れ、同じぶんが `client_read` にも
/// 二重に乗る。**プールを使い回した要求はこの関数を通らない**ので、そちらの
/// `client_read` には ACL の名前解決が入ったままになる (要求の 1%。README に 1 行)。
fn origin_detail(
    acquire_started: Instant,
    cause: Option<Failure>,
    stages: StageMs,
    dns_before: u64,
) -> Detail {
    let (dns_ms, dns_misses) = crate::dns::take_resolve_cost();
    let total_ms = acquire_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
    // `acquire_started` からの窓の**内側**で払った名前解決だけを引く
    let dns_inside = dns_ms.saturating_sub(dns_before);
    Detail {
        dns_ms,
        dns_misses,
        connect_ms: total_ms.saturating_sub(dns_inside),
        family_v6: crate::dns::take_family(),
        // 確立までに SYN を送り直した回数 (T14.46)。**プールが接続を張った要求だけ**
        // 0 でない (使い回せた要求は `net` の確立点を通らないので 0)
        syn_retrans: crate::dns::take_syn_retrans(),
        // 失敗のときは 8 つの原因と DNS の内訳を 1 回で受け取る (T25.0)
        cause: cause.map(|f| f.cause),
        dns_cause: cause.and_then(|f| f.dns),
        first_byte_ms: None,
        // ここまでに測った段階 (`queue` / `client_read`) は引き継ぐ (T14.3 (1))。
        // `client_read` には時計より前の名前解決が丸ごと入っているので除く (T15.0 (1))
        stages: StageMs {
            client_read: stages
                .client_read
                .saturating_sub(dns_before.min(u32::MAX as u64) as u32),
            ..stages
        },
        // 向き別のバイト (T14.26) は応答を流し終えてからでないと分からない
        // ([`Ctx::log`] が入れる)
        ..Detail::default()
    }
}

/// 再利用した接続が死んでいた (要求が届いていない) と判断してよいエラーか。
/// タイムアウトは「届いたが処理が遅い」場合があるので含めない (含めると二重送信になる)。
fn is_stale_conn_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            // 状態行が壊れている = 前のやり取りの読み残しを読んだ
            | io::ErrorKind::InvalidData
    )
}

fn is_eof_like(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::TimedOut
    )
}
