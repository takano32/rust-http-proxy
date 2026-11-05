//! 接続の 1 枠の単体試験 (T.34.2 で `conns.rs` の `conn_tests` から移した。名前はそのまま)。

use super::*;
use crate::recent::{
    ConnTable, RecentRing, STAGE_CLIENT_READ, STAGE_CONNECT, STAGE_DNS, STAGE_FIRST_RELAY,
    STAGE_QUEUE,
};

/// `poll` の旗は `IN|HUP` のように表の順の名前で並べ、下位 16 bit がクライアント側・上位が
/// オリジン側 (T15.0 (4)。T.34.2 で直に押さえる)。
#[test]
fn revents_are_packed_per_side_and_named_in_order() {
    let packed = pack_revents(0x001 | 0x010, 0x004);
    assert_eq!(revents_names(packed, CLIENT_SIDE), "IN|HUP");
    assert_eq!(revents_names(packed, ORIGIN_SIDE), "OUT");
    assert_eq!(revents_names(pack_revents(0, 0), CLIENT_SIDE), "");
    // 表に無い bit は名前にならず、負の値 (全部の bit が立った i16) も崩れない
    assert_eq!(
        revents_names(pack_revents(0x020 | 0x100, 0), CLIENT_SIDE),
        "NVAL"
    );
    assert_eq!(
        revents_names(pack_revents(-1, -1), ORIGIN_SIDE),
        "IN|OUT|ERR|HUP|NVAL"
    );
}

/// 半閉じは「側 + 1」を上位 8 bit、接続を受けてからの ms を下位 56 bit に詰め、`0` は「まだ」
/// (T15.0 (4)。T.34.2 で直に押さえる)。
#[test]
fn half_closed_packs_the_side_and_the_ms_and_zero_means_not_yet() {
    assert_eq!(unpack_half_closed(0), None);
    assert_eq!(
        unpack_half_closed(pack_half_closed(CLIENT_SIDE, 0)),
        Some((CLIENT_SIDE, 0))
    );
    assert_eq!(
        unpack_half_closed(pack_half_closed(ORIGIN_SIDE, 123_456)),
        Some((ORIGIN_SIDE, 123_456))
    );
    // ms は 56 bit で頭打ち (側の bit を壊さない)
    assert_eq!(
        unpack_half_closed(pack_half_closed(ORIGIN_SIDE, u64::MAX)),
        Some((ORIGIN_SIDE, HALF_CLOSED_MS_MASK))
    );
}

/// 閉じた接続の個票が 1 件ずつ残ること (`/recent`。T14.4)。
#[test]
fn closing_a_connection_leaves_one_entry() {
    let t = ConnTable::new();
    let ring = RecentRing::new();
    let slot = t.register(7, "198.51.100.7", Instant::now()).unwrap();
    slot.begin_tunnel("mtalk.google.com:5228");
    slot.on_park();
    slot.on_unpark();
    slot.finish(
        CloseReason::ClientEof,
        ConnTally {
            up: 4096,
            down: 65536,
            status: 0,
            stage_ms: [3, 9, 0, 0, 0, 0],
            // カーネルの RTT: 利用者は 48 ms、宛先は 30 ms (T14.5)
            rtt_us: [48_300, 30_100],
            retrans: [0, 0],
            // 確立まで SYN を 1 回送り直した (= 1 秒待たされた接続。T14.46)
            syn_retrans: 1,
            // 中継の詰まり: クライアントへ書けずに 1,800 ms 待った (T14.42)
            stall_ms: [1_800, 0],
        },
        0,
    );
    let got = t.unregister(7).expect("枠が返る");
    ring.push(
        got.closed_entry(Instant::now())
            .expect("宛先のある接続は残る"),
    );
    assert!(t.is_empty());
    assert!(t.unregister(7).is_none(), "2 回目は何も返らない");

    let (rows, total) = ring.select(0, "");
    assert_eq!(total, 1);
    assert_eq!(rows.len(), 1);
    let e = &rows[0];
    assert_eq!(e.id, 7);
    assert_eq!(e.client, "198.51.100.7");
    assert_eq!(e.target, "mtalk.google.com:5228");
    assert!(e.connect);
    assert_eq!(e.up, 4096);
    assert_eq!(e.down, 65536);
    assert_eq!(e.bytes(), 4096 + 65536);
    assert_eq!(e.reason, CloseReason::ClientEof);
    assert_eq!(e.connect_ms(), 9);
    assert_eq!(e.parks, 1);
    let json = e.to_json();
    assert!(json.contains("\"reason\":\"client_eof\""), "{}", json);
    assert!(json.contains("\"kind\":\"connect\""), "{}", json);
    assert!(
        json.contains("\"ms\":{\"dns\":3,\"connect\":9}"),
        "{}",
        json
    );
    // カーネルの RTT と再送は両側とも必ず出る (T14.5)
    assert_eq!(e.rtt_us, [48_300, 30_100]);
    assert!(
        json.contains("\"rtt_ms\":{\"client\":48.3,\"origin\":30.1}"),
        "{}",
        json
    );
    assert!(
        json.contains("\"retrans\":{\"client\":0,\"origin\":0}"),
        "{}",
        json
    );
    // T14.3 が埋める段階はまだ 0 なので 1 バイトも出さない
    assert!(!json.contains("queue"), "{}", json);
    // 空回りと半閉じ (T15.0 (4)) は、`/connections` と違ってここでは 0 でも `null` でも
    // 必ず出す (件数で切る口なので「欄が無い」= 古い版と区別させるため)
    assert!(json.contains("\"spins\":0"), "{}", json);
    assert!(
        json.contains("\"half_closed\":null,\"half_closed_ms\":null"),
        "{}",
        json
    );
    // 上限は欄が増えたぶん引き上げてある (T14.5 の `rtt_ms` / `retrans`、T14.26 の
    // 向き別、T14.46 の `syn_retrans` で 363 B、T15.0 (4) の 3 欄で +51 B = 実測 414 B)
    assert!(json.len() <= 450, "ありふれた 1 件が {} B", json.len());
    println!("closed entry: typical {} B\n  {}", json.len(), json);
}

