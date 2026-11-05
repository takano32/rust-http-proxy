//! `/status` (集計の JSON。`?sort=` と `?hosts=0`) と `/metrics` (Prometheus) の組み立て。

use super::respond::Reply;
use super::{Endpoint, parse_query};
use crate::metrics;
use crate::persist;
use crate::prom;
use crate::reload;

/// `/status` — `?sort=requests|errors|dns|slow` は `hosts[]` の上位 50 を切り出す鍵だけを変える
/// (T13.3)。`?hosts=0` は `hosts[]` を丸ごと省く軽い口 (T25.1 段 2)。どちらも知らない値は既定に倒す。
pub(super) fn status(ep: &Endpoint<'_>, query: Option<&str>) -> Reply {
    let params = parse_query(query.unwrap_or(""));
    let sort = host_sort(&params);
    let hosts = include_hosts(&params);
    (200, "application/json", status_body(ep, sort, hosts)).into()
}

/// `/metrics` — Prometheus のテキスト形式。
///
/// 上限といまのスレッド数はここで 1 回だけ引く (`/status` と同じ形)。
/// 版とビルドの種類は末尾に 1 行 (T.29.19。描く側はこの層の `Endpoint` しか持たない)。
pub(super) fn prometheus(ep: &Endpoint<'_>) -> Reply {
    let mut body = prom::render(ep.metrics, Some(ep.cache), (ep.concurrency)());
    prom::build_info(&mut body, ep.version, ep.build);
    (200, "text/plain; version=0.0.4; charset=utf-8", body).into()
}

/// `/status?sort=requests|errors|dns|slow` の切り出しの鍵 (知らない値は既定 = 要求数順。T13.3)。
fn host_sort(params: &[(String, String)]) -> metrics::HostSort {
    params
        .iter()
        .find(|(k, _)| k == "sort")
        .map_or(metrics::HostSort::Requests, |(_, v)| {
            metrics::HostSort::from_param(v)
        })
}

/// `/status?hosts=0` のときに `hosts[]` を省くか (T25.1 段 2)。
///
/// **`0` のときだけ偽**。書いていない・`1`・知らない値は今までどおり全部を返す
/// (`?sort=` と同じ「知らない値は既定に倒す」)。`/status` は `hosts` が応答の 8 割を
/// 占め、宛先が増えるほど育つ (`MAX_HOSTS` = 1,000 で約 538 KB) ので、監視が 30 秒ごとに
/// 引く口はここを落とした軽い形を使う。
fn include_hosts(params: &[(String, String)]) -> bool {
    params
        .iter()
        .find(|(k, _)| k == "hosts")
        .is_none_or(|(_, v)| v != "0")
}

/// `/status` の本体を組み立てる (`?sort=` は `hosts[]` の上位 50 を切り出す鍵。T13.3)。
///
/// `hosts` は `hosts[]` を出すかどうか (`/status?hosts=0` で偽。T25.1 段 2)。
/// 偽のときは `hosts` の鍵ごと省き、**ほかの鍵は 1 バイトも変えない**。
///
/// **`/snapshot` (T14.4) もここを呼ぶ** (`hosts` は常に真 = 今までどおり全部): 1 要求で
/// 全部取るのに自分へ HTTP で繋ぎ直すと、上限に当たっているときに取れない・接続を 17 本
/// 増やす・測る行為が状態を変える。同じプロセス内の関数呼び出しで組む。
///
/// `/status` の組み立てはこの層の仕事 (指標は部品を並べるだけにしてある。
/// 下の層が上の層を呼ぶと依存が輪になるため)。
pub(super) fn status_body(ep: &Endpoint<'_>, sort: metrics::HostSort, hosts: bool) -> String {
    let mut out = ep.metrics.to_json_with_cache(
        Some(ep.cache),
        metrics::StatusExtras {
            settings: &reload::status_json(),
            blocklist: &crate::blocklist::status_json(),
            state_file: &persist::status_json(),
            // 状態ファイルが持っている「起動 60 秒の heap」(`/status` の `memory.baseline`。T24.4)
            baseline: &persist::baseline_json(),
            version: ep.version,
            concurrency: (ep.concurrency)(),
            sort,
            hosts,
        },
    );
    // ビルドの種類は**末尾に足す** (T.28.42。既存の鍵の順は 1 バイトも変えない)。
    // 値は cargo のプロファイル名なので英数字と `-` `_` だけ (逃がす文字は無いが、念のため通す)
    if out.ends_with('}') {
        out.pop();
        out.push_str(",\"build\":\"");
        out.push_str(&crate::json::escape(ep.build));
        out.push_str("\"}");
    }
    out
}

#[cfg(test)]
mod status_query_tests {
    use super::{host_sort, include_hosts, parse_query};
    use crate::metrics::HostSort;

    /// `?hosts=0` のときだけ `hosts[]` を省き、書いていない・`1`・知らない値は
    /// 今までどおり全部を返すこと (T25.1 段 2)。
    #[test]
    fn only_hosts_zero_drops_the_hosts_table() {
        let q = |s: &str| parse_query(s);
        assert!(include_hosts(&q("")), "書いていなければ今までどおり");
        assert!(include_hosts(&q("sort=errors")));
        assert!(include_hosts(&q("hosts=1")));
        // 知らない値は既定 (全部) に倒す
        assert!(include_hosts(&q("hosts=off")));
        assert!(include_hosts(&q("hosts=")));
        // `0` のときだけ省く。`?sort=` と一緒でも読める (順は問わない)
        assert!(!include_hosts(&q("hosts=0")));
        assert!(!include_hosts(&q("sort=errors&hosts=0")));
        assert!(!include_hosts(&q("hosts=0&sort=errors")));
    }

    /// `?sort=` の読み方は今までどおり (T13.3。知らない値は要求数順に倒す)。
    #[test]
    fn the_sort_key_still_falls_back_to_requests() {
        let q = |s: &str| parse_query(s);
        assert_eq!(host_sort(&q("")), HostSort::Requests);
        assert_eq!(host_sort(&q("sort=errors")), HostSort::Errors);
        assert_eq!(host_sort(&q("sort=dns")), HostSort::Dns);
        assert_eq!(host_sort(&q("sort=slow")), HostSort::Slow);
        assert_eq!(host_sort(&q("sort=nope")), HostSort::Requests);
        // `?hosts=0` と一緒でも鍵は読める
        assert_eq!(host_sort(&q("hosts=0&sort=slow")), HostSort::Slow);
    }
}
