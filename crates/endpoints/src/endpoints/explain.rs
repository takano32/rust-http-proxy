//! 1 相手の説明 `/explain?host=<name>` / `?client=<ip>` (T14.36)。
//!
//! 「この宛先はなぜ遅いか」「この接続元は誰か」を調べるには `/hosts` `/dns` `/recent`
//! `/clients` `/errors` を横断して読む必要があった (T14.0 で実際にやった作業)。
//! ここは**サーバーが 1 相手について全部を 1 枚に組んで**、読める文章 (日本語) と
//! 同じ内容の数字で返す口。
//!
//! **新しい記録は 1 つも増やしていない**: この口に来たときだけ、既にある表とリング
//! ([`crate::metrics::Metrics`]・[`crate::dns::table`]・`/recent` と `/errors` のリング) を
//! 読んで組み立てる。要求の経路の費用は 0。
//!
//! **判定はしない**: `summary` は段階の数字を 1 行に並べるだけで、「だから RTT だ」の
//! 推定は読む人がする (T14.3 の (e) と同じ方針)。
//!
//! 知らない相手でも **200 のまま `"known":false`** を返す (探した場所はすべて空だった、
//! という答えそのものが読む人の欲しい情報なので 404 にしない)。応答は [`MAX_BODY`]
//! (64 KiB) 以下で、個票は 10 本・エラーは 5 件まで。

use std::fmt::Write as _;

use super::{Endpoint, parse_query};
use crate::metrics::{ERR_CAUSE_NAMES, HostStats};
use crate::recent::{MAX_ERRORS, RecentEntry};

/// 応答 1 本の上限 (64 KiB)。個票の口 (256 KiB) より小さいのは、ここが
/// 「1 相手ぶんを 1 画面で読む」口で、件数の上限 (10 本 / 5 件) がそもそも小さいため。
pub const MAX_BODY: usize = 64 * 1024;

/// 末尾 (`,"summary":…,"truncated":true}`) のために空けておくぶん。
const TRAILER: usize = 2048;

/// 出す個票の本数 (`/recent` のそのホスト / その接続元の直近)。
const MAX_ROWS: usize = 10;

/// 出すエラーの件数 (`/errors` の直近)。
const MAX_ERROR_ROWS: usize = 5;

/// 出すホスト別統計の鍵の数 (`connect://` と `http://` とポート違いで増える)。
const MAX_KEYS: usize = 8;

/// 添える異常 (T14.23) の件数。**プロキシ全体の判定**で、相手ごとではない。
const MAX_ANOMALIES: usize = 3;

/// 添える時系列 (T14.22) の窓の数 (5 分 × 12 = 直近 1 時間)。
const SERIES_WINDOWS: usize = 12;

/// 応答に写す名前の長さ (バイト)。個票のリングは既に 45〜80 B で切ってあるが、
/// ホスト表の鍵と問い合わせ文字列は長さの上限を持たないのでここで切る。
const MAX_NAME: usize = 80;

/// `/explain?host=<name>` / `/explain?client=<ip>` — 1 相手ぶんの説明。
///
/// どちらも書いていなければ 400 (`/lookup` と同じ方針: 引数が要る口は案内を返す)。
pub fn explain(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    let params = parse_query(query.unwrap_or(""));
    let value = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let host = value("host");
    let client = value("client");
    if !host.is_empty() {
        (200, "application/json", host_body(ep, &host))
    } else if !client.is_empty() {
        (200, "application/json", client_body(ep, &client))
    } else {
        (
            400,
            "application/json",
            "{\"error\":\"use /explain?host=<name[:port]> or /explain?client=<ip>\"}".to_string(),
        )
    }
}

/// `scheme://host:port` の鍵 (`connect://` / `http://` / `blocked://` / `loop://`) から
/// ホストとポートを取り出す。個票の `host:port` も同じ関数を通す (前綴りが無いだけ)。
fn key_host_port(key: &str) -> (&str, Option<u16>) {
    let rest = match key.find("://") {
        Some(i) => &key[i + 3..],
        None => key,
    };
    let rest = match rest.find('/') {
        Some(i) => &rest[..i],
        None => rest,
    };
    crate::net::split_host_port_ref(rest)
}

