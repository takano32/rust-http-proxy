//! 接続元 1 つの個票 ([`ClientStats`]。`/clients`。T14.7) と、内部エンドポイントを
//! 引いた接続元 ([`Reader`]。`/readers`。T14.53)。
//!
//! **ここに置いてあるのは層の都合**: どちらも `Metrics` が表に抱えている型で、
//! `Metrics` は 1 つ上のクレート (`proxy-metrics-core`) に居る (T14.55 で割ったとき、
//! 抱えられる側をこちらへ出した)。元の場所は `metrics` なので、そちらから今までの
//! 名前で引ける (`crate::metrics::ClientStats`)。

use std::time::Duration;

use crate::metrics::{
    Detail, HostOutcome, HostStats, MAX_AGENT_BYTES, MAX_CLIENT_AGENTS, MAX_CLIENT_PORTS,
    MAX_CLIENT_TARGETS, stats_json,
};

/// 接続元 1 つの個票 (`/clients`。T14.7)。
///
/// 要求数・転送量・応答時間は今までどおり [`HostStats`] で数える (`.rrd` に残る通算)。
/// **この型が足す欄はメモリだけ**で、再起動で消える (`/clients` の `"persisted":false`)。
/// `.rrd` の 1 スロットは名前 128 B + 55 項目 × 8 B = 568 B を使っている。版 3 で
/// ペイロードが 636 B になって余白は 4 → 68 B に広がった (T14.14) が、`agents` だけで
/// 4 × 128 B 要るので**やはり入らない** (数字の欄なら 8 個ほど足せる)。
/// 名前の並びを `.rrd` に残したくなったら、領域を増やして版を上げることになる。
#[derive(Debug, Default, Clone)]
pub struct ClientStats {
    /// 要求数・転送量・応答時間 (`/status` の `clients[]` の今までの欄)
    pub stats: HostStats,
    /// 初めて見た時刻 (epoch 秒)。**`0` はこの起動より前から居た**
    /// (状態ファイルから読み戻した接続元。いつからかは分からない)
    pub first_seen: u64,
    /// 見た `User-Agent` (最大 [`MAX_CLIENT_AGENTS`] 種、先頭 [`MAX_AGENT_BYTES`] バイト)
    pub agents: Vec<String>,
    /// そのうち最後に見たものの添字 (`/status` に出す 1 つ)
    agent_last: usize,
    /// 覚え切れなかった `User-Agent` の数 (5 種類目以降を名乗った接続の数)
    pub agents_dropped: u64,
    /// 宛先ホストの指紋 (最大 [`MAX_CLIENT_TARGETS`])。**名前は持たない**:
    /// 読みたいのは「何種類か」だけで、名前を持つと 1,000 接続元 × 256 件で MB になる
    targets: std::collections::HashSet<u64>,
    /// 宛先が [`MAX_CLIENT_TARGETS`] を超えた (`/clients` では `256+` の意味)
    pub targets_capped: bool,
    /// 使ったポートと、その要求数 (最大 [`MAX_CLIENT_PORTS`] 種)
    pub ports: Vec<(u16, u64)>,
    /// 覚え切れなかったポートへの要求数
    pub ports_other: u64,
    /// 宛先が IP リテラルだった要求数 (名前を引かずに繋いでいる = 普通のブラウザではない)
    pub literal_targets: u64,
    /// 443 / 80 以外のポートへの要求数
    pub nonstandard_ports: u64,
    /// 直前の要求の宛先 (そのままの文字列) と、そこから決まる 4 つ。
    /// **同じ宛先が続く間は分解も指紋も表もやり直さない**ための記憶
    /// ([`ClientStats::note_target`] の実測を参照)
    last_target: String,
    last_literal: bool,
    last_nonstandard: bool,
    last_port_slot: Option<usize>,
    last_ports_other: bool,
    /// 接続元ごとの同時接続の上限 (`PROXY_MAX_CONNS_PER_CLIENT`) に当たって断った本数
    /// (T14.13)。**書くのは断った経路だけ** ([`Metrics::record_client_rejected`])
    pub rejected: u64,
}

