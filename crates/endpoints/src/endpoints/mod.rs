//! プロキシ自身のエンドポイント: `/dashboard` (コントロールパネル)、`/status`
//! (`?sort=requests|errors|dns|slow` で `hosts[]` の上位 50 の切り出しを変えられる。T13.3)、
//! `/healthz` (**本当の健康診断**。軽い JSON で、**`fatal` な**検査が 1 つでも偽なら 503。
//! 本文の `ok` は `fatal` でない検査も込み。T14.12 / T15.0 (6))、
//! `/history` (JSON、`res=5|60|3600&n=`。カーネルと cgroup の窓が `kernel` に付く。
//! `res=5` は 6 時間ぶん持っていて `n=` で 4,320 本まで遡れる。T14.32。
//! `?since=&until=&summary=1` は**期間を畳んだ 1 行だけ**を返す。T14.24)、
//! `/metrics` (Prometheus)、`/proxy.pac` (ブラウザの自動設定)、
//! `/purge` と `PURGE` メソッド、`/lookup`、`/blocklist` (判定と手動の上書き)。
//!
//! 自分宛てかどうかは**ポートだけ**で決める: 絶対形式 (`http://host:PORT/status`) は authority の、
//! オリジン形式 (`GET /status` + `Host:`) は `Host` のポート (無ければ 80) が自分の待ち受けポートと
//! 同じときだけ自分宛て。自分宛てで知らないパスは 404、`/` は 200 でこの一覧を返す
//! (自分へ転送してループしない。T12.3)。ポートの違うオリジン形式は今までどおり転送する。
//! 応答は常に `Connection: close`。認証は無いので、到達できる人は誰でも purge できる
//! (公開ポートなら ACL や到達制御で守ること)。`PROXY_ENDPOINTS_READONLY=on` にすると
//! **書き換える口だけ** (`/purge` / `PURGE` / `/blocklist?action=`) を 405 で断る (T14.18)。
//! **重い口** (`/snapshot` `/profile` `/explain` と大きく引いた `/hosts` `/recent` `/history`) は
//! **同時に 1 本だけ**組み、2 本目からは `503` + `Retry-After: 1` で断る (T14.51)。

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::cache::{Cache, cache_key};
use crate::http::parse_origin;
use crate::log_info;
use crate::metrics::{self, Metrics};
use crate::persist;
use crate::prom;
use crate::reload;

pub struct Endpoint<'a> {
    pub metrics: &'a Metrics,
    pub cache: &'a Cache,
    pub conn_id: usize,
    /// 自分の待ち受けポート (絶対形式の自分宛て判定に使う)
    pub port: u16,
    /// 要求の `Host` ヘッダー (`/proxy.pac` が自分の名前を知るため)
    pub host: Option<&'a str>,
    /// 接続元 IP (`readers` に 1 行残すため。T14.53)。**要求で来たときだけ**入り、
    /// 日次の `/snapshot` を履歴スレッドが組むときは `None` (誰も引いていないので
    /// 数えない)
    pub client: Option<&'a str>,
    /// `/proxy.pac` で DIRECT にするホストのパターン
    pub pac_direct: &'a [String],
    /// lite プロファイル (ダッシュボードを持たない)
    pub lite: bool,
    /// 書き換える口 (`/purge` / `PURGE` / `/blocklist?action=`) を 405 で断る
    /// (`PROXY_ENDPOINTS_READONLY`。読む口は今までどおり。認証ではない。T14.18)
    pub readonly: bool,
    /// 動いているバイナリの版 (`/status` に出す。本体クレートの `VERSION`)
    pub version: &'a str,
    /// 上限といまのスレッド数を引く口 (`/status` と `/metrics` を組み立てるときだけ呼ぶ)。
    ///
    /// 値そのものではなく関数で受け取るのは、生きているスレッド数と待ち行列を数えるのに
    /// **全接続スレッドで共有している鍵**を取るため。`Endpoint` は要求ごとに組むので、
    /// ここで数えると熱い経路に乗ってしまう (この 2 つのパスに来たときだけ引く)
    pub concurrency: &'a dyn Fn() -> metrics::Concurrency,
}

mod blocklist;
mod config;
mod explain;
mod health;
mod pac;
mod profile;
mod recent;

const DASHBOARD_HTML: &str = include_str!("../web/dashboard.html");

/// 「調査」ページ (T14.8)。`/dashboard` が「いま」を見る画面なのに対して、
/// **起きたことを時間軸で読む**ための別のページ (個票を描く)。
/// `--lite` でも 200 で返す (記録が無ければページの中で「記録していません」と出る)。
const INSPECT_HTML: &str = include_str!("../web/inspect.html");

/// 「端末から測る」ページ (T14.33)。プロキシ側の計測は「プロキシに届いてから」しか
/// 見えないので、**利用者のブラウザから** `/status` の往復と、プロキシ経由で小さな URL を
/// 取る時間を測り、`/clients` の自分の行 (T14.7) と `rtt_ms` (T14.5) に並べる。
/// 測った値はサーバーへ送らない (端末の中だけ)。`--lite` でも 200 で返す
/// (接続元を記録していないことはページの中で伝える)。
const PROBE_HTML: &str = include_str!("../web/probe.html");

/// 要求ターゲットを自分宛てのパスに直す。**どちらの形式もポートだけで判定する**:
/// 絶対形式は authority の、オリジン形式は `Host` ヘッダーのポート (無ければ 80) が
/// 自分の待ち受けポートと同じときだけ自分宛て。それ以外 (他所への転送) は `None`。
///
/// オリジン形式を無条件に自分宛てにしていた頃は、知らないパスが `Host` 宛ての転送に落ちて
/// **`Host` が自分自身ならループした** (1 要求で `max_conns` 本。T12.3 の前提 4)。
/// `Host` が無い HTTP/1.0 のオリジン形式は自分宛てにしない (今までどおり 400)。
fn local_path<'a>(target: &'a str, port: u16, host: Option<&str>) -> Option<&'a str> {
    if target.starts_with('/') {
        return (authority_port(host?)? == port).then_some(target);
    }
    let rest = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("HTTP://"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    (authority_port(authority)? == port).then_some(path)
}

