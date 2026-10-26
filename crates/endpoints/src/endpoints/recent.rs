//! 個票を読み出すエンドポイント: `/errors` `/connections` `/dns` `/log` `/hosts` (T13.4)、
//! `/recent` と `/snapshot` (閉じた接続と、1 回で全部。T14.4)、接続元の個票 `/clients` (T14.7)、
//! 山の写真 `/bursts` (T14.6)、接続元 1 つの追跡 `/trace` (T14.27)。
//!
//! `/status` (集計) と `/history` (時系列) では「**誰が・いつ・なぜ**」が読めない。
//! ここは「今この瞬間の中身」と「直近に起きたこと」を、集計に畳む前の形で出す口で、
//! どれも JSON・`Cache-Control: no-store`・`Connection: close` (組み立ては
//! [`super::handle`] が共通で行う)。時刻は epoch 秒。
//!
//! **`"persisted"` と `"restored"`** (`/recent` `/errors` `/bursts` `/events` `/log`。T14.9):
//! この 4 つのリングは 5 秒ごとに `$HOME/.rust-http-proxy.recent` (固定 4 MiB、統計の
//! `.rrd` とは別のファイル) へ追記され、次の起動で読み戻される。`persisted` がその可否、
//! `restored` が**再起動前から引き継いだ件数**。
//!
//! **応答は必ず [`MAX_BODY`] 以下**にする。件数の上限 (`?n=` / `?limit=`) とは別に
//! バイト数でも打ち切り、切ったときは `"truncated":true` を出す。上限を件数だけで
//! 決めると、長いホスト名や多いアドレスで簡単に越えてしまう
//! (`/dns` は 1 ホストに A / AAAA が 10 本以上返ることがある)。

use std::fmt::Write as _;
use std::time::Instant;

use super::{Endpoint, next_offset, offset_param, parse_query};
use crate::events::MAX_EVENTS;
use crate::metrics::SCHEMA_HEAD;
use crate::recent::{BurstShot, MAX_BURSTS, MAX_ERRORS, MAX_RECENT, RecentEntry};
use crate::trace::{MAX_PATH, MAX_TRACE};

mod snapshot;
#[cfg(test)]
mod tests;

pub use snapshot::{snapshot, snapshot_file, snapshot_with, snapshots};

/// 個票の応答 1 本の上限 (256 KiB)。監視が 1 分おきに引いても回線を埋めない大きさで、
/// `/errors` 500 件・`/connections` 240 件・`/hosts` 1,000 件のどれも収まる。
pub const MAX_BODY: usize = 256 * 1024;

/// 末尾 (`],"truncated":true,...}`) のために空けておくぶん。
const TRAILER: usize = 512;

/// 要素を上限のバイト数まで `[...]` に並べる。入り切らなかったらそこで止める。
/// 返すのは (書けた件数, 打ち切ったか)。
fn array_within(out: &mut String, items: impl IntoIterator<Item = String>) -> (usize, bool) {
    array_within_budget(out, items, MAX_BODY)
}

