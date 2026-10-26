//! 要求 1 つの読み取りから振り分けまで ([`serve_one`])。

use super::*;

use super::header_read::{
    HeaderClock, Line, MAX_HEADER_BYTES, MAX_HEADER_LINES, MAX_LINE, header_timeout, is_timeout,
    overload, read_line_or_idle, reject,
};

/// 要求を 1 つ処理する。次に何をするかを返す。
pub(super) fn serve_one(conn: &mut Conn) -> io::Result<Step> {
    let (conn_id, local_port) = (conn.conn_id, conn.accepted.local_port);
    // 上限の外で受けた接続か (T13.2)。自分宛て (内部エンドポイント) のときだけ応える
    let overflow = conn.overflow.is_some();
    // その枠を使ったのが接続元ごとの上限なら、断った 1 本はそちらに数える (T14.13)
    let over_client = conn.overflow.as_ref().is_some_and(|g| g.per_client);
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
        workers,
        peer_ip,
        proxied,
        slot,
        queue_ms,
        tally,
        ..
    } = conn;
    let peer_ip: &str = peer_ip;
    let tally: &std::cell::Cell<recent::ConnTally> = tally;
    let scratch = scratch.as_mut().expect("just set");
    scratch.reset();

    // 2 回目以降で監視スレッドに預けられるなら、待つのは猶予のあいだだけ。
    // 空振りしたら「暇だ」と解釈して預ける (poll を別に呼ばずに済む)。
    // **`config.timeout` が 0 (無期限) でもここは変わらない**: 猶予は `park_grace` の
    // 長さで、`timeout` とは別物なので、空振り = 「暇だ」の判定はそのまま起きる
    // (T6.5 の預ける仕組みは無期限でも動く。結合テストで確かめてある)
    let grace = (*served > 0
        && park.is_some()
        && !config.park_grace.is_zero()
        && !config.keepalive.is_zero())
    .then_some(config.park_grace);
    // 最初の要求は `PROXY_HEADER_TIMEOUT_SECS` と通常のタイムアウトの小さい方
    // (accept から 1 バイト目までを縛る。T.28.0)、2 回目以降は keep-alive のアイドル時間で待つ。
    // 両方 0 なら 1 要求目は無期限に待つ (= 何も送ってこない接続を閉じない。T10.6。
    // `PROXY_TIMEOUT_SECS=0` だけでは、未設定のヘッダーの締め切り (10 秒) が残る)
    let first = config::first_read_timeout(config.timeout, config.header_timeout);
    let wait = match (grace, *served) {
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
    // タイムアウトの再設定は値が変わるときだけ (setsockopt は要求ごとに効いてくる)。
    // `wait` が 0 なら無期限 (`PROXY_TIMEOUT_SECS=0`。T10.6)
    if *read_timeout != Some(wait) {
        client.set_read_timeout(timeout::for_socket(wait))?;
        *read_timeout = Some(wait);
    }
    // 猶予で待っている間に要求が届き始めたら、本来のアイドル時間まで待ち直す
    let extend = grace.map(|_| config.keepalive);
    let mut extended = false;
    // 要求行とヘッダーを読み終える期限 (T.28.0)。普通の要求では時計を 1 回も読まない
    let mut clock = HeaderClock {
        stream: client,
        current: read_timeout,
        limit: config.header_timeout,
        started: false,
        deadline: None,
    };
    let mut reader = buf.reader(client);
    match read_line_or_idle(
        &mut reader,
        &mut scratch.request_line,
        extend,
        true,
        &mut extended,
        &mut clock,
    ) {
        Ok(Line::Idle) => return Ok(Step::Park),
        Ok(Line::Read(None)) => {
            // 理由別に 1 件数えて個票にも残す (`/errors`。T14.28)。**断る経路だけ**を通るので
            // 通した要求には 1 命令も足していない。WARN は理由ごとに 1 分に 1 行 (T.28.9)
            if let Some(more) =
                metrics.record_bad_request(metrics::BadRequestReason::RequestLine, peer_ip, 414)
            {
                log_warn!(
                    Some(conn_id),
                    "414 URI Too Long (request line over {} bytes){}",
                    MAX_LINE,
                    more
                );
            }
            reject(client, 414, "URI Too Long")?;
            return Ok(Step::Close(recent::CloseReason::Shutdown));
        }
        Ok(Line::Read(Some(0))) => {
            log_debug!(Some(conn_id), "client closed ({} requests served)", *served);
            return Ok(Step::Close(recent::CloseReason::ClientEof));
        }
        Ok(Line::Read(Some(_))) => {}
        // 1 バイト目から期限を越えた (1 バイトずつの trickle。2 要求目以降も)
        Err(e) if is_timeout(&e) && clock.expired() => {
            return header_timeout(client, conn_id, config.header_timeout);
        }
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
            return Ok(Step::Close(recent::CloseReason::KeepaliveTimeout));
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
            return Ok(overload(
                client,
                metrics,
                conn_id,
                "a silent connection",
                false,
                over_client.then_some(peer_ip),
            ));
        }
        // 1 要求目が黙ったまま (accept から `first_read_timeout`) か、途中で黙った (T.28.0)
        Err(e) if clock.is_header_timeout(&e, *served) => {
            return header_timeout(client, conn_id, config.header_timeout);
        }
        Err(e) => return Err(e),
    }
    // ここから `Host` を読み終えるまでが `client_read` の段階 (T14.3 (1))。
    // **起点は要求行が届いたところ**で、接続を開けたまま黙っている時間は含めない
    let read_started = profile::mark();
    // 要求の前の空行は読み飛ばす (RFC 9112 §2.2)
    if scratch.request_line.trim().is_empty() {
        return Ok(Step::Next);
    }
    // 起動時の自己ベンチ (T14.43) が自分で打った要求は `/status` の合計に数えない
    // (20,000 要求が乗ると、0.015 req/s のデプロイ先では合計が自己ベンチだけになる)。
    // 費用は自己ベンチが回っていないときの原子の読み 1 回
    if !selfbench::is_client(peer_ip) {
        metrics.inc_requests();
    }
    log_trace!(
        Some(conn_id),
        "request line: {}",
        scratch.request_line.trim_end()
    );

    let request_line = std::mem::take(&mut scratch.request_line);
    let mut parts = request_line.split_ascii_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        // メソッドが token として読めなければ `method`、読めるなら `request_line` (T14.28)。
        // **個票に要求行そのものは入れない** (壊れた行にも URL が載っているため)。
        // WARN は理由ごとに 1 分に 1 行 (T.28.9)
        if let Some(more) = metrics.record_bad_request(
            metrics::BadRequestReason::of_request_line(&request_line),
            peer_ip,
            400,
        ) {
            log_warn!(
                Some(conn_id),
                "malformed request line: {}{}",
                crate::log::Clip(request_line.trim()),
                more
            );
        }
        // 今までは応答を返さずに閉じていた。理由を数えるからには**返した状態コードを
        // 個票に正しく書く**必要があるので、414 / 431 と同じ形で 400 を返してから閉じる
        reject(client, 400, "Bad Request")?;
        scratch.request_line = request_line;
        return Ok(Step::Close(recent::CloseReason::Shutdown));
    };
    // 版が `HTTP/1.x` でない要求 (`HTTP/9.9`・HTTP/2 の preface `PRI * HTTP/2.0`) は 505 (T27.8)。
    // 今までは `HTTP/9.9` を 1.1 として転送し、preface は `*` を宛先として引きに行って 502 だった。
    // 版の無い要求行 (HTTP/0.9 の形) は今までどおり 1.0 として扱う (`http` 側の既定)
    if let Some(version) = parts.next()
        && !version
            .get(..7)
            .is_some_and(|v| v.eq_ignore_ascii_case("HTTP/1."))
    {
        if let Some(more) =
            metrics.record_bad_request(metrics::BadRequestReason::RequestLine, peer_ip, 505)
        {
            log_warn!(
                Some(conn_id),
                "505 HTTP Version Not Supported: {}{}",
                crate::log::Clip(version),
                more
            );
        }
        reject(client, 505, "HTTP Version Not Supported")?;
        scratch.request_line = request_line;
        return Ok(Step::Close(recent::CloseReason::Shutdown));
    }

    // ヘッダー全体の大きさ (要求行を含む)
    let mut header_bytes = request_line.len();
    // ヘッダーは読むだけで、なめるのは読み終えてからの 1 回 (`parse_request_headers_visit`。T.28.15)
    loop {
        let index = scratch.used;
        // 要求の途中なので、空振りしても預けない (待ち直す)
        let line = match read_line_or_idle(
            &mut reader,
            scratch.next(),
            extend,
            false,
            &mut extended,
            &mut clock,
        ) {
            Ok(Line::Read(v)) => v,
            Ok(Line::Idle) => unreachable!("allow_idle is false"),
            // ヘッダーの途中で期限を越えた・黙った (T.28.0)
            Err(e) if clock.is_header_timeout(&e, *served) => {
                return header_timeout(client, conn_id, config.header_timeout);
            }
            Err(e) => return Err(e),
        };
        match line {
            None => {
                if let Some(more) = metrics.record_bad_request(
                    metrics::BadRequestReason::HeaderTooLarge,
                    peer_ip,
                    431,
                ) {
                    log_warn!(Some(conn_id), "431 Request Header Fields Too Large{}", more);
                }
                reject(client, 431, "Request Header Fields Too Large")?;
                return Ok(Step::Close(recent::CloseReason::Shutdown));
            }
            Some(0) => break,
            Some(n) => {
                header_bytes += n;
                if header_bytes > MAX_HEADER_BYTES {
                    if let Some(more) = metrics.record_bad_request(
                        metrics::BadRequestReason::HeaderTooLarge,
                        peer_ip,
                        431,
                    ) {
                        log_warn!(
                            Some(conn_id),
                            "431 Request Header Fields Too Large (headers over {} bytes){}",
                            MAX_HEADER_BYTES,
                            more
                        );
                    }
                    reject(client, 431, "Request Header Fields Too Large")?;
                    return Ok(Step::Close(recent::CloseReason::Shutdown));
                }
            }
        }
        if scratch.lines[index].trim().is_empty() {
            break;
        }
        if index >= MAX_HEADER_LINES {
            if let Some(more) =
                metrics.record_bad_request(metrics::BadRequestReason::HeaderTooLarge, peer_ip, 431)
            {
                log_warn!(
                    Some(conn_id),
                    "431 Request Header Fields Too Large (too many lines){}",
                    more
                );
            }
            reject(client, 431, "Request Header Fields Too Large")?;
            return Ok(Step::Close(recent::CloseReason::Shutdown));
        }
        scratch.commit();
    }
    let raw_headers = scratch.headers();
    // `User-Agent` は行の添字で覚える (`/clients`。T14.7)。**拾うのは接続の最初の要求だけ**で、
    // 2 要求目からはこの旗が倒れているので、ヘッダー 1 行につき比較 1 回で飛ばせる
    // (`--lite` では記録しないので端から見ない。Phase 14 の共通の決まり)
    let want_agent = *served == 0 && !config.lite;
    let mut agent_line: Option<usize> = None;
    // 受けた Via に自分の印 (起動ごとの 8 桁 16 進) があるか = 自分を通った要求が戻ってきた
    let mut via_loop = false;
    let req = request::parse_request_headers_visit(raw_headers, conn_id, |i, k, v| {
        if !via_loop && k.eq_ignore_ascii_case("via") {
            via_loop = via::is_self(v);
        } else if want_agent && agent_line.is_none() && k.eq_ignore_ascii_case("user-agent") {
            agent_line = Some(i);
        }
    });
    // 本文付きの要求だけ、読み取りタイムアウトを本来の値に戻す (`Content-Length` か
    // `Transfer-Encoding` の行があった。値が読めない `Content-Length` も含む)
    let has_body = req.content_length.is_some() || req.content_length_invalid || req.te_present;
    // 要求行とヘッダーは keep-alive のアイドル時間で待っている。本文を読むならここで戻す
    if has_body && *read_timeout != Some(config.timeout) {
        client.set_read_timeout(timeout::for_socket(config.timeout))?;
        *read_timeout = Some(config.timeout);
    }
    // 接続の個票 (`/recent`) の「上り」に要求のヘッダーぶんを足す (T14.4)。
    // 箱の中の足し算だけで、原子も鍵も無い (`--lite` でも同じ道を通る)
    let mut t = tally.get();
    t.up = t.up.saturating_add(header_bytes as u64);
    tally.set(t);
    let host_header: Option<&str> = req.host.as_deref();
    // この 1 要求ぶんの `queue` は使ったら 0 に戻す (T14.3 (1))。`client_read` の
    // **終わりの時計は読まない**: 下の層 (`http::handle_http_with_headers` と
    // `tunnel::open`) が入口で既に `Instant::now()` を読んでいるので、起点だけ渡して
    // 向こうで引き算させる (要求ごとの時計が 1 回減る)
    let stages = metrics::StageMs {
        queue: std::mem::take(queue_ms),
        ..metrics::StageMs::default()
    };

    // 自分の印の付いた `Via` を受けた = 自分を通った要求が自分に戻ってきた (T12.3 の保険)。
    // 印は起動ごとの乱数なので、rust-http-proxy を 2 段に並べた正当な構成 (別プロセス) は
    // 誤検出しない。同じプロセスの別の待ち受けへ回した形はここで 1 段目で止まる
    if via_loop {
        // 自分の印は外から付けられる (誰かが印を写して送ってくる) ので、これも 1 分に 1 行 (T.28.9)
        static LOOP_WARN: log::Throttle = log::Throttle::new();
        if let Some(more) = LOOP_WARN.pass(log::WARN_EVERY_SECS) {
            log_warn!(
                Some(conn_id),
                "508 Loop Detected (request carries our own Via: {}){}",
                via::token(),
                more
            );
        }
        reject(client, 508, "Loop Detected")?;
        // 原因つきで 1 件数える (T12.4 (2))。ここは要求ターゲットをまだ解いていないので、
        // 鍵は `Host` (無ければ要求ターゲット) から作る。この経路は 1 要求に 1 回しか
        // 通らないので `format!` の確保は熱い経路に乗らない
        let detail = metrics::Detail {
            cause: Some(metrics::ErrCause::Loop),
            ..metrics::Detail::default()
        };
        metrics.record_host_detail(
            &format!("loop://{}", host_header.unwrap_or(target)),
            metrics::HostOutcome::Error,
            0,
            None,
            detail,
        );
        // 個票にも 1 件残す (`/errors`。T13.4)
        metrics.record_error(false, host_header.unwrap_or(target), peer_ip, 508, &detail);
        // この枝は下の `endpoints::handle` より**手前**で return するので、下の
        // `*proxied = true` を通らない。ここも `record_client` で `clients[]` の行を
        // 作る側なので、母数 (T15.0 (12)) にはこの枝も入れる。自分宛ての要求は
        // 自分の `Via` を持たないので、ここへ来るのはプロキシとして受けた要求だけ
        *proxied = true;
        metrics.record_client(
            peer_ip,
            metrics::HostOutcome::Error,
            0,
            // 508 で閉じたので運んだバイトは上りも下りも無い (T14.26)
            (0, 0),
            None,
            Some(host_header.unwrap_or(target)),
        );
        return Ok(Step::Close(recent::CloseReason::Error(
            metrics::ErrCause::Loop,
        )));
    }

    // プロキシ自身のエンドポイント (/dashboard, /status, /metrics, /proxy.pac, /purge, /lookup, PURGE)
    // 上限といまのスレッド数は `/status` のときだけ引く (`Workers` の鍵は全接続スレッドが
    // 共有しているので、要求ごとに数えると熱い経路に乗る)
    let concurrency = || crate::metrics::Concurrency {
        max_conns: config.max_conns,
        max_threads: workers.max_threads(),
        live_threads: workers.live_count(),
        idle_threads: workers.idle_count(),
        queued_jobs: workers.queued(),
    };
    // 書き換える口が外の IP リテラルで名指しされたときだけ呼ばれる (T.28.40)。
    // 自分の IP = この接続を受けたローカルの IP (`getsockname` はここでだけ引く)・
    // `PROXY_BIND` に書いた IP・`SERVER_IP` (パネルが実際の環境変数で渡す割り当ての IP。
    // `.env` の名前ではないので `envfile` ではなく環境変数だけを見る)。未指定 (`0.0.0.0` /
    // `::`) は自分の名前にしない
    let own_ip = |ip: std::net::IpAddr| {
        !ip.is_unspecified()
            && (client
                .local_addr()
                .is_ok_and(|a| a.ip().to_canonical() == ip)
                || config.bind_addrs.contains(&ip)
                || std::env::var("SERVER_IP")
                    .ok()
                    .and_then(|s| s.trim().parse::<std::net::IpAddr>().ok())
                    .is_some_and(|s| s.to_canonical() == ip))
    };
    let ep = endpoints::Endpoint {
        metrics,
        cache,
        conn_id,
        // 実際に受けたポート (テストや複数 bind でも自分宛て判定が合うように)
        port: local_port,
        host: host_header,
        // 内部エンドポイントを引いた接続元を 1 行残すため (`/status` の `readers`。T14.53)。
        // 数えるのは `endpoints::handle` が「自分宛て」と決めたあとなので、
        // プロキシとして通す要求はこの値を 1 度も使わない。**渡すのは生の IP**で、
        // `PROXY_RECORDS` の off / hashed は記録の入口 (`Metrics::record_reader`) が
        // 見る (接続元を記録の形に直す関数は 1 つだけ。T14.41)
        client: Some(peer_ip),
        pac_direct: &config.pac_direct,
        lite: config.lite,
        readonly: config.endpoints_readonly,
        version: VERSION,
        concurrency: &concurrency,
        own_ip: &own_ip,
    };
    if endpoints::handle(&mut &*client, method, target, &ep)? {
        return Ok(Step::Close(recent::CloseReason::Shutdown));
    }
    // ここから下は「自分宛てではない = プロキシとして扱う要求」なので、この接続を
    // `clients[]` の母数に入れる (T15.0 (12))。**代入 1 回だけ**で、原子も鍵も取らない。
    // 立てる場所がここなのは、断る枝 (503 / 403) も CONNECT も `record_client` で
    // 行を作るのに、どれも `*served += 1` を通らないため (508 だけはここより手前で
    // return するので、あちらの枝で立てている)
    *proxied = true;
    // 上限の外で受けた接続で、自分宛てではなかった (T13.2)。判定は上の
    // `endpoints::handle` = T12.3 の `local_path` そのもので、偽ならここへ来る
    if overflow {
        return Ok(overload(
            client,
            metrics,
            conn_id,
            request_line.trim_end(),
            true,
            over_client.then_some(peer_ip),
        ));
    }

    // 接続元の個票に `User-Agent` を写す (`/clients`。T14.7)。ここまで来た要求は
    // 「自分宛てでもなく、上限の外でもない = プロキシとして通す要求」なので、
    // `/status` を引いただけの相手で `clients[]` が増えることはない。
    // **通るのは接続の最初の要求の 1 回だけ** (鍵を取るのも接続 1 本につき 1 回)
    if let Some(i) = agent_line
        && let Some((_, v)) = scratch.lines[i].split_once(':')
    {
        metrics.record_client_agent(peer_ip, v);
    }

    // ACL / Host Check
    let is_connect = method.eq_ignore_ascii_case("CONNECT");
    // CONNECT は `http::handle_http_with_headers` を通らずトンネルの経路に入るので、
    // T19 で入れた要求の枠組みの検査がここへ来ない。**同じ判定をこの入口にも置く** (T27.0)。
    // CONNECT に本文は無く (RFC 9110 §9.3.6)、`Host` の重複 (RFC 9110 §7.2)・obs-fold
    // (RFC 9112 §5.2)・本文の枠組みをそのままにしてトンネルを張ると、ヘッダーの後ろに
    // 付けたバイトが**トンネルの最初のデータとして宛先へ流れる** (前段がいる構成では要求の密輸)。
    // ここで断れば宛先へは接続もしない (`Step::Connect` に進まない) ので、1 バイトも届かない。
    if is_connect {
        let h = &req;
        let reason = if h.dup_host {
            Some(metrics::BadRequestReason::DupHost)
        } else if h.obs_fold {
            Some(metrics::BadRequestReason::ObsFold)
        } else if h.bad_header {
            // 名前とコロンの間の空白・値の CR / LF / NUL (T27.5)
            Some(metrics::BadRequestReason::BadHeader)
        } else if has_body || h.content_length_invalid {
            // `has_body` はヘッダーを読むループが `Content-Length` / `Transfer-Encoding` を
            // 見たときに立つ旗 (本文を読むため)。値が数字でない `Content-Length` も
            // ここに含まれる (T19.1 と同じ扱い = `body_framing`)
            Some(metrics::BadRequestReason::BodyFraming)
        } else {
            None
        };
        if let Some(reason) = reason {
            if let Some(more) = metrics.record_bad_request(reason, peer_ip, 400) {
                log_warn!(
                    Some(conn_id),
                    "400 Bad Request: {} in CONNECT {}{}",
                    reason.name(),
                    target,
                    more
                );
            }
            // ステータスを返してから閉じる (無言で切ると客からは「落ちた」と見える)
            reject(client, 400, "Bad Request")?;
            return Ok(Step::Close(recent::CloseReason::Shutdown));
        }
    }
    // 判定に要るのはホストだけなので、要求行から借りる (String を 2 本作らない)
    let target_host: &str = if is_connect {
        target
    } else {
        match request::target_host(target, host_header) {
            Ok(h) => h,
            Err(e) => {
                // `Host` が無い (`no_host`) のか絶対 URI が壊れている (`bad_uri`) のかを
                // 分けて数える (T14.28)。判定は**断る経路だけ**を通る
                if let Some(more) = metrics.record_bad_request(
                    metrics::BadRequestReason::of_target(method, target),
                    peer_ip,
                    400,
                ) {
                    log_warn!(Some(conn_id), "400 Bad Request: {}{}", e, more);
                }
                let _ = (&*client).write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return Ok(Step::Close(recent::CloseReason::Shutdown));
            }
        }
    };
    let (bare_host, host_port) = net::split_host_port_ref(target_host);
    // CONNECT の宛先は `host:port` の形でなければならない (RFC 9110 §9.3.6)。ポートが無い・読めない・0 の
    // ときは今まで 443 とみなすか 0 番へ繋ぎに行って 502 だった。クライアントの誤りなので 400 (T27.8)
    if is_connect && (bare_host.is_empty() || !host_port.is_some_and(|p| p != 0)) {
        if let Some(more) =
            metrics.record_bad_request(metrics::BadRequestReason::BadUri, peer_ip, 400)
        {
            log_warn!(
                Some(conn_id),
                "400 Bad Request: CONNECT target without a port: {}{}",
                target,
                more
            );
        }
        reject(client, 400, "Bad Request")?;
        return Ok(Step::Close(recent::CloseReason::Shutdown));
    }
    // ローカル宛ての判定で引いた答え (接続でもう一度引かないために持ち回す。T12.7)
    let mut resolved: Option<dns::Resolved> = None;
    let denied = if !config.acl.is_allowed(target_host) {
        Some(metrics::BlockCause::Acl)
    } else if blocklist::is_blocked(bare_host) {
        Some(metrics::BlockCause::Blocklist)
    } else if is_connect && !config.connect_ports.allows(host_port.unwrap_or(443)) {
        Some(metrics::BlockCause::ConnectPort)
    } else if config.allow_local != acl::AllowLocal::On {
        // クラウドのメタデータ (169.254.169.254) 経由の SSRF を止める。
        // **判定に使った答えはそのまま接続へ渡す** (名前解決は 1 要求 1 回。T12.7)。
        // 名前解決の費用はここで払うので、前の要求がこのスレッドに残した分は
        // 解決の前に捨てる (次の `take` でこの要求のぶんだけが取れるように。T12.4 (2))
        let _ = dns::take_resolve_cost();
        let _ = dns::take_family();
        let _ = dns::take_syn_retrans();
        // `private` なら私設アドレスは通す (ループバック・リンクローカル・未指定は断る。T.28.41)
        let (local, r) = acl::resolve_target(target_host, config.allow_local.allows_private());
        resolved = r;
        local.then_some(metrics::BlockCause::Local)
    } else {
        None
    };
    if let Some(why) = denied {
        // 403 も宛先を選ぶのは相手なので、理由ごとに 1 分に 1 行 (T.28.9)。本数は
        // `/status` の宛先・接続元ごとの `blocked` と `/errors` の個票が持つ
        static BLOCK_WARN: [log::Throttle; 4] = [const { log::Throttle::new() }; 4];
        let gate = match why {
            metrics::BlockCause::Acl => 0,
            metrics::BlockCause::Blocklist => 1,
            metrics::BlockCause::ConnectPort => 2,
            metrics::BlockCause::Local => 3,
        };
        if let Some(more) = BLOCK_WARN[gate].pass(log::WARN_EVERY_SECS) {
            log_warn!(
                Some(conn_id),
                "403 Forbidden ({} blocked host: {}){}",
                why.label(),
                target_host,
                more
            );
        }
        metrics.record_host(
            &format!("blocked://{}", bare_host),
            metrics::HostOutcome::Blocked,
            0,
        );
        metrics.record_client(
            peer_ip,
            metrics::HostOutcome::Blocked,
            0,
            // 403 で断ったので運んだバイトは上りも下りも無い (T14.26)
            (0, 0),
            None,
            Some(target_host),
        );
        // 個票にも 1 件残す (`/errors`。T14.2 (4))。403 は集計では `blocked` に数えてあり、
        // `errors_by_cause` には乗らないので、**誰が何を拒否されたか**はここでしか読めない
        metrics.record_blocked(is_connect, target_host, peer_ip, why);
        (&*client).write_all(FORBIDDEN_RESPONSE)?;
        (&*client).flush()?;
        return Ok(Step::Close(recent::CloseReason::Shutdown));
    }

    if is_connect {
        // 先読みしてしまったバイト (TLS ClientHello など) はトンネルへ渡す。
        // 同時に読み取りバッファを手放す (トンネルの間は要求として読まないので、
        // アイドルのトンネルを大量に抱えるときの資源が減る)
        let prefix = reader.take_buffered();
        let (addrs, preferred) = match resolved {
            Some(r) => {
                let preferred = r.preferred();
                (Some(r.into_addrs()), preferred)
            }
            None => (None, None),
        };
        return Ok(Step::Connect {
            target: target.to_string(),
            prefix,
            addrs,
            preferred,
            stages,
            read_started,
        });
    }

    // keep-alive の HTTP 接続の宛先を `/connections` に出す (T14.2 (5))。
    // **書くのは接続の最初の要求のときだけ** (2 本目以降は宛先が変わりうるが、
    // 要求ごとに表を触らない方針は T13.4 のまま)。CONNECT はトンネルを開くときに書く。
    //
    // 宛先は CONNECT と同じ `host:port` の形にそろえる (T14.48)。ポートを書かない要求
    // (`http://example.com/`) には、ホスト別統計の鍵 (`pool_key` = `scheme://host:port`) と
    // **同じ規則**でスキームの既定 (http なら 80) を補うので、個票を読む道具は
    // 「ポートのある形」と「無い形」の 2 つを扱わなくてよい。確保が要るのはポートを
    // 省いたときだけで、通るのは接続の最初の要求の 1 回
    if *served == 0
        && let Some(slot) = slot.as_ref()
    {
        match host_port {
            Some(_) => slot.set_first_target(target_host),
            None => slot.set_first_target(&net::join_host_port(
                bare_host,
                request::target_scheme(target).default_port(),
            )),
        }
    }
    // この接続で処理する最後の要求か (`Config::max_requests_per_conn`。T14.2)。
    // `http` 側はこれが立っていると応答に `Connection: close` を付け、`keep` に false を返す
    let last = *served + 1 >= config.max_requests_per_conn;
    let shared = http::Shared {
        timeout: config.timeout,
        // forward のオリジン接続の確立も CONNECT と同じ締め切り (T.28.8)
        connect_timeout: config.connect_timeout,
        keepalive: config.keepalive,
        conn_id,
        metrics: Arc::clone(metrics),
        cache: Arc::clone(cache),
        upstream: Arc::clone(upstream),
        // 裏側の再検証もこの置き場で走らせる (T11.3)。借りるだけなので要求あたりの費用は 0
        workers: &*workers,
        // プールに無くて繋ぎに行くときは、判定で引いた答えを使う (T12.7)
        resolved: resolved.as_ref(),
        last,
        stages,
        read_started,
        // 閉じた接続の個票に積み上げる箱 (T14.4)。`--lite` では枠が無いので渡さない
        tally: slot.is_some().then_some(tally),
        // 追跡中の接続元か (`PROXY_TRACE_CLIENT`。T14.27)。要求ごとに読むのはこの
        // 旗 1 つだけで、立っていない要求は `Ctx::log` の分岐 1 回で飛ばす
        traced: slot.as_ref().is_some_and(|s| s.traced()),
    };
    let keep = http::handle_http_parsed(
        client,
        Some(peer_ip),
        &request_line,
        raw_headers,
        &req,
        &mut reader,
        &shared,
    )?;
    scratch.request_line = request_line;
    *served += 1;
    // `last` が立っていれば `keep` は必ず false (応答に `Connection: close` が付いている)
    if !keep {
        // 1,000 要求で打ち切ったのか、クライアントが `Connection: close` を求めたのか
        // (どちらも `/recent` の理由になる。T14.4)
        Ok(Step::Close(if last {
            recent::CloseReason::Limit
        } else {
            recent::CloseReason::ClientEof
        }))
    } else {
        // 応答を書き終えて**次の要求を待つ直前**に、この接続の累計バイトを
        // `/connections` の枠へ 1 回だけ写す (T24.5。T13.4 の「要求ごとに書かない」は
        // そのまま — 書くのは待ちに入るこの 1 か所だけ)。値は「この接続で終わった
        // 要求のバイトの和」なので、**いま処理中の 1 要求は入らない** (要求 1 本ぶん
        // 遅れる)。`tally` は要求ごとに箱の中で足しているだけで、原子に移すのはここ
        if let Some(slot) = slot.as_ref() {
            let t = tally.get();
            slot.set_bytes(t.up.saturating_add(t.down));
        }
        Ok(Step::Next)
    }
}
