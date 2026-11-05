//! ホスト別 / 接続元別の 1 行を `/status` の `hosts[]` / `clients[]` と `/hosts` / `/clients` の
//! 形にする ([`stats_json`])。**組み立てはこの 1 か所**。

use std::fmt::Write as _;

use super::HostStats;

/// ホスト別 / 接続元別に共通の統計フィールド (先頭・末尾の波括弧なし)。
///
/// `detail` はホスト別だけ (T12.4 (2))。接続元別には名前解決も接続も族も無いので、
/// 全部 0 の列を 50 行ぶん並べても `/status` が太るだけになる。
/// 向き別のバイト (`bytes_in` / `bytes_out`。T14.26) は**両方に出す**: 接続元にも
/// 「この端末は上りが主か下りが主か」があり、どちらも実際に数えた値が入る。
///
/// 外から呼べるのは `/hosts` (T13.4) が **`/status` の `hosts[]` と同じ形**で
/// 全ホストを出すため。形が 2 つに分かれると `scripts/status-diff.py` が両方を
/// 読めなくなるので、組み立てはこの 1 か所に置く。
pub fn stats_json(s: &HostStats, detail: bool) -> String {
    let mut out = format!(
        "\"requests\":{},\"hits\":{},\"misses\":{},\"bypass\":{},\"errors\":{},\"blocked\":{},\"bytes\":{},\"bytes_in\":{},\"bytes_out\":{},\"timed\":{},\"avg_ms\":{:.1},\"p50_ms\":{:.1},\"p95_ms\":{:.1},\"max_ms\":{},\"last_seen\":{}",
        s.requests,
        s.hits,
        s.misses,
        s.bypass,
        s.errors,
        s.blocked,
        s.bytes,
        s.bytes_in,
        s.bytes_out,
        s.timed,
        s.avg_ms(),
        s.quantile_ms(0.5),
        s.quantile_ms(0.95),
        s.duration_ms_max,
        s.last_seen
    );
    // カーネルの RTT (T14.5)。標本が無ければ `null` (Linux 以外・`--lite`・
    // まだ 1 本も閉じていない)。ホスト別はオリジン側、接続元別はクライアント側
    match (s.rtt_avg_ms(), s.rtt_min_ms()) {
        (Some(avg), Some(min)) => {
            let _ = write!(
                out,
                ",\"rtt_ms\":{{\"avg\":{:.3},\"min\":{:.3},\"samples\":{}}},\"retrans\":{}",
                avg, min, s.rtt_samples, s.retrans
            );
        }
        _ => out.push_str(",\"rtt_ms\":null,\"retrans\":0"),
    }
    if detail {
        let _ = write!(
            out,
            ",\"dns_ms_sum\":{},\"dns_misses\":{},\"connect_ms_sum\":{},\"v4_wins\":{},\"v6_wins\":{},\"errors_by_cause\":[",
            s.dns_ms_sum, s.dns_misses, s.connect_ms_sum, s.v4_wins, s.v6_wins
        );
        for (i, c) in s.errors_by_cause.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", c);
        }
        // **後ろに足すだけ** (T25.0)。DNS の失敗の内訳 (`nxdomain` / `dns_temp`) を
        // 9・10 番目に置く。先頭 8 つの意味と並びは 1 つも変えていないので、過去の
        // 雪像 (8 要素) を読む側はそのままでよい (名前は `/status` の README と
        // `DNS_CAUSE_NAMES`、`/explain` は名前つきで出す)
        for c in s.dns_by_cause {
            out.push(',');
            let _ = write!(out, "{}", c);
        }
        out.push(']');
        // CONNECT のホストと SNI の食い違い (T14.38)。**ホスト別だけ** (接続元別には
        // 宛先が無い)。**末尾に足した** ので既存の鍵の順は変わらない
        let _ = write!(out, ",\"sni_mismatch\":{}", s.sni_mismatch);
        // 確立までの SYN の再送 (T14.46)。**ホスト別だけ** (`sni_mismatch` と同じく
        // `detail` の内側 — 接続元別には「繋ぎに行く先」が無い)。
        // **末尾に足した**ので既存の鍵の順は変わらない
        let _ = write!(out, ",\"syn_retrans\":{}", s.syn_retrans);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 行の形を 1 バイト単位で縛る: 接続元別 (`detail` なし、RTT なし) と
    /// ホスト別 (`detail` あり、RTT あり。`errors_by_cause` は 8 + 2 要素)。
    #[test]
    fn the_row_json_is_byte_for_byte_stable() {
        let s = HostStats {
            requests: 4,
            hits: 1,
            misses: 1,
            bypass: 1,
            errors: 1,
            blocked: 0,
            bytes: 3102,
            bytes_in: 1034,
            bytes_out: 2068,
            timed: 2,
            duration_ms_sum: 300,
            duration_ms_max: 257,
            last_seen: 1_700_000_000,
            dns_ms_sum: 12,
            dns_misses: 1,
            connect_ms_sum: 245,
            v4_wins: 1,
            v6_wins: 1,
            errors_by_cause: [2, 0, 0, 0, 0, 0, 0, 0],
            dns_by_cause: [1, 1],
            sni_mismatch: 3,
            syn_retrans: 4,
            ..HostStats::default()
        };
        assert_eq!(
            stats_json(&s, false),
            "\"requests\":4,\"hits\":1,\"misses\":1,\"bypass\":1,\"errors\":1,\"blocked\":0,\"bytes\":3102,\"bytes_in\":1034,\"bytes_out\":2068,\"timed\":2,\"avg_ms\":150.0,\"p50_ms\":257.0,\"p95_ms\":257.0,\"max_ms\":257,\"last_seen\":1700000000,\"rtt_ms\":null,\"retrans\":0"
        );
        let mut with_rtt = s.clone();
        with_rtt.observe_rtt(30_100, 2);
        with_rtt.observe_rtt(20_000, 0);
        assert_eq!(
            stats_json(&with_rtt, true),
            "\"requests\":4,\"hits\":1,\"misses\":1,\"bypass\":1,\"errors\":1,\"blocked\":0,\"bytes\":3102,\"bytes_in\":1034,\"bytes_out\":2068,\"timed\":2,\"avg_ms\":150.0,\"p50_ms\":257.0,\"p95_ms\":257.0,\"max_ms\":257,\"last_seen\":1700000000,\"rtt_ms\":{\"avg\":25.050,\"min\":20.000,\"samples\":2},\"retrans\":2,\"dns_ms_sum\":12,\"dns_misses\":1,\"connect_ms_sum\":245,\"v4_wins\":1,\"v6_wins\":1,\"errors_by_cause\":[2,0,0,0,0,0,0,0,1,1],\"sni_mismatch\":3,\"syn_retrans\":4"
        );
    }
}
