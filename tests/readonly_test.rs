//! `PROXY_ENDPOINTS_READONLY` の結合テスト (T14.18)。
//!
//! 既定 (`off`) では今までどおりで、`on` にすると**書き換える口だけ** (`/purge` / `PURGE` /
//! `/blocklist?action=`) が 405 になる。読む口 (`/status`、判定だけの `/blocklist?host=`) は
//! そのまま通る。**認証ではない**ので、読める人は読めたままであることも一緒に確かめる。

mod common;
use common::*;

/// `PROXY_ENDPOINTS_READONLY` だけを変えたテスト用プロキシ。
fn readonly_proxy(on: bool) -> u16 {
    let mut cfg = proxy_config();
    cfg.endpoints_readonly = on;
    start_test_proxy(cfg)
}

/// 自分宛ての 1 要求を投げて応答全文を返す (状態行を見るので `endpoint_json` は使わない)。
fn get(port: u16, path: &str) -> String {
    raw_get(
        port,
        &format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            path, port
        ),
    )
}

#[test]
fn test_integration_readonly_refuses_only_the_write_endpoints_with_405() {
    let port = readonly_proxy(true);

    for path in [
        "/purge?all=1",
        "/purge?url=http://ro.example.com/x",
        "/blocklist?host=ro1.example.com&action=block",
        "/blocklist?host=ro1.example.com&action=clear",
    ] {
        let resp = get(port, path);
        assert!(
            resp.starts_with("HTTP/1.1 405 Method Not Allowed"),
            "{} -> {}",
            path,
            resp
        );
        assert!(resp.contains("PROXY_ENDPOINTS_READONLY"), "{}", resp);
    }

    // `PURGE <url>` (メソッドの方) も同じ
    let resp = raw_get(
        port,
        "PURGE http://ro.example.com/x HTTP/1.1\r\nHost: ro.example.com\r\nConnection: close\r\n\r\n",
    );
    assert!(
        resp.starts_with("HTTP/1.1 405 Method Not Allowed"),
        "{}",
        resp
    );

    // 読む口は今までどおり (認証ではない)
    for path in [
        "/status",
        "/healthz",
        "/metrics",
        "/blocklist?host=ro1.example.com",
    ] {
        let resp = get(port, path);
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "{} -> {}", path, resp);
    }
    // 断られた `action=block` が効いていないこと (405 は「読んで捨てた」ではなく書いていない)
    let resp = get(port, "/blocklist?host=ro1.example.com");
    assert!(resp.contains("\"blocked\":false"), "{}", resp);
}

#[test]
fn test_integration_endpoints_are_writable_by_default() {
    let port = readonly_proxy(false);

    let resp = get(port, "/purge?all=1");
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{}", resp);
    assert!(resp.contains("\"all\":true"), "{}", resp);

    let resp = get(port, "/blocklist?host=ro2.example.com&action=block");
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{}", resp);
    assert!(resp.contains("\"blocked\":true"), "{}", resp);
    // 後片付け (上書きの表はプロセス全体で 1 つ)
    let _ = get(port, "/blocklist?host=ro2.example.com&action=clear");
}

