//! `/history` (時系列の JSON。`res=5|60|3600&n=`、カーネルと cgroup の窓が `kernel` に付く。
//! T14.32) と `/history?since=&until=&summary=1` (期間を畳んだ 1 行だけ。T14.24) の組み立て。
//!
//! 問い合わせの読み方 ([`summary_params`]) と時刻の求め方 ([`now_and_start`]) は時計を持たない
//! 純粋な関数なので、そのまま単体試験で縛ってある。

use super::respond::Reply;
use super::{Endpoint, param, parse_query};

/// `/history` — `?summary=1` なら期間を畳んだ 1 行だけ、そうでなければ標本の配列。
pub(super) fn history(ep: &Endpoint<'_>, query: Option<&str>) -> Reply {
    let params = parse_query(query.unwrap_or(""));
    let res = param(&params, "res").and_then(|v| v.parse::<u64>().ok());
    if params.iter().any(|(k, v)| k == "summary" && v != "0") {
        // 期間を畳んだ 1 行だけ (標本は返さない。T14.24)
        return (200, "application/json", history_summary(ep, &params, res)).into();
    }
    let res = res.map_or(0, crate::history::History::index_for);
    // `n=` は**新しい方から何本返すか** (既定 720、`res=5` だけ 4,320 まで。T14.32)
    let n = param(&params, "n").and_then(|v| v.parse::<usize>().ok());
    (200, "application/json", history_body(ep, res, n)).into()
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
fn history_summary(ep: &Endpoint<'_>, params: &[(String, String)], res: Option<u64>) -> String {
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let (now, started) = now_and_start(wall, ep.metrics.start_time.elapsed());
    crate::history::summary::of(
        &ep.metrics.history,
        &summary_params(params, res, now, started),
    )
    // 直近 1,024 本の正確な分位点を**別の鍵で**添える (T14.31)。`p50_ms` は期間を畳んだ
    // 区間の補間、`recent_quantiles` は期間に関わらず直近 1,024 本の実測なので別物
    .to_json_with(Some(&ep.metrics.recent_quantiles_json()))
}

/// 今と起動時刻 (どちらも epoch 秒の切り捨て) を、**同じ 1 回の時計の読み** `wall` と
/// 稼働時間 `elapsed` から出す (T21.4)。
///
/// 端数を残したまま引いてから切り捨てる。前は `now_epoch() − elapsed().as_secs()`
/// (切り捨て同士の引き算) で、今の秒の端数が起動の端数より小さいと起動時刻が 1 秒
/// 遅く出て、起動と同じ秒に取った標本 (`t` は取った時刻の切り捨て) が
/// `since=restart` から外れていた (起動直後の要約が `"connects":0` になった)。
fn now_and_start(wall: std::time::Duration, elapsed: std::time::Duration) -> (u64, u64) {
    (wall.as_secs(), wall.saturating_sub(elapsed).as_secs())
}

/// `?since=&until=&res=&normal_hours_only=` を読む (**時計を持たない**ので試験できる)。
///
/// `since=restart` は起動時刻 `started` (epoch 秒の切り捨て。`/status` の `since_start_secs`
/// と同じ起動から)。起動のあとに取った標本の `t` は必ず `started` 以上になる (T21.4)。
/// 数として読めない値と書いていない `since` は 0 (残っているいちばん古い標本から)、
/// `until` は今。`normal_hours_only=1` でバーストの標本を外す (T14.0 の「平常時」。閾は 1 時間 300 本と
/// その期間の中央値 × 3 の大きい方 = `history::summary::normal_limit`。T29.13)。
fn summary_params(
    params: &[(String, String)],
    res: Option<u64>,
    now: u64,
    started: u64,
) -> crate::history::summary::Params {
    let val = |key: &str| param(params, key);
    crate::history::summary::Params {
        since: match val("since") {
            Some("restart") => started,
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

#[cfg(test)]
mod tests {
    use super::super::pages::endpoint_list;
    use super::{now_and_start, parse_query, summary_params};

    /// `/history?summary=1` の引数の読み方 (T14.24)。**`since=restart` が
    /// `/status` の `since_start_secs` と合う**ことがこのタスクの受け入れ基準の 1 つ。
    #[test]
    fn the_summary_params_read_since_restart_and_the_period() {
        let now = 1_757_000_000u64;
        let q = |s: &str| parse_query(s);
        // `since=restart` = 起動時刻 (= `/status` の窓の始まり)
        let p = summary_params(&q("summary=1&since=restart"), None, now, now - 7_200);
        assert_eq!((p.since, p.until), (now - 7_200, now));
        assert!(!p.normal_hours_only);
        // 期間 2 時間なら 60 秒の窓が自動で選ばれる (1 時間を越えるので 5 秒では足りない)
        assert_eq!(p.res_index(), 1);
        // epoch で切る / `?res=` と `normal_hours_only=1` を足す
        let p = summary_params(
            &q("since=1756000000&until=1756100000&normal_hours_only=1"),
            Some(3600),
            now,
            now - 10,
        );
        assert_eq!((p.since, p.until), (1_756_000_000, 1_756_100_000));
        assert!(p.normal_hours_only);
        assert_eq!(p.res_index(), 2);
        // 書いていなければ「残っている全部」から「今」まで、数として読めない値も同じ
        let p = summary_params(&q("summary=1&since=yesterday"), None, now, now - 10);
        assert_eq!((p.since, p.until), (0, now));
        assert_eq!(p.res_index(), 2);
        // `normal_hours_only=0` は切らない
        assert!(!summary_params(&q("normal_hours_only=0"), None, now, now - 10).normal_hours_only);
        // 案内にも載っている
        assert!(endpoint_list(false).contains("/history?since=&until=&summary=1"));
    }

    /// `since=restart` の起動時刻は端数を残して引いてから切り捨てる (T21.4)。
    /// 起動 1,000.7 秒・今 1,001.2 秒 (稼働 0.5 秒) のとき、起動と同じ秒の 1,000.9 秒に
    /// 取った標本 (`t` = 1,000) が入るには起動時刻が 1,000 でなければならない。
    /// 切り捨て同士の引き算 (1,001 − 0) だと 1,001 になって外れていた。
    #[test]
    fn since_restart_does_not_skip_the_second_the_proxy_started_in() {
        use std::time::Duration;
        let ms = Duration::from_millis;
        let (now, started) = now_and_start(ms(1_001_200), ms(500));
        assert_eq!((now, started), (1_001, 1_000));
        // 前の求め方では起動の次の秒になっていた (この差が「8 回に 1 回」の正体)
        assert_eq!(ms(1_001_200).as_secs() - ms(500).as_secs(), 1_001);
        let p = summary_params(&parse_query("summary=1&since=restart"), None, now, started);
        let sample_t = ms(1_000_900).as_secs();
        assert!(p.since <= sample_t && sample_t <= p.until, "{:?}", p);
        // 端数が逆向きでも、稼働が長くても同じ (起動 1,000.2 秒・稼働 2 時間と 0.9 秒)
        assert_eq!(now_and_start(ms(8_201_100), ms(7_200_900)), (8_201, 1_000));
        // 時計が稼働時間より小さい (壊れた時計) ときは 0 から
        assert_eq!(now_and_start(ms(300), ms(500)), (0, 0));
    }
}
