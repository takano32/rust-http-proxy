//! `/recent` — **閉じた接続**の個票 (T14.4)。T.34.82 で recent.rs から分けた。
//!
//! 並べ替え ([`RecentSort`])・接続元の RTT での絞り ([`RttClient`]) と、口の本体
//! ([`recent`] / 停止の雪像も使う [`recent_within`])。

use std::fmt::Write as _;

use super::super::{next_offset, offset_param, param, parse_query};
use super::{Endpoint, MAX_BODY, array_within_budget, num_param, persisted, str_param};
use crate::metrics::SCHEMA_HEAD;
use crate::recent::{MAX_RECENT, RecentEntry};

/// `/recent` の並べ替えの鍵。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RecentSort {
    /// 閉じた新しい順 (既定)
    Time,
    /// 確立にかかった ms の大きい順 (「遅かった 1 本」を探す)
    Slow,
    /// 運んだバイトの多い順
    Bytes,
}

impl RecentSort {
    fn from_param(v: &str) -> RecentSort {
        match v {
            "slow" => RecentSort::Slow,
            "bytes" => RecentSort::Bytes,
            _ => RecentSort::Time,
        }
    }

    fn name(self) -> &'static str {
        match self {
            RecentSort::Time => "time",
            RecentSort::Slow => "slow",
            RecentSort::Bytes => "bytes",
        }
    }
}

/// 接続元のカーネルの RTT で個票を 2 つに割る閾 (us。`/recent?rtt_client=lt100|ge100`。T.29.18)。
///
/// tokyo の接続元は NAT の 1 つに見えるが、RTT で 2 集団に割れる (近い 21% = RTT p50 約 20 ms、
/// 遠い 79% = 約 245 ms。first_relay p50 は 20 ms と 4.1 秒)。100 ms は 2 つの山の間。
pub(super) const RTT_CLIENT_SPLIT_US: u32 = 100_000;

/// `/recent?rtt_client=` の値 (T.29.18)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RttClient {
    /// 絞らない (既定)
    Any,
    /// 接続元の RTT が 100 ms 未満 (RTT が読めなかった行は入れない)
    Lt100,
    /// 100 ms 以上
    Ge100,
}

impl RttClient {
    fn from_param(v: &str) -> RttClient {
        match v {
            "lt100" => RttClient::Lt100,
            "ge100" => RttClient::Ge100,
            _ => RttClient::Any,
        }
    }

    fn name(self) -> &'static str {
        match self {
            RttClient::Any => "",
            RttClient::Lt100 => "lt100",
            RttClient::Ge100 => "ge100",
        }
    }

    /// その行を残すか (`rtt_us` が 0 = 読めなかった行は、絞るときはどちらにも入れない)。
    pub(super) fn keeps(self, e: &RecentEntry) -> bool {
        let rtt = e.rtt_us[crate::recent::CLIENT_SIDE];
        match self {
            RttClient::Any => true,
            RttClient::Lt100 => rtt > 0 && rtt < RTT_CLIENT_SPLIT_US,
            RttClient::Ge100 => rtt >= RTT_CLIENT_SPLIT_US,
        }
    }
}

/// `/recent?n=200&offset=0&since=<epoch>&client=<ip>&sort=time|slow|bytes&rtt_client=lt100|ge100` — **閉じた接続**の個票
/// (既定 200 件・最大 [`MAX_RECENT`]。T14.4)。
///
/// `/connections` は「いま」しか見えず、`/errors` は失敗だけ。ここは閉じた接続 1 本ごとの
/// 記録なので、**バーストのとき誰が何を開いたか**も**遅かった 1 本がどの段階で遅かったか**も
/// 後から読める。書くのは接続の終了で 1 回だけ (`ConnSlot` の抹消と同じ場所)。
///
/// **`?offset=` (T15.0 (11))**: 2,000 件は 1 件 424 B で [`MAX_BODY`] の 3.2 倍あり、
/// 1 枚では 615 件しか返らない。`offset` は**いまの並び** (`sort=` のあと。既定は
/// **閉じた新しい順**) を何本飛ばすかで、`next_offset` が次の頁の `offset`
/// (続きが無ければ `null`)。**`id` は accept 順・`at` は開いた時刻**でどちらも並びとは
/// 一致しない (長生きのトンネルは若い `id` で最後に閉じる) ので、頁を跨ぐ目印には使えない。
/// 取っている間に新しく閉じた接続があると並びがずれて**同じ行が 2 つの頁に出る**ことは
/// あるが (**欠けることは無い**)、行に `id` があるので読む側で落とせる。
pub(in crate::endpoints) fn recent(
    ep: &Endpoint<'_>,
    query: Option<&str>,
) -> (u16, &'static str, String) {
    recent_within(ep, query, MAX_BODY)
}

