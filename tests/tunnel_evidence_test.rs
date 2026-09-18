//! トンネル 1 本ごとの証拠 (`/connections` の `tid` / `spins` / `revents` /
//! `half_closed` / `idle_secs` と、個票の `spins` / `half_closed` / `half_closed_ms`)
//! の結合テスト (T15.0 (4))。
//!
//! 2026-09-18 のデプロイ先では、齢 27 時間・通算 9 kB・0 bps のトンネル 2 本が
//! 「回っている (CPU を食っている)」のか「待っている」のかを `/connections` の 1 行から
//! 区別できなかった (欄が `id` `client` `target` `kind` `state` `age_secs` `bytes` `fds`
//! `rate_bps` しか無かった)。足りなかったのは次の 4 つで、ここではそれが**外から
//! 読める**ことを見る:
//!
//! - **どのスレッドが受け持っているか** (`tid`。`/profile` の `threads_top` と同じ番号)
//! - **半閉じの向きと、そこから経った秒** (`half_closed` / `half_closed_secs`)
//! - **起こされたのに進まなかった回数** (`spins`。T15.5 の空回りはここが伸び続ける)
//! - **前の周の `poll` が返した旗** (`revents`)
//!
//! 入力は「**accept して黙るだけのオリジン**」+「クライアントが `shutdown(Write)`」=
//! 半閉じのトンネル 1 本。`spins` の**書き方**は単体テスト
//! (`a_tunnel_slot_carries_the_evidence_of_a_spin`) で見るので、ここで見るのは
//! **数え方**の 2 つだけ: (i) 5 バイト流して半閉じしただけの 1 本は 0 のまま
//! (起床は必ず `fill` か `drain` につながるので数えない。`poll` の空振り 1 回ぶんだけ許す)、
//! (ii) 黙って待つ 500 ms のあいだに伸びない (空回りなら同じ窓で数十万まで増える)。
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

mod common;
use common::*;

use rust_http_proxy::config::Config;

/// 死んだ半閉じのトンネルを閉じるまでの無通信打ち切り。**見に行く時間を取れるだけ長く、
/// テストが待てるだけ短く**。
const TUNNEL_IDLE: Duration = Duration::from_secs(5);

/// 「空回りしていない」と言える回数の上限 (T15.5 のあとは 0 のはず。
/// 空回りしていれば 500 ms で数十万になるので、境目はここで十分に離れている)。
const MAX_SPINS: u64 = 100;

fn evidence_config() -> Config {
    let mut cfg = park_config();
    cfg.tunnel_idle = TUNNEL_IDLE;
    cfg
}

/// CONNECT を張って `200` まで読む。
fn open_tunnel(proxy_port: u16, target_port: u16) -> TcpStream {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let req = format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        target_port, target_port
    );
    stream.write_all(req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    stream
}

/// **accept して黙るだけ**のオリジン (`start_echo_server` と違って 1 バイトも返さない)。
///
/// 届いたバイトは読み捨てる (読まないと詰まりになって別のものを測ってしまう)。
/// 受けたソケットは呼び出し側へ渡して持たせる — ここで落とすと FIN が出て、
/// 半閉じではなく普通の終わりになるため。
fn start_mute_origin() -> (u16, mpsc::Receiver<TcpStream>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                }
                let _ = tx.send(stream);
            });
        }
    });
    (port, rx)
}

/// `/connections` の配列から `"kind":"connect"` の行を 1 つ取り出す
/// (`tests/rate_test.rs` と同じ手)。
fn connect_row(json: &str) -> Option<String> {
    let head = "\"connections\":[";
    let at = json.find(head)? + head.len();
    let end = at + json[at..].find(']')?;
    json[at..end]
        .split("},{")
        .find(|r| r.contains("\"kind\":\"connect\""))
        .map(|r| r.to_string())
}

/// `"key":<数>` を読む (欄が無ければ `None`。`status_number` と違って落ちない)。
fn num_field(row: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{}\":", key);
    let at = row.find(&pat)? + pat.len();
    row[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

/// `"key":"<文字列>"` を読む (欄が無いか `null` なら `None`)。
fn str_field(row: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\":\"", key);
    let at = row.find(&pat)? + pat.len();
    let end = at + row[at..].find('"')?;
    Some(row[at..end].to_string())
}

/// `"revents":{"client":"…","origin":"…"}` の 2 つの旗の文字列 (欄が無ければ `None`)。
fn revents_flags(row: &str) -> Option<(String, String)> {
    let head = "\"revents\":{\"client\":\"";
    let at = row.find(head)? + head.len();
    let end = at + row[at..].find('"')?;
    let client = row[at..end].to_string();
    let rest = &row[end..];
    let tail = "\"origin\":\"";
    let at2 = rest.find(tail)? + tail.len();
    let end2 = at2 + rest[at2..].find('"')?;
    Some((client, rest[at2..end2].to_string()))
}