/// `host[:port]` のポート (無ければ 80)。`[::1]` のようなブラケット付きも扱う。
fn authority_port(authority: &str) -> Option<u16> {
    if authority.is_empty() {
        return None;
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => p.parse::<u16>().ok(),
        _ => Some(80),
    }
}

/// 自分宛ての `/` に返す案内 (エンドポイントの一覧)。`--lite` では `/dashboard` を載せない。
fn endpoint_list(lite: bool) -> String {
    let dashboard = if lite {
        ""
    } else {
        "  /dashboard                                  control panel (graphs, per-host stats)\n"
    };
    format!(
        "rust-http-proxy - an HTTP/HTTPS(CONNECT) forward proxy.\n\n\
         This is the proxy itself, not a web site. Point your browser or client at\n\
         this address as an HTTP proxy (or use /proxy.pac below).\n\n\
         endpoints:\n\
         {}\
         \x20 /inspect                                    control panel: what happened (timeline)\n\
         \x20 /probe.html                                 measure this proxy from your browser\n\
         \x20 /status[?sort=errors|dns|slow]              JSON: counters, hosts, cache, threads\n\
         \x20 /errors?n=100                               JSON: the last errors (who, when, why)\n\
         \x20 /connections                                JSON: the connections open right now\n\
         \x20 /recent?n=200&since=&client=&sort=&offset=  JSON: the connections that closed\n\
         \x20 /bursts?n=50                                JSON: snapshots taken at each spike\n\
         \x20 /trace?n=200&since=                         JSON: one client's requests (PROXY_TRACE_CLIENT)\n\
         \x20 /events?n=200&since=                        JSON: starts, reloads, and other events\n\
         \x20 /snapshot                                   JSON: everything above in one request\n\
         \x20 /snapshots                                  JSON: the daily snapshots kept on disk\n\
         \x20 /snapshots/<YYYY-MM-DD>                     JSON: one saved day (as taken)\n\
         \x20 /dns?sort=age|host|misses                   JSON: the resolver cache table\n\
         \x20 /log?n=200                                  JSON: the last warnings and errors\n\
         \x20 /hosts?sort=&limit=200&offset=              JSON: every host (/status keeps 50)\n\
         \x20 /hosts/series?top=16&host=<name>            JSON: per-host series (5 min x 24 h)\n\
         \x20 /clients?sort=&limit=200                    JSON: every client (agent, targets, ports)\n\
         \x20 /readers                                    JSON: who reads these endpoints (scan or monitor)\n\
         \x20 /explain?host=<name> | ?client=<ip>         JSON: one peer, explained in one page\n\
         \x20 /config                                     JSON: effective settings and where they came from\n\
         \x20 /healthz                                    health checks (503 when unhealthy)\n\
         \x20 /history?res=5|60|3600&n=720                JSON: time series (res=5 keeps 6 h)\n\
         \x20 /history?since=&until=&summary=1            JSON: one summary row for a period\n\
         \x20 /profile?res=5|60&n=&offset=&summary=1      JSON: stages, threads, locks\n\
         \x20 /daily?n=365                                JSON: one summary line per day (kept forever)\n\
         \x20 /slo?days=7                                 JSON: how much of the time the SLO was met\n\
         \x20 /metrics                                    Prometheus text format\n\
         \x20 /proxy.pac                                  browser auto-config script\n\
         \x20 /lookup?url=<url>                           cache entry state\n\
         \x20 /purge?url=<url> | /purge?all=1             drop cache entries (also: PURGE <url>)\n\
         \x20 /blocklist?host=&action=block|allow|clear   blocklist decision and overrides\n",
        dashboard
    )
}

/// 日次の `/snapshot` (T14.34) を組むのに要るもの。[`register_snapshot`] に渡す。
pub struct SnapshotSource {
    pub metrics: std::sync::Arc<Metrics>,
    pub cache: std::sync::Arc<Cache>,
    /// 待ち受けポート (`Endpoint` の自分宛て判定と同じ値。中身には出ない)
    pub port: u16,
    pub version: &'static str,
    pub lite: bool,
    pub readonly: bool,
    /// 上限といまのスレッド数を引く口 (`/status` を組むときだけ呼ぶ。要求ごとの
    /// `Endpoint` と同じもので、こちらは履歴スレッドが 1 日 1 回呼ぶ)
    pub concurrency: Box<dyn Fn() -> metrics::Concurrency + Send + Sync>,
}

/// **日付の変わり目に `/snapshot` を組む閉包を履歴スレッドへ預ける** (起動時に 1 回。T14.34)。
///
/// 組み立ては要求で来たときと**同じ関数** ([`recent::snapshot`]) で、自分へ HTTP で
/// 繋ぎ直さない (T14.4 と同じ)。預ける形にしてあるのは、書く側の `proxy-metrics` が
/// この層より**下**にあるため (下から上を呼ぶと依存が輪になる。T14.11 の `events::poll`
/// と同じ判断で、**向きは上から預ける**)。
pub fn register_snapshot(src: SnapshotSource) {
    crate::snapshots::set_builder(Box::new(move || {
        let ep = Endpoint {
            metrics: &src.metrics,
            cache: &src.cache,
            // 要求で来たわけではないのでログの接続 id は無い (履歴スレッドが組む)
            conn_id: 0,
            port: src.port,
            host: None,
            // 要求で来たわけではないので読み手は数えない (T14.53)
            client: None,
            pac_direct: &[],
            lite: src.lite,
            readonly: src.readonly,
            version: src.version,
            concurrency: &*src.concurrency,
        };
        recent::snapshot(&ep).2
    }));
}

/// **重い口を組んでいる最中か** (T14.51)。旗はプロセスに 1 つだけ。
///
/// 認証なしの公開ポートでは `/snapshot` (4 MiB を組む。T14.4) を誰でも好きなだけ叩けるので、
/// 1 秒に 10 回引かれると CPU と鍵の時間をそれだけで食う。**同時に組むのは 1 本**にして、
/// 2 本目からは断る。走査に対する最小限の保護で、**認証ではない** (§0 は守る。
/// 順に引けば誰でも全部取れる)。
static HEAVY_BUSY: AtomicBool = AtomicBool::new(false);

