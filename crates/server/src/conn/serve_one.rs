//! 要求 1 つの読み取りから振り分けまで ([`serve_one`])。

use super::*;

use super::access::{access_decision, loop_detected, refuse_blocked};
use super::connect_check::{connect_header_reason, connect_target_ok};
use super::first_line::{Peer, first_line_wait, read_request_line, refuse_request_line};
use super::forward::{ForwardParts, first_target, forward_request, shared_for};
use super::header_read::{HeaderClock, overload, read_headers, refuse_headers};
use super::request_line::check_request_line;

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

    let (grace, wait) = first_line_wait(config, *served, park.is_some(), overflow);
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
    let peer = Peer {
        client,
        metrics,
        ip: peer_ip,
        conn_id,
        served: *served,
        overflow,
        over_client,
        header_timeout: config.header_timeout,
    };
    if let Some(step) = read_request_line(
        &mut reader,
        &mut scratch.request_line,
        extend,
        &mut extended,
        &mut clock,
        peer,
    ) {
        return step;
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
    let (method, target) = match check_request_line(&request_line) {
        Ok(l) => (l.method, l.target),
        Err(e) => {
            let step = refuse_request_line(peer, &request_line, e)?;
            scratch.request_line = request_line;
            return Ok(step);
        }
    };

    // ヘッダー全体の大きさ (要求行を含む)
    let header_bytes = match read_headers(
        &mut reader,
        scratch,
        extend,
        &mut extended,
        &mut clock,
        *served,
        request_line.len(),
    ) {
        Ok(n) => n,
        Err(stop) => return refuse_headers(peer, stop),
    };
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

    // 自分の印の付いた `Via` を受けた = 自分を通った要求が自分に戻ってきた (T12.3 の保険)
    if via_loop {
        return loop_detected(peer, host_header, target, proxied);
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
        build: BUILD,
        concurrency: &concurrency,
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
    // メソッドは大小を区別する (RFC 9110 §9.1。T.32.15 (6)。T.32.8 と同じ)。`connect` は未知のメソッド
    let is_connect = method == "CONNECT";
    // CONNECT の枠組みの検査 (T27.0)。断れば宛先へは接続もしない
    if is_connect && let Some(reason) = connect_header_reason(&req, has_body) {
        // ステータスを返してから閉じる (無言で切ると客からは「落ちた」と見える)
        return peer.refuse(reason, 400, "Bad Request", |more| {
            log_warn!(
                Some(conn_id),
                "400 Bad Request: {} in CONNECT {}{}",
                reason.name(),
                target,
                more
            )
        });
    }
    // 判定に要るのはホストだけなので、要求行から借りる (String を 2 本作らない)
    let target_host: &str = if is_connect {
        target
    } else {
        match request::target_host(target, host_header) {
            Ok(h) => h,
            Err(e) => {
                // `Host` が無い (`no_host`) のか絶対 URI が壊れている (`bad_uri`) のかを
                // 分けて数える (T14.28)。判定は**断る経路だけ**を通る。
                // ここだけは応答を書けなくても閉じるだけ (元から `let _ = reject(..)`)
                return peer
                    .refuse(
                        metrics::BadRequestReason::of_target(method, target),
                        400,
                        "Bad Request",
                        |more| log_warn!(Some(conn_id), "400 Bad Request: {}{}", e, more),
                    )
                    .or(Ok(Step::Close(recent::CloseReason::Shutdown)));
            }
        }
    };
    let (bare_host, host_port) = net::split_host_port_ref(target_host);
    // CONNECT の宛先は `host:port` の形でなければならない (RFC 9110 §9.3.6。T27.8)
    if is_connect && !connect_target_ok(bare_host, host_port) {
        return peer.refuse(
            metrics::BadRequestReason::BadUri,
            400,
            "Bad Request",
            |more| {
                log_warn!(
                    Some(conn_id),
                    "400 Bad Request: CONNECT target without a port: {}{}",
                    target,
                    more
                )
            },
        );
    }
    // ローカル宛ての判定で引いた答え (接続でもう一度引かないために持ち回す。T12.7)
    let (denied, resolved) = access_decision(config, target_host, bare_host, host_port, is_connect);
    if let Some(why) = denied {
        return refuse_blocked(peer, why, target_host, bare_host, is_connect);
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

    // keep-alive の HTTP 接続の宛先を `/connections` に出す (接続の最初の要求だけ。T14.2 (5))
    if *served == 0
        && let Some(slot) = slot.as_ref()
    {
        first_target(slot, target, target_host, bare_host, host_port);
    }
    // この接続で処理する最後の要求か (`Config::max_requests_per_conn`。T14.2)
    let last = *served + 1 >= config.max_requests_per_conn;
    let parts = ForwardParts {
        config,
        conn_id,
        metrics,
        cache,
        upstream,
        workers,
        slot: slot.as_ref(),
        tally,
    };
    let shared = shared_for(parts, resolved.as_ref(), last, stages, read_started);
    let step = forward_request(
        peer,
        &request_line,
        raw_headers,
        &req,
        &mut reader,
        &shared,
        slot.as_ref(),
    )?;
    scratch.request_line = request_line;
    *served += 1;
    Ok(step)
}
