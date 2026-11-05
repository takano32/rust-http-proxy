//! `/explain?host=<name[:port]>` の材料 ([`HostFacts::gather`]) と組み立て ([`HostFacts::render`])。
//!
//! 表とリングを読むのは `gather` だけ。`render` は集めた材料を JSON に並べるだけで、時計も
//! リングも読まないので、手で作った材料で**鍵の並びをそのまま**単体試験で縛れる。

use std::fmt::Write as _;

use super::{MAX_ERROR_ROWS, MAX_KEYS, MAX_ROWS, SERIES_WINDOWS, anomalies, array_within, clip};
use crate::Endpoint;
use crate::dns::TableRow;
use crate::metrics::{DNS_CAUSE_NAMES, ERR_CAUSE_NAMES, HostStats, SCHEMA_HEAD};
use crate::recent::{ErrorEntry, MAX_ERRORS, RecentEntry};

/// `/explain?host=<name[:port]>` の本体。
pub(super) fn host_body(ep: &Endpoint<'_>, query: &str) -> String {
    HostFacts::gather(ep, query).render()
}

/// 1 相手 (ホスト) について、上の口から集めた材料。
pub(super) struct HostFacts {
    /// 問い合わせそのもの (`"query"`)
    pub(super) query: String,
    /// 探している名前と、書いてあればそのポート
    pub(super) want: String,
    pub(super) want_port: Option<u16>,
    /// ポート違い・`connect://` と `http://` をまとめたホスト別の統計
    pub(super) stats: HostStats,
    /// 一致した鍵と要求数 (要求数順、[`MAX_KEYS`] まで)
    pub(super) keys: Vec<(String, u64)>,
    /// 一致した鍵の数 (切る前)
    pub(super) keys_total: usize,
    /// CONNECT と forward の要求数
    pub(super) connects: u64,
    pub(super) forwards: u64,
    /// 名前解決の表の行
    pub(super) dns_row: Option<TableRow>,
    /// 一致した閉じた接続の個票 (新しい順。出すのは [`MAX_ROWS`] 本まで)
    pub(super) recent: Vec<RecentEntry>,
    /// 絞る前に読んだ個票の数 (`"recent_scanned"`)
    pub(super) recent_scanned: usize,
    /// 一致したエラーの個票 (出すのは [`MAX_ERROR_ROWS`] 件まで)
    pub(super) errors: Vec<ErrorEntry>,
    /// 添える時系列の JSON (無ければ `null`)
    pub(super) series: String,
    /// 添える異常 (JSON の要素)
    pub(super) anomalies: Vec<String>,
    /// 組んだ時刻 (epoch 秒)
    pub(super) at: u64,
}

impl HostFacts {
    /// 表とリングを読んで材料を集める。
    pub(super) fn gather(ep: &Endpoint<'_>, query: &str) -> HostFacts {
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
        let recent_scanned = closed.len();
        let recent: Vec<RecentEntry> = closed
            .into_iter()
            .filter(|e| same_host(&e.target, &want, want_port))
            .collect();
        // (4) エラーの個票 (直近 5 件)
        let (errors, _) = ep.metrics.errors.recent(MAX_ERRORS);
        let errors: Vec<ErrorEntry> = errors
            .into_iter()
            .filter(|e| same_host(&e.target, &want, want_port))
            .collect();
        let series = series_json(ep, keys.first().map(|(k, _)| k.as_str()));
        HostFacts {
            query: query.to_string(),
            want,
            want_port,
            stats,
            keys,
            keys_total,
            connects,
            forwards,
            dns_row,
            recent,
            recent_scanned,
            errors,
            series,
            anomalies: anomalies(),
            at: crate::cache::now_epoch(),
        }
    }

    /// 探した場所のどこかに居たか (居なくても 200 のまま `"known":false`)。
    fn known(&self) -> bool {
        !self.keys.is_empty()
            || self.dns_row.is_some()
            || !self.recent.is_empty()
            || !self.errors.is_empty()
    }