/// 旗を取れているあいだ生きる番人 (T14.51)。
///
/// **落ちるときに必ず旗を戻す**ので、組んでいる途中で `?` で抜けてもパニックしても
/// 旗が立ったままにならない (`AtomicBool` を手で戻すと、戻し忘れた経路が 1 本でもあると
/// 以後ずっと 503 になる)。
pub struct HeavyGuard {
    _private: (),
}

impl Drop for HeavyGuard {
    fn drop(&mut self) {
        HEAVY_BUSY.store(false, Ordering::Release);
    }
}

/// 重い口を組み始める。**空いていれば番人**、既に 1 本走っていれば `None` (T14.51)。
///
/// 待たない (待つと、断るより高くつく「4 MiB を組む行列」ができる)。
///
/// **`.is_ok().then_some(HeavyGuard { .. })` と書いてはいけない**: `then_some` は引数を
/// 先に組むので、**取れなかったときにも番人が 1 つ出来てすぐ落ち**、その `Drop` が
/// **他人の握っている旗を戻して**しまう (同時 4 本のうち 2 本が通る、で最初に踏んだ)。
pub fn begin_heavy() -> Option<HeavyGuard> {
    if HEAVY_BUSY
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        Some(HeavyGuard { _private: () })
    } else {
        None
    }
}

/// **重い口の一覧はここ 1 か所** (T14.51)。
///
/// 重いのは「大きい応答を組む」口で、`/status` `/healthz` `/metrics` のような軽い口は
/// 入れない (監視が 5 秒ごとに叩く口を断ると、断ったこと自体が事故になる)。
/// `/hosts` `/recent` `/history` は**大きく引いたときだけ**重い (既定のままなら軽い)。
/// 問い合わせを読むのはこの 3 つのパスに来たときだけなので、他の口は文字列の照合 1 回で抜ける。
fn is_heavy(is_get: bool, path: &str, query: Option<&str>) -> bool {
    if !is_get {
        return false;
    }
    match path {
        // 17 部を 1 つの JSON に組む (T14.4)。無条件に重い
        "/snapshot" => true,
        // 待ちの段階・スレッドの CPU と状態・ロックの取り合い (T14.3)。
        // **`?summary=1` だけは軽い** (標本を 1 本も組まず 3 段に畳んだ数字だけ返す。T15.0 (11))
        "/profile" => !has_flag(query, "summary"),
        // 1 相手を上の口から横断して読む (T14.36)
        "/explain" => true,
        // 既定の `limit` / `n` を越えて引いたときだけ (T14.32 の `/history?res=5&n=4320` は 1.9 MB)
        "/hosts" => num_over(query, "limit", 200),
        "/recent" => num_over(query, "n", 500),
        "/history" => num_over(query, "n", 720),
        _ => false,
    }
}

/// 問い合わせに `key=` が**立っている**か (`/history?summary=1` と同じ読み方。`0` は偽)。
pub(super) fn has_flag(query: Option<&str>, key: &str) -> bool {
    let Some(q) = query else {
        return false;
    };
    parse_query(q).iter().any(|(k, v)| k == key && v != "0")
}

/// `?offset=N` を読む (無い / 読めない値は 0。上は `max` で止める。T15.0 (11))。
///
/// 読むのは `/recent` `/hosts` `/profile` の 3 つで、**読み方はここ 1 か所**に置く。
/// `recent.rs` の `num_param` と別なのは**下限が 0** だから (あちらは `.clamp(1, max)` なので
/// 「1 件目から」を表せない)。意味は「いまの並びを何本飛ばすか」で、並びは `/recent` が
/// 閉じた新しい順、`/hosts` が `sort=` の順、`/profile` が新しい標本の順。
pub(super) fn offset_param(query: Option<&str>, max: usize) -> usize {
    parse_query(query.unwrap_or(""))
        .iter()
        .find(|(k, _)| k == "offset")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(max)
}

/// 次の頁の `offset` (続きが無ければ `null`)。上の 3 つが同じ綴りで応答の末尾に出す。
pub(super) fn next_offset(offset: usize, shown: usize, total: usize) -> String {
    match offset + shown < total {
        true => (offset + shown).to_string(),
        false => "null".to_string(),
    }
}

/// 問い合わせの `key=` が `limit` を越えているか (無い・読めない値は「越えていない」)。
fn num_over(query: Option<&str>, key: &str, limit: u64) -> bool {
    let Some(q) = query else {
        return false;
    };
    parse_query(q)
        .iter()
        .any(|(k, v)| k == key && v.parse::<u64>().is_ok_and(|n| n > limit))
}

