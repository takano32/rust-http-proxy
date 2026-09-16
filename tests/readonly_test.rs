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
