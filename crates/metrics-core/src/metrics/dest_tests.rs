//! `/slo` が外す「443・80 宛てのエラーのいちばん多い宛先」と、満杯のホスト表の試験
//! (T34.33 で `latency_tests.rs` から移した)。

use super::*;

/// 時間の区切りの直前なら次の時間まで待つ (同じ時間の中で数えたいテスト用。T30.4)。
fn away_from_the_hour_boundary() -> u64 {
    let now = crate::cache::now_epoch();
    if now % 3600 >= 3590 {
        std::thread::sleep(Duration::from_secs(3600 - now % 3600 + 1));
    }
    crate::cache::now_epoch()
}

/// **T30.4**: 443・80 宛てのエラーのいちばん多い宛先を、ホスト別統計の行で時間ごとに数える。
///
/// 見るのは: (a) エラーの合計は名前解決に成功した 443・80 宛てだけ (`:993`・原因 `dns` は入らない)、
/// (b) いちばん多い宛先の試行 = その時間の窓に入った成功 + エラー (成功は区切りで写した `timed` からの差。
/// 読み戻した行の通算は入らない)、(c) 時間を越えると前の時間が確定し、いまの時間は 0 から。
#[test]
fn the_destination_with_the_most_web_errors_is_counted_per_hour() {
    let m = Metrics::new();
    let took = Some(Duration::from_millis(5));
    let err = |cause| Detail {
        cause: Some(cause),
        ..Detail::default()
    };
    let ok = Detail::default;
    // 読み戻した行 (前の起動の通算 100 本。この時間の成功には入らない)
    m.restore(
        vec![(
            "connect://pravda.example:443".to_string(),
            HostStats {
                requests: 100,
                timed: 100,
                ..HostStats::default()
            },
        )],
        Vec::new(),
    );
    let now = away_from_the_hour_boundary();
    let hour = now - now % 3600;
    // 履歴スレッドの最初の 1 回 (ここで区切りの写しを取る)
    let (prev, cur) = m.web_error_hours(now);
    assert_eq!(prev, None);
    assert_eq!(
        (cur.hour, cur.errors, cur.destination.as_str()),
        (hour, 0, "")
    );
    let www = "connect://www.example:443";
    let pravda = "connect://pravda.example:443";
    for _ in 0..10 {
        m.record_host_detail(www, HostOutcome::Bypass, 0, took, ok());
    }
    for _ in 0..2 {
        m.record_host_detail(www, HostOutcome::Error, 0, took, err(ErrCause::Refused));
    }
    m.record_host_detail(pravda, HostOutcome::Bypass, 0, took, ok());
    for _ in 0..3 {
        m.record_host_detail(pravda, HostOutcome::Error, 0, took, err(ErrCause::Timeout));
    }
    // (a) 入らないもの
    m.record_host_detail(pravda, HostOutcome::Error, 0, took, err(ErrCause::Dns));
    for _ in 0..5 {
        let mail = "connect://mail.example:993";
        m.record_host_detail(mail, HostOutcome::Error, 0, took, err(ErrCause::Timeout));
    }
    // 応答時間の無い `:80` のエラー (合計に入る)
    let b = "http://b.example:80";
    m.record_host_detail(b, HostOutcome::Error, 0, None, err(ErrCause::Reset));
    let (prev, cur) = m.web_error_hours(now);
    assert_eq!(prev, None);
    assert_eq!(
        cur,
        DestHour {
            hour,
            errors: 2 + 3 + 1,
            destination: pravda.to_string(),
            top_errors: 3,
            // (b) 成功 1 + エラー 3 (名前解決の失敗と、読み戻した 100 本は入らない)
            top_attempts: 4,
        }
    );
    // (c) 次の時間: 前の時間が確定し、いまの時間は 0 から
    let (prev, cur) = m.web_error_hours(hour + 3600);
    assert_eq!(prev.as_ref().map(|d| d.top_attempts), Some(4));
    assert_eq!(prev.map(|d| d.errors), Some(6));
    assert_eq!((cur.hour, cur.errors, cur.top_errors), (hour + 3600, 0, 0));
}

/// **T32.10**: ホスト別統計が満杯のとき、新しい鍵は `last_seen` のいちばん古い行を 1 つ捨てて入る
/// (前は全部 `other` に畳まれた)。捨てた行の通算は `other` に足し、捨てた数は `hosts_evicted` に出る。
/// 空きがあるうちは何も捨てない。
#[test]
fn a_full_host_table_drops_the_oldest_row_for_a_new_key() {
    let m = Metrics::new();
    let now = crate::cache::now_epoch();
    // 1,000 行が埋まった表 (どの行も 14 日前が最後。h0 だけ 20 日前)
    let rows = (0..MAX_HOSTS)
        .map(|i| {
            let age = if i == 0 { 20 } else { 14 } * 86_400;
            let s = HostStats {
                requests: 2,
                last_seen: now - age,
                errors_by_cause: [1, 0, 0, 0, 0, 0, 0, 0],
                dns_by_cause: [1, 0],
                ..HostStats::default()
            };
            (format!("connect://h{i}.example:443"), s)
        })
        .collect();
    m.restore(rows, Vec::new());
    let nx_before = m.dns_cause_totals();
    m.record_host("connect://new.example:443", HostOutcome::Bypass, 0);
    let hosts = m.hosts_sorted();
    assert_eq!(hosts.len(), MAX_HOSTS, "表の大きさは変わらない");
    let names: Vec<&str> = hosts.iter().map(|(h, _)| h.as_str()).collect();
    assert!(
        names.contains(&"connect://new.example:443"),
        "新しい鍵が行を持つ"
    );
    assert!(
        !names.contains(&"connect://h0.example:443"),
        "いちばん古い行を捨てる"
    );
    // `other` の行を作るぶんもう 1 行捨てる (1,000 行に収める)
    assert_eq!(m.hosts_evicted(), 2);
    let other = &hosts.iter().find(|(h, _)| h == "other").expect("other").1;
    assert_eq!(other.requests, 4, "捨てた 2 行の通算は other に足す");
    assert_eq!(m.dns_cause_totals(), nx_before, "表を舐めた合計は減らない");
    // 既にある鍵は何も捨てない
    m.record_host("connect://new.example:443", HostOutcome::Bypass, 0);
    assert_eq!(m.hosts_evicted(), 2);
    // 次の新しい鍵は 1 行だけ捨てる (`other` は捨てない)
    m.record_host("connect://new2.example:443", HostOutcome::Bypass, 0);
    assert_eq!(m.hosts_evicted(), 3);
    assert_eq!(m.hosts_sorted().len(), MAX_HOSTS);
    assert!(
        m.to_json()
            .contains(",\"hosts_evicted\":3,\"silent_closes\":0}"),
        "`/status` の末尾に出る"
    );
}