    /// 材料を 1 枚の JSON に並べる (鍵の並びは固定)。
    pub(super) fn render(&self) -> String {
        let known = self.known();
        let mut out = String::with_capacity(8192);
        // 応答の形の版は**いちばん先頭の鍵** (T14.49)
        out.push_str(SCHEMA_HEAD);
        let _ = write!(
            out,
            "\"kind\":\"host\",\"query\":\"{}\",\"host\":\"{}\",\"port\":{},\"known\":{},\"at\":{},\"keys\":[",
            crate::json::escape(&clip(&self.query)),
            crate::json::escape(&clip(&self.want)),
            match self.want_port {
                Some(p) => p.to_string(),
                None => "null".to_string(),
            },
            known,
            self.at,
        );
        for (i, (key, n)) in self.keys.iter().enumerate() {
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
            self.keys_total,
            crate::metrics::stats_json(&self.stats, true)
        );
        // T25.0 の 2 列 (DNS の失敗の内訳) も**うしろに**並べる。先頭 8 つの名前と
        // 並びは変えないので、読む側は今までどおり `dns` … `other` で引ける
        for (i, (name, n)) in ERR_CAUSE_NAMES
            .iter()
            .zip(self.stats.errors_by_cause)
            .chain(DNS_CAUSE_NAMES.iter().zip(self.stats.dns_by_cause))
            .enumerate()
        {
            let _ = write!(out, "{}\"{}\":{}", if i == 0 { "" } else { "," }, name, n);
        }
        out.push_str("},\"dns\":");
        match &self.dns_row {
            Some(r) => out.push_str(&r.to_json()),
            None => out.push_str("null"),
        }
        out.push_str(",\"recent\":");
        let mut cut = array_within(
            &mut out,
            self.recent.iter().take(MAX_ROWS).map(|e| e.to_json()),
        );
        let _ = write!(
            out,
            ",\"recent_matched\":{},\"recent_scanned\":{},\"errors\":",
            self.recent.len(),
            self.recent_scanned
        );
        cut |= array_within(
            &mut out,
            self.errors.iter().take(MAX_ERROR_ROWS).map(|e| e.to_json()),
        );
        let _ = write!(out, ",\"errors_matched\":{},\"series\":", self.errors.len());
        out.push_str(&self.series);
        out.push_str(",\"anomalies\":");
        cut |= array_within(&mut out, self.anomalies.iter().cloned());
        let (summary, numbers) = host_summary(
            &self.stats,
            self.connects,
            self.forwards,
            self.dns_row.as_ref(),
            self.recent.len(),
            self.errors.len(),
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
}

/// `scheme://host:port` の鍵 (`connect://` / `http://` / `blocked://` / `loop://`) から
/// ホストとポートを取り出す。個票の `host:port` も同じ関数を通す (前綴りが無いだけ)。
pub(super) fn key_host_port(key: &str) -> (&str, Option<u16>) {
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
pub(super) fn same_host(key: &str, want: &str, want_port: Option<u16>) -> bool {
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
pub(super) fn merge_into(acc: &mut HostStats, s: &HostStats) {
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
    // DNS の失敗の内訳 (T25.0) も足す。**足さないと** `/explain` の `errors_by_cause` が
    // `dns` だけを出して内訳が 0 になる (`/status` の行と食い違う)
    for (a, b) in acc.dns_by_cause.iter_mut().zip(s.dns_by_cause) {
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

/// ホストの 1 行の判定と、同じ内容の数字の欄。
///
/// **段階を足し算の形で並べるだけ** (`確立 259.0 ms = 名前解決 9.0 + 接続 250.0 +
/// プロキシ側 0.3 ms`)。どれが原因かは書かない — 数字の隣に RTT を置くので、
/// 「接続 250 ms は RTT 250 ms とほぼ同じ = 物理」は読む人が言える。
pub(super) fn host_summary(
    s: &HostStats,
    connects: u64,
    forwards: u64,
    dns_row: Option<&TableRow>,
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
    dns_row: Option<&TableRow>,
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
