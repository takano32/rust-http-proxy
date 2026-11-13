use super::window::{State, diff, push};
use super::*;

/// 窓の中身を直に組み立てる (グローバルの [`STATE`] はテストの間で共有なので触らない)。
fn raw(t: u64, overflows: u64, tw: u64) -> Latest {
    Latest {
        at: t,
        avail: SRC_NETSTAT | SRC_SNMP | SRC_SOCKSTAT | SRC_PSI_CPU | SRC_STATE_FILE,
        listen_overflows: overflows,
        time_wait: tw,
        psi_cpu_some: 43.02,
        psi_cpu_full: 2.99,
        ..Latest::default()
    }
}

/// 累計のものは**増分**、値のものは値で窓に入る。
#[test]
fn counters_become_deltas_and_gauges_stay_values() {
    let a = raw(1_000_000, 100, 30_912);
    let b = raw(1_000_005, 103, 31_000);
    let first = diff(&a, &a);
    assert_eq!(first.listen_overflows, 0, "1 本目は増分が出せない");
    assert_eq!(first.time_wait, 30_912, "値はそのまま");
    let next = diff(&a, &b);
    assert_eq!(next.listen_overflows, 3);
    assert_eq!(next.time_wait, 31_000);
    // カウンタが減っていたら 0 (折り返し・入れ替え)
    assert_eq!(diff(&b, &a).listen_overflows, 0);
}

/// 名前解決のミスは「この窓の回数」と「1 回の平均 ms」になる。
#[test]
fn dns_misses_become_the_price_of_one_miss() {
    let mut a = raw(1_000_000, 0, 0);
    a.dns_misses = 10;
    a.dns_us_sum = 100_000;
    let mut b = a;
    b.at = 1_000_005;
    b.dns_misses = 12;
    b.dns_us_sum = 100_000 + 2 * 12_600;
    let s = diff(&a, &b);
    assert_eq!(s.dns_misses, 2);
    assert_eq!(s.dns_miss_ms, 13, "12.6 ms は 13 に丸まる");
    assert_eq!(diff(&a, &a).dns_miss_ms, 0, "ミスが無ければ 0");
}

/// 1 分へ畳むと増分は足し合わせ、値と PSI は**最大** (山が消えない)。
#[test]
fn downsampling_sums_the_deltas_and_keeps_the_peaks() {
    let mut a = diff(&raw(0, 0, 0), &raw(1_000_000, 3, 1_000));
    let mut b = diff(&raw(0, 0, 0), &raw(1_000_005, 4, 30_912));
    a.dns_misses = 1;
    a.dns_miss_ms = 10;
    b.dns_misses = 3;
    b.dns_miss_ms = 30;
    b.psi_cpu_some = 90.0;
    let agg = Sample::downsample(&[a, b], 1_000_000);
    assert_eq!(agg.listen_overflows, 7);
    assert_eq!(agg.time_wait, 30_912, "山は最大値");
    assert_eq!(agg.psi_cpu_some, 90.0);
    assert_eq!(agg.dns_misses, 4);
    assert_eq!(agg.dns_miss_ms, 25, "回数で重みを付けた平均 (10+90)/4");
}

/// CPU の絞りは**割合**で読む: 期間の数が分母、絞られた期間が分子 (T15.0)。
///
/// 累計はこのプロセスより前から動いている (親の階層の値) ので、生の値ではなく
/// 増分どうしを割る。1 分へ畳むときは分子も分母も足し合わせる。
#[test]
fn the_cpu_periods_are_the_denominator_of_the_throttling() {
    let raw = |t: u64, periods: u64, throttled: u64| Latest {
        at: t,
        avail: SRC_CGROUP_CPU,
        cpu_nr_periods: periods,
        cpu_nr_throttled: throttled,
        ..Latest::default()
    };
    // 起動より前から動いている累計 (8,000 期間のうち 915 絞られていた)
    let a = raw(1_000_000, 8_000, 915);
    let b = raw(1_000_005, 8_050, 965);
    assert_eq!(diff(&a, &a).cpu_nr_periods, 0, "1 本目は増分が出せない");
    let s = diff(&a, &b);
    assert_eq!(s.cpu_nr_periods, 50);
    assert_eq!(s.cpu_nr_throttled, 50, "この 5 秒は 50/50 = 100%");
    // 畳むと分子も分母も足し合わせ (割合が保たれる)
    let c = diff(&b, &raw(1_000_010, 8_100, 1_015));
    let agg = Sample::downsample(&[s, c], 1_000_000);
    assert_eq!((agg.cpu_nr_throttled, agg.cpu_nr_periods), (100, 100));
    // 列は [`KEYS`] の**末尾**に出る (古い読み手は位置で開く)
    let mut out = String::new();
    s.push_row(&mut out);
    let cols: Vec<&str> = out.trim_matches(['[', ']']).split(',').collect();
    assert_eq!(cols.len(), KEYS.len(), "{}", out);
    let at = |k: &str| cols[KEYS.iter().position(|n| *n == k).unwrap()];
    assert_eq!(at("cpu_nr_periods"), "50");
    // cgroup が読めない環境では `null`
    let mut out = String::new();
    Sample::default().push_row(&mut out);
    assert!(out.ends_with(",null]"), "{}", out);
}

