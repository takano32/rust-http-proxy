//! 接続の表の単体試験 (T.34.2 で `conns.rs` の `conn_tests` から移した。名前はそのまま)。

use super::*;
use crate::recent::{CLIENT_SIDE, ConnState};

/// 直近の周期に `min_delta` 回以上空回りした接続を多い順 (同点は通し番号の順) に `max` 本まで、
/// 閾を越えた総数と一緒に返す (T15.0 (6)。T.34.2 で直に押さえる)。
#[test]
fn spinning_lists_the_worst_tunnels_first_and_counts_the_rest() {
    let t = ConnTable::new();
    let now = Instant::now();
    let a = t.register(1, "198.51.100.1", now).unwrap();
    let b = t.register(2, "198.51.100.2", now).unwrap();
    let c = t.register(3, "198.51.100.3", now).unwrap();
    let _quiet = t.register(4, "198.51.100.4", now).unwrap();
    for (slot, target) in [
        (&a, "a.example:443"),
        (&b, "b.example:443"),
        (&c, "c.example:443"),
    ] {
        slot.begin_tunnel(target);
    }
    t.update_rates(0); // 1 回目は控えるだけ
    a.set_relaying(0, 5_000, 0);
    b.set_relaying(0, 9_000, 0);
    c.set_relaying(0, 5_000, 0);
    t.update_rates(5_000);
    // 閾を越えた 3 本のうち上位 2 本: b (9,000)、次に a (5,000。同点の c より通し番号が小さい)
    let (top, total) = t.spinning(1_000, 2);
    assert_eq!(total, 3);
    assert_eq!(
        top.iter()
            .map(|s| (s.id, s.spins_delta))
            .collect::<Vec<_>>(),
        vec![(2, 9_000), (1, 5_000)]
    );
    assert_eq!(top[0].target, "b.example:443");
    assert_eq!(top[0].half_closed, None);
    // 閾の方が高ければ 1 本も出ない。半閉じの向きも 1 行に乗る
    assert_eq!(t.spinning(10_000, 1), (Vec::new(), 0));
    b.set_half_closed(CLIENT_SIDE);
    let (one, _) = t.spinning(9_000, 5);
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].half_closed, Some("client"));
    // 次の周期で回らなくなれば差分は 0 に戻る
    t.update_rates(5_000);
    assert_eq!(t.spinning(1, 5).1, 0);
}

#[test]
fn the_table_registers_and_unregisters_once() {
    let t = ConnTable::new();
    assert!(t.is_empty());
    let now = Instant::now();
    let a = t.register(1, "127.0.0.1", now).expect("登録できる");
    let _b = t.register(2, "::1", now).expect("登録できる");
    assert_eq!(t.len(), 2);
    // 古い順 (通し番号の順)
    let snap = t.snapshot();
    assert_eq!(snap[0].id, 1);
    assert_eq!(snap[1].id, 2);
    assert_eq!(snap[0].state(), ConnState::Serving);
    assert!(!snap[0].is_connect());

    // 状態と宛先は表の鍵を取らずに書ける
    a.begin_tunnel("example.com:443");
    a.set_bytes(4096);
    assert_eq!(t.snapshot()[0].state(), ConnState::Relaying);
    assert!(t.snapshot()[0].is_connect());
    let json = t.snapshot()[0].to_json(Instant::now());
    assert!(json.contains("\"kind\":\"connect\""), "{}", json);
    assert!(json.contains("\"state\":\"relaying\""), "{}", json);
    assert!(json.contains("\"target\":\"example.com:443\""), "{}", json);
    assert!(json.contains("\"bytes\":4096"), "{}", json);
    assert!(json.contains("\"fds\":2"), "{}", json);

    a.set_state(ConnState::Parked);
    assert_eq!(t.snapshot()[0].state(), ConnState::Parked);
    t.unregister(1);
    assert_eq!(t.len(), 1);
    t.unregister(1);
    assert_eq!(t.len(), 1, "2 回抹消しても増減しない");
}

