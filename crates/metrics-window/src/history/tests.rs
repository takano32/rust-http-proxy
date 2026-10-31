use super::*;

fn sample(t: u64) -> Sample {
    Sample {
        t,
        requests: t * 2,
        active: (t % 7) as usize,
        mem_used: 100,
        ..Sample::default()
    }
}

/// `"samples":[[..],[..]]` の行数 (入れ子の配列は 1 列として数えない)。
fn rows(json: &str) -> usize {
    let at = json.find("\"samples\":[").unwrap() + "\"samples\":[".len();
    let (mut depth, mut n) = (0usize, 0usize);
    for c in json[at..].chars() {
        match c {
            '[' => {
                depth += 1;
                if depth == 1 {
                    n += 1;
                }
            }
            ']' if depth == 0 => break,
            ']' => depth -= 1,
            _ => {}
        }
    }
    n
}

#[test]
fn keeps_the_newest_samples_in_order() {
    let h = History::default();
    for t in 0..(DEFAULT_N as u64 + 5) {
        h.push(sample(t));
    }
    // リングは 6 時間ぶん持つので 1 本も落ちていないが、**既定の応答は新しい方の
    // 720 本だけ** (T14.32。`?n=` で遡る)
    assert_eq!(h.len(), DEFAULT_N + 5);
    let json = h.to_json();
    assert_eq!(rows(&json), DEFAULT_N);
    // 応答の形の版が先頭、標本は配列の配列で、キーはその後ろに 1 回だけ (T12.4 (3)、T14.49)
    assert!(
        json.starts_with("{\"schema\":1,\"interval_secs\":5,\"keys\":[\"t\","),
        "{}",
        &json[..80]
    );
    assert!(json.contains("\"samples\":[[5,10,0,5,0,"), "{}", json);
    // 標本の配列の閉じ方は変えず、その**後ろ**に閉じた接続の分布が付く (T14.6)。
    // 末尾は T15.0 (10) で足した `wait_buckets` と 4 列 (`dns_warm` /
    // `requests_delta` / `bytes_delta` / `active_peak`)、そのうしろが T16.0 の 3 列
    // (`dns_warm_max` / `dns_warm_sum` / `gauge_n`)、さらにうしろが T.28.45 の
    // `excess` の窓 4 列と `dns_fail_misses`、そのうしろが T.29.12 の `conn_excess` の窓 4 列、
    // そのうしろが T.30.3 の 2 列 (`other_port_attempts` / `other_port_errors`)、
    // いちばんうしろが T.32.16 の `dns_fail_ms_sum`
    assert!(
        json.contains(
            ",0,0,0,[0,0,0,0,0,0,0,0,0,0,0,0,0],0,0,0,0,0,0,0,\
             0,0,0,[0,0,0,0,0,0,0,0,0,0,0,0,0],0,\
             0,0,0,[0,0,0,0,0,0,0,0,0,0,0,0,0],0,0,0]],\"closed\":{"
        ),
        "{}",
        &json[json.len() - 600..]
    );
    // `keys` の隣に同じ長さの `key_kinds` が並ぶ (T15.0 (10))
    assert!(
        json.contains("\"evicted_idle\",\"waits\",\"wait_ms_sum\",\"wait_ms_max\",\"wait_buckets\",\"dns_warm\",\"requests_delta\",\"bytes_delta\",\"active_peak\",\"dns_warm_max\",\"dns_warm_sum\",\"gauge_n\",\"excesses\",\"excess_ms_sum\",\"excess_ms_max\",\"excess_buckets\",\"dns_fail_misses\",\"conn_excesses\",\"conn_excess_ms_sum\",\"conn_excess_ms_max\",\"conn_excess_buckets\",\"other_port_attempts\",\"other_port_errors\",\"dns_fail_ms_sum\"],\"key_kinds\":[\"time\",\"cumulative\","),
        "{}",
        &json[..900]
    );
    let kinds = json
        [json.find("\"key_kinds\":[").unwrap() + 13..json.find("],\"bounds_ms\"").unwrap()]
        .split(',')
        .count();
    assert_eq!(kinds, KEYS.len(), "`key_kinds` は `keys` と同じ長さ");
    // canary (T14.10) は**別の配列**で末尾に付く (既存の列は 1 つも動かない)
    assert!(
        json.ends_with(",\"canary\":{\"keys\":[\"t\",\"canary_dns_ms\",\"canary_connect_ms\",\"canary_host\",\"canary_ipv6_connect_ms\"],\"samples\":[]}}"),
        "{}",
        &json[json.len() - 150..]
    );
    // 列の数が `KEYS` と合っていること (入れ子の配列は 1 列と数える)
    let first = &json[json.find("\"samples\":[[").unwrap() + 11..];
    let row = &first[..first.find("],[").unwrap() + 1];
    let mut depth = 0;
    let cols = 1 + row
        .chars()
        .filter(|c| {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
            *c == ',' && depth == 1
        })
        .count();
    assert_eq!(cols, KEYS.len(), "{}", row);
}