/// 鍵 (または個票の宛先) が探しているホストかどうか。
///
/// **ポートを書いていなければ全ポートをまとめる**、書いてあればそのポートだけ。
/// 名前の大小は区別しない (DNS と同じ)。
fn same_host(key: &str, want: &str, want_port: Option<u16>) -> bool {
    let (host, port) = key_host_port(key);
    if !host.eq_ignore_ascii_case(want) {
        return false;
    }
    match want_port {
        Some(w) => port == Some(w),
        None => true,
    }
}

/// 2 つのホスト別統計を足す (ポート違い・`connect://` と `http://` をまとめるため)。
///
/// 全部の欄が `pub` なので `crates/metrics` には口を足していない。分位点は区間の
/// 件数 (`buckets`) を足せばそのまま出る ([`HostStats::quantile_ms`])。
fn merge_into(acc: &mut HostStats, s: &HostStats) {
    acc.requests = acc.requests.saturating_add(s.requests);
    acc.hits = acc.hits.saturating_add(s.hits);
    acc.misses = acc.misses.saturating_add(s.misses);
    acc.bypass = acc.bypass.saturating_add(s.bypass);
    acc.errors = acc.errors.saturating_add(s.errors);
    acc.blocked = acc.blocked.saturating_add(s.blocked);
    acc.bytes = acc.bytes.saturating_add(s.bytes);
    // 向き別 (T14.26)。`bytes` と同じく足すだけ
    acc.bytes_in = acc.bytes_in.saturating_add(s.bytes_in);
    acc.bytes_out = acc.bytes_out.saturating_add(s.bytes_out);
    acc.timed = acc.timed.saturating_add(s.timed);
    acc.duration_ms_sum = acc.duration_ms_sum.saturating_add(s.duration_ms_sum);
    acc.duration_ms_max = acc.duration_ms_max.max(s.duration_ms_max);
    for (a, b) in acc.buckets.iter_mut().zip(s.buckets) {
        *a = a.saturating_add(b);
    }
    acc.last_seen = acc.last_seen.max(s.last_seen);
    acc.dns_ms_sum = acc.dns_ms_sum.saturating_add(s.dns_ms_sum);
    acc.dns_misses = acc.dns_misses.saturating_add(s.dns_misses);
    acc.connect_ms_sum = acc.connect_ms_sum.saturating_add(s.connect_ms_sum);
    acc.v4_wins = acc.v4_wins.saturating_add(s.v4_wins);
    acc.v6_wins = acc.v6_wins.saturating_add(s.v6_wins);
    for (a, b) in acc.errors_by_cause.iter_mut().zip(s.errors_by_cause) {
        *a = a.saturating_add(b);
    }
    acc.rtt_us_sum = acc.rtt_us_sum.saturating_add(s.rtt_us_sum);
    acc.rtt_samples = acc.rtt_samples.saturating_add(s.rtt_samples);
    acc.retrans = acc.retrans.saturating_add(s.retrans);
    if s.rtt_us_min > 0 && (acc.rtt_us_min == 0 || s.rtt_us_min < acc.rtt_us_min) {
        acc.rtt_us_min = s.rtt_us_min;
    }
}

/// `a / b` (b が 0 なら 0.0)。
fn per(a: u64, b: u64) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}

/// 配列を 1 つ書く (`MAX_BODY` に収まる分だけ。`array_within` の 64 KiB 版)。
fn array_within(out: &mut String, items: impl IntoIterator<Item = String>) -> bool {
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
    cut
}

