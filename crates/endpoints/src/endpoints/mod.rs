//! プロキシ自身のエンドポイント: `/dashboard` (コントロールパネル)、`/status`
//! (`?sort=requests|errors|dns|slow` で `hosts[]` の上位 50 の切り出しを変えられる。T13.3)、
//! `/healthz` (**本当の健康診断**。軽い JSON で、**`fatal` な**検査が 1 つでも偽なら 503。
//! 本文の `ok` は `fatal` でない検査も込み。T14.12 / T15.0)、
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
//! 書き換える口も読む口と同じくポートだけで決め、名前は見ない (T29.35、利用者の決定
//! 「すべて外から使える」。T28.40 で名前を自分のときだけに絞り、T28.47 で `PROXY_SELF_NAMES` を
//! 足したのを戻した。公開の名前で開いたダッシュボードの「消す」「上書き」が転送になっていたため)。
//! **重い口** (`/snapshot` `/profile` `/explain` と大きく引いた `/hosts` `/recent` `/history`) は
//! **同時に 1 本だけ**組み、2 本目からは `503` + `Retry-After: 1` で断る (T14.51)。
//!
//! 中の割り方 (T34.4): 要求の読み方は [`incoming`] (ポートだけの判定・メソッドの種類・パスと
//! 問い合わせ)、門は [`heavy`] (重い口の旗) と [`handle`] (読み取り専用)、経路表は [`dispatch`](mod@dispatch)、
//! 応答の形は [`respond`]。`/status` `/metrics` は [`status`]、`/history` は [`history`]、
//! `/purge` `/lookup` は [`purge`]、ページ 3 本と `/` の案内は [`pages`]。

use std::io::{self, Write};
use std::sync::atomic::Ordering;

use crate::cache::Cache;
use crate::log_debug;
use crate::metrics::{self, Metrics};

// 要求 1 本ぶんの文脈と問い合わせの読み方は葉クレート `proxy-endpoints-core` に、
// `/explain` は `proxy-endpoints-explain` にある (T15.12 段 1 (ii))。
// **今までと同じ綴りで使えるように**ここで出し直す (`crate::endpoints::Endpoint`、
// 子モジュールの `super::parse_query` / `super::has_flag` / `super::offset_param` /
// `super::next_offset`、`super::param` / `super::num_param` / `super::str_param` (T34.93)、
// `explain::explain`)。
pub use proxy_endpoints_core::{Endpoint, parse_query, percent_decode};
use proxy_endpoints_core::{has_flag, next_offset, num_param, offset_param, param, str_param};
use proxy_endpoints_explain as explain;

mod blocklist;
mod config;
mod dispatch;
mod health;
mod heavy;
mod history;
mod incoming;
mod pac;
mod pages;
mod profile;
mod purge;
mod recent;
mod respond;
mod status;
// 試験のときだけ効く口 (T22.6)。既定のビルドには入らない
#[cfg(feature = "test-hooks")]
pub mod test_hooks;

pub use heavy::{HeavyGuard, begin_heavy};

use dispatch::dispatch;
use heavy::is_heavy;
use incoming::Incoming;
use respond::Reply;

/// 日次の `/snapshot` (T14.34) を組むのに要るもの。[`register_snapshot`] に渡す。
pub struct SnapshotSource {
    pub metrics: std::sync::Arc<Metrics>,
    pub cache: std::sync::Arc<Cache>,
    /// 待ち受けポート (`Endpoint` の自分宛て判定と同じ値。中身には出ない)
    pub port: u16,
    pub version: &'static str,
    /// ビルドの種類 (`/status` の `build`。T28.42)
    pub build: &'static str,
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
    // 引数は「`recent` の部の上限」(`None` = 256 KiB で切る。停止の 1 枚は 4 MiB = 全部 (T28.20)、
    // 日次は 1 MiB (T28.46))
    crate::snapshots::set_builder(Box::new(move |recent_max, limit| {
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
            build: src.build,
            concurrency: &*src.concurrency,
        };
        recent::snapshot_with(&ep, recent_max, limit).2
    }));
}

/// 内部エンドポイントなら応答して `Ok(true)` を返す。そうでなければ何もせず `Ok(false)`。
///
/// 順は 4 段: 読む ([`Incoming::parse`]) → 読み手を数える → 門 (重い口の旗・読み取り専用) →
/// 口を選んで組む ([`dispatch`](fn@dispatch)) → 1 本の応答にして書く ([`respond::render`])。
pub fn handle(
    client: &mut impl Write,
    method: &str,
    target: &str,
    ep: &Endpoint<'_>,
) -> io::Result<bool> {
    // ここで `Some` になった要求はどちらの形式でも自分宛て (`local_path` がポートで判定済み)
    let Some(req) = Incoming::parse(method, target, ep.port, ep.host) else {
        return Ok(false);
    };
    // **内部エンドポイントを引いた接続元を 1 行残す** (`/status` の `readers` と
    // `/readers`。T14.53)。ここまで来た要求は自分宛てと決まっているので、
    // プロキシとして通す要求 (CONNECT / forward) はこの行を 1 度も通らない
    // (T14.7 の `clients[]` はその逆で、自分宛てだけの接続を数えない)。
    // 渡すのは**問い合わせ文字列を外したパス**で、数えるのは 1 要求につき 1 回だけ。
    // 応答を組む前に数えるので、**読んだ応答にはその要求自身が入っている**
    // (`/status` を 3 回引いた 3 回目の応答が `count: 3`)。
    // **`PURGE <url>` は `/purge` として残す** (同じ仕事をする `/purge?url=` と同じ口)。
    // PURGE の `req.path` は要求ターゲットの URL そのもので、前は他所の URL が記録に残っていた (T34.160)
    if let Some(client) = ep.client {
        let path = if req.method.is_purge() {
            "/purge"
        } else {
            req.path
        };
        ep.metrics.record_reader(client, path);
    }
    // **重い口は同時に 1 本だけ**組む (T14.51)。取れなければ下で `503` + `Retry-After: 1`。
    // 番人は口を組むところより長く生き、**組み終わったところ**で旗を戻す (途中で `?` で
    // 抜けてもパニックしても戻る)。軽い口はこの `if` にも原子にも触らない。
    // **読み手の記録 (T14.53) の直後**に置くので、断った要求も読み手としては数える
    // (走査が来ていることが `/readers` から読める)
    let mut heavy_busy = false;
    let heavy_guard = if is_heavy(req.method.is_get(), req.path, req.query) {
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
    let reply = if heavy_busy {
        // 既に 1 本走っている (T14.51)。**組まずに**断るので、これ自体は安い
        Reply::json_error(503, "busy")
    } else if ep.readonly && req.is_write() {
        Reply::json_error(405, "read-only (PROXY_ENDPOINTS_READONLY=on)")
    } else {
        dispatch(ep, &req, target)
    };
    // **組み終わったので旗を戻す** (T14.51)。client への書き出しまで握ると、読むのが
    // 遅い相手 1 人で `/snapshot` `/profile` が全員に 503 を返し続けることになる
    drop(heavy_guard);
    let response = respond::render(&reply, heavy_busy, req.method.is_head());
    client.write_all(response.as_bytes())?;
    client.flush()?;
    // 自分宛ての要求の行は debug (T28.9)。ダッシュボードを開いたままにすると約 1.2 行/秒で
    // コンソールを埋めていた (`PROXY_LOG_LEVEL=debug` にすれば今までどおり見える)
    log_debug!(
        Some(ep.conn_id),
        "{} {} -> {} (internal endpoint)",
        method,
        target,
        reply.status
    );
    Ok(true)
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