/// 閉じた理由は**先に書いた方が勝つ** (T13.2 の追い出しが中継の終わり方に負けない)。
#[test]
fn the_first_close_reason_wins() {
    let t = ConnTable::new();
    let slot = t.register(1, "127.0.0.1", Instant::now()).unwrap();
    slot.begin_tunnel("example.net:443");
    slot.set_close(CloseReason::Evicted);
    slot.finish(CloseReason::ClientEof, ConnTally::default(), 0);
    let e = slot
        .closed_entry(Instant::now())
        .expect("宛先のある接続は残る");
    assert_eq!(e.reason, CloseReason::Evicted);
    assert_eq!(e.reason.text(), "evicted");
    // 何も書かなければ「プロキシ側の都合」
    let other = t.register(2, "127.0.0.1", Instant::now()).unwrap();
    other.begin_tunnel("example.net:443");
    assert_eq!(
        other.closed_entry(Instant::now()).unwrap().reason,
        CloseReason::Shutdown
    );
    // 宛先を 1 つも選ばなかった接続 (自分宛ての `/status` など) は残さない
    let local = t.register(3, "127.0.0.1", Instant::now()).unwrap();
    assert!(local.closed_entry(Instant::now()).is_none());
}

/// 桁を振り切った 1 件でも 760 B に収まること (**リングの大きさの上限**)。
///
/// 1 件の目安は 300 B (T14.5 のカーネルの RTT 2 側ぶんで 256 B から上がった)。
/// ここで見るのは「起こりえない桁 (転送 20 桁、段階の ms が 6 つとも 7 桁、
/// RTT と再送が 4,294,967,295、T15.0 (4) の `spins` が 20 桁) を並べても
/// リングが 2,000 × 760 B = 1,484 KiB を越えない」ことだけで、
/// `/recent?n=2000` の応答は 256 KiB のバイト数打ち切りに当たるのが設計どおり
/// (`crates/endpoints` のテストで見る)。
#[test]
fn the_worst_closed_entry_stays_small() {
    let slot = ConnSlot::new(
        u64::MAX,
        "2001:0db8:0000:0000:0000:ff00:0042:8329%enp0s31f6xx",
        Instant::now(),
    );
    slot.begin_tunnel(&"sub.".repeat(40));
    slot.on_park();
    // トンネル 1 本の証拠も最悪の値で (T15.0 (4))
    slot.set_relaying(u64::MAX, u64::MAX, pack_revents(0x03d, 0x03d));
    slot.set_half_closed(ORIGIN_SIDE);
    slot.finish(
        CloseReason::KeepaliveTimeout,
        ConnTally {
            up: u64::MAX,
            down: u64::MAX,
            status: 599,
            stage_ms: [u64::MAX; STAGES],
            rtt_us: [u32::MAX; SIDES],
            retrans: [u32::MAX; SIDES],
            syn_retrans: u8::MAX,
            stall_ms: [u32::MAX; SIDES],
        },
        u32::MAX,
    );
    let e = slot
        .closed_entry(Instant::now())
        .expect("宛先のある接続は残る");
    assert!(e.target.len() <= MAX_RECENT_TARGET, "{}", e.target.len());
    assert!(e.client.len() <= MAX_CLIENT, "{}", e.client.len());
    let json = e.to_json();
    // 上限は欄が増えたぶん引き上げてある (T14.55 で 633 B、T15.0 (4) の
    // `spins` / `half_closed` / `half_closed_ms` で +71 B = 実測 704 B)
    assert!(json.len() <= 760, "最悪の 1 件が {} B", json.len());
    println!("closed entry: worst {} B", json.len());
}