/// `/explain?host=<name[:port]>` の本体。
fn host_body(ep: &Endpoint<'_>, query: &str) -> String {
    let (want, want_port) = {
        let (h, p) = crate::net::split_host_port_ref(query);
        (h.to_string(), p)
    };
    // (1) ホスト別の統計。ポートを書いていなければ全ポート・両方の経路をまとめる
    let mut stats = HostStats::default();
    let mut keys: Vec<(String, u64)> = Vec::new();
    let (mut connects, mut forwards) = (0u64, 0u64);
    for (key, s) in ep
        .metrics
        .hosts_sorted_by(crate::metrics::HostSort::Requests)
    {
        if !same_host(&key, &want, want_port) {
            continue;
        }
        if key.starts_with("connect://") {
            connects = connects.saturating_add(s.requests);
        } else if key.starts_with("http://") || key.starts_with("https://") {
            forwards = forwards.saturating_add(s.requests);
        }
        merge_into(&mut stats, &s);
        keys.push((key, s.requests));
    }
    let keys_total = keys.len();
    keys.truncate(MAX_KEYS);
    // (2) 名前解決の表の行 (warm か、残り TTL、答え)
    let dns_row = crate::dns::table(crate::dns::DnsSort::Host)
        .into_iter()
        .find(|r| r.host.eq_ignore_ascii_case(&want));
    // (3) 閉じた接続の個票 (このホスト宛ての直近 10 本)。絞りはリングの外
    // (`select` の鍵は接続元。ここは 1 要求に 1 回しか通らない口なので写してから絞る)
    let (closed, _) = ep.metrics.closed.select(0, "");
    let rows: Vec<&RecentEntry> = closed
        .iter()
        .filter(|e| same_host(&e.target, &want, want_port))
        .collect();
    let rows_matched = rows.len();
    // (4) エラーの個票 (直近 5 件)
    let (errors, _) = ep.metrics.errors.recent(MAX_ERRORS);
    let errs: Vec<&crate::recent::ErrorEntry> = errors
        .iter()
        .filter(|e| same_host(&e.target, &want, want_port))
        .collect();
    let errs_matched = errs.len();
    let known = !keys.is_empty() || dns_row.is_some() || rows_matched > 0 || errs_matched > 0;

    let mut out = String::with_capacity(8192);
    let _ = write!(
        out,
        "{{\"kind\":\"host\",\"query\":\"{}\",\"host\":\"{}\",\"port\":{},\"known\":{},\"at\":{},\"keys\":[",
        crate::json::escape(&clip(query)),
        crate::json::escape(&clip(&want)),
        match want_port {
            Some(p) => p.to_string(),
            None => "null".to_string(),
        },
        known,
        crate::cache::now_epoch(),
    );
    for (i, (key, n)) in keys.iter().enumerate() {
        let _ = write!(
            out,
            "{}{{\"key\":\"{}\",\"requests\":{}}}",
            if i == 0 { "" } else { "," },
            crate::json::escape(&clip(key)),
            n
        );
    }
    let _ = write!(
        out,
        "],\"keys_total\":{},\"stats\":{{{}}},\"errors_by_cause\":{{",
        keys_total,
        crate::metrics::stats_json(&stats, true)
    );
    for (i, (name, n)) in ERR_CAUSE_NAMES
        .iter()
        .zip(stats.errors_by_cause)
        .enumerate()
    {
        let _ = write!(out, "{}\"{}\":{}", if i == 0 { "" } else { "," }, name, n);
    }
    out.push_str("},\"dns\":");
    match &dns_row {
        Some(r) => out.push_str(&r.to_json()),
        None => out.push_str("null"),
    }
    out.push_str(",\"recent\":");
    let mut cut = array_within(&mut out, rows.iter().take(MAX_ROWS).map(|e| e.to_json()));
    let _ = write!(
        out,
        ",\"recent_matched\":{},\"recent_scanned\":{},\"errors\":",
        rows_matched,
        closed.len()
    );
    cut |= array_within(
        &mut out,
        errs.iter().take(MAX_ERROR_ROWS).map(|e| e.to_json()),
    );
    let _ = write!(out, ",\"errors_matched\":{},\"series\":", errs_matched);
    out.push_str(&series_json(ep, keys.first().map(|(k, _)| k.as_str())));
    out.push_str(",\"anomalies\":");
    cut |= array_within(&mut out, anomalies());
    let (summary, numbers) = host_summary(
        &stats,
        connects,
        forwards,
        dns_row.as_ref(),
        rows_matched,
        errs_matched,
        known,
    );
    let _ = write!(
        out,
        ",\"numbers\":{{{}}},\"summary\":\"{}\",\"truncated\":{}}}",
        numbers,
        crate::json::escape(&summary),
        cut
    );
    out
}

