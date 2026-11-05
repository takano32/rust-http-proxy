//! 標本 1 本の `.rrd` のレコード (`Sample::encode` / `decode`) と予備の試験
//! (T.34.28 で `tests.rs` から移した。名前はそのまま)。

use super::codec::V3_FIXED_ITEMS;
use super::tests::sample;
use super::*;

/// 全部の欄に**違う値**を入れて `.rrd` のレコードへ往復させる。
///
/// 縛っているのは値の保存だけでなく **[`Sample::encode`] と [`Sample::decode`] の
/// 並びが同じこと**。`/history` の列数を見る検査 (`the_history_json_is_compact`) は
/// 順番の食い違いを通してしまい、統計が別の意味で読み戻る (T15.0 (10))。
/// 同じ値を 2 つ置くとその 2 つの入れ替えが見えなくなるので、**値は全部違える**こと。
#[test]
fn sample_encoding_round_trips() {
    let mut connect = Window::default();
    connect.observe(257);
    connect.observe(3);
    let mut forward = Window::default();
    forward.observe(9999);
    // `connect` / `forward` のどちらとも件数・合計・最大・段が違う窓 (T15.0 (10))
    let mut wait = Window::default();
    wait.observe(1);
    wait.observe(40);
    wait.observe(4000);
    // T.28.45 の「確立 − RTT」の窓も、ほかの 3 つと全部違える
    let mut excess = Window::default();
    excess.observe(2);
    excess.observe(60);
    excess.observe(61);
    excess.observe(700);
    // T.29.12 の名前解決を除いた窓も、ほかの 4 つと全部違える
    let mut conn_excess = Window::default();
    conn_excess.observe(4);
    conn_excess.observe(30);
    let s = Sample {
        t: 5,
        requests: 6,
        bytes: 7,
        active: 8,
        hits: 9,
        misses: 10,
        stores: 11,
        evictions: 12,
        mem_used: 13,
        mem_limit: 14,
        disk_used: 15,
        disk_limit: 16,
        rss: 17,
        connect,
        forward,
        errors: 18,
        errors_by_cause: [1, 2, 3, 4, 5, 6, 7, 8],
        dns_misses: 19,
        dns_ms_sum: 20,
        threads: 21,
        fds: 22,
        max_fds: 23,
        active_max: 24,
        threads_max: 25,
        fds_max: 26,
        evicted_idle: 27,
        wait,
        dns_warm: 28,
        requests_delta: 29,
        bytes_delta: 30,
        active_peak: 31,
        dns_warm_max: 32,
        dns_warm_sum: 33,
        gauge_n: 34,
        excess,
        dns_fail_misses: 35,
        conn_excess,
        other_port_attempts: 36,
        other_port_errors: 37,
        dns_fail_ms_sum: 38,
        web_dns_misses: 39,
        web_dns_ms_sum: 40,
    };
    let enc = s.encode();
    assert!(
        enc.len() <= crate::rrd::SAMPLE_RECORD - 4,
        "{} > {}",
        enc.len(),
        crate::rrd::SAMPLE_RECORD - 4
    );
    assert_eq!(Sample::decode(&enc), Some(s));
    assert_eq!(Sample::decode(&[0u8; 104]), None);

    // T15.0 (10) で足した 20 項目が**レコードの末尾**に並んでいること (手前の欄は
    // 1 つも動いていない = 版 3 のレコードがそのまま読み継げる)
    let mut d = crate::rrd::Dec(&enc[V3_FIXED_ITEMS * 8..]);
    assert_eq!(
        Window::decode(&mut d),
        wait,
        "予備の先頭は `wait` の窓 16 項目"
    );
    assert_eq!(
        [d.u64(), d.u64(), d.u64(), d.u64()],
        [28, 29, 30, 31],
        "`dns_warm` `requests_delta` `bytes_delta` `active_peak` の順"
    );
    // T16.0 の 3 項目はそのうしろ
    assert_eq!(
        [d.u64(), d.u64(), d.u64()],
        [32, 33, 34],
        "`dns_warm_max` `dns_warm_sum` `gauge_n` の順"
    );
    // T.28.45 の 17 項目はさらにそのうしろ
    assert_eq!(Window::decode(&mut d), excess, "`excess` の窓 16 項目");
    assert_eq!(d.u64(), 35, "`dns_fail_misses`");
    // T.29.12 の窓 16 項目、T.30.3 の 2 項目はさらにそのうしろ
    assert_eq!(
        Window::decode(&mut d),
        conn_excess,
        "`conn_excess` の窓 16 項目"
    );
    assert_eq!(
        [d.u64(), d.u64()],
        [36, 37],
        "`other_port_attempts` `other_port_errors` の順"
    );
    // T.32.16 の 1 項目、T.33.3 の 2 項目はさらにそのうしろ
    assert_eq!(d.u64(), 38, "`dns_fail_ms_sum`");
    assert_eq!(
        [d.u64(), d.u64()],
        [39, 40],
        "`web_dns_misses` `web_dns_ms_sum` の順"
    );
}