/// [`array_within`] の上限を `limit` にした版 (停止の雪像の `recent` だけが使う。T.28.20)。
fn array_within_budget(
    out: &mut String,
    items: impl IntoIterator<Item = String>,
    limit: usize,
) -> (usize, bool) {
    let budget = limit - TRAILER;
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

/// 個票を状態ファイルに残しているか (`$HOME/.rust-http-proxy.recent`。T14.9)。
///
/// `PROXY_STATS_PERSIST=off` と、ファイルが開けなかったときは `false`
/// (= 「この口の中身は再起動で消える」の意味)。
fn persisted(ep: &Endpoint<'_>) -> bool {
    // `PROXY_RECORDS=off` は個票そのものを作らないので、ファイルにも 1 バイトも書かない
    // (= `false`。T14.41)
    crate::records::recording()
        && ep
            .metrics
            .recent_persisted
            .load(std::sync::atomic::Ordering::Relaxed)
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
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"errors\":");
    let (shown, cut) = array_within(&mut out, entries.iter().map(|e| e.to_json()));
    let _ = write!(
        out,
        ",\"count\":{},\"kept\":{},\"capacity\":{},\"recorded\":{},\"persisted\":{},\"restored\":{},\"truncated\":{}}}",
        shown,
        ep.metrics.errors.len(),
        MAX_ERRORS,
        total,
        persisted(ep),
        ep.metrics.errors.restored(),
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
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"connections\":");
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

/// `/dns?sort=age|host|misses&limit=300&offset=0` — 名前解決の表の中身 (T13.1 の効きを見る口)。
///
/// **`?offset=` (T27.1)**: 雪像は `limit=4096` で引くが、tokyo の 811 名前は 256 KiB に 678 件しか
/// 入らなかった (T25.99)。`offset` は `sort=` で並べたあとの列を何本飛ばすかで、`next_offset` が
/// 次の頁の `offset` (`/hosts` と同じ読み方)。表は [`crate::dns`] の上限 4,096 件まで。
pub fn dns(query: Option<&str>) -> (u16, &'static str, String) {
    let sort = crate::dns::DnsSort::from_param(&str_param(query, "sort"));
    let limit = num_param(query, "limit", 300, 4096);
    let offset = offset_param(query, 4096);
    let rows = crate::dns::table(sort);
    let count = rows.len();
    let mut out = String::with_capacity(8192);
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"entries\":");
    let (shown, cut) = array_within(
        &mut out,
        rows.iter().skip(offset).take(limit).map(|r| r.to_json()),
    );
    // 新しい欄は**末尾**に足す (既存の鍵の順は変えない)
    let _ = write!(
        out,
        ",\"count\":{},\"shown\":{},\"sort\":\"{}\",\"ttl_secs\":{},\"negative_ttl_secs\":{},\"truncated\":{},\"offset\":{},\"next_offset\":{}}}",
        count,
        shown,
        sort.name(),
        crate::dns::ttl().as_secs(),
        crate::dns::negative_ttl().as_secs(),
        cut,
        offset,
        next_offset(offset, shown, count)
    );
    (200, "application/json", out)
}

/// `/log?n=200` — warn 以上の直近 N 行 (新しい順、既定 200 行・最大 1,000)。
///
/// `info` のアクセスログは写していない (熱い経路を重くしないため。T10.10)。
/// 動作環境 (Pterodactyl) のコンソールは流れて消えるので、これがその代わり。
pub fn log(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    let n = num_param(query, "n", 200, crate::log::MAX_LOG_LINES);
    let (lines, total) = crate::log::recent(n);
    let mut out = String::with_capacity(8192);
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"lines\":");
    let (shown, cut) = array_within(&mut out, lines.iter().map(log_line_json));
    let _ = write!(
        out,
        ",\"count\":{},\"kept\":{},\"capacity\":{},\"recorded\":{},\"level\":\"{}\",\"persisted\":{},\"restored\":{},\"truncated\":{}}}",
        shown,
        crate::log::recent_len(),
        crate::log::MAX_LOG_LINES,
        total,
        crate::log::current_level()
            .as_str()
            .trim()
            .to_ascii_lowercase(),
        persisted(ep),
        crate::log::restored_count(),
        cut
    );
    (200, "application/json", out)
}

/// `/daily?n=365` — 1 日 1 行の要約 (**古い順**。既定 1 年、最大
/// [`crate::daily::MAX_DAYS`] 日。T14.20)。
///
/// `/history` は 30 日で消えるが、この口が読むファイル
/// (`$HOME/.rust-http-proxy.daily.jsonl`) は**永久に残る** (上限 2 MiB = 11 年ぶん)。
/// 行はファイルにある JSON をそのまま並べるだけ (プロキシは組み立て直さない)。
/// `PROXY_STATS_PERSIST=off` では 1 行も書いていないので `days` は空で `path` が `null`。
pub fn daily(query: Option<&str>) -> (u16, &'static str, String) {
    let n = num_param(
        query,
        "n",
        crate::daily::DEFAULT_DAYS,
        crate::daily::MAX_DAYS,
    );
    let d = crate::daily::recent(n);
    let mut out = String::with_capacity(8192);
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"days\":");
    let (shown, cut) = array_within(&mut out, d.lines);
    let _ = write!(
        out,
        ",\"count\":{},\"shown\":{},\"bytes\":{},\"max_bytes\":{},\"max_line\":{},\"path\":{},\"truncated\":{}}}",
        d.count,
        shown,
        d.bytes,
        crate::daily::MAX_BYTES,
        crate::daily::MAX_LINE,
        crate::json::quote_opt(d.path.as_ref().map(|p| p.display().to_string()).as_deref()),
        cut || d.truncated
    );
    (200, "application/json", out)
}