/// T14.32 の受け入れ基準: **5 秒のリングを 5,000 回進めても 4,320 本 (6 時間) で
/// 頭打ち**になり、`?n=` で 4,320 本まで返せること。
#[test]
fn the_five_second_ring_holds_six_hours() {
    assert_eq!(CAPACITY * 5, 6 * 3600, "5 秒 × 4,320 = 6 時間");
    let h = History::default();
    for i in 0..5_000u64 {
        h.push(sample(i * 5));
    }
    assert_eq!(h.len(), CAPACITY);
    // 残っているのは新しい方の 4,320 本
    let q = h.rings[0].lock().unwrap();
    assert_eq!(q.front().unwrap().t, (5_000 - CAPACITY as u64) * 5);
    assert_eq!(q.back().unwrap().t, 4_999 * 5);
    drop(q);
    // 既定は今までどおり 720 本、`n=` は 4,320 本で頭打ち、下は 1 本
    assert_eq!(rows(&h.to_json()), DEFAULT_N);
    assert_eq!(rows(&h.to_json_res_n(0, Some(CAPACITY))), CAPACITY);
    assert_eq!(rows(&h.to_json_res_n(0, Some(99_999))), CAPACITY);
    assert_eq!(rows(&h.to_json_res_n(0, Some(1))), 1);
    assert_eq!(rows(&h.to_json_res_n(0, Some(0))), 1);
    // `n=` で切るのは新しい方から (いちばん新しい標本は必ず入る)
    let one = h.to_json_res_n(0, Some(1));
    assert!(one.contains("\"samples\":[[24995,"), "{}", one);
    // 1 分と 1 時間は 1 本も変えない
    assert_eq!(History::capacity(1), RESOLUTIONS[1].1);
    assert_eq!(History::capacity(2), RESOLUTIONS[2].1);
    assert_eq!(History::default_n(1), RESOLUTIONS[1].1);
    assert_eq!(History::default_n(2), RESOLUTIONS[2].1);
    // 読み戻し (`.rrd` の 720 本) はリングの末尾に入る
    let h2 = History::default();
    h2.restore(0, (0..RESOLUTIONS[0].1 as u64).map(|i| sample(i * 5)));
    assert_eq!(h2.len(), RESOLUTIONS[0].1);
}

#[test]
fn minute_and_hour_windows_are_rolled_up() {
    let h = History::default();
    let mut minutes = 0;
    let mut hours = 0;
    // 2 時間 + 少し、5 秒刻み
    for i in 0..(2 * 720 + 20) {
        let p = h.push(sample(1_000_000 + i * 5));
        if p.minute.is_some() {
            minutes += 1;
        }
        if p.hour.is_some() {
            hours += 1;
        }
    }
    assert!((119..=123).contains(&minutes), "minutes {}", minutes);
    assert!((1..=3).contains(&hours), "hours {}", hours);
    let m = h.to_json_res(1);
    assert!(m.starts_with("{\"schema\":1,\"interval_secs\":60,"));
    let q = h.rings[1].lock().unwrap();
    let first = q.front().unwrap();
    assert_eq!(first.t % 60, 0, "window start is aligned");
    // 累計は窓の最後の値 (t + 55 の standard)、ゲージは平均
    assert_eq!(first.requests, (first.t + 55) * 2);
    assert_eq!(first.mem_used, 100);
    let hq = h.rings[2].lock().unwrap();
    assert_eq!(hq.front().unwrap().t % 3600, 0);
}