/// 内部エンドポイントなら応答して `Ok(true)` を返す。そうでなければ何もせず `Ok(false)`。
pub fn handle(
    client: &mut impl Write,
    method: &str,
    target: &str,
    ep: &Endpoint<'_>,
) -> io::Result<bool> {
    let is_purge = method.eq_ignore_ascii_case("PURGE");
    let local = if is_purge {
        Some(target)
    } else {
        local_path(target, ep.port, ep.host)
    };
    let Some(local) = local else {
        return Ok(false);
    };
    // ここへ来た要求はどちらの形式でも自分宛て (`local_path` がポートで判定済み)。
    // この旗は「絶対形式か」= `/proxy.pac` が自分の名前をどちらから取るか、だけに使う
    let absolute_form = !target.starts_with('/');
    let (path, query) = match local.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (local, None),
    };
    let is_get = method.eq_ignore_ascii_case("GET");
    // **内部エンドポイントを引いた接続元を 1 行残す** (`/status` の `readers` と
    // `/readers`。T14.53)。ここまで来た要求は自分宛てと決まっているので、
    // プロキシとして通す要求 (CONNECT / forward) はこの行を 1 度も通らない
    // (T14.7 の `clients[]` はその逆で、自分宛てだけの接続を数えない)。
    // 渡すのは**問い合わせ文字列を外したパス**で、数えるのは 1 要求につき 1 回だけ。
    // 応答を組む前に数えるので、**読んだ応答にはその要求自身が入っている**
    // (`/status` を 3 回引いた 3 回目の応答が `count: 3`)
    if let Some(client) = ep.client {
        ep.metrics.record_reader(client, path);
    }
    // **重い口は同時に 1 本だけ**組む (T14.51)。取れなければ下で `503` + `Retry-After: 1`。
    // 番人は下の `if` 連鎖より長く生き、**組み終わったところ**で旗を戻す (途中で `?` で
    // 抜けてもパニックしても戻る)。軽い口はこの `if` にも原子にも触らない。
    // **読み手の記録 (T14.53) の直後**に置くので、断った要求も読み手としては数える
    // (走査が来ていることが `/readers` から読める)
    let mut heavy_busy = false;
    let heavy_guard = if is_heavy(is_get, path, query) {
        let guard = begin_heavy();
        if guard.is_none() {
            heavy_busy = true;
            ep.metrics.heavy_rejected.fetch_add(1, Ordering::Relaxed);
        }
        guard
    } else {
        None
    };
    // `PROXY_ENDPOINTS_READONLY=on` なら**書き換える口だけ**断る (T14.18)。読む口は
    // 今までどおりなので、これは認証ではなく「消せる口を閉じる」つまみでしかない
    let (status, content_type, body) = if heavy_busy {
        // 既に 1 本走っている (T14.51)。**組まずに**断るので、これ自体は安い
        (
            503,
            "application/json",
            format!("{}\"error\":\"busy\"}}", metrics::SCHEMA_HEAD),
        )
    } else if ep.readonly && is_write(is_purge, path, query) {
        (
            405,
            "application/json",
            format!(
                "{}\"error\":\"read-only (PROXY_ENDPOINTS_READONLY=on)\"}}",
                metrics::SCHEMA_HEAD
            ),
        )
    } else if is_purge {
        purge_url(ep, target)
    } else if is_get && (path == "/inspect" || path == "/inspect/" || path == "/dashboard/inspect")
    {
        // 「調査」ページ (T14.8)。`--lite` でも 200 — 個票が空でもページは開ける
        // (読む人が「記録していません」と分かるのはページの中)
        (200, "text/html; charset=utf-8", INSPECT_HTML.to_string())
    } else if is_get && (path == "/probe.html" || path == "/probe" || path == "/probe/") {
        // 「端末から測る」ページ (T14.33)。`--lite` でも 200 — 測れるのは (1) だけになる
        (200, "text/html; charset=utf-8", PROBE_HTML.to_string())
    } else if is_get && (path == "/dashboard" || path == "/dashboard/") {
        if ep.lite {
            (
                200,
                "text/plain; charset=utf-8",
                "lite mode: the dashboard is off (PROXY_PROFILE=lite)\n".to_string(),
            )
        } else {
            (200, "text/html; charset=utf-8", DASHBOARD_HTML.to_string())
        }
    } else if is_get && path == "/proxy.pac" {
        (
            200,
            "application/x-ns-proxy-autoconfig",
            pac::render(ep, target, absolute_form),
        )
    } else if is_get && path == "/history" {
        let params = parse_query(query.unwrap_or(""));
        let res = params
            .iter()
            .find(|(k, _)| k == "res")
            .and_then(|(_, v)| v.parse::<u64>().ok());
        if params.iter().any(|(k, v)| k == "summary" && v != "0") {
            // 期間を畳んだ 1 行だけ (標本は返さない。T14.24)
            (200, "application/json", history_summary(ep, &params, res))
        } else {
            let res = res.map_or(0, crate::history::History::index_for);
            // `n=` は**新しい方から何本返すか** (既定 720、`res=5` だけ 4,320 まで。T14.32)
            let n = params
                .iter()
                .find(|(k, _)| k == "n")
                .and_then(|(_, v)| v.parse::<usize>().ok());
            (200, "application/json", history_body(ep, res, n))
        }
    } else if is_get && path == "/profile" {
        // 待ちの段階・スレッドの CPU と状態・ロックの取り合い (T14.3)
        profile::profile(ep, query)
    } else if is_get && path == "/errors" {
        // 個票 (T13.4)。集計 (`/status`) では読めない「誰が・いつ・なぜ」を出す
        recent::errors(ep, query)
    } else if is_get && path == "/connections" {
        recent::connections(ep)
    } else if is_get && path == "/recent" {
        // 閉じた接続の個票 (T14.4)。`/connections` の「いま」に対して「起きたこと」
        recent::recent(ep, query)
    } else if is_get && path == "/bursts" {
        // 山の写真 (T14.6)。同時接続数が上限の一定割合を越えた瞬間の `/connections`
        recent::bursts(ep, query)
    } else if is_get && path == "/trace" {
        // 接続元 1 つの追跡 (T14.27)。`PROXY_TRACE_CLIENT` に一致した接続元の要求行と
        // 応答の状態と段階。**他の個票と違い URL のパスが入る** (追跡中だけ)
        recent::trace(ep, query)
    } else if is_get && path == "/events" {
        // 起きたことの時系列 (T14.11)。起動・再読込・ブロックリスト・IPv6・圧迫・
        // バラスト・状態ファイル・追い出し・accept の失敗・停止シグナルを 1 本に
        recent::events(ep, query)
    } else if is_get && path == "/snapshot" {
        // 17 本の URL を 1 要求で (T14.4)。`scripts/collect-deployed.sh` が保存する
        recent::snapshot(ep)
    } else if is_get && (path == "/snapshots" || path == "/snapshots/") {
        // 日次で残した `/snapshot` の一覧 (T14.34)
        recent::snapshots()
    } else if is_get && let Some(date) = path.strip_prefix("/snapshots/") {
        // 残してある 1 日ぶんをそのまま (T14.34)。日付として読めない名前は 404
        recent::snapshot_file(date)
    } else if is_get && path == "/dns" {
        recent::dns(query)
    } else if is_get && path == "/slo" {
        // SLO の達成率 (T14.50)。5 秒の標本ごとの判定を日ごと・時間ごとに畳んで返す
        recent::slo(query)
    } else if is_get && path == "/daily" {
        // 1 日 1 行の要約 (T14.20)。`/history` (30 日) が消えたあとも残る
        recent::daily(query)
    } else if is_get && path == "/log" {
        recent::log(ep, query)
    } else if is_get && path == "/hosts/series" {
        // ホスト別の時系列 (上位 16 ホスト × 5 分 × 24 時間。T14.22)
        recent::host_series(ep, query)
    } else if is_get && path == "/hosts" {
        recent::hosts(ep, query)
    } else if is_get && path == "/clients" {
        // 接続元の個票 (T14.7)。認証なしで誰でも見えるのは他の個票と同じ
        recent::clients(ep, query)
    } else if is_get && path == "/readers" {
        // 内部エンドポイントを**引いた側**の一覧 (T14.53)。`/clients` が
        // 「プロキシとして通した相手」なのに対して、こちらは「個票を読んでいる相手」。
        // 上限のバイト数は個票の口と同じ 256 KiB (`recent::MAX_BODY`)
        (
            200,
            "application/json",
            ep.metrics.readers_body(recent::MAX_BODY),
        )
    } else if is_get && path == "/explain" {
        // 1 相手の説明 (T14.36)。上の口を横断して読む作業をサーバー側で 1 枚に組む
        explain::explain(ep, query)
    } else if is_get && path == "/config" {
        // 効いている設定とその出どころ、この環境で何が読めるか (T14.15)
        config::render(ep)
    } else if is_get && path == "/blocklist" {
        blocklist::handle(&parse_query(query.unwrap_or("")))
    } else if is_get && path == "/healthz" {
        // `/status` の写しではなく**本当の健康診断** (T14.12)。軽い JSON で、
        // **`fatal` な**検査が 1 つでも偽なら 503 (本文の `ok` は全部込み。T15.0 (6))。
        // **問い合わせは読まない** (監視が叩く口の意味を変えない)
        health::healthz(ep)
    } else if is_get && path == "/status" {
        // `?sort=requests|errors|dns|slow` は `hosts[]` の上位 50 を切り出す鍵だけを変える
        // (T13.3)。知らない値は既定に倒す
        let sort = parse_query(query.unwrap_or(""))
            .iter()
            .find(|(k, _)| k == "sort")
            .map_or(metrics::HostSort::Requests, |(_, v)| {
                metrics::HostSort::from_param(v)
            });
        (200, "application/json", status_body(ep, sort))
    } else if is_get && path == "/metrics" {
        (
            200,
            "text/plain; version=0.0.4; charset=utf-8",
            // 上限といまのスレッド数はここで 1 回だけ引く (`/status` と同じ形)
            prom::render(ep.metrics, Some(ep.cache), (ep.concurrency)()),
        )
    } else if is_get && path == "/purge" {
        let params = parse_query(query.unwrap_or(""));
        if params.iter().any(|(k, v)| k == "all" && v != "0") {
            let n = ep.cache.clear_all();
            (
                200,
                "application/json",
                format!("{}\"purged\":{},\"all\":true}}", metrics::SCHEMA_HEAD, n),
            )
        } else if let Some((_, url)) = params.iter().find(|(k, _)| k == "url") {
            purge_url(ep, url)
        } else {
            (
                400,
                "application/json",
                format!(
                    "{}\"error\":\"use /purge?url=<url> or /purge?all=1\"}}",
                    metrics::SCHEMA_HEAD
                ),
            )
        }
    } else if is_get && path == "/lookup" {
        let params = parse_query(query.unwrap_or(""));
        match params.iter().find(|(k, _)| k == "url") {
            Some((_, url)) => lookup(ep, url),
            None => (
                400,
                "application/json",
                format!(
                    "{}\"error\":\"use /lookup?url=<url>\"}}",
                    metrics::SCHEMA_HEAD
                ),
            ),
        }
    } else if is_get && (path == "/" || path.is_empty()) {
        // ブラウザでプロキシの URL を開いた人への案内 (`--lite` でも出す)
        (200, "text/plain; charset=utf-8", endpoint_list(ep.lite))
    } else {
        // 自分宛てだが知らないパス: 自分へ転送するとループするので 404
        (
            404,
            "text/plain; charset=utf-8",
            format!("not found.\n\n{}", endpoint_list(ep.lite)),
        )
    };
    // **組み終わったので旗を戻す** (T14.51)。client への書き出しまで握ると、読むのが
    // 遅い相手 1 人で `/snapshot` `/profile` が全員に 503 を返し続けることになる
    drop(heavy_guard);
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        // `/healthz` の **`fatal` な**検査が 1 つでも偽 (T14.12 / T15.0 (6))、
        // または重い口が 1 本走っている (T14.51)
        503 => "Service Unavailable",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{}Cache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        content_type,
        body.len(),
        // 重い口を断ったときだけ「1 秒後に」 (T14.51)。`/healthz` の 503 には付けない
        // (あちらは「治るまで待て」ではなく「この検査が偽」なので、目安の秒が出せない)
        if heavy_busy { "Retry-After: 1\r\n" } else { "" },
        body
    );
    client.write_all(response.as_bytes())?;
    client.flush()?;
    log_info!(
        Some(ep.conn_id),
        "{} {} -> {} (internal endpoint)",
        method,
        target,
        status
    );
    Ok(true)
}

