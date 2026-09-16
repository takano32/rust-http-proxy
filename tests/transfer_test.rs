//! 転送速度と半閉じの分布 (`/history` の `transfer`) の結合テスト (T14.25)。
//!
//! 「遅い」には確立が遅いのと転送が遅いのがある。1 本ごとの速さは `/recent` の寿命と
//! バイトから割れるが、**分布** (どの程度のトンネルが 100 KiB/s 未満か) が無かった。
//! 半閉じ (片側だけ閉じた) から反対側が閉じるまでの時間は `PROXY_TUNNEL_IDLE_SECS` の
//! 設計の材料になる。ここで見るのは「実際に流したトンネルがその区間に 1 件入るか」だけ。
//!
//! 窓を閉じるのは history スレッド (`/history` の標本と同じ 5 秒の境目) なので、
//! `/history` を叩く前に**指標を直に読んで**窓が閉じるのを待つ (`tests/bursts_test.rs`
//! と同じ作法。`/history` 自体が 1 本の接続なので、待つのに使わない)。
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

mod common;
use common::*;

use rust_http_proxy::metrics::Metrics;

/// history スレッドの周期 (本番は 5 秒。テストは短くする)。
const TICK: Duration = Duration::from_millis(50);

/// 流すバイト数 (受け入れ基準の 1 MiB)。
const MIB: usize = 1 << 20;

/// 半閉じを作るオリジンが、EOF を見てから閉じるまで待つ時間。
const LINGER: Duration = Duration::from_millis(150);

/// 履歴スレッド付きのテスト用プロキシ。
fn transfer_proxy() -> (u16, Arc<Metrics>) {
    let mut cfg = park_config();
    // 握ったままの接続が預かり所の期限で閉じないように長くする
    cfg.keepalive = Duration::from_secs(60);
    start_test_proxy_with_history(cfg, TICK)
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

/// クライアントの EOF を見てから `delay` だけ待って閉じるオリジン (**半閉じを作る**)。
///
/// 内蔵の echo (`start_echo_server`) は EOF を見たらすぐ閉じるので、半閉じでいる時間が
/// 0 ms になり「区間に入ったのは偶然か」が分からない。ここでは 150 ms 待たせて、
/// 1 ms や 4 ms の段ではなくその上の段に入ることを見る。
fn start_lingering_origin(delay: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = [0u8; 64 * 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
                // ここに来たのは相手が FIN を出したとき (= プロキシから見て半閉じ)。
                // すぐ閉じずに待つと、その間トンネルは片側だけ閉じたまま生きる
                thread::sleep(delay);
                let _ = stream.shutdown(std::net::Shutdown::Both);
            });
        }
    });
    port
}

/// トンネルが閉じて、5 秒の窓が 1 つ閉じるまで待つ (最大 5 秒 + 余裕)。
fn wait_for_a_closed_window(metrics: &Metrics) {
    wait_until(
        || metrics.active_connections.load(Ordering::Relaxed) == 0,
        "the tunnel to close",
    );
    wait_until(
        || metrics.history.transfer.counts().0 >= 1,
        "the 5s transfer window to close",
    );
}