/// T12.4 (3) の受け入れ基準: **2 つの区間に別々の分位点が出る**こと。
/// 累計しか無かった頃は「起動からの平均」しか読めず、直した前後が同じ数字に混ざっていた。
#[test]
fn two_intervals_keep_their_own_medians() {
    let h = History::default();
    let mut slow = sample(1_000_000);
    for _ in 0..100 {
        slow.connect.observe(257);
    }
    let mut fast = sample(1_000_005);
    for _ in 0..100 {
        fast.connect.observe(7);
    }
    h.push(slow);
    h.push(fast);
    let q = h.rings[0].lock().unwrap();
    let p50: Vec<f64> = q.iter().map(|s| s.connect.quantile_ms(0.5)).collect();
    assert_eq!(p50.len(), 2);
    assert!((250.0..=265.0).contains(&p50[0]), "{:?}", p50);
    assert!((5.0..=10.0).contains(&p50[1]), "{:?}", p50);
    // 畳むと 200 件がひとつの分布になる (件数は足し合わせ、最大は大きい方)
    drop(q);
    let agg = Sample::downsample(&[slow, fast], 1_000_000);
    assert_eq!(agg.connect.count, 200);
    assert_eq!(agg.connect.ms_max, 257);
    assert!((agg.connect.avg_ms() - 132.0).abs() < 1e-9);
}

/// `/history?res=5` の**既定** (720 標本) が 512 KiB に収まること (T12.4 (3))。
/// 値は「1 年動かしたあと」を想定した大きめの桁で埋める (短い数字で測ると通ってしまう)。
///
/// リングは 6 時間ぶん (4,320 本) 持つ (T14.32) が、**既定の応答は今までどおり**で、
/// `?n=4320` と書いたときだけ 6 倍になる (ここでその大きさも測って印字する)。
#[test]
fn a_full_hour_of_samples_fits_in_512_kib() {
    let h = History::default();
    for i in 0..(CAPACITY as u64) {
        let mut s = Sample {
            t: 1_770_000_000 + i * 5,
            requests: 12_345_678 + i,
            bytes: 987_654_321_098 + i,
            active: 240,
            hits: 1_234_567,
            misses: 2_345_678,
            stores: 345_678,
            evictions: 45_678,
            mem_used: 201_326_592,
            mem_limit: 268_435_456,
            disk_used: 2_900_000_000,
            disk_limit: 3_221_225_472,
            rss: 215_900_000,
            errors: 12_345,
            errors_by_cause: [1111, 2222, 3333, 4444, 5555, 6666, 7777, 8888],
            dns_misses: 99_999,
            dns_ms_sum: 888_888,
            threads: 128,
            fds: 1000,
            max_fds: 1024,
            active_max: 240,
            threads_max: 128,
            fds_max: 1010,
            ..Sample::default()
        };
        for b in s.connect.buckets.iter_mut() {
            *b = 999_999;
        }
        s.connect.count = 12_999_987;
        s.connect.ms_sum = 3_333_333_333;
        s.connect.ms_max = 30_000;
        s.forward = s.connect;
        h.push(s);
    }
    let json = h.to_json();
    assert_eq!(h.len(), CAPACITY);
    assert_eq!(rows(&json), DEFAULT_N);
    assert!(
        json.len() <= 512 * 1024,
        "/history?res=5 が {} B (512 KiB 超)",
        json.len()
    );
    let six = h.to_json_res_n(0, Some(CAPACITY));
    assert_eq!(rows(&six), CAPACITY);
    assert!(
        six.len() <= 6 * 512 * 1024,
        "/history?res=5&n=4320 が {} B",
        six.len()
    );
    println!(
        "/history?res=5 の既定 {} B ({} 標本)、n=4320 は {} B ({} 標本)",
        json.len(),
        DEFAULT_N,
        six.len(),
        CAPACITY
    );
}

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
    // T.32.16 の 1 項目はいちばんうしろ
    assert_eq!(d.u64(), 38, "`dns_fail_ms_sum`");
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

