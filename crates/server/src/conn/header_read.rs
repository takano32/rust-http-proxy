//! 要求行とヘッダーを読む部品 (行の置き場・期限・断るときの応答)。

use super::*;

use std::io::{BufRead, Read};

use super::first_line::Peer;

/// 要求行・ヘッダー行 1 本の最大長と、ヘッダー行数の上限 (超えたら 414 / 431)。
pub(super) const MAX_LINE: usize = 64 * 1024;
pub(super) const MAX_HEADER_LINES: usize = 256;
/// 1 要求のヘッダー全体 (要求行 + ヘッダー行) の合計上限。
///
/// 1 行の上限だけだと `MAX_LINE` の行を `MAX_HEADER_LINES` 本並べられ、認証なしの
/// 開放プロキシでは正常な形の要求を数本送るだけでメモリを食い潰せる
/// (実測: 同時 8 接続で RSS 15 MiB → 272 MiB)。要求行 1 本 + 最大長のヘッダー 1 本は通る幅。
pub(super) const MAX_HEADER_BYTES: usize = 128 * 1024;
/// 次の要求のために抱えておく行数と 1 行の容量。これを超えたぶんは要求ごとに解放する
/// (使い回しの利得はほぼそのままで、接続あたりの居座りを 32 KiB 程度に抑える)。
const KEEP_LINES: usize = 32;
const KEEP_LINE_CAP: usize = 1024;

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
pub(super) struct Scratch {
    pub(super) lines: Vec<String>,
    /// 今の要求で使っている行数
    pub(super) used: usize,
    pub(super) request_line: String,
}

