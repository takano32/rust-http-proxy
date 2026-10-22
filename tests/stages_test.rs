//! 個票 (`/recent`) の段の値が 0 に潰れないこと (T15.0 (1)) の結合テスト。
//!
//! `detail_of` は接続の段を「全体 − 名前解決」で出すが、`PROXY_ALLOW_LOCAL=false`
//! (既定) では入口の ACL が `open()` の時計より**前に**名前を引くので、その費用を
//! 引くと `connect` が 0 に落ちる (デプロイ先の実測: ミスした個票 54 本のうち 40 本)。
//!
//! **ACL の枝そのものは結合テストでは再現できない** (`acl::is_local_ip` は loopback を
//! local と見るので `localhost` 宛ては 403、IP リテラル宛ては名前を引かずに早期 return
//! する)。引き算の中身は単体テスト (`crates/tunnel/src/tunnel.rs` の `mod tests` と
//! `crates/net-dns/src/dns_tests.rs` の `peeking_the_resolve_cost_does_not_take_it`) で見ているので、
//! ここで縛るのは**個票に `connect` と `client_read` の欄が出ること**だけ。
//!
//! `client_read` は 0 のとき JSON に出さない決まり (`STAGE_NAMES`) なので、要求行を
//! 送ってから `Host` を送るまでを空けて、0 でない値を作ってから読む。
//!
//! **空きが個票に出るまで送り直す** (T24.6)。負荷の下では proxy の最初の `read` が
//! 2 つの write をまとめて受け取ることがあり、その回は空きが「測る対象に存在しない」
//! (`client_read` は 0 = 欄が出ない)。空きを伸ばすと機械の速さを前提にした試験に
//! なるので、**同じ形の 1 本を、値が出るまで送り直す** (`gap_in_the_record`)。

use std::io::Write;
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

mod common;
use common::*;

/// `"<key>":[ ... ]` を 1 件ずつに割る (`ms` や `rtt_ms` が入れ子なので括弧を数える)。
fn rows(body: &str, key: &str) -> Vec<String> {
    let head = format!("\"{}\":[", key);
    let at = body
        .find(&head)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, body))
        + head.len();
    let rest = &body[at..];
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    out.push(rest[start..=i].to_string());
                }
            }
            ']' if depth == 0 => break,
            _ => {}
        }
    }
    out
}

/// `"key":<数>` を読む (整数だけ)。
fn num(obj: &str, key: &str) -> u64 {
    let pat = format!("\"{}\":", key);
    let at = obj
        .find(&pat)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, obj))
        + pat.len();
    obj[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} が数でない: {}", key, obj))
}

/// 要求行と `Host` の間を空ける幅 (測る対象の「空き」)。
const GAP: Duration = Duration::from_millis(40);
/// 個票の `client_read` がこれ以上なら「空きが測れた」と見る (丸めのぶんだけ下に置く)。
const GAP_MS: u64 = 30;
/// 同じ形の 1 本を送り直す上限 (負荷の下で 1 回も測れないことは無い幅)。
const TRIES: usize = 10;

/// `/recent` の行のうち `needle` を含むものの**いちばん新しい 1 件** (`id` が最大)。
fn newest_row(body: &str, needle: &str) -> Option<String> {
    rows(body, "recent")
        .into_iter()
        .filter(|r| r.contains(needle))
        .max_by_key(|r| num(r, "id"))
}

/// `/recent?n=50` を引いて、`needle` を含む行の最大の `id` (無ければ `None`)。
fn newest_id(proxy_port: u16, needle: &str) -> Option<u64> {
    newest_row(&endpoint_json(proxy_port, "/recent?n=50"), needle).map(|r| num(&r, "id"))
}

/// 個票の `client_read` (0 のときは欄そのものが出ないので `None`)。
fn client_read_ms(entry: &str) -> Option<u64> {
    entry
        .contains("\"client_read\":")
        .then(|| num(entry, "client_read"))
}

/// 空けた間が個票の `client_read` に出るまで、同じ形の 1 本を送り直して、その個票を返す。
///
/// **時間で待たない**: 負荷の下では proxy の最初の `read` が 2 つの write をまとめて
/// 受け取るので、その回の空きは測る対象に存在しない (`client_read` は 0 = 欄が出ない)。
/// 空きを伸ばす代わりに、値が出るまで**同じ形で送り直す** (何回で出るかは機械の
/// 混み具合が決める)。1 回も出なければ、見た値を全部並べて落ちる — `client_read` が
/// 常に 0 に潰れる退行は、上限まで送っても値が出ないので必ず落ちる。
fn gap_in_the_record(
    proxy_port: u16,
    needle: &str,
    what: &str,
    mut one_request: impl FnMut(),
) -> String {
    // 前の回の行と取り違えないよう、いまの最大の `id` から始める
    let mut last = newest_id(proxy_port, needle).unwrap_or(0);
    let mut seen = Vec::new();
    for _ in 1..=TRIES {
        one_request();
        // 個票は接続が閉じたあとに積まれるので、**id が増えた行**が出るまで待つ
        wait_until(
            || newest_id(proxy_port, needle).is_some_and(|id| id > last),
            what,
        );
        let body = endpoint_json(proxy_port, "/recent?n=50");
        let entry = newest_row(&body, needle).unwrap_or_else(|| panic!("{}: {}", what, body));
        last = num(&entry, "id");
        match client_read_ms(&entry) {
            Some(ms) if ms >= GAP_MS => return entry,
            other => seen.push(match other {
                Some(ms) => format!("{} ms", ms),
                None => "欄なし".to_string(),
            }),
        }
    }
    panic!(
        "{}: {} 回送っても {} ms の空きが `client_read` に出なかった (閾 {} ms。見た値: {})",
        what,
        TRIES,
        GAP.as_millis(),
        GAP_MS,
        seen.join(", ")
    );
}