/// 区間の分位点は区間内を補間し、その窓の最大値で頭打ちになる (T12.4 (3) の受け入れ基準)。
#[test]
fn window_quantiles_land_inside_the_bucket() {
    let mut a = Window::default();
    for _ in 0..100 {
        a.observe(257);
    }
    let mut b = Window::default();
    for _ in 0..100 {
        b.observe(7);
    }
    assert!(
        (250.0..=265.0).contains(&a.quantile_ms(0.5)),
        "{}",
        a.quantile_ms(0.5)
    );
    assert!(
        (5.0..=10.0).contains(&b.quantile_ms(0.5)),
        "{}",
        b.quantile_ms(0.5)
    );
    assert_eq!(a.count, 100);
    assert!((a.avg_ms() - 257.0).abs() < 1e-9);
    assert_eq!(Window::default().quantile_ms(0.5), 0.0);
}

/// 粗い解像度へ畳むとき、区間の値は足し合わせ、ゲージの山は最大値で残る。
#[test]
fn downsampling_sums_the_windows_and_keeps_the_peaks() {
    let mut a = sample(0);
    a.connect.observe(257);
    a.errors = 1;
    a.errors_by_cause[0] = 1;
    a.dns_misses = 2;
    a.active_max = 5;
    a.fds_max = 30;
    a.wait.observe(300);
    a.requests = 100;
    a.requests_delta = 10;
    a.bytes = 4_000;
    a.bytes_delta = 400;
    a.dns_warm = 3;
    a.active_peak = 7;
    let mut b = sample(5);
    b.connect.observe(7);
    b.errors = 2;
    b.errors_by_cause[3] = 2;
    b.dns_misses = 1;
    b.active_max = 41;
    b.fds_max = 12;
    b.wait.observe(11);
    b.requests = 130;
    b.requests_delta = 30;
    b.bytes = 4_900;
    b.bytes_delta = 900;
    b.dns_warm = 5;
    b.active_peak = 62;
    let agg = Sample::downsample(&[a, b], 0);
    assert_eq!(agg.connect.count, 2);
    assert_eq!(agg.connect.ms_max, 257);
    assert_eq!(agg.errors, 3);
    assert_eq!(agg.errors_by_cause[0], 1);
    assert_eq!(agg.errors_by_cause[3], 2);
    assert_eq!(agg.dns_misses, 3);
    // 平均に畳むと 23 になって山が消える
    assert_eq!(agg.active_max, 41);
    assert_eq!(agg.fds_max, 30);
    // T15.0 (10) で足した列。`wait` は `connect` と同じ区間の窓なので足し合わせ
    assert_eq!((agg.wait.count, agg.wait.ms_max), (2, 300));
    // 通算は最後の値、その隣の区間の値は足し合わせ (同じ表に 2 種類あることを縛る)
    assert_eq!((agg.requests, agg.requests_delta), (130, 40));
    assert_eq!((agg.bytes, agg.bytes_delta), (4_900, 1_300));
    // warm な名前の件数は**平均** (`refreshes/h ÷ 平均 warm` がそのまま読める)
    assert_eq!(agg.dns_warm, 4);
    // 同時接続の山は最大 (平均に畳むと 34 になって消える)
    assert_eq!(agg.active_peak, 62);
}