/// 版 3 の予備は **60 項目ぶんで始まり、まだ残っている** (T14.14 の受け入れ基準)。
///
/// 「これから足される欄」を今のコードで真似る: 固定の欄のうしろに残っている予備
/// ([`SAMPLE_SPARE_ITEMS`]) ぶんの `u64` を書いて `.rrd` に往復させ、
/// (a) 1 レコード (1,024 B) に収まる、(b) 全部そのまま読み戻る、
/// (c) **その欄を知らない今の `decode` は固定の欄だけを読んで 1 つもずれない**、の 3 つを見る。
///
/// **T14.14 の 60 は「版 3 を始めた時点の予備」** ([`V3_SPARE_ITEMS`] = 64) の話で、
/// 欄を足すたびに減る今の予備の話ではない。T14.14 はそれを
/// `SAMPLE_SPARE_ITEMS >= 60` と書き損ねていて、**4 項目を超えて足すと組み立てで
/// 落ちた** (T15.0 (10) で式を直した。60 項目は T14.6 の閉じた理由 8 + 寿命とバイトの 24・
/// T14.10 の canary 2・T14.12 のカーネルと cgroup 23 を全部載せてもまだ入る量)。
#[test]
fn the_version3_slack_started_at_sixty_and_what_is_left_survives_a_round_trip() {
    use crate::rrd::ring::Ring;
    use crate::rrd::{Dec, Enc, Rrd};

    assert_eq!(crate::rrd::SAMPLE_RECORD, 1024, "版 3 の標本のレコード");
    assert_eq!(
        V3_SPARE_ITEMS, 64,
        "版 3 を始めた時点の予備 516 B = 64 項目"
    );
    // 「版 3 の予備は 60 項目ぶん」と「今の予備が 1 項目以上」は上の `const _: ()` が
    // 組み立てのときに見ている (ここで書くと `clippy::assertions_on_constants`)
    let n = SAMPLE_SPARE_ITEMS as u64;
    let mut s = sample(1_700_000_000);
    s.requests = 12_345;
    s.evicted_idle = 7;
    s.active_peak = 9;
    let mut rec = s.encode();
    let mut e = Enc::new();
    for i in 1..=n {
        e.u64(i * 11);
    }
    rec.extend_from_slice(&e.0);
    assert_eq!(rec.len(), (SAMPLE_ITEMS + SAMPLE_SPARE_ITEMS) * 8);
    assert!(
        rec.len() <= crate::rrd::SAMPLE_RECORD - 4,
        "予備を使い切ると {} B で 1 レコードに入らない",
        rec.len()
    );

    let path = std::env::temp_dir().join(format!("shp-spare-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let (rrd, _) = Rrd::open(&path).unwrap();
    let (mut ring, _) = Ring::load(&rrd, rrd.layout.history_fine).unwrap();
    ring.push(&rrd, &rec).unwrap();
    let (_, got) = Ring::load(&rrd, rrd.layout.history_fine).unwrap();
    assert_eq!(got.len(), 1);
    // (c) 足した欄を知らない今のコードでも、固定の欄は 1 つもずれない
    assert_eq!(Sample::decode(&got[0]), Some(s));
    // (b) 足した項目もそのまま残っている
    let mut d = Dec(&got[0]);
    for _ in 0..SAMPLE_ITEMS {
        d.u64();
    }
    for i in 1..=n {
        assert_eq!(d.u64(), i * 11, "予備の {} 項目目", i);
    }
    let _ = std::fs::remove_file(&path);
}

/// `evicted_idle` は**レコードの余白に足した**ので `.rrd` の版は上がらない (T14.2 (3))。
///
/// 見るのは 2 つ: (a) 63 項目が領域 (508 B) に収まっていて、まだ余白があること、
/// (b) 版 2 で書かれた 62 項目のレコード (末尾はゼロ埋め) を読むと
/// `evicted_idle` が 0 になり、**手前の項目は 1 つもずれない**こと。
/// ずれるような足し方をすると、統計を捨てずに済まなくなる。
#[test]
fn evicted_idle_fits_in_the_record_slack_and_old_records_still_decode() {
    let mut s = sample(1_700_000_000);
    s.active_max = 240;
    s.threads_max = 128;
    s.fds_max = 1010;
    s.max_fds = 1024;
    s.evicted_idle = 7;
    let enc = s.encode();
    assert_eq!(
        enc.len(),
        SAMPLE_ITEMS * 8,
        "固定の欄だけで {} 項目 × 8 B",
        SAMPLE_ITEMS
    );
    assert!(
        enc.len() <= crate::rrd::SAMPLE_RECORD - 4,
        "{} > {} (版を上げずには入らない)",
        enc.len(),
        crate::rrd::SAMPLE_RECORD - 4
    );
    assert_eq!(Sample::decode(&enc), Some(s));

    // 版 2 のレコード = 最後の 1 項目が無く、領域の残りはゼロ埋め
    let mut old = enc[..62 * 8].to_vec();
    old.resize(crate::rrd::SAMPLE_RECORD - 4, 0);
    let back = Sample::decode(&old).expect("版 2 のレコードも読める");
    assert_eq!(back.evicted_idle, 0, "無い項目は 0");
    assert_eq!(
        Sample {
            evicted_idle: 0,
            ..s
        },
        back,
        "手前の項目は 1 つもずれない"
    );
}

/// T15.0 (10) の 20 列も**レコードの余白に足した**ので `.rrd` の版は上がらない。
///
/// 見るのは 3 つ: (a) 83 項目が 1 レコード (1,020 B) に収まっていて予備が残っている、
/// (b) **版 3 の 63 項目のレコード** (末尾はゼロ埋め) を読むと新しい 20 列が 0 になり、
/// **手前の項目は 1 つもずれない**、(c) その 20 列の値を入れた標本も往復する。
#[test]
fn the_new_columns_fit_in_the_record_slack_and_version3_records_still_decode() {
    let mut s = sample(1_700_000_000);
    s.active_max = 240;
    s.evicted_idle = 7;
    s.wait.observe(12);
    s.wait.observe(3400);
    s.dns_warm = 5;
    s.requests_delta = 1234;
    s.bytes_delta = 987_654;
    s.active_peak = 251;
    let enc = s.encode();
    // (a)
    assert_eq!(
        SAMPLE_ITEMS - 17 - 16 - 2 - 1 - 2,
        86,
        "63 + `wait` の窓 16 + 4 列 + T16.0 の 3 列 (+ T.28.45 の 17 + T.29.12 の 16 + T.30.3 の 2 + T.32.16 の 1 + T.33.3 の 2)"
    );
    assert_eq!(enc.len(), SAMPLE_ITEMS * 8);
    assert!(
        enc.len() <= crate::rrd::SAMPLE_RECORD - 4 && SAMPLE_SPARE_ITEMS >= 1,
        "{} B、予備 {} 項目",
        enc.len(),
        SAMPLE_SPARE_ITEMS
    );
    // (c)
    assert_eq!(Sample::decode(&enc), Some(s));

    // (b) 版 3 のレコード = 手前の 63 項目だけ書かれていて、残りはゼロ埋め
    let mut v3 = enc[..V3_FIXED_ITEMS * 8].to_vec();
    v3.resize(crate::rrd::SAMPLE_RECORD - 4, 0);
    let back = Sample::decode(&v3).expect("版 3 のレコードも読める");
    assert_eq!(
        (
            back.wait,
            back.dns_warm,
            back.requests_delta,
            back.bytes_delta,
            back.active_peak
        ),
        (Window::default(), 0, 0, 0, 0),
        "無い項目は 0"
    );
    assert_eq!(
        Sample {
            wait: Window::default(),
            dns_warm: 0,
            requests_delta: 0,
            bytes_delta: 0,
            active_peak: 0,
            ..s
        },
        back,
        "手前の項目は 1 つもずれない"
    );
}

/// T.28.45 の 2 つ (`excess` = 接続ごとの「確立 − カーネルの RTT」の窓と
/// `dns_fail_misses` = 名前解決に失敗した試みのミス) も**レコードの余白に足した**ので
/// `.rrd` の版は上がらない。
///
/// 見るのは 4 つ: (a) 103 項目が 1 レコードに収まり予備が残る、(b) T.28.45 より前の
/// 86 項目のレコード (末尾はゼロ埋め) を読むと 2 つが 0 (= 空の窓) で、手前は 1 つもずれない、
/// (c) 値を入れた標本も往復する、(d) 粗い解像度へは `connect` と同じく足し合わせ、
/// `/history` の列は**末尾**に 5 つ (`excesses` `excess_ms_sum` `excess_ms_max`
/// `excess_buckets` `dns_fail_misses`)。
#[test]
fn the_rtt_excess_window_fits_in_the_slack_and_older_records_read_it_as_empty() {
    let mut s = sample(1_700_000_000);
    s.connect.observe(30);
    s.excess.observe(3);
    s.excess.observe(70);
    s.dns_misses = 4;
    s.dns_fail_misses = 3;
    s.gauge_n = 1;
    let enc = s.encode();
    // (a)
    const {
        assert!(
            SAMPLE_ITEMS >= 103,
            "86 + `excess` の窓 16 + `dns_fail_misses` 1 (T.29.12 でさらに 16)"
        )
    };
    assert_eq!(enc.len(), SAMPLE_ITEMS * 8);
    assert!(
        enc.len() <= crate::rrd::SAMPLE_RECORD - 4 && SAMPLE_SPARE_ITEMS >= 1,
        "{} B、予備 {} 項目",
        enc.len(),
        SAMPLE_SPARE_ITEMS
    );
    // (c)
    assert_eq!(Sample::decode(&enc), Some(s));
    // (b) T.28.45 より前の 86 項目
    let mut old = enc[..86 * 8].to_vec();
    old.resize(crate::rrd::SAMPLE_RECORD - 4, 0);
    let back = Sample::decode(&old).expect("T.28.45 より前のレコードも読める");
    assert_eq!((back.excess, back.dns_fail_misses), (Window::default(), 0));
    assert_eq!(
        Sample {
            excess: Window::default(),
            dns_fail_misses: 0,
            ..s
        },
        back,
        "手前の項目は 1 つもずれない"
    );
    // (d) 畳むと足し合わせ
    let mut t = sample(1_700_000_005);
    t.excess.observe(1);
    t.dns_fail_misses = 2;
    let agg = Sample::downsample(&[s, t], 1_700_000_000);
    assert_eq!(agg.excess.count, 3);
    assert_eq!(agg.excess.ms_max, 70);
    assert_eq!(agg.excess.ms_sum, 74);
    assert_eq!(agg.dns_fail_misses, 5);
    let n = KEYS.iter().position(|k| *k == "excesses").unwrap();
    assert_eq!(
        &KEYS[n..n + 5],
        &[
            "excesses",
            "excess_ms_sum",
            "excess_ms_max",
            "excess_buckets",
            "dns_fail_misses"
        ]
    );
    assert_eq!(
        &KEY_KINDS[n..n + 5],
        &["delta", "delta", "peak", "buckets", "delta"]
    );
    // 行にその 5 列が並ぶ (後ろは T.29.12 の空の窓)
    let h = History::default();
    h.push(s);
    let json = h.to_json();
    assert!(
        json.contains(",2,73,70,[0,0,1,0,0,0,1,0,0,0,0,0,0],3,0,0,0,"),
        "{}",
        json
    );
}

/// T.29.12 の `conn_excess` (名前解決を除いた「確立 − カーネルの RTT」) も**レコードの余白に足した**ので
/// `.rrd` の版は上がらない。(a) 119 項目が 1 レコードに収まり予備が残る、(b) T.29.12 より前の 103 項目の
/// レコードは空の窓で読み戻り手前は 1 つもずれない、(c) 畳むと足し合わせ、(d) `/history` の列は**末尾**に 4 つ。
#[test]
fn the_connect_minus_rtt_window_fits_in_the_slack_and_older_records_read_it_as_empty() {
    let mut s = sample(1_700_000_000);
    s.connect.observe(30);
    s.excess.observe(300);
    s.conn_excess.observe(3);
    s.conn_excess.observe(70);
    s.gauge_n = 1;
    let enc = s.encode();
    // (a)
    const {
        assert!(
            SAMPLE_ITEMS >= 119,
            "103 + `conn_excess` の窓 16 (T.30.3 でさらに 2)"
        )
    };
    assert_eq!(enc.len(), SAMPLE_ITEMS * 8);
    const { assert!(SAMPLE_SPARE_ITEMS >= 1, "予備が残る") };
    assert_eq!(Sample::decode(&enc), Some(s));
    // (b)
    let mut old = enc[..103 * 8].to_vec();
    old.resize(crate::rrd::SAMPLE_RECORD - 4, 0);
    let back = Sample::decode(&old).expect("T.29.12 より前のレコードも読める");
    assert_eq!(
        Sample {
            conn_excess: Window::default(),
            ..s
        },
        back,
        "手前の項目は 1 つもずれない"
    );
    // (c)
    let mut t = sample(1_700_000_005);
    t.conn_excess.observe(1);
    let agg = Sample::downsample(&[s, t], 1_700_000_000);
    assert_eq!(
        (
            agg.conn_excess.count,
            agg.conn_excess.ms_sum,
            agg.conn_excess.ms_max
        ),
        (3, 74, 70)
    );
    // (d)
    let n = KEYS.len();
    assert_eq!(
        &KEYS[n - 9..n - 5],
        &[
            "conn_excesses",
            "conn_excess_ms_sum",
            "conn_excess_ms_max",
            "conn_excess_buckets"
        ]
    );
    assert_eq!(
        &KEY_KINDS[n - 9..n - 5],
        &["delta", "delta", "peak", "buckets"]
    );
    let h = History::default();
    h.push(s);
    let json = h.to_json();
    assert!(
        json.contains(",2,73,70,[0,0,1,0,0,0,1,0,0,0,0,0,0],0,0,0,0,0]"),
        "{}",
        json
    );
}

/// T.30.3 の `other_port_attempts` / `other_port_errors` (443・80 以外の宛先への試みとそのエラー。`/slo` の
/// `error_rate` が引く) も**レコードの余白に足した**ので `.rrd` の版は上がらない。(a) 121 項目が 1 レコードに
/// 収まり予備が残る、(b) T.30.3 より前の 119 項目のレコードは 0 (= 引くものが無い = 旧の数え方) で読み戻り
/// 手前は 1 つもずれない、(c) 畳むと足し合わせ、(d) `/history` の列は**末尾**に 2 つ (`delta`)。
#[test]
fn the_other_port_counts_fit_in_the_slack_and_older_records_read_them_as_zero() {
    let mut s = sample(1_700_000_000);
    s.connect.observe(30);
    s.conn_excess.observe(3);
    s.errors = 7;
    s.other_port_attempts = 9;
    s.other_port_errors = 6;
    s.gauge_n = 1;
    let enc = s.encode();
    // (a)
    const {
        assert!(
            SAMPLE_ITEMS >= 121,
            "119 + T.30.3 の 2 (T.32.16 でさらに 1)"
        )
    };
    assert_eq!(enc.len(), SAMPLE_ITEMS * 8);
    const { assert!(SAMPLE_SPARE_ITEMS >= 1, "予備が残る") };
    assert_eq!(Sample::decode(&enc), Some(s));
    // (b)
    let mut old = enc[..119 * 8].to_vec();
    old.resize(crate::rrd::SAMPLE_RECORD - 4, 0);
    let back = Sample::decode(&old).expect("T.30.3 より前のレコードも読める");
    assert_eq!(
        Sample {
            other_port_attempts: 0,
            other_port_errors: 0,
            ..s
        },
        back,
        "手前の項目は 1 つもずれない"
    );
    // (c)
    let mut t = sample(1_700_000_005);
    t.other_port_attempts = 1;
    t.other_port_errors = 1;
    let agg = Sample::downsample(&[s, t], 1_700_000_000);
    assert_eq!((agg.other_port_attempts, agg.other_port_errors), (10, 7));
    // (d)
    let i = KEYS
        .iter()
        .position(|k| *k == "other_port_attempts")
        .unwrap();
    assert_eq!(
        &KEYS[i..i + 2],
        &["other_port_attempts", "other_port_errors"]
    );
    assert_eq!(&KEY_KINDS[i..i + 2], &["delta", "delta"]);
    let h = History::default();
    h.push(s);
    let json = h.to_json();
    assert!(
        json.contains(",1,3,3,[0,0,1,0,0,0,0,0,0,0,0,0,0],9,6,0,0,0]"),
        "{}",
        json
    );
}

/// T.32.16 の `dns_fail_ms_sum` (名前解決に失敗した試みが払った ms。`dns_slow` が引く) も**レコードの余白に
/// 足した**ので `.rrd` の版は上がらない。(a) 122 項目が 1 レコードに収まり予備が 5 項目残る、(b) T.32.16 より前の
/// 121 項目のレコードは 0 (= 引くものが無い) で読み戻り手前は 1 つもずれない、(c) 畳むと足し合わせ、
/// (d) `/history` の列は**末尾**に 1 つ (`delta`)。
#[test]
fn the_failed_dns_ms_fit_in_the_slack_and_older_records_read_them_as_zero() {
    let mut s = sample(1_700_000_000);
    s.dns_misses = 5;
    s.dns_ms_sum = 2_400;
    s.dns_fail_misses = 2;
    s.dns_fail_ms_sum = 2_100;
    s.other_port_errors = 4;
    s.gauge_n = 1;
    let enc = s.encode();
    // (a)
    const {
        assert!(
            SAMPLE_ITEMS >= 122,
            "121 + T.32.16 の 1 (T.33.3 でさらに 2)"
        )
    };
    assert_eq!(enc.len(), SAMPLE_ITEMS * 8);
    const { assert!(SAMPLE_SPARE_ITEMS >= 1, "予備が残る") };
    assert_eq!(Sample::decode(&enc), Some(s));
    // (b)
    let mut old = enc[..121 * 8].to_vec();
    old.resize(crate::rrd::SAMPLE_RECORD - 4, 0);
    let back = Sample::decode(&old).expect("T.32.16 より前のレコードも読める");
    assert_eq!(
        Sample {
            dns_fail_ms_sum: 0,
            web_dns_misses: 0,
            web_dns_ms_sum: 0,
            ..s
        },
        back,
        "手前の項目は 1 つもずれない"
    );
    // (c)
    let mut t = sample(1_700_000_005);
    t.dns_fail_ms_sum = 7;
    let agg = Sample::downsample(&[s, t], 1_700_000_000);
    assert_eq!(agg.dns_fail_ms_sum, 2_107);
    // (d)
    let n = KEYS.len();
    assert_eq!(KEYS[n - 3], "dns_fail_ms_sum");
    assert_eq!(KEY_KINDS[n - 3], "delta");
    let h = History::default();
    h.push(s);
    let json = h.to_json();
    assert!(json.contains(",0,4,2100,0,0]"), "{}", json);
}

/// T.33.3 の `web_dns_misses`・`web_dns_ms_sum` (443・80 宛ての、名前解決に成功したミスの回数と ms。`dns_slow` が
/// 読む) も**レコードの余白に足した**ので `.rrd` の版は上がらない。(a) 124 項目が 1 レコードに収まり予備が 3 項目
/// 残る、(b) T.33.3 より前の 122 項目のレコードは 0 で読み戻り手前は 1 つもずれない、(c) 畳むと足し合わせ、
/// (d) `/history` の列は**末尾**に 2 つ (`delta`)。
#[test]
fn the_web_dns_misses_fit_in_the_slack_and_older_records_read_them_as_zero() {
    let mut s = sample(1_700_000_000);
    s.dns_misses = 15;
    s.dns_ms_sum = 5_500;
    s.dns_fail_ms_sum = 100;
    s.dns_fail_misses = 1;
    s.web_dns_misses = 5;
    s.web_dns_ms_sum = 1_500;
    s.gauge_n = 1;
    let enc = s.encode();
    // (a)
    assert_eq!(SAMPLE_ITEMS, 124, "122 + T.33.3 の 2");
    assert_eq!(enc.len(), SAMPLE_ITEMS * 8);
    assert_eq!(SAMPLE_SPARE_ITEMS, 3);
    assert_eq!(Sample::decode(&enc), Some(s));
    // (b)
    let mut old = enc[..122 * 8].to_vec();
    old.resize(crate::rrd::SAMPLE_RECORD - 4, 0);
    let back = Sample::decode(&old).expect("T.33.3 より前のレコードも読める");
    assert_eq!(
        Sample {
            web_dns_misses: 0,
            web_dns_ms_sum: 0,
            ..s
        },
        back,
        "手前の項目は 1 つもずれない"
    );
    // (c)
    let mut t = sample(1_700_000_005);
    t.web_dns_misses = 2;
    t.web_dns_ms_sum = 7;
    let agg = Sample::downsample(&[s, t], 1_700_000_000);
    assert_eq!((agg.web_dns_misses, agg.web_dns_ms_sum), (7, 1_507));
    // (d)
    let n = KEYS.len();
    assert_eq!(&KEYS[n - 2..], &["web_dns_misses", "web_dns_ms_sum"]);
    assert_eq!(&KEY_KINDS[n - 2..], &["delta", "delta"]);
    let h = History::default();
    h.push(s);
    let json = h.to_json();
    assert!(json.contains(",100,5,1500]"), "{}", json);
}