/// `/explain?client=<ip>` の本体。
fn client_body(ep: &Endpoint<'_>, ip: &str) -> String {
    // (1) 接続元の行 (`/clients` と同じ形。鍵は完全一致 — `/recent?client=` と同じ)
    let row = ep
        .metrics
        .clients_sorted_by(crate::metrics::ClientSort::Requests)
        .into_iter()
        .find(|(k, _)| k == ip);
    // (2) その接続元の閉じた接続 (リングの鍵の内側で絞れる)
    let (closed, _) = ep.metrics.closed.select(0, ip);
    let rows_matched = closed.len();
    // (3) エラーの個票
    let (errors, _) = ep.metrics.errors.recent(MAX_ERRORS);
    let errs: Vec<&crate::recent::ErrorEntry> = errors.iter().filter(|e| e.client == ip).collect();
    let errs_matched = errs.len();
    let known = row.is_some() || rows_matched > 0 || errs_matched > 0;

    let mut out = String::with_capacity(8192);
    let _ = write!(
        out,
        "{{\"kind\":\"client\",\"query\":\"{}\",\"client\":\"{}\",\"known\":{},\"at\":{},\"stats\":",
        crate::json::escape(&clip(ip)),
        crate::json::escape(&clip(ip)),
        known,
        crate::cache::now_epoch(),
    );
    match &row {
        Some((k, s)) => out.push_str(&s.to_json(k)),
        None => out.push_str("null"),
    }
    out.push_str(",\"recent\":");
    let mut cut = array_within(
        &mut out,
        closed.iter().take(MAX_ROWS).map(RecentEntry::to_json),
    );
    let _ = write!(out, ",\"recent_matched\":{},\"errors\":", rows_matched);
    cut |= array_within(
        &mut out,
        errs.iter().take(MAX_ERROR_ROWS).map(|e| e.to_json()),
    );
    let _ = write!(out, ",\"errors_matched\":{},\"anomalies\":", errs_matched);
    cut |= array_within(&mut out, anomalies());
    let (summary, numbers) = client_summary(
        row.as_ref().map(|(_, s)| s),
        rows_matched,
        errs_matched,
        known,
    );
    let _ = write!(
        out,
        ",\"numbers\":{{{}}},\"summary\":\"{}\",\"truncated\":{}}}",
        numbers,
        crate::json::escape(&summary),
        cut
    );
    out
}

