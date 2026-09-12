//! 個票を読み出すエンドポイント (T13.4): `/errors` `/connections` `/dns` `/log` `/hosts`。
//!
//! `/status` (集計) と `/history` (時系列) では「**誰が・いつ・なぜ**」が読めない。
//! ここは「今この瞬間の中身」と「直近に起きたこと」を、集計に畳む前の形で出す口で、
//! どれも JSON・`Cache-Control: no-store`・`Connection: close` (組み立ては
//! [`super::handle`] が共通で行う)。時刻は epoch 秒。
//!
//! **応答は必ず [`MAX_BODY`] 以下**にする。件数の上限 (`?n=` / `?limit=`) とは別に
//! バイト数でも打ち切り、切ったときは `"truncated":true` を出す。上限を件数だけで
//! 決めると、長いホスト名や多いアドレスで簡単に越えてしまう
//! (`/dns` は 1 ホストに A / AAAA が 10 本以上返ることがある)。

use std::fmt::Write as _;
use std::time::Instant;

use super::{Endpoint, parse_query};
use crate::recent::MAX_ERRORS;

/// 個票の応答 1 本の上限 (256 KiB)。監視が 1 分おきに引いても回線を埋めない大きさで、
/// `/errors` 500 件・`/connections` 240 件・`/hosts` 1,000 件のどれも収まる。
pub const MAX_BODY: usize = 256 * 1024;

/// 末尾 (`],"truncated":true,...}`) のために空けておくぶん。
const TRAILER: usize = 512;

/// 要素を上限のバイト数まで `[...]` に並べる。入り切らなかったらそこで止める。
/// 返すのは (書けた件数, 打ち切ったか)。
fn array_within(out: &mut String, items: impl IntoIterator<Item = String>) -> (usize, bool) {
    let budget = MAX_BODY - TRAILER;
    out.push('[');
    let (mut n, mut cut) = (0usize, false);
    for item in items {
        if out.len() + item.len() + 2 > budget {
            cut = true;
            break;
        }
        if n > 0 {
            out.push(',');
        }
        out.push_str(&item);
        n += 1;
    }
    out.push(']');
    (n, cut)
}

