//! 自分宛ての要求をどの口へ渡すか (経路表)。
//!
//! 門 (重い口は同時に 1 本・読み取り専用) は `super::handle` の側で、ここへ来る要求は
//! 門を通ったもの。パスは**完全一致**で引き、前綴りで見るのは `/snapshots/<名前>` だけ。

use super::explain;
use super::incoming::Incoming;
use super::pages::{DASHBOARD_HTML, INSPECT_HTML, PROBE_HTML, endpoint_list};
use super::respond::Reply;
#[cfg(feature = "test-hooks")]
use super::test_hooks;
use super::{
    Endpoint, blocklist, config, health, history, pac, parse_query, profile, purge, recent, status,
};

/// ブラウザが勝手に取りに来るアイコンのパス (204 で返す。T28.7)。口の一覧には入れない。
const FAVICON: &str = "/favicon.ico";

/// 門を通った自分宛ての要求に答える口を選ぶ。
pub(super) fn dispatch(ep: &Endpoint<'_>, req: &Incoming<'_>, target: &str) -> Reply {
    if req.method.is_purge() {
        return purge::purge_url(ep, target);
    }
    if req.method.is_head() && req.is_write() {
        // 書き換える口 (`/purge` と `/blocklist?action=`) は HEAD では**何もしない**
        // (HEAD は安全なメソッド。GET として組むと消してしまう)
        return Reply::json_error(405, "use GET for this endpoint").with_allow("Allow: GET\r\n");
    }
    if !req.method.is_get() {
        // GET / HEAD / PURGE 以外 (POST など) は 405 + `Allow: GET, HEAD` (T28.7)。
        // 今までは 404 と案内 2.8 KB で、「無い口」と「メソッドが違う」の区別が付かなかった
        return Reply::json_error(405, "method not allowed").with_allow("Allow: GET, HEAD\r\n");
    }
    dispatch_get(ep, req, target)
}

