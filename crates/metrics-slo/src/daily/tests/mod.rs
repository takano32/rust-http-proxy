//! `daily` の単体試験。ファイルの扱いの試験は [`file`] に割った (T.34.46)。

mod file;

use std::io::Write as _;

use super::acc::{Acc, Cum};
use super::file::{append, field, is_line, last_day, newest_within, read_lines};
use super::versions::{clean_version, versions_of_day};
use super::*;
use crate::history::Window;

const DAY: u64 = SECS_PER_DAY;

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "rhp-t1420-{}-{}-{:?}.jsonl",
        name,
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_file(&p);
    p
}

/// `t` 秒の標本 1 本 (要求は 5 秒で 10 本、CONNECT を 1 本確立した形)。
fn sample(t: u64, requests: u64) -> Sample {
    let mut connect = Window::default();
    connect.observe(9);
    Sample {
        t,
        requests,
        bytes: requests * 1024,
        active: 3,
        active_max: 3,
        rss: 50 * 1024 * 1024,
        connect,
        errors: 1,
        dns_misses: 2,
        dns_ms_sum: 30,
        ..Sample::default()
    }
}

fn lines(path: &Path) -> Vec<String> {
    read_lines(path)
}

#[test]
fn a_line_is_written_when_the_utc_day_changes() {
    let path = tmp("rollover");
    let mut w = Writer::new(Some(path.clone()), "0.1.0+test");
    // 1 日目 (day 20000) の 4 本
    let base = 20_000 * DAY;
    for i in 0..4 {
        w.add(&sample(base + 86_000 + i * 5, 100 + i * 10), 0);
    }
    assert!(lines(&path).is_empty(), "日が変わるまでは 1 行も書かない");
    // 日が変わった最初の標本で 1 行
    w.add(&sample(base + DAY + 5, 200), 2);
    let rows = lines(&path);
    assert_eq!(rows.len(), 1, "{:?}", rows);
    assert_eq!(field(&rows[0], "day"), Some("2024-10-04"));
    assert_eq!(
        field(&rows[0], "t"),
        Some((20_000 * DAY).to_string().as_str())
    );
    // 累計の差 (100 → 200) と、区間の足し合わせ (5 本 × errors 1)
    assert_eq!(field(&rows[0], "requests"), Some("100"));
    assert_eq!(field(&rows[0], "errors"), Some("5"));
    assert_eq!(field(&rows[0], "connects"), Some("5"));
    assert_eq!(field(&rows[0], "dns_misses"), Some("10"));
    assert_eq!(field(&rows[0], "bursts"), Some("2"));
    assert_eq!(field(&rows[0], "samples"), Some("5"));
    assert_eq!(field(&rows[0], "version"), Some("0.1.0+test"));
    assert!(rows[0].len() <= MAX_LINE, "{} B", rows[0].len());
    // 次の日も同じように 1 行増える (2 日目 → 3 日目)
    w.add(&sample(base + DAY + 10, 210), 2);
    w.add(&sample(base + 2 * DAY, 300), 2);
    assert_eq!(lines(&path).len(), 2);
    let _ = fs::remove_file(&path);
}

/// T.32.17: 名前解決に失敗した試みのミスを除いた率 (`dns_resolved_per_connect`) を末尾の欄に書く。
/// `dns_per_connect` は今までどおり失敗も入れた値 (読む側の互換)。
#[test]
fn a_line_carries_the_miss_rate_without_failed_lookups() {
    let path = tmp("resolved");
    let mut w = Writer::new(Some(path.clone()), "0.1.0+test");
    let base = 20_400 * DAY;
    for i in 0..4 {
        // 1 本に CONNECT 1 本・ミス 2 回 (うち失敗 1 回は 1 本おき)
        let mut s = sample(base + 100 + i * 5, 10);
        s.dns_fail_misses = (i % 2 == 0) as u64;
        w.add(&s, 0);
    }
    w.add(&sample(base + DAY + 5, 20), 0);
    let rows = lines(&path);
    assert_eq!(rows.len(), 1, "{:?}", rows);
    // 5 本 (境目の 1 本も終わる日に足す): ミス 10・失敗 2・CONNECT 5
    assert_eq!(field(&rows[0], "dns_per_connect"), Some("2.000"));
    assert_eq!(field(&rows[0], "dns_fail_misses"), Some("2"));
    assert_eq!(field(&rows[0], "dns_resolved_per_connect"), Some("1.600"));
    let _ = fs::remove_file(&path);
}

