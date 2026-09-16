//! 全エンドポイントの応答の先頭に `"schema":N` があることの結合テスト (T14.49)。
//!
//! JSON の形は Phase ごとに増えていて、読む道具 (`scripts/status-diff.py`
//! `scripts/snapshot-diff.py` `scripts/check-dashboard.js`) が「この JSON はどの版か」を
//! `parts` の有無などで**推測**していた。応答の**先頭の鍵**に版を書けば推測が要らない。
//!
//! ここで見るのは 3 つ:
//!
//! 1. **`/` の案内に載っている口を機械的に取り出して**、JSON を返す口の応答の
//!    **先頭 64 バイト**に `"schema":` があること (一覧を手で写さないので、口が増えたときに
//!    漏れない。案内に新しい口が出たのにこのテストが知らなければ、その場で落ちる)
//! 2. **エラーの応答 (400 / 404 / 405) にも版がある**こと (読む道具は成功も失敗も同じ入口で読む)
//! 3. **`/snapshot` は外側と各部の両方**に版があること (各部はそれぞれの口の出力そのものなので、
//!    切り出して 1 本の応答として読める)

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;

use common::*;

/// 版が入っていなければならない先頭のバイト数 (T14.49 の受け入れ基準)。
const HEAD: usize = 64;

/// JSON を返さない口 (この一覧にある口は 1 の確認から外す)。**外す理由も一緒に持つ**ので、
/// 新しい口が案内に出たときは「JSON か、そうでないか」をここで 1 度考えることになる。
const NOT_JSON: &[(&str, &str)] = &[
    ("/dashboard", "HTML (コントロールパネル)"),
    ("/inspect", "HTML (「調査」ページ)"),
    ("/probe.html", "HTML (「端末から測る」ページ)"),
    ("/metrics", "Prometheus の text 形式"),
    ("/proxy.pac", "ブラウザの自動設定スクリプト"),
];

/// 案内に出ている口を、実際に引くときの問い合わせ (`<name>` のような雛形を実際の値にする)。
///
/// **案内に出た口はここか [`NOT_JSON`] のどちらかに無ければならない** (下の `endpoint_paths`
/// が突き合わせる)。`/purge` は書き換える口だが、消すものが無い状態で引くので副作用は無い。
const QUERIES: &[(&str, &str)] = &[
    ("/status", "/status"),
    ("/status", "/status?sort=errors"),
    ("/errors", "/errors?n=100"),
    ("/connections", "/connections"),
    ("/recent", "/recent?n=200"),
    ("/bursts", "/bursts?n=50"),
    ("/trace", "/trace?n=200"),
    ("/events", "/events?n=200"),
    ("/snapshot", "/snapshot"),
    ("/snapshots", "/snapshots"),
    // 置いていない日 (404 も JSON)
    ("/snapshots/", "/snapshots/2000-01-01"),
    ("/dns", "/dns?sort=age"),
    ("/log", "/log?n=200"),
    ("/hosts", "/hosts?limit=200"),
    ("/hosts/series", "/hosts/series?top=16"),
    ("/clients", "/clients?limit=200"),
    ("/explain", "/explain?host=example.com"),
    ("/explain", "/explain?client=198.51.100.7"),
    ("/config", "/config"),
    ("/healthz", "/healthz"),
    ("/history", "/history?res=5"),
    ("/history", "/history?res=60&n=10"),
    ("/history", "/history?since=0&summary=1"),
    ("/profile", "/profile?res=5"),
    ("/slo", "/slo?days=7"),
    ("/daily", "/daily?n=365"),
    ("/lookup", "/lookup?url=http://example.com/x"),
    ("/purge", "/purge?url=http://example.com/x"),
    ("/blocklist", "/blocklist"),
    ("/blocklist", "/blocklist?host=example.com"),
];

/// `/` の案内から口のパスを機械的に取り出す (一覧を手で写さないため)。
///
/// 案内の 1 行は `  /recent?n=200&since=&client=&sort=          JSON: …` の形なので、
/// 行頭の空白の後ろの 1 語を取り、`?` 以降と `[...]` を落として `/` で始まるものだけ残す。
/// `/snapshots/<YYYY-MM-DD>` のような雛形は `/snapshots/` までにする。
fn endpoint_paths(index: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in index.lines() {
        let Some(word) = line.strip_prefix("  ").map(str::trim) else {
            continue;
        };
        let word = word.split_whitespace().next().unwrap_or("");
        if !word.starts_with('/') {
            continue;
        }
        let mut path = word.split(['?', '[']).next().unwrap_or("").to_string();
        // `/snapshots/<YYYY-MM-DD>` は日付の雛形を落として `/snapshots/` に
        if let Some(at) = path.find('<') {
            path.truncate(at);
        }
        if !path.is_empty() && !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// 自分宛ての GET を 1 本投げて (状態コード, 本文) を返す (200 でなくてもよい)。
fn get_body(proxy_port: u16, path: &str) -> (u16, String) {
    let out = raw_get(
        proxy_port,
        &format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            path, proxy_port
        ),
    );
    let status = out
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("{} の応答が読めない: {}", path, out));
    let body = match out.split_once("\r\n\r\n") {
        Some((_, b)) => b.to_string(),
        None => String::new(),
    };
    (status, body)
}

