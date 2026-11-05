use super::layout::V2_FILE_SIZE;
use super::*;

fn tmp(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("shp-rrd-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn creates_fixed_size_file_and_round_trips_records() {
    let path = tmp("basic");
    let (rrd, opened) = Rrd::open(&path).unwrap();
    assert!(opened.created);
    assert_eq!(opened.converted_from, None, "新規は変換ではない");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), rrd.layout.total);
    assert_eq!(rrd.layout.total, FILE_SIZE, "8 MiB 固定");
    assert_eq!(FILE_SIZE, 8_388_608);
    assert!(
        rrd.layout.used <= rrd.layout.total,
        "領域が余白に食い込まない"
    );
    let r = rrd.layout.overrides;
    assert!(rrd.read_all(r).unwrap().is_empty());
    let mut e = Enc::new();
    e.u64(42).str("ads.example.com", 128);
    rrd.write(r, 3, &e.0).unwrap();
    rrd.write(r, 0, &Enc::new().u64(1).str("a", 128).0).unwrap();
    let all = rrd.read_all(r).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[1].0, 3);
    let mut d = Dec(&all[1].1);
    assert_eq!(d.u64(), 42);
    assert_eq!(d.str(128), "ads.example.com");
    // 再オープンで残っている / サイズも変わらない
    let (rrd2, opened) = Rrd::open(&path).unwrap();
    assert!(!opened.created);
    assert_eq!(rrd2.read_all(r).unwrap().len(), 2);
    rrd2.clear(r, 3).unwrap();
    assert_eq!(rrd2.read_all(r).unwrap().len(), 1);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), rrd.layout.total);
    let _ = std::fs::remove_file(&path);
}

/// 版 3 のファイルを**そのまま** (余白のゼロのまま) 開くと、足した `baseline` は空で、
/// 前の 6 領域のオフセットは 1 バイトも動いていない (T24.4)。
#[test]
fn the_baseline_region_sits_in_the_spare_space_of_version_three() {
    let path = tmp("baseline");
    let (rrd, opened) = Rrd::open(&path).unwrap();
    assert!(opened.created);
    let l = rrd.layout;
    assert_eq!(
        l.baseline.offset,
        l.overrides.offset + l.overrides.bytes(),
        "足したのは領域の並びの末尾 (前の 6 領域は動かない)"
    );
    assert_eq!(l.baseline.count, 1);
    assert!(l.used <= l.total, "8 MiB の余白に収まる");
    assert_eq!(
        rrd.read_all(l.baseline).unwrap().len(),
        0,
        "まだ書いていない = 古い版 3 のファイルと同じ (ゼロ)"
    );
    let mut e = Enc::new();
    e.u64(1_790_000_000).u64(17_300_000);
    rrd.write(l.baseline, 0, &e.0).unwrap();
    let all = rrd.read_all(l.baseline).unwrap();
    assert_eq!(all.len(), 1);
    let mut d = Dec(&all[0].1);
    assert_eq!(d.u64(), 1_790_000_000);
    assert_eq!(d.u64(), 17_300_000);
    // 再オープンでも読める (版は 3 のまま)
    drop(rrd);
    let (rrd, _) = Rrd::open(&path).unwrap();
    assert_eq!(VERSION, 3, "足しただけなので版は上げない");
    let all = rrd.read_all(rrd.layout.baseline).unwrap();
    assert_eq!(Dec(&all[0].1).u64(), 1_790_000_000);
    let _ = std::fs::remove_file(&path);
}

/// 版 2 のファイルを 1 本作る (領域の `idx` 番目に `payload` を置く)。
fn write_v2(path: &std::path::Path, recs: &[(usize, usize, Vec<u8>)]) {
    let v2 = Layout::v2();
    let (f, created) = Fixed::open(path, MAGIC_V2, v2.total).unwrap();
    assert!(created);
    for (region, idx, payload) in recs {
        f.write(v2.regions()[*region], *idx, payload).unwrap();
    }
}

