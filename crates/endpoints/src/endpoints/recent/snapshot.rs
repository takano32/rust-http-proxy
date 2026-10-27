//! 1 回で全部の `/snapshot` と、残してある雪像の一覧・1 枚 (`/snapshots`。T14.34・T.28.20)。

use super::super::{history_body, profile, status_body};
use super::*;

/// `/snapshots` — 日次で残した `/snapshot` の一覧 (T14.34)。
///
/// 履歴スレッドが UTC の日付をまたいだ瞬間に `$HOME/.rust-http-proxy/snapshots/<日付>.json`
/// へ 1 ファイル書いている (既定 30 日ぶん、`PROXY_SNAPSHOT_DAYS`)。ここはその置き場所と
/// 日付・大きさを並べるだけで、中身を読むのは [`snapshot_file`] (`/snapshots/<date>`)。
/// `PROXY_STATS_PERSIST=off` と `PROXY_SNAPSHOT_DAYS=0` では 1 つも書いていないので
/// `dir` が `null` で `files` は空。
pub fn snapshots() -> (u16, &'static str, String) {
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
    // 起動と停止の 1 枚 (T.28.20) は別の並びにする (日次の `files` を読む道具が、日付で
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
        ",\"count\":{},\"shown\":{},\"bytes\":{},\"days\":{},\"max_bytes\":{},\"dir\":{},\"truncated\":{},\"moments_count\":{},\"moments_shown\":{},\"moments_bytes\":{},\"moments_truncated\":{}}}",
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
        moments_cut
    );
    (200, "application/json", out)
}

