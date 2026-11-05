//! `Race` の締め切りと間隔の単体試験 (T34.79)。
//!
//! 接続する関数を作り物に差し替え、網には出ない。候補は IPv4 だけにする (IPv6 の候補は
//! 全体の勝敗の記録 `note_ipv6_attempt` を動かすので使わない)。作り物はポート番号で振る舞いを変える。

use super::*;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([192, 0, 2, 1], port))
}

/// ポート 1: 300 ms 黙ってから繋がる。ポート 2: すぐ繋がる。ポート 3: すぐ断られる。
/// ポート 4: 30 ms 待ってから断られる。返す値は繋いだポート番号。
fn fake(addr: &SocketAddr, _timeout: Option<Duration>) -> io::Result<u16> {
    match addr.port() {
        1 => {
            thread::sleep(Duration::from_millis(300));
            Ok(1)
        }
        2 => Ok(2),
        3 => Err(io::Error::new(io::ErrorKind::ConnectionRefused, "port 3")),
        _ => {
            thread::sleep(Duration::from_millis(30));
            Err(io::Error::new(io::ErrorKind::ConnectionReset, "port 4"))
        }
    }
}

#[test]
fn the_deadline_ends_the_race_with_timed_out() {
    let addrs = [addr(1), addr(1)];
    let t0 = Instant::now();
    let mut race = Race::new(
        &addrs,
        Duration::from_millis(60),
        Duration::from_millis(10),
        fake,
    );
    let err = race.run().expect_err("締め切りの前には誰も繋がらない");
    assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    assert_eq!(err.to_string(), "connection attempts timed out");
    assert!(race.timed_out);
    assert_eq!(race.launched, 2, "間隔が過ぎたので 2 本目も起こした");
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_millis(60) && took < Duration::from_millis(280),
        "{took:?}"
    );
}

#[test]
fn the_second_candidate_starts_after_the_stagger_and_can_win() {
    let addrs = [addr(1), addr(2)];
    let t0 = Instant::now();
    let mut race = Race::new(
        &addrs,
        Duration::from_secs(5),
        Duration::from_millis(40),
        fake,
    );
    let (index, got) = race.run().expect("2 本目が繋がる");
    let took = t0.elapsed();
    assert_eq!((index, got), (1, 2));
    assert!(
        took >= Duration::from_millis(40) && took < Duration::from_millis(280),
        "間隔のぶんだけ待ってから 2 本目: {took:?}"
    );
    assert!(!race.timed_out);
}

#[test]
fn when_every_candidate_fails_the_last_failure_is_returned() {
    // 1 本目はすぐ断られる (走っているものが無くなるので、間隔を待たずに 2 本目を起こす)。
    // 2 本目は 30 ms 後に切られる。返すのは最後に返ってきた方
    let addrs = [addr(3), addr(4)];
    let t0 = Instant::now();
    let mut race = Race::new(&addrs, Duration::from_secs(5), Duration::from_secs(1), fake);
    let err = race.run().expect_err("全部失敗");
    assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    assert_eq!(err.to_string(), "port 4");
    assert!(
        !race.timed_out,
        "全部が失敗したときは締め切りの印を立てない"
    );
    assert!(
        t0.elapsed() < Duration::from_millis(900),
        "間隔 (1 秒) を待っていない"
    );
    // 締め切りが無くても同じ
    let mut race = Race::new(&addrs, Duration::ZERO, Duration::from_secs(1), fake);
    assert_eq!(race.run().unwrap_err().to_string(), "port 4");
}