/// `/slo?days=7` — SLO の達成率 (T14.50)。
///
/// 閾は `PROXY_SLO` の 4 つ (`connect_p50_ms` / `connect_p95_ms` / `error_rate` /
/// `dns_miss_per_connect`) で、**5 秒の標本 1 本ごと**に 4 つとも満たしたかを
/// 履歴スレッドが判定している (`crate::slo`)。ここはその時間ごとの集計を
/// 日ごと・時間ごとに畳んで、**外れた時間帯 (連続する外れは 1 行)** と一緒に返すだけ。
/// 応答は [`crate::slo::MAX_BODY`] (64 KiB) 以下。判定していない標本
/// (確立が 1 本も無い 5 秒) は分母に入らないので `ratio` が `null` になることがある。
pub fn slo(query: Option<&str>) -> (u16, &'static str, String) {
    let days = num_param(
        query,
        "days",
        crate::slo::DEFAULT_DAYS as usize,
        crate::slo::MAX_DAYS as usize,
    ) as u64;
    let body = crate::slo::report(crate::cache::now_epoch(), days).to_json();
    (200, "application/json", body)
}

/// `/log` の 1 要素。`conn` は `[conn#N]` の N (`[main]` なら `null`)。
fn log_line_json(line: &crate::log::Line) -> String {
    format!(
        "{{\"at\":{},\"level\":\"{}\",\"conn\":{},\"msg\":\"{}\"}}",
        line.at,
        line.level.as_str().trim().to_ascii_lowercase(),
        match line.conn {
            Some(id) => id.to_string(),
            None => "null".to_string(),
        },
        crate::json::escape(&line.msg)
    )
}

/// `/hosts?sort=requests|errors|dns|slow&limit=200&offset=0` — `.rrd` にある**全ホスト**を
/// `/status` の `hosts[]` と同じ形で (T13.4)。
///
/// `/status` の上位 50 は変えない (監視が 5 秒ごとに引く口を太らせない)。
/// 上位 50 に入らない残り 950 ホストを見るのがこちらの仕事で、
/// `scripts/status-diff.py` がそのまま読めるように窓の目印も同じ名前で出す。
///
/// **`?offset=` (T15.0 (11))**: 1,000 件 × 408 B = 398 KiB は [`MAX_BODY`] に入らず
/// 639 件で切れていた (2 枚の雪像の差分から 8 ホストが黙って落ちた)。`offset` は
/// **`sort=` で並べたあとの列**を何本飛ばすかで、`next_offset` が次の頁の `offset`。
/// `limit` の最大 1,000 と「200 を越えたら重い口」はそのまま。
pub fn hosts(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    let sort = crate::metrics::HostSort::from_param(&str_param(query, "sort"));
    let limit = num_param(query, "limit", 200, crate::metrics::MAX_HOSTS);
    let offset = offset_param(query, crate::metrics::MAX_HOSTS);
    let all = ep.metrics.hosts_sorted_by(sort);
    let count = all.len();
    // `hosts[]` は `.rrd` の通算なので、いつからの通算かも一緒に出す (`/status` と同じ)
    let restored_since = all
        .iter()
        .map(|(_, s)| s.last_seen)
        .filter(|&t| t > 0)
        .min()
        .unwrap_or(0);
    let mut out = String::with_capacity(16384);
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"hosts\":");
    let (shown, cut) = array_within(
        &mut out,
        all.iter().skip(offset).take(limit).map(|(h, s)| {
            format!(
                "{{\"host\":\"{}\",{}}}",
                crate::json::escape(h),
                crate::metrics::stats_json(s, true)
            )
        }),
    );
    // 新しい欄は**末尾**に足す (既存の鍵の順は変えない)
    let _ = write!(
        out,
        ",\"count\":{},\"shown\":{},\"sort\":\"{}\",\"limit\":{},\"truncated\":{},\"uptime_secs\":{},\"total_requests\":{},\"restored_since\":{},\"offset\":{},\"next_offset\":{}}}",
        count,
        shown,
        sort_name(sort),
        limit,
        cut,
        ep.metrics.start_time.elapsed().as_secs(),
        ep.metrics
            .total_requests
            .load(std::sync::atomic::Ordering::Relaxed),
        restored_since,
        offset,
        next_offset(offset, shown, count)
    );
    (200, "application/json", out)
}