impl ClientStats {
    /// いま初めて見た接続元。
    /// **呼ぶのは 1 つ上の層の `Metrics` だけ** (T14.55 でクレートを割るまでは
    /// 同じクレートの中の `fn` だった)。
    pub fn now() -> ClientStats {
        ClientStats {
            first_seen: crate::cache::now_epoch(),
            ..ClientStats::default()
        }
    }

    /// 状態ファイルから読み戻した接続元 (**初めて見た時刻は分からない** ので `0`)。
    /// **呼ぶのは 1 つ上の層の `Metrics` だけ** (T14.55 でクレートを割るまでは
    /// 同じクレートの中の `fn` だった)。
    pub fn restored(stats: HostStats) -> ClientStats {
        ClientStats {
            stats,
            ..ClientStats::default()
        }
    }

    /// 1 要求を数える。**呼び出し側が既に鍵を取っている** ([`Metrics::record_client`])。
    ///
    /// `dir` は向き別のバイト `(上り, 下り)` (T14.26)。接続元にも名前解決や族の内訳は
    /// 無いので、[`Detail`] はこの 2 欄だけを埋めて渡す (`/status` に 0 の列は増えない)。
    /// **呼ぶのは 1 つ上の層の `Metrics` だけ** (T14.55 でクレートを割るまでは
    /// 同じクレートの中の `fn` だった)。
    pub fn count(
        &mut self,
        outcome: HostOutcome,
        bytes: u64,
        dir: (u64, u64),
        took: Option<Duration>,
        target: Option<&str>,
    ) {
        let detail = Detail {
            bytes_in: dir.0,
            bytes_out: dir.1,
            ..Detail::default()
        };
        self.stats
            .count(crate::cache::now_epoch(), outcome, bytes, took, &detail);
        if let Some(t) = target {
            self.note_target(t);
        }
    }

    /// 宛先の種類・ポート・IP リテラルを数える (鍵の内側)。
    ///
    /// **同じ宛先が続く間は、分解も指紋も表の引き直しもやらない** (`last_*` の記憶に
    /// 当たれば数え上げるだけ)。要求ごとに分解をやり直すと、その 100 ns 前後が
    /// forward の CPU/要求 では 1 us 以上になって出てくる (接続元の表の鍵は 8 並列では
    /// 取り合いになる)。**実測 (forward の CPU/要求、前後交互 6 組の中央値)**:
    /// 毎要求やり直す版は 40.94 → 42.27 us (+3.3%)、記憶つきは 40.57 → 40.96 us
    /// (+0.95%、ぶれの中)。`note_target` を止めただけの版は変更前と同じ 40.57 us。
    fn note_target(&mut self, target: &str) {
        if self.last_target != target {
            self.remember(target);
        }
        if self.last_literal {
            self.literal_targets += 1;
        }
        if self.last_nonstandard {
            self.nonstandard_ports += 1;
        }
        match self.last_port_slot {
            Some(i) => self.ports[i].1 += 1,
            None if self.last_ports_other => self.ports_other += 1,
            None => {}
        }
    }