/// `?key=` の文字列 (無ければ `""`)。
fn str_param(query: Option<&str>, key: &str) -> String {
    parse_query(query.unwrap_or(""))
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// `?key=N` を読む (無い / 読めない / 範囲外は既定か端に倒す。`/status?sort=` と同じ方針)。
fn num_param(query: Option<&str>, key: &str, default: usize, max: usize) -> usize {
    parse_query(query.unwrap_or(""))
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(1, max)
}

/// `/errors?n=100` — 直近のエラーの個票 (新しい順、既定 100 件・最大 [`MAX_ERRORS`])。
pub fn errors(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    let n = num_param(query, "n", 100, MAX_ERRORS);
    let (entries, total) = ep.metrics.errors.recent(n);
    let mut out = String::with_capacity(4096);
    out.push_str("{\"errors\":");
    let (shown, cut) = array_within(&mut out, entries.iter().map(|e| e.to_json()));
    let _ = write!(
        out,
        ",\"count\":{},\"kept\":{},\"capacity\":{},\"recorded\":{},\"truncated\":{}}}",
        shown,
        ep.metrics.errors.len(),
        MAX_ERRORS,
        total,
        cut
    );
    (200, "application/json", out)
}

/// `/connections` — いま開いている接続の一覧 (通し番号の小さい順 = 古い順)。
///
/// `--lite` では登録していないので空の一覧を返す (`"lite":true` でそれと分かる)。
/// 件数の上限は置かず、[`MAX_BODY`] に収まるところまで出す (`"truncated"` で分かる)。
pub fn connections(ep: &Endpoint<'_>) -> (u16, &'static str, String) {
    let now = Instant::now();
    let all = ep.metrics.conns.snapshot();
    let count = all.len();
    let mut out = String::with_capacity(8192);
    out.push_str("{\"connections\":");
    let (shown, cut) = array_within(&mut out, all.iter().map(|c| c.to_json(now)));
    let _ = write!(
        out,
        ",\"count\":{},\"shown\":{},\"truncated\":{},\"lite\":{}}}",
        count,
        shown,
        cut,
        !ep.metrics.conns.enabled()
    );
    (200, "application/json", out)
}

/// `/dns?sort=age|host|misses&limit=300` — 名前解決の表の中身 (T13.1 の効きを見る口)。
pub fn dns(query: Option<&str>) -> (u16, &'static str, String) {
    let sort = crate::dns::DnsSort::from_param(&str_param(query, "sort"));
    let limit = num_param(query, "limit", 300, 4096);
    let rows = crate::dns::table(sort);
    let count = rows.len();
    let mut out = String::with_capacity(8192);
    out.push_str("{\"entries\":");
    let (shown, cut) = array_within(&mut out, rows.iter().take(limit).map(|r| r.to_json()));
    let _ = write!(
        out,
        ",\"count\":{},\"shown\":{},\"sort\":\"{}\",\"ttl_secs\":{},\"negative_ttl_secs\":{},\"truncated\":{}}}",
        count,
        shown,
        sort.name(),
        crate::dns::ttl().as_secs(),
        crate::dns::negative_ttl().as_secs(),
        cut
    );
    (200, "application/json", out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{Detail, ErrCause, Metrics};
    use crate::recent::ConnState;

    /// 500 件の最悪 (宛先も接続元も上限いっぱい) でも 256 KiB に収まること。
    #[test]
    fn the_errors_response_stays_under_256_kib() {
        let m = Metrics::new();
        let long = "x".repeat(300);
        for _ in 0..(MAX_ERRORS + 20) {
            m.record_error(
                true,
                &long,
                "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6",
                502,
                &Detail {
                    cause: Some(ErrCause::Unreachable),
                    dns_ms: u64::MAX,
                    connect_ms: u64::MAX,
                    ..Detail::default()
                },
            );
        }
        let ep_metrics = m;
        let (entries, total) = ep_metrics.errors.recent(MAX_ERRORS);
        assert_eq!(entries.len(), MAX_ERRORS);
        assert_eq!(total, MAX_ERRORS as u64 + 20);
        let mut body = String::from("{\"errors\":");
        let (shown, cut) = array_within(&mut body, entries.iter().map(|e| e.to_json()));
        body.push('}');
        assert_eq!(shown, MAX_ERRORS, "500 件が全部入ること");
        assert!(!cut);
        assert!(body.len() <= MAX_BODY, "{} B", body.len());
        println!(
            "errors 500 件の応答: {} B (上限 {} B)",
            body.len(),
            MAX_BODY
        );
    }

    /// 原因の分からないエラーはリングに書かない。
    #[test]
    fn errors_without_a_cause_are_not_recorded() {
        let m = Metrics::new();
        m.record_error(
            false,
            "example.com:80",
            "127.0.0.1",
            502,
            &Detail::default(),
        );
        assert!(m.errors.is_empty());
    }

    /// 240 本 (デプロイ先の上限) でも、1,000 本でも 256 KiB に収まること。
    #[test]
    fn the_connections_response_stays_under_256_kib() {
        let m = Metrics::new();
        let long_host = format!("{}.example.net:65535", "sub.".repeat(30));
        let now = Instant::now();
        for i in 0..1000u64 {
            let slot = m
                .conns
                .register(i, "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6", now)
                .expect("登録できる");
            slot.begin_tunnel(&long_host);
            slot.set_bytes(u64::MAX);
            slot.set_state(ConnState::Parked);
        }
        let all = m.conns.snapshot();
        assert_eq!(all.len(), 1000);
        assert_eq!(all[0].id, 0, "古い順に並ぶ");
        for n in [240usize, 1000] {
            let mut body = String::from("{\"connections\":");
            let (shown, cut) = array_within(&mut body, all.iter().take(n).map(|c| c.to_json(now)));
            body.push('}');
            assert_eq!(shown, n, "{} 本が全部入ること", n);
            assert!(!cut);
            assert!(body.len() <= MAX_BODY, "{} 本で {} B", n, body.len());
            println!(
                "connections {} 本の応答: {} B (上限 {} B)",
                n,
                body.len(),
                MAX_BODY
            );
        }
    }

    /// `/dns` は**どんな中身でも** 256 KiB に収まること。
    ///
    /// 名前は 253 文字 (DNS の上限) まで、アドレスは 1 ホストに 8 本まで出すので、
    /// 最悪の 1 行は 900 B 近くなる。件数の上限 (既定 300) だけでは足りないので
    /// **バイト数でも打ち切る** — その打ち切りがちゃんと効くことをここで見る。
    #[test]
    fn the_dns_response_stays_under_256_kib() {
        use crate::dns::{MAX_ROW_ADDRS, TableRow};
        use std::net::IpAddr;
        let v6: IpAddr = "2001:db8:85a3:8d3:1319:8a2e:370:7348".parse().unwrap();
        let v4: IpAddr = "93.184.216.34".parse().unwrap();
        // (1) ありふれた行 (名前 15 文字、A と AAAA の 2 本)。既定の 300 行が収まること
        let plain = TableRow {
            host: "www.example.com".to_string(),
            addrs: vec![v4, v6],
            addr_count: 2,
            age_secs: 12,
            ttl_left: 48,
            idle_secs: 3,
            win_v6: Some(false),
            failed: None,
            refreshing: false,
            misses: 4,
            refreshes: 2,
        };
        let mut body = String::from("{\"entries\":");
        let (shown, cut) = array_within(&mut body, vec![plain; 300].iter().map(|r| r.to_json()));
        body.push('}');
        assert_eq!(shown, 300);
        assert!(!cut);
        assert!(body.len() <= MAX_BODY, "{} B", body.len());
        println!(
            "dns 300 行 (ありふれた行): {} B (上限 {} B)",
            body.len(),
            MAX_BODY
        );

        // (2) 最悪の行を表いっぱい (4,096 = `MAX_ENTRIES`) 並べても上限を越えない
        let worst = TableRow {
            host: "a".repeat(253),
            addrs: vec![v6; MAX_ROW_ADDRS],
            addr_count: 32,
            age_secs: u64::MAX,
            ttl_left: u64::MAX,
            idle_secs: u64::MAX,
            win_v6: Some(true),
            failed: Some((u64::MAX, "e".repeat(300))),
            refreshing: true,
            misses: u64::MAX,
            refreshes: u64::MAX,
        };
        let mut body = String::from("{\"entries\":");
        let (shown, cut) = array_within(&mut body, vec![worst; 4096].iter().map(|r| r.to_json()));
        body.push('}');
        assert!(cut, "バイト数で打ち切られること");
        assert!(body.len() <= MAX_BODY, "{} B", body.len());
        println!(
            "dns 4,096 行 (最悪の行): {} B / 出せたのは {} 行 (上限 {} B)",
            body.len(),
            shown,
            MAX_BODY
        );
    }

    #[test]
    fn numeric_parameters_fall_back_to_the_default() {
        assert_eq!(num_param(None, "n", 100, 500), 100);
        assert_eq!(num_param(Some("n=7"), "n", 100, 500), 7);
        assert_eq!(num_param(Some("n=9999"), "n", 100, 500), 500);
        assert_eq!(num_param(Some("n=0"), "n", 100, 500), 1);
        assert_eq!(num_param(Some("n=abc"), "n", 100, 500), 100);
        assert_eq!(str_param(Some("sort=host"), "sort"), "host");
        assert_eq!(str_param(Some("a=1&sort=misses"), "sort"), "misses");
        assert_eq!(str_param(None, "sort"), "");
    }
}