/// `/history` の `transfer` の塊を切り出す (次の鍵の手前まで)。
fn transfer_block(json: &str) -> String {
    let at = json.find("\"transfer\":").expect("transfer がある");
    let rest = &json[at + "\"transfer\":".len()..];
    if rest.starts_with("null") {
        return "null".to_string();
    }
    let mut depth = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return rest[..=i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("transfer の塊が閉じていない: {}", rest);
}

/// `transfer` の標本 1 行を、入れ子の配列を 1 つの文字列のまま取り出す。
fn first_row(block: &str) -> Vec<String> {
    let at = block.find("\"samples\":[").expect("samples がある");
    let rows = &block[at + "\"samples\":[".len()..];
    assert!(rows.starts_with('['), "窓が 1 つも無い: {}", block);
    let mut depth = 0usize;
    let mut end = 0usize;
    for (i, c) in rows.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    let row = &rows[1..end];
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in row.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(row[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(row[start..].to_string());
    out
}

/// `[1,0,2]` を数の並びにする。
fn nums(field: &str) -> Vec<u64> {
    field
        .trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .map(|s| s.trim().parse::<u64>().expect("数の並び"))
        .collect()
}

fn num(field: &str) -> u64 {
    field.trim().parse::<u64>().expect("数")
}

/// 1 MiB を流したトンネルが速さの区間に 1 件入ること (受け入れ基準)。
#[test]
fn test_integration_a_megabyte_lands_in_one_speed_bucket() {
    let echo_port = start_echo_server();
    let (proxy_port, metrics) = transfer_proxy();

    // 1 MiB 上げて 1 MiB 下ろす (echo なので運ぶのは合計 2 MiB)。
    // 書くのは別スレッド: 下りを読まないまま上りを 1 MiB 書くと、下りの緩衝が
    // 埋まったところで試験そのものが止まる
    let tunnel = open_tunnel(proxy_port, echo_port);
    let mut writer = tunnel.try_clone().unwrap();
    let sender = thread::spawn(move || {
        writer.write_all(&vec![0x5au8; MIB]).unwrap();
        writer.flush().unwrap();
    });
    let mut reader = tunnel.try_clone().unwrap();
    let mut got = vec![0u8; MIB];
    reader.read_exact(&mut got).unwrap();
    sender.join().unwrap();
    assert!(got.iter().all(|&b| b == 0x5a), "運んだ中身が違う");

    // クライアントから閉じる (`client_eof`)
    tunnel.shutdown(std::net::Shutdown::Write).unwrap();
    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest);
    drop(tunnel);
    wait_for_a_closed_window(&metrics);

    let json = endpoint_json(proxy_port, "/history?res=5");
    // 既存の形はそのまま (標本 → `closed` → `transfer` の順で、どれも別の配列)
    let samples_at = json.find("\"samples\":").expect("標本がある");
    let closed_at = json.find("\"closed\":").expect("閉じた接続の分布がある");
    let transfer_at = json.find("\"transfer\":").expect("速さの分布がある");
    assert!(
        samples_at < closed_at && closed_at < transfer_at,
        "transfer は closed の隣 (後ろ)"
    );
    let block = transfer_block(&json);
    assert!(block.contains("\"interval_secs\":5"), "{}", block);
    assert!(
        block.contains(
            "\"keys\":[\"t\",\"tunnels\",\"speed_n\",\"speed\",\"half_close_n\",\"half_close\",\"bytes_sum\",\"relay_ms_sum\",\"half_close_ms_sum\",\"stall_client_ms_sum\",\"stall_origin_ms_sum\"]"
        ),
        "{}",
        block
    );
    assert!(
        block.contains(
            "\"speed_bounds_bps\":[1024,4096,16384,65536,262144,1048576,4194304,16777216,67108864,268435456,1073741824,4294967296]"
        ),
        "{}",
        block
    );
    assert!(
        block.contains(
            "\"half_close_bounds_ms\":[1,4,16,64,256,1024,4096,16384,65536,262144,1048576,4194304]"
        ),
        "{}",
        block
    );
    assert!(block.contains("\"min_bytes\":1024"), "{}", block);
    assert!(block.contains("\"capacity\":720"), "{}", block);
    assert!(block.contains("\"recorded\":1"), "{}", block);

    let row = first_row(&block);
    // 末尾の 2 列は T14.42 (詰まりの向きの合計)
    assert_eq!(row.len(), 11, "列の数が keys と合わない: {:?}", row);
    assert!(num(&row[0]) > 1_700_000_000, "窓の時刻: {}", row[0]);
    assert_eq!(num(&row[1]), 1, "終わったトンネルは 1 本");
    assert_eq!(num(&row[2]), 1, "速さを数えたのは 1 本");
    let speed = nums(&row[3]);
    assert_eq!(speed.len(), 13, "12 段 + 上限なし: {:?}", speed);
    assert_eq!(
        speed.iter().sum::<u64>(),
        1,
        "速さの区間に 1 件: {:?}",
        speed
    );
    // 1 本しか入っていないので、入った段がそのまま実測の速さ。ループバックで
    // 2 MiB を運んだのだから、少なくとも 256 KiB/s (= 2 MiB を 8 秒) より速い
    let at = speed.iter().position(|&n| n == 1).unwrap();
    assert!(at >= 5, "2 MiB が 256 KiB/s 未満の段に入った: {:?}", speed);
    assert_eq!(num(&row[6]), 2 * MIB as u64, "運んだ合計が 2 MiB でない");
    // `closed` (T14.6) とも突き合わせられる (同じ 1 本を両方が数えている)
    let closed = &json[closed_at..transfer_at];
    assert!(closed.contains("\"recorded\":1"), "{}", closed);
}

/// 半閉じで終わったトンネルが半閉じの区間に 1 件入ること (受け入れ基準)。
#[test]
fn test_integration_a_half_closed_tunnel_lands_in_one_half_close_bucket() {
    let origin_port = start_lingering_origin(LINGER);
    let (proxy_port, metrics) = transfer_proxy();

    // 少し流してからクライアントが先に閉じる (= 半閉じ)。オリジンは 150 ms 待って閉じる
    let mut tunnel = open_tunnel(proxy_port, origin_port);
    tunnel.write_all(b"hello").unwrap();
    let mut buf = [0u8; 5];
    tunnel.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello");
    tunnel.shutdown(std::net::Shutdown::Write).unwrap();
    let mut rest = Vec::new();
    let _ = tunnel.read_to_end(&mut rest);
    drop(tunnel);
    wait_for_a_closed_window(&metrics);

    let json = endpoint_json(proxy_port, "/history?res=5");
    let block = transfer_block(&json);
    let row = first_row(&block);
    assert_eq!(num(&row[1]), 1, "終わったトンネルは 1 本");
    // 運んだのは 10 バイトなので速さは数えない (1 KiB 未満)
    assert_eq!(num(&row[2]), 0, "1 KiB 未満は速さを数えない");
    assert_eq!(num(&row[4]), 1, "半閉じで終わったのは 1 本");
    let half = nums(&row[5]);
    assert_eq!(half.len(), 13, "12 段 + 上限なし: {:?}", half);
    assert_eq!(
        half.iter().sum::<u64>(),
        1,
        "半閉じの区間に 1 件: {:?}",
        half
    );
    // 150 ms 待たせたので、64 ms 以下の段には入らない (段の境目は 1/4/16/64/256…)
    let at = half.iter().position(|&n| n == 1).unwrap();
    assert!(at >= 4, "半閉じの時間が短すぎる段に入った: {:?}", half);
    let took = num(&row[8]);
    assert!(
        (100..30_000).contains(&took),
        "半閉じの合計 ms が実際に待った時間と違う: {}",
        took
    );

    // 1 分の窓も同じ形、1 時間は残していない (`closed` と同じ)
    let minute = endpoint_json(proxy_port, "/history?res=60");
    assert!(
        transfer_block(&minute).contains("\"interval_secs\":60"),
        "{}",
        minute
    );
    let hour = endpoint_json(proxy_port, "/history?res=3600");
    assert_eq!(transfer_block(&hour), "null", "{}", hour);
}

/// 窓が空でも `/history` は今までどおり読めること (`transfer` は 0 窓)。
#[test]
fn test_integration_history_without_tunnels_has_an_empty_transfer_block() {
    let (proxy_port, _metrics) = transfer_proxy();
    let json = endpoint_json(proxy_port, "/history?res=5");
    let block = transfer_block(&json);
    assert!(block.contains("\"samples\":[]"), "{}", block);
    assert!(block.contains("\"windows\":0"), "{}", block);
    assert!(block.contains("\"recorded\":0"), "{}", block);
}
