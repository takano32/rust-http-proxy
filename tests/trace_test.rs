//! 接続元 1 つの追跡 `PROXY_TRACE_CLIENT` → `/trace` の結合テスト (T14.27)。
//!
//! 全体のログ水準を `trace` に上げるとアクセスログが**全員に**乗る (T10.10 の 7.2 us/要求)
//! ので、**1 つの接続元だけ**を追う口。旗は accept 直後に立て、要求ごとに見るのはその旗
//! 1 つだけ。一致しない接続元からの要求は 1 行も残らない。
//!
//! **別の接続元は送信元アドレスで作る** (`connect_from([127,0,0,2], ..)`。T14.13 と同じ)。
//! ループバックは `127.0.0.0/8` が丸ごと自分のアドレスなので、1 台の機械で「2 人の利用者」
//! が作れる。
//!
//! `/trace` のリングは**プロセスに 1 本きり**なので、同じプロセスで動くテスト
//! (`start_test_proxy_*`) は [`SERIAL`] の鍵で 1 つずつ通す。実バイナリを起こすテストは
//! 別プロセスなので鍵は要らない。
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;

mod common;
use common::*;

/// 同じプロセスで動くテストを 1 つずつ通す鍵 (リングは静的に 1 本)。
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    rust_http_proxy::trace::clear();
    g
}

