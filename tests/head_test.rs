//! 内部エンドポイントの HEAD と 405 の結合テスト (T.28.7)。
//!
//! HEAD は GET として組んで本文だけ書かない (RFC 9110 §9.3.2)。GET / HEAD / PURGE 以外は
//! `405` + `Allow: GET, HEAD`、書き換える口の HEAD は何もせず `405` + `Allow: GET`、
//! `/favicon.ico` は `204`。今までは全部 404 と案内 2,864 B だった。

mod common;
use common::*;

/// 自分宛ての 1 要求を `method` で投げて応答全文を返す。
fn send(port: u16, method: &str, path: &str) -> String {
    raw_get(
        port,
        &format!(
            "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            method, path, port
        ),
    )
}

/// (状態行, `Content-Length` の値, 本文のバイト数)。
fn parts(resp: &str) -> (String, Option<usize>, usize) {
    let (head, body) = resp.split_once("\r\n\r\n").expect("ヘッダーの終わり");
    let status = head.lines().next().unwrap_or("").to_string();
    let len = head.lines().find_map(|l| {
        l.strip_prefix("Content-Length: ")
            .and_then(|v| v.trim().parse().ok())
    });
    (status, len, body.len())
}

#[test]
fn test_integration_head_is_get_without_the_body() {
    let port = start_test_proxy(proxy_config());

    // 本文が要求ごとに変わらない口は、GET と HEAD で `Content-Length` がぴったり同じ
    for path in ["/proxy.pac", "/dashboard"] {
        let (gs, glen, gbody) = parts(&send(port, "GET", path));
        let (hs, hlen, hbody) = parts(&send(port, "HEAD", path));
        assert_eq!(gs, "HTTP/1.1 200 OK", "{}", path);
        assert_eq!(hs, "HTTP/1.1 200 OK", "{}", path);
        assert_eq!(glen, Some(gbody), "{}", path);
        assert_eq!(hlen, glen, "{}: HEAD は GET と同じ Content-Length", path);
        assert_eq!(hbody, 0, "{}: HEAD は本文 0 B", path);
    }

    // `/healthz` は時刻で桁が動くことがあるので、同じ長さになるまで 3 回まで引き直す
    let mut same = false;
    for _ in 0..3 {
        let (gs, glen, _) = parts(&send(port, "GET", "/healthz"));
        let (hs, hlen, hbody) = parts(&send(port, "HEAD", "/healthz"));
        assert_eq!(gs, "HTTP/1.1 200 OK");
        assert_eq!(hs, "HTTP/1.1 200 OK", "HEAD /healthz は 200");
        assert_eq!(hbody, 0, "HEAD /healthz は本文 0 B");
        assert!(hlen.is_some_and(|n| n > 0), "Content-Length がある");
        if hlen == glen {
            same = true;
            break;
        }
    }
    assert!(same, "HEAD /healthz の Content-Length が GET と同じ");

    // `/status` と `/metrics` も HEAD で 200 (本文は書かない)
    for path in ["/status", "/metrics"] {
        let (hs, hlen, hbody) = parts(&send(port, "HEAD", path));
        assert_eq!(hs, "HTTP/1.1 200 OK", "{}", path);
        assert!(hlen.is_some_and(|n| n > 0), "{}", path);
        assert_eq!(hbody, 0, "{}", path);
    }
}

#[test]
fn test_integration_other_methods_get_405_with_allow() {
    let port = start_test_proxy(proxy_config());

    for method in ["POST", "PUT", "DELETE", "OPTIONS"] {
        let resp = send(port, method, "/status");
        assert!(
            resp.starts_with("HTTP/1.1 405 Method Not Allowed"),
            "{} -> {}",
            method,
            resp
        );
        assert!(resp.contains("\r\nAllow: GET, HEAD\r\n"), "{}", resp);
        // 案内 (2.8 KB) は付けない
        assert!(!resp.contains("not found"), "{}", resp);
    }

    // 書き換える口は HEAD では何もしない (405 + `Allow: GET`)
    let resp = send(port, "HEAD", "/purge?all=1");
    assert!(
        resp.starts_with("HTTP/1.1 405 Method Not Allowed"),
        "{}",
        resp
    );
    assert!(resp.contains("\r\nAllow: GET\r\n"), "{}", resp);

    // ブラウザが取りに来るアイコンは 204 (本文も `Content-Length` も無し)
    for method in ["GET", "HEAD"] {
        let resp = send(port, method, "/favicon.ico");
        assert!(resp.starts_with("HTTP/1.1 204 No Content"), "{}", resp);
        assert!(!resp.contains("Content-Length"), "{}", resp);
        assert!(resp.ends_with("\r\n\r\n"), "{}", resp);
    }

    // 知らないパスの GET は今までどおり 404 と案内
    let resp = send(port, "GET", "/no-such-endpoint");
    assert!(resp.starts_with("HTTP/1.1 404 Not Found"), "{}", resp);
}