/// **ゲージの平均が真の平均 ±0.5 に入り、最大が残る** (T16.0)。
///
/// 5 秒の値を並べた作り物を `History` に 2 時間ぶん流し、60 秒と 1 時間の行の `dns_warm` を
/// 5 秒の値の真の平均と比べる。値は 3 と 4 を 3 対 1 で混ぜる (真の平均 3.25。
/// 前の `Σ / 行数` の切り捨ては 60 秒で 3、1 時間でも 3 = 0.25 下。ただし 3.917 のような
/// 窓では 60 秒で 3 = 0.917 下に落ち、それを 1 時間でさらに切り捨てていた)。
#[test]
fn gauge_averages_stay_within_half_of_the_true_mean_and_the_max_survives() {
    let h = History::default();
    let base = 1_800_000_000u64; // 時の頭 (3600 で割り切れる)
    // 5 秒ごとの warm: 11 本 4 と 1 本 3 を 12 本の周期で (真の平均 3 + 11/12 = 3.9167)、
    // 1 時間に 1 回だけ 32 (バーストで満杯)。`active` も同じ並び
    let warm_at = |i: u64| -> u64 {
        if i % 720 == 100 {
            32
        } else if i.is_multiple_of(12) {
            3
        } else {
            4
        }
    };
    let steps = 2 * 720 + 13;
    let mut fine = Vec::new();
    for i in 0..steps {
        let w = warm_at(i);
        let s = Sample {
            t: base + i * 5,
            active: w as usize,
            dns_warm: w,
            dns_warm_max: w,
            dns_warm_sum: w,
            gauge_n: 1,
            ..Sample::default()
        };
        fine.push(s);
        h.push(s);
    }
    let true_mean = |from: u64, secs: u64| -> f64 {
        let v: Vec<u64> = fine
            .iter()
            .filter(|s| s.t >= from && s.t < from + secs)
            .map(|s| s.dns_warm)
            .collect();
        v.iter().sum::<u64>() as f64 / v.len() as f64
    };
    // 60 秒: 12 本 = 3 が 1 本と 4 が 11 本 (3.9167 → 4。切り捨てなら 3)
    let minutes: Vec<Sample> = h.rings[1].locked().iter().copied().collect();
    assert!(minutes.len() >= 120, "{}", minutes.len());
    for m in &minutes {
        let want = true_mean(m.t, 60);
        assert!(
            (m.dns_warm as f64 - want).abs() <= 0.5,
            "60 秒 t={} の dns_warm {} が真の平均 {:.4} から 0.5 より離れた",
            m.t,
            m.dns_warm,
            want
        );
        assert!((m.active as f64 - want).abs() <= 0.5, "active も同じ");
        assert_eq!(m.gauge_n, 12, "12 本を畳んだ");
        assert_eq!(m.dns_warm_sum, (want * 12.0).round() as u64);
    }
    // 1 時間: 真の平均 (3.9167 × 719 + 32) / 720 ≈ 3.956 → 4、最大 32 が残る
    let hours: Vec<Sample> = h.rings[2].locked().iter().copied().collect();
    assert!(!hours.is_empty());
    for hr in &hours {
        let want = true_mean(hr.t, 3600);
        assert!(
            (hr.dns_warm as f64 - want).abs() <= 0.5,
            "1 時間 t={} の dns_warm {} が真の平均 {:.4} から 0.5 より離れた",
            hr.t,
            hr.dns_warm,
            want
        );
        assert_eq!(hr.dns_warm_max, 32, "バーストの 32 が 1 時間の行に残る");
        assert_eq!(hr.gauge_n, 720);
        // ほかのゲージ 8 つは 60 秒の丸めを重ねるので最悪 ±1.0
        assert!((hr.active as f64 - want).abs() <= 1.0);
    }
    // 60 秒の行でも最大は残る (平均は 4 に埋もれる)
    let burst = minutes
        .iter()
        .find(|m| m.t <= base + 500 && base + 500 < m.t + 60)
        .expect("バーストの窓");
    assert_eq!(burst.dns_warm_max, 32);
    assert!(burst.dns_warm < 32);
}