/// `/hosts/series?top=16&host=<name>` — ホスト別の時系列 (T14.22)。
///
/// `/hosts` は**通算**、`/history` は**全体**しか無いので、その間を埋める口。
/// 直近 1 時間の要求数で選んだ**上位 16 ホスト**について、5 分の窓ごとの
/// 件数・確立 (forward は初バイト) の合計 ms・最大 ms・名前解決 ms・エラー数を
/// 24 時間ぶん (288 標本) 返す。`?host=` を渡すとそのホストだけ。
///
/// 標本は**古い順**で、`i` 番目の時刻は `t0 + i * window_secs` (`t` を 288 × 16 個
/// 並べると応答が 50 KB 太るので置いていない)。上位に居ないホストは系列を持たない
/// (空の一覧になる) ので、`tracked` で「いま何本あるか」が分かる。
pub fn host_series(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    let host = str_param(query, "host");
    let top = num_param(
        query,
        "top",
        crate::hostseries::SLOTS,
        crate::hostseries::SLOTS,
    );
    let want = (!host.is_empty()).then_some(host.as_str());
    let view = ep.metrics.host_series(want, top);
    let count = view.series.len();
    let mut out = String::with_capacity(64 * 1024);
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"series\":");
    let (shown, cut) = array_within(&mut out, view.series.iter().map(series_json));
    let _ = write!(
        out,
        ",\"keys\":[{}],\"window_secs\":{},\"samples\":{},\"slots\":{},\"t0\":{},\"tracked\":{},\"rotations\":{},\"count\":{},\"shown\":{},\"host\":\"{}\",\"top\":{},\"truncated\":{}}}",
        crate::hostseries::FIELD_NAMES
            .iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(","),
        view.window_secs,
        crate::hostseries::SAMPLES,
        crate::hostseries::SLOTS,
        view.t0,
        view.tracked,
        view.rotations,
        count,
        shown,
        crate::json::escape(&host),
        top,
        cut
    );
    (200, "application/json", out)
}

/// `/hosts/series` の 1 ホストぶん (標本は古い順の配列の配列。`/history` と同じ作法)。
fn series_json(s: &crate::hostseries::Series) -> String {
    let t = s.totals();
    let mut out = String::with_capacity(8192);
    let _ = write!(
        out,
        "{{\"host\":\"{}\",\"hour_requests\":{},\"total\":[{},{},{},{},{}],\"samples\":[",
        crate::json::escape(&s.host),
        s.hour_requests,
        t[0],
        t[1],
        t[2],
        t[3],
        t[4]
    );
    for (i, r) in s.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "[{},{},{},{},{}]", r[0], r[1], r[2], r[3], r[4]);
    }
    out.push_str("]}");
    out
}

