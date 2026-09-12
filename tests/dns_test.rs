//! 名前解決を 1 要求 1 回にする (T12.7)。
//!
//! ACL の「ローカル宛てか」の判定と、その直後の接続が同じ答えを使うことを、
//! `/status` の `dns.hits + dns.misses` の増分で見る (T12.7 の前は 1 要求で 2 回引いていた)。
//! 見るのは**引いた回数だけ**なので、宛先に届くかどうかは問わない (502 でも 200 でも
//! 名前解決の回数は同じ)。

mod common;

use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Mutex;
use std::time::Duration;

use common::*;
use rust_http_proxy::acl;

/// `/status` の DNS カウンタはプロセス全体で 1 組なので、この束は直列に回す。
static DNS_TEST_LOCK: Mutex<()> = Mutex::new(());

/// `/status` の `"dns"` の `hits + misses` (= 名前解決を試みた回数)。
fn dns_lookups(proxy_port: u16) -> u64 {
    let json = status_json(proxy_port);
    let at = json.find("\"dns\":").expect("no dns in /status");
    let dns = &json[at..];
    status_number(dns, "hits") + status_number(dns, "misses")
}

/// **ローカル宛てでない**名前を `/etc/hosts` から 1 つ探す (無ければ `None`)。
///
/// テストのオリジンは普通ループバックに置くが、それだと `PROXY_ALLOW_LOCAL` を切った
/// ときに 403 になり、判定のあとの接続まで進まない。判定と接続の両方を通すには
/// 「網に出ずに引けて、ループバックでもリンクローカルでもない名前」が要る。
/// そういう名前があるのは `/etc/hosts` だけなので、そこから選ぶ
/// (コンテナなら自分のホスト名がたいてい該当する)。
fn non_local_hostname() -> Option<String> {
    let hosts = std::fs::read_to_string("/etc/hosts").ok()?;
    for line in hosts.lines() {
        let line = line.split('#').next().unwrap_or("");
        // 先頭はアドレス、残りが名前
        for name in line.split_whitespace().skip(1) {
            let Ok(addrs) = (name, 80u16).to_socket_addrs() else {
                continue;
            };
            let addrs: Vec<_> = addrs.collect();
            // 1 つでもローカル宛てが混ざると 403 になる。IPv6 が混ざると
            // 待ち受けていない族を先に試して 250 ms 待つので IPv4 だけにする
            if !addrs.is_empty()
                && addrs
                    .iter()
                    .all(|a| a.is_ipv4() && !acl::is_local_ip(a.ip()))
            {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// 判定を通す設定 (`PROXY_ALLOW_LOCAL` は既定の off)。宛先には届かなくてよいので
/// 締め切りは短くする (届かない相手への CONNECT がテストを待たせないように)。
fn checking_config() -> rust_http_proxy::config::Config {
    let mut cfg = proxy_config();
    cfg.allow_local = false;
    cfg.timeout = Duration::from_secs(1);
    cfg
}

/// CONNECT 1 本と forward 1 本で、名前解決はそれぞれ 1 回だけ (T12.7)。
///
/// `PROXY_ALLOW_LOCAL` は既定 (off) なので、要求ごとにローカル宛ての判定が走る。
/// T12.7 の前はここで 1 回引き、接続でもう 1 回引いていた (1 要求 2 回)。
#[test]
fn a_request_resolves_the_name_once() {
    let _guard = DNS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(host) = non_local_hostname() else {
        eprintln!("no non-local name in /etc/hosts; skipping");
        return;
    };
    rust_http_proxy::dns::set_ttl(Duration::from_secs(60));
    rust_http_proxy::dns::clear();
    let proxy = start_test_proxy(checking_config());

    // CONNECT (トンネル)
    let before = dns_lookups(proxy);
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy)).unwrap();
    stream
        .write_all(format!("CONNECT {}:9 HTTP/1.1\r\n\r\n", host).as_bytes())
        .unwrap();
    let head = read_connect_response(&mut stream);
    assert!(
        !head.starts_with("HTTP/1.1 403"),
        "判定は通るはず: {}",
        head
    );
    assert_eq!(
        dns_lookups(proxy) - before,
        1,
        "CONNECT 1 本で引くのは 1 回 (判定の答えで繋ぐ)"
    );
    drop(stream);

    // forward (プールに無いので繋ぎに行く)
    let before = dns_lookups(proxy);
    let res = raw_get(
        proxy,
        &format!(
            "GET http://{h}:9/one HTTP/1.1\r\nHost: {h}:9\r\nConnection: close\r\n\r\n",
            h = host
        ),
    );
    assert!(!res.starts_with("HTTP/1.1 403"), "判定は通るはず: {}", res);
    assert_eq!(
        dns_lookups(proxy) - before,
        1,
        "forward の 1 本目も 1 回 (判定の答えで繋ぐ)"
    );
}

/// `PROXY_DNS_TTL_SECS=0` (キャッシュ無効) でも `getaddrinfo` は 1 要求 1 回 (T12.7)。
///
/// ここが 2 回だと、判定に使った答えと接続に使う答えが**別**になりうる
/// (DNS rebinding でローカル宛ての判定をすり抜けられる)。
#[test]
fn zero_ttl_still_resolves_once_per_request() {
    let _guard = DNS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(host) = non_local_hostname() else {
        eprintln!("no non-local name in /etc/hosts; skipping");
        return;
    };
    rust_http_proxy::dns::set_ttl(Duration::ZERO);
    rust_http_proxy::dns::clear();
    let proxy = start_test_proxy(checking_config());

    for i in 0..3 {
        let before = dns_lookups(proxy);
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy)).unwrap();
        stream
            .write_all(format!("CONNECT {}:9 HTTP/1.1\r\n\r\n", host).as_bytes())
            .unwrap();
        let head = read_connect_response(&mut stream);
        assert!(
            !head.starts_with("HTTP/1.1 403"),
            "判定は通るはず: {}",
            head
        );
        // キャッシュ無効なので毎回 OS に投げる = ミスが 1 だけ増える
        assert_eq!(
            dns_lookups(proxy) - before,
            1,
            "{} 本目: TTL 0 でも 1 要求 1 回",
            i + 1
        );
    }
    rust_http_proxy::dns::set_ttl(Duration::from_secs(60));
}