/// cgroup のユーザー空間とカーネル側は**増分**で窓に入り、畳むと足し合わせ、
/// 行の末尾 2 列に出る。`user_usec` の行が無ければ `null` (T16.0)。
#[test]
fn the_cgroup_user_and_system_time_are_deltas_at_the_end() {
    let raw = |t: u64, user: u64, system: u64| Latest {
        at: t,
        avail: SRC_CGROUP_CPU | SRC_CGROUP_USAGE,
        cpu_usage_usec: user + system,
        cpu_user_usec: user,
        cpu_system_usec: system,
        ..Latest::default()
    };
    let a = raw(1_000_000, 13_000_000, 18_000_000);
    let b = raw(1_000_005, 13_300_000, 18_700_000);
    let c = raw(1_000_010, 13_400_000, 18_800_000);
    let s1 = diff(&a, &b);
    assert_eq!((s1.cpu_user_usec, s1.cpu_system_usec), (300_000, 700_000));
    let s2 = diff(&b, &c);
    let agg = Sample::downsample(&[s1, s2], 1_000_000);
    assert_eq!((agg.cpu_user_usec, agg.cpu_system_usec), (400_000, 800_000));
    assert_eq!(
        agg.avail & SRC_CGROUP_USAGE,
        SRC_CGROUP_USAGE,
        "旗は 9 ビット目も残る"
    );
    let mut out = String::new();
    s1.push_row(&mut out);
    let cols: Vec<&str> = out.trim_matches(['[', ']']).split(',').collect();
    assert_eq!(cols.len(), KEYS.len(), "{}", out);
    assert_eq!(KEYS[KEYS.len() - 2..], ["cpu_user_usec", "cpu_system_usec"]);
    assert!(out.ends_with(",300000,700000]"), "{}", out);
    // 行の無いファイル (旗が立たない) は、絞りの列は数のまま、この 2 列だけ `null`
    let mut old = s1;
    old.avail = SRC_CGROUP_CPU;
    let mut out = String::new();
    old.push_row(&mut out);
    assert!(out.ends_with(",0,null,null]"), "{}", out);
}

/// 直近 5 分の絞りは `/healthz` と異常の規則が読む割合になる (T15.0)。
#[test]
fn the_five_minute_throttling_is_a_ratio() {
    assert_eq!(Health::default().cpu_throttled_ratio(), None, "読めない");
    let h = Health {
        cpu_throttled_5m: Some((45, 60)),
        ..Health::default()
    };
    assert_eq!(h.cpu_throttled_ratio(), Some(0.75));
    let zero = Health {
        cpu_throttled_5m: Some((0, 0)),
        ..Health::default()
    };
    assert_eq!(zero.cpu_throttled_ratio(), None, "分母 0 は割らない");
}

/// 読めない源の列は `null` で出る (Linux 以外・cgroup v1・`/proc/net` の無いコンテナ)。
#[test]
fn unreadable_sources_are_null_in_the_row() {
    let mut out = String::new();
    Sample {
        t: 1_770_000_000,
        ..Sample::default()
    }
    .push_row(&mut out);
    let cols: Vec<&str> = out.trim_matches(['[', ']']).split(',').collect();
    assert_eq!(cols.len(), KEYS.len(), "{}", out);
    assert_eq!(cols[0], "1770000000");
    // 時刻と「その窓のミスの回数」以外は全部 null
    for (k, c) in KEYS.iter().zip(&cols).skip(1) {
        let want = if *k == "dns_misses" { "0" } else { "null" };
        assert_eq!(*c, want, "{} が {}", k, c);
    }
}

/// 読めた源は数で出る (`time_wait` と `psi_cpu_some_avg10` が窓に出ること)。
#[test]
fn a_readable_row_has_the_time_wait_and_the_psi() {
    let s = diff(&raw(1_000_000, 100, 0), &raw(1_000_005, 101, 30_912));
    let mut out = String::new();
    s.push_row(&mut out);
    let cols: Vec<&str> = out.trim_matches(['[', ']']).split(',').collect();
    assert_eq!(cols.len(), KEYS.len());
    let at = |k: &str| cols[KEYS.iter().position(|n| *n == k).unwrap()];
    assert_eq!(at("listen_overflows"), "1");
    assert_eq!(at("time_wait"), "30912");
    assert_eq!(at("psi_cpu_some_avg10"), "43.02");
    assert_eq!(at("psi_io_some_avg10"), "null", "io の PSI は読めていない");
    assert_eq!(at("curr_estab"), "0");
}

/// 環は 5 秒 × 720 で頭打ちになり、1 分の窓は閉じたところで 1 本できる。
#[test]
fn the_rings_roll_up_and_stay_bounded() {
    let mut st = State::new();
    for i in 0..(RESOLUTIONS[0].1 as u64 + 20) {
        let s = diff(&raw(0, 0, 0), &raw(1_000_000 + i * 5, i, 1_000 + i));
        push(&mut st, s);
    }
    assert_eq!(st.rings[0].len(), RESOLUTIONS[0].1);
    // 740 標本 = 3,700 秒 ≈ 61 分ぶん
    assert!(
        (60..=62).contains(&st.rings[1].len()),
        "1 分の標本が {} 本",
        st.rings[1].len()
    );
    assert_eq!(
        st.rings[1].front().unwrap().t % 60,
        0,
        "窓の先頭は 60 秒境界"
    );
}

/// `/history` の `kernel` は `keys` と `samples` を持ち、3600 の解像度には無い。
#[test]
fn the_history_array_has_the_keys_and_no_hour_resolution() {
    let json = history_json(0);
    assert!(
        json.starts_with("{\"interval_secs\":5,\"keys\":[\"t\","),
        "{}",
        json
    );
    assert!(json.contains("\"time_wait\""), "{}", json);
    assert!(json.contains("\"psi_cpu_some_avg10\""), "{}", json);
    assert!(json.contains("\"samples\":["), "{}", json);
    assert!(history_json(1).starts_with("{\"interval_secs\":60,"));
    assert_eq!(history_json(2), "null");
}