    /// 宛先が変わったときだけ通る道 (分解・指紋・宛先の表・ポートの席を決める)。
    ///
    /// 数えるのは呼び出し元 ([`ClientStats::note_target`]) なので、ここでは
    /// **席を決めるだけ**にする (二重に数えないため)。
    fn remember(&mut self, target: &str) {
        // 置き場は使い回す (宛先が 2 つの間で交互に来ても確保しない)
        self.last_target.clear();
        self.last_target.push_str(target);
        let (host, port) = target_parts(target);
        self.last_literal = false;
        self.last_nonstandard = false;
        self.last_port_slot = None;
        self.last_ports_other = false;
        if host.is_empty() {
            return;
        }
        let fp = fingerprint(host);
        if self.targets.len() < MAX_CLIENT_TARGETS {
            self.targets.insert(fp);
        } else if !self.targets.contains(&fp) {
            self.targets_capped = true;
        }
        self.last_literal = host.parse::<std::net::IpAddr>().is_ok();
        let Some(p) = port else {
            return;
        };
        self.last_nonstandard = p != 443 && p != 80;
        self.last_port_slot = match self.ports.iter().position(|(q, _)| *q == p) {
            Some(i) => Some(i),
            None if self.ports.len() < MAX_CLIENT_PORTS => {
                // 0 で置いて、数えるのは呼び出し元に任せる
                self.ports.push((p, 0));
                Some(self.ports.len() - 1)
            }
            None => {
                self.last_ports_other = true;
                None
            }
        };
    }

    /// `User-Agent` を 1 つ覚える (**接続の最初の要求だけ**通る)。
    /// **呼ぶのは 1 つ上の層の `Metrics` だけ** (T14.55 でクレートを割るまでは
    /// 同じクレートの中の `fn` だった)。
    pub fn note_agent(&mut self, agent: &str) {
        let agent = agent.trim();
        if agent.is_empty() {
            return;
        }
        // 知っている `User-Agent` なら確保しない (接続 1 本につき 1 回通る道なので、
        // ここで `clip` の String を作ると接続ごとの確保が 1 回増える)
        if let Some(i) = self.agents.iter().position(|a| a.as_str() == agent) {
            self.agent_last = i;
            return;
        }
        // 覚えるのは切った形なので、長いものは切ってからもう一度見比べる
        // (切る前と比べたままだと、長い `User-Agent` がいつまでも「知らない 1 つ」になる)
        let agent = crate::recent::clip(agent, MAX_AGENT_BYTES);
        if let Some(i) = self.agents.iter().position(|a| *a == agent) {
            self.agent_last = i;
            return;
        }
        if self.agents.len() >= MAX_CLIENT_AGENTS {
            self.agents_dropped += 1;
            return;
        }
        self.agents.push(agent);
        self.agent_last = self.agents.len() - 1;
    }

    /// 宛先ホストの種類 ([`MAX_CLIENT_TARGETS`] で頭打ち。
    /// `targets_capped` が立っていたら「これ以上」の意味)。
    pub fn distinct_targets(&self) -> usize {
        self.targets.len()
    }

    /// 最後に見た `User-Agent` (`/status` に出す 1 つ)。
    pub fn agent(&self) -> Option<&str> {
        self.agents.get(self.agent_last).map(|s| s.as_str())
    }

    /// 使ったポートを要求数の多い順に (同数は番号順。順序は 1 つに決まる)。
    pub fn ports_sorted(&self) -> Vec<(u16, u64)> {
        let mut v = self.ports.clone();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }

    /// `/status` の `clients[]` に足す欄 (**今までの欄の後ろに並べる**。先頭の `,` 込み)。
    pub fn status_json(&self) -> String {
        format!(
            ",\"first_seen\":{},\"agent\":{},\"distinct_targets\":{},\"literal_targets\":{}",
            self.first_seen,
            crate::json::quote_opt(self.agent()),
            self.distinct_targets(),
            self.literal_targets
        )
    }

    /// `/clients` の 1 行 (`/status` の `clients[]` と同じ欄 + 個票だけの欄)。
    pub fn to_json(&self, client: &str) -> String {
        let ports: Vec<String> = self
            .ports_sorted()
            .iter()
            .map(|(p, n)| format!("{{\"port\":{},\"requests\":{}}}", p, n))
            .collect();
        format!(
            "{{\"client\":\"{}\",{}{},\"agents\":{},\"agents_dropped\":{},\"distinct_targets_capped\":{},\"ports\":[{}],\"ports_other\":{},\"nonstandard_ports\":{},\"rejected\":{}}}",
            crate::json::escape(client),
            stats_json(&self.stats, false),
            self.status_json(),
            crate::json::list(&self.agents),
            self.agents_dropped,
            self.targets_capped,
            ports.join(","),
            self.ports_other,
            self.nonstandard_ports,
            self.rejected
        )
    }
}