/// `/clients?sort=requests|recent|targets|literal&limit=200` — **全接続元**の個票 (T14.7)。
///
/// `/status` の `clients[]` は上位 50 で欄も 4 つだけ。こちらは全部を、`User-Agent` ・
/// 宛先の種類・使ったポート・IP リテラル宛て・443 / 80 以外のポートまで付けて出す。
/// 認証なしの公開プロキシなので、**見知らぬ接続元が「誰のどのプログラムで、何をしているか」**
/// を読むのがこの口の仕事 (T14.0 の判断 5)。
///
/// `"persisted":false` は「この行の新しい欄は状態ファイルに残らない (再起動で消える)」の意味。
/// `.rrd` のスロットに余白が無いので**版を上げずにメモリだけで持つ**と決めた ([`crate::metrics::ClientStats`])。
pub fn clients(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    let sort = crate::metrics::ClientSort::from_param(&str_param(query, "sort"));
    let limit = num_param(query, "limit", 200, crate::metrics::MAX_CLIENTS);
    let all = ep.metrics.clients_sorted_by(sort);
    let count = all.len();
    // `clients[]` は `.rrd` の通算なので、いつからの通算かも一緒に出す (`/hosts` と同じ)
    let restored_since = all
        .iter()
        .map(|(_, s)| s.stats.last_seen)
        .filter(|&t| t > 0)
        .min()
        .unwrap_or(0);
    let mut out = String::with_capacity(16384);
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"clients\":");
    let (shown, cut) = array_within(&mut out, all.iter().take(limit).map(|(c, s)| s.to_json(c)));
    let _ = write!(
        out,
        ",\"count\":{},\"shown\":{},\"sort\":\"{}\",\"limit\":{},\"truncated\":{},\"persisted\":false,\"max_targets\":{},\"uptime_secs\":{},\"total_requests\":{},\"restored_since\":{}}}",
        count,
        shown,
        sort.name(),
        limit,
        cut,
        crate::metrics::MAX_CLIENT_TARGETS,
        ep.metrics.start_time.elapsed().as_secs(),
        ep.metrics
            .total_requests
            .load(std::sync::atomic::Ordering::Relaxed),
        restored_since
    );
    (200, "application/json", out)
}

/// `/recent` の並べ替えの鍵。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RecentSort {
    /// 閉じた新しい順 (既定)
    Time,
    /// 確立にかかった ms の大きい順 (「遅かった 1 本」を探す)
    Slow,
    /// 運んだバイトの多い順
    Bytes,
}

impl RecentSort {
    fn from_param(v: &str) -> RecentSort {
        match v {
            "slow" => RecentSort::Slow,
            "bytes" => RecentSort::Bytes,
            _ => RecentSort::Time,
        }
    }

    fn name(self) -> &'static str {
        match self {
            RecentSort::Time => "time",
            RecentSort::Slow => "slow",
            RecentSort::Bytes => "bytes",
        }
    }
}

/// `/recent?n=200&offset=0&since=<epoch>&client=<ip>&sort=time|slow|bytes` — **閉じた接続**の個票
/// (既定 200 件・最大 [`MAX_RECENT`]。T14.4)。
///
/// `/connections` は「いま」しか見えず、`/errors` は失敗だけ。ここは閉じた接続 1 本ごとの
/// 記録なので、**バーストのとき誰が何を開いたか**も**遅かった 1 本がどの段階で遅かったか**も
/// 後から読める。書くのは接続の終了で 1 回だけ (`ConnSlot` の抹消と同じ場所)。
///
/// **`?offset=` (T15.0 (11))**: 2,000 件は 1 件 424 B で [`MAX_BODY`] の 3.2 倍あり、
/// 1 枚では 615 件しか返らない。`offset` は**いまの並び** (`sort=` のあと。既定は
/// **閉じた新しい順**) を何本飛ばすかで、`next_offset` が次の頁の `offset`
/// (続きが無ければ `null`)。**`id` は accept 順・`at` は開いた時刻**でどちらも並びとは
/// 一致しない (長生きのトンネルは若い `id` で最後に閉じる) ので、頁を跨ぐ目印には使えない。
/// 取っている間に新しく閉じた接続があると並びがずれて**同じ行が 2 つの頁に出る**ことは
/// あるが (**欠けることは無い**)、行に `id` があるので読む側で落とせる。
pub fn recent(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    recent_within(ep, query, MAX_BODY)
}

