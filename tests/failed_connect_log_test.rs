//! 失敗した CONNECT のログが WARN 1 行だけになることの結合テスト (T.28.4)。
//!
//! 0.27.8 までは、tunnel が 502 を書いて WARN を出したあと、同じ `io::Error` を呼び元
//! (`run_conn`) が ERROR でもう 1 行出していた (宛先の無い `error: Connection refused …`)。
//! tokyo の `/log` 1,000 行のうち 499 接続ぶんがこの 2 行で、環の窓が半分になっていた。
//!
//! `/log` の環は処理系で 1 つなので、このファイルのテストは 1 本だけにする。

use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

mod common;
use common::*;

/// `/log` の 1 行 (`{"at":…,"level":"…","conn":N,…}`) から `conn` と `level` を取り出す。
fn lines(json: &str) -> Vec<(String, Option<u64>, String)> {
    json.split("{\"at\":")
        .skip(1)
        .map(|row| {
            let level = row
                .split("\"level\":\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("")
                .to_string();
            let conn = row
                .split("\"conn\":")
                .nth(1)
                .map(|s| {
                    s.chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>()
                })
                .and_then(|s| s.parse().ok());
            (level, conn, row.to_string())
        })
        .collect()
}

#[test]
fn test_integration_failed_connect_logs_one_warn_and_no_error() {
    let refused = RefusedPort::reserve();
    let proxy_port = start_test_proxy(proxy_config());

    let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let target = format!("127.0.0.1:{}", refused.port());
    s.write_all(format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n", target, target).as_bytes())
        .unwrap();
    let head = read_connect_response(&mut s);
    assert!(head.starts_with("HTTP/1.1 502"), "{}", head);
    drop(s);

    // 個票は今までどおり 1 件 (`/errors`)
    wait_until(
        || endpoint_json(proxy_port, "/errors").contains(&target),
        "the failed CONNECT should be in /errors",
    );

    // 呼び元が出し直すのは tunnel が戻ったあとなので、少し待ってから数える
    std::thread::sleep(Duration::from_millis(300));
    let json = endpoint_json(proxy_port, "/log");
    let rows = lines(&json);
    let warn = rows
        .iter()
        .find(|(level, _, row)| {
            level == "warn" && row.contains(&format!("502 Bad Gateway: connect {}", target))
        })
        .unwrap_or_else(|| panic!("warn の行が無い: {}", json));
    let conn = warn.1.expect("warn の行に接続番号がある");
    let same: Vec<&(String, Option<u64>, String)> =
        rows.iter().filter(|(_, c, _)| *c == Some(conn)).collect();
    assert_eq!(
        same.iter().filter(|(l, _, _)| l == "warn").count(),
        1,
        "warn は 1 行: {:?}",
        same
    );
    assert_eq!(
        same.iter().filter(|(l, _, _)| l == "error").count(),
        0,
        "error は 0 行: {:?}",
        same
    );
}
