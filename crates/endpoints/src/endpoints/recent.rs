//! 個票を読み出すエンドポイント: `/errors` `/connections` `/dns` `/log` `/hosts` (T13.4)、
//! `/recent` と `/snapshot` (閉じた接続と、1 回で全部。T14.4)、接続元の個票 `/clients` (T14.7)、
//! 山の写真 `/bursts` (T14.6)。
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
use crate::events::MAX_EVENTS;
use crate::recent::{BurstShot, MAX_BURSTS, MAX_ERRORS, MAX_RECENT, RecentEntry};

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

/// `/log?n=200` — warn 以上の直近 N 行 (新しい順、既定 200 行・最大 1,000)。
///
/// `info` のアクセスログは写していない (熱い経路を重くしないため。T10.10)。
/// 動作環境 (Pterodactyl) のコンソールは流れて消えるので、これがその代わり。
pub fn log(query: Option<&str>) -> (u16, &'static str, String) {
    let n = num_param(query, "n", 200, crate::log::MAX_LOG_LINES);
    let (lines, total) = crate::log::recent(n);
    let mut out = String::with_capacity(8192);
    out.push_str("{\"lines\":");
    let (shown, cut) = array_within(&mut out, lines.iter().map(log_line_json));
    let _ = write!(
        out,
        ",\"count\":{},\"kept\":{},\"capacity\":{},\"recorded\":{},\"level\":\"{}\",\"truncated\":{}}}",
        shown,
        crate::log::recent_len(),
        crate::log::MAX_LOG_LINES,
        total,
        crate::log::current_level()
            .as_str()
            .trim()
            .to_ascii_lowercase(),
        cut
    );
    (200, "application/json", out)
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

/// `/hosts?sort=requests|errors|dns|slow&limit=200` — `.rrd` にある**全ホスト**を
/// `/status` の `hosts[]` と同じ形で (T13.4)。
///
/// `/status` の上位 50 は変えない (監視が 5 秒ごとに引く口を太らせない)。
/// 上位 50 に入らない残り 950 ホストを見るのがこちらの仕事で、
/// `scripts/status-diff.py` がそのまま読めるように窓の目印も同じ名前で出す。
pub fn hosts(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    let sort = crate::metrics::HostSort::from_param(&str_param(query, "sort"));
    let limit = num_param(query, "limit", 200, crate::metrics::MAX_HOSTS);
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
    out.push_str("{\"hosts\":");
    let (shown, cut) = array_within(
        &mut out,
        all.iter().take(limit).map(|(h, s)| {
            format!(
                "{{\"host\":\"{}\",{}}}",
                crate::json::escape(h),
                crate::metrics::stats_json(s, true)
            )
        }),
    );
    let _ = write!(
        out,
        ",\"count\":{},\"shown\":{},\"sort\":\"{}\",\"limit\":{},\"truncated\":{},\"uptime_secs\":{},\"total_requests\":{},\"restored_since\":{}}}",
        count,
        shown,
        sort_name(sort),
        limit,
        cut,
        ep.metrics.start_time.elapsed().as_secs(),
        ep.metrics
            .total_requests
            .load(std::sync::atomic::Ordering::Relaxed),
        restored_since
    );
    (200, "application/json", out)
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
    out.push_str("{\"clients\":");
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

/// `/recent?n=200&since=<epoch>&client=<ip>&sort=time|slow|bytes` — **閉じた接続**の個票
/// (既定 200 件・最大 [`MAX_RECENT`]。T14.4)。
///
/// `/connections` は「いま」しか見えず、`/errors` は失敗だけ。ここは閉じた接続 1 本ごとの
/// 記録なので、**バーストのとき誰が何を開いたか**も**遅かった 1 本がどの段階で遅かったか**も
/// 後から読める。書くのは接続の終了で 1 回だけ (`ConnSlot` の抹消と同じ場所)。
pub fn recent(ep: &Endpoint<'_>, query: Option<&str>) -> (u16, &'static str, String) {
    let n = num_param(query, "n", 200, MAX_RECENT);
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
    out.push_str("{\"recent\":");
    let (shown, cut) = array_within(&mut out, rows.iter().take(n).map(RecentEntry::to_json));
    let _ = write!(
        out,
        ",\"count\":{},\"matched\":{},\"shown\":{},\"kept\":{},\"capacity\":{},\"recorded\":{},\"sort\":\"{}\",\"since\":{},\"client\":\"{}\",\"truncated\":{},\"lite\":{}}}",
        shown,
        matched,
        shown,
        ep.metrics.closed.len(),
        MAX_RECENT,
        total,
        sort.name(),
        since,
        crate::json::escape(&client),
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
    out.push_str("{\"bursts\":");
    let (shown, cut) = array_within(&mut out, shots.iter().map(BurstShot::to_json));
    let _ = write!(
        out,
        ",\"count\":{},\"shown\":{},\"kept\":{},\"capacity\":{},\"recorded\":{},\"threshold\":{},\"max_conns\":{},\"active\":{},\"armed\":{},\"pending\":{},\"truncated\":{},\"lite\":{}}}",
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
/// `settings` は最後の 1 回しか残さない。種類は `kinds` に並ぶ 10 種で固定。
pub fn events(query: Option<&str>) -> (u16, &'static str, String) {
    let n = num_param(query, "n", 200, MAX_EVENTS);
    // `?since=` は `/recent` と同じ扱い (「その時刻以降に起きたもの」。無ければ 0 = 全部)
    let since = parse_query(query.unwrap_or(""))
        .iter()
        .find(|(k, _)| k == "since")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .unwrap_or(0);
    let (events, total) = crate::events::select(since, n);
    let mut out = String::with_capacity(8192);
    out.push_str("{\"events\":");
    let (shown, cut) = array_within(&mut out, events.iter().map(crate::events::Event::to_json));
    let _ = write!(
        out,
        ",\"count\":{},\"kept\":{},\"capacity\":{},\"recorded\":{},\"since\":{},\"kinds\":{},\"truncated\":{}}}",
        shown,
        crate::events::len(),
        MAX_EVENTS,
        total,
        since,
        crate::json::list(crate::events::KINDS.iter().map(|k| k.name())),
        cut
    );
    (200, "application/json", out)
}

/// `/snapshot` の上限 (4 MiB)。個票 1 本 1 本の上限 ([`MAX_BODY`]) とは別枠。
///
/// デプロイ先から 1 日 1 回取って保存する大きさなので、回線を占めない・エディタで
/// 開ける・`python -m json.tool` が通る、の 3 つが収まる値にしてある。
pub const MAX_SNAPSHOT: usize = 4 * 1024 * 1024;

/// `/snapshot` が越えたときに落とす順 (大きいものから)。落としたものは `"dropped"` に出る。
const DROP_ORDER: [&str; 3] = ["recent", "log", "history.5"];

/// `/snapshot` — 上の全部を **1 要求で** 1 つの JSON にして返す (T14.4)。
///
/// デプロイ先のデータ収集が 17 本の URL を手で叩く作業になっていた (T14.0) ので、
/// `scripts/collect-deployed.sh` が 1 回で取って保存できる形にする。
///
/// **組み立ては同じプロセス内の関数呼び出し**で、自分へ HTTP で繋ぎ直さない
/// (17 本ぶんの接続を増やさない。上限に当たっている最中でも取れる。測る行為が
/// `/connections` や `/recent` を変えない)。**保持もしない** (要求ごとに組む)。
///
/// 中身: `status` (`?sort=` の 3 通り)、`history` (5 / 60 / 3600 秒)、`dns`、`errors`、
/// `connections`、`recent`、`hosts`、`clients` (T14.7)、`bursts` (T14.6)、`log`。
/// **`history.*` にはカーネルと cgroup の窓 (`kernel`) も一緒に入る** (T14.12。`/history` と同じ組み立て)。
/// **T14.3 の `/profile` は、入ったらここに 1 行足す** (`parts` に名前が出るので、
/// 読む側は「この版に何が入っていたか」を JSON だけで判別できる)。
pub fn snapshot(ep: &Endpoint<'_>) -> (u16, &'static str, String) {
    use crate::history::History;
    use crate::metrics::HostSort;

    // 個票はどれも 256 KiB 以下、`/status` は 64 KiB 以下、履歴は 3 本で最大 2 MiB。
    // 先に全部組んでから、合計が 4 MiB を越えていたら順に落とす
    let mut part: Vec<(&'static str, String)> = vec![
        ("status", super::status_body(ep, HostSort::Requests)),
        ("status_errors", super::status_body(ep, HostSort::Errors)),
        ("status_dns", super::status_body(ep, HostSort::Dns)),
        // `/history` と同じ組み立て (カーネルと cgroup の窓も一緒に入る。T14.12)
        ("history.5", super::history_body(ep, History::index_for(5))),
        (
            "history.60",
            super::history_body(ep, History::index_for(60)),
        ),
        (
            "history.3600",
            super::history_body(ep, History::index_for(3600)),
        ),
        ("dns", dns(Some("sort=age&limit=4096")).2),
        ("errors", errors(ep, Some("n=500")).2),
        ("connections", connections(ep).2),
        ("recent", recent(ep, Some("n=2000")).2),
        ("hosts", hosts(ep, Some("limit=1000")).2),
        ("clients", clients(ep, Some("limit=1000")).2),
        ("bursts", bursts(ep, Some("n=50")).2),
        ("events", events(Some("n=512")).2),
        ("log", log(Some("n=1000")).2),
    ];
    let names: Vec<&'static str> = part.iter().map(|(k, _)| *k).collect();
    let dropped = drop_to_fit(&mut part, MAX_SNAPSHOT);
    let total: usize = overhead_of(&part) + part.iter().map(|(_, v)| v.len()).sum::<usize>();

    let mut out = String::with_capacity(total + 1024);
    let _ = write!(
        out,
        "{{\"taken_at\":{},\"version\":\"{}\",\"uptime_secs\":{},\"limit_bytes\":{},\"parts\":[",
        crate::cache::now_epoch(),
        crate::json::escape(ep.version),
        ep.metrics.start_time.elapsed().as_secs(),
        MAX_SNAPSHOT,
    );
    for (i, name) in names.iter().enumerate() {
        let _ = write!(out, "{}\"{}\"", if i == 0 { "" } else { "," }, name);
    }
    out.push_str("],\"dropped\":[");
    for (i, name) in dropped.iter().enumerate() {
        let _ = write!(out, "{}\"{}\"", if i == 0 { "" } else { "," }, name);
    }
    out.push(']');
    // `history` だけは `{"5":…,"60":…,"3600":…}` に入れ子にする (`/history?res=` と同じ鍵)
    let mut history_open = false;
    for (name, body) in &part {
        match name.strip_prefix("history.") {
            Some(res) => {
                out.push_str(if history_open { "," } else { ",\"history\":{" });
                history_open = true;
                let _ = write!(out, "\"{}\":{}", res, body);
            }
            None => {
                if history_open {
                    out.push('}');
                    history_open = false;
                }
                let _ = write!(out, ",\"{}\":{}", name, body);
            }
        }
    }
    if history_open {
        out.push('}');
    }
    out.push('}');
    (200, "application/json", out)
}

/// 頭 (`taken_at` など) と鍵の飾りぶんの余白。
fn overhead_of(part: &[(&'static str, String)]) -> usize {
    512 + part.iter().map(|(k, _)| k.len() + 8).sum::<usize>()
}

/// 合計が `limit` を越えていたら [`DROP_ORDER`] の順に `null` へ置き換え、落とした名前を返す。
///
/// 落とす順は「大きい割に後から取り直せるもの」から: `recent` (2,000 件) →
/// `log` (1,000 行) → `history.5` (5 秒標本 720 本。60 秒と 3600 秒があれば形は見える)。
fn drop_to_fit(part: &mut [(&'static str, String)], limit: usize) -> Vec<&'static str> {
    let mut total: usize = overhead_of(part) + part.iter().map(|(_, v)| v.len()).sum::<usize>();
    let mut dropped = Vec::new();
    for name in DROP_ORDER {
        if total <= limit {
            break;
        }
        if let Some((_, body)) = part.iter_mut().find(|(k, _)| *k == name) {
            total = total - body.len() + 4;
            *body = "null".to_string();
            dropped.push(name);
        }
    }
    dropped
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

    /// `/recent` は 2,000 件のリングを丸ごと読んでも 256 KiB に収まること (T14.4)。
    ///
    /// 1 件はありふれた値で 225 B なので **2,000 件 (440 KiB) は入りきらない**のが
    /// 設計どおり。入らない分はバイト数で打ち切って `"truncated":true` を出す
    /// (既定の 200 件は最悪の値でも入る)。ここは `/recent` そのものを通して見る。
    #[test]
    fn the_recent_response_stays_under_256_kib() {
        use crate::recent::{CloseReason, ConnTally, MAX_RECENT, SIDES, STAGES};

        let m = Metrics::new();
        let now = Instant::now();
        let long_host = format!("{}.example.net:65535", "sub.".repeat(30));
        for i in 0..(MAX_RECENT as u64) {
            let slot = m
                .conns
                .register(i, "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6", now)
                .expect("登録できる");
            slot.begin_tunnel(&long_host);
            slot.finish(
                CloseReason::Error(crate::metrics::ErrCause::Unreachable),
                ConnTally {
                    up: u64::MAX,
                    down: u64::MAX,
                    status: 599,
                    stage_ms: [u64::MAX; STAGES],
                    rtt_us: [u32::MAX; SIDES],
                    retrans: [u32::MAX; SIDES],
                },
                u32::MAX,
            );
            m.record_closed(i);
        }
        assert_eq!(m.closed.len(), MAX_RECENT);
        assert!(m.conns.is_empty(), "抹消は接続の終了で 1 回ずつ");

        let cache = crate::cache::Cache::new(crate::cache::CacheConfig::disabled());
        let concurrency = || crate::metrics::Concurrency {
            max_conns: 0,
            max_threads: 0,
            live_threads: 0,
            idle_threads: 0,
            queued_jobs: 0,
        };
        let ep = Endpoint {
            metrics: &m,
            cache: &cache,
            conn_id: 1,
            port: 8080,
            host: None,
            pac_direct: &[],
            lite: false,
            readonly: false,
            version: "test",
            concurrency: &concurrency,
        };
        // **エンドポイントそのものを通す** (既定の 200 件は最悪の値でも入り、2,000 件は切れる)
        for (q, want_cut) in [("n=200", false), ("n=2000", true)] {
            let body = recent(&ep, Some(q)).2;
            assert!(body.len() <= MAX_BODY, "{} で {} B", q, body.len());
            assert!(
                body.contains(&format!("\"truncated\":{}", want_cut)),
                "{} の truncated が {} でない: …{}",
                q,
                want_cut,
                &body[body.len() - 220..]
            );
            assert!(body.contains("\"matched\":2000"), "{}", q);
            let shown = body.matches("\"id\":").count();
            assert_eq!(shown == 200, !want_cut, "{} で {} 件", q, shown);
            println!(
                "recent {} (最悪の値) の応答: {} B / 出せたのは {} 件 (上限 {} B)",
                q,
                body.len(),
                shown,
                MAX_BODY
            );
        }
    }

    /// `/events` の形と、`?n=` `?since=` の絞り (T14.11)。
    ///
    /// リングは静的に 1 本なので、ここでは 1 つのテストにまとめて順に見る。
    #[test]
    fn the_events_endpoint_filters_by_n_and_since() {
        use crate::events::{EventKind, MAX_EVENTS};
        crate::events::clear();
        crate::events::push(EventKind::Start, "version 0.0.0 on port 8080");
        crate::events::push(EventKind::Reload, "PROXY_TIMEOUT_SECS 30 \u{2192} 10");
        let body = events(None).2;
        assert!(body.starts_with("{\"events\":["), "{}", body);
        assert!(body.contains("\"kind\":\"reload\""), "{}", body);
        assert!(
            body.contains("PROXY_TIMEOUT_SECS 30 \u{2192} 10"),
            "前後の値が出る: {}",
            body
        );
        assert!(body.contains("\"count\":2"), "{}", body);
        assert!(body.contains("\"recorded\":2"), "{}", body);
        assert!(body.contains("\"capacity\":512"), "{}", body);
        assert!(body.contains("\"truncated\":false"), "{}", body);
        // 10 種の名前が全部出る (README の一覧と合っているか)
        for kind in [
            "start",
            "reload",
            "blocklist",
            "ipv6",
            "pressure",
            "ballast",
            "state_file",
            "evict",
            "emfile",
            "shutdown",
        ] {
            assert!(body.contains(&format!("\"{}\"", kind)), "{} が無い", kind);
        }
        // 新しい順
        let first = body.find("reload").unwrap();
        let second = body.find("\"kind\":\"start\"").unwrap();
        assert!(first < second, "新しい順でない: {}", body);
        // `?n=1` で 1 件
        let one = events(Some("n=1")).2;
        assert!(one.contains("\"count\":1"), "{}", one);
        assert!(!one.contains("\"kind\":\"start\""), "{}", one);
        // `?since=` は「その時刻以降」。先の時刻なら 0 件
        let none = events(Some("since=9999999999")).2;
        assert!(none.starts_with("{\"events\":[]"), "{}", none);
        assert!(none.contains("\"since\":9999999999"), "{}", none);
        assert!(none.contains("\"recorded\":2"), "通算は残る: {}", none);
        // 満杯 (512 件) の最悪でも 256 KiB に収まる
        crate::events::clear();
        for i in 0..MAX_EVENTS + 10 {
            crate::events::push(
                EventKind::Blocklist,
                &format!("{}{}", "\u{2192}".repeat(50), i),
            );
        }
        let body = events(Some("n=512")).2;
        assert!(body.contains("\"count\":512"), "{}", body);
        assert!(body.contains("\"truncated\":false"), "{}", body);
        assert!(body.len() <= MAX_BODY, "{} B", body.len());
        println!(
            "events 512 件の応答: {} B (上限 {} B)",
            body.len(),
            MAX_BODY
        );
        crate::events::clear();
    }

    /// 4 MiB を越えたら `/recent` → `/log` → `/history?res=5` の順に落とすこと (T14.4)。
    #[test]
    fn the_snapshot_drops_the_biggest_parts_in_order() {
        let build = || {
            vec![
                ("status", "s".repeat(100)),
                ("history.5", "a".repeat(400)),
                ("history.60", "b".repeat(400)),
                ("recent", "c".repeat(500)),
                ("log", "d".repeat(300)),
            ]
        };
        // 収まっていれば 1 つも落とさない
        let mut part = build();
        assert!(drop_to_fit(&mut part, MAX_SNAPSHOT).is_empty());
        assert_eq!(part[3].1.len(), 500);

        // いちばん大きい `recent` から順に落ちる
        let total = overhead_of(&part) + 1700;
        let mut part = build();
        assert_eq!(drop_to_fit(&mut part, total - 1), ["recent"]);
        assert_eq!(part[3], ("recent", "null".to_string()));
        assert_eq!(part[4].1.len(), 300, "log はまだ残る");

        let mut part = build();
        assert_eq!(drop_to_fit(&mut part, total - 700), ["recent", "log"]);
        let mut part = build();
        assert_eq!(
            drop_to_fit(&mut part, 1),
            ["recent", "log", "history.5"],
            "3 つ落としてもこれ以上は落とさない"
        );
        assert_eq!(part[0].1.len(), 100, "`/status` は落とさない");
        assert_eq!(part[2].1.len(), 400, "60 秒の履歴は落とさない");
    }

    /// `?sort=` の並びと `?since=` / `?client=` の絞り (T14.4)。
    #[test]
    fn recent_sorts_and_filters() {
        let m = Metrics::new();
        let now = Instant::now();
        for (i, (client, connect_ms, bytes)) in [
            ("10.0.0.1", 5u64, 100u64),
            ("10.0.0.2", 90, 10),
            ("10.0.0.1", 1, 5000),
        ]
        .into_iter()
        .enumerate()
        {
            let slot = m.conns.register(i as u64, client, now).expect("登録できる");
            slot.begin_tunnel("example.net:443");
            slot.finish(
                crate::recent::CloseReason::ClientEof,
                crate::recent::ConnTally {
                    up: 0,
                    down: bytes,
                    status: 0,
                    stage_ms: [0, connect_ms, 0, 0, 0, 0],
                    ..Default::default()
                },
                0,
            );
            m.record_closed(i as u64);
        }
        let concurrency = || crate::metrics::Concurrency {
            max_conns: 0,
            max_threads: 0,
            live_threads: 0,
            idle_threads: 0,
            queued_jobs: 0,
        };
        let cache = crate::cache::Cache::new(crate::cache::CacheConfig::disabled());
        let ep = Endpoint {
            metrics: &m,
            cache: &cache,
            conn_id: 1,
            port: 8080,
            host: None,
            pac_direct: &[],
            lite: false,
            readonly: false,
            version: "test",
            concurrency: &concurrency,
        };
        // 既定は閉じた新しい順
        let body = recent(&ep, None).2;
        assert!(body.contains("\"count\":3"), "{}", body);
        assert!(body.contains("\"sort\":\"time\""), "{}", body);
        let ids = |b: &str| {
            b.match_indices("\"id\":")
                .map(|(i, _)| b[i + 5..].split(',').next().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&body), ["2", "1", "0"]);
        // 確立の遅い順 / 転送の多い順
        assert_eq!(ids(&recent(&ep, Some("sort=slow")).2), ["1", "0", "2"]);
        assert_eq!(ids(&recent(&ep, Some("sort=bytes")).2), ["2", "0", "1"]);
        // 接続元で絞る
        let mine = recent(&ep, Some("client=10.0.0.1")).2;
        assert_eq!(ids(&mine), ["2", "0"]);
        assert!(mine.contains("\"client\":\"10.0.0.1\""), "{}", mine);
        // 未来の時刻で絞れば 0 件 (`recorded` は減らない)
        let none = recent(&ep, Some("since=9999999999")).2;
        assert!(none.contains("\"recent\":[]"), "{}", none);
        assert!(none.contains("\"recorded\":3"), "{}", none);
        // `?n=` で件数を絞る
        assert_eq!(ids(&recent(&ep, Some("n=1")).2).len(), 1);
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
            warm: true,
            next_refresh_secs: Some(33),
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
            warm: true,
            next_refresh_secs: Some(u64::MAX),
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

    /// `/log` はどんな中身でも 256 KiB に収まること。
    ///
    /// 1 行 256 B × 1,000 行 = 250 KiB に JSON の飾りが乗るので、**上限いっぱいの
    /// 1,000 行は入りきらない**のが設計どおり (既定の 200 行は最悪でも入る)。
    /// 入らない分はバイト数で打ち切って `"truncated":true` を出す。
    #[test]
    fn the_log_response_stays_under_256_kib() {
        let line = crate::log::Line {
            at: u64::MAX,
            level: crate::log::Level::Warn,
            conn: Some(usize::MAX),
            // 1 行は 256 B で切ってあるが、`\"` に化ける文字だけの最悪も見る
            msg: "\"".repeat(crate::log::MAX_LOG_LINE),
        };
        let lines = vec![line; crate::log::MAX_LOG_LINES];
        for (n, want_cut) in [(200usize, false), (crate::log::MAX_LOG_LINES, true)] {
            let mut body = String::from("{\"lines\":");
            let (shown, cut) = array_within(&mut body, lines.iter().take(n).map(log_line_json));
            body.push('}');
            assert_eq!(cut, want_cut, "{} 行", n);
            assert!(body.len() <= MAX_BODY, "{} 行で {} B", n, body.len());
            println!(
                "log {} 行 (最悪の行) の応答: {} B / 出せたのは {} 行 (上限 {} B)",
                n,
                body.len(),
                shown,
                MAX_BODY
            );
        }
    }

    /// `/hosts` は 1,000 ホスト (`MAX_HOSTS` = `.rrd` に入る全部) でも 256 KiB 以下。
    ///
    /// デプロイ先並みの値なら 1,000 件が丸ごと入る。桁を振り切った値 (転送 20 桁、
    /// 応答 9,999 ms、原因が 8 種類とも埋まる) を 1,000 件並べると入りきらないので、
    /// そこは**バイト数で打ち切る** — その打ち切りが効くことも一緒に見る。
    #[test]
    fn the_hosts_response_stays_under_256_kib() {
        use crate::metrics::{Detail, ErrCause, HostOutcome, HostSort, MAX_HOSTS};
        use std::time::Duration;

        let row = |m: &Metrics, host: &str, bytes: u64, ms: u64, detail: Detail| {
            m.record_host_detail(
                host,
                HostOutcome::Bypass,
                bytes,
                Some(Duration::from_millis(ms)),
                detail,
            );
        };

        // (1) デプロイ先並み (鍵 29 文字、転送 100 MB、確立 12 ms)
        let plain = Metrics::new();
        for i in 0..MAX_HOSTS {
            row(
                &plain,
                &format!("connect://host{:04}.example.net:443", i),
                98_765_432,
                12,
                Detail {
                    dns_ms: 3,
                    dns_misses: 1,
                    connect_ms: 9,
                    family_v6: Some(false),
                    cause: None,
                    first_byte_ms: None,
                },
            );
        }
        // (2) 桁を振り切った最悪
        let worst = Metrics::new();
        for i in 0..MAX_HOSTS {
            row(
                &worst,
                &format!("connect://host{:04}.cdn.example.net:443", i),
                u64::MAX / 2,
                9999,
                Detail {
                    dns_ms: 123,
                    dns_misses: 45,
                    connect_ms: 678,
                    family_v6: Some(true),
                    cause: Some(ErrCause::Dns),
                    first_byte_ms: None,
                },
            );
        }

        let build = |m: &Metrics, take: usize| {
            let all = m.hosts_sorted_by(HostSort::Requests);
            assert_eq!(all.len(), MAX_HOSTS);
            let mut body = String::from("{\"hosts\":");
            let (shown, cut) = array_within(
                &mut body,
                all.iter().take(take).map(|(h, s)| {
                    format!(
                        "{{\"host\":\"{}\",{}}}",
                        crate::json::escape(h),
                        crate::metrics::stats_json(s, true)
                    )
                }),
            );
            body.push('}');
            (body.len(), shown, cut)
        };

        // 既定の 200 件は、桁を振り切った値でも丸ごと入る
        let (len, shown, cut) = build(&worst, 200);
        assert_eq!(shown, 200);
        assert!(!cut);
        assert!(len <= MAX_BODY, "{} B", len);
        println!("hosts 200 件 (最悪の値): {} B (上限 {} B)", len, MAX_BODY);

        // 1,000 件は 1 件 325 B (ありふれた値) なので 256 KiB には入りきらない。
        // **バイト数で打ち切って上限を守る**のが設計どおり (`"truncated":true` で分かる)
        for (label, m) in [("ありふれた値", &plain), ("最悪の値", &worst)] {
            let (len, shown, cut) = build(m, MAX_HOSTS);
            assert!(len <= MAX_BODY, "{} で {} B", label, len);
            assert!(cut, "{}: バイト数で打ち切られること", label);
            assert!(shown >= 500, "{}: {} 件しか出ていない", label, shown);
            println!(
                "hosts 1,000 件 ({}): {} B / 出せたのは {} 件 (上限 {} B)",
                label, len, shown, MAX_BODY
            );
        }
    }

    /// `/clients` は 1,000 接続元 (`MAX_CLIENTS` = 表に入る全部) でも 256 KiB 以下。
    ///
    /// 1 行の最悪は `User-Agent` 4 種 × 128 B が効いて 1 KB 近くになるので、
    /// **既定の 200 件は丸ごと入り、1,000 件はバイト数で打ち切る**のが設計どおり。
    #[test]
    fn the_clients_response_stays_under_256_kib() {
        use crate::metrics::{ClientSort, HostOutcome, MAX_CLIENTS};
        use std::time::Duration;

        let m = Metrics::new();
        for i in 0..MAX_CLIENTS {
            let ip = format!("2001:db8:1234:5678:9abc:def0:1234:{:04x}", i);
            for a in 0..4 {
                m.record_client_agent(&ip, &format!("{}/{}", "x".repeat(200), a));
            }
            for p in 0..8u16 {
                m.record_client(
                    &ip,
                    HostOutcome::Bypass,
                    // 8 本ぶんの合計が桁を振り切る手前まで (1 行の JSON を最悪にする)
                    u64::MAX / 16,
                    Some(Duration::from_millis(1234)),
                    Some(&format!(
                        "very-long-host-name-{}.example.net:{}",
                        p,
                        1000 + p
                    )),
                );
            }
        }
        let all = m.clients_sorted_by(ClientSort::Requests);
        assert_eq!(all.len(), MAX_CLIENTS);
        for (n, want_cut) in [(200usize, false), (MAX_CLIENTS, true)] {
            let mut body = String::from("{\"clients\":");
            let (shown, cut) =
                array_within(&mut body, all.iter().take(n).map(|(c, s)| s.to_json(c)));
            body.push('}');
            assert!(body.len() <= MAX_BODY, "{} 件で {} B", n, body.len());
            assert_eq!(cut, want_cut, "{} 件", n);
            println!(
                "clients {} 件 (最悪の行): {} B / 出せたのは {} 件 (上限 {} B)",
                n,
                body.len(),
                shown,
                MAX_BODY
            );
        }
    }

    /// `/bursts` は 50 枚の最悪 (接続元も宛先も上限いっぱい) でも 256 KiB に収まること (T14.6)。
    #[test]
    fn the_bursts_response_stays_under_256_kib() {
        use crate::recent::{BurstShot, MAX_BURSTS, MAX_SHOT_CLIENTS, MAX_SHOT_TARGETS};

        let m = Metrics::new();
        let now = Instant::now();
        let long_host = format!("{}.example.net:65535", "sub.".repeat(30));
        for i in 0..(MAX_SHOT_CLIENTS + MAX_SHOT_TARGETS + 8) as u64 {
            let slot = m
                .conns
                .register(
                    i,
                    &format!("2001:0db8:0000:0000:0000:ff00:0042:{:04x}%enp0s31f6", i),
                    now,
                )
                .expect("登録できる");
            slot.begin_tunnel(&format!("{}{}", i, long_host));
        }
        let rows = m.conns.snapshot();
        for i in 0..(MAX_BURSTS as u64 + 5) {
            m.bursts.push(BurstShot::take(
                &rows,
                i + 1,
                usize::MAX,
                usize::MAX,
                4096,
                2048,
                u64::MAX,
                u64::MAX,
            ));
        }
        assert_eq!(m.bursts.len(), MAX_BURSTS);

        let cache = crate::cache::Cache::new(crate::cache::CacheConfig::disabled());
        let concurrency = || crate::metrics::Concurrency {
            max_conns: 0,
            max_threads: 0,
            live_threads: 0,
            idle_threads: 0,
            queued_jobs: 0,
        };
        let ep = Endpoint {
            metrics: &m,
            cache: &cache,
            conn_id: 1,
            port: 8080,
            host: None,
            pac_direct: &[],
            lite: false,
            readonly: false,
            version: "test",
            concurrency: &concurrency,
        };
        let body = bursts(&ep, None).2;
        assert!(body.len() <= MAX_BODY, "{} B", body.len());
        assert!(
            body.contains("\"count\":50"),
            "50 枚が全部入ること: {}",
            &body[body.len() - 300..]
        );
        assert!(
            body.contains("\"truncated\":false"),
            "{}",
            &body[body.len() - 300..]
        );
        assert!(
            body.contains("\"recorded\":55"),
            "{}",
            &body[body.len() - 300..]
        );
        println!(
            "bursts 50 枚 (最悪の 1 枚): {} B / 1 枚 {} B (上限 {} B)",
            body.len(),
            body.len() / MAX_BURSTS,
            MAX_BODY
        );
        // `?n=` で絞れ、1 枚も無ければ空の一覧
        assert_eq!(bursts(&ep, Some("n=3")).2.matches("\"seq\":").count(), 3);
        let empty = Metrics::new();
        let ep2 = Endpoint {
            metrics: &empty,
            ..ep
        };
        let none = bursts(&ep2, None).2;
        assert!(none.contains("\"bursts\":[]"), "{}", none);
        assert!(none.contains("\"recorded\":0"), "{}", none);
        assert!(none.contains("\"armed\":true"), "{}", none);
        assert!(none.contains("\"pending\":false"), "{}", none);
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