/// `version <v> on port …` の `start` 1 件 (T.32.18)。
fn start_event(at: u64, v: &str) -> crate::events::Event {
    crate::events::Event {
        at,
        kind: crate::events::EventKind::Start,
        text: format!("version {} on port 8080 boot x", v),
    }
}

/// プロセス 1 本ぶん: `from`〜`until` (5 秒ごと) の標本を `h` に積みながら `w` に足す (本番と同じ順:
/// `daily` が先、`push` が後)。要求は 5 秒に 10 本 (`requests` は**このプロセスの通算**で 0 から)、
/// CONNECT 1 本・ミス 2 回 (うち失敗 1 回)。返すのはこのプロセスが通した要求の数
fn run_process(
    h: &crate::history::History,
    w: &mut Writer,
    from: u64,
    until: u64,
    events: &[crate::events::Event],
) -> u64 {
    let mut total = 0;
    let mut t = from;
    while t <= until {
        let mut s = sample(t, total);
        // 最初の 1 本は起点 (区間の値は 0。`tick.rs` の `totals` が `None` の 1 本目と同じ)
        s.requests_delta = if t == from { 0 } else { 10 };
        s.bytes_delta = s.requests_delta * 1024;
        s.dns_fail_misses = 1;
        s.gauge_n = 1;
        if !w.seeded() {
            w.seed(h, &s, 0, events);
        }
        w.add(&s, 0);
        h.push(s);
        t += 5;
        total += 10;
    }
    // 最後の 1 本の `requests` が通算 (= 足した `requests_delta` の和)
    total - 10
}

/// T.32.18 の受け入れ基準: 日の途中で再起動しても、その日の `secs` が 86,400 前後になり、要求の和が
/// 前後のプロセスの和と一致する。前のプロセスのぶんは `.rrd` から読み戻した `history["60"]`
/// (ここでは前の `History` の 60 秒の行をそのまま `restore`) から組み直す。`version` はその日に動いた版の並び。
#[test]
fn a_restart_in_the_middle_of_the_day_keeps_the_earlier_process() {
    let path = tmp("restart");
    let day = 20_500 * DAY;
    // 前のプロセス (0.1.0+aaa): 前の日の 23:00 から 12:00:00 まで。11:59 の行は 12:00:00 の標本で閉じる
    let h1 = crate::history::History::default();
    let mut w1 = Writer::new(Some(path.clone()), "0.1.0+aaa");
    let ev1 = [start_event(day - 3600, "0.1.0+aaa")];
    run_process(&h1, &mut w1, day - 3600, day + 12 * 3600, &ev1);
    assert_eq!(lines(&path).len(), 1, "前の日の行は前のプロセスが書いた");
    // 0 時からの 12 時間ぶんの要求 (12:00:00 の標本は窓が閉じる前に止まったので戻らない)
    let before: u64 = 12 * 3600 / 5 * 10;
    // 次のプロセス (0.1.0+bbb): 12:00:30 に起動。`.rrd` から 60 秒の行を読み戻す
    let h2 = crate::history::History::default();
    let mut rows = Vec::new();
    h1.for_each_in(SEED_RES, 0, u64::MAX, |s| rows.push(*s));
    h2.restore(SEED_RES, rows);
    let mut w2 = Writer::new(Some(path.clone()), "0.1.0+bbb");
    let ev2 = [
        ev1[0].clone(),
        start_event(day + 12 * 3600 + 30, "0.1.0+bbb"),
    ];
    // 翌日 0:00:00 の標本 (日をまたいだ最初の 1 本) は終わる日に足して、そこで 1 行書く
    let after = run_process(&h2, &mut w2, day + 12 * 3600 + 30, day + DAY, &ev2);
    let rows = lines(&path);
    assert_eq!(rows.len(), 2, "{:?}", rows);
    let today = &rows[1];
    let secs: u64 = field(today, "secs").unwrap().parse().unwrap();
    assert!(
        (86_000..=86_400).contains(&secs),
        "secs {}: {}",
        secs,
        today
    );
    let requests: u64 = field(today, "requests").unwrap().parse().unwrap();
    assert_eq!(requests, before + after, "{}", today);
    assert_eq!(field(today, "version"), Some("0.1.0+aaa 0.1.0+bbb"));
    // 区間の値も前のプロセスのぶんが入る (CONNECT は 5 秒に 1 本)
    let connects: u64 = field(today, "connects").unwrap().parse().unwrap();
    assert!(connects > 17_000, "{}", today);
    assert_eq!(field(today, "dns_resolved_per_connect"), Some("1.000"));
    assert!(today.len() <= MAX_LINE);
    // 組み直すのは起動して最初の 1 回だけ
    assert!(w2.seeded());
    let _ = fs::remove_file(&path);
}

