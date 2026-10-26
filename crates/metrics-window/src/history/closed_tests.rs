use super::*;
use crate::recent::{CLOSED_KEYS, CloseReason, RecentEntry, SIDES, STAGES};

fn sample(t: u64) -> Sample {
    Sample {
        t,
        ..Sample::default()
    }
}

fn closed(reason: CloseReason, secs: u64) -> RecentEntry {
    RecentEntry {
        id: 1,
        at: 1_700_000_000,
        client: "198.51.100.7".to_string(),
        target: "example.net:443".to_string(),
        connect: true,
        secs,
        requests: 0,
        up: 2048,
        down: 4096,
        reason,
        status: 0,
        parked_secs: 1,
        parks: 1,
        stage_ms: [0; STAGES],
        rtt_us: [0; SIDES],
        retrans: [0; SIDES],
        sni: None,
        syn_retrans: 0,
        stall_ms: [0; SIDES],
        spins: 0,
        half_closed: None,
    }
}

/// 5 秒の窓が閉じ、60 秒の窓はその足し合わせになること (境目は `/history` と同じ)。
#[test]
fn windows_close_on_the_same_boundaries_as_the_samples() {
    let w = ClosedWindows::new();
    // 5 秒にも 60 秒にも揃った時刻から始める (窓の始まりを読みやすくするため)
    let t0 = 1_700_000_100;
    assert_eq!(t0 % 60, 0);
    // 最初の呼び出しは「今の窓」を決めるだけ (空の窓は残さない)
    w.roll(t0 + 2);
    assert_eq!(w.counts(), (0, 0, 0));

    w.observe(&closed(CloseReason::ClientEof, 3));
    w.observe(&closed(CloseReason::IdleTimeout, 301));
    w.roll(t0 + 7);
    assert_eq!(w.counts(), (1, 0, 2), "5 秒の窓が 1 つ閉じた");

    w.observe(&closed(CloseReason::ClientEof, 1));
    // 分をまたぐ (60 秒の窓も閉じる)
    w.roll(t0 + 62);
    let (fine, minute, total) = w.counts();
    assert_eq!((fine, minute, total), (2, 1, 3));

    let json = w.to_json_res(0);
    assert!(
        json.starts_with("{\"interval_secs\":5,\"keys\":[\"t\","),
        "{}",
        json
    );
    assert!(json.contains("\"reasons\":[\"client_eof\","), "{}", json);
    assert!(
        json.contains("\"life_bounds_secs\":[1,2,5,10,15,"),
        "{}",
        json
    );
    assert!(json.contains("\"byte_bounds\":[1024,4096,"), "{}", json);
    assert!(json.contains("\"windows\":2"), "{}", json);
    assert!(json.contains("\"recorded\":3"), "{}", json);
    // 窓の始まりは 5 秒に丸めた時刻
    assert!(json.contains("\"samples\":[[1700000100,2,"), "{}", json);
    let minute_json = w.to_json_res(1);
    assert!(
        minute_json.contains("\"interval_secs\":60"),
        "{}",
        minute_json
    );
    assert!(minute_json.contains("[1700000100,3,"), "{}", minute_json);
    // 1 時間の解像度では残していない
    assert_eq!(w.to_json_res(2), "null");
    assert_eq!(CLOSED_KEYS[0], "t");
}

/// 件数 0 の窓は残さない (43 本/時 のプロキシで 719 本のゼロ行を作らない)。
#[test]
fn empty_windows_are_not_kept() {
    let w = ClosedWindows::new();
    for i in 0..100u64 {
        w.roll(1_700_000_000 + i * 5);
    }
    assert_eq!(w.counts(), (0, 0, 0));
    assert!(w.to_json_res(0).contains("\"samples\":[]"));
}

/// 窓は 5 秒 × 720 と 60 秒 × 1,440 で頭打ち。
#[test]
fn the_windows_are_capped() {
    let w = ClosedWindows::new();
    for i in 0..(RESOLUTIONS[0].1 as u64 + 50) {
        w.observe(&closed(CloseReason::ClientEof, 1));
        w.roll(1_700_000_000 + (i + 1) * 5);
    }
    let (fine, minute, total) = w.counts();
    assert_eq!(fine, RESOLUTIONS[0].1, "5 秒の窓は 720 で頭打ち");
    assert_eq!(total, RESOLUTIONS[0].1 as u64 + 50);
    assert!(minute > 0 && minute <= RESOLUTIONS[1].1);
}