/// [`recent`] の本体。`limit` は応答の上限 (ふだんは [`MAX_BODY`]、停止の雪像だけ
/// [`MAX_SNAPSHOT`]。T.28.20)。
pub(super) fn recent_within(
    ep: &Endpoint<'_>,
    query: Option<&str>,
    limit: usize,
) -> (u16, &'static str, String) {
    let n = num_param(query, "n", 200, MAX_RECENT);
    let offset = offset_param(query, MAX_RECENT);
    let since = param(&parse_query(query.unwrap_or("")), "since")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let client = str_param(query, "client");
    let sort = RecentSort::from_param(&str_param(query, "sort"));
    // 接続元の RTT で 2 集団に割る (T.29.18。NAT の向こうの近い利用者と遠い利用者)
    let rtt_client = RttClient::from_param(&str_param(query, "rtt_client"));
    // 絞りはリングの鍵の内側で済ませる (2,000 件を複製してから捨てない)
    let (mut rows, total) = ep.metrics.closed.select(since, &client);
    if rtt_client != RttClient::Any {
        rows.retain(|e| rtt_client.keeps(e));
    }
    let matched = rows.len();
    match sort {
        // `select` が既に新しい順で返している
        RecentSort::Time => {}
        // `sort_by_key` は安定なので、同点は `select` が返した新しい順のまま
        RecentSort::Slow => rows.sort_by_key(|e| std::cmp::Reverse(e.connect_ms())),
        RecentSort::Bytes => rows.sort_by_key(|e| std::cmp::Reverse(e.bytes())),
    }
    let mut out = String::with_capacity(8192);
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"recent\":");
    let (shown, cut) = array_within_budget(
        &mut out,
        rows.iter().skip(offset).take(n).map(RecentEntry::to_json),
        limit,
    );
    // 新しい欄は**末尾**に足す (既存の鍵の順は変えない)
    let _ = write!(
        out,
        ",\"count\":{},\"matched\":{},\"shown\":{},\"kept\":{},\"capacity\":{},\"recorded\":{},\"sort\":\"{}\",\"since\":{},\"client\":\"{}\",\"persisted\":{},\"restored\":{},\"truncated\":{},\"lite\":{},\"offset\":{},\"next_offset\":{},\"rtt_client\":\"{}\"}}",
        shown,
        matched,
        shown,
        ep.metrics.closed.len(),
        MAX_RECENT,
        total,
        sort.name(),
        since,
        crate::json::escape(&client),
        persisted(ep),
        ep.metrics.closed.restored(),
        cut,
        !ep.metrics.conns.enabled(),
        offset,
        next_offset(offset, shown, matched),
        rtt_client.name()
    );
    (200, "application/json", out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recent::{CLIENT_SIDE, CloseReason, ORIGIN_SIDE, SIDES, STAGES};

    /// 接続元の RTT だけを決めた個票。
    fn entry(client_rtt_us: u32) -> RecentEntry {
        let mut rtt_us = [0u32; SIDES];
        rtt_us[CLIENT_SIDE] = client_rtt_us;
        // オリジン側の RTT は絞りに効かない
        rtt_us[ORIGIN_SIDE] = 999_999;
        RecentEntry {
            id: 1,
            at: 0,
            client: "203.0.113.9".to_string(),
            target: "example.com:443".to_string(),
            connect: true,
            secs: 0,
            requests: 0,
            up: 0,
            down: 0,
            reason: CloseReason::ClientEof,
            status: 0,
            parked_secs: 0,
            parks: 0,
            stage_ms: [0; STAGES],
            rtt_us,
            retrans: [0; SIDES],
            sni: None,
            syn_retrans: 0,
            stall_ms: [0; SIDES],
            spins: 0,
            half_closed: None,
        }
    }

    /// `?rtt_client=` は接続元の RTT を 100 ms で 2 つに割り、読めなかった行 (0) は
    /// 絞るときはどちらにも入れない (T.34.82)。
    #[test]
    fn rtt_client_keeps_split_at_100_ms() {
        let unread = entry(0);
        let near = entry(RTT_CLIENT_SPLIT_US - 1);
        let edge = entry(RTT_CLIENT_SPLIT_US);
        for e in [&unread, &near, &edge] {
            assert!(RttClient::Any.keeps(e), "絞らなければ全部残す");
        }
        assert!(!RttClient::Lt100.keeps(&unread));
        assert!(RttClient::Lt100.keeps(&near));
        assert!(
            !RttClient::Lt100.keeps(&edge),
            "100 ms ちょうどは ge100 の側"
        );
        assert!(!RttClient::Ge100.keeps(&unread));
        assert!(!RttClient::Ge100.keeps(&near));
        assert!(RttClient::Ge100.keeps(&edge));
        // 値の読みと名前は往復する (読めない値は絞らない)
        for v in ["lt100", "ge100"] {
            assert_eq!(RttClient::from_param(v).name(), v);
        }
        assert_eq!(RttClient::from_param("lt50"), RttClient::Any);
        assert_eq!(RttClient::Any.name(), "");
    }
}