/// 読み戻した行が無ければ (`.rrd` が無い・前の日のぶんしか無い) 今までどおり今のプロセスのぶんだけ。
#[test]
fn without_restored_rows_the_day_is_this_process_only() {
    let path = tmp("norows");
    let day = 20_600 * DAY;
    let h = crate::history::History::default();
    let mut w = Writer::new(Some(path.clone()), "0.1.0+ccc");
    let ev = [
        start_event(day - 50, "0.1.0+old"),
        start_event(day + 600, "0.1.0+ccc"),
    ];
    run_process(&h, &mut w, day + 600, day + DAY + 5, &ev);
    let rows = lines(&path);
    assert_eq!(rows.len(), 1);
    assert_eq!(field(&rows[0], "version"), Some("0.1.0+ccc"));
    let secs: u64 = field(&rows[0], "secs").unwrap().parse().unwrap();
    assert_eq!(
        secs,
        DAY - 600,
        "日をまたいだ最初の標本 (翌日 0:00:00) までの秒"
    );
    let _ = fs::remove_file(&path);
}

/// 版の並び: 0 時に動いていた版 + 0 時からの `start` + 今の版 (続けて同じ版は 1 つ)。長すぎれば古い方から落とす。
#[test]
fn the_versions_of_the_day_are_listed_oldest_first() {
    let day = 20_700 * DAY;
    let ev = [
        start_event(day - 7200, "0.1.0+old"),
        start_event(day - 100, "0.1.0+a"),
        start_event(day + 100, "0.1.0+b"),
        start_event(day + 200, "0.1.0+b"),
        crate::events::Event {
            at: day + 300,
            kind: crate::events::EventKind::Start,
            text: "self-bench: 3 runs".to_string(),
        },
        start_event(day + 400, "0.1.0+c"),
    ];
    assert_eq!(
        versions_of_day(&ev, day, day + 500, true, "0.1.0+c"),
        "0.1.0+a 0.1.0+b 0.1.0+c"
    );
    assert_eq!(
        versions_of_day(&ev, day, day + 500, false, "0.1.0+c"),
        "0.1.0+b 0.1.0+c"
    );
    let many: Vec<_> = (0..20)
        .map(|i| start_event(day + i, &format!("0.{}.0+abcdef0", i)))
        .collect();
    let v = versions_of_day(&many, day, day + 100, false, "9.9.9+fffffff");
    assert!(v.len() <= MAX_VERSIONS, "{}", v);
    assert!(v.ends_with("0.19.0+abcdef0 9.9.9+fffffff"), "{}", v);
}

#[test]
fn starting_twice_on_the_same_day_keeps_one_line() {
    let path = tmp("twice");
    let base = 20_100 * DAY;
    let mut first = Writer::new(Some(path.clone()), "0.1.0+test");
    first.add(&sample(base + 100, 10), 0);
    first.add(&sample(base + DAY + 100, 20), 0);
    assert_eq!(lines(&path).len(), 1);
    // 同じ日をもう一度またぐ起動 (最後の行の日付を見て書かない)
    let mut second = Writer::new(Some(path.clone()), "0.1.0+test");
    second.add(&sample(base + 200, 10), 0);
    second.add(&sample(base + DAY + 200, 40), 0);
    let rows = lines(&path);
    assert_eq!(rows.len(), 1, "{:?}", rows);
    assert_eq!(second.counts(), (0, 0));
    // その次の日はちゃんと増える
    second.add(&sample(base + 2 * DAY, 60), 0);
    assert_eq!(lines(&path).len(), 2);
    let _ = fs::remove_file(&path);
}