/// **T34.145**: 満杯の表で `last_seen` が同着なら、捨てる行は名前の順で決まる (前は HashMap の
/// 順で、実行ごとに違う行を捨てた)。同じ `last_seen` の 1,000 行に新しい鍵を 1 件入れると、
/// 名前のいちばん小さい 2 行 (新しい鍵のぶんと、`other` の行を作るぶん) が捨てられる。
#[test]
fn a_tie_on_last_seen_drops_the_rows_in_name_order() {
    let m = Metrics::new();
    let now = crate::cache::now_epoch();
    let rows = (0..MAX_HOSTS)
        .map(|i| {
            let s = HostStats {
                requests: 1,
                last_seen: now - 86_400,
                ..HostStats::default()
            };
            (format!("connect://h{i:04}.example:443"), s)
        })
        .collect();
    m.restore(rows, Vec::new());
    m.record_host("connect://new.example:443", HostOutcome::Bypass, 0);
    assert_eq!(m.hosts_evicted(), 2);
    let names: Vec<String> = m.hosts_sorted().into_iter().map(|(h, _)| h).collect();
    for gone in ["connect://h0000.example:443", "connect://h0001.example:443"] {
        assert!(!names.iter().any(|h| h == gone), "{gone} が残った");
    }
    assert!(names.iter().any(|h| h == "connect://h0002.example:443"));
}

/// **T32.10**: `/slo` の宛先は、ホスト別統計から切り離した小さな表で数える。満杯の表から
/// `other` に畳まれる宛先 (捨てられる行が無いとき) でも、その時間のエラーの宛先として数える。
#[test]
fn web_errors_count_their_destination_even_when_the_host_table_cannot_take_it() {
    let m = Metrics::new();
    let now = away_from_the_hour_boundary();
    m.web_error_hours(now);
    let took = Some(Duration::from_millis(5));
    // 満杯の表 (どの行も今使われている時系列の枠を持つ = 捨てられない。`other` に畳まれる形)
    let rows = (0..MAX_HOSTS)
        .map(|i| {
            let s = HostStats {
                requests: 1,
                last_seen: now,
                series_slot: Some(0),
                ..HostStats::default()
            };
            (format!("connect://h{i}.example:443"), s)
        })
        .collect();
    m.restore(rows, Vec::new());
    let e = Detail {
        cause: Some(ErrCause::Timeout),
        ..Detail::default()
    };
    for _ in 0..3 {
        m.record_host_detail("connect://late.example:443", HostOutcome::Error, 0, took, e);
    }
    m.record_host_detail(
        "connect://late2.example:443",
        HostOutcome::Error,
        0,
        took,
        e,
    );
    assert_eq!(m.hosts_evicted(), 0, "捨てられる行が無い");
    assert!(m.hosts_sorted().iter().any(|(h, _)| h == "other"));
    let (_, cur) = m.web_error_hours(now);
    assert_eq!(
        (cur.errors, cur.destination.as_str(), cur.top_errors),
        (4, "connect://late.example:443", 3)
    );
    // 行が無いので成功は数えられない (試行 = エラーだけ)
    assert_eq!(cur.top_attempts, 3);
}

/// **T32.10**: 小さな表は 64 宛てまで。溢れた宛先は合計にだけ入る。
#[test]
fn the_destination_table_of_the_hour_holds_up_to_64() {
    let m = Metrics::new();
    let now = away_from_the_hour_boundary();
    m.web_error_hours(now);
    let took = Some(Duration::from_millis(5));
    let e = Detail {
        cause: Some(ErrCause::Refused),
        ..Detail::default()
    };
    for i in 0..web_dest::MAX_DESTS {
        let key = format!("connect://d{i}.example:443");
        m.record_host_detail(&key, HostOutcome::Error, 0, took, e);
    }
    // 65 個目は 2 件出しても宛先にはならない
    for _ in 0..2 {
        m.record_host_detail("connect://late.example:443", HostOutcome::Error, 0, took, e);
    }
    let (_, cur) = m.web_error_hours(now);
    assert_eq!(cur.errors, web_dest::MAX_DESTS as u64 + 2);
    assert_eq!(
        (cur.destination.as_str(), cur.top_errors),
        ("connect://d0.example:443", 1)
    );
}