/// `needle` を含む 1 行 (`{...}`) を切り出す (段階の `ms` が入れ子なので括弧を数える)。
fn row_with(body: &str, needle: &str) -> String {
    let at = body
        .find(needle)
        .unwrap_or_else(|| panic!("{} が無い: {}", needle, body));
    let start = at
        - body[..at]
            .chars()
            .rev()
            .position(|c| c == '{')
            .expect("行の頭")
        - 1;
    let mut depth = 0i32;
    for (i, c) in body[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return body[start..start + i + 1].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("行の尻が無い: {}", body);
}

/// `/snapshot` の `parts` に並んだ名前 (この中に `trace` が入っていないことを見る)。
fn snapshot_parts(body: &str) -> String {
    let at = body.find("\"parts\":[").expect("parts");
    let end = at + body[at..].find(']').expect("parts の尻") + 1;
    body[at..end].to_string()
}

fn connect_request(target_port: u16) -> String {
    format!(
        "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        target_port, target_port
    )
}

/// CONNECT を 1 本張ってすぐ閉じる (トンネルの終わりで `/trace` に 1 行残るように)。
fn open_and_close_a_tunnel(proxy_port: u16, target_port: u16) {
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(connect_request(target_port).as_bytes())
        .unwrap();
    let head = read_connect_response(&mut s);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    drop(s);
}

/// `src` から forward の GET を 1 本投げる (応答の先頭行を返す)。
fn get_from(src: [u8; 4], proxy_port: u16, origin_port: u16, path: &str) -> String {
    let mut s = connect_from(src, proxy_port);
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(
        format!(
            "GET http://127.0.0.1:{}{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            origin_port, path, origin_port
        )
        .as_bytes(),
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

/// 受け入れ基準 (1): `PROXY_TRACE_CLIENT=127.0.0.1` のとき、`/trace` に **forward の
/// 要求行と状態と段階**、**CONNECT の宛先と閉じた理由**が並ぶこと。
#[test]
fn test_trace_lists_the_request_line_the_status_the_stages_and_the_close_reason() {
    let _g = serial();
    let (origin_port, _origin) = start_mock_origin();
    let echo_port = start_echo_server();
    let mut cfg = proxy_config();
    cfg.trace_client = Some("127.0.0.1".parse().unwrap());
    let proxy_port = start_test_proxy(cfg);

    let url = format!("http://127.0.0.1:{}/hello?q=1", origin_port);
    let resp = get_via_proxy(proxy_port, &url, &format!("127.0.0.1:{}", origin_port));
    assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);
    open_and_close_a_tunnel(proxy_port, echo_port);

    // トンネルの行は**終わったとき**に書かれるので、両方そろうまで待つ
    wait_until(
        || {
            let body = endpoint_json(proxy_port, "/trace?n=200");
            body.contains("\"method\":\"GET\"") && body.contains("\"method\":\"CONNECT\"")
        },
        "/trace に forward と CONNECT が並ぶ",
    );
    let body = endpoint_json(proxy_port, "/trace?n=200");

    // forward: 要求行 (メソッド + URL + 版) と応答の状態と段階の ms
    let get = row_with(&body, "\"method\":\"GET\"");
    assert!(get.contains("\"client\":\"127.0.0.1\""), "{}", get);
    assert!(
        get.contains(&format!("\"target\":\"{}\"", url)),
        "パスが残っていない (この口だけの例外): {}",
        get
    );
    assert!(get.contains("\"version\":\"HTTP/1.1\""), "{}", get);
    assert!(get.contains("\"status\":200"), "{}", get);
    assert!(
        get.contains("\"ms\":{\"dns\":") && get.contains("\"connect\":"),
        "段階が無い: {}",
        get
    );
    assert!(get.contains("\"took_ms\":"), "{}", get);
    // 閉じた理由は接続 1 本のものなので forward の 1 要求では `null`
    assert!(get.contains("\"reason\":null"), "{}", get);

    // CONNECT: 宛先と閉じた理由
    let connect = row_with(&body, "\"method\":\"CONNECT\"");
    assert!(
        connect.contains(&format!("\"target\":\"127.0.0.1:{}\"", echo_port)),
        "宛先が無い: {}",
        connect
    );
    assert!(connect.contains("\"status\":200"), "{}", connect);
    assert!(
        !connect.contains("\"reason\":null"),
        "閉じた理由が無い: {}",
        connect
    );

    // 口そのものの形 (容量・パスの上限・`--lite` ではないこと)。**自分宛ての
    // `/trace` や `/status` は 1 行も残らない** (内部エンドポイントは中継を通らない)
    assert!(body.contains("\"capacity\":1000"), "{}", body);
    assert!(body.contains("\"max_path\":256"), "{}", body);
    assert!(body.contains("\"lite\":false"), "{}", body);
    assert!(!body.contains("\"target\":\"/trace"), "{}", body);
    assert_eq!(body.matches("\"method\":").count(), 2, "{}", body);
    assert!(body.len() < 256 * 1024, "{} B", body.len());
}

/// 受け入れ基準 (2): `PROXY_TRACE_CLIENT` が**別の接続元**なら空であること。
///
/// 追跡先を `127.0.0.2` にして `127.0.0.1` から叩くと 1 行も残らず、同じプロキシへ
/// `127.0.0.2` から叩くと残る (旗は接続元ごとに accept で立つ)。
#[test]
fn test_only_the_configured_client_is_traced() {
    let _g = serial();
    let (origin_port, _origin) = start_mock_origin();
    let mut cfg = proxy_config();
    cfg.trace_client = Some("127.0.0.2".parse().unwrap());
    let proxy_port = start_test_proxy(cfg);

    let url = format!("http://127.0.0.1:{}/other", origin_port);
    let resp = get_via_proxy(proxy_port, &url, &format!("127.0.0.1:{}", origin_port));
    assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);
    let body = endpoint_json(proxy_port, "/trace");
    assert!(
        body.contains("\"trace\":[]"),
        "追跡していない接続元: {}",
        body
    );
    assert!(body.contains("\"count\":0"), "{}", body);
    assert!(body.contains("\"recorded\":0"), "{}", body);

    // 追跡先の接続元から叩けば残る
    let resp = get_from([127, 0, 0, 2], proxy_port, origin_port, "/mine");
    assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);
    wait_until(
        || endpoint_json(proxy_port, "/trace").contains("\"client\":\"127.0.0.2\""),
        "127.0.0.2 の行が残る",
    );
    let body = endpoint_json(proxy_port, "/trace");
    assert!(body.contains("/mine"), "{}", body);
    assert!(!body.contains("/other"), "他の接続元が混ざった: {}", body);
}