impl Scratch {
    /// 次の要求に備えて空にする。前の要求で大きく育った行は容量ごと手放す
    /// (使い回すのは先頭 [`KEEP_LINES`] 本 × [`KEEP_LINE_CAP`] まで。大きなヘッダーを
    /// 1 回送られただけでメモリを抱え込まないように)。
    pub(super) fn reset(&mut self) {
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
    pub(super) fn take() -> Scratch {
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
    pub(super) fn next(&mut self) -> &mut String {
        if self.used == self.lines.len() {
            self.lines.push(String::new());
        }
        let line = &mut self.lines[self.used];
        line.clear();
        line
    }

    pub(super) fn commit(&mut self) {
        self.used += 1;
    }

    pub(super) fn headers(&self) -> &[String] {
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
pub(super) enum Line {
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
pub(super) fn read_line_or_idle(
    reader: &mut clientio::ClientReader<'_>,
    line: &mut String,
    extend: Option<std::time::Duration>,
    allow_idle: bool,
    extended: &mut bool,
    clock: &mut HeaderClock<'_>,
) -> io::Result<Line> {
    loop {
        match read_limited_line(reader, line, clock) {
            Ok(v) => return Ok(Line::Read(v)),
            Err(e) if is_timeout(&e) => {
                let Some(full) = extend else {
                    return Err(e);
                };
                if *extended || clock.expired() {
                    return Err(e);
                }
                if allow_idle && line.is_empty() {
                    return Ok(Line::Idle);
                }
                // 要求の途中: 猶予ではなく本来のアイドル時間で待ち直す
                // (ヘッダーの期限が先に来るなら、次の読みの前に `HeaderClock` が縮める)
                clock.set_timeout(full)?;
                *extended = true;
            }
            Err(e) => return Err(e),
        }
    }
}

/// [`read_headers`] が読むのをやめた理由 (断る応答は [`refuse_headers`] が組む)。
pub(super) enum HeaderStop {
    /// 1 行が `MAX_LINE` を越えた (431)
    LineTooLong,
    /// ヘッダー全体が `MAX_HEADER_BYTES` を越えた (431)
    TooManyBytes,
    /// 行が `MAX_HEADER_LINES` を越えた (431)
    TooManyLines,
    /// ヘッダーの途中で期限を越えた・黙った (408。T.28.0)
    Timeout,
    /// ほかの読みの失敗 (そのまま返す)
    Io(io::Error),
}

/// ヘッダーを空行まで読んで `scratch` に積み、ヘッダー全体の大きさ (要求行を含む) を返す。
///
/// `header_bytes` は要求行のバイト数。ヘッダーは読むだけで、なめるのは読み終えてからの 1 回
/// (`parse_request_headers_visit`。T.28.15)。
#[inline]
pub(super) fn read_headers(
    reader: &mut clientio::ClientReader<'_>,
    scratch: &mut Scratch,
    extend: Option<std::time::Duration>,
    extended: &mut bool,
    clock: &mut HeaderClock<'_>,
    served: usize,
    mut header_bytes: usize,
) -> Result<usize, HeaderStop> {
    loop {
        let index = scratch.used;
        // 要求の途中なので、空振りしても預けない (待ち直す)
        let line = match read_line_or_idle(reader, scratch.next(), extend, false, extended, clock) {
            Ok(Line::Read(v)) => v,
            Ok(Line::Idle) => unreachable!("allow_idle is false"),
            // ヘッダーの途中で期限を越えた・黙った (T.28.0)
            Err(e) if clock.is_header_timeout(&e, served) => return Err(HeaderStop::Timeout),
            Err(e) => return Err(HeaderStop::Io(e)),
        };
        match line {
            None => return Err(HeaderStop::LineTooLong),
            Some(0) => break,
            Some(n) => {
                header_bytes += n;
                if header_bytes > MAX_HEADER_BYTES {
                    return Err(HeaderStop::TooManyBytes);
                }
            }
        }
        if scratch.lines[index].trim().is_empty() {
            break;
        }
        if index >= MAX_HEADER_LINES {
            return Err(HeaderStop::TooManyLines);
        }
        scratch.commit();
    }
    Ok(header_bytes)
}

/// ヘッダーを読み切れなかった要求を断る (431 / 408)。読みの失敗はそのまま返す。
#[cold]
pub(super) fn refuse_headers(peer: Peer<'_>, stop: HeaderStop) -> io::Result<Step> {
    let conn_id = peer.conn_id;
    let why = metrics::BadRequestReason::HeaderTooLarge;
    let reason = "Request Header Fields Too Large";
    match stop {
        HeaderStop::LineTooLong => peer.refuse(why, 431, reason, |more| {
            log_warn!(Some(conn_id), "431 Request Header Fields Too Large{}", more)
        }),
        HeaderStop::TooManyBytes => peer.refuse(why, 431, reason, |more| {
            log_warn!(
                Some(conn_id),
                "431 Request Header Fields Too Large (headers over {} bytes){}",
                MAX_HEADER_BYTES,
                more
            )
        }),
        HeaderStop::TooManyLines => peer.refuse(why, 431, reason, |more| {
            log_warn!(
                Some(conn_id),
                "431 Request Header Fields Too Large (too many lines){}",
                more
            )
        }),
        HeaderStop::Timeout => header_timeout(
            peer.client,
            peer.metrics,
            peer.ip,
            conn_id,
            peer.header_timeout,
        ),
        HeaderStop::Io(e) => Err(e),
    }
}

/// 要求行とヘッダーを読み終える期限 (`PROXY_HEADER_TIMEOUT_SECS`。T.28.0)。
///
/// `SO_RCVTIMEO` は読めるたびに待ち直す「無通信の締め切り」なので、1 バイトずつ送られると
/// 何秒たっても切れない (slowloris)。ここでは **1 バイト目を受け取ってから**の総時間を
/// `Instant` で持ち、ソケットから読む前に残りを読み取りタイムアウトに当てる。
///
/// **先読みバッファに残っているぶんを読むときは何もしない** ([`Timed`] はバッファが空の
/// ときだけ呼ぶ)。要求行とヘッダーが 1 つの塊で届く普通の要求では、時計も `setsockopt` も
/// 1 回も増えない。期限が立つのは「1 バイト目が来たのに、まだ続きをソケットから読む」とき
/// だけで、その時刻を 1 バイト目の時刻の代わりにする (直前の読みが返った直後なので差は小さい)。
///
/// 1 バイト目を待つ読み (1 要求目は accept から、2 要求目以降は keep-alive のアイドル) は
/// ここでは縛らない。1 要求目のその待ちは [`config::first_read_timeout`] を読み取り
/// タイムアウトにして縛る (待ち受けから継承させるので接続ごとの `setsockopt` は増えない)。
pub(super) struct HeaderClock<'s> {
    pub(super) stream: &'s TcpStream,
    /// いまソケットに当たっている読み取りタイムアウト (`Conn::read_timeout` そのもの。
    /// 当て直したらここも書き換えて、次の要求の `wait` の比べ合わせを正しく保つ)
    pub(super) current: &'s mut Option<std::time::Duration>,
    /// `PROXY_HEADER_TIMEOUT_SECS` の実効値。`0` なら何もしない
    pub(super) limit: std::time::Duration,
    /// この要求の 1 バイト目を受け取ったか
    pub(super) started: bool,
    pub(super) deadline: Option<Instant>,
}

impl HeaderClock<'_> {
    /// ソケットから読む直前に呼ぶ。期限を越えていたら `TimedOut`。
    fn before_recv(&mut self) -> io::Result<()> {
        if self.limit.is_zero() || !self.started {
            return Ok(());
        }
        let now = Instant::now();
        let deadline = *self.deadline.get_or_insert(now + self.limit);
        let left = deadline.saturating_duration_since(now);
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "request header timeout",
            ));
        }
        // 当たっている待ちが残りより長い (か無期限) ときだけ縮める。keep-alive の待ちの方が
        // 短ければそちらが先に切れる (今までどおりの「途中で黙った」)
        if self.current.is_none_or(|c| c.is_zero() || c > left) {
            self.stream.set_read_timeout(Some(left))?;
            *self.current = Some(left);
        }
        Ok(())
    }