/// GET (と HEAD) の経路表。
fn dispatch_get(ep: &Endpoint<'_>, req: &Incoming<'_>, target: &str) -> Reply {
    let (path, query) = (req.path, req.query);
    match path {
        // ダッシュボードを開くたびにブラウザが取りに来る (今までは 404 と案内 2.8 KB)。
        // 口ではないので `/` の案内にも README にも載せない
        FAVICON => (204, "", String::new()).into(),
        // 「調査」ページ (T14.8)。`--lite` でも 200 — 個票が空でもページは開ける
        // (読む人が「記録していません」と分かるのはページの中)
        "/inspect" | "/inspect/" | "/dashboard/inspect" => {
            (200, "text/html; charset=utf-8", INSPECT_HTML.to_string()).into()
        }
        // 「端末から測る」ページ (T14.33)。`--lite` でも 200 — 測れるのは (1) だけになる
        "/probe.html" | "/probe" | "/probe/" => {
            (200, "text/html; charset=utf-8", PROBE_HTML.to_string()).into()
        }
        "/dashboard" | "/dashboard/" => {
            if ep.lite {
                (
                    200,
                    "text/plain; charset=utf-8",
                    "lite mode: the dashboard is off (PROXY_PROFILE=lite)\n".to_string(),
                )
                    .into()
            } else {
                (200, "text/html; charset=utf-8", DASHBOARD_HTML.to_string()).into()
            }
        }
        "/proxy.pac" => (
            200,
            "application/x-ns-proxy-autoconfig",
            pac::render(ep, target, req.absolute_form),
        )
            .into(),
        "/history" => history::history(ep, query),
        // 待ちの段階・スレッドの CPU と状態・ロックの取り合い (T14.3)
        "/profile" => profile::profile(ep, query).into(),
        // 個票 (T13.4)。集計 (`/status`) では読めない「誰が・いつ・なぜ」を出す
        "/errors" => recent::errors(ep, query).into(),
        "/connections" => recent::connections(ep).into(),
        // 閉じた接続の個票 (T14.4)。`/connections` の「いま」に対して「起きたこと」
        "/recent" => recent::recent(ep, query).into(),
        // 山の写真 (T14.6)。同時接続数が上限の一定割合を越えた瞬間の `/connections`
        "/bursts" => recent::bursts(ep, query).into(),
        // 接続元 1 つの追跡 (T14.27)。`PROXY_TRACE_CLIENT` に一致した接続元の要求行と
        // 応答の状態と段階。**他の個票と違い URL のパスが入る** (追跡中だけ)
        "/trace" => recent::trace(ep, query).into(),
        // 起きたことの時系列 (T14.11)。起動・再読込・ブロックリスト・IPv6・圧迫・
        // バラスト・状態ファイル・追い出し・accept の失敗・停止シグナルを 1 本に
        "/events" => recent::events(ep, query).into(),
        "/snapshot" => {
            // 17 本の URL を 1 要求で (T14.4)。`scripts/collect-deployed.sh` が保存する。
            // 試験のビルドだけ、旗を握ったまま組み立ての手前で止まれる (T22.6。既定のビルドには無い)
            #[cfg(feature = "test-hooks")]
            test_hooks::pause_snapshot();
            recent::snapshot(ep).into()
        }
        // 日次で残した `/snapshot` の一覧 (T14.34)
        "/snapshots" | "/snapshots/" => recent::snapshots().into(),
        "/dns" => recent::dns(query).into(),
        // SLO の達成率 (T14.50)。5 秒の標本ごとの判定を日ごと・時間ごとに畳んで返す
        "/slo" => recent::slo(query).into(),
        // 1 日 1 行の要約 (T14.20)。`/history` (30 日) が消えたあとも残る
        "/daily" => recent::daily(query).into(),
        "/log" => recent::log(ep, query).into(),
        // ホスト別の時系列 (上位 16 ホスト × 5 分 × 24 時間。T14.22)
        "/hosts/series" => recent::host_series(ep, query).into(),
        "/hosts" => recent::hosts(ep, query).into(),
        // 接続元の個票 (T14.7)。認証なしで誰でも見えるのは他の個票と同じ
        "/clients" => recent::clients(ep, query).into(),
        // 内部エンドポイントを**引いた側**の一覧 (T14.53)。`/clients` が
        // 「プロキシとして通した相手」なのに対して、こちらは「個票を読んでいる相手」。
        // 上限のバイト数は個票の口と同じ 256 KiB (`recent::MAX_BODY`)
        "/readers" => (
            200,
            "application/json",
            ep.metrics.readers_body(recent::MAX_BODY),
        )
            .into(),
        // 1 相手の説明 (T14.36)。上の口を横断して読む作業をサーバー側で 1 枚に組む
        "/explain" => explain::explain(ep, query).into(),
        // 効いている設定とその出どころ、この環境で何が読めるか (T14.15)
        "/config" => config::render(ep).into(),
        "/blocklist" => blocklist::handle(&parse_query(query.unwrap_or(""))).into(),
        // `/status` の写しではなく**本当の健康診断** (T14.12)。軽い JSON で、
        // **`fatal` な**検査が 1 つでも偽なら 503 (本文の `ok` は全部込み。T15.0 (6))。
        // **問い合わせは読まない** (監視が叩く口の意味を変えない)
        "/healthz" => health::healthz(ep).into(),
        "/status" => status::status(ep, query),
        "/metrics" => status::prometheus(ep),
        "/purge" => purge::purge(ep, query),
        "/lookup" => purge::lookup(ep, query),
        // ブラウザでプロキシの URL を開いた人への案内 (`--lite` でも出す)
        "/" | "" => (200, "text/plain; charset=utf-8", endpoint_list(ep.lite)).into(),
        _ => match path.strip_prefix("/snapshots/") {
            // 残してある 1 日ぶんをそのまま (T14.34)。起動と停止の 1 枚 (`<時刻>-start|shutdown`。
            // T28.20) も同じ口。どちらとしても読めない名前は 404
            Some(date) => recent::snapshot_file(date).into(),
            // 自分宛てだが知らないパス: 自分へ転送するとループするので 404
            None => (
                404,
                "text/plain; charset=utf-8",
                format!("not found.\n\n{}", endpoint_list(ep.lite)),
            )
                .into(),
        },
    }
}
