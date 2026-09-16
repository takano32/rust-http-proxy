//! ホスト別の上り / 下りのバイト (`bytes_in` / `bytes_out`) の結合テスト (T14.26)。
//!
//! `hosts[]` の `bytes` は合計で、アップロード主体の相手 (ログ送信) とダウンロード主体の
//! 相手を区別できなかった。上り主体のホストは「利用者の回線の上り」が律速で、プロキシでも
//! オリジンでもない — その切り分けに要る。
//!
//! 見るのは 3 つ: (a) CONNECT で 1 KiB 上げて 2 KiB 下ろしたら `bytes_in` / `bytes_out` が
//! そのとおりに割れて `bytes` はその和のままであること、(b) forward は要求本文が上り・
//! 応答が下りに入ること、(c) **T14.26 より前に書かれた `.rrd`** (`HostStats` が 53 項目) から
//! 起動しても落ちず、新しい 2 欄が 0 で読み戻ること (T14.5 と同じ流儀で実バイナリを起こす)。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

mod common;
use common::*;

/// 上げるバイト数 (受け入れ基準)。
const UP: usize = 1024;
/// 下ろすバイト数 (受け入れ基準。上りと違う値にして取り違えに気付けるようにする)。
const DOWN: usize = 2048;

/// `1 KiB 受け取ったら 2 KiB 返す` オリジン (**上りと下りを違う量にする**ため)。
///
/// 内蔵の echo (`start_echo_server`) は上りと下りが同じ量になるので、`bytes_in` と
/// `bytes_out` を取り違えていても気付けない。
fn start_up_then_down_origin() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut got = vec![0u8; UP];
                if stream.read_exact(&mut got).is_err() {
                    return;
                }
                let _ = stream.write_all(&vec![0xa5u8; DOWN]);
                let _ = stream.flush();
                // クライアントが閉じるまで付き合う (先に閉じると理由が変わる)
                let mut rest = Vec::new();
                let _ = stream.read_to_end(&mut rest);
            });
        }
    });
    port
}

/// `{"host":"<name>",…}` の 1 行を切り出す (入れ子の `rtt_ms` を数えて閉じ括弧まで)。
fn row_of(json: &str, key: &str, name: &str) -> String {
    let pat = format!("\"{}\":\"{}\"", key, name);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("{} が {} に無い: {}", name, key, json));
    let rest = &json[at..];
    let mut depth = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '{' => depth += 1,
            '}' if depth == 0 => return rest[..i].to_string(),
            '}' => depth -= 1,
            _ => {}
        }
    }
    panic!("行が閉じていない: {}", rest);
}

/// 1 行から `"key":<数>` を読む。
fn field(row: &str, key: &str) -> u64 {
    let pat = format!("\"{}\":", key);
    let at = row
        .find(&pat)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, row))
        + pat.len();
    row[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} が数でない: {}", key, row))
}