/// [`recent`] の本体。`limit` は応答の上限 (ふだんは [`MAX_BODY`]、停止の雪像だけ
/// [`MAX_SNAPSHOT`]。T.28.20)。
fn recent_within(
    ep: &Endpoint<'_>,
    query: Option<&str>,
    limit: usize,
) -> (u16, &'static str, String) {
    let n = num_param(query, "n", 200, MAX_RECENT);
    let offset = offset_param(query, MAX_RECENT);
    let since = parse_query(query.unwrap_or(""))
        .iter()
        .find(|(k, _)| k == "since")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .unwrap_or(0);
    let client = str_param(query, "client");
    let sort = RecentSort::from_param(&str_param(query, "sort"));
    // 絞りはリングの鍵の内側で済ませる (2,000 件を複製してから捨てない)
    let (mut rows, total) = ep.metrics.closed.select(since, &client);
    let matched = rows.len();
    match sort {
        // `select` が既に新しい順で返している
        RecentSort::Time => {}
        // `sort_by_key` は安定なので、同点は `select` が返した新しい順のまま
        RecentSort::Slow => rows.sort_by_key(|e| std::cmp::Reverse(e.connect_ms())),
        RecentSort::Bytes => rows.sort_by_key(|e| std::cmp::Reverse(e.bytes())),
    }
    let mut out = String::with_capacity(8192);
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"recent\":");
    let (shown, cut) = array_within_budget(
        &mut out,
        rows.iter().skip(offset).take(n).map(RecentEntry::to_json),
        limit,
    );
    // 新しい欄は**末尾**に足す (既存の鍵の順は変えない)
    let _ = write!(
        out,
        ",\"count\":{},\"matched\":{},\"shown\":{},\"kept\":{},\"capacity\":{},\"recorded\":{},\"sort\":\"{}\",\"since\":{},\"client\":\"{}\",\"persisted\":{},\"restored\":{},\"truncated\":{},\"lite\":{},\"offset\":{},\"next_offset\":{}}}",
        shown,
        matched,
        shown,
        ep.metrics.closed.len(),
        MAX_RECENT,
        total,
        sort.name(),
        since,
        crate::json::escape(&client),
        persisted(ep),
        ep.metrics.closed.restored(),
        cut,
        !ep.metrics.conns.enabled(),
        offset,
        next_offset(offset, shown, matched)
    );
    (200, "application/json", out)
}

/// `/trace?n=200&since=<epoch>` — 追跡中の接続元 1 つの要求の並び (新しい順、既定 200 行・
/// 最大 [`MAX_TRACE`]。T14.27)。
///
/// `PROXY_TRACE_CLIENT=<ip>` を設定している間だけ、**その接続元の**要求行 (メソッド +
/// URL の先頭 [`MAX_PATH`] バイト + HTTP の版)・応答の状態・段階の ms・運んだバイトと、
/// CONNECT の宛先と閉じた理由が 1 行ずつ並ぶ。全体のログ水準を `trace` に上げると
/// アクセスログが全員に乗る (T10.10 の 7.2 us/要求) のに対し、ここは 1 人ぶんだけ。
///
/// **他の個票と違い URL のパスが入る**ので (T14.4〜T14.8 の共通の決まりの唯一の例外)、
/// `/snapshot` には**入れない** (パスが雪像のファイルに残らないように)。リングは
/// **メモリだけ**なので再起動で消える (T14.9 の永続化の対象ではない)。
/// `--lite` は枠 (`ConnSlot`) を作らないので旗も立たず、1 行も残らない。
pub fn trace(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    let n = num_param(query, "n", 200, MAX_TRACE);
    // `?since=` は `/recent` `/events` と同じ扱い (「その時刻以降に書いたもの」)
    let since = parse_query(query.unwrap_or(""))
        .iter()
        .find(|(k, _)| k == "since")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .unwrap_or(0);
    let (rows, total) = crate::trace::select(since, n);
    let mut out = String::with_capacity(8192);
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"trace\":");
    let (shown, cut) = array_within(&mut out, rows.iter().map(crate::trace::TraceLine::to_json));
    let _ = write!(
        out,
        ",\"count\":{},\"kept\":{},\"capacity\":{},\"recorded\":{},\"since\":{},\"max_path\":{},\"persisted\":false,\"truncated\":{},\"lite\":{}}}",
        shown,
        crate::trace::len(),
        MAX_TRACE,
        total,
        since,
        MAX_PATH,
        cut,
        !ep.metrics.conns.enabled()
    );
    (200, "application/json", out)
}