/// 要求行と `Host` の間を空けて CONNECT を 1 本張る (`client_read` を 0 でなくする)。
fn slow_tunnel(proxy_port: u16, target: &str) {
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(format!("CONNECT {} HTTP/1.1\r\n", target).as_bytes())
        .unwrap();
    s.flush().unwrap();
    thread::sleep(GAP);
    s.write_all(format!("Host: {}\r\n\r\n", target).as_bytes())
        .unwrap();
    let head = read_connect_response(&mut s);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    drop(s);
}

/// 受け入れ基準: 個票の `ms` に `connect` と `client_read` があり、どちらも
/// 「起きたこと」と矛盾しない値であること (loopback なので `dns` は 0)。
#[test]
fn a_tunnel_records_both_the_connect_and_the_client_read_stage() {
    // 段階の窓と同じ旗 (`--lite` では時計を読まない。T14.3)
    rust_http_proxy::profile::set_enabled(true);

    let echo_port = start_echo_server();
    let proxy_port = start_test_proxy(proxy_config());
    let target = format!("127.0.0.1:{}", echo_port);

    // 空けた 40 ms が個票に出るまで送り直す (`client_read` は 0 なら欄が出ない)
    let entry = gap_in_the_record(
        proxy_port,
        &format!("\"target\":\"{}\"", target),
        "/recent に空きの入った CONNECT",
        || slow_tunnel(proxy_port, &target),
    );

    // `dns` と `connect` は 0 でも必ず出る欄 (`STAGE_NAMES`)
    assert!(entry.contains("\"dns\":"), "`dns` の欄が無い: {}", entry);
    assert!(
        entry.contains("\"connect\":"),
        "`connect` の欄が無い: {}",
        entry
    );
    // 要求行と `Host` の間を空けたので、この段は 0 でない = JSON に出る
    assert!(
        entry.contains("\"client_read\":"),
        "`client_read` の欄が無い: {}",
        entry
    );
    assert!(
        num(&entry, "client_read") >= GAP_MS,
        "空けた {} ms が入っていない: {}",
        GAP.as_millis(),
        entry
    );
    // loopback は名前を引かないので、引き算に使う値そのものが 0
    assert_eq!(num(&entry, "dns"), 0, "{}", entry);
    // 「`dns` が 0 より大きいのに `connect` が 0」が出ないこと (受け入れ基準)
    assert!(
        num(&entry, "dns") == 0 || num(&entry, "connect") > 0,
        "名前解決を払ったのに接続の段が 0: {}",
        entry
    );
}

/// forward でオリジンを掴めなかった (502) 要求でも、個票の `client_read` が 0 に
/// 潰れないこと (T17.4)。
///
/// 掴めなかった枝は `origin_detail` に本体クレートの段 (`queue` だけ) を渡していて、
/// 入口で測った `client_read` を落としていた。閉じたポートへ、要求行と `Host` の間を
/// 空けて forward の GET を 1 本送り、`/recent` の http の 1 件を読む。
#[test]
fn a_failed_forward_keeps_its_client_read_stage() {
    rust_http_proxy::profile::set_enabled(true);

    let dead_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let proxy_port = start_test_proxy(proxy_config());
    let host = format!("127.0.0.1:{}", dead_port);

    // 要求行と `Host` の間を空けた forward。空きが個票に出るまで送り直す (T24.6)
    let entry = gap_in_the_record(
        proxy_port,
        "\"kind\":\"http\"",
        "/recent に 502 の 1 本",
        || {
            let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            s.write_all(format!("GET http://{}/t174 HTTP/1.1\r\n", host).as_bytes())
                .unwrap();
            s.flush().unwrap();
            thread::sleep(GAP);
            s.write_all(format!("Host: {}\r\nConnection: close\r\n\r\n", host).as_bytes())
                .unwrap();
            let (head, _) = read_response(&mut s);
            assert!(head.starts_with("HTTP/1.1 502"), "{}", head);
            drop(s);
        },
    );
    // 0 の段は JSON に出ない決まりなので、欄があること自体が「0 でない」印
    assert!(
        entry.contains("\"client_read\":"),
        "502 の個票で `client_read` が 0 に落ちた: {}",
        entry
    );
    assert!(
        num(&entry, "client_read") >= GAP_MS,
        "空けた {} ms が入っていない: {}",
        GAP.as_millis(),
        entry
    );
}