/// 先頭 [`HEAD`] バイトに `"schema":` があること。
fn assert_schema(what: &str, body: &str) {
    let head = &body[..HEAD.min(body.len())];
    assert!(
        head.contains("\"schema\":"),
        "{} の先頭 {} バイトに版が無い: {}",
        what,
        HEAD,
        head
    );
    // 版 1 = 2026-09-16 の Phase 14 の形 (`crates/metrics` の `SCHEMA`)
    assert!(
        head.contains("\"schema\":1,") || head.contains("\"schema\":1}"),
        "{} の版が 1 ではない: {}",
        what,
        head
    );
}

/// 通した接続が個票と統計に乗るように、CONNECT を 1 本張って 1 往復して閉じる。
fn connect_once(proxy_port: u16, target: &str) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "CONNECT {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: t1449/1.0\r\n\r\n",
        target, target
    );
    stream.write_all(req.as_bytes()).unwrap();
    let head = read_connect_response(&mut stream);
    assert!(head.starts_with("HTTP/1.1 200"), "{} -> {}", target, head);
    stream.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
}

/// 1. `/` の案内に載っている JSON の口の**全部**に版があること。
#[test]
fn test_integration_every_endpoint_starts_with_the_schema_version() {
    let echo = start_echo_server();
    let proxy_port = start_test_proxy(proxy_config());
    // 個票と統計が空でない状態で引く (空の一覧でだけ通るテストにしないため)。
    // `localhost` は IP リテラルではないので `/dns` の表にも 1 行残る
    connect_once(proxy_port, &format!("localhost:{}", echo));

    let (status, index) = get_body(proxy_port, "/");
    assert_eq!(status, 200, "{}", index);
    let listed = endpoint_paths(&index);
    assert!(listed.len() >= 20, "案内の口が少なすぎる: {:?}", listed);

    // 案内に出た口は、JSON でない一覧か、引き方の一覧のどちらかに必ずある
    for path in &listed {
        let known = NOT_JSON.iter().any(|(p, _)| p == path)
            || QUERIES.iter().any(|(p, _)| p == path)
            // `/snapshots/<YYYY-MM-DD>` は `/snapshots/` として持っている
            || QUERIES.iter().any(|(p, _)| *p == format!("{}/", path));
        assert!(
            known,
            "`/` の案内に知らない口がある: {} (tests/schema_test.rs の QUERIES か NOT_JSON に足すこと)",
            path
        );
    }

    let mut checked = 0;
    for (path, query) in QUERIES {
        // 案内に出ていない口を確かめ続けないように、こちらからも突き合わせる
        assert!(
            listed
                .iter()
                .any(|p| p == path || p == &path[..path.len() - usize::from(path.ends_with('/'))]),
            "QUERIES の {} が `/` の案内に無い (口を消したらここも消すこと)",
            path
        );
        let (status, body) = get_body(proxy_port, query);
        assert!(
            (200..=499).contains(&status),
            "{} -> {} {}",
            query,
            status,
            body
        );
        assert_schema(query, &body);
        checked += 1;
    }
    assert_eq!(checked, QUERIES.len());

    // JSON でない口は今までどおり (版を足していない = 本文の形を変えていない)
    for (path, why) in NOT_JSON {
        let (status, body) = get_body(proxy_port, path);
        assert_eq!(status, 200, "{} ({}) -> {}", path, why, body);
        assert!(
            !body.starts_with("{\"schema\""),
            "{} ({}) は JSON ではないのに版が付いている",
            path,
            why
        );
    }
}

/// 2. エラーの応答 (400 / 404 / 405) にも版があること。
///
/// 読む道具は成功も失敗も同じ入口で読むので、**失敗だけ版が無い**と分岐が 2 つに割れる。
#[test]
fn test_integration_error_bodies_carry_the_schema_version_too() {
    // `PROXY_ENDPOINTS_READONLY=on` の 405 も JSON (T14.18)
    let mut readonly = proxy_config();
    readonly.endpoints_readonly = true;
    let ro_port = start_test_proxy(readonly);
    let (status, body) = get_body(ro_port, "/purge?all=1");
    assert_eq!(status, 405, "{}", body);
    assert_schema("/purge?all=1 (read-only)", &body);

    let proxy_port = start_test_proxy(proxy_config());
    for (query, want) in [
        ("/lookup", 400u16),
        // ホストの無い URL (`parse_origin` が断る = `{"error":"invalid url"}`)
        ("/lookup?url=http://", 400),
        ("/purge", 400),
        ("/purge?url=http://", 400),
        ("/explain", 400),
        ("/blocklist?host=notadomain", 400),
        ("/blocklist?host=example.com&action=nonsense", 400),
        ("/snapshots/not-a-date", 404),
        ("/lookup?url=http://example.com/never-stored", 404),
    ] {
        let (status, body) = get_body(proxy_port, query);
        assert_eq!(status, want, "{} -> {}", query, body);
        assert_schema(query, &body);
    }
}