/// `/history` は標本の**後ろに別の配列**として出す (既存の読み方を変えない)。
#[test]
fn history_appends_the_closed_windows_after_the_samples() {
    let h = History::default();
    h.push(sample(1_700_000_000));
    h.closed.roll(1_700_000_000);
    h.closed.observe(&closed(CloseReason::Evicted, 7));
    h.closed.roll(1_700_000_005);
    let json = h.to_json_res(0);
    // 既存の形はそのまま (先頭の版だけが増えた。T14.49)
    assert!(
        json.starts_with("{\"schema\":1,\"interval_secs\":5,\"keys\":[\"t\","),
        "{}",
        json
    );
    let samples_at = json.find("\"samples\":").expect("標本がある");
    let closed_at = json.find("\"closed\":").expect("分布がある");
    assert!(samples_at < closed_at, "分布は標本の後ろ");
    assert!(
        json.contains("\"closed\":{\"interval_secs\":5,"),
        "{}",
        json
    );
    assert!(json.ends_with("}}"), "{}", &json[json.len() - 40..]);
    // 1 時間の解像度は `null`。**末尾で見ない**: T14.10 の `canary` がこのうしろに
    // 付くので、`ends_with` で見ると「別の配列を足したら落ちるテスト」になる
    let hour = h.to_json_res(2);
    assert!(hour.contains(",\"closed\":null"), "{}", hour);
    assert!(hour.ends_with("}"), "{}", hour);
}

/// 速さと半閉じの窓 (T14.25) は `closed` の**隣**に出る (どちらも標本の後ろ)。
#[test]
fn history_appends_the_transfer_windows_next_to_the_closed_ones() {
    let h = History::default();
    h.push(sample(1_700_000_000));
    h.transfer.roll(1_700_000_000);
    h.transfer.observe(
        1 << 20,
        std::time::Duration::from_secs(1),
        Some(std::time::Duration::from_millis(120)),
        [0; SIDES],
    );
    h.transfer.roll(1_700_000_005);
    let json = h.to_json_res(0);
    let closed_at = json.find("\"closed\":").expect("閉じた接続の分布がある");
    let transfer_at = json.find("\"transfer\":").expect("速さの分布がある");
    assert!(closed_at < transfer_at, "transfer は closed の後ろ");
    assert!(
        json.contains(",\"transfer\":{\"interval_secs\":5,"),
        "{}",
        &json[transfer_at..]
    );
    assert!(
        json.contains("[1700000000,1,1,["),
        "{}",
        &json[transfer_at..]
    );
    // 1 時間の解像度は `closed` と同じく `null`
    let hour = h.to_json_res(2);
    assert!(hour.contains(",\"transfer\":null"), "{}", hour);
}

/// 窓が埋まったときの大きさ (1 窓 ≈ 300 B。`/history` が太る分をここで押さえておく)。
///
/// デプロイ先は 43 本/時 なので、ほとんどの窓は空で**出さない**。ここで見るのは
/// 「忙しいプロキシで 5 秒 × 720 が全部埋まったとき」の上限で、標本 (`samples`) と
/// 同じ桁に収まっていること。60 秒 × 1,440 は同じ 1 行の 2 倍の本数になる。
#[test]
fn a_full_ring_of_closed_windows_stays_the_same_order_as_the_samples() {
    let w = ClosedWindows::new();
    let mut e = closed(CloseReason::ClientEof, 1234);
    e.up = 987_654_321;
    e.down = 12_345_678_901;
    e.parked_secs = 300;
    for i in 0..(RESOLUTIONS[0].1 as u64 + 5) {
        for _ in 0..200 {
            w.observe(&e);
        }
        w.roll(1_700_000_000 + (i + 1) * 5);
    }
    let json = w.to_json_res(0);
    let windows = json.matches("[1700").count();
    assert_eq!(windows, RESOLUTIONS[0].1, "{} 窓", windows);
    assert!(json.len() <= 512 * 1024, "res=5 が {} B", json.len());
    println!(
        "closed res=5 が満杯のとき: {} B ({} 窓 = 1 窓 {} B。res=60 は 1,440 窓)",
        json.len(),
        windows,
        json.len() / windows
    );
}

/// 窓が空なら `/history` に足すのは区間の定義ぶんだけで、上限は変わらない。
///
/// 見ているのは標本 (`samples`) の**後ろに付く別の配列**の合計
/// (`closed` = 482 B + `transfer` = 約 400 B + `canary`)。T14.25 で `transfer` が
/// 増えたので上限を 600 → 1,200 B にした (それでも 720 標本の本体に対して 0.3%)。
#[test]
fn empty_windows_barely_grow_the_history_response() {
    let h = History::default();
    for i in 0..(DEFAULT_N as u64) {
        h.push(sample(1_700_000_000 + i * 5));
    }
    let json = h.to_json_res(0);
    assert!(json.len() <= 512 * 1024, "{} B", json.len());
    let tail = &json[json.find("\"closed\":").unwrap()..];
    assert!(tail.len() < 1_200, "空の窓で {} B", tail.len());
    println!("空の窓が /history に足す分: {} B", tail.len());
}
