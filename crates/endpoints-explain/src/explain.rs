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
//!
//! 中の割り方 (T.34.4): ホストは [`host`]、接続元は [`client`]。どちらも「表とリングを読んで
//! 材料を集める」(`gather`) と「材料を JSON に並べる」(`render`。時計もリングも読まない) に
//! 分けてあるので、鍵の並びは材料を手で作って単体試験で縛れる。

mod client;
mod host;
#[cfg(test)]
mod tests;

use crate::metrics::SCHEMA_HEAD;
use crate::{Endpoint, parse_query};

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
        (200, "application/json", host::host_body(ep, &host))
    } else if !client.is_empty() {
        (200, "application/json", client::client_body(ep, &client))
    } else {
        (
            400,
            "application/json",
            format!(
                "{}\"error\":\"use /explain?host=<name[:port]> or /explain?client=<ip>\"}}",
                SCHEMA_HEAD
            ),
        )
    }
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