/// `/status` の本体を組み立てる (`?sort=` は `hosts[]` の上位 50 を切り出す鍵。T13.3)。
///
/// **`/snapshot` (T14.4) もここを呼ぶ**: 1 要求で全部取るのに自分へ HTTP で繋ぎ直すと、
/// 上限に当たっているときに取れない・接続を 17 本増やす・測る行為が状態を変える。
/// 同じプロセス内の関数呼び出しで組む。
///
/// `/status` の組み立てはこの層の仕事 (指標は部品を並べるだけにしてある。
/// 下の層が上の層を呼ぶと依存が輪になるため)。
pub(super) fn status_body(ep: &Endpoint<'_>, sort: metrics::HostSort) -> String {
    ep.metrics.to_json_with_cache(
        Some(ep.cache),
        metrics::StatusExtras {
            settings: &reload::status_json(),
            blocklist: &crate::blocklist::status_json(),
            state_file: &persist::status_json(),
            version: ep.version,
            concurrency: (ep.concurrency)(),
            sort,
        },
    )
}

/// `/history` の JSON に、カーネルと cgroup の窓を**別の配列**として足す (T14.12)。
///
/// 標本の配列 (`keys` / `samples`) は 1 バイトも変えない: `.rrd` の標本の余白は 4 B しか
/// 残っていない (T14.2 (3)) ので、カーネルの窓はメモリだけの別物になっている。
/// 項目数も解像度も違うので、同じ行に混ぜずに `"kernel":{"keys":[...],"samples":[[...]]}` で並べる
/// (1 時間の解像度はこの窓に無いので `null`)。
/// `n` は**新しい方から何本返すか** (`None` = 既定。`res=5` は 6 時間ぶん持っているが
/// 既定は今までどおり 720 本。T14.32)。カーネルの窓は 720 本までしか無いので `n` で切らない。
pub(super) fn history_body(ep: &Endpoint<'_>, res: usize, n: Option<usize>) -> String {
    let base = ep.metrics.history.to_json_res_n(res, n);
    match base.strip_suffix('}') {
        Some(head) => format!("{},\"kernel\":{}}}", head, crate::kernel::history_json(res)),
        None => base,
    }
}