#[test]
fn nothing_is_written_without_a_path() {
    let mut w = Writer::new(None, "0.1.0+test");
    let base = 20_200 * DAY;
    w.add(&sample(base, 1), 0);
    w.add(&sample(base + DAY, 2), 0);
    assert_eq!(w.counts(), (0, 0));
}

/// 1 行は必ず [`MAX_LINE`] 以下 (現実離れした値でも)。
#[test]
fn a_line_never_exceeds_the_limit() {
    let mut connect = Window::default();
    for _ in 0..3 {
        connect.observe(u64::MAX / 4);
    }
    let acc = Acc {
        day: 20_000,
        first_t: 0,
        last_t: u64::MAX,
        samples: u64::MAX,
        start: Cum::default(),
        end: Cum {
            requests: u64::MAX,
            bytes: u64::MAX,
            evicted_idle: u64::MAX,
            bursts: u64::MAX,
        },
        connect,
        errors: u64::MAX,
        dns_misses: u64::MAX,
        dns_ms_sum: u64::MAX,
        dns_fail_misses: u64::MAX,
        active_max: u64::MAX,
        rss_max: u64::MAX,
        rss_sum: u64::MAX,
        rss_n: 1,
        seed_secs: u64::MAX,
        seed_requests: u64::MAX,
        seed_bytes: u64::MAX,
        seed_evicted: u64::MAX,
        // その日に動いた版の並びも上限まで (T.32.18)
        versions: Some("9".repeat(MAX_VERSIONS)),
    };
    let worst = acc.line(&clean_version(&"9".repeat(200)), [[u32::MAX; 2]; 2]);
    println!("daily line: worst {} B (limit {})", worst.len(), MAX_LINE);
    assert!(worst.len() <= MAX_LINE, "{} B: {}", worst.len(), worst);
    assert!(is_line(&worst));
    // ありふれた 1 行も測っておく (1 年 = 365 行の大きさの根拠)
    let mut w = Window::default();
    for ms in [3, 8, 9, 12, 45] {
        w.observe(ms);
    }
    let typical = Acc {
        day: 20_713,
        first_t: 20_713 * DAY,
        last_t: 20_713 * DAY + 86_395,
        samples: 17_279,
        start: Cum::default(),
        end: Cum {
            requests: 12_345,
            bytes: 9_876_543_210,
            evicted_idle: 3,
            bursts: 2,
        },
        connect: w,
        errors: 5,
        dns_misses: 123,
        dns_ms_sum: 3_988,
        dns_fail_misses: 100,
        active_max: 42,
        rss_max: 58_720_256,
        rss_sum: 52_428_800,
        rss_n: 1,
        ..Acc::default()
    }
    .line("0.1.0+fffb795", [[23, 1], [24, 0]]);
    println!("daily line: typical {} B -> {}", typical.len(), typical);
    assert!(typical.len() <= MAX_LINE);
}

/// 大域の口 ([`configure`] / [`tick`] / [`recent`]) も 1 度通しておく
/// (history スレッドが呼ぶのはこちら。**この単体テストだけが大域に触る**)。
#[test]
fn the_global_writer_records_and_reads_back() {
    let path = tmp("global");
    let metrics = Metrics::new();
    configure(Some(path.clone()), "0.1.0+global");
    let base = 20_300 * DAY;
    tick(&metrics, &sample(base + 10, 1));
    assert!(recent(DEFAULT_DAYS).lines.is_empty(), "日が変わるまでは空");
    tick(&metrics, &sample(base + DAY + 10, 9));
    let d = recent(DEFAULT_DAYS);
    assert_eq!(d.count, 1);
    assert_eq!(d.lines.len(), 1);
    assert!(!d.truncated);
    assert_eq!(d.path.as_deref(), Some(path.as_path()));
    assert!(d.bytes > 0 && d.bytes <= MAX_BYTES);
    assert_eq!(field(&d.lines[0], "requests"), Some("8"));
    assert_eq!(field(&d.lines[0], "version"), Some("0.1.0+global"));
    // 書かない設定に戻すと `tick` は原子の読み 1 回で戻り、`recent` は空
    configure(None, "");
    tick(&metrics, &sample(base + 2 * DAY, 99));
    assert!(recent(DEFAULT_DAYS).path.is_none());
    assert_eq!(read_lines(&path).len(), 1);
    let _ = fs::remove_file(&path);
}
