//! 接続元別の表の決まり: 覚える上限 ([`MAX_CLIENTS`] ほか)、初めて見た接続元の写し
//! ([`NewClient`])、`/clients` の並べ方 ([`ClientSort`])。個票そのもの (`ClientStats`) は
//! 1 つ上の層 (`proxy-metrics-window` の `clients`) に居る。

/// 接続元 IP ごとの統計を持つ上限。
pub const MAX_CLIENTS: usize = 1000;

/// 接続元ごとに覚えておく `User-Agent` の種類 (T14.7)。
///
/// 1 つの IP の裏に複数の端末が居る (NAT) ことがあるので 1 つでは足りず、
/// 並べても読めないので 4 つ。あふれた分は数えるだけ (`agents_dropped`)。
pub const MAX_CLIENT_AGENTS: usize = 4;

/// `User-Agent` を覚えておく長さ (バイト)。
///
/// 個票に入れてよい**唯一のヘッダー**で、先頭 128 バイトだけ (Phase 14 の共通の決まり:
/// URL のパスも問い合わせ文字列も本文も入れない)。
pub const MAX_AGENT_BYTES: usize = 128;

/// 接続元ごとに数える宛先の種類の上限。超えたら `distinct_targets_capped` を立てる。
pub const MAX_CLIENT_TARGETS: usize = 256;

/// 接続元ごとに覚えておくポートの種類。あふれた分は `ports_other` にまとめる。
pub const MAX_CLIENT_PORTS: usize = 8;

/// **初めて見た接続元** 1 件の写し (`Metrics::clients_first_seen_in` が返す。T14.54)。
///
/// `ClientStats` を丸ごと写すと宛先の集合 (最大 [`MAX_CLIENT_TARGETS`] 件) まで
/// 付いてくるので、`/events` の 1 行に要るものだけを持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewClient {
    /// 接続元 (**表の鍵そのまま**。あふれた分の `other` もここに来る。
    /// `PROXY_RECORDS=hashed` (T14.41) では鍵が 16 進なので、この値もそれになる)
    pub client: String,
    /// 初めて見た時刻 (epoch 秒)
    pub first_seen: u64,
    /// 最後に名乗った `User-Agent` (無ければ `None`)
    pub agent: Option<String>,
    /// ここまでに数えた要求数
    pub requests: u64,
    /// **最初の宛先の種類**: 最初に使ったポート (`ClientStats::ports` は出た順に
    /// 並ぶので先頭が最初の宛先のもの。1 つも無ければ `None`)
    pub port: Option<u16>,
    /// 同じく、IP リテラル宛てがあったか (名前を引かずに繋いでいる = 普通の閲覧ではない)
    pub literal: bool,
}

/// `/clients` を並べる鍵 (`?sort=`。T14.7)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClientSort {
    /// 要求数の多い順 (既定。`/status` の `clients[]` と同じ並び)
    #[default]
    Requests,
    /// 最後に要求を受けた時刻の新しい順
    Recent,
    /// 宛先ホストの種類が多い順
    Targets,
    /// IP リテラル宛ての要求が多い順
    Literal,
}

impl ClientSort {
    /// `?sort=` の値から決める。**知らない値は既定 (`requests`) に倒す**
    /// ([`HostSort::from_param`](super::HostSort::from_param) と同じ方針)。
    pub fn from_param(v: &str) -> Self {
        match v {
            "recent" => ClientSort::Recent,
            "targets" => ClientSort::Targets,
            "literal" => ClientSort::Literal,
            _ => ClientSort::Requests,
        }
    }

    /// `/clients` の `"sort"` に出す名前。
    pub fn name(self) -> &'static str {
        match self {
            ClientSort::Requests => "requests",
            ClientSort::Recent => "recent",
            ClientSort::Targets => "targets",
            ClientSort::Literal => "literal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `?sort=` の 4 つの値は名前と往復し、知らない値 (大文字・空・綴り違い) は既定に倒れる。
    #[test]
    fn the_client_sort_round_trips_its_name_and_falls_back_to_requests() {
        for s in [
            ClientSort::Requests,
            ClientSort::Recent,
            ClientSort::Targets,
            ClientSort::Literal,
        ] {
            assert_eq!(ClientSort::from_param(s.name()), s);
        }
        for v in ["", "RECENT", "nonsense", "targets?"] {
            assert_eq!(ClientSort::from_param(v), ClientSort::Requests, "{v}");
        }
        assert_eq!(ClientSort::default(), ClientSort::Requests);
    }
}