/// `/history?since=<epoch>|restart&until=<epoch>&summary=1` の要約 (T14.24)。
///
/// **標本は返さない** (期間を畳んだ 1 行だけ)。畳むのは
/// [`crate::history::summary`] で、`scripts/snapshot-diff.py` が手元でやっている集計と
/// 同じ求め方。`/history` の他の応答は 1 バイトも変えていない。
pub(super) fn history_summary(
    ep: &Endpoint<'_>,
    params: &[(String, String)],
    res: Option<u64>,
) -> String {
    let now = crate::cache::now_epoch();
    // `since=restart` は `/status` の `since_start_secs` と同じ起動時刻から
    let uptime = ep.metrics.start_time.elapsed().as_secs();
    crate::history::summary::of(
        &ep.metrics.history,
        &summary_params(params, res, now, uptime),
    )
    // 直近 1,024 本の正確な分位点を**別の鍵で**添える (T14.31)。`p50_ms` は期間を畳んだ
    // 区間の補間、`recent_quantiles` は期間に関わらず直近 1,024 本の実測なので別物
    .to_json_with(Some(&ep.metrics.recent_quantiles_json()))
}

/// `?since=&until=&res=&normal_hours_only=` を読む (**時計を持たない**ので試験できる)。
///
/// `since=restart` は `now - uptime` (= `/status` の `since_start_secs` で切るのと同じ)。
/// 数として読めない値と書いていない `since` は 0 (残っているいちばん古い標本から)、
/// `until` は今。`normal_hours_only=1` で 1 時間 300 本以上の標本を外す (T14.0 の「平常時」)。
fn summary_params(
    params: &[(String, String)],
    res: Option<u64>,
    now: u64,
    uptime: u64,
) -> crate::history::summary::Params {
    let val = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    crate::history::summary::Params {
        since: match val("since") {
            Some("restart") => now.saturating_sub(uptime),
            Some(v) => v.parse::<u64>().unwrap_or(0),
            None => 0,
        },
        until: val("until")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(now),
        res,
        normal_hours_only: val("normal_hours_only").is_some_and(|v| v != "0"),
    }
}

/// URL を正規化して全バリアントを消す。
fn purge_url(ep: &Endpoint<'_>, url: &str) -> (u16, &'static str, String) {
    match parse_origin(url, None) {
        Ok(origin) => {
            let canonical = origin.url();
            let n = ep.cache.invalidate(&canonical, ep.conn_id);
            (
                200,
                "application/json",
                format!(
                    "{}\"purged\":{},\"url\":\"{}\"}}",
                    metrics::SCHEMA_HEAD,
                    n,
                    crate::json::escape(&canonical)
                ),
            )
        }
        Err(e) => (
            400,
            "application/json",
            format!(
                "{}\"error\":\"{}\"}}",
                metrics::SCHEMA_HEAD,
                crate::json::escape(&e.to_string())
            ),
        ),
    }
}

/// エントリの状態を返す (LRU には触らない)。バリアント無し (Accept-Encoding 無し) のキーを見る。
fn lookup(ep: &Endpoint<'_>, url: &str) -> (u16, &'static str, String) {
    let Ok(origin) = parse_origin(url, None) else {
        return (
            400,
            "application/json",
            format!("{}\"error\":\"invalid url\"}}", metrics::SCHEMA_HEAD),
        );
    };
    let canonical = origin.url();
    let key = cache_key("GET", &canonical);
    match ep.cache.peek(key) {
        Some(info) => {
            let now = crate::cache::now_epoch();
            (
                200,
                "application/json",
                format!(
                    "{}\"found\":true,\"url\":\"{}\",\"memory\":{},\"disk\":{},\"size\":{},\"stored_at\":{},\"expires_at\":{},\"fresh\":{},\"ttl_left\":{},\"validators\":{}}}",
                    metrics::SCHEMA_HEAD,
                    crate::json::escape(&canonical),
                    info.memory,
                    info.disk,
                    info.size,
                    info.meta.stored_at,
                    info.meta.expires_at,
                    info.meta.expires_at > now,
                    info.meta.expires_at.saturating_sub(now),
                    info.meta.validators
                ),
            )
        }
        None => (
            404,
            "application/json",
            format!(
                "{}\"found\":false,\"url\":\"{}\"}}",
                metrics::SCHEMA_HEAD,
                crate::json::escape(&canonical)
            ),
        ),
    }
}

/// `a=b&c=d` を (キー, パーセントデコード済みの値) に分ける。
/// 書き換える口か (`/purge` / `PURGE` / `/blocklist?action=<空でない値>`。T14.18)。
///
/// 呼ぶのは `PROXY_ENDPOINTS_READONLY=on` のときだけ (`&&` の右に置いてある) なので、
/// 既定では `/blocklist` の問い合わせを二度読むことはない。
fn is_write(is_purge: bool, path: &str, query: Option<&str>) -> bool {
    is_purge
        || path == "/purge"
        || (path == "/blocklist"
            && parse_query(query.unwrap_or(""))
                .iter()
                .any(|(k, v)| k == "action" && !v.is_empty()))
}

pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect()
}

/// `%XX` を戻す (`+` はそのまま: URL の中の `+` を壊さない)。
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (
                hex(bytes[i + 1]),
                hex(bytes.get(i + 2).copied().unwrap_or(0)),
            )
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_queries() {
        assert_eq!(percent_decode("a%20b%2Fc+d"), "a b/c+d");
        assert_eq!(percent_decode("bad%zz%4"), "bad%zz%4");
        let q = parse_query("url=http%3A%2F%2Fexample.com%2Fx%3Fy%3D1&all=1&flag");
        assert_eq!(
            q[0],
            ("url".to_string(), "http://example.com/x?y=1".to_string())
        );
        assert_eq!(q[1], ("all".to_string(), "1".to_string()));
        assert_eq!(q[2], ("flag".to_string(), String::new()));
        assert_eq!(crate::json::escape("a\"b\\c"), "a\\\"b\\\\c");
    }
}

