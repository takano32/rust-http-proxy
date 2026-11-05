//! `snapshots` の単体試験 (T34.2 で `snapshots.rs` の `mod tests` から移した。名前はそのまま)。

use super::names::{day_of, moment_name};
use super::store::{existing, existing_moments, newest_day};
use super::writer::fits;
use super::*;

const DAY: u64 = SECS_PER_DAY;

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "rhp-t1434-{}-{}-{:?}",
        name,
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&p);
    p
}

/// 組んだことにする `/snapshot` (中身は形だけ)。
fn body() -> Option<String> {
    Some("{\"taken_at\":1,\"parts\":[\"status\"],\"status\":{}}".to_string())
}

/// 空きは潤沢 (予算では断らない)。
fn roomy(_dir: &Path) -> (Option<u64>, u64) {
    (Some(64 * 1024 * 1024 * 1024), 0)
}

fn dates(dir: &Path) -> Vec<String> {
    existing(dir)
}

#[test]
fn a_file_is_written_when_the_utc_day_changes() {
    let dir = tmp("rollover");
    let mut w = Writer::new(Some(dir.clone()), DEFAULT_DAYS);
    let base = 20_000 * DAY;
    // 同じ日のあいだは 1 つも書かない
    for i in 0..4 {
        assert_eq!(w.add(base + 86_000 + i * 5, body, roomy), None);
    }
    assert!(dates(&dir).is_empty(), "{:?}", dates(&dir));
    // 日が変わった最初の標本で 1 ファイル (**終わった日**の名前)
    let path = w.add(base + DAY + 5, body, roomy).expect("書かれていない");
    assert_eq!(path.file_name().unwrap(), "2024-10-04.json");
    assert_eq!(dates(&dir), vec!["2024-10-04"]);
    assert_eq!(w.counts(), (1, 0, 0));
    // 同じ日のうちは増えない
    assert_eq!(w.add(base + DAY + 10, body, roomy), None);
    assert_eq!(dates(&dir).len(), 1);
    // 次の日も 1 つ増える
    w.add(base + 2 * DAY, body, roomy).expect("2 日目");
    assert_eq!(dates(&dir), vec!["2024-10-04", "2024-10-05"]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn starting_twice_on_the_same_day_keeps_one_file() {
    let dir = tmp("twice");
    let base = 20_100 * DAY;
    let mut first = Writer::new(Some(dir.clone()), DEFAULT_DAYS);
    first.add(base + 100, body, roomy);
    first.add(base + DAY + 100, body, roomy).expect("1 つ目");
    assert_eq!(dates(&dir).len(), 1);
    // 同じ日をもう一度またぐ起動 (置いてある日付を見て書かない)
    let mut second = Writer::new(Some(dir.clone()), DEFAULT_DAYS);
    second.add(base + 200, body, roomy);
    assert_eq!(second.add(base + DAY + 200, body, roomy), None);
    assert_eq!(dates(&dir).len(), 1, "{:?}", dates(&dir));
    assert_eq!(second.counts(), (0, 0, 0));
    // その次の日はちゃんと増える
    second.add(base + 2 * DAY, body, roomy).expect("次の日");
    assert_eq!(dates(&dir).len(), 2);
    let _ = fs::remove_dir_all(&dir);
}

/// 31 個目を書いたら最古が消える (`PROXY_SNAPSHOT_DAYS` 既定 30)。
#[test]
fn the_oldest_is_dropped_at_the_thirty_first() {
    let dir = tmp("keep30");
    let base = 20_200 * DAY;
    let mut w = Writer::new(Some(dir.clone()), DEFAULT_DAYS);
    w.add(base, body, roomy);
    for i in 1..=30 {
        w.add(base + i * DAY, body, roomy).expect("毎日 1 つ");
    }
    let all = dates(&dir);
    assert_eq!(all.len(), 30, "{:?}", all);
    assert_eq!(all.first(), Some(&day_name(20_200)));
    // 31 個目 (= 通算 31 日目) で最古が 1 つ消えて 30 のまま
    w.add(base + 31 * DAY, body, roomy).expect("31 個目");
    let all = dates(&dir);
    assert_eq!(all.len(), 30, "{:?}", all);
    assert_eq!(all.first(), Some(&day_name(20_201)));
    assert_eq!(all.last(), Some(&day_name(20_230)));
    assert_eq!(w.counts(), (31, 0, 0));
    let _ = fs::remove_dir_all(&dir);
}

/// `PROXY_SNAPSHOT_DAYS=0` は止める (書き先を持たない)。
#[test]
fn zero_days_writes_nothing() {
    let dir = tmp("zero");
    let mut w = Writer::new(Some(dir.clone()), 0);
    assert!(w.dir().is_none());
    let base = 20_300 * DAY;
    w.add(base, body, roomy);
    assert_eq!(w.add(base + DAY, body, roomy), None);
    assert!(!dir.exists(), "ディレクトリを作っている");
    // 書き先が無いときも同じ
    let mut none = Writer::new(None, DEFAULT_DAYS);
    none.add(base, body, roomy);
    assert_eq!(none.add(base + DAY, body, roomy), None);
    assert_eq!(none.counts(), (0, 0, 0));
}

/// ディスクの予算 (T12.5 の `keep_free`) を越えるなら書かない。
#[test]
fn the_disk_budget_stops_the_write() {
    // 見送ったことを `/events` に 1 件残すので、リング 1 本きりの鍵を取る
    let _g = crate::events::TEST_LOCK.locked();
    let dir = tmp("budget");
    let base = 20_400 * DAY;
    let mut w = Writer::new(Some(dir.clone()), DEFAULT_DAYS);
    w.add(base, body, roomy);
    // 空き 1 MiB・空けておく量 1 MiB なので 1 バイトも書けない
    let tight = |_: &Path| (Some(1024 * 1024), 1024 * 1024);
    assert_eq!(w.add(base + DAY, body, tight), None);
    assert!(dates(&dir).is_empty(), "{:?}", dates(&dir));
    assert_eq!(w.counts(), (0, 0, 1));
    // 次の日に空きが戻れば書く (見送った日はもう来ない)
    w.add(base + 2 * DAY, body, roomy)
        .expect("空きが戻ったら書く");
    assert_eq!(dates(&dir), vec![day_name(20_401)]);
    // 判定そのもの
    assert!(fits(None, 1 << 30, 4 << 20), "測れない環境では書く");
    assert!(fits(Some(100), 0, 100));
    assert!(!fits(Some(99), 0, 100));
    assert!(!fits(Some(1 << 30), 1 << 30, 1));
    let _ = fs::remove_dir_all(&dir);
}

/// 組み方を預かっていなければ書かない (依存の輪を作らないための形)。
#[test]
fn nothing_is_written_without_a_builder() {
    let dir = tmp("nobuilder");
    let base = 20_500 * DAY;
    let mut w = Writer::new(Some(dir.clone()), DEFAULT_DAYS);
    w.add(base, || None, roomy);
    assert_eq!(w.add(base + DAY, || None, roomy), None);
    assert!(dates(&dir).is_empty());
    let _ = fs::remove_dir_all(&dir);
}

/// 日付の名前は往復できる (`day_name` ↔ `day_of`)、読めない名前は入れない。
#[test]
fn dates_round_trip_and_bad_names_are_refused() {
    for day in [0u64, 1, 19_000, 20_713, 40_000] {
        let name = day_name(day);
        assert!(is_date(&name), "{}", name);
        assert_eq!(day_of(&name), Some(day), "{}", name);
    }
    // 目印 (T14.20 の `daily` の単体テストと同じ日)
    assert_eq!(day_name(20_000), "2024-10-04");
    for bad in [
        "",
        "2026-9-16",
        "2026-09-16.json",
        "../../etc/passwd",
        "2026-09-1x",
        "2026/09/16",
        "2026-13-01",
        "2026-09-32",
    ] {
        assert!(day_of(bad).is_none(), "{}", bad);
    }
    assert!(!is_date("../../etc"));
    // 一覧に入るのは `<YYYY-MM-DD>.json` だけ
    let dir = tmp("names");
    fs::create_dir_all(&dir).unwrap();
    for name in [
        "2026-09-15.json",
        "2026-09-16.json",
        "notes.txt",
        "2026-09-17.json.tmp",
        "snapshot.json",
    ] {
        fs::write(dir.join(name), "{}").unwrap();
    }
    assert_eq!(dates(&dir), vec!["2026-09-15", "2026-09-16"]);
    assert_eq!(newest_day(&dir), day_of("2026-09-16").unwrap());
    assert_eq!(newest_day(Path::new("/nonexistent/rhp-t1434")), 0);
    let _ = fs::remove_dir_all(&dir);
}

/// 起動と停止の名前は往復でき、読めない名前は入れない (T28.20)。
#[test]
fn moment_names_round_trip_and_bad_names_are_refused() {
    // 2026-10-25T20:31:34Z (tokyo の T28 の雪像の時刻)
    let t = 20_751 * DAY + 20 * 3600 + 31 * 60 + 34;
    assert_eq!(
        moment_name(t, Moment::Shutdown),
        "2026-10-25T203134Z-shutdown"
    );
    assert_eq!(moment_name(t, Moment::Start), "2026-10-25T203134Z-start");
    for m in Moment::ALL {
        assert_eq!(parse_moment(&moment_name(t, m)), Some((t, m)));
    }
    for bad in [
        "",
        "2026-10-25",
        "2026-10-25T203134Z",
        "2026-10-25T203134Z-reboot",
        "2026-10-25T2031Z-shutdown",
        "2026-10-25T243134Z-shutdown",
        "2026-10-25T206034Z-start",
        "2026-13-25T203134Z-start",
        "../2026-10-25T203134Z-start",
        "2026-10-25T203134Z-start/../../x",
        "2026-10-25T20313xZ-start",
    ] {
        assert!(parse_moment(bad).is_none(), "{}", bad);
    }
    // 一覧に入るのは形の合う名前だけ (日次の 1 枚とは混ざらない)
    let dir = tmp("moment-names");
    fs::create_dir_all(&dir).unwrap();
    for name in [
        "2026-10-25T203134Z-shutdown.json",
        "2026-10-24T010203Z-start.json",
        "2026-10-25.json",
        "2026-10-25T203134Z-shutdown.json.tmp",
        "2026-10-25T203134Z-snapshot.json",
    ] {
        fs::write(dir.join(name), "{}").unwrap();
    }
    let names: Vec<String> = existing_moments(&dir).into_iter().map(|m| m.0).collect();
    assert_eq!(
        names,
        ["2026-10-24T010203Z-start", "2026-10-25T203134Z-shutdown"]
    );
    assert_eq!(dates(&dir), vec!["2026-10-25"]);
    let _ = fs::remove_dir_all(&dir);
}

/// 起動・停止の 1 枚を書き、種類ごとに日数と枚数で古いものを消す。日次の 1 枚には触らない。
#[test]
fn moments_are_written_and_pruned_by_kind() {
    let dir = tmp("moments");
    let mut w = Writer::new(Some(dir.clone()), 3);
    let base = 20_700 * DAY;
    // 日次の 1 枚を置いておく (消されないこと)
    w.add(base, body, roomy);
    w.add(base + DAY, body, roomy).expect("日次");
    // 停止を 1 日おきに 5 回 (3 日より古いものと、新しい方から 3 枚を越えたものが消える)
    let mut written = Vec::new();
    for i in 0..5 {
        let t = base + i * DAY + 100;
        let p = w
            .write_moment(t, Moment::Shutdown, body, roomy)
            .expect("停止の 1 枚");
        assert_eq!(
            p.file_name().unwrap().to_string_lossy(),
            format!("{}.json", moment_name(t, Moment::Shutdown))
        );
        written.push(moment_name(t, Moment::Shutdown));
    }
    // 起動を同じ時刻に 4 回 (日数では消えず、枚数の 3 で 1 枚消える)
    for i in 0..4 {
        w.write_moment(base + 4 * DAY + 200 + i, Moment::Start, body, roomy)
            .expect("起動の 1 枚");
    }
    let left: Vec<(String, Moment)> = existing_moments(&dir)
        .into_iter()
        .map(|(n, _, m)| (n, m))
        .collect();
    let shutdowns: Vec<&String> = left
        .iter()
        .filter(|(_, m)| *m == Moment::Shutdown)
        .map(|(n, _)| n)
        .collect();
    // 最後に書いた時刻 (base + 4 日) から 3 日以内 = 1・2・3・4 日目のうち新しい 3 枚
    assert_eq!(shutdowns, [&written[2], &written[3], &written[4]]);
    assert_eq!(left.iter().filter(|(_, m)| *m == Moment::Start).count(), 3);
    assert_eq!(dates(&dir), vec![day_name(20_700)], "日次の 1 枚はそのまま");
    assert_eq!(w.moments_written(), 9);
    // 予算を越えるなら書かない (日次と同じ判定)
    let _g = crate::events::TEST_LOCK.locked();
    let tight = |_: &Path| (Some(1024 * 1024), 1024 * 1024);
    assert_eq!(
        w.write_moment(base + 5 * DAY, Moment::Shutdown, body, tight),
        None
    );
    // 書き先が無い設定では組みもしない
    let mut none = Writer::new(None, 3);
    assert_eq!(
        none.write_moment(base, Moment::Start, || panic!("組まない"), roomy),
        None
    );
    let _ = fs::remove_dir_all(&dir);
}

/// 大域の口 ([`configure`] / [`tick`] / [`list`] / [`read`]) も 1 度通しておく
/// (**この単体テストだけが大域に触る**)。
#[test]
fn the_global_writer_records_and_reads_back() {
    let dir = tmp("global");
    let cache = Cache::new(proxy_cache::config::CacheConfig::disabled());
    let sample = |t: u64| Sample {
        t,
        ..Sample::default()
    };
    // 組み方に渡った「`recent` の部の上限」を中身に写す (T28.20・T28.46。`null` = 256 KiB で切る)
    set_builder(Box::new(|max, _limit| {
        format!(
            "{{\"taken_at\":1,\"parts\":[\"status\"],\"recent_max\":{}}}",
            max.map_or("null".to_string(), |n| n.to_string())
        )
    }));
    // 書かない設定のうちは起動・停止の 1 枚も書かない (組みもしない)
    configure(None, DEFAULT_DAYS);
    assert_eq!(take_shutdown(&cache), None);
    assert_eq!(take_start_if_due(&cache, true), None);
    configure(Some(dir.clone()), DEFAULT_DAYS);
    // 起動の 1 枚: 待ち時間の前は書かず、たったら 1 回だけ (`recent` は切る方)
    assert_eq!(take_start_if_due(&cache, false), None);
    let start = take_start_if_due(&cache, true).expect("起動の 1 枚");
    assert_eq!(take_start_if_due(&cache, true), None, "起動ごとに 1 回");
    assert!(
        fs::read_to_string(&start)
            .unwrap()
            .contains("\"recent_max\":null")
    );
    // 停止の 1 枚は `recent` を全部 (4 MiB まで) 入れる方で組む
    let stop = take_shutdown(&cache).expect("停止の 1 枚");
    assert!(
        fs::read_to_string(&stop)
            .unwrap()
            .contains("\"recent_max\":4194304")
    );
    let l = list();
    assert!(l.files.is_empty(), "日次の一覧には混ぜない");
    let kinds: Vec<Moment> = l.moments.iter().map(|m| m.kind).collect();
    assert!(kinds.contains(&Moment::Start) && kinds.contains(&Moment::Shutdown));
    assert_eq!(l.moments_bytes, l.moments.iter().map(|m| m.bytes).sum());
    for m in &l.moments {
        assert!(parse_moment(&m.name).is_some(), "{}", m.name);
        assert!(read(&m.name).unwrap().contains("\"taken_at\""));
    }
    for p in [&start, &stop] {
        fs::remove_file(p).unwrap();
    }
    let base = 20_600 * DAY;
    tick(&cache, &sample(base + 10));
    assert!(list().files.is_empty(), "日が変わるまでは空");
    tick(&cache, &sample(base + DAY + 10));
    let l = list();
    assert_eq!(l.days, DEFAULT_DAYS);
    assert_eq!(l.dir.as_deref(), Some(dir.as_path()));
    assert_eq!(l.files.len(), 1);
    assert_eq!(l.files[0].date, day_name(20_600));
    assert!(l.files[0].bytes > 0 && l.bytes == l.files[0].bytes);
    assert!(read(&day_name(20_600)).unwrap().contains("\"taken_at\""));
    // 日次の 1 枚は `recent` を 1 MiB まで持つ方で組む (T28.46)
    assert!(
        read(&day_name(20_600))
            .unwrap()
            .contains("\"recent_max\":1048576")
    );
    assert!(read(&day_name(20_599)).is_none(), "置いていない日");
    assert!(read("../../etc/passwd").is_none());
    // 書かない設定に戻すと `tick` は原子の読み 1 回で戻り、`list` は空
    configure(None, DEFAULT_DAYS);
    tick(&cache, &sample(base + 2 * DAY));
    assert!(list().dir.is_none() && list().files.is_empty());
    assert!(read(&day_name(20_600)).is_none());
    assert_eq!(dates(&dir).len(), 1, "置いてあるものには手を付けない");
    let _ = fs::remove_dir_all(&dir);
}