/// トンネル 1 本の証拠 (`tid` / `spins` / `revents` / 半閉じ / 暇な秒) が枠に入り、
/// `/connections` の 1 行と個票の両方に出ること (T15.0 (4))。
///
/// **既定のままの行には 1 バイトも足さない**ことも一緒に縛る: `/connections` は
/// 「256 KiB に 1,000 本」の口で、実測の余りは 1 行あたり 4.7 B しか無い。
#[test]
fn a_tunnel_slot_carries_the_evidence_of_a_spin() {
    let slot = ConnSlot::new(7, "198.51.100.9", Instant::now());
    slot.begin_tunnel("mtalk.google.com:5228");
    let now = Instant::now();

    // まだ何も起きていない行は、欄を足す前と 1 バイトも変わらない
    let plain = slot.to_json(now);
    for key in ["tid", "spins", "revents", "half_closed", "idle_secs"] {
        assert!(
            !plain.contains(&format!("\"{}\"", key)),
            "既定の行に {} が出ている: {}",
            key,
            plain
        );
    }

    // 中継が待ちに入るたびに置く 3 つ。旗はクライアント側に `POLLERR | POLLHUP` が
    // 立ったまま = 空回りの型 (T15.5 が消した形)
    slot.set_tid(4242);
    slot.set_relaying(9_000, 12_345, pack_revents(0x008 | 0x010, 0));
    slot.set_half_closed(CLIENT_SIDE);
    let json = slot.to_json(now);
    assert!(json.contains("\"tid\":4242"), "{}", json);
    assert!(json.contains("\"spins\":12345"), "{}", json);
    assert!(
        json.contains("\"revents\":{\"client\":\"ERR|HUP\",\"origin\":\"\"}"),
        "{}",
        json
    );
    assert!(json.contains("\"half_closed\":\"client\""), "{}", json);
    assert!(json.contains("\"half_closed_secs\":0"), "{}", json);
    assert!(!json.contains("\"idle_secs\""), "まだ動いている: {}", json);

    // 暇な秒を数えるのは history スレッドだけ (同じ `bytes` の周期を足す)
    assert_eq!(slot.sweep_rate(1000), 9_000, "1 周期で 9,000 B 動いた");
    slot.sweep_rate(1000);
    slot.sweep_rate(1000);
    let json = slot.to_json(now);
    assert!(json.contains("\"idle_secs\":2"), "{}", json);
    assert!(json.contains("\"rate_bps\":0"), "動いていない: {}", json);

    // 個票にも同じ 2 つが移る (`half_closed_ms` は半閉じから閉じるまで)
    let e = slot
        .closed_entry(Instant::now())
        .expect("宛先のある接続は残る");
    assert_eq!(e.spins, 12_345);
    let (side, ms) = e.half_closed.expect("半閉じしている");
    assert_eq!(side, CLIENT_SIDE);
    assert!(ms < 60_000, "半閉じからの ms が大きすぎる: {}", ms);
    let j = e.to_json();
    assert!(j.contains("\"spins\":12345"), "{}", j);
    assert!(j.contains("\"half_closed\":\"client\""), "{}", j);
    assert!(j.contains(&format!("\"half_closed_ms\":{}", ms)), "{}", j);
}

