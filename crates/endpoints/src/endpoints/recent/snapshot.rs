//! 1 回で全部の `/snapshot` と、残してあるスナップショットの一覧・1 枚 (`/snapshots`。T14.34・T28.20)。

use super::super::history::history_body;
use super::super::profile;
use super::super::status::status_body;
use super::*;

/// `/snapshots` — 日次で残した `/snapshot` の一覧 (T14.34)。
///
/// 履歴スレッドが UTC の日付をまたいだ瞬間に `$HOME/.rust-http-proxy/snapshots/<日付>.json`
/// へ 1 ファイル書いている (既定 30 日ぶん、`PROXY_SNAPSHOT_DAYS`)。ここはその置き場所と
/// 日付・大きさを並べるだけで、中身を読むのは [`snapshot_file`] (`/snapshots/<date>`)。
/// `PROXY_STATS_PERSIST=off` と `PROXY_SNAPSHOT_DAYS=0` では 1 つも書いていないので
/// `dir` が `null` で `files` は空。
pub(in crate::endpoints) fn snapshots() -> (u16, &'static str, String) {
    let l = crate::snapshots::list();
    let mut out = String::with_capacity(1024);
    out.push_str(SCHEMA_HEAD);
    out.push_str("\"files\":");
    let (shown, cut) = array_within(
        &mut out,
        l.files.iter().map(|f| {
            format!(
                "{{\"date\":\"{}\",\"bytes\":{},\"t\":{}}}",
                f.date, f.bytes, f.t
            )
        }),
    );
    // 起動と停止の 1 枚 (T28.20) は別の並びにする (日次の `files` を読む道具が、日付で
    // 取りに行く口に名前を渡して 404 にならないように)。**新しい欄は末尾に足す**
    out.push_str(",\"moments\":");
    let (moments_shown, moments_cut) = array_within(
        &mut out,
        l.moments.iter().map(|m| {
            format!(
                "{{\"name\":\"{}\",\"kind\":\"{}\",\"bytes\":{},\"t\":{}}}",
                m.name,
                m.kind.name(),
                m.bytes,
                m.t
            )
        }),
    );
    let _ = write!(
        out,
        ",\"count\":{},\"shown\":{},\"bytes\":{},\"days\":{},\"max_bytes\":{},\"dir\":{},\"truncated\":{},\"moments_count\":{},\"moments_shown\":{},\"moments_bytes\":{},\"moments_truncated\":{},\"daily_max_bytes\":{}}}",
        l.files.len(),
        shown,
        l.bytes,
        l.days,
        crate::snapshots::MAX_FILE,
        crate::json::quote_opt(l.dir.as_ref().map(|p| p.display().to_string()).as_deref()),
        cut,
        l.moments.len(),
        moments_shown,
        l.moments_bytes,
        moments_cut,
        // ディスクに書く 1 枚の上限 (T29.16 で日次、T38.7 で起動と停止も)。`max_bytes` (`/snapshot` の上限) も T39.8 で同じ値
        crate::snapshots::DAILY_MAX_FILE
    );
    (200, "application/json", out)
}

/// `/snapshots/<YYYY-MM-DD>` — 残してある 1 日ぶんを**そのまま**返す (T14.34)。
/// `/snapshots/<YYYY-MM-DDTHHMMSSZ>-start|shutdown` (起動と停止の 1 枚。T28.20) も同じ口。
///
/// 中身は `/snapshot` の応答そのもの (組み直さない) なので、`scripts/snapshot-diff.py` や
/// `scripts/collect-deployed.sh --from-server` がそのまま読める。**版 (`schema`) も
/// 書いた時のまま**で、読む道具は版の無い古いファイルも今までどおり読める (T14.49)。日付として読めない名前と
/// 置いていない日は 404 (`..` を書かれても置き場所の外は見ない)。
pub(in crate::endpoints) fn snapshot_file(date: &str) -> (u16, &'static str, String) {
    match crate::snapshots::read(date) {
        Some(body) => (200, "application/json", body),
        None => (
            404,
            "application/json",
            format!(
                "{}\"error\":\"no snapshot for that day (or name)\",\"date\":{},\"see\":\"/snapshots\"}}",
                SCHEMA_HEAD,
                crate::json::quote(date)
            ),
        ),
    }
}

