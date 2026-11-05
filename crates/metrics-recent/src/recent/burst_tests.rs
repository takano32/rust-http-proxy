//! 山の写真の単体試験 (T14.6。T34.2 で `recent/mod.rs` の `burst_tests` から移した)。

use super::*;
use crate::recent::{ConnState, ConnTable};
use std::time::Instant;

/// **閾を越えた瞬間の 1 本だけ**が旗を立て、同じ山の 2 本目以降は立てない。
#[test]
fn only_the_connection_that_crosses_raises_the_flag() {
    let ring = BurstRing::new();
    assert!(ring.armed());
    assert!(!ring.pending());
    // 閾 4 (上限 8 × 50%) を越えるのは 5 本目
    ring.request(5, 4, 8);
    assert_eq!(ring.take_pending(), Some(5), "越えた本数が旗に乗る");
    assert!(!ring.armed(), "撮ると旗は下りる");
    // 6〜8 本目は何も頼まない
    for n in 6..=8 {
        ring.request(n, 4, 8);
    }
    assert_eq!(ring.take_pending(), None, "同じ山では 1 枚だけ");
    assert_eq!(ring.threshold(), 4);
    assert_eq!(ring.max_conns(), 8);
}

/// 閾の 80% を下回るまでは次の 1 枚を撮らない。
#[test]
fn the_next_shot_waits_until_the_spike_is_over() {
    let ring = BurstRing::new();
    ring.request(5, 4, 8);
    assert_eq!(ring.take_pending(), Some(5));
    // 閾の 80% (4 × 0.8 = 3.2 本) 以上のあいだは戻らない
    for active in [8, 5, 4] {
        ring.rearm_if_calm(active);
        assert!(!ring.armed(), "{} 本ではまだ戻らない", active);
    }
    ring.rearm_if_calm(3);
    assert!(ring.armed(), "3 本 (< 3.2) まで減ったら次の 1 枚に備える");
    ring.request(5, 4, 8);
    assert_eq!(ring.take_pending(), Some(5), "2 枚目が撮れる");
}

/// まだ撮っていない旗を、山が引いたからといって消さない。
#[test]
fn a_pending_shot_is_not_lost_when_the_spike_passes() {
    let ring = BurstRing::new();
    ring.request(5, 4, 8);
    ring.rearm_if_calm(0);
    assert!(!ring.armed(), "頼んだ 1 枚を撮るまでは戻らない");
    assert_eq!(ring.take_pending(), Some(5));
    ring.rearm_if_calm(0);
    assert!(ring.armed());
}

/// 写真は `/connections` の表から作る (接続元・宛先・状態・種類の内訳)。
#[test]
fn a_shot_summarises_the_connection_table() {
    let t = ConnTable::new();
    let now = Instant::now();
    for i in 0..5u64 {
        let slot = t.register(i, "198.51.100.7", now).unwrap();
        slot.begin_tunnel("mtalk.google.com:5228");
        slot.set_state(ConnState::Parked);
    }
    let other = t.register(9, "203.0.113.9", now).unwrap();
    other.set_first_target("example.net:80");
    other.set_state(ConnState::Reading);

    let shot = BurstShot::take(&t.snapshot(), 1, 6, 5, 8, 4, 2, 7);
    assert_eq!(shot.seq, 1);
    assert_eq!(shot.active, 6);
    assert_eq!(shot.trigger_active, 5);
    assert_eq!(shot.max_conns, 8);
    assert_eq!(shot.threshold, 4);
    assert_eq!(shot.clients.len(), 2);
    assert_eq!(shot.clients[0], ("198.51.100.7".to_string(), 5), "多い順");
    assert_eq!(shot.clients_distinct, 2);
    assert_eq!(shot.targets[0], ("mtalk.google.com:5228".to_string(), 5));
    assert_eq!(shot.targets_distinct, 2);
    assert_eq!(shot.states[ConnState::Parked as usize], 5);
    assert_eq!(shot.states[ConnState::Reading as usize], 1);
    assert_eq!((shot.connects, shot.https), (5, 1));
    assert_eq!((shot.evicted_idle, shot.rejected_overload), (2, 7));
    let json = shot.to_json();
    assert!(json.contains("\"active\":6"), "{}", json);
    assert!(
        json.contains(
            "\"states\":{\"serving\":0,\"reading\":1,\"parked\":5,\"queued\":0,\"relaying\":0}"
        ),
        "{}",
        json
    );
    assert!(
        json.contains("\"kinds\":{\"connect\":5,\"http\":1}"),
        "{}",
        json
    );
    assert!(
        json.contains("{\"client\":\"198.51.100.7\",\"conns\":5}"),
        "{}",
        json
    );
    assert!(
        json.contains("{\"target\":\"mtalk.google.com:5228\",\"conns\":5}"),
        "{}",
        json
    );
    assert!(json.contains("\"evicted_idle\":2"), "{}", json);
    println!("burst shot: typical {} B", json.len());
}