#[cfg(test)]
mod local_path_tests {
    use super::{endpoint_list, local_path, parse_query, summary_params};

    #[test]
    fn origin_form_is_local_only_when_the_host_port_is_ours() {
        assert_eq!(
            local_path("/status", 8080, Some("127.0.0.1:8080")),
            Some("/status")
        );
        assert_eq!(
            local_path("/purge?all=1", 8080, Some("[::1]:8080")),
            Some("/purge?all=1")
        );
        assert_eq!(local_path("/", 80, Some("proxy.example.net")), Some("/"));
        // ポートが違えば今までどおり転送する (透過プロキシの使い方を壊さない)
        assert_eq!(local_path("/status", 8080, Some("example.com")), None);
        assert_eq!(local_path("/status", 8080, Some("example.com:80")), None);
        assert_eq!(local_path("/x", 8080, Some("127.0.0.1:9999")), None);
        // Host の無い HTTP/1.0 のオリジン形式は今までどおり (転送に落ちて 400)
        assert_eq!(local_path("/status", 8080, None), None);
        assert_eq!(local_path("/status", 8080, Some("")), None);
        assert_eq!(local_path("/status", 8080, Some("host:nope")), None);
    }

    #[test]
    fn absolute_form_is_local_only_on_our_port() {
        assert_eq!(
            local_path("http://tokyo.example.net:60624/status", 60624, None),
            Some("/status")
        );
        assert_eq!(local_path("http://[::1]:60624", 60624, None), Some("/"));
        assert_eq!(local_path("http://example.com/status", 60624, None), None);
        assert_eq!(
            local_path("http://example.com/status", 80, None),
            Some("/status")
        );
        assert_eq!(local_path("http://example.com:8080/x", 60624, None), None);
        assert_eq!(local_path("https://example.com:60624/x", 60624, None), None);
    }

    /// ダッシュボードに Phase 13 が見る図と KPI が載っていること (T12.4 (5)、T13.3)。
    ///
    /// ブラウザが無いので絵は確かめられない。**`/history` と `/status` の読み方が
    /// 合っているか**は `scripts/check-dashboard.js` (Node があるときだけ) が
    /// 実出力を通して見る。ここで見るのは「消えていないこと」だけ。
    #[test]
    fn the_dashboard_has_the_charts_phase_13_reads() {
        let html = super::DASHBOARD_HTML;
        for id in [
            "ch-conn",
            "ch-err",
            "ch-fd",
            "connp50",
            "version",
            "window",
            "dnsmiss",
            "connlimit",
            "bad",
            // 個票の表 (T13.4)
            "errors",
            "errhint",
            "conns",
            "connhint",
            // プロファイル (T14.3)
            "proflead",
            "st-connect-setup",
            "st-connect-after",
            "st-forward-setup",
            "st-forward-after",
            "stages",
            "ch-rolecpu",
            "ch-cpureq",
            "roles",
            "locks",
            "profhint",
            // T15.0 (14) の 5 枚: (a) CPU の絞り / (b) 動かないトンネル /
            // (c) 名前解決の内訳 / (d) 受付待ち / (e) 利用者が待つ時間
            "cputag",
            "cpukpi",
            "cpubar",
            "cpudetail",
            "ch-cpu",
            "topthreads",
            "topthreadshint",
            "idleconns",
            "idleconnhint",
            "ch-dnswarm",
            "lg-dnskind",
            "st-dnskind",
            "dnskindhint",
            "lg-queue",
            "st-queue",
            "queuehint",
            "ch-rundelay",
            "rundelayhint",
            "waitp50",
            "waitp50tag",
            "waitdetail",
            "ch-wait",
        ] {
            assert!(html.contains(&format!("id=\"{}\"", id)), "{} が無い", id);
        }
        // 配列の配列を読む側 (キー名を戻す関数) と区間の分位点、`/status` を読む側 (T13.3)
        for f in [
            "function toSamples(",
            "function winQuantile(",
            "function mergeWindows(",
            "function dnsStats(",
            "function badHosts(",
            "function peak(",
            // 個票を読む側 (T13.4)
            "function errorRows(",
            "function connRows(",
            // プロファイルを読む側 (T14.3)
            "function toProfile(",
            "function stageRows(",
            "function longestStage(",
            "function roleRows(",
            "function lockRows(",
            "function drawStack(",
            // T15.0 (14) の 5 枚が読む側 (`scripts/check-dashboard.js` が名前で切り出す)
            "function toKernel(",
            "function cpuThrottle(",
            "function topThreads(",
            "function runDelay(",
            "function queueSpread(",
            "function idleTunnels(",
            "function dnsMissKinds(",
            "function waitKpi(",
        ] {
            assert!(html.contains(f), "{} が無い", f);
        }
        // ホスト別の表の列 (名前解決 / 接続、v4 / v6) と並びの選択肢 (T13.3 で 2 つ増えた)
        assert!(html.contains("名前解決 / 接続"), "{}", "内訳の列が無い");
        assert!(html.contains("v4 / v6"), "{}", "族の列が無い");
        for opt in ["\"p95\"", "\"errors\"", "\"dns\"", "\"slow\"", "\"bytes\""] {
            assert!(
                html.contains(&format!("<option value={}>", opt)),
                "並びの選択肢 {} が無い",
                opt
            );
        }
        // 「悪いホスト」は `?sort=` の 2 本を **30 秒に 1 回だけ** 取る (負荷を増やさない)
        assert!(
            html.contains("'/status?sort='+k"),
            "{}",
            "?sort= を取っていない"
        );
        assert!(
            html.contains("setInterval(pollBad,30000)"),
            "{}",
            "30 秒ごとになっていない"
        );
        // 個票は 5 秒ごとに `/errors?n=20` と `/connections` の 2 本 (T13.4)
        assert!(
            html.contains("fetchJson('/errors?n=20')"),
            "{}",
            "/errors を取っていない"
        );
        assert!(
            html.contains("fetchJson('/connections')"),
            "{}",
            "/connections を取っていない"
        );
        assert!(
            html.contains("setInterval(pollRecent,5000)"),
            "{}",
            "個票が 5 秒ごとになっていない"
        );
        // プロファイルは 5 秒ごとに `/profile?res=` を 1 本 (T14.3)
        assert!(
            html.contains("fetchJson('/profile?res='+res)"),
            "{}",
            "/profile を取っていない"
        );
        assert!(
            html.contains("setInterval(pollProfile,5000)"),
            "{}",
            "プロファイルが 5 秒ごとになっていない"
        );
        // ヘッダーから個票へ行けること (T13.4)
        for link in [
            "/dns",
            "/log",
            "/hosts?limit=1000",
            "/errors",
            "/connections",
            "/profile",
        ] {
            assert!(
                html.contains(&format!("<a href=\"{}\" target=\"_blank\">", link)),
                "{} へのリンクが無い",
                link
            );
        }
        // 外部ライブラリは読み込まない (依存なしの 1 ページ)
        assert!(!html.contains("<script src="), "外部 JS を読み込んでいる");
        assert!(
            !html.contains("<link rel=\"stylesheet\""),
            "外部 CSS を読み込んでいる"
        );
    }