/// `/snapshot` の上限 (6 MiB = ディスクに書く 1 枚と同じ `snapshots::DAILY_MAX_FILE`。T39.8)。
/// 個票 1 本 1 本の上限 ([`MAX_BODY`]) とは別枠。
///
/// デプロイ先から 1 日 1 回取って保存する大きさなので、回線を占めない・エディタで
/// 開ける・`python -m json.tool` が通る、の 3 つが収まる値にしてある。T39.8 より前は 4 MiB で、
/// 0.38.10 の締めの 1 枚は 3,400,448 B (81.1%) で `recent`・`hosts`・`dns`・`profile` が部ごとに切れ、
/// `collect-deployed.sh` が続きを 7.4 秒かけて繋いでいた。
pub(super) const MAX_SNAPSHOT: usize = crate::snapshots::DAILY_MAX_FILE as usize;

/// `/snapshot` が越えたときに落とす順 (大きいものから)。落としたものは `"dropped"` に出る。
const DROP_ORDER: [&str; 3] = ["recent", "log", "history.5"];

/// ディスクに書く 1 枚で部の上限を外す部と、全体の上限を越えたときに**切った形へ戻す順** (T38.7)。
///
/// 締めの判定の材料として大事なものほど後ろ (最後まで全部のまま残す): `profile` (段階とスレッドの
/// 標本) → `hosts` (ホスト別の通算) → `dns` (答えのある名前 1 つあたりの裏の引き直しの分母) →
/// `recent` (接続のエラーの行の分母。T28.20 から全部入れている)。
const WHOLE_ORDER: [&str; 4] = ["profile", "hosts", "dns", "recent"];

/// 部を組む問い合わせ (いつもの `/snapshot` とディスクに書く 1 枚、切った形へ戻すときで同じもの)。
const RECENT_QUERY: &str = "n=2000";
const DNS_QUERY: &str = "sort=age&limit=4096";
const HOSTS_QUERY: &str = "limit=1000";
const PROFILE_QUERY: &str = "res=5";

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
/// `connections`、`recent`、`hosts`、`clients` (T14.7)、`bursts` (T14.6)、
/// `profile` (T14.3)、`log`。
/// **`history.*` にはカーネルと cgroup の窓 (`kernel`) も一緒に入る** (T14.12。`/history` と同じ組み立て)。
/// **T14.3 の `/profile` は、入ったらここに 1 行足す** (`parts` に名前が出るので、
/// 読む側は「この版に何が入っていたか」を JSON だけで判別できる)。
///
/// **組み方はディスクに書く起動と停止の 1 枚と同じ** (T39.8): `recent`・`dns`・`hosts`・`profile` を部の
/// 256 KiB で切らずに入れ、全体が [`MAX_SNAPSHOT`] を越えたら [`WHOLE_ORDER`] の順に切った形へ戻す。
pub(in crate::endpoints) fn snapshot(ep: &Endpoint<'_>) -> (u16, &'static str, String) {
    disk_snapshot(ep, Some(MAX_SNAPSHOT), MAX_SNAPSHOT)
}

/// **ディスクに書く 1 枚** (日次・起動・停止。`snapshots` の組み方として預ける。T38.7)。いつもの
/// `/snapshot` も同じ組み方 (T39.8)。
///
/// `recent` を `recent_max` まで入れ、`dns`・`hosts`・`profile` も部の 256 KiB ([`MAX_BODY`]) で切らずに
/// 全体の上限 (`limit`) まで入れる。0.36.34 の停止の 1 枚は全体が 4 MiB の 94% で、この 3 つの部が
/// 部ごとの上限で切れていた (`/dns` は 1,122 行のうち 694 行。締めの「前」の材料が欠けた)。
/// 合計が `limit` を越えるときは [`WHOLE_ORDER`] の順に**切った形に戻してから**いつもの順
/// ([`DROP_ORDER`]) に落とす (全部入れたせいで部ごと `null` にはしない)。
/// `/dns`・`/hosts`・`/profile` の口は変えない。
pub(in crate::endpoints) fn disk_snapshot(
    ep: &Endpoint<'_>,
    recent_max: Option<usize>,
    limit: usize,
) -> (u16, &'static str, String) {
    build_snapshot(ep, recent_max, true, limit)
}