/// いまの転送速度 (T14.39)。history スレッドの周期が `bytes` の差分を速さにする。
#[test]
fn sweeping_turns_the_byte_counter_into_a_rate() {
    let t = ConnTable::new();
    let now = Instant::now();
    let a = t.register(1, "198.51.100.7", now).expect("登録できる");
    let b = t.register(2, "198.51.100.8", now).expect("登録できる");
    a.begin_tunnel("a.example.net:443");
    b.begin_tunnel("b.example.net:443");

    // 1 回目は控えるだけ (割る幅が無い)。5 MiB 運んでいても速さは 0
    a.set_bytes(5 << 20);
    assert_eq!(t.update_rates(0), 0);
    assert_eq!(a.rate_bps(), 0);

    // 5 秒で +5 MiB → 1 MiB/s、動かなかった方は 0
    a.set_bytes(10 << 20);
    assert_eq!(t.update_rates(5000), 1 << 20);
    assert_eq!(a.rate_bps(), 1 << 20);
    assert_eq!(b.rate_bps(), 0);
    assert_eq!(t.rate_bps_total(), 1 << 20);

    // 和は全接続ぶん
    a.set_bytes((10 << 20) + (1 << 20));
    b.set_bytes(2 << 20);
    assert_eq!(t.update_rates(1000), (1 << 20) + (2 << 20));
    assert_eq!(t.rate_bps_total(), 3 << 20);

    // `/connections` の行の末尾に出る
    let json = t.snapshot()[0].to_json(Instant::now());
    assert!(
        json.ends_with(",\"fds\":2,\"rate_bps\":1048576}"),
        "{}",
        json
    );

    // 止まれば次の周期で 0 に戻る
    assert_eq!(t.update_rates(5000), 0);
    assert_eq!(a.rate_bps(), 0);
    assert_eq!(t.rate_bps_total(), 0);
}

/// `--lite` (記録しない) では枠ごと捨てるので和も 0 (T14.39)。
#[test]
fn the_rate_total_is_zero_without_the_table() {
    let t = ConnTable::new();
    let slot = t.register(1, "198.51.100.7", Instant::now()).expect("枠");
    slot.set_bytes(1 << 20);
    t.update_rates(1000);
    assert_eq!(t.rate_bps_total(), 1 << 20);
    t.set_enabled(false);
    assert_eq!(t.rate_bps_total(), 0);
    assert!(t.register(2, "198.51.100.8", Instant::now()).is_none());
    assert_eq!(t.update_rates(1000), 0);
}

/// 接続元ごとの本数は、上限が設定されたときだけ数える (T14.13)。
#[test]
fn counting_per_client_starts_when_a_limit_is_set() {
    let t = ConnTable::new();
    let now = Instant::now();
    // 上限を見るまでは数えない (既定の費用 0)
    let _a = t.register(1, "10.0.0.1", now);
    assert!(!t.counting_clients());
    assert_eq!(t.client_conns("10.0.0.1"), 0);

    // 初めて判定したとき、生きている接続から数え直す
    assert!(!t.at_client_limit("10.0.0.1", 2));
    assert!(t.counting_clients());
    assert_eq!(t.client_conns("10.0.0.1"), 1);

    let _b = t.register(2, "10.0.0.1", now);
    assert_eq!(t.client_conns("10.0.0.1"), 2);
    assert!(t.at_client_limit("10.0.0.1", 2), "2 本目で上限");
    assert!(!t.at_client_limit("10.0.0.2", 2), "別の接続元は 0 本から");

    // 抹消で 1 本減り、0 本になった接続元は表から落ちる
    t.unregister(2);
    assert_eq!(t.client_conns("10.0.0.1"), 1);
    assert!(!t.at_client_limit("10.0.0.1", 2));
    t.unregister(1);
    assert_eq!(t.client_conns("10.0.0.1"), 0);

    // 上限を 0 に戻したら表も捨てる
    let _c = t.register(3, "10.0.0.1", now);
    assert_eq!(t.client_conns("10.0.0.1"), 1);
    t.stop_counting_clients();
    assert!(!t.counting_clients());
    assert_eq!(t.client_conns("10.0.0.1"), 0);
    // 数えていない間の抹消は空振り (負にならない)
    t.unregister(3);
    assert!(!t.at_client_limit("10.0.0.1", 1), "数え直しても 0 本");
}

/// `--lite` は枠を持たないが、接続元ごとの本数は数えられる (T14.13)。
#[test]
fn lite_still_counts_connections_per_client() {
    let t = ConnTable::new();
    t.set_enabled(false);
    assert!(!t.at_client_limit("10.0.0.1", 1));
    assert!(
        t.register(1, "10.0.0.1", Instant::now()).is_none(),
        "枠は作らない"
    );
    assert!(t.is_empty());
    assert_eq!(t.client_conns("10.0.0.1"), 1);
    assert!(t.at_client_limit("10.0.0.1", 1));
    t.unregister(1);
    assert_eq!(t.client_conns("10.0.0.1"), 0);
}

/// `--lite` では登録しない (空の一覧)。
#[test]
fn lite_registers_nothing() {
    let t = ConnTable::new();
    t.set_enabled(false);
    assert!(t.register(1, "127.0.0.1", Instant::now()).is_none());
    assert!(t.is_empty());
    assert!(t.snapshot().is_empty());
    assert!(!t.enabled());
}