/// 受け入れ基準 (3): 実バイナリで `.env` を書き換えると**次の接続から**効くこと。
///
/// あわせて `/config` に効いている値と出どころが出ること、`/` の案内に載ること、
/// **`/snapshot` には入っていない**こと (パスが雪像に残らないように) も見る。
#[test]
fn test_integration_the_env_file_takes_effect_on_the_next_connection() {
    let (origin_port, _origin) = start_mock_origin();
    let dir = std::env::temp_dir().join(format!("rhp-t1427-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let write_env = |client: &str| {
        std::fs::write(
            dir.join(".env"),
            format!(
                "SERVER_PORT=0\n\
                 PROXY_BIND=127.0.0.1\n\
                 PROXY_LOG_LEVEL=info\n\
                 PROXY_ALLOW_LOCAL=on\n\
                 PROXY_CACHE_ENABLED=off\n\
                 PROXY_TRACE_CLIENT={}\n",
                client
            ),
        )
        .unwrap();
    };

    // まずは別の接続元を追いかけている
    write_env("127.0.0.2");
    let proxy = ProxyProcess::start(&dir);
    let url = format!("http://127.0.0.1:{}/before", origin_port);
    let resp = get_via_proxy(proxy.port, &url, &format!("127.0.0.1:{}", origin_port));
    assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);
    let body = endpoint_json(proxy.port, "/trace");
    assert!(body.contains("\"trace\":[]"), "{}", body);

    // `/config` に効いている値と出どころ
    let config = endpoint_json(proxy.port, "/config");
    assert!(
        config.contains("\"PROXY_TRACE_CLIENT\":{\"value\":\"127.0.0.2\",\"source\":\"env_file\""),
        "{}",
        &config[..config.len().min(4096)]
    );

    // `.env` で書き換える (待つのは起動ログの行。`/status` を叩いて待つと、その 1 本が
    // 数えたい状態を動かす — T14.18 で踏んだ罠)
    write_env("127.0.0.1");
    let line = proxy.wait_for_log("PROXY_TRACE_CLIENT");
    assert!(line.contains("settings reloaded"), "{}", line);

    // **次の接続から**効く
    let url = format!("http://127.0.0.1:{}/after", origin_port);
    let resp = get_via_proxy(proxy.port, &url, &format!("127.0.0.1:{}", origin_port));
    assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);
    wait_until(
        || endpoint_json(proxy.port, "/trace").contains("/after"),
        "書き換えたあとの要求が `/trace` に残る",
    );
    let body = endpoint_json(proxy.port, "/trace");
    assert!(
        !body.contains("/before"),
        "書き換える前の要求が残った: {}",
        body
    );
    assert!(body.contains("\"status\":200"), "{}", body);

    // 案内に載る
    let root = raw_get(
        proxy.port,
        &format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            proxy.port
        ),
    );
    assert!(root.contains("/trace?n=200"), "{}", root);

    // **`/snapshot` には入れない** (パスが雪像のファイルに残らないように)。
    // `memory.rings.trace` (容量の見積もり) は数字なので、見るのは `parts` と中身
    let snapshot = endpoint_json(proxy.port, "/snapshot");
    let parts = snapshot_parts(&snapshot);
    assert!(!parts.contains("\"trace\""), "雪像に入っている: {}", parts);
    assert!(!snapshot.contains("/after"), "雪像にパスが残った");

    drop(proxy);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--lite` は枠 (`ConnSlot`) を作らないので旗も立たず、1 行も残らないこと (README の注記)。
#[test]
fn test_integration_the_lite_profile_does_not_trace() {
    let (origin_port, _origin) = start_mock_origin();
    let dir = std::env::temp_dir().join(format!("rhp-t1427-lite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\n\
         PROXY_BIND=127.0.0.1\n\
         PROXY_PROFILE=lite\n\
         PROXY_LOG_LEVEL=info\n\
         PROXY_ALLOW_LOCAL=on\n\
         PROXY_TRACE_CLIENT=127.0.0.1\n",
    )
    .unwrap();
    let proxy = ProxyProcess::start(&dir);

    let url = format!("http://127.0.0.1:{}/lite", origin_port);
    let resp = get_via_proxy(proxy.port, &url, &format!("127.0.0.1:{}", origin_port));
    assert!(resp.starts_with("HTTP/1.1 200"), "{}", resp);
    let body = endpoint_json(proxy.port, "/trace");
    assert!(body.contains("\"trace\":[]"), "{}", body);
    assert!(body.contains("\"lite\":true"), "{}", body);

    drop(proxy);
    let _ = std::fs::remove_dir_all(&dir);
}
