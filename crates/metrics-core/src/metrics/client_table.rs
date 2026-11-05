//! 接続元 IP ごとの個票の表 (`/clients`。T.34.26 で `metrics.rs` から移した)。
//!
//! 行を動かしただけで中身は変えていない。

use super::*;

impl Metrics {
    /// 接続元 IP ごとに 1 要求を数える。
    ///
    /// `target` はこの要求の宛先 (`scheme://host:port` / `host:port`。分からなければ
    /// `None`)。宛先の種類・ポート・IP リテラルは**この関数が既に取っている鍵の内側**で
    /// 数える (原子操作もシステムコールも増やさない。T14.7)。
    ///
    /// 壁時計は自分で読む。要求の経路はホスト別の記録が読んだ時刻を渡す
    /// [`record_client_at`](Self::record_client_at) を使う (T.34.146)。
    pub fn record_client(
        &self,
        client: &str,
        outcome: HostOutcome,
        bytes: u64,
        dir: (u64, u64),
        took: Option<Duration>,
        target: Option<&str>,
    ) {
        self.record_client_at(
            client,
            outcome,
            bytes,
            dir,
            took,
            target,
            crate::cache::now_epoch(),
        );
    }

    /// [`record_client`](Self::record_client) の、壁時計 (epoch 秒) を受け取る版 (T.34.146)。
    ///
    /// `now` は同じ要求の [`record_host`](Self::record_host) / [`record_host_detail`](Self::record_host_detail)
    /// が返した値。`Metrics::record` は壁時計を 1 回だけ読む約束なのに、今までは接続元の
    /// 個票 ([`ClientStats::count`]) がもう 1 回読んでいた (要求 1 本で 2 回)。
    #[allow(clippy::too_many_arguments)]
    pub fn record_client_at(
        &self,
        client: &str,
        outcome: HostOutcome,
        bytes: u64,
        dir: (u64, u64),
        took: Option<Duration>,
        target: Option<&str>,
        now: u64,
    ) {
        // ホスト別と同じ理由で、自己ベンチのぶんは `/clients` にも入れない (T14.43)
        if target.is_some_and(crate::selfbench::is_target) {
            return;
        }
        // 記録の一括 off とハッシュ化 (T14.41)。`off` は鍵も取らずに戻る
        // (ホスト別の統計 `/hosts` はこの上の `record_host*` なので残る)
        if !crate::records::recording() {
            return;
        }
        self.with_client(client, |stats| {
            stats.count(now, outcome, bytes, dir, took, target)
        });
    }

    /// 接続元の行を取るか作って、`f` を 1 回呼ぶ (T.34.35。`record_client`・`record_client_agent`・
    /// `record_client_rejected` の 3 か所が同じ形で持っていたもの)。
    ///
    /// 鍵は接続元の表の 1 回だけ。**既にある行はキーを作り直さない** (毎要求の `String` 確保をしない)。
    /// 表が [`MAX_CLIENTS`] で満杯なら `other` の行に畳む。記録の一括 off (`records::recording()`) は
    /// 呼ぶ側が先に見ておくこと (ここは記録の形 (`client_key`) に直すところから)。
    #[inline]
    fn with_client(&self, client: &str, f: impl FnOnce(&mut ClientStats)) {
        let client = &crate::records::client_key(client);
        let mut clients = self.clients.locked();
        if let Some(stats) = clients.get_mut(client.as_ref()) {
            f(stats);
            return;
        }
        let key = if clients.len() >= MAX_CLIENTS {
            "other".to_string()
        } else {
            client.to_string()
        };
        f(clients.entry(key).or_insert_with(ClientStats::now));
    }

    /// 接続元 (クライアント側) のカーネルの RTT と再送を 1 標本足す (`/clients`。T14.5)。
    ///
    /// 利用者 → プロキシの往復がここで初めて数字になる。[`record_host_rtt`](Self::record_host_rtt)
    /// と同じく**接続の終わりだけ**で、鍵は `record_client` のものと同じ 1 つ。
    pub fn record_client_rtt(&self, client: &str, rtt_us: u32, retrans: u32) {
        if rtt_us == 0 || !crate::records::recording() {
            return;
        }
        let client = crate::records::client_key(client);
        let mut clients = self.clients.locked();
        if let Some(c) = clients.get_mut(client.as_ref()) {
            c.observe_rtt(rtt_us, retrans);
        }
    }

    /// 全体の RTT の合計 (`/metrics` の `sorahost_rtt_seconds`。T14.5)。
    ///
    /// 返すのは `[(us 合計, 標本数); 2]` で、添字は
    /// [`crate::recent::CLIENT_SIDE`] / [`crate::recent::ORIGIN_SIDE`]。
    /// **ホスト別は出さない** (系列が増えすぎる) ので、ここで畳んでから渡す。
    /// 読むのは `/metrics` に来たときだけなので、表を一度なめてよい。
    pub fn rtt_totals(&self) -> [(u64, u64); 2] {
        let fold = |acc: (u64, u64), s: &HostStats| {
            (
                acc.0.saturating_add(s.rtt_us_sum),
                acc.1.saturating_add(s.rtt_samples),
            )
        };
        let client = self
            .clients
            .locked()
            .values()
            .fold((0, 0), |a, c| fold(a, c.stats()));
        let origin = self.hosts.locked().map.values().fold((0, 0), fold);
        [client, origin]
    }