/// `.env` を書き換えると再起動なしで効くこと (T14.18)。
///
/// 実バイナリを `HOME` を差し替えて起こし、「ファイル → inotify → `reload::Live` →
/// 接続ごとの設定 → `Endpoint`」の配線を丸ごと見る。**待つのは `/status` ではなく
/// 起動ログの行**で、`.env` を読み直した瞬間が分かる。
#[test]
fn test_integration_readonly_follows_the_env_file() {
    let dir = std::env::temp_dir().join(format!("rhp-t1418-ro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let write_env = |v: &str| {
        std::fs::write(
            dir.join(".env"),
            format!(
                "SERVER_PORT=0\n\
                 PROXY_BIND=127.0.0.1\n\
                 PROXY_PROFILE=lite\n\
                 PROXY_LOG_LEVEL=info\n\
                 PROXY_ENDPOINTS_READONLY={}\n",
                v
            ),
        )
        .unwrap();
    };

    write_env("off");
    let proxy = ProxyProcess::start(&dir);
    let resp = get(proxy.port, "/purge?all=1");
    assert!(
        resp.starts_with("HTTP/1.1 200 OK"),
        "既定は今までどおり: {}",
        resp
    );

    write_env("on");
    proxy.wait_for_log("PROXY_ENDPOINTS_READONLY");
    let resp = get(proxy.port, "/purge?all=1");
    assert!(
        resp.starts_with("HTTP/1.1 405 Method Not Allowed"),
        "再読込で効く: {}",
        resp
    );
    let resp = get(proxy.port, "/status");
    assert!(
        resp.starts_with("HTTP/1.1 200 OK"),
        "読む口はそのまま: {}",
        resp
    );

    drop(proxy);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **状態を変える口は、宛先の名前が自分のときだけ受ける** (T.28.40、利用者の決定 1)。
///
/// 自分宛ての判定はポートだけなので、ポート 80 で動かすと外のサイトが
/// `http://任意:80/purge?all=1` を踏ませるだけでキャッシュを消せた。`Host`
/// (と絶対形式のホスト) が `localhost`・ループバックの IP・待ち受けの IP でなければ
/// 書き換える口 (`/purge`・`/blocklist?action=`) は**普通の転送に回す**。
/// 転送先は `PROXY_DENY_HOSTS` で断らせて (403)、名前解決にも外にも出ない形で確かめる。
/// 読む口 (`/lookup`・判定だけの `/blocklist?host=`) は名前によらず今までどおり。
#[test]
fn test_integration_write_endpoints_need_an_own_host_name() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(counter, "Cache-Control: max-age=60\r\n");
    let mut cfg = rust_http_proxy::config::Config::new(
        "0",
        None,
        Some("example.com"),
        Duration::from_secs(5),
    )
    .unwrap();
    cfg.keepalive = Duration::from_secs(2);
    cfg.allow_local = true;
    let port = start_test_proxy_with_cache(cfg, cache_cfg("rhp-t2840-own-name"));
    let host = format!("127.0.0.1:{}", origin_port);
    let url = format!("http://{}/asset", host);
    get_via_proxy(port, &url, &host);
    let lookup = |name: &str| {
        raw_get(
            port,
            &format!(
                "GET /lookup?url={} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
                url, name, port
            ),
        )
    };
    let found = lookup("127.0.0.1");
    assert!(found.contains("\"found\":true"), "{}", found);

    // 外の名前の書き換えは転送に回る (ここでは ACL の 403)。キャッシュは消えない
    for req in [
        format!(
            "GET /purge?all=1 HTTP/1.1\r\nHost: example.com:{}\r\nConnection: close\r\n\r\n",
            port
        ),
        format!(
            "GET http://example.com:{}/purge?all=1 HTTP/1.1\r\nHost: example.com:{}\r\nConnection: close\r\n\r\n",
            port, port
        ),
        format!(
            "GET /blocklist?host=own1.example.org&action=block HTTP/1.1\r\nHost: example.com:{}\r\nConnection: close\r\n\r\n",
            port
        ),
    ] {
        let resp = raw_get(port, &req);
        assert!(resp.starts_with("HTTP/1.1 403"), "{} -> {}", req, resp);
        assert!(!resp.contains("\"purged\""), "{}", resp);
    }
    // 読む口は外の名前でも今までどおり自分宛て (`/lookup` で消えていないことも示す)
    let found = lookup("example.com");
    assert!(found.contains("\"found\":true"), "{}", found);
    let resp = raw_get(
        port,
        &format!(
            "GET /blocklist?host=own1.example.org HTTP/1.1\r\nHost: example.com:{}\r\nConnection: close\r\n\r\n",
            port
        ),
    );
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{}", resp);
    assert!(
        resp.contains("\"blocked\":false"),
        "action=block は効いていない: {}",
        resp
    );

    // 自分の名前 (`localhost`・ループバックの IP) なら今までどおり効く
    let resp = raw_get(
        port,
        &format!(
            "GET /blocklist?host=own1.example.org&action=block HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
            port
        ),
    );
    assert!(resp.contains("\"blocked\":true"), "{}", resp);
    let _ = get(port, "/blocklist?host=own1.example.org&action=clear");
    let resp = get(port, "/purge?all=1");
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{}", resp);
    assert!(resp.contains("\"all\":true"), "{}", resp);
    let gone = lookup("127.0.0.1");
    assert!(gone.contains("\"found\":false"), "{}", gone);
}