/// 1 枚は 4 KiB 以内 (50 枚で 200 KiB。`/bursts` の 256 KiB に収まる大きさ)。
#[test]
fn one_shot_fits_in_4_kib() {
    let t = ConnTable::new();
    let now = Instant::now();
    // 接続元も宛先も上限いっぱいの長さで、載せる数より多い種類を作る
    for i in 0..(MAX_SHOT_CLIENTS as u64 + MAX_SHOT_TARGETS as u64 + 20) {
        let slot = t
            .register(
                i,
                &format!("2001:0db8:0000:0000:0000:ff00:0042:{:04x}%enp0s31f6xx", i),
                now,
            )
            .unwrap();
        // **通し番号は先頭に置く**: 宛先は 80 B で切られるので、末尾に置くと
        // 切られたあとが全部同じ名前になって「種類が 1 つ」になってしまう
        slot.begin_tunnel(&format!("{:03}{}example.net:65535", i, "sub.".repeat(20)));
    }
    let shot = BurstShot::take(
        &t.snapshot(),
        u64::MAX,
        usize::MAX,
        usize::MAX,
        4096,
        2048,
        u64::MAX,
        u64::MAX,
    );
    assert_eq!(shot.clients.len(), MAX_SHOT_CLIENTS);
    assert_eq!(shot.targets.len(), MAX_SHOT_TARGETS);
    assert!(
        shot.clients_other > 0 && shot.targets_other > 0,
        "残りは本数だけ"
    );
    let json = shot.to_json();
    assert!(json.len() <= 4096, "1 枚が {} B", json.len());
    println!("burst shot: worst {} B", json.len());
}

/// リングは 50 枚で頭打ち、新しい順に返る。
#[test]
fn the_burst_ring_keeps_the_newest_fifty() {
    let ring = BurstRing::new();
    let t = ConnTable::new();
    for i in 0..(MAX_BURSTS as u64 + 3) {
        assert_eq!(ring.next_seq(), i + 1);
        ring.push(BurstShot::take(&t.snapshot(), i + 1, 5, 5, 8, 4, 0, 0));
    }
    let (got, total) = ring.recent(MAX_BURSTS);
    assert_eq!(total, MAX_BURSTS as u64 + 3);
    assert_eq!(got.len(), MAX_BURSTS);
    assert_eq!(got[0].seq, MAX_BURSTS as u64 + 3, "新しい順");
    assert_eq!(got[MAX_BURSTS - 1].seq, 4, "最古は 50 枚前");
    assert_eq!(ring.recent(2).0.len(), 2);
}

/// 多い順に `max` 件まで取り、同数は名前の順で安定させ、残りは本数だけ (T34.2 で直に押さえる)。
#[test]
fn top_counts_keeps_the_busiest_and_sums_the_rest() {
    let rows = vec![
        ("b".to_string(), 2u32),
        ("a".to_string(), 2),
        ("c".to_string(), 5),
        ("d".to_string(), 1),
    ];
    let (top, other) = top_counts(rows.clone().into_iter(), 2);
    assert_eq!(
        top,
        vec![("c".to_string(), 5), ("a".to_string(), 2)],
        "同数は名前の順"
    );
    assert_eq!(other, 3, "残り (b 2 + d 1) は本数だけ");
    let (all, none) = top_counts(rows.into_iter(), 10);
    assert_eq!(all.len(), 4);
    assert_eq!(none, 0);
    let (empty, zero) = top_counts(std::iter::empty(), 3);
    assert!(empty.is_empty());
    assert_eq!(zero, 0);
}

/// 表を数える所 ([`ShotTally::of`]) は時計も `/proc` も触らない: 宛先の無い枠は宛先に数えず、
/// 状態と種類は枠の原子から読む (T34.2)。
#[test]
fn the_tally_counts_clients_targets_states_and_kinds_without_a_clock() {
    let t = ConnTable::new();
    let now = Instant::now();
    let a = t.register(1, "198.51.100.7", now).unwrap();
    a.begin_tunnel("a.example.net:443");
    let b = t.register(2, "198.51.100.7", now).unwrap();
    b.set_first_target("b.example.net:80");
    b.set_state(ConnState::Reading);
    // 宛先をまだ選んでいない http の接続 (自分宛ての `/status` など) は宛先に数えない
    let _c = t.register(3, "203.0.113.9", now).unwrap();
    let tally = ShotTally::of(&t.snapshot());
    assert_eq!(
        tally.clients,
        vec![
            ("198.51.100.7".to_string(), 2),
            ("203.0.113.9".to_string(), 1)
        ]
    );
    assert_eq!((tally.clients_distinct, tally.clients_other), (2, 0));
    assert_eq!(
        tally.targets,
        vec![
            ("a.example.net:443".to_string(), 1),
            ("b.example.net:80".to_string(), 1)
        ]
    );
    assert_eq!((tally.targets_distinct, tally.targets_other), (2, 0));
    assert_eq!(tally.states[ConnState::Relaying as usize], 1);
    assert_eq!(tally.states[ConnState::Reading as usize], 1);
    assert_eq!(tally.states[ConnState::Serving as usize], 1);
    assert_eq!((tally.connects, tally.https), (1, 2));
    let empty = ShotTally::of(&[]);
    assert!(empty.clients.is_empty() && empty.targets.is_empty());
    assert_eq!(empty.states, [0; CONN_STATES]);
}

/// 撮った時刻と `/proc` の数は渡した値がそのまま入る (T34.48)。
#[test]
fn take_at_records_the_given_time_and_process_counts() {
    let t = ConnTable::new();
    let slot = t.register(1, "198.51.100.1", Instant::now()).unwrap();
    slot.begin_tunnel("example.net:443");
    let shot = BurstShot::take_at(
        &t.snapshot(),
        3,
        1,
        1,
        8,
        4,
        5,
        6,
        1_800_000_007,
        (42, 900, 4096),
    );
    assert_eq!(shot.at, 1_800_000_007);
    assert_eq!((shot.threads, shot.fds, shot.max_fds), (42, 900, 4096));
    assert_eq!(
        (shot.seq, shot.evicted_idle, shot.rejected_overload),
        (3, 5, 6)
    );
    let json = shot.to_json();
    assert!(
        json.starts_with("{\"at\":1800000007,\"seq\":3,"),
        "{}",
        json
    );
    assert!(
        json.ends_with(",\"threads\":42,\"fds\":900,\"max_fds\":4096}"),
        "{}",
        json
    );
}