    /// 読み取りタイムアウトを当て直す (猶予から keep-alive の待ちへ延ばすとき)。
    fn set_timeout(&mut self, t: std::time::Duration) -> io::Result<()> {
        self.stream.set_read_timeout(timeout::for_socket(t))?;
        *self.current = Some(t);
        Ok(())
    }

    /// 期限を越えたか (読みが時間切れで返ったときだけ見る)。
    pub(super) fn expired(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// 時間切れで返った読みを締め切りの扱いにするか。1 要求目は締め切りが効いていれば何でも
    /// (黙ったまま・途中で黙った・期限切れ)、2 要求目以降は期限を越えたときだけ
    /// (1 バイト目を待つ keep-alive のアイドルは今までどおり静かに閉じる)。
    /// 1 要求目の黙ったまま (`started` が偽) は呼ぶ側が 408 を返さずに閉じる (T.33.0)。
    pub(super) fn is_header_timeout(&self, e: &io::Error, served: usize) -> bool {
        is_timeout(e) && !self.limit.is_zero() && (served == 0 || self.expired())
    }
}

pub(super) fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// [`HeaderClock`] を通して読む [`clientio::ClientReader`]。バッファが空のとき
/// (= 次の `fill_buf` がソケットを読む) だけ時計を見る。
struct Timed<'c, 'a, 's> {
    reader: &'c mut clientio::ClientReader<'a>,
    clock: &'c mut HeaderClock<'s>,
}

impl Read for Timed<'_, '_, '_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Timed<'_, '_, '_> {
    #[inline]
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.reader.buffer().is_empty() {
            self.clock.before_recv()?;
        }
        self.reader.fill_buf()
    }

    #[inline]
    fn consume(&mut self, amount: usize) {
        if amount > 0 {
            self.clock.started = true;
        }
        self.reader.consume(amount);
    }
}

/// 長さ制限付きで 1 行読む。制限を超えたら `Ok(None)`。
fn read_limited_line(
    reader: &mut clientio::ClientReader<'_>,
    line: &mut String,
    clock: &mut HeaderClock<'_>,
) -> io::Result<Option<usize>> {
    let n = Timed { reader, clock }
        .take(MAX_LINE as u64)
        .read_line(line)?;
    if n == MAX_LINE && !line.ends_with('\n') {
        return Ok(None);
    }
    Ok(Some(n))
}

pub(super) fn reject(client: &TcpStream, status: u16, reason: &str) -> io::Result<()> {
    let mut client = client;
    // `Date` も付ける (RFC 9110 §6.6.1。T.29.9)
    let mut head = [0u8; crate::httpdate::ERROR_HEAD_MAX];
    client.write_all(crate::httpdate::error_head(status, reason, &mut head))?;
    client.flush()?;
    linger(client);
    Ok(())
}