/// [`disk_snapshot`] の本体。`recent_max` が `Some(上限)` なら **`recent` を部の 256 KiB ([`MAX_BODY`]) で
/// 切らずに、その上限まで入れる** (起動と停止と `/snapshot` は 2,000 件を全部。T28.20・T39.8。日次は
/// `snapshots::DAILY_RECENT_MAX` = 1 MiB。T28.46)。`whole` なら `dns`・`hosts`・`profile` も部の上限を外す
/// (T38.7)。`false` は試験だけが使う (T39.8 より前のいつもの `/snapshot` の形)。
///
/// `limit` は全体の上限 (どれも `snapshots::DAILY_MAX_FILE` = 6 MiB。T29.16・T38.7・T39.8)。`limit_bytes` に出る。
/// 合計が上限を越えるときは**切った形に戻してから**いつもの順に落とす (全部入れたせいで部ごと `null` にはしない)。
fn build_snapshot(
    ep: &Endpoint<'_>,
    recent_max: Option<usize>,
    whole: bool,
    limit: usize,
) -> (u16, &'static str, String) {
    let mut part = collect_parts(ep, recent_max, whole, limit);
    // 部の上限を外した部 (`recent` は `recent_max` があるときだけ)
    let opened: Vec<&str> = WHOLE_ORDER
        .into_iter()
        .filter(|&name| {
            if name == "recent" {
                recent_max.is_some()
            } else {
                whole
            }
        })
        .collect();
    fit_whole_parts(&mut part, limit, &opened, |name| cut_part(ep, name));
    let dropped = drop_to_fit(&mut part, limit);
    let at = crate::cache::now_epoch();
    let uptime = ep.metrics.start_time.elapsed().as_secs();
    let body = render_snapshot(ep.version, uptime, at, limit, &part, &dropped);
    (200, "application/json", body)
}

/// 17 の部を、それぞれの口と同じ関数で組む (T34.91 で `/snapshot` の本体から出した)。
///
/// 並びは `parts` に出る順そのもの。`recent_max` と `whole` は [`build_snapshot`] と同じ。
fn collect_parts(
    ep: &Endpoint<'_>,
    recent_max: Option<usize>,
    whole: bool,
    limit: usize,
) -> Vec<(&'static str, String)> {
    use crate::history::History;
    use crate::metrics::HostSort;

    // 個票はどれも 256 KiB 以下、`/status` は 64 KiB 以下、履歴は 3 本で最大 2 MiB。
    // 先に全部組んでから、合計が上限を越えていたら切った形に戻し、それでも越えれば順に落とす
    // `status` の 3 通りは**今までどおり `hosts[]` も入れる** (軽い形は `/status?hosts=0`
    // だけの話で、`/snapshot` の `parts` は変えない。T25.1 段 2)
    vec![
        ("status", status_body(ep, HostSort::Requests, true)),
        ("status_errors", status_body(ep, HostSort::Errors, true)),
        ("status_dns", status_body(ep, HostSort::Dns, true)),
        // `/history` と同じ組み立て (カーネルと cgroup の窓も一緒に入る。T14.12)
        ("history.5", history_body(ep, History::index_for(5), None)),
        ("history.60", history_body(ep, History::index_for(60), None)),
        (
            "history.3600",
            history_body(ep, History::index_for(3600), None),
        ),
        (
            "dns",
            if whole {
                dns_within(Some(DNS_QUERY), limit).2
            } else {
                dns(Some(DNS_QUERY)).2
            },
        ),
        ("errors", errors(ep, Some("n=500")).2),
        ("connections", connections(ep).2),
        (
            "recent",
            match recent_max {
                Some(max) => recent_within(ep, Some(RECENT_QUERY), max.min(limit)).2,
                None => recent(ep, Some(RECENT_QUERY)).2,
            },
        ),
        (
            "hosts",
            if whole {
                hosts_within(ep, Some(HOSTS_QUERY), limit).2
            } else {
                hosts(ep, Some(HOSTS_QUERY)).2
            },
        ),
        // ホスト別の時系列 (上位 16 × 5 分 × 24 時間。T14.22)
        ("hosts_series", host_series(ep, Some("top=16")).2),
        ("clients", clients(ep, Some("limit=1000")).2),
        ("bursts", bursts(ep, Some("n=50")).2),
        // 待ちの段階・スレッドの CPU と状態・ロックの取り合い (T14.3)。
        // `--lite` では `{"profile":"off"}` の 1 行になる
        (
            "profile",
            if whole {
                profile::profile_within(ep, Some(PROFILE_QUERY), limit).2
            } else {
                profile::profile(ep, Some(PROFILE_QUERY)).2
            },
        ),
        ("events", events(ep, Some("n=512")).2),
        ("log", log(ep, Some("n=1000")).2),
    ]
}

