//! 転送の準備 ([`first_target`]・[`shared_for`]) と、転送して要求のあとの始末を決める
//! ([`forward_request`])。T34.15

use super::*;

use super::first_line::Peer;

/// keep-alive の HTTP 接続の宛先を `/connections` に出す (T14.2 (5))。
///
/// **書くのは接続の最初の要求のときだけ** (2 本目以降は宛先が変わりうるが、
/// 要求ごとに表を触らない方針は T13.4 のまま)。CONNECT はトンネルを開くときに書く。
///
/// 宛先は CONNECT と同じ `host:port` の形にそろえる (T14.48)。ポートを書かない要求
/// (`http://example.com/`) には、ホスト別統計の鍵 (`pool_key` = `scheme://host:port`) と
/// **同じ規則**でスキームの既定 (http なら 80) を補うので、個票を読む道具は
/// 「ポートのある形」と「無い形」の 2 つを扱わなくてよい。確保が要るのはポートを
/// 省いたときだけで、通るのは接続の最初の要求の 1 回
#[cold]
pub(super) fn first_target(
    slot: &recent::ConnSlot,
    target: &str,
    target_host: &str,
    bare_host: &str,
    host_port: Option<u16>,
) {
    match host_port {
        Some(_) => slot.set_first_target(target_host),
        None => slot.set_first_target(&net::join_host_port(
            bare_host,
            request::target_scheme(target).default_port(),
        )),
    }
}

/// [`http::Shared`] を組むのに要る、この接続の持ち物 (借りるだけ)。
pub(super) struct ForwardParts<'a> {
    pub(super) config: &'a Config,
    pub(super) conn_id: usize,
    pub(super) metrics: &'a Arc<Metrics>,
    pub(super) cache: &'a Arc<Cache>,
    pub(super) upstream: &'a Arc<Upstream>,
    pub(super) workers: &'a Arc<workers::Workers>,
    /// `/connections` の枠 (`--lite` では無い)
    pub(super) slot: Option<&'a Arc<recent::ConnSlot>>,
    /// 閉じた接続の個票に積み上げる箱 (T14.4)
    pub(super) tally: &'a std::cell::Cell<recent::ConnTally>,
}

/// 転送の 1 要求ぶんの [`http::Shared`] を組む。
///
/// `last` はこの接続で処理する最後の要求か (`Config::max_requests_per_conn`。T14.2)。
/// `http` 側はこれが立っていると応答に `Connection: close` を付け、`keep` に false を返す。
#[inline(always)]
pub(super) fn shared_for<'a>(
    p: ForwardParts<'a>,
    resolved: Option<&'a dns::Resolved<'a>>,
    last: bool,
    stages: metrics::StageMs,
    read_started: Option<Instant>,
) -> http::Shared<'a> {
    let ForwardParts {
        config,
        conn_id,
        metrics,
        cache,
        upstream,
        workers,
        slot,
        tally,
    } = p;
    http::Shared {
        timeout: config.timeout,
        // forward のオリジン接続の確立も CONNECT と同じ締め切り (T28.8)
        connect_timeout: config.connect_timeout,
        // 転送中の最小の速さ (T29.34)。見張りは `http` が要求ごとに作る
        min_transfer: &config.min_transfer,
        keepalive: config.keepalive,
        conn_id,
        metrics: Arc::clone(metrics),
        cache: Arc::clone(cache),
        upstream: Arc::clone(upstream),
        // 裏側の再検証もこの置き場で走らせる (T11.3)。借りるだけなので要求あたりの費用は 0
        workers,
        // プールに無くて繋ぎに行くときは、判定で引いた答えを使う (T12.7)
        resolved,
        last,
        stages,
        read_started,
        // 閉じた接続の個票に積み上げる箱 (T14.4)。`--lite` では枠が無いので渡さない
        tally: slot.is_some().then_some(tally),
        // 追跡中の接続元か (`PROXY_TRACE_CLIENT`。T14.27)。要求ごとに読むのはこの
        // 旗 1 つだけで、立っていない要求は `Ctx::log` の分岐 1 回で飛ばす
        traced: slot.is_some_and(|s| s.traced()),
    }
}

/// 要求を転送し、次に何をするか (閉じるか、次の要求を待つか) を返す。
///
/// 転送の前後の始末 (要求行の置き場を戻す・処理した数を足す) は呼ぶ側が持つ。
#[inline(always)]
pub(super) fn forward_request(
    peer: Peer<'_>,
    request_line: &str,
    raw_headers: &[String],
    req: &request::RequestHeaders,
    reader: &mut clientio::ClientReader<'_>,
    shared: &http::Shared<'_>,
    slot: Option<&Arc<recent::ConnSlot>>,
) -> io::Result<Step> {
    let keep = http::handle_http_parsed(
        peer.client,
        Some(peer.ip),
        request_line,
        raw_headers,
        req,
        reader,
        shared,
    )?;
    // `last` が立っていれば `keep` は必ず false (応答に `Connection: close` が付いている)
    if !keep {
        // 1,000 要求で打ち切ったのか、クライアントが `Connection: close` を求めたのか
        // (どちらも `/recent` の理由になる。T14.4)
        Ok(Step::Close(if shared.last {
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
        if let Some(slot) = slot
            && let Some(tally) = shared.tally
        {
            let t = tally.get();
            slot.set_bytes(t.up.saturating_add(t.down));
        }
        Ok(Step::Next)
    }
}
