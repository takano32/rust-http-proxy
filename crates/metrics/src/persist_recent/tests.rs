//! 個票の永続化の試験 (T34.29 で `persist_recent.rs` から移した。名前はそのまま)。

use super::codec::*;
use super::*;
use crate::metrics::{BlockCause, ErrCause};
use crate::recent::{CloseReason, RecentEntry};

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "shp-recent-{}-{}-{:?}",
        name,
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn closed_entry(id: u64) -> RecentEntry {
    RecentEntry {
        id,
        at: 1_700_000_000 + id,
        client: "203.0.113.7".to_string(),
        target: "mtalk.google.com:5228".to_string(),
        connect: true,
        secs: 42,
        requests: 3,
        up: 1234,
        down: 5678,
        reason: CloseReason::Error(ErrCause::Refused),
        status: 200,
        parked_secs: 7,
        parks: 2,
        stage_ms: [1, 2, 3, 0, 0, 0],
        rtt_us: [1234, 5678],
        retrans: [0, 2],
        sni: Some("mtalk.google.com".into()),
        syn_retrans: 2,
        stall_ms: [1500, 0],
        spins: 9,
        half_closed: Some((crate::recent::ORIGIN_SIDE, 250)),
    }
}

/// 割り付けが **4 MiB ちょうど**であること (伸びないことと同じくらい大事な性質:
/// 余らせると次の版で「入るはず」と誤解する)。
#[test]
fn the_layout_fills_exactly_four_mib() {
    let l = Layout::current();
    assert_eq!(l.total, 4 * 1024 * 1024);
    assert_eq!(l.used, l.total, "ヘッダー + 4 領域でちょうど埋まる");
    assert_eq!(l.closed.bytes(), 2 * 1024 * 1024);
    assert_eq!(l.closed.record_size, 512);
    assert_eq!(l.errors.bytes(), 512 * 1024);
    assert_eq!(l.bursts.bytes(), 512 * 1024);
    assert_eq!(l.events.bytes(), 128 * 1024);
    assert_eq!(l.log.bytes(), 913_408);
    // 1 レコードの中身が入ること (組み立て時の assert と同じものを数字で残す)
    assert_eq!(
        (
            CLOSED_PAYLOAD,
            ERROR_PAYLOAD,
            LOG_PAYLOAD,
            EVENT_PAYLOAD,
            SHOT_PAYLOAD
        ),
        (392, 188, 252, 156, 2016)
    );
}

/// **版 3 のまま**、T15.0 より前に書いたレコードが読み継げること。
///
/// 前の版は 368 B しか書いていない (`spins` と半閉じの 3 本が無い)。`Dec` は
/// 足りなければ 0 を返すので、新しいコードで読むと**新しい欄だけが既定値**になり、
/// 古い欄は 1 つもずれない。`.recent` の `MAGIC` (`SHPREC03`) は上げない。
#[test]
fn a_record_written_before_the_spin_fields_still_reads_back() {
    let e = closed_entry(3);
    let full = encode_closed(1, &e);
    assert_eq!(full.len(), 392, "いまの中身は 392 B");
    // 前の版が書いた長さ (T14.42 の `stall_ms` まで)
    let old = &full[..368];
    let got = decode_closed(old).expect("前の版のレコードも読める");
    assert_eq!(got.spins, 0, "無い欄は 0");
    assert_eq!(got.half_closed, None, "無い欄は「半閉じしていない」");
    // 古い欄は 1 つもずれない
    assert_eq!(
        RecentEntry {
            spins: 0,
            half_closed: None,
            ..e
        },
        got
    );
}