/// ホストの 1 行の判定と、同じ内容の数字の欄。
///
/// **段階を足し算の形で並べるだけ** (`確立 259.0 ms = 名前解決 9.0 + 接続 250.0 +
/// プロキシ側 0.3 ms`)。どれが原因かは書かない — 数字の隣に RTT を置くので、
/// 「接続 250 ms は RTT 250 ms とほぼ同じ = 物理」は読む人が言える。
fn host_summary(
    s: &HostStats,
    connects: u64,
    forwards: u64,
    dns_row: Option<&crate::dns::TableRow>,
    recent: usize,
    errors: usize,
    known: bool,
) -> (String, String) {
    // 時間の平均は「時間を測れた要求」で割る (段階の 3 つを同じ分母に揃える)
    let n = if s.timed > 0 { s.timed } else { s.requests };
    let avg = s.avg_ms();
    let dns_ms = per(s.dns_ms_sum, n);
    let connect_ms = per(s.connect_ms_sum, n);
    let proxy_ms = (avg - dns_ms - connect_ms).max(0.0);
    let (p50, p95) = (s.quantile_ms(0.5), s.quantile_ms(0.95));
    let rtt = s.rtt_avg_ms();
    let miss_rate = per(s.dns_misses, s.requests);
    let label = if connects > 0 && forwards == 0 {
        "確立"
    } else if forwards > 0 && connects == 0 {
        "応答"
    } else {
        "所要"
    };
    // 引き算で出る残り (`proxy_ms`) の呼び名。CONNECT の `確立` は名前解決 + 接続 +
    // プロキシ側の待ちで閉じているが、**forward の平均は応答を返し終えるまで**なので、
    // 残りには**オリジンが考えていた時間と本文を流した時間も入る**。同じ名前で呼ぶと
    // 「プロキシが 5 ms 待たせた」と読めてしまうので、forward が混ざる相手では言い方を変える
    let rest = if forwards == 0 {
        "プロキシ側"
    } else {
        "残り (プロキシ側の待ち + オリジンの応答と本文)"
    };
    let mut text = String::with_capacity(256);
    if !known {
        // 探した場所を全部並べて終わり (無い相手に「表に行は無い」を続けても読む手が増えるだけ)
        text.push_str(
            "この相手の記録は 1 件も無い (ホスト別の統計・名前解決の表・閉じた接続の個票・エラーのどれにも出てこない)。",
        );
        return (
            text,
            host_numbers(s, connects, forwards, dns_row, recent, errors),
        );
    }
    if s.requests == 0 {
        text.push_str("ホスト別の統計には 1 要求も無い (名前解決の表か個票にだけ居る)。");
    } else {
        let _ = write!(
            text,
            "{} 要求 (CONNECT {} / forward {})。{}の平均 {:.1} ms = 名前解決 {:.1} + 接続 {:.1} + {} {:.1} ms。p50 {:.1} / p95 {:.1} ms。",
            s.requests,
            connects,
            forwards,
            label,
            avg,
            dns_ms,
            connect_ms,
            rest,
            proxy_ms,
            p50,
            p95
        );
        match rtt {
            Some(v) => {
                let _ = write!(
                    text,
                    "カーネルの RTT {:.1} ms (標本 {}、再送 {})。",
                    v, s.rtt_samples, s.retrans
                );
            }
            None => text.push_str("カーネルの RTT は記録なし。"),
        }
        let _ = write!(
            text,
            "名前解決は OS に {} 回 ({:.2} 回/要求、合計 {} ms)。エラー {} 件",
            s.dns_misses, miss_rate, s.dns_ms_sum, s.errors
        );
        match top_cause(s) {
            Some((name, count)) => {
                let _ = write!(text, " (最多の原因 {} が {} 件)。", name, count);
            }
            None => text.push('。'),
        }
    }
    match dns_row {
        Some(r) => {
            let _ = write!(
                text,
                "名前解決の表: {}、残り TTL {} 秒、答え {} 件、OS への問い合わせ {} 回。",
                if r.warm { "warm" } else { "warm ではない" },
                r.ttl_left,
                r.addr_count,
                r.misses
            );
        }
        None => text.push_str(
            "名前解決の表に行は無い (IP リテラル宛てか、まだ引いていないか、追い出されたか)。",
        ),
    }
    let _ = write!(
        text,
        "閉じた接続の個票 {} 本、エラーの個票 {} 件 (段階と閉じた理由は recent / errors の欄)。",
        recent, errors
    );

    (
        text,
        host_numbers(s, connects, forwards, dns_row, recent, errors),
    )
}

/// ホストの数字の欄 (`summary` の文章と**同じ値**を機械が読む形で)。
fn host_numbers(
    s: &HostStats,
    connects: u64,
    forwards: u64,
    dns_row: Option<&crate::dns::TableRow>,
    recent: usize,
    errors: usize,
) -> String {
    let n = if s.timed > 0 { s.timed } else { s.requests };
    let avg = s.avg_ms();
    let dns_ms = per(s.dns_ms_sum, n);
    let connect_ms = per(s.connect_ms_sum, n);
    let mut numbers = String::with_capacity(512);
    let _ = write!(
        numbers,
        "\"requests\":{},\"connect_requests\":{},\"forward_requests\":{},\"timed\":{},\"avg_ms\":{:.1},\"dns_ms\":{:.1},\"connect_ms\":{:.1},\"proxy_ms\":{:.1},\"p50_ms\":{:.1},\"p95_ms\":{:.1},\"max_ms\":{},\"rtt_ms\":{},\"rtt_samples\":{},\"retrans\":{},\"dns_misses\":{},\"dns_misses_per_request\":{:.2},\"dns_ms_sum\":{},\"errors\":{},\"error_rate\":{:.3},\"blocked\":{},\"bytes\":{},\"bytes_in\":{},\"bytes_out\":{},\"recent\":{},\"recent_errors\":{},\"dns_entry\":{}",
        s.requests,
        connects,
        forwards,
        s.timed,
        avg,
        dns_ms,
        connect_ms,
        (avg - dns_ms - connect_ms).max(0.0),
        s.quantile_ms(0.5),
        s.quantile_ms(0.95),
        s.duration_ms_max,
        match s.rtt_avg_ms() {
            Some(v) => format!("{:.1}", v),
            None => "null".to_string(),
        },
        s.rtt_samples,
        s.retrans,
        s.dns_misses,
        per(s.dns_misses, s.requests),
        s.dns_ms_sum,
        s.errors,
        s.error_rate(),
        s.blocked,
        s.bytes,
        s.bytes_in,
        s.bytes_out,
        recent,
        errors,
        dns_row.is_some()
    );
    numbers
}