/// `/bursts?n=50` — 同時接続数が上限の一定割合を越えた瞬間の**写真** (新しい順。T14.6)。
///
/// T13.2 (上限に当たったら暇なトンネルを 1 本閉じる) が本当に効くかは「バーストが来たとき」
/// にしか見えないが、来たときに `/connections` を見ている人はいない。そこで、
/// `active_connections` が `PROXY_MAX_CONNS × PROXY_BURST_PERCENT` を**下から上に越えた
/// 瞬間**に 1 枚だけ自動で撮る (同じ山では 1 枚。閾の 80% を下回ると次の 1 枚に備える)。
///
/// 撮るのは history スレッドなので、**accept の経路には比較 1 回しか増えていない**。
/// 裏を返すと **`PROXY_STATS_PERSIST=off` (履歴スレッドを起こさない) では撮らない**。
/// `--lite` では `/connections` の表そのものが空なので写真も撮らない (`"lite":true`)。
pub fn bursts(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    let n = num_param(query, "n", MAX_BURSTS, MAX_BURSTS);
    let (shots, total) = ep.metrics.bursts.recent(n);
    let mut out = String::with_capacity(8192);
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"bursts\":");
    let (shown, cut) = array_within(&mut out, shots.iter().map(BurstShot::to_json));
    let _ = write!(
        out,
        ",\"count\":{},\"shown\":{},\"kept\":{},\"capacity\":{},\"recorded\":{},\"threshold\":{},\"max_conns\":{},\"active\":{},\"armed\":{},\"pending\":{},\"persisted\":{},\"restored\":{},\"truncated\":{},\"lite\":{}}}",
        shown,
        shown,
        ep.metrics.bursts.len(),
        MAX_BURSTS,
        total,
        ep.metrics.bursts.threshold(),
        ep.metrics.bursts.max_conns(),
        ep.metrics
            .active_connections
            .load(std::sync::atomic::Ordering::Relaxed),
        ep.metrics.bursts.armed(),
        ep.metrics.bursts.pending(),
        persisted(ep),
        ep.metrics.bursts.restored(),
        cut,
        !ep.metrics.conns.enabled()
    );
    (200, "application/json", out)
}

/// `/events?n=200&since=<epoch>` — 起きたことの時系列 (新しい順、既定 200 件・最大
/// [`MAX_EVENTS`]。T14.11)。
///
/// 起動・設定の再読込・ブロックリストの更新・IPv4 優先の切替・メモリの圧迫・バラストの
/// 増減・状態ファイルの異常・上限での追い出し・accept の失敗・停止シグナルを **1 本の
/// 時系列**にしたもの。`/log` は warn 以上なので info の出来事が入らず、`/status` の
/// `settings` は最後の 1 回しか残さない。種類は `kinds` に並ぶ 12 種で固定
/// (11 種目は異常の自動検知 `anomaly` (T14.23)、12 種目は初めて見た接続元
/// `new_client` (T14.54 の規則 6))。
pub fn events(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    let n = num_param(query, "n", 200, MAX_EVENTS);
    // `?since=` は `/recent` と同じ扱い (「その時刻以降に起きたもの」。無ければ 0 = 全部)
    let since = parse_query(query.unwrap_or(""))
        .iter()
        .find(|(k, _)| k == "since")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .unwrap_or(0);
    let (events, total) = crate::events::select(since, n);
    let mut out = String::with_capacity(8192);
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"events\":");
    let (shown, cut) = array_within(&mut out, events.iter().map(crate::events::Event::to_json));
    let _ = write!(
        out,
        ",\"count\":{},\"kept\":{},\"capacity\":{},\"recorded\":{},\"since\":{},\"kinds\":{},\"persisted\":{},\"restored\":{},\"truncated\":{}}}",
        shown,
        crate::events::len(),
        MAX_EVENTS,
        total,
        since,
        crate::json::list(crate::events::KINDS.iter().map(|k| k.name())),
        persisted(ep),
        crate::events::restored_count(),
        cut
    );
    (200, "application/json", out)
}

/// `?sort=` に出す名前 (`/status?sort=` と同じ綴り)。
fn sort_name(sort: crate::metrics::HostSort) -> &'static str {
    match sort {
        crate::metrics::HostSort::Requests => "requests",
        crate::metrics::HostSort::Errors => "errors",
        crate::metrics::HostSort::Dns => "dns",
        crate::metrics::HostSort::Slow => "slow",
    }
}