#[test]
fn records_round_trip() {
    let e = closed_entry(9);
    // 書いた長さが上の定数と一致すること (欄を足したら両方直す)
    assert_eq!(encode_closed(1, &e).len(), CLOSED_PAYLOAD);
    assert_eq!(decode_closed(&encode_closed(1, &e)).unwrap(), e);

    let err = ErrorEntry::new(
        EntryKind::Connect,
        "ads.example.com:443",
        "198.51.100.3",
        403,
        EntryCause::Blocked(BlockCause::Blocklist),
        12,
        34,
    );
    assert_eq!(encode_error(1, &err).len(), ERROR_PAYLOAD);
    assert_eq!(decode_error(&encode_error(1, &err)).unwrap(), err);

    let line = Line {
        at: 1_700_000_123,
        level: Level::Warn,
        conn: Some(42),
        msg: "origin connect failed".to_string(),
    };
    assert_eq!(decode_log(&encode_log(1, &line)).unwrap(), line);
    let main_line = Line { conn: None, ..line };
    assert_eq!(decode_log(&encode_log(2, &main_line)).unwrap(), main_line);

    let event = Event {
        at: 1_700_000_300,
        kind: EventKind::Shutdown,
        text: "stop signal received".to_string(),
    };
    assert_eq!(encode_event(1, &event).len(), EVENT_PAYLOAD);
    assert_eq!(decode_event(&encode_event(1, &event)).unwrap(), event);

    let shot = BurstShot {
        at: 1_700_000_200,
        seq: 3,
        active: 5,
        trigger_active: 5,
        max_conns: 8,
        threshold: 4,
        clients: vec![("127.0.0.1".to_string(), 5)],
        clients_distinct: 1,
        clients_other: 0,
        targets: vec![("127.0.0.1:1234".to_string(), 5)],
        targets_distinct: 1,
        targets_other: 0,
        states: [0, 0, 5, 0, 0],
        connects: 5,
        https: 0,
        evicted_idle: 1,
        rejected_overload: 2,
        threads: 13,
        fds: 21,
        max_fds: 1024,
    };
    assert_eq!(
        encode_shot(1, &shot).len(),
        8 * 25 + (W_CLIENT + 8) + (W_ETARGET + 8)
    );
    assert_eq!(decode_shot(&encode_shot(1, &shot)).unwrap(), shot);
}

/// 長すぎる欄は**切れるが壊れない** (レコードからはみ出さない)。
#[test]
fn oversized_fields_are_clipped_not_overflowed() {
    let mut e = closed_entry(1);
    e.client = "x".repeat(200);
    e.target = "y".repeat(200);
    let rec = encode_closed(1, &e);
    assert!(rec.len() <= CLOSED_RECORD - 4, "{}", rec.len());
    let back = decode_closed(&rec).unwrap();
    assert_eq!(back.client.len(), W_CLIENT - 1);
    assert_eq!(back.target.len(), W_TARGET - 1);

    let line = Line {
        at: 1,
        level: Level::Error,
        conn: None,
        msg: "z".repeat(crate::log::MAX_LOG_LINE),
    };
    let rec = encode_log(1, &line);
    assert_eq!(rec.len(), LOG_PAYLOAD);
    assert_eq!(decode_log(&rec).unwrap().msg.len(), W_MSG - 1);
}

/// **リングが回ってもファイルは 4,194,304 B のまま**で、新しい方が残ること
/// (受け入れ基準: 10,000 件閉じても伸びない)。
#[test]
fn the_file_never_grows_when_the_ring_wraps() {
    let path = tmp("wrap");
    let metrics = Metrics::new();
    let (file, restored) = RecentFile::open(path.clone()).unwrap();
    assert!(restored.created);
    assert!(restored.closed.is_empty());
    for id in 0..10_000u64 {
        metrics.closed.push(closed_entry(id));
        file.write_new(&metrics);
    }
    assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
    assert_eq!(file.write_errors(), 0);
    drop(file);

    // 開き直すと、ファイルに残っている 8,192 件のうち新しい方から
    // メモリのリングのぶん (2,000 件) が返る
    let (file, restored) = RecentFile::open(path.clone()).unwrap();
    assert!(!restored.created, "版が同じなら作り直さない");
    assert_eq!(restored.closed.len(), MAX_RECENT);
    assert_eq!(
        restored.closed.first().unwrap().id,
        10_000 - MAX_RECENT as u64
    );
    assert_eq!(restored.closed.last().unwrap().id, 9_999);
    // 続きの通し番号から書ける (上書きしない)
    let m2 = Metrics::new();
    m2.closed.push(closed_entry(10_000));
    assert_eq!(file.write_new(&m2).closed, 1);
    drop(file);
    let (_, restored) = RecentFile::open(path.clone()).unwrap();
    assert_eq!(restored.closed.last().unwrap().id, 10_000);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
    let _ = std::fs::remove_file(&path);
}

/// 版が違う / 先頭が壊れたファイルは**読み捨てて作り直す** (落ちない)。
#[test]
fn a_corrupt_or_old_file_is_rebuilt() {
    let path = tmp("corrupt");
    let metrics = Metrics::new();
    {
        let (file, _) = RecentFile::open(path.clone()).unwrap();
        metrics.closed.push(closed_entry(1));
        assert_eq!(file.write_new(&metrics).closed, 1);
    }
    // 先頭 (版の印) を潰す
    let mut raw = std::fs::read(&path).unwrap();
    raw[..8].copy_from_slice(b"XXXXXXXX");
    std::fs::write(&path, &raw).unwrap();
    let (file, restored) = RecentFile::open(path.clone()).unwrap();
    assert!(restored.created, "版の印が違えば作り直す");
    assert!(restored.closed.is_empty(), "中身は捨てる");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
    // 短いファイルも同じ
    drop(file);
    std::fs::write(&path, b"SHPREC01").unwrap();
    let (_, restored) = RecentFile::open(path.clone()).unwrap();
    assert!(restored.created);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), FILE_SIZE);
    let _ = std::fs::remove_file(&path);
}