/// 組んだ部を 1 つの JSON にする (I/O も時計も読まない純粋な関数。T34.91)。
///
/// `parts` の名前は落とした部 (`null`) も含めて全部 `"parts"` に並び、`dropped` に
/// 落とした名前が並ぶ。`history.*` の部は `"history":{"5":…,"60":…}` に入れ子にする。
fn render_snapshot(
    version: &str,
    uptime: u64,
    at: u64,
    limit: usize,
    part: &[(&'static str, String)],
    dropped: &[&'static str],
) -> String {
    let names: Vec<&'static str> = part.iter().map(|(k, _)| *k).collect();
    let total: usize = overhead_of(part) + part.iter().map(|(_, v)| v.len()).sum::<usize>();

    let mut out = String::with_capacity(total + 1024);
    // 応答の形の版は**いちばん先頭の鍵** (T14.49)。**各部にもそれぞれ入っている**
    // (部はどれもその口の出力そのものなので、切り出して 1 本の応答として読める)
    out.push_str(SCHEMA_HEAD);
    let _ = write!(
        out,
        "\"taken_at\":{},\"version\":\"{}\",\"uptime_secs\":{},\"limit_bytes\":{},\"parts\":[",
        at,
        crate::json::escape(version),
        uptime,
        limit,
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
    for (name, body) in part {
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
    out
}

/// 部の上限を外した部 (`opened`) で合計が `limit` を越えるなら、[`WHOLE_ORDER`] の順に 1 つずつ
/// `cut` (いつもの 256 KiB で切った形) に置き換え、収まったらそこで止める (T28.20・T38.7)。
/// 置き換えた部の名前を返す。
fn fit_whole_parts(
    part: &mut [(&'static str, String)],
    limit: usize,
    opened: &[&str],
    mut cut: impl FnMut(&str) -> String,
) -> Vec<&'static str> {
    let mut total: usize = overhead_of(part) + part.iter().map(|(_, v)| v.len()).sum::<usize>();
    let mut cuts = Vec::new();
    for name in WHOLE_ORDER {
        if total <= limit {
            break;
        }
        if !opened.contains(&name) {
            continue;
        }
        if let Some((_, body)) = part.iter_mut().find(|(k, _)| *k == name) {
            let short = cut(name);
            total = total - body.len() + short.len();
            *body = short;
            cuts.push(name);
        }
    }
    cuts
}

/// 部をいつもの `/snapshot` と同じ形 (部ごと 256 KiB で切る) で組み直す ([`fit_whole_parts`] が使う)。
fn cut_part(ep: &Endpoint<'_>, name: &str) -> String {
    match name {
        "recent" => recent(ep, Some(RECENT_QUERY)).2,
        "dns" => dns(Some(DNS_QUERY)).2,
        "hosts" => hosts(ep, Some(HOSTS_QUERY)).2,
        "profile" => profile::profile(ep, Some(PROFILE_QUERY)).2,
        _ => unreachable!("切った形の無い部: {}", name),
    }
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

#[cfg(test)]
mod tests {
    use super::super::tests::with_ep;
    use super::*;
    use crate::metrics::Metrics;

    use crate::recent::{CloseReason, ConnTally, MAX_RECENT, SIDES, STAGES};

    /// 環 (`MAX_RECENT` = 2,000 件) を `worst` なら最悪の値、そうでなければデプロイ先に
    /// ありがちな 1 件 (tokyo 10-25 の `/recent` の 1 行の形: IPv4 の接続元・IP の宛先・
    /// 数 KB の上り下り・RTT 250 ms) で埋める。
    fn fill(m: &Metrics, worst: bool) {
        let now = Instant::now();
        let long_host = format!("{}.example.net:65535", "sub.".repeat(30));
        for i in 0..(MAX_RECENT as u64) {
            let (client, host) = if worst {
                (
                    "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6",
                    long_host.as_str(),
                )
            } else {
                ("10.255.0.1", "40.126.31.0:443")
            };
            let slot = m.conns.register(i, client, now).expect("登録できる");
            slot.begin_tunnel(host);
            let tally = if worst {
                ConnTally {
                    up: u64::MAX,
                    down: u64::MAX,
                    status: 599,
                    stage_ms: [u64::MAX; STAGES],
                    rtt_us: [u32::MAX; SIDES],
                    retrans: [u32::MAX; SIDES],
                    syn_retrans: u8::MAX,
                    stall_ms: [u32::MAX; SIDES],
                }
            } else {
                ConnTally {
                    up: 4_497,
                    down: 5_176,
                    status: 0,
                    stage_ms: [249; STAGES],
                    rtt_us: [248_790, 248_313],
                    retrans: [0; SIDES],
                    syn_retrans: 0,
                    stall_ms: [0; SIDES],
                }
            };
            let reason = if worst {
                CloseReason::Error(crate::metrics::ErrCause::Unreachable)
            } else {
                CloseReason::ClientEof
            };
            slot.finish(reason, tally, if worst { u32::MAX } else { 0 });
            m.record_closed(i);
        }
    }

    /// T39.8 より前のいつもの `/snapshot` の形 (部ごと 256 KiB で切り、全体は 4 MiB)。
    fn cut_snapshot(ep: &Endpoint<'_>) -> String {
        build_snapshot(ep, None, false, 4 * 1024 * 1024).2
    }

    /// スナップショットの `recent` の部 (その部の閉じ括弧までではなく、部の始まりから最後まで)。
    fn part_of(body: &str) -> &str {
        let at = body
            .find(",\"recent\":{\"schema\":")
            .expect("recent の部が無い");
        &body[at..]
    }

    /// スナップショットの `recent` の部の大きさ (`"hosts"` の部の手前まで)。
    fn recent_bytes(body: &str) -> usize {
        let p = part_of(body);
        p.find(",\"hosts\":").expect("hosts の部が無い")
    }

    /// **T28.46 の受け入れ基準**: 日次の 1 枚 (`recent_max` = `DAILY_RECENT_MAX`) は
    /// `recent` を 1 MiB まで持ち、ありがちな行なら**環の 2,000 件が全部入る**
    /// (いつもの `/snapshot` は 256 KiB で切って 2,000 件を割る)。最悪の値の行は 1 MiB で切る。
    #[test]
    fn the_daily_snapshot_keeps_the_whole_ring_within_one_mib() {
        let daily = Some(crate::snapshots::DAILY_RECENT_MAX);
        assert_eq!(crate::snapshots::DAILY_RECENT_MAX, 1024 * 1024);
        let m = Metrics::new();
        fill(&m, false);
        let (usual, day) = with_ep(&m, |ep| {
            (
                cut_snapshot(ep),
                build_snapshot(ep, daily, false, crate::snapshots::DAILY_MAX_FILE as usize).2,
            )
        });
        println!(
            "T28.46 recent 2,000 件 (ありがちな行): いつものスナップショット {} B (recent {} B) / 日次 {} B (recent {} B)",
            usual.len(),
            recent_bytes(&usual),
            day.len(),
            recent_bytes(&day)
        );
        assert!(part_of(&usual).contains("\"truncated\":true"));
        let p = part_of(&day);
        assert!(p.contains("\"truncated\":false"), "{}", &p[..300]);
        assert!(p.contains(&format!("\"shown\":{}", MAX_RECENT)));
        assert!(recent_bytes(&day) <= crate::snapshots::DAILY_RECENT_MAX);
        assert!(day.contains("\"dropped\":[]"));

        // 最悪の値の行は 1 MiB で切る (停止の 1 枚のように 4 MiB までは持たない)
        let m = Metrics::new();
        fill(&m, true);
        let day = with_ep(&m, |ep| {
            build_snapshot(ep, daily, false, crate::snapshots::DAILY_MAX_FILE as usize).2
        });
        let p = part_of(&day);
        assert!(p.contains("\"truncated\":true"), "{}", &p[..300]);
        assert!(recent_bytes(&day) <= crate::snapshots::DAILY_RECENT_MAX);
        assert!(recent_bytes(&day) > MAX_BODY, "256 KiB より多く持つ");
    }

    /// 停止のスナップショット (`recent_max` = [`MAX_SNAPSHOT`]) は `recent` の 2,000 件を切らずに全部入れ、
    /// 部ごとに切る形 (T39.8 より前のいつもの `/snapshot`) は 256 KiB で切る (T28.20)。行は最悪の値で埋める。
    #[test]
    fn the_shutdown_snapshot_keeps_every_recent_row() {
        let m = Metrics::new();
        fill(&m, true);
        let (usual, full, took) = with_ep(&m, |ep| {
            let usual = cut_snapshot(ep);
            let full = build_snapshot(ep, Some(MAX_SNAPSHOT), false, MAX_SNAPSHOT).2;
            let t = Instant::now();
            let _ = build_snapshot(ep, Some(MAX_SNAPSHOT), false, MAX_SNAPSHOT);
            (usual, full, t.elapsed())
        });
        println!(
            "T28.20 recent 2,000 件 (最悪の値): いつものスナップショット {} B / 停止のスナップショット {} B、停止の 1 枚を組むのに {:.1} ms (debug ビルド)",
            usual.len(),
            full.len(),
            took.as_secs_f64() * 1000.0
        );
        assert!(part_of(&usual).contains("\"truncated\":true"));
        let p = part_of(&full);
        assert!(p.contains("\"truncated\":false"), "{}", &p[..300]);
        assert!(p.contains(&format!("\"shown\":{}", MAX_RECENT)));
        assert!(full.len() <= MAX_SNAPSHOT, "{} B", full.len());
        assert!(
            full.contains("\"dropped\":[]"),
            "全部入れても何も落とさない"
        );

        // 全部入れて上限を越えるなら、切った `recent` に戻す (部ごと `null` にはしない)
        let mut part = vec![("status", "s".repeat(100)), ("recent", "r".repeat(1000))];
        let cut = |_: &str| "cut".to_string();
        assert!(fit_whole_parts(&mut part, 4096, &["recent"], cut).is_empty());
        assert_eq!(part[1].1.len(), 1000, "収まるなら全部のまま");
        assert_eq!(
            fit_whole_parts(&mut part, 1000, &["recent"], cut),
            ["recent"]
        );
        assert_eq!(part[1], ("recent", "cut".to_string()));
    }

    /// **T29.16**: 日次の 1 枚だけ上限は 6 MiB。0.28.48 の日次 `2026-10-26` の大きさ (全体 3,692,328 B、
    /// `history` 1,550,971 B・`recent` 896,914 B) に `history` が 60 万 B 増えて 4 MiB を越えても、日次なら
    /// `recent` の 2,000 行を切らずに持つ (4 MiB の上限では 256 KiB の `recent` に黙って戻っていた)。
    #[test]
    fn the_daily_snapshot_has_room_above_four_mib() {
        let daily = crate::snapshots::DAILY_MAX_FILE as usize;
        assert_eq!(daily, 6 * 1024 * 1024);
        let parts = || {
            vec![
                ("history.60", "h".repeat(1_550_971 + 600_000)),
                ("hosts", "o".repeat(3_692_328 - 1_550_971 - 896_914)),
                ("recent", "r".repeat(896_914)),
            ]
        };
        let cut = |_: &str| "cut".to_string();
        let mut four = parts();
        assert_eq!(
            fit_whole_parts(&mut four, 4 * 1024 * 1024, &["recent"], cut),
            ["recent"]
        );
        let mut six = parts();
        assert!(fit_whole_parts(&mut six, daily, &["recent"], cut).is_empty());
        assert_eq!(six[2].1.len(), 896_914, "recent は全部のまま");
        assert!(drop_to_fit(&mut six, daily).is_empty());
        let total: usize = overhead_of(&six) + six.iter().map(|(_, v)| v.len()).sum::<usize>();
        // 実際の 3,692,328 B は 6 MiB の 58.7% (受け入れ基準の 70% 以下)、60 万 B 増えても 68.2%
        assert!(total * 100 <= daily * 70, "{} B", total);
        assert!(3_692_328 * 100 <= daily * 70);
    }

    /// `recent`・`dns`・`hosts`・`profile` の 4 つの部を、どれも 256 KiB を越える中身で埋める (T38.7)。
    ///
    /// `dns` の表はプロセスに 1 つ (大域) なので、ほかの試験と混ざらない名前で置く。
    fn fill_four_parts(m: &Metrics) {
        use crate::metrics::{Detail, HostOutcome, MAX_HOSTS};
        use crate::profile::{ROLES, RoleWindow, Sample};
        use std::time::Duration;

        fill(m, false);
        for i in 0..MAX_HOSTS {
            m.record_host_detail(
                &format!("connect://host{:04}.t38-7.example.net:443", i),
                HostOutcome::Bypass,
                98_765_432,
                Some(Duration::from_millis(12)),
                Detail {
                    dns_ms: 3,
                    connect_ms: 9,
                    family_v6: Some(false),
                    ..Detail::default()
                },
            );
        }
        let v4: std::net::IpAddr = "93.184.216.34".parse().unwrap();
        let v6: std::net::IpAddr = "2001:db8:85a3:8d3:1319:8a2e:370:7348".parse().unwrap();
        for i in 0..1_200 {
            crate::dns::put_answer_for_test(
                &format!("name{:04}.t38-7-snapshot.example.com", i),
                &[v4, v6],
                Duration::from_secs(30),
            );
        }
        let role = RoleWindow {
            cpu_us: 123_456,
            user_us: 98_765,
            samples: 1_000,
            states: [99; crate::profile::NSTATES],
        };
        for i in 0..crate::profile::RESOLUTIONS[0].1 as u64 {
            m.profile.push(Sample {
                t: 1_700_000_000 + i * 5,
                requests: 1_234,
                cpu_us: 456_789,
                threads: [role; ROLES.len()],
                ..Sample::default()
            });
        }
    }

    /// スナップショットの部 `name` の中身 (次の部 `next` の手前まで)。
    fn section<'a>(body: &'a str, name: &str, next: &str) -> &'a str {
        let from = body
            .find(&format!(",\"{}\":{{\"schema\":", name))
            .unwrap_or_else(|| panic!("{} の部が無い", name));
        let rest = &body[from..];
        let to = rest
            .find(&format!(",\"{}\":", next))
            .unwrap_or_else(|| panic!("{} の部が無い", next));
        &rest[..to]
    }

    /// 部の末尾の `truncated` (行の中ではなく、その口の応答の最後の欄)。
    fn cut_of(part: &str) -> bool {
        let at = part.rfind("\"truncated\":").expect("truncated が無い");
        part[at..].starts_with("\"truncated\":true")
    }

    /// 4 つの部の名前と、その次の部の名前 (部の並びは [`collect_parts`] の順)。
    const FOUR: [(&str, &str); 4] = [
        ("dns", "errors"),
        ("recent", "hosts"),
        ("hosts", "hosts_series"),
        ("profile", "events"),
    ];

    /// **T38.7 の受け入れ基準**: 止めるときの 1 枚 (ディスクに書く 1 枚) は `recent`・`dns`・`hosts`・`profile` の
    /// 4 つの部を `truncated` にせず全部入れ、全体の上限は 6 MiB。**T39.8 からいつもの `/snapshot` も同じ**
    /// (前は 4 つとも 256 KiB で切っていた)。
    #[test]
    fn the_shutdown_snapshot_keeps_the_four_parts_whole() {
        let daily = crate::snapshots::DAILY_MAX_FILE as usize;
        let m = Metrics::new();
        fill_four_parts(&m);
        let (usual, disk, cut) = with_ep(&m, |ep| {
            (
                snapshot(ep).2,
                disk_snapshot(ep, Some(daily), daily).2,
                cut_snapshot(ep),
            )
        });
        let sizes: Vec<String> = FOUR
            .iter()
            .map(|(name, next)| format!("{} {} B", name, section(&disk, name, next).len()))
            .collect();
        println!(
            "T38.7 4 つの部を埋めた 1 枚: いつもの /snapshot {} B / 止めるときの 1 枚 {} B ({})",
            usual.len(),
            disk.len(),
            sizes.join("、")
        );
        assert!(disk.contains(&format!("\"limit_bytes\":{}", daily)));
        assert!(disk.contains("\"dropped\":[]"), "{}", &disk[..300]);
        assert!(disk.len() <= daily, "{} B", disk.len());
        for (name, next) in FOUR {
            let whole = section(&disk, name, next);
            assert!(!cut_of(whole), "{} が切れている", name);
            assert!(whole.len() > MAX_BODY, "{} は 256 KiB を越えて入る", name);
            assert!(
                !cut_of(section(&usual, name, next)),
                "いつもの /snapshot の {} も切らない (T39.8)",
                name
            );
            assert!(
                cut_of(section(&cut, name, next)),
                "前の形の {} は切る",
                name
            );
        }
        assert!(usual.len() <= MAX_SNAPSHOT, "{} B", usual.len());
        assert_eq!(
            MAX_SNAPSHOT, daily,
            "/snapshot の上限はディスクの 1 枚と同じ (T39.8)"
        );
        assert!(usual.contains(&format!("\"limit_bytes\":{}", daily)));
        assert!(usual.contains("\"dropped\":[]"), "{}", &usual[..300]);
    }

    /// 全部入れた 4 つの部で全体の上限を越えるなら、`profile` → `hosts` → `dns` → `recent` の順に
    /// 1 つずつ切った形へ戻し、収まったらそこで止める (部ごと `null` にはしない。T38.7)。
    #[test]
    fn the_disk_snapshot_cuts_whole_parts_back_before_dropping() {
        let daily = crate::snapshots::DAILY_MAX_FILE as usize;
        let m = Metrics::new();
        fill_four_parts(&m);
        // 上限を、全部入れた 1 枚より 100,000 B 小さくする (`profile` を戻せば約 37 万 B 減るので収まり、
        // ほかの 3 つは全部のまま)。前は 1 B だけ小さくしていたが、名前解決の表はプロセスで 1 つなので、
        // 並んで走るほかの試験が 2 回組むあいだに表を変えると 2 回目が数 B 縮み、`profile` を戻さずに
        // 収まって CI で落ちた (T39.8)
        let (full, tight) = with_ep(&m, |ep| {
            let full = disk_snapshot(ep, Some(daily), daily).2;
            let limit = full.len() - 100_000;
            (full.len(), disk_snapshot(ep, Some(daily), limit).2)
        });
        assert!(tight.len() < full);
        assert!(tight.contains("\"dropped\":[]"), "{}", &tight[..300]);
        assert!(
            cut_of(section(&tight, "profile", "events")),
            "profile は切った形"
        );
        for (name, next) in &FOUR[..3] {
            assert!(
                !cut_of(section(&tight, name, next)),
                "{} は全部のまま",
                name
            );
        }

        // 作り物の部 (6 MiB を越える): 戻す順と、戻したぶんで収まれば何も落とさないこと
        let parts = || {
            vec![
                ("history.60", "h".repeat(2_700_000)),
                ("dns", "d".repeat(1_500_000)),
                ("recent", "r".repeat(1_000_000)),
                ("hosts", "o".repeat(1_000_000)),
                ("profile", "p".repeat(1_000_000)),
                ("log", "l".repeat(200_000)),
            ]
        };
        let opened = ["recent", "dns", "hosts", "profile"];
        let cut = |name: &str| format!("cut-{}", name);
        let mut part = parts();
        assert_eq!(
            fit_whole_parts(&mut part, daily, &opened, cut),
            ["profile", "hosts"]
        );
        assert_eq!(part[4].1, "cut-profile");
        assert_eq!(part[3].1, "cut-hosts");
        assert_eq!(part[1].1.len(), 1_500_000, "dns は全部のまま");
        assert_eq!(part[2].1.len(), 1_000_000, "recent は全部のまま");
        assert!(drop_to_fit(&mut part, daily).is_empty());
        // 4 つとも戻しても越えるなら、そのあとでいつもの順に落とす
        let mut part = parts();
        assert_eq!(
            fit_whole_parts(&mut part, 2_000_000, &opened, cut),
            ["profile", "hosts", "dns", "recent"]
        );
        assert_eq!(drop_to_fit(&mut part, 2_000_000), ["recent", "log"]);
        // 部の上限を外していない部は戻さない (いつもの `/snapshot` の形の `recent_max` だけ)
        let mut part = parts();
        assert_eq!(
            fit_whole_parts(&mut part, daily, &["recent"], cut),
            ["recent"]
        );
        assert_eq!(part[4].1.len(), 1_000_000);
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

    /// 偽の部を組むと、頭・`parts`・`dropped` と、`history.*` の入れ子がそのまま出る (T34.91)。
    #[test]
    fn the_snapshot_renders_parts_and_nests_history() {
        let part = vec![
            ("status", "{\"s\":1}".to_string()),
            ("history.5", "[5]".to_string()),
            ("history.60", "[60]".to_string()),
            ("recent", "null".to_string()),
            ("log", "{\"l\":2}".to_string()),
        ];
        let out = render_snapshot("1.2.\"3\"", 42, 1_700_000_000, 4096, &part, &["recent"]);
        assert_eq!(
            out,
            concat!(
                "{\"schema\":1,\"taken_at\":1700000000,\"version\":\"1.2.\\\"3\\\"\",",
                "\"uptime_secs\":42,\"limit_bytes\":4096,",
                "\"parts\":[\"status\",\"history.5\",\"history.60\",\"recent\",\"log\"],",
                "\"dropped\":[\"recent\"],",
                "\"status\":{\"s\":1},\"history\":{\"5\":[5],\"60\":[60]},",
                "\"recent\":null,\"log\":{\"l\":2}}"
            )
        );
    }

    /// `history.*` が最後の部でも入れ子を閉じ、何も落とさなければ `dropped` は空 (T34.91)。
    #[test]
    fn the_snapshot_closes_a_trailing_history() {
        let part = vec![
            ("history.3600", "[1]".to_string()),
            ("dns", "{}".to_string()),
            ("history.5", "null".to_string()),
        ];
        let out = render_snapshot("v", 0, 0, 1, &part, &[]);
        assert!(
            out.ends_with(
                "\"dropped\":[],\"history\":{\"3600\":[1]},\"dns\":{},\"history\":{\"5\":null}}"
            ),
            "{}",
            out
        );
        // 部が 1 つも無くても形は崩れない
        let empty = render_snapshot("v", 0, 0, 1, &[], &[]);
        assert!(empty.ends_with("\"parts\":[],\"dropped\":[]}"), "{}", empty);
    }
}