/// CONNECT で 1 KiB 上げて 2 KiB 下ろしたら、`bytes` がその和のまま 2 つに割れること。
#[test]
fn test_integration_a_connect_splits_its_bytes_into_up_and_down() {
    let origin_port = start_up_then_down_origin();
    let (proxy_port, metrics) = start_test_proxy_with_metrics(park_config());
    let host_key = format!("connect://127.0.0.1:{}", origin_port);

    let mut tunnel = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    tunnel.set_nodelay(true).unwrap();
    tunnel
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    tunnel
        .write_all(
            format!(
                "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
                origin_port, origin_port
            )
            .as_bytes(),
        )
        .unwrap();
    let head = read_connect_response(&mut tunnel);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);

    tunnel.write_all(&vec![0x5au8; UP]).unwrap();
    tunnel.flush().unwrap();
    let mut got = vec![0u8; DOWN];
    tunnel.read_exact(&mut got).unwrap();
    assert!(got.iter().all(|&b| b == 0xa5), "下ろした中身が違う");
    drop(tunnel);

    // 統計を書くのは `tunnel::report` (トンネルの終わり) なので、指標を直に見て待つ
    // (`/hosts` を叩いて待つと、待つその行為が接続を 1 本増やす)
    wait_until(
        || {
            metrics
                .hosts_sorted()
                .iter()
                .any(|(h, s)| *h == host_key && s.bytes_in >= UP as u64)
        },
        "the tunnel to be counted",
    );

    let row = row_of(&endpoint_json(proxy_port, "/hosts"), "host", &host_key);
    let (bytes, up, down) = (
        field(&row, "bytes"),
        field(&row, "bytes_in"),
        field(&row, "bytes_out"),
    );
    assert!(up >= UP as u64, "上りが 1 KiB に足りない: {}", row);
    assert!(down >= DOWN as u64, "下りが 2 KiB に足りない: {}", row);
    assert_eq!(bytes, up + down, "`bytes` は今までどおり和: {}", row);
    // 取り違えていないこと (上りと下りを入れ替えたら 1024 と 2048 が逆になる)
    assert!(down > up, "上りと下りが入れ替わっている: {}", row);

    // `/status` の `hosts[]` と `clients[]` にも同じ 2 欄が出る (同じ組み立てを通る)
    let status = status_json(proxy_port);
    let host_row = row_of(&status, "host", &host_key);
    assert_eq!(field(&host_row, "bytes_in"), up, "{}", host_row);
    let client_row = row_of(&status, "client", "127.0.0.1");
    assert_eq!(field(&client_row, "bytes_in"), up, "{}", client_row);
    assert_eq!(field(&client_row, "bytes_out"), down, "{}", client_row);
}