    /// 「調査」ページに T14.8 が描く図と、node の整形テストが抜き出す関数があること。
    ///
    /// 絵はブラウザが無いと確かめられないので、ここで見るのは「消えていないこと」と
    /// **大きさが 64 KiB 以下**なことだけ (読み方が実出力と合っているかは
    /// `scripts/check-dashboard.js` が `/snapshot` の実出力で見る)。
    #[test]
    fn the_inspect_page_has_what_phase_14_reads() {
        let html = super::INSPECT_HTML;
        assert!(
            html.len() <= 64 * 1024,
            "inspect.html が 64 KiB を超えた: {} B",
            html.len()
        );
        for id in [
            "ch-timeline", // (a) タイムライン
            "lg-timeline",
            "tllead",
            "slow", // (b) 遅い接続の表
            "lg-stages",
            "bursts",  // (c) 山の写真
            "clients", // (d) 接続元
            "ch-rtt",  // (e) RTT の散布
            "rtt",
            "win", // (f) 起動からの窓
            "winkv",
            "series-card", // T14.22 が入ったときだけ出る折れ線
            "lite",
        ] {
            assert!(html.contains(&format!("id=\"{}\"", id)), "{} が無い", id);
        }
        // node の整形テストが名前で抜き出す関数 (名前を変えるならあちらも直すこと)
        for f in [
            "function timeline(",
            "function eventMarks(",
            "function slowRows(",
            "function burstCards(",
            "function clientRows(",
            "function rttScatter(",
            "function sinceStart(",
            "function seriesLines(",
            "function toSamples(",
            "function winQuantile(",
        ] {
            assert!(html.contains(f), "{} が無い", f);
        }
        // (g) 先頭に `/snapshot` へのリンク。個票の口もヘッダーから開ける
        for link in [
            "/snapshot",
            "/recent?n=2000",
            "/bursts",
            "/clients?limit=1000",
        ] {
            assert!(
                html.contains(&format!("<a href=\"{}\" target=\"_blank\">", link)),
                "{} へのリンクが無い",
                link
            );
        }
        // 段階は T14.3 の 5 つ、閉じた理由は T14.4 の 8 種を色で持つ
        for stage in ["queue", "client_read", "dns", "connect", "first_relay"] {
            assert!(html.contains(stage), "段階 {} が無い", stage);
        }
        for reason in [
            "client_eof",
            "server_eof",
            "idle_timeout",
            "keepalive_timeout",
            "evicted",
            "limit",
            "shutdown",
        ] {
            assert!(html.contains(reason), "閉じた理由 {} が無い", reason);
        }
        // 外部ライブラリは読み込まない (依存なしの 1 ページ)
        assert!(!html.contains("<script src="), "外部 JS を読み込んでいる");
        assert!(
            !html.contains("<link rel=\"stylesheet\""),
            "外部 CSS を読み込んでいる"
        );
    }

    /// `/history?summary=1` の引数の読み方 (T14.24)。**`since=restart` が
    /// `/status` の `since_start_secs` と合う**ことがこのタスクの受け入れ基準の 1 つ。
    #[test]
    fn the_summary_params_read_since_restart_and_the_period() {
        let now = 1_757_000_000u64;
        let q = |s: &str| parse_query(s);
        // `since=restart` = いまから稼働秒数を引いた時刻 (= `/status` の窓の始まり)
        let p = summary_params(&q("summary=1&since=restart"), None, now, 7_200);
        assert_eq!((p.since, p.until), (now - 7_200, now));
        assert!(!p.normal_hours_only);
        // 期間 2 時間なら 60 秒の窓が自動で選ばれる (1 時間を越えるので 5 秒では足りない)
        assert_eq!(p.res_index(), 1);
        // epoch で切る / `?res=` と `normal_hours_only=1` を足す
        let p = summary_params(
            &q("since=1756000000&until=1756100000&normal_hours_only=1"),
            Some(3600),
            now,
            10,
        );
        assert_eq!((p.since, p.until), (1_756_000_000, 1_756_100_000));
        assert!(p.normal_hours_only);
        assert_eq!(p.res_index(), 2);
        // 書いていなければ「残っている全部」から「今」まで、数として読めない値も同じ
        let p = summary_params(&q("summary=1&since=yesterday"), None, now, 10);
        assert_eq!((p.since, p.until), (0, now));
        assert_eq!(p.res_index(), 2);
        // `normal_hours_only=0` は切らない
        assert!(!summary_params(&q("normal_hours_only=0"), None, now, 10).normal_hours_only);
        // 案内にも載っている
        assert!(endpoint_list(false).contains("/history?since=&until=&summary=1"));
    }

    #[test]
    fn the_listing_hides_the_dashboard_in_lite_mode() {
        assert!(endpoint_list(false).contains("/dashboard"));
        assert!(!endpoint_list(true).contains("/dashboard"));
        for path in ["/status", "/metrics", "/proxy.pac", "/purge?url="] {
            assert!(endpoint_list(true).contains(path), "{}", path);
        }
    }
}