/// 半閉じは**先に EOF を出した側**だけを残す (1 本につき 1 回。T15.0 (4))。
/// http の行には `idle_secs` を出さない (`bytes` を置くのはトンネルだけ)。
#[test]
fn the_first_half_close_wins_and_http_rows_have_no_idle_secs() {
    let slot = ConnSlot::new(1, "10.0.0.1", Instant::now());
    slot.begin_tunnel("example.net:443");
    slot.set_half_closed(ORIGIN_SIDE);
    slot.set_half_closed(CLIENT_SIDE);
    assert!(
        slot.to_json(Instant::now())
            .contains("\"half_closed\":\"origin\""),
        "先に書いた方が勝つ"
    );

    let http = ConnSlot::new(2, "10.0.0.2", Instant::now());
    http.set_first_target("example.net:80");
    http.sweep_rate(1000);
    http.sweep_rate(1000);
    let json = http.to_json(Instant::now());
    assert!(json.contains("\"kind\":\"http\""), "{}", json);
    assert!(
        !json.contains("idle_secs"),
        "http の行には出さない: {}",
        json
    );
    // 半閉じしていない接続の個票は `null` (「欄が無い」と区別させる)
    let e = http
        .closed_entry(Instant::now())
        .expect("宛先のある接続は残る");
    assert_eq!(e.half_closed, None);
    let j = e.to_json();
    assert!(j.contains("\"spins\":0"), "{}", j);
    assert!(
        j.contains("\"half_closed\":null,\"half_closed_ms\":null"),
        "{}",
        j
    );
}

/// keep-alive の HTTP 接続は記述子 1 本・宛先なし。
#[test]
fn a_keepalive_connection_shows_one_descriptor() {
    let t = ConnTable::new();
    let slot = t.register(7, "10.0.0.1", Instant::now()).unwrap();
    slot.set_state(ConnState::Reading);
    let json = slot.to_json(Instant::now());
    assert!(json.contains("\"kind\":\"http\""), "{}", json);
    assert!(json.contains("\"state\":\"reading\""), "{}", json);
    assert!(json.contains("\"target\":\"\""), "{}", json);
    assert!(json.contains("\"fds\":1"), "{}", json);
    assert!(json.contains("\"id\":7"), "{}", json);
}

/// T14.3 が足した 3 つの段階 (`queue` / `client_read` / `first_relay`) が個票に出ること。
///
/// `dns` と `connect` は必ず出て、残りは 0 でないときだけ出る (0 の段階で行を伸ばさない)。
#[test]
fn the_three_stages_from_the_profile_show_up_in_the_record() {
    let t = ConnTable::new();
    let slot = t.register(3, "198.51.100.7", Instant::now()).expect("枠");
    slot.begin_tunnel("a.example.net:443");
    let mut tally = ConnTally::default();
    let mut ms = [0u64; STAGES];
    ms[STAGE_DNS] = 6;
    ms[STAGE_CONNECT] = 9;
    ms[STAGE_QUEUE] = 159;
    ms[STAGE_CLIENT_READ] = 2;
    ms[STAGE_FIRST_RELAY] = 37;
    tally.add_request(0, 4, 4, ms, 0);
    slot.finish(CloseReason::ClientEof, tally, 0);
    let json = t
        .unregister(3)
        .expect("枠が返る")
        .closed_entry(Instant::now())
        .expect("宛先のある接続は残る")
        .to_json();
    for (name, v) in [
        ("dns", 6),
        ("connect", 9),
        ("queue", 159),
        ("client_read", 2),
        ("first_relay", 37),
    ] {
        assert!(
            json.contains(&format!("\"{}\":{}", name, v)),
            "{} が出ていない: {}",
            name,
            json
        );
    }
    // 0 の段階 (`first_byte`) は出さない
    assert!(!json.contains("\"first_byte\""), "{}", json);
}

/// 開いた時刻は「閉じた時刻 − 寿命」(T.34.48。時刻を注入して秒まで縛る)。
#[test]
fn closed_entry_at_dates_the_entry_from_the_given_epoch() {
    let t = ConnTable::new();
    let opened = Instant::now();
    let slot = t.register(4, "198.51.100.4", opened).unwrap();
    slot.begin_tunnel("example.net:443");
    let e = slot
        .closed_entry_at(
            opened + std::time::Duration::from_millis(90_500),
            1_800_000_000,
        )
        .expect("宛先のある接続は残る");
    assert_eq!(e.at, 1_800_000_000 - 90, "寿命の秒 (切り捨て) を引く");
    assert_eq!(e.secs, 90);
    // 寿命が時刻より長くても 0 で止まる
    let e = slot
        .closed_entry_at(opened + std::time::Duration::from_secs(30), 10)
        .unwrap();
    assert_eq!(e.at, 0);
}