/// 接続元の 1 行の判定と、同じ内容の数字の欄 (ホストと同じ形)。
///
/// 接続元には名前解決も接続の段階も無い ([`crate::metrics::stats_json`] の `detail` が
/// ホスト別だけなのと同じ理由) ので、並べるのは本数・応答時間・RTT (利用者 → プロキシ)・
/// 宛先の種類・断った本数。
fn client_summary(
    row: Option<&crate::metrics::ClientStats>,
    recent: usize,
    errors: usize,
    known: bool,
) -> (String, String) {
    let empty = crate::metrics::ClientStats::default();
    let c = row.unwrap_or(&empty);
    let s = &c.stats;
    let avg = s.avg_ms();
    let (p50, p95) = (s.quantile_ms(0.5), s.quantile_ms(0.95));
    let rtt = s.rtt_avg_ms();
    let mut text = String::with_capacity(256);
    if !known {
        text.push_str(
            "この接続元の記録は 1 件も無い (接続元の統計・閉じた接続の個票・エラーのどれにも出てこない)。",
        );
        return (text, client_numbers(c, recent, errors));
    }
    {
        let _ = write!(
            text,
            "{} 要求。応答まで (CONNECT は確立まで) の平均 {:.1} ms、p50 {:.1} / p95 {:.1} ms。",
            s.requests, avg, p50, p95
        );
        match rtt {
            Some(v) => {
                let _ = write!(
                    text,
                    "カーネルの RTT (利用者 → プロキシ) {:.1} ms (標本 {}、再送 {})。",
                    v, s.rtt_samples, s.retrans
                );
            }
            None => text.push_str("カーネルの RTT は記録なし。"),
        }
        let _ = write!(
            text,
            "{}、宛先 {} 種 (IP リテラル宛て {} 要求、443 / 80 以外 {} 要求)。エラー {} 件、断った接続 {} 本。",
            match c.agent() {
                Some(a) => format!("User-Agent は「{}」", crate::recent::clip(a, 64)),
                None => "User-Agent は記録なし".to_string(),
            },
            c.distinct_targets(),
            c.literal_targets,
            c.nonstandard_ports,
            s.errors,
            c.rejected
        );
    }
    let _ = write!(
        text,
        "閉じた接続の個票 {} 本、エラーの個票 {} 件 (段階と閉じた理由は recent / errors の欄)。",
        recent, errors
    );

    (text, client_numbers(c, recent, errors))
}

/// 接続元の数字の欄 (`summary` の文章と**同じ値**)。
fn client_numbers(c: &crate::metrics::ClientStats, recent: usize, errors: usize) -> String {
    let s = &c.stats;
    let mut numbers = String::with_capacity(512);
    let _ = write!(
        numbers,
        "\"requests\":{},\"timed\":{},\"avg_ms\":{:.1},\"p50_ms\":{:.1},\"p95_ms\":{:.1},\"max_ms\":{},\"rtt_ms\":{},\"rtt_samples\":{},\"retrans\":{},\"errors\":{},\"error_rate\":{:.3},\"blocked\":{},\"bytes\":{},\"bytes_in\":{},\"bytes_out\":{},\"distinct_targets\":{},\"literal_targets\":{},\"nonstandard_ports\":{},\"rejected\":{},\"first_seen\":{},\"recent\":{},\"recent_errors\":{}",
        s.requests,
        s.timed,
        s.avg_ms(),
        s.quantile_ms(0.5),
        s.quantile_ms(0.95),
        s.duration_ms_max,
        match s.rtt_avg_ms() {
            Some(v) => format!("{:.1}", v),
            None => "null".to_string(),
        },
        s.rtt_samples,
        s.retrans,
        s.errors,
        s.error_rate(),
        s.blocked,
        s.bytes,
        s.bytes_in,
        s.bytes_out,
        c.distinct_targets(),
        c.literal_targets,
        c.nonstandard_ports,
        c.rejected,
        c.first_seen,
        recent,
        errors
    );
    numbers
}