/// forward は要求本文が上り・応答が下りに入ること。
///
/// forward の `bytes` は**今までどおり応答のぶんだけ**なので (欄の意味は変えない)、
/// CONNECT と違って `bytes_in + bytes_out` の方が大きくなる。
#[test]
fn test_integration_a_forwarded_request_counts_its_body_as_upload() {
    let origin_port = start_body_echo_origin();
    let (proxy_port, metrics) = start_test_proxy_with_metrics(proxy_config());
    let host = format!("127.0.0.1:{}", origin_port);
    let host_key = format!("http://127.0.0.1:{}", origin_port);

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    let payload = vec![b'a'; UP];
    stream
        .write_all(
            format!(
                "POST http://{}/up HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\n\r\n",
                host,
                host,
                payload.len()
            )
            .as_bytes(),
        )
        .unwrap();
    stream.write_all(&payload).unwrap();
    let (head, _) = read_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
    drop(stream);

    wait_until(
        || {
            metrics
                .hosts_sorted()
                .iter()
                .any(|(h, s)| *h == host_key && s.bytes_in >= UP as u64)
        },
        "the request to be counted",
    );

    let row = row_of(&endpoint_json(proxy_port, "/hosts"), "host", &host_key);
    let (bytes, up, down) = (
        field(&row, "bytes"),
        field(&row, "bytes_in"),
        field(&row, "bytes_out"),
    );
    assert_eq!(up, UP as u64, "上りは要求本文ちょうど: {}", row);
    assert_eq!(down, bytes, "下りは応答 (`bytes` と同じ): {}", row);
    assert!(down > 0, "応答のバイトが 0: {}", row);
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// T14.26 より前に書かれた `.rrd` (版 2 のまま、`HostStats` が 53 項目) を置いて起動しても
/// 落ちず、新しい 2 欄 (向き別のバイト) が 0 で読み戻ること。
///
/// **版は上げていない** ので、古いファイルは読み捨てられずにそのまま復元される。
/// `Dec` は足りなければ 0 を返すので、末尾に足した欄だけが 0 になるのが期待の動き
/// (T14.5 の RTT の 4 欄がそのまま読めることも一緒に見る = 欄がずれていない証拠)。
#[test]
fn test_integration_a_pre_direction_state_file_restores_with_zero_direction_bytes() {
    use rust_http_proxy::rrd::{Enc, Rrd};

    let dir = std::env::temp_dir().join(format!("rhp-t1426-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".rust-http-proxy.rrd");

    // 版 2 のファイルを作り、**T14.26 より前の形** (名前 128 B + 53 項目) で 1 行ずつ書く
    {
        let (rrd, created) = Rrd::open(&path).unwrap();
        assert!(created);
        for (region, name, requests) in [
            (rrd.layout.hosts, "connect://mtalk.google.com:5228", 920u64),
            (rrd.layout.clients, "198.51.100.7", 463),
        ] {
            let mut e = Enc::new();
            e.str(name, 128)
                .u64(1_700_000_000) // last_seen
                .u64(requests)
                .u64(0) // hits
                .u64(0) // misses
                .u64(requests) // bypass
                .u64(0) // errors
                .u64(0) // blocked
                .u64(123_456) // bytes (向きの分からない昔の合計)
                .u64(requests) // timed
                .u64(requests * 30) // duration_ms_sum
                .u64(31); // duration_ms_max
            for _ in 0..25 {
                e.u64(0); // buckets (24 段 + 上限なし)
            }
            e.u64(0).u64(0).u64(requests * 30).u64(requests).u64(0); // dns/connect/族
            for _ in 0..8 {
                e.u64(0); // errors_by_cause
            }
            // T14.5 の RTT の 4 欄 (30.1 ms を 1 標本、再送 2) までは書かれている
            e.u64(30_100).u64(30_100).u64(1).u64(2);
            assert_eq!(e.0.len(), 128 + 53 * 8, "T14.26 より前の 1 行は 552 B");
            rrd.write(region, 0, &e.0).unwrap();
        }
    }

    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\nPROXY_CACHE_RESERVE=off\n",
    )
    .unwrap();
    let mut child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
            .env("HOME", &dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("could not start the proxy binary"),
    );
    let stdout = child.0.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut port = None;
    let mut state_line = String::new();
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(30)) {
        if line.contains("state file ") {
            state_line = line.clone();
        }
        if let Some(rest) = line.split_once("listening on ") {
            port = rest
                .1
                .split_whitespace()
                .next()
                .and_then(|a| a.rsplit(':').next())
                .and_then(|p| p.parse::<u16>().ok());
            break;
        }
    }
    let port = port.expect("the proxy did not log its listening port");
    assert!(
        !state_line.contains("created"),
        "版は上げていないので作り直さないはず: {:?}",
        state_line
    );
    assert!(
        state_line.contains("1 hosts, 1 clients restored"),
        "古いファイルの行が読み戻っていない: {:?}",
        state_line
    );

    let status = status_json(port);
    for (key, name, requests) in [
        ("host", "connect://mtalk.google.com:5228", 920),
        ("client", "198.51.100.7", 463),
    ] {
        let row = row_of(&status, key, name);
        // 前の欄はそのまま読める (ずれていれば要求数から化ける)
        assert_eq!(field(&row, "requests"), requests, "{}", row);
        // 末尾に足した 2 欄は 0 で読み戻る。合計は昔のファイルにも入っている
        assert_eq!(field(&row, "bytes"), 123_456, "{}", row);
        assert_eq!(field(&row, "bytes_in"), 0, "{}", row);
        assert_eq!(field(&row, "bytes_out"), 0, "{}", row);
        // T14.5 の 4 欄も読めている (新しい 2 欄が前に割り込んでいれば RTT が化ける)
        assert!(
            row.contains("\"rtt_ms\":{\"avg\":30.100,\"min\":30.100,\"samples\":1},\"retrans\":2"),
            "{}",
            row
        );
    }
    assert!(status.contains("\"write_errors\":0"), "{}", status);

    drop(child);
    let _ = std::fs::remove_dir_all(&dir);
}
