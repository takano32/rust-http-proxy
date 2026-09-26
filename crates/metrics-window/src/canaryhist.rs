//! canary (T14.10) の**窓だけ** — `/history` の `canary` の列を持つ環状バッファ。
//!
//! **ここに置いてあるのは層の都合**: 測る側 (`canary`) は `Metrics` が要るので 1 つ上の
//! クレート (`proxy-metrics-core`) に居るが、窓を読むのは [`crate::history`] の
//! `/history` の組み立てで、そちらはここと同じ層に居る (T14.55 で割ったときに
//! 「測る側」と「窓」を分けた)。測る側から今までの名前で引ける。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;

use crate::sync::LockExt;

/// 窓の解像度 (秒) と本数。**`.rrd` には書かない** (メモリだけ)。
pub const RESOLUTIONS: [(u64, usize); 2] = [(5, 720), (60, 1440)];

/// 窓の 1 行 (`/history` の `canary` の 1 標本)。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    t: u64,
    dns_ms: u64,
    connect_ms: u64,
    host: String,
    /// IPv6 側だけの 1 本 (繋がらなければ `null`。T14.37)
    ipv6_connect_ms: Option<u64>,
}

/// `/history` の `canary` の列名 (この順で並ぶ)。
///
/// **足すのは末尾だけ** (T14.37 で `canary_ipv6_connect_ms` を 5 列目に足した)。
/// 読む側 (`scripts/check-dashboard.js`) は先頭からの一致で見るので、列を増やしても
/// 古い版の出力がそのまま読める。
pub const KEYS: [&str; 5] = [
    "t",
    "canary_dns_ms",
    "canary_connect_ms",
    "canary_host",
    "canary_ipv6_connect_ms",
];

/// メモリ上の窓 (5 秒 × 720 / 60 秒 × 1,440)。**`.rrd` には書かない。**
static RINGS: Mutex<Option<[VecDeque<Row>; RESOLUTIONS.len()]>> = Mutex::new(None);

/// 窓に 1 行足す (同じ窓に 2 回入ったら**新しい方で置き換える**)。
pub fn push(at: u64, dns_ms: u64, connect_ms: u64, host: &str, ipv6_connect_ms: Option<u64>) {
    let mut guard = RINGS.locked();
    let rings = guard.get_or_insert_with(Default::default);
    for (ring, (step, cap)) in rings.iter_mut().zip(RESOLUTIONS) {
        let t = (at / step) * step;
        let row = Row {
            t,
            dns_ms,
            connect_ms,
            host: host.to_string(),
            ipv6_connect_ms,
        };
        match ring.back_mut() {
            Some(back) if back.t == t => *back = row,
            _ => {
                if ring.len() >= cap {
                    ring.pop_front();
                }
                ring.push_back(row);
            }
        }
    }
}

/// `/history` の末尾に `canary` の配列を足す (**別の配列**なので既存の `keys` /
/// `samples` を読む側は 1 行も変えなくてよい)。
pub fn push_history_json(out: &mut String, res: usize) {
    out.push_str(",\"canary\":{\"keys\":[");
    for (i, k) in KEYS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{}\"", k);
    }
    out.push_str("],\"samples\":[");
    let guard = RINGS.locked();
    if let Some(ring) = guard.as_ref().and_then(|rings| rings.get(res)) {
        for (i, r) in ring.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "[{},{},{},\"{}\",{}]",
                r.t,
                r.dns_ms,
                r.connect_ms,
                crate::json::escape(&r.host),
                num_or_null(r.ipv6_connect_ms),
            );
        }
    }
    drop(guard);
    out.push_str("]}");
}

/// 窓の中 (`since <= t <= until`) の canary の名前解決 (`dns_ms`) の中央値 (T17.1)。
///
/// `dns_slow` (`proxy-metrics-watch` の `anomaly`) が「いまのリゾルバの速さ」の基準線に
/// 使う。読むのは 5 秒の窓 (720 行 = 1 時間) で、1 行も無ければ `None`
/// (canary が `off`、起動直後、履歴スレッドが無い)。偶数本なら下の中央値
/// (整数の ms のまま。平均しない)。**失敗した回も入る** (名前解決で落ちた回は締め切り
/// までの ms) が、中央値なので半分を越えて落ちない限り動かない。
pub fn dns_p50_ms(since: u64, until: u64) -> Option<u64> {
    let mut v: Vec<u64> = {
        let guard = RINGS.locked();
        let ring = guard.as_ref()?.first()?;
        ring.iter()
            .filter(|r| since <= r.t && r.t <= until)
            .map(|r| r.dns_ms)
            .collect()
    };
    if v.is_empty() {
        return None;
    }
    let mid = (v.len() - 1) / 2;
    Some(*v.select_nth_unstable(mid).1)
}

/// JSON の数 (繋がらなかった IPv6 側は `null`。T14.37)。
fn num_or_null(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

/// 試験用: 窓を空に戻す (同じプロセスで 2 度目を測るため)。
///
/// **`#[cfg(test)]` を付けていない**のは、測る側 (`canary`) が 1 つ上のクレートに居て、
/// そちらのテストから引くため (T14.55)。実体は 1 行。
pub fn clear() {
    *RINGS.locked() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T17.1: 窓の中の `dns_ms` の中央値。窓の外と、1 行も無いときは数えない。
    /// (このクレートで [`RINGS`] に触るテストはこれ 1 本だけ)
    #[test]
    fn the_dns_median_reads_only_the_window() {
        clear();
        assert_eq!(dns_p50_ms(0, u64::MAX), None, "空なら None");
        push(1_000, 900, 5, "a:443", None); // 窓の外 (古い)
        push(3_600, 12, 5, "a:443", None);
        push(3_660, 8, 5, "a:443", None);
        push(3_720, 300, 5, "a:443", None);
        push(3_780, 9, 5, "a:443", None);
        assert_eq!(
            dns_p50_ms(3_000, 4_000),
            Some(9),
            "8, 9, 12, 300 の下の中央値"
        );
        assert_eq!(dns_p50_ms(3_700, 4_000), Some(9), "300, 9");
        assert_eq!(dns_p50_ms(5_000, 6_000), None, "窓に 1 行も無い");
        clear();
    }
}