/// 閉じた接続のレコードが**書いた順のまま**読み戻ること (T14.55)。
///
/// T14.46 が数値の途中に `syn_retrans` を差し込んだので、版の印を `SHPREC03` に
/// 上げた。ずれていれば `stage_ms` 以降が 1 つずれて読めるので、**全部の欄**を
/// 突き合わせる。版 2 のファイルは読み捨てられること (中身が混ざらないこと) も見る。
#[test]
fn a_closed_record_reads_back_field_by_field() {
    let path = tmp("layout");
    let metrics = Metrics::new();
    // 数値の欄を全部違う値にして、1 つでもずれたら気づけるようにする
    let mut want = closed_entry(7);
    want.syn_retrans = 3;
    want.stage_ms = [11, 22, 33, 44, 55, 66];
    want.rtt_us = [77, 88];
    want.retrans = [9, 10];
    want.stall_ms = [1234, 4321];
    {
        let (file, _) = RecentFile::open(path.clone()).unwrap();
        metrics.closed.push(want.clone());
        assert_eq!(file.write_new(&metrics).closed, 1);
    }
    let (file, restored) = RecentFile::open(path.clone()).unwrap();
    assert!(!restored.created, "同じ版なら作り直さない");
    assert_eq!(restored.closed.len(), 1);
    assert_eq!(restored.closed[0], want, "欄の並びがずれている");

    // 版 2 の印を書いたファイルは読み捨てる (T14.46 より前のレコードは並びが違う)
    drop(file);
    let mut raw = std::fs::read(&path).unwrap();
    raw[..8].copy_from_slice(b"SHPREC02");
    std::fs::write(&path, &raw).unwrap();
    let (_, restored) = RecentFile::open(path.clone()).unwrap();
    assert!(restored.created, "版 2 は作り直す");
    assert!(restored.closed.is_empty(), "中身は捨てる");
    let _ = std::fs::remove_file(&path);
}

/// 1 周期に書くのは 64 KiB まで (越えた分は古い方から落ち、メモリには残る)。
#[test]
fn one_tick_writes_at_most_sixty_four_kib() {
    assert_eq!(
        TICK_CLOSED * CLOSED_RECORD
            + TICK_ERRORS * SMALL_RECORD
            + TICK_BURSTS * SHOT_RECORD
            + TICK_EVENTS * SMALL_RECORD
            + TICK_LOG * SMALL_RECORD,
        64 * 1024
    );
    let path = tmp("tick");
    let metrics = Metrics::new();
    let (file, _) = RecentFile::open(path.clone()).unwrap();
    for id in 0..(TICK_CLOSED as u64 + 40) {
        metrics.closed.push(closed_entry(id));
    }
    assert_eq!(file.write_new(&metrics).closed, TICK_CLOSED);
    // 落ちたのは古い方 (新しい 160 件が残る)
    drop(file);
    let (_, restored) = RecentFile::open(path.clone()).unwrap();
    assert_eq!(
        restored.closed.len(),
        TICK_CLOSED,
        "落ちた 40 件は書かれない"
    );
    assert_eq!(
        restored.closed.last().unwrap().id,
        TICK_CLOSED as u64 + 39,
        "残るのは新しい方"
    );
    let _ = std::fs::remove_file(&path);
}

/// 読み戻した件は**書き直さない** (印が通算に合っている)。
#[test]
fn restored_records_are_not_written_again() {
    let path = tmp("install");
    let first = Metrics::new();
    {
        let (file, _) = RecentFile::open(path.clone()).unwrap();
        for id in 0..3u64 {
            first.closed.push(closed_entry(id));
        }
        assert_eq!(file.write_new(&first).closed, 3);
    }
    let second = Metrics::new();
    let (file, restored) = RecentFile::open(path.clone()).unwrap();
    // ログのリングはプロセスに 1 つなので、他のテストの警告が混じりうる。
    // ここで見るのは閉じた接続の数だけ
    assert_eq!(restored.install(&second)[0], 3);
    assert_eq!(second.closed.len(), 3);
    assert_eq!(second.closed.restored(), 3);
    assert_eq!(
        file.write_new(&second).closed,
        0,
        "読み戻した分は書き直さない"
    );
    // 新しい 1 件だけが増える
    second.closed.push(closed_entry(99));
    assert_eq!(file.write_new(&second).closed, 1);
    drop(file);
    let (_, restored) = RecentFile::open(path.clone()).unwrap();
    assert_eq!(restored.closed.len(), 4);
    let _ = std::fs::remove_file(&path);
}