/// 要求を断る共通の形 (T.34.11): 理由別に 1 件数えて個票にも残し (`/errors`。T14.28)、
/// WARN は理由ごとに 1 分に 1 行 (T.28.9) だけ `warn` に出させ (文面は呼ぶ側が持つ)、
/// `status` を返してから閉じる (無言で切ると客からは「落ちた」と見える)。
///
/// **断る経路だけ**を通るので、通した要求には 1 命令も足さない。
#[cold]
pub(super) fn refuse(
    client: &TcpStream,
    metrics: &Metrics,
    peer_ip: &str,
    why: metrics::BadRequestReason,
    status: u16,
    reason: &str,
    warn: impl FnOnce(crate::log::Skipped),
) -> io::Result<Step> {
    if let Some(more) = metrics.record_bad_request(why, peer_ip, status) {
        warn(more);
    }
    reject(client, status, reason)?;
    Ok(Step::Close(recent::CloseReason::Shutdown))
}

/// 断ったあと、相手がまだ送っている途中で閉じると RST になり、書いた応答ごと捨てられて
/// クライアントは理由が分からない。少しだけ読み捨ててから閉じる (時間もバイト数も上限つきなので、
/// これ自体を居座りに使うことはできない)。
fn linger(client: &TcpStream) {
    let mut client = client;
    let _ = client.set_read_timeout(Some(LINGER_TIMEOUT));
    let mut sink = [0u8; 8 * 1024];
    let mut drained = 0usize;
    while drained < LINGER_BYTES {
        match client.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
}

/// 要求行とヘッダーが期限 (`PROXY_HEADER_TIMEOUT_SECS`) までに揃わなかった接続に
/// `408 Request Timeout` を返して閉じる (T.28.0)。
///
/// 1 要求目の 1 バイト目も来なかった接続 (先張り) はここへ来ない: 408 を返さずに閉じて
/// `/status` の `silent_closes` に数える (T.33.0。`serve_one`)。ここへ来るのは 1 バイト以上
/// 送ってから止まった接続 (ヘッダーの途中で止まる・1 バイトずつ送る slowloris) と、2 要求目以降の
/// 期限切れ。来るたびに出るので、ログは debug に留める
/// (1 本ごとに WARN を出すと、ログの環が攻撃の行で埋まる)。
///
/// 数は `/status` の `rejected_requests.header_timeout` と `/errors` の 1 行 (`bad_request:header_timeout`、
/// 状態 408)、`/recent` の理由は `error:header_timeout` (T.29.10。CONNECT の確立の締め切りの
/// `error:timeout` と分ける)。ログは数えたあとも debug のまま (WARN の絞りの戻り値は捨てる)
pub(super) fn header_timeout(
    client: &TcpStream,
    metrics: &Metrics,
    peer_ip: &str,
    conn_id: usize,
    limit: std::time::Duration,
) -> io::Result<Step> {
    let _ = metrics.record_bad_request(metrics::BadRequestReason::HeaderTimeout, peer_ip, 408);
    log_debug!(
        Some(conn_id),
        "408 Request Timeout (request line and headers not received within {} s)",
        limit.as_secs_f64()
    );
    reject(client, 408, "Request Timeout")?;
    Ok(Step::Close(recent::CloseReason::HeaderTimeout))
}

/// 上限の外で受けた接続 (T13.2) に 503 を返して閉じる。
///
/// 自分宛て (内部エンドポイント) でなければここへ来る。accept のところで返す 503 と
/// 同じ本文で、断った数も同じ `rejected_overload` に数える (下の `per_client` を除く)。
/// `drain` は「要求を読んだあとか」: 本文が届いている途中かもしれないので読み捨ててから
/// 閉じる。1 バイトも届いていない (黙ったままの) 接続では読み捨てるものが無い。
///
/// `per_client` が `Some(接続元)` なら、枠を使ったのは**接続元ごとの上限**
/// (`PROXY_MAX_CONNS_PER_CLIENT`) なので、数える先は `rejected_per_client` と
/// その接続元の個票 (`/clients` の `rejected`。T14.13)。
pub(super) fn overload(
    client: &TcpStream,
    metrics: &Metrics,
    conn_id: usize,
    why: &str,
    drain: bool,
    per_client: Option<&str>,
) -> Step {
    log_debug!(Some(conn_id), "over the connection limit: 503 for {}", why);
    match per_client {
        Some(ip) => metrics.record_client_rejected(ip),
        None => {
            metrics.rejected_overload.fetch_add(1, Ordering::Relaxed);
        }
    }
    let mut client = client;
    let _ = client.write_all(OVERLOAD_RESPONSE);
    let _ = client.flush();
    if drain {
        linger(client);
    }
    // 上限に当たって断った接続はプロキシ側の都合で閉じている (数は `rejected_overload`)
    Step::Close(recent::CloseReason::Shutdown)
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
