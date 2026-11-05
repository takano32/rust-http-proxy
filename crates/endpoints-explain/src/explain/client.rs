//! `/explain?client=<ip>` の材料 ([`ClientFacts::gather`]) と組み立て ([`ClientFacts::render`])。
//! 割り方はホストの側 (`super::host`) と同じ: 読むのは `gather` だけ、`render` は純粋。

use std::fmt::Write as _;

use super::{MAX_ERROR_ROWS, MAX_ROWS, anomalies, array_within, clip};
use crate::Endpoint;
use crate::metrics::{ClientStats, SCHEMA_HEAD};
use crate::recent::{ErrorEntry, MAX_ERRORS, RecentEntry};

/// `/explain?client=<ip>` の本体。
pub(super) fn client_body(ep: &Endpoint<'_>, ip: &str) -> String {
    ClientFacts::gather(ep, ip).render()
}

/// 1 相手 (接続元) について、上の口から集めた材料。
pub(super) struct ClientFacts {
    /// 問い合わせの IP (`"query"` と `"client"` の両方に出る)
    pub(super) ip: String,
    /// 接続元の行 (`/clients` と同じ形。鍵は完全一致 — `/recent?client=` と同じ)
    pub(super) row: Option<(String, ClientStats)>,
    /// その接続元の閉じた接続 (新しい順。出すのは [`MAX_ROWS`] 本まで)
    pub(super) recent: Vec<RecentEntry>,
    /// その接続元のエラーの個票 (出すのは [`MAX_ERROR_ROWS`] 件まで)
    pub(super) errors: Vec<ErrorEntry>,
    /// 添える異常 (JSON の要素)
    pub(super) anomalies: Vec<String>,
    /// 組んだ時刻 (epoch 秒)
    pub(super) at: u64,
}

impl ClientFacts {
    /// 表とリングを読んで材料を集める。
    pub(super) fn gather(ep: &Endpoint<'_>, ip: &str) -> ClientFacts {
        // (1) 接続元の行 (`/clients` と同じ形。鍵は完全一致 — `/recent?client=` と同じ)
        let row = ep
            .metrics
            .clients_sorted_by(crate::metrics::ClientSort::Requests)
            .into_iter()
            .find(|(k, _)| k == ip);
        // (2) その接続元の閉じた接続 (リングの鍵の内側で絞れる)
        let (recent, _) = ep.metrics.closed.select(0, ip);
        // (3) エラーの個票
        let (errors, _) = ep.metrics.errors.recent(MAX_ERRORS);
        let errors: Vec<ErrorEntry> = errors.into_iter().filter(|e| e.client == ip).collect();
        ClientFacts {
            ip: ip.to_string(),
            row,
            recent,
            errors,
            anomalies: anomalies(),
            at: crate::cache::now_epoch(),
        }
    }

    /// 探した場所のどこかに居たか (居なくても 200 のまま `"known":false`)。
    fn known(&self) -> bool {
        self.row.is_some() || !self.recent.is_empty() || !self.errors.is_empty()
    }

    /// 材料を 1 枚の JSON に並べる (鍵の並びは固定)。
    pub(super) fn render(&self) -> String {
        let known = self.known();
        let mut out = String::with_capacity(8192);
        // 応答の形の版は**いちばん先頭の鍵** (T14.49)
        out.push_str(SCHEMA_HEAD);
        let _ = write!(
            out,
            "\"kind\":\"client\",\"query\":\"{}\",\"client\":\"{}\",\"known\":{},\"at\":{},\"stats\":",
            crate::json::escape(&clip(&self.ip)),
            crate::json::escape(&clip(&self.ip)),
            known,
            self.at,
        );
        match &self.row {
            Some((k, s)) => out.push_str(&s.to_json(k)),
            None => out.push_str("null"),
        }
        out.push_str(",\"recent\":");
        let mut cut = array_within(
            &mut out,
            self.recent.iter().take(MAX_ROWS).map(RecentEntry::to_json),
        );
        let _ = write!(out, ",\"recent_matched\":{},\"errors\":", self.recent.len());
        cut |= array_within(
            &mut out,
            self.errors.iter().take(MAX_ERROR_ROWS).map(|e| e.to_json()),
        );
        let _ = write!(
            out,
            ",\"errors_matched\":{},\"anomalies\":",
            self.errors.len()
        );
        cut |= array_within(&mut out, self.anomalies.iter().cloned());
        let (summary, numbers) = client_summary(
            self.row.as_ref().map(|(_, s)| s),
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

/// 接続元の 1 行の判定と、同じ内容の数字の欄 (ホストと同じ形)。
///
/// 接続元には名前解決も接続の段階も無い ([`crate::metrics::stats_json`] の `detail` が
/// ホスト別だけなのと同じ理由) ので、並べるのは本数・応答時間・RTT (利用者 → プロキシ)・
/// 宛先の種類・断った本数。
pub(super) fn client_summary(
    row: Option<&ClientStats>,
    recent: usize,
    errors: usize,
    known: bool,
) -> (String, String) {
    let empty = ClientStats::default();
    let c = row.unwrap_or(&empty);
    let s = c.stats();
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
            c.literal_targets(),
            c.nonstandard_ports(),
            s.errors,
            c.rejected()
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
fn client_numbers(c: &ClientStats, recent: usize, errors: usize) -> String {
    let s = c.stats();
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
        c.literal_targets(),
        c.nonstandard_ports(),
        c.rejected(),
        c.first_seen(),
        recent,
        errors
    );
    numbers
}
