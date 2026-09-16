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