/// いちばん多いエラーの原因 (1 件も無ければ `None`)。
fn top_cause(s: &HostStats) -> Option<(&'static str, u64)> {
    ERR_CAUSE_NAMES
        .iter()
        .zip(s.errors_by_cause)
        .filter(|(_, n)| *n > 0)
        .max_by_key(|(_, n)| *n)
        .map(|(name, n)| (*name, n))
}

/// そのホストの時系列 (T14.22) の**直近 1 時間ぶんだけ** (上位 16 に居なければ `null`)。
///
/// 24 時間ぶん (288 標本) は `/hosts/series?host=<鍵>` で読む口が既にあるので、
/// ここは「いま遅いのか、ずっと遅いのか」が分かる長さに切って添える。
fn series_json(ep: &Endpoint<'_>, key: Option<&str>) -> String {
    let Some(key) = key else {
        return "null".to_string();
    };
    let view = ep.metrics.host_series(Some(key), 1);
    let Some(s) = view.series.first() else {
        return "null".to_string();
    };
    let start = s.rows.len().saturating_sub(SERIES_WINDOWS);
    let mut out = String::with_capacity(1024);
    let _ = write!(
        out,
        "{{\"host\":\"{}\",\"keys\":[{}],\"window_secs\":{},\"t0\":{},\"samples\":[",
        crate::json::escape(&clip(&s.host)),
        crate::hostseries::FIELD_NAMES
            .iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(","),
        view.window_secs,
        view.t0 + start as u64 * view.window_secs,
    );
    for (i, r) in s.rows[start..].iter().enumerate() {
        let _ = write!(
            out,
            "{}[{},{},{},{},{}]",
            if i == 0 { "" } else { "," },
            r[0],
            r[1],
            r[2],
            r[3],
            r[4]
        );
    }
    out.push_str("]}");
    out
}

/// 直近の異常 (T14.23) を新しい順に [`MAX_ANOMALIES`] 件。
///
/// **プロキシ全体の判定**で相手ごとではない (`/events` の `anomaly` そのもの)。
/// 「この宛先が遅かった時間帯に、プロキシ全体でも何か起きていたか」を隣に置くため。
fn anomalies() -> Vec<String> {
    let (events, _) = crate::events::select(0, crate::events::MAX_EVENTS);
    events
        .iter()
        .filter(|e| e.kind == crate::events::EventKind::Anomaly)
        .take(MAX_ANOMALIES)
        .map(crate::events::Event::to_json)
        .collect()
}

