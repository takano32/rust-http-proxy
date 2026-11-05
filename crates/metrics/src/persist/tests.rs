//! 状態ファイルの試験 (T34.31 で `persist.rs` から移した。名前はそのまま)。

use super::*;

/// 試験用の状態ファイル (プロセスごとに別の名前)。
fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("shp-baseline-{}-{}.rrd", name, std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(crate::persist_recent::path_next_to(&p));
    p
}

fn cache() -> Cache {
    Cache::new(crate::cache::CacheConfig::disabled())
}

/// `n` 件のホストを持つ統計 (要求数は `i + 1`、`bump` に入った番号だけ 1,000,000 足す)。
fn hosts(n: usize, bump: &[usize]) -> Metrics {
    let m = Metrics::new();
    let rows = (0..n)
        .map(|i| {
            let s = HostStats {
                requests: i as u64 + 1 + if bump.contains(&i) { 1_000_000 } else { 0 },
                last_seen: 1_700_000_000 + i as u64,
                ..HostStats::default()
            };
            (format!("host{i}.example"), s)
        })
        .collect();
    m.restore(rows, Vec::new());
    m
}

/// 60 秒ごとの表の書き直しは**変わった行だけ** (T32.22)。1,200 ホストのうち 10 件が動いた回は
/// 10 回 + 上位 1,000 の出入りのぶんだけ書き、再起動で読み戻す中身は今と同じ。
#[test]
fn the_stats_table_rewrites_only_the_rows_that_changed() {
    let path = tmp("tables");
    let (store, _) = Store::open(path.clone()).unwrap();
    let first = hosts(1200, &[]);
    store.flush_stats(&first);
    assert_eq!(store.table_writes(), 1000, "初回は上位 1,000 行を書く");
    // 何も変わらなければ 1 行も書かない
    store.flush_stats(&first);
    assert_eq!(store.table_writes(), 1000);
    // 上位 1,000 の中の 5 件と、外の 5 件 (199 番以下 = 上位から外れていた) が動いた:
    // 中の 5 件を書き直し、外の 5 件が入ったぶん 5 件が押し出される (5 スロットに上書き)
    let bumped = [1000usize, 1100, 1150, 1199, 1050, 0, 1, 2, 3, 4];
    let second = hosts(1200, &bumped);
    store.flush_stats(&second);
    let writes = store.table_writes() - 1000;
    assert!(writes <= 50, "1 分の pwrite が 50 回を越えた: {writes}");
    assert_eq!(
        writes, 10,
        "動いた 10 件だけ (押し出された 5 件のスロットは上書きで済む)"
    );
    drop(store);
    // 読み戻すと、いまの上位 1,000 がそのまま戻る (並びはスロットの順で、名前で比べる)
    let (store, loaded) = Store::open(path.clone()).unwrap();
    let mut got = loaded.hosts.clone();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    let mut want: Vec<_> = second.hosts_sorted().into_iter().take(1000).collect();
    want.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(got, want);
    // 読み戻した直後の書き直しも、変わっていなければ 1 行も書かない
    store.flush_stats(&second);
    assert_eq!(store.table_writes(), 0);
    // 減ったら余りを消す
    store.flush_stats(&hosts(10, &[]));
    drop(store);
    let (_, loaded) = Store::open(path.clone()).unwrap();
    assert_eq!(loaded.hosts.len(), 10);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(crate::persist_recent::path_next_to(&path));
}

/// 起動から 60 秒の基準が **1 回だけ**書かれ、次の起動では取り直されること (T24.4)。
///
/// 状態ファイルの版は上げていないので、記録は「余白に 1 本」置かれるだけ。
#[test]
fn the_baseline_is_written_once_and_taken_again_after_a_restart() {
    let path = tmp("once");
    let cache = cache();
    if heap_bytes(&cache).is_none() {
        // `mallinfo2` が読めない環境 (musl / glibc 2.32 以下 / Linux 以外) では書かない
        let (store, _) = Store::open(path.clone()).unwrap();
        assert!(!store.write_baseline_if_due(&cache, Duration::from_secs(3600)));
        assert_eq!(store.baseline(), None);
        let _ = std::fs::remove_file(&path);
        return;
    }
    let (store, _) = Store::open(path.clone()).unwrap();
    assert_eq!(store.baseline(), None, "まだ取っていない");
    assert!(
        !store.write_baseline_if_due(&cache, Duration::from_secs(BASELINE_AFTER_SECS - 1)),
        "60 秒の手前では取らない"
    );
    assert_eq!(store.baseline(), None);
    assert!(
        store.write_baseline_if_due(&cache, Duration::from_secs(BASELINE_AFTER_SECS)),
        "60 秒で 1 回だけ取る"
    );
    let (at, bytes) = store.baseline().expect("この起動の基準");
    assert!(bytes > 0, "基準のバイト数が 0");
    assert!(at > 0);
    // 2 回目以降は書かない (値も動かない)
    assert!(!store.write_baseline_if_due(&cache, Duration::from_secs(3_600)));
    assert_eq!(store.baseline(), Some((at, bytes)), "1 回だけ");
    drop(store);

    // 次の起動: 状態ファイルには前の起動の値が残っているが、**この起動の基準は出さない**
    // (別の起動の値なので、増えの基準にはできない)。60 秒たったら取り直す
    let (store, _) = Store::open(path.clone()).unwrap();
    assert_eq!(store.baseline(), None, "まだ取っていない");
    let recs = store.rrd.read_all(store.rrd.layout.baseline).unwrap();
    assert_eq!(
        recs.len(),
        1,
        "記録そのものは状態ファイルに残っている (版は 3 のまま)"
    );
    let mut d = crate::rrd::Dec(&recs[0].1);
    assert_eq!((d.u64(), d.u64()), (at, bytes), "前の起動の値が残っている");
    std::thread::sleep(Duration::from_millis(1_100));
    assert!(store.write_baseline_if_due(&cache, Duration::from_secs(BASELINE_AFTER_SECS)));
    let (again, _) = store.baseline().expect("取り直した基準");
    assert!(again > at, "取り直した時刻は前より後 ({} > {})", again, at);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(crate::persist_recent::path_next_to(&path));
}