/// `/status` の `"dns"` 以降の切れ端 (この中の鍵を `status_number` で読む)。
fn dns_status(proxy_port: u16) -> String {
    let json = status_json(proxy_port);
    let at = json.find("\"dns\":").expect("no dns in /status");
    json[at..].to_string()
}

/// 直近 TTL 内に使われた名前は期限の 3/4 で裏で引き直され、`/status` の
/// `dns.refreshes` が増える (T13.1)。
///
/// 実時間を待つので TTL は 4 秒 (引き直しの窓は 3 秒〜4 秒)。窓を取りこぼさないように
/// 250 ms ごとに要求を出しながら待つ (取りこぼしても期限切れのミスから数え直すだけ)。
#[test]
fn a_hot_name_is_refreshed_in_the_background() {
    let _guard = DNS_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    rust_http_proxy::dns::set_ttl(Duration::from_secs(4));
    rust_http_proxy::dns::clear();
    let (origin, _origin) = start_mock_origin();
    // オリジンは 127.0.0.1 だが、**表を通るのは名前で来たときだけ**なので `localhost` で引く
    let proxy = start_test_proxy(proxy_config());
    let url = format!("http://localhost:{}/", origin);
    let host = format!("localhost:{}", origin);

    let dns = dns_status(proxy);
    assert!(dns.contains("\"negative_ttl_secs\":"), "{}", dns);
    let before = status_number(&dns, "refreshes");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let res = get_via_proxy(proxy, &url, &host);
        assert!(
            res.starts_with("HTTP/1.1 200"),
            "{}",
            &res[..res.len().min(80)]
        );
        if status_number(&dns_status(proxy), "refreshes") > before {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "期限の 3/4 を過ぎても裏で引き直していない"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    rust_http_proxy::dns::set_ttl(Duration::from_secs(60));
}