/// 3. `/snapshot` は外側と**各部**の両方に版があること。
///
/// 部はそれぞれの口の出力そのものなので、切り出して 1 本の応答として読める
/// (`scripts/snapshot-diff.py` の `--from-files` は逆に、1 本ずつのファイルから雪像を組む)。
#[test]
fn test_integration_snapshot_carries_the_version_on_every_part() {
    let echo = start_echo_server();
    let proxy_port = start_test_proxy(proxy_config());
    connect_once(proxy_port, &format!("localhost:{}", echo));

    let (status, body) = get_body(proxy_port, "/snapshot");
    assert_eq!(status, 200);
    assert_schema("/snapshot", &body);

    // `"parts":["status","status_errors",…]` に並んだ名前ぶん、その値の先頭に版が要る
    let top = members(&body);
    let parts: Vec<String> = top
        .iter()
        .find(|(k, _)| k == "parts")
        .map(|(_, v)| v.trim_matches(['[', ']'].as_slice()))
        .unwrap_or_else(|| panic!("/snapshot に parts が無い"))
        .split(',')
        .map(|s| s.trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    assert!(parts.len() >= 15, "部が少なすぎる: {:?}", parts);
    let history = top
        .iter()
        .find(|(k, _)| k == "history")
        .map(|(_, v)| members(v))
        .unwrap_or_default();
    let mut seen = 0;
    for name in &parts {
        // `history.5` などは `"history":{"5":{…}}` に入れ子になっている
        let (list, key) = match name.split_once('.') {
            Some((_, res)) => (&history, res.to_string()),
            None => (&top, name.clone()),
        };
        let value = list
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("/snapshot に部 {} が無い", name));
        assert!(
            value.starts_with("{\"schema\":1,"),
            "/snapshot の部 {} の先頭に版が無い: {}",
            name,
            &value[..HEAD.min(value.len())]
        );
        seen += 1;
    }
    assert_eq!(seen, parts.len());
}

/// JSON の object の**いちばん外側の鍵と値**を順に返す (入れ子には降りない)。
///
/// `/snapshot` の部を鍵で探すのに、文字列の中の `{` や、`/status` の入れ子の `"dns"` を
/// 数え違えないため (`body.split_once("\"dns\":{")` は `/status` の中の `dns` に当たる)。
/// テストの中だけの素朴な走査で、値は切り出した部分文字列をそのまま返す。
fn members(obj: &str) -> Vec<(String, &str)> {
    let b = obj.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    // 先頭の `{`
    while i < b.len() && b[i] != b'{' {
        i += 1;
    }
    i += 1;
    loop {
        while i < b.len() && (b[i] == b',' || b[i].is_ascii_whitespace()) {
            i += 1;
        }
        if i >= b.len() || b[i] == b'}' {
            return out;
        }
        assert_eq!(b[i], b'"', "鍵が文字列で始まっていない: {}", &obj[i..]);
        let (key, next) = scan_string(obj, i);
        i = next;
        while i < b.len() && (b[i] == b':' || b[i].is_ascii_whitespace()) {
            i += 1;
        }
        let start = i;
        i = scan_value(obj, i);
        out.push((key, &obj[start..i]));
    }
}

/// `i` にある JSON の文字列を読む (返すのは中身と、閉じ `"` の次の位置)。
fn scan_string(s: &str, i: usize) -> (String, usize) {
    let b = s.as_bytes();
    let mut j = i + 1;
    while j < b.len() && b[j] != b'"' {
        j += if b[j] == b'\\' { 2 } else { 1 };
    }
    (s[i + 1..j].to_string(), j + 1)
}

/// `i` にある JSON の値を読み飛ばす (返すのは値の終わりの次の位置)。
fn scan_value(s: &str, i: usize) -> usize {
    let b = s.as_bytes();
    let (mut j, mut depth) = (i, 0usize);
    while j < b.len() {
        match b[j] {
            b'"' => j = scan_string(s, j).1 - 1,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                if depth == 0 {
                    return j;
                }
                depth -= 1;
                if depth == 0 {
                    return j + 1;
                }
            }
            b',' if depth == 0 => return j,
            _ => {}
        }
        j += 1;
    }
    j
}