/// T16.0 より前のレコード (`dns_warm_max` / `dns_warm_sum` / `gauge_n` が 0) を畳んでも、
/// 平均は今までの値の平均 (1 行 = 1 本)、最大は `dns_warm` を下限にする。
#[test]
fn old_records_without_the_gauge_sums_fold_as_one_sample_each() {
    let old = |t: u64, warm: u64, active: usize| Sample {
        t,
        dns_warm: warm,
        active,
        mem_used: 100,
        ..Sample::default()
    };
    // 古い 60 秒の行 3 本 (平均 5, 6, 8)
    let rows = [old(0, 5, 2), old(60, 6, 3), old(120, 8, 3)];
    let agg = Sample::downsample(&rows, 0);
    assert_eq!(agg.gauge_n, 3, "古い行は 1 本ずつ");
    assert_eq!(agg.dns_warm_sum, 19);
    assert_eq!(agg.dns_warm, 6, "19 / 3 = 6.33 → 6");
    assert_eq!(agg.dns_warm_max, 8, "最大は dns_warm を下限にする");
    assert_eq!(agg.active, 3, "8 / 3 = 2.67 → 3 (切り捨てなら 2)");
    assert_eq!(agg.mem_used, 100);
    // 新しい行と混ざっても、新しい行は標本の数で重みが付く
    let new = Sample {
        t: 180,
        dns_warm: 10,
        dns_warm_max: 12,
        dns_warm_sum: 120,
        gauge_n: 12,
        ..Sample::default()
    };
    let mixed = Sample::downsample(&[rows[0], new], 0);
    assert_eq!(mixed.gauge_n, 13);
    assert_eq!(mixed.dns_warm_sum, 125);
    assert_eq!(mixed.dns_warm, 10, "125 / 13 = 9.6 → 10");
    assert_eq!(mixed.dns_warm_max, 12);
    // 古いレコードを `.rrd` から読み戻しても値は変わらない (新しい欄は 0)
    let enc = rows[2].encode();
    let back = Sample::decode(&enc[..83 * 8]).expect("T16.0 より前の長さでも読める");
    assert_eq!(back, rows[2]);
    assert_eq!(
        (back.dns_warm_max, back.dns_warm_sum, back.gauge_n),
        (0, 0, 0)
    );
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
        SAMPLE_ITEMS - 17 - 16 - 2 - 1,
        86,
        "63 + `wait` の窓 16 + 4 列 + T16.0 の 3 列 (+ T.28.45 の 17 + T.29.12 の 16 + T.30.3 の 2 + T.32.16 の 1)"
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
        &KEYS[n - 7..n - 3],
        &[
            "conn_excesses",
            "conn_excess_ms_sum",
            "conn_excess_ms_max",
            "conn_excess_buckets"
        ]
    );
    assert_eq!(
        &KEY_KINDS[n - 7..n - 3],
        &["delta", "delta", "peak", "buckets"]
    );
    let h = History::default();
    h.push(s);
    let json = h.to_json();
    assert!(
        json.contains(",2,73,70,[0,0,1,0,0,0,1,0,0,0,0,0,0],0,0,0]"),
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
        json.contains(",1,3,3,[0,0,1,0,0,0,0,0,0,0,0,0,0],9,6,0]"),
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
    assert_eq!(SAMPLE_ITEMS, 122, "121 + T.32.16 の 1");
    assert_eq!(enc.len(), SAMPLE_ITEMS * 8);
    assert_eq!(SAMPLE_SPARE_ITEMS, 5);
    assert_eq!(Sample::decode(&enc), Some(s));
    // (b)
    let mut old = enc[..121 * 8].to_vec();
    old.resize(crate::rrd::SAMPLE_RECORD - 4, 0);
    let back = Sample::decode(&old).expect("T.32.16 より前のレコードも読める");
    assert_eq!(
        Sample {
            dns_fail_ms_sum: 0,
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
    assert_eq!(KEYS[n - 1], "dns_fail_ms_sum");
    assert_eq!(KEY_KINDS[n - 1], "delta");
    let h = History::default();
    h.push(s);
    let json = h.to_json();
    assert!(json.contains(",0,4,2100]"), "{}", json);
}