/// 内部エンドポイントを引いた接続元を覚えておく数 (T14.53)。
///
/// 溢れたら**最後に引いたのがいちばん古い行**を捨てる (走査は次々に別の IP で来るので、
/// 古い行を残すと「いま誰が読んでいるか」が押し出される)。1 行は鍵 (最大
/// [`crate::recent::MAX_CLIENT`] B) とパス ([`MAX_READER_PATH`] B) で 200 B 前後なので、
/// 満杯でも 45 KB ほど (`/status` の `memory.rings.readers`)。
pub const MAX_READERS: usize = 256;

/// `last_path` に残す長さ (バイト)。**問い合わせ文字列 (`?` 以降) は落とす**
/// (Phase 14 の共通の決まり: 記録に URL の問い合わせ文字列を入れない)。
pub const MAX_READER_PATH: usize = 64;

/// `/status` の `readers` に出す件数 (上位から。全部は `/readers`)。
pub const STATUS_READERS: usize = 20;

/// 内部エンドポイント (`/status` `/clients` …) を引いた接続元 1 つ (T14.53)。
///
/// T14.7 の `clients[]` は**自分宛てだけの接続を数えない** (監視で埋まってしまうため。
/// 呼ぶ側 `crates/server/src/lib.rs` の [`Metrics::record_client_agent`] の手前にその注記がある)。
/// こちらはその逆で、**自分宛てだけ**を数える別の表: 認証なしの公開ポートで
/// 「誰が個票を読んでいるか」(走査か、自分の監視か) を見分けるためのもの。
/// **プロキシとしての要求 (CONNECT / forward) は 1 件も入らない**ので、2 つの表は混ざらない。
#[derive(Debug, Default, Clone)]
pub struct Reader {
    /// 内部エンドポイントを引いた回数
    pub count: u64,
    /// 最後に引いた時刻 (epoch 秒)
    pub last_at: u64,
    /// 最後に引いたパス (先頭 [`MAX_READER_PATH`] バイト、**`?` 以降は落とす**)
    pub last_path: String,
}

impl Reader {
    /// `/status` の `readers[]` と `/readers` の 1 行 (**形は同じ**。読む道具が
    /// 両方を 1 つの読み方で扱えるように)。
    pub fn to_json(&self, client: &str) -> String {
        format!(
            "{{\"client\":\"{}\",\"count\":{},\"last_at\":{},\"last_path\":\"{}\"}}",
            crate::json::escape(client),
            self.count,
            self.last_at,
            crate::json::escape(&self.last_path)
        )
    }
}

/// 宛先 (`scheme://host:port` / `host:port` / `host`) を (ホスト, ポート) に分ける。
///
/// 呼び出し側の鍵の形がまちまち (forward は `http://host:port`、CONNECT は `host:port`、
/// 403 は要求行のまま) なので、**ここで 1 つに寄せる**。
pub fn target_parts(target: &str) -> (&str, Option<u16>) {
    let t = match target.find("://") {
        Some(i) => &target[i + 3..],
        None => target,
    };
    let t = match t.find('/') {
        Some(i) => &t[..i],
        None => t,
    };
    crate::net::split_host_port_ref(t)
}

/// 宛先ホストの 64 ビットの指紋 (FNV-1a、大小同一視)。
///
/// 種類の数を数えるだけなので名前は要らない。256 件で衝突する確率は 1e-15 の桁。
pub fn fingerprint(host: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in host.as_bytes() {
        h ^= b.to_ascii_lowercase() as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