#[test]
fn a_version_two_file_is_converted_in_place_and_keeps_every_record() {
    let path = tmp("convert");
    // 履歴 (環状の領域 0) 3 本、ホスト 1 行、上書き 1 件を版 2 の形で置く
    let sample = |t: u64| {
        let mut e = Enc::new();
        e.u64(t).u64(t * 100);
        e.0
    };
    let host = {
        let mut e = Enc::new();
        e.str("connect://fixture.invalid:443", 128).u64(7);
        e.0
    };
    let over = {
        let mut e = Enc::new();
        e.u64(1).str("blocked.invalid", 128);
        e.0
    };
    write_v2(
        &path,
        &[
            (0, 5, sample(1_700_000_005)),
            (0, 6, sample(1_700_000_010)),
            (0, 7, sample(1_700_000_015)),
            (3, 0, host),
            (5, 2, over),
        ],
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), V2_FILE_SIZE);

    let (rrd, opened) = Rrd::open(&path).unwrap();
    assert_eq!(opened.converted_from, Some(2));
    assert!(!opened.created, "変換したファイルは「作り直した」ではない");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
    // 中身は添字ごとそのまま (末尾がゼロで伸びただけ)
    let hosts = rrd.read_all(rrd.layout.hosts).unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].0, 0);
    assert_eq!(
        hosts[0].1.len(),
        STATS_RECORD - 4,
        "版 3 の長さに伸びている"
    );
    let mut d = Dec(&hosts[0].1);
    assert_eq!(d.str(128), "connect://fixture.invalid:443");
    assert_eq!(d.u64(), 7);
    assert_eq!(d.u64(), 0, "予備はゼロで読める");
    // 環状の領域は時刻の順に戻り、続きから書ける
    let (mut ring, got) = ring::Ring::load(&rrd, rrd.layout.history_fine).unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(Dec(&got[0]).u64(), 1_700_000_005);
    assert_eq!(Dec(&got[2]).u64(), 1_700_000_015);
    ring.push(&rrd, &sample(1_700_000_020)).unwrap();
    let (_, got) = ring::Ring::load(&rrd, rrd.layout.history_fine).unwrap();
    assert_eq!(got.len(), 4);
    assert_eq!(Dec(&got[3]).u64(), 1_700_000_020);
    assert_eq!(rrd.read_all(rrd.layout.overrides).unwrap().len(), 1);
    assert_eq!(rrd.read_all(rrd.layout.clients).unwrap().len(), 0);
    // 2 回目の起動は変換しない (もう版 3 なので読むだけ)
    drop(rrd);
    let (rrd, opened) = Rrd::open(&path).unwrap();
    assert_eq!(opened, Opened::default(), "変換は 1 回だけ");
    assert_eq!(rrd.read_all(rrd.layout.hosts).unwrap().len(), 1);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_half_written_conversion_is_thrown_away_on_the_next_start() {
    let path = tmp("halfway");
    let row = {
        let mut e = Enc::new();
        e.str("a.invalid", 128).u64(1);
        e.0
    };
    write_v2(&path, &[(3, 0, row)]);
    // 変換の途中で落ちた形 = 8 MiB に伸びていて版の印が無い
    {
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(FILE_SIZE).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&f, &[0u8; 8], 0).unwrap();
    }
    let (rrd, opened) = Rrd::open(&path).unwrap();
    assert!(opened.created, "印の無いファイルは作り直す");
    assert_eq!(opened.converted_from, None);
    assert_eq!(rrd.read_all(rrd.layout.hosts).unwrap().len(), 0);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_version_one_file_is_still_thrown_away() {
    let path = tmp("v1");
    let mut buf = vec![0u8; 1_064_960];
    buf[..8].copy_from_slice(b"SHPRRD01");
    std::fs::write(&path, &buf).unwrap();
    let (rrd, opened) = Rrd::open(&path).unwrap();
    assert!(opened.created);
    assert_eq!(opened.converted_from, None);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
    assert_eq!(rrd.layout.used, 4_274_208, "領域の合計 (余白は 4 MiB 弱)");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn corrupt_record_is_skipped() {
    let path = tmp("corrupt");
    let (rrd, _) = Rrd::open(&path).unwrap();
    let r = rrd.layout.history_fine;
    rrd.write(r, 5, &Enc::new().u64(7).0).unwrap();
    // 1 バイト壊す
    rrd.inner
        .file
        .write_all_at(&[0xFF], r.offset + (5 * r.record_size) as u64 + 3)
        .unwrap();
    assert!(rrd.read_all(r).unwrap().is_empty());
    let _ = std::fs::remove_file(&path);
}