/// 名前を [`MAX_NAME`] バイトに切る (個票のリングと同じ作法)。
fn clip(s: &str) -> String {
    crate::recent::clip(s, MAX_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 鍵と個票の宛先から、ホストとポートを同じ形で取れること。
    #[test]
    fn the_key_and_the_target_split_the_same_way() {
        assert_eq!(key_host_port("connect://a.test:443"), ("a.test", Some(443)));
        assert_eq!(key_host_port("http://a.test:80/x"), ("a.test", Some(80)));
        assert_eq!(key_host_port("blocked://ads.test"), ("ads.test", None));
        assert_eq!(key_host_port("a.test:8080"), ("a.test", Some(8080)));
        assert_eq!(key_host_port("[::1]:443"), ("::1", Some(443)));
    }

    /// ポートを書かなければ全ポート、書けばそのポートだけ。大小は区別しない。
    #[test]
    fn a_name_without_a_port_gathers_every_port() {
        assert!(same_host("connect://a.test:443", "a.test", None));
        assert!(same_host("http://A.TEST:80", "a.test", None));
        assert!(same_host("connect://a.test:443", "a.test", Some(443)));
        assert!(!same_host("connect://a.test:443", "a.test", Some(8443)));
        // ポートを持たない鍵は「ポート指定あり」では混ぜない
        assert!(!same_host("blocked://a.test", "a.test", Some(443)));
        assert!(same_host("blocked://a.test", "a.test", None));
        assert!(!same_host("connect://b.test:443", "a.test", None));
    }

    /// 足し算で分位点と RTT の最小が壊れないこと (ポート違いをまとめる道)。
    #[test]
    fn merging_two_ports_keeps_the_quantiles_and_the_minimum_rtt() {
        let two = HostStats {
            requests: 2,
            timed: 2,
            duration_ms_sum: 20,
            duration_ms_max: 10,
            buckets: {
                let mut b = [0u64; crate::metrics::LATENCY_BOUNDS_MS.len() + 1];
                b[0] = 2;
                b
            },
            ..HostStats::default()
        };
        let a = HostStats {
            rtt_us_min: 500,
            rtt_us_sum: 1000,
            rtt_samples: 2,
            ..two.clone()
        };
        let b = HostStats {
            rtt_us_min: 300,
            rtt_us_sum: 600,
            rtt_samples: 2,
            ..two
        };
        let mut acc = HostStats::default();
        merge_into(&mut acc, &a);
        merge_into(&mut acc, &b);
        assert_eq!(acc.requests, 4);
        assert_eq!(acc.timed, 4);
        assert_eq!(acc.buckets[0], 4);
        assert_eq!(acc.avg_ms(), 10.0);
        assert_eq!(acc.rtt_us_min, 300);
        assert_eq!(acc.rtt_samples, 4);
        // 片方だけ RTT を持っているときは、その値がそのまま最小になる
        let mut only = HostStats::default();
        merge_into(&mut only, &HostStats::default());
        merge_into(&mut only, &a);
        assert_eq!(only.rtt_us_min, 500);
    }

    /// 1 行の判定が段階を足し算の形で並べ、数字の欄と同じ値を持つこと。
    #[test]
    fn the_one_line_verdict_lines_up_the_stages() {
        let s = HostStats {
            requests: 10,
            timed: 10,
            duration_ms_sum: 2590,
            duration_ms_max: 300,
            buckets: {
                let mut b = [0u64; crate::metrics::LATENCY_BOUNDS_MS.len() + 1];
                // 区間の最後 (上限なし) に入れて、分位点が最大値で頭打ちになる道を通す
                b[crate::metrics::LATENCY_BOUNDS_MS.len()] = 10;
                b
            },
            dns_ms_sum: 90,
            dns_misses: 3,
            connect_ms_sum: 2500,
            rtt_us_sum: 2_500_000,
            rtt_us_min: 250_000,
            rtt_samples: 10,
            ..HostStats::default()
        };
        let (text, numbers) = host_summary(&s, 10, 0, None, 3, 0, true);
        assert!(text.contains("確立の平均 259.0 ms"), "{}", text);
        assert!(
            text.contains("名前解決 9.0 + 接続 250.0 + プロキシ側 0.0 ms"),
            "{}",
            text
        );
        assert!(text.contains("カーネルの RTT 250.0 ms"), "{}", text);
        assert!(text.contains("名前解決の表に行は無い"), "{}", text);
        assert!(numbers.contains("\"avg_ms\":259.0"), "{}", numbers);
        assert!(numbers.contains("\"connect_ms\":250.0"), "{}", numbers);
        assert!(numbers.contains("\"dns_ms\":9.0"), "{}", numbers);
        assert!(numbers.contains("\"rtt_ms\":250.0"), "{}", numbers);
        assert!(numbers.contains("\"dns_entry\":false"), "{}", numbers);
        // forward だけなら「応答」、混ざれば「所要」。引き算の残りの呼び名も変わる
        // (forward の平均はオリジンの応答と本文まで含むので「プロキシ側」とは呼べない)
        let (fwd, _) = host_summary(&s, 0, 10, None, 0, 0, true);
        assert!(fwd.contains("応答の平均"), "{}", fwd);
        assert!(
            fwd.contains("残り (プロキシ側の待ち + オリジンの応答と本文) 0.0 ms"),
            "{}",
            fwd
        );
        let (both, _) = host_summary(&s, 5, 5, None, 0, 0, true);
        assert!(both.contains("所要の平均"), "{}", both);
        // 知らない相手は「1 件も無い」と言う
        let (none, numbers) = host_summary(&HostStats::default(), 0, 0, None, 0, 0, false);
        assert!(none.contains("記録は 1 件も無い"), "{}", none);
        assert!(numbers.contains("\"requests\":0"), "{}", numbers);
    }
}