/// `/snapshots/<YYYY-MM-DD>` — 残してある 1 日ぶんを**そのまま**返す (T14.34)。
/// `/snapshots/<YYYY-MM-DDTHHMMSSZ>-start|shutdown` (起動と停止の 1 枚。T.28.20) も同じ口。
///
/// 中身は `/snapshot` の応答そのもの (組み直さない) なので、`scripts/snapshot-diff.py` や
/// `scripts/collect-deployed.sh --from-server` がそのまま読める。**版 (`schema`) も
/// 書いた時のまま**で、読む道具は版の無い古いファイルも今までどおり読める (T14.49)。日付として読めない名前と
/// 置いていない日は 404 (`..` を書かれても置き場所の外は見ない)。
pub fn snapshot_file(date: &str) -> (u16, &'static str, String) {
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
/// `connections`、`recent`、`hosts`、`clients` (T14.7)、`bursts` (T14.6)、
/// `profile` (T14.3)、`log`。
/// **`history.*` にはカーネルと cgroup の窓 (`kernel`) も一緒に入る** (T14.12。`/history` と同じ組み立て)。
/// **T14.3 の `/profile` は、入ったらここに 1 行足す** (`parts` に名前が出るので、
/// 読む側は「この版に何が入っていたか」を JSON だけで判別できる)。
pub fn snapshot(ep: &Endpoint<'_>) -> (u16, &'static str, String) {
    snapshot_with(ep, None)
}

/// [`snapshot`] の本体。`recent_max` が `Some(上限)` なら **`recent` を部の 256 KiB ([`MAX_BODY`]) で
/// 切らずに、その上限まで入れる** (停止の雪像は [`MAX_SNAPSHOT`] = 2,000 件を全部。T.28.20。
/// 日次の雪像は `snapshots::DAILY_RECENT_MAX` = 1 MiB。T.28.46)。`None` はいつもの `/snapshot`。
///
/// 日次は部ごと 256 KiB で切っていたので `/recent` が約 580 行しか入らず、締めの「前」に日次を
/// 使うと接続のエラーの行が「判定できず」になった (T.28.18)。1 MiB なら 1 件 400〜500 B の
/// 行で環の 2,000 件が入る。
///
/// 締めの「前」は `/recent` の窓が判定の分母 (phase25 の接続のエラーは試行 1,000 本が下限) になるのに、
/// `collect-deployed.sh` が `offset=` で続きを追って繋ぐ形 (T.22.5) は、`--from-server` で取り寄せた
/// 1 枚にはできない (取り寄せたときにはもうプロセスが居ない)。全部入れても 2,000 件 × 1 件 500 B
/// 前後 = 1 MB 前後で、合計が 4 MiB を越えるときは**切った `recent` に戻してから**いつもの順に落とす
/// (全部入れたせいで `recent` ごと `null` にはしない)。
pub fn snapshot_with(ep: &Endpoint<'_>, recent_max: Option<usize>) -> (u16, &'static str, String) {
    use crate::history::History;
    use crate::metrics::HostSort;

    // 個票はどれも 256 KiB 以下、`/status` は 64 KiB 以下、履歴は 3 本で最大 2 MiB。
    // 先に全部組んでから、合計が 4 MiB を越えていたら順に落とす
    // `status` の 3 通りは**今までどおり `hosts[]` も入れる** (軽い形は `/status?hosts=0`
    // だけの話で、`/snapshot` の `parts` は変えない。T25.1 段 2)
    let mut part: Vec<(&'static str, String)> = vec![
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
        ("dns", dns(Some("sort=age&limit=4096")).2),
        ("errors", errors(ep, Some("n=500")).2),
        ("connections", connections(ep).2),
        (
            "recent",
            match recent_max {
                Some(max) => recent_within(ep, Some("n=2000"), max.min(MAX_SNAPSHOT)).2,
                None => recent(ep, Some("n=2000")).2,
            },
        ),
        ("hosts", hosts(ep, Some("limit=1000")).2),
        // ホスト別の時系列 (上位 16 × 5 分 × 24 時間。T14.22)
        ("hosts_series", host_series(ep, Some("top=16")).2),
        ("clients", clients(ep, Some("limit=1000")).2),
        ("bursts", bursts(ep, Some("n=50")).2),
        // 待ちの段階・スレッドの CPU と状態・ロックの取り合い (T14.3)。
        // `--lite` では `{"profile":"off"}` の 1 行になる
        ("profile", profile::profile(ep, Some("res=5")).2),
        ("events", events(ep, Some("n=512")).2),
        ("log", log(ep, Some("n=1000")).2),
    ];
    let names: Vec<&'static str> = part.iter().map(|(k, _)| *k).collect();
    if recent_max.is_some() {
        fit_full_recent(&mut part, MAX_SNAPSHOT, || recent(ep, Some("n=2000")).2);
    }
    let dropped = drop_to_fit(&mut part, MAX_SNAPSHOT);
    let total: usize = overhead_of(&part) + part.iter().map(|(_, v)| v.len()).sum::<usize>();

    let mut out = String::with_capacity(total + 1024);
    // 応答の形の版は**いちばん先頭の鍵** (T14.49)。**各部にもそれぞれ入っている**
    // (部はどれもその口の出力そのものなので、切り出して 1 本の応答として読める)
    out.push_str(SCHEMA_HEAD);
    let _ = write!(
        out,
        "\"taken_at\":{},\"version\":\"{}\",\"uptime_secs\":{},\"limit_bytes\":{},\"parts\":[",
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

/// 全部入れた `recent` で合計が `limit` を越えるなら、`cut` (いつもの 256 KiB で切った `recent`) に
/// 置き換える (T.28.20)。置き換えたかを返す。
fn fit_full_recent(
    part: &mut [(&'static str, String)],
    limit: usize,
    cut: impl FnOnce() -> String,
) -> bool {
    let total: usize = overhead_of(part) + part.iter().map(|(_, v)| v.len()).sum::<usize>();
    if total <= limit {
        return false;
    }
    match part.iter_mut().find(|(k, _)| *k == "recent") {
        Some((_, body)) => {
            *body = cut();
            true
        }
        None => false,
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

    fn with_ep<R>(m: &Metrics, f: impl FnOnce(&Endpoint<'_>) -> R) -> R {
        let cache = crate::cache::Cache::new(crate::cache::CacheConfig::disabled());
        let concurrency = || crate::metrics::Concurrency {
            max_conns: 0,
            max_threads: 0,
            live_threads: 0,
            idle_threads: 0,
            queued_jobs: 0,
        };
        let ep = Endpoint {
            metrics: m,
            cache: &cache,
            conn_id: 1,
            port: 8080,
            host: None,
            client: None,
            pac_direct: &[],
            lite: false,
            readonly: false,
            version: "test",
            build: "test",
            concurrency: &concurrency,
        };
        f(&ep)
    }

    /// 雪像の `recent` の部 (その部の閉じ括弧までではなく、部の始まりから最後まで)。
    fn part_of(body: &str) -> &str {
        let at = body
            .find(",\"recent\":{\"schema\":")
            .expect("recent の部が無い");
        &body[at..]
    }

    /// 雪像の `recent` の部の大きさ (`"hosts"` の部の手前まで)。
    fn recent_bytes(body: &str) -> usize {
        let p = part_of(body);
        p.find(",\"hosts\":").expect("hosts の部が無い")
    }

    /// **T.28.46 の受け入れ基準**: 日次の 1 枚 (`snapshot_with(.., Some(DAILY_RECENT_MAX))`) は
    /// `recent` を 1 MiB まで持ち、ありがちな行なら**環の 2,000 件が全部入る**
    /// (いつもの `/snapshot` は 256 KiB で切って 2,000 件を割る)。最悪の値の行は 1 MiB で切る。
    #[test]
    fn the_daily_snapshot_keeps_the_whole_ring_within_one_mib() {
        let daily = Some(crate::snapshots::DAILY_RECENT_MAX);
        assert_eq!(crate::snapshots::DAILY_RECENT_MAX, 1024 * 1024);
        let m = Metrics::new();
        fill(&m, false);
        let (usual, day) = with_ep(&m, |ep| (snapshot(ep).2, snapshot_with(ep, daily).2));
        println!(
            "T.28.46 recent 2,000 件 (ありがちな行): いつもの雪像 {} B (recent {} B) / 日次 {} B (recent {} B)",
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
        let day = with_ep(&m, |ep| snapshot_with(ep, daily).2);
        let p = part_of(&day);
        assert!(p.contains("\"truncated\":true"), "{}", &p[..300]);
        assert!(recent_bytes(&day) <= crate::snapshots::DAILY_RECENT_MAX);
        assert!(recent_bytes(&day) > MAX_BODY, "256 KiB より多く持つ");
    }

    /// 停止の雪像 (`snapshot_with(.., Some(MAX_SNAPSHOT))`) は `recent` の 2,000 件を切らずに全部入れ、
    /// いつもの `/snapshot` は今までどおり 256 KiB で切る (T.28.20)。行は最悪の値で埋める。
    #[test]
    fn the_shutdown_snapshot_keeps_every_recent_row() {
        let m = Metrics::new();
        fill(&m, true);
        let (usual, full, took) = with_ep(&m, |ep| {
            let usual = snapshot(ep).2;
            let full = snapshot_with(ep, Some(MAX_SNAPSHOT)).2;
            let t = Instant::now();
            let _ = snapshot_with(ep, Some(MAX_SNAPSHOT));
            (usual, full, t.elapsed())
        });
        println!(
            "T.28.20 recent 2,000 件 (最悪の値): いつもの雪像 {} B / 停止の雪像 {} B、停止の 1 枚を組むのに {:.1} ms (debug ビルド)",
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

        // 全部入れて 4 MiB を越えるなら、切った `recent` に戻す (部ごと `null` にはしない)
        let mut part = vec![("status", "s".repeat(100)), ("recent", "r".repeat(1000))];
        assert!(!fit_full_recent(&mut part, 4096, || "cut".to_string()));
        assert_eq!(part[1].1.len(), 1000, "収まるなら全部のまま");
        assert!(fit_full_recent(&mut part, 1000, || "cut".to_string()));
        assert_eq!(part[1], ("recent", "cut".to_string()));
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
}