    /// 接続元の `User-Agent` を 1 つ覚える (`/clients`。T14.7)。
    ///
    /// **呼ぶのは接続の最初の要求のときだけ** (`crates/server/src/conn/serve_one.rs`)。要求ごとに見ると
    /// ヘッダーの走査も鍵も要求ごとに増えるので、接続 1 本につき 1 回に決めてある
    /// (2 要求目からは呼び出し側の旗で飛ばす)。
    pub fn record_client_agent(&self, client: &str, agent: &str) {
        if !crate::records::recording() {
            return;
        }
        self.with_client(client, |stats| stats.note_agent(agent));
    }

    /// 接続元ごとの同時接続の上限に当たって断った 1 本を数える (`/clients` の `rejected`。T14.13)。
    ///
    /// **通るのは断った経路だけ**なので、通した接続には 1 命令も足さない。全体の合計
    /// (`rejected_per_client`) も同じ場所で足す (鍵は接続元の表の 1 回)。
    pub fn record_client_rejected(&self, client: &str) {
        // **合計 (`/status` の `rejected_per_client`) は `off` でも数える**
        // (「誰が」を含まない数字なので、止めるのは接続元別の表だけ。T14.41)
        self.rejected_per_client
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if !crate::records::recording() {
            return;
        }
        self.with_client(client, |stats| stats.note_rejected());
    }

    /// 要求数の多い順に並べた接続元別統計 (`.rrd` と `/metrics` が使う欄だけ)。
    pub fn clients_sorted(&self) -> Vec<(String, HostStats)> {
        self.clients_sorted_by(ClientSort::Requests)
            .into_iter()
            .map(|(k, s)| (k, s.into_stats()))
            .collect()
    }

    /// 鍵を選んで並べた接続元の個票 (`/clients` と `/status` の `clients[]`。T14.7)。
    ///
    /// **同点は要求数 → 名前で崩す**ので、どの鍵でも順序は 1 つに決まる
    /// ([`Metrics::hosts_sorted_by`] と同じ作法。テストが順序で書ける)。
    pub fn clients_sorted_by(&self, sort: ClientSort) -> Vec<(String, ClientStats)> {
        let clients = self.clients.locked();
        let mut v: Vec<(String, ClientStats)> = clients
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        v.sort_by(|a, b| {
            let tie =
                b.1.stats()
                    .requests
                    .cmp(&a.1.stats().requests)
                    .then_with(|| a.0.cmp(&b.0));
            match sort {
                ClientSort::Requests => tie,
                ClientSort::Recent => b.1.stats().last_seen.cmp(&a.1.stats().last_seen).then(tie),
                ClientSort::Targets => {
                    b.1.distinct_targets()
                        .cmp(&a.1.distinct_targets())
                        .then(tie)
                }
                ClientSort::Literal => b.1.literal_targets().cmp(&a.1.literal_targets()).then(tie),
            }
        });
        v
    }

    /// **初めて見た時刻がこの窓の中にある接続元だけ**を拾う (T14.54 の規則 6)。
    ///
    /// 窓は `[from, to]` (両端を含む。呼ぶ側は「前に見た時刻」から「いまの時刻」を
    /// 渡し、同じ秒を二度見ても書かないよう自分で覚えておく)。`first_seen` が `0` の
    /// 接続元 (状態ファイルから読み戻した = この起動より前から居る) は**新しくない**
    /// ので外す。並びは `first_seen` の古い順 (同じ秒は名前順) で、`max` 件まで。
    /// 2 つ目は `max` に入り切らなかった件数。
    ///
    /// **鍵の内側でやるのは `first_seen` の比較だけ**で、写すのは拾った数件だけ。
    /// [`Metrics::clients_sorted_by`] で表を丸ごと写すと、1,000 接続元 × 宛先 256 件の
    /// clone を 5 秒ごとに鍵を握ったまま行うことになり、要求の経路
    /// ([`Metrics::record_client`]) が待たされる。
    pub fn clients_first_seen_in(&self, from: u64, to: u64, max: usize) -> (Vec<NewClient>, usize) {
        let mut found: Vec<NewClient> = {
            let clients = self.clients.locked();
            clients
                .iter()
                .filter(|(_, s)| {
                    let t = s.first_seen();
                    t > 0 && t >= from && t <= to
                })
                .map(|(k, s)| NewClient {
                    client: k.clone(),
                    first_seen: s.first_seen(),
                    agent: s.agent().map(|a| a.to_string()),
                    requests: s.stats().requests,
                    // `ports` は**出た順**に伸びるので、先頭が最初の宛先のポート
                    port: s.first_port(),
                    literal: s.has_literal_targets(),
                })
                .collect()
        };
        found.sort_by(|a, b| {
            a.first_seen
                .cmp(&b.first_seen)
                .then(a.client.cmp(&b.client))
        });
        let over = found.len().saturating_sub(max);
        found.truncate(max);
        (found, over)
    }
}