/// 半閉じのトンネル 1 本の証拠が `/connections` に出て、閉じたあと個票にも残ること。
#[test]
fn test_integration_a_half_closed_tunnel_shows_its_evidence() {
    let (origin_port, held) = start_mute_origin();
    let proxy_port = start_test_proxy(evidence_config());

    let mut client = open_tunnel(proxy_port, origin_port);
    client.write_all(b"hello").unwrap();
    client.flush().unwrap();
    // クライアントが先に EOF (プロキシは dirs[0] を done にしてオリジンへ shutdown(Write))。
    // オリジンは黙ったままなので、残った向きは打ち切りまで待つ = 半閉じのトンネル
    client.shutdown(Shutdown::Write).unwrap();

    // 半閉じが枠に届くのを待つ (中継の輪が EOF を読んで待ちに入るまで)
    wait_until(
        || {
            connect_row(&endpoint_json(proxy_port, "/connections"))
                .is_some_and(|r| r.contains("\"half_closed\":"))
        },
        "the tunnel to report its half-close",
    );
    let row = connect_row(&endpoint_json(proxy_port, "/connections")).expect("CONNECT の行がある");
    println!("/connections の 1 行: {{{}}}", row);

    // (1) 半閉じの向きと、そこから経った秒
    assert_eq!(
        str_field(&row, "half_closed").as_deref(),
        Some("client"),
        "先に EOF を出したのはクライアント: {}",
        row
    );
    let since = num_field(&row, "half_closed_secs").expect("half_closed_secs がある");
    assert!(
        since < 60,
        "半閉じからの秒が大きすぎる: {} ({})",
        since,
        row
    );

    // (2) 受け持っているスレッド (預かり中なら 0 だが、半閉じのトンネルは預けない)
    let tid = num_field(&row, "tid").expect("tid がある");
    assert!(tid > 0, "tid が 0: {}", row);
    assert_eq!(
        str_field(&row, "state").as_deref(),
        Some("relaying"),
        "半閉じのトンネルは預けない: {}",
        row
    );

    // (3) 空回りしていない (T15.5 のあとは `poll` の起床が必ず 1 回の読み書きになる)。
    //     絶対値は実機の `poll` の返り方に依るので縛らず、**黙って待つ 500 ms のあいだに
    //     伸びないこと**を見る (空回りしていれば同じ窓で数十万まで増える)。
    //     「1 往復ごとに 1 回数える」実装の誤りも、5 バイト流したこの 1 本で 0 にならない
    let spins = num_field(&row, "spins").unwrap_or(0);
    assert!(
        spins <= 1,
        "起床は必ず読み書きにつながるので数えない (`poll` の空振り 1 回ぶんだけ許す): {}",
        row
    );
    thread::sleep(Duration::from_millis(500));
    let later = connect_row(&endpoint_json(proxy_port, "/connections")).expect("まだ開いている");
    let grew = num_field(&later, "spins")
        .unwrap_or(0)
        .saturating_sub(spins);
    assert!(
        grew < MAX_SPINS,
        "黙って待った 500 ms で spins が {} 増えた = 空回り ({})",
        grew,
        later
    );

    // (4) 前の周の `poll` が返した旗は、出ていれば名前で読める
    if let Some((client, origin)) = revents_flags(&row) {
        for side in [client, origin] {
            for name in side.split('|').filter(|s| !s.is_empty()) {
                assert!(
                    ["IN", "OUT", "ERR", "HUP", "NVAL"].contains(&name),
                    "知らない旗 {} ({})",
                    name,
                    row
                );
            }
        }
    }

    // 打ち切りで閉じたあと、同じ 2 つが個票に残ること
    wait_until(
        || connect_row(&endpoint_json(proxy_port, "/connections")).is_none(),
        "the half-closed tunnel to be closed by the idle timeout",
    );
    let recent = endpoint_json(proxy_port, "/recent");
    assert!(
        recent.contains("\"half_closed\":\"client\""),
        "個票に半閉じの向きが無い: {}",
        recent
    );
    assert!(
        recent.contains("\"half_closed_ms\":"),
        "個票に半閉じの ms が無い: {}",
        recent
    );
    assert!(
        recent.contains("\"spins\":"),
        "個票に spins が無い: {}",
        recent
    );

    // オリジン側のソケットは最後まで持っておく (先に落とすと FIN で輪が終わる)
    drop(held);
    drop(client);
}
