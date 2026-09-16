//! 山の写真 `/bursts` と、閉じた接続の分布 (`/history` の `closed`) の結合テスト (T14.6)。
//!
//! T13.2 (上限に当たったら暇なトンネルを 1 本閉じる) の効きは「バーストが来たとき」に
//! しか見えないが、来たときに `/connections` を見ている人はいない。同時接続数が上限の
//! 一定割合を越えた瞬間に自動で 1 枚撮っておくのがここで見るもの。
//!
//! **写真を待つのに `/bursts` を使わない**: `/bursts` 自体が 1 本の接続なので、
//! 取りに行く行為が同時接続数を 1 増やして写真の中身を変える (`tests/overload_test.rs`
//! と同じ理由)。待つのは指標を直に読んで行い、`/bursts` は最後に 1 回だけ叩く。
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

mod common;
use common::*;

use rust_http_proxy::metrics::Metrics;

/// history スレッドの周期 (本番は 5 秒。テストは短くする)。
const TICK: Duration = Duration::from_millis(50);

/// 上限 `max_conns` 本・閾 `percent`% のテスト用プロキシ (履歴スレッド付き)。
fn burst_proxy(max_conns: usize, percent: usize) -> (u16, Arc<Metrics>) {
    let mut cfg = park_config();
    cfg.max_conns = max_conns;
    cfg.burst_percent = percent;
    // 欄を直に書き換えたので閾を計算し直す (`from_env` が最後にしているのと同じこと)
    cfg.refresh_burst_at();
    // 握ったままの接続が預かり所の期限で閉じないように長くする
    cfg.keepalive = Duration::from_secs(60);
    start_test_proxy_with_history(cfg, TICK)
}

/// CONNECT を張って `200` まで読む。
fn open_tunnel(proxy_port: u16, target_port: u16) -> TcpStream {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    stream.set_nodelay(true).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
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

/// history スレッドが `n` 周期ぶん回るまで待つ (標本が増えるのを数える)。
fn wait_ticks(metrics: &Metrics, n: usize) {
    let want = metrics.history.len() + n;
    wait_until(|| metrics.history.len() >= want, "history thread to tick");
}

/// 閾を越えた瞬間に 1 枚だけ撮り、山が引いてから越え直すと 2 枚目を撮ること。
///
/// 受け入れ基準そのもの: `PROXY_MAX_CONNS=8` `PROXY_BURST_PERCENT=50` (= 閾 4) で
/// **5 本目のトンネルで 1 枚**、6〜8 本目では増えず、2 本まで減ってから再び 5 本で 2 枚目。
#[test]
fn test_integration_a_spike_is_photographed_once_per_mountain() {
    let echo_port = start_echo_server();
    let (proxy_port, metrics) = burst_proxy(8, 50);

    // 閾 (4) までは撮らない
    let mut tunnels: Vec<TcpStream> = (0..4).map(|_| open_tunnel(proxy_port, echo_port)).collect();
    wait_until(
        || metrics.active_connections.load(Ordering::Relaxed) == 4,
        "four tunnels to be open",
    );
    wait_ticks(&metrics, 2);
    assert_eq!(metrics.bursts.len(), 0, "閾ちょうどでは撮らない");
    assert!(metrics.bursts.armed(), "まだ 1 枚も撮っていない");

    // 5 本目で 1 枚
    tunnels.push(open_tunnel(proxy_port, echo_port));
    wait_until(|| metrics.bursts.len() == 1, "the first burst shot");
    assert!(!metrics.bursts.armed(), "撮ったら旗は下りる");

    // 6〜8 本目では増えない (同じ山)
    for _ in 0..3 {
        tunnels.push(open_tunnel(proxy_port, echo_port));
    }
    wait_until(
        || metrics.active_connections.load(Ordering::Relaxed) == 8,
        "eight tunnels to be open",
    );
    wait_ticks(&metrics, 2);
    assert_eq!(metrics.bursts.len(), 1, "同じ山では 1 枚だけ");

    // 2 本まで減らすと次の 1 枚に備える
    tunnels.truncate(2);
    wait_until(
        || metrics.active_connections.load(Ordering::Relaxed) == 2,
        "the spike to pass",
    );
    wait_until(|| metrics.bursts.armed(), "the ring to re-arm");

    // 再び 5 本で 2 枚目
    for _ in 0..3 {
        tunnels.push(open_tunnel(proxy_port, echo_port));
    }
    wait_until(|| metrics.bursts.len() == 2, "the second burst shot");

    // ここまで指標だけで見てきた。`/bursts` を叩くのは最後の 1 回だけ (接続が 1 本増える)
    let json = endpoint_json(proxy_port, "/bursts");
    assert!(json.contains("\"count\":2"), "{}", json);
    assert!(json.contains("\"kept\":2"), "{}", json);
    assert!(json.contains("\"capacity\":50"), "{}", json);
    assert!(json.contains("\"recorded\":2"), "{}", json);
    assert!(json.contains("\"threshold\":4"), "{}", json);
    assert!(json.contains("\"max_conns\":8"), "{}", json);
    assert!(!json.contains("\"truncated\":true"), "{}", json);
    assert!(!json.contains("\"lite\":true"), "{}", json);
    // 新しい順 (2 枚目が先頭)
    assert!(json.contains("\"seq\":2"), "{}", json);
    assert!(json.contains("\"seq\":1"), "{}", json);
    let first = json.find("\"seq\":2").unwrap();
    let second = json.find("\"seq\":1").unwrap();
    assert!(first < second, "新しい順に並ぶ: {}", json);

    // 1 枚目の中身: 5 本、接続元 1 つ、宛先の上位は試験のホスト、状態は parked か relaying。
    // `"seq":1` から後ろが 1 枚目の中身 (新しい順なので 1 枚目は末尾)
    let shot = &json[second..];
    assert!(shot.contains("\"active\":5"), "{}", shot);
    assert!(shot.contains("\"trigger_active\":5"), "{}", shot);
    assert!(
        shot.contains("{\"client\":\"127.0.0.1\",\"conns\":5}"),
        "接続元 1 つ 5 本が無い: {}",
        shot
    );
    assert!(shot.contains("\"clients_distinct\":1"), "{}", shot);
    assert!(
        shot.contains(&format!(
            "{{\"target\":\"127.0.0.1:{}\",\"conns\":5}}",
            echo_port
        )),
        "宛先の上位に試験のホストが無い: {}",
        shot
    );
    assert!(
        shot.contains("\"kinds\":{\"connect\":5,\"http\":0}"),
        "{}",
        shot
    );
    // 状態別は 5 本ぶんが `parked` か `relaying` に入っていること
    let field = |key: &str| -> u32 {
        shot.split_once(&format!("\"{}\":", key))
            .and_then(|(_, rest)| rest.split([',', '}']).next())
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0)
    };
    assert_eq!(
        field("parked") + field("relaying"),
        5,
        "状態別の内訳が 5 本にならない: {}",
        shot
    );
    // スレッド数と記述子も載る (Linux)
    assert!(shot.contains("\"threads\":"), "{}", shot);
    assert!(shot.contains("\"fds\":"), "{}", shot);
    assert!(shot.contains("\"evicted_idle\":"), "{}", shot);
    assert!(shot.contains("\"rejected_overload\":"), "{}", shot);
    drop(tunnels);
}

/// `PROXY_BURST_PERCENT=0` なら 1 枚も撮らない (`/bursts` は空で 200)。
#[test]
fn test_integration_zero_percent_takes_no_photos() {
    let echo_port = start_echo_server();
    let (proxy_port, metrics) = burst_proxy(4, 0);
    let tunnels: Vec<TcpStream> = (0..4).map(|_| open_tunnel(proxy_port, echo_port)).collect();
    wait_until(
        || metrics.active_connections.load(Ordering::Relaxed) == 4,
        "four tunnels to be open",
    );
    wait_ticks(&metrics, 2);
    assert_eq!(metrics.bursts.len(), 0, "0% は撮らない");
    let json = endpoint_json(proxy_port, "/bursts");
    assert!(json.contains("\"bursts\":[]"), "{}", json);
    assert!(json.contains("\"recorded\":0"), "{}", json);
    assert!(json.contains("\"threshold\":0"), "{}", json);
    drop(tunnels);
}

/// 閉じた接続の分布が `/history` の `closed` に出ること (理由・寿命・バイト)。
#[test]
fn test_integration_history_carries_the_closed_connection_distribution() {
    let echo_port = start_echo_server();
    let (proxy_port, metrics) = burst_proxy(8, 50);

    // トンネルを 1 本張って少し流し、**クライアントが先に EOF を出して**閉じる
    // (= `client_eof`。`tests/recent_test.rs` の (a) と同じ手順)
    let mut tunnel = open_tunnel(proxy_port, echo_port);
    tunnel.write_all(b"hello").unwrap();
    let mut buf = [0u8; 5];
    tunnel.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello");
    tunnel.shutdown(std::net::Shutdown::Write).unwrap();
    let mut rest = Vec::new();
    let _ = tunnel.read_to_end(&mut rest);
    drop(tunnel);
    wait_until(
        || metrics.active_connections.load(Ordering::Relaxed) == 0,
        "the tunnel to close",
    );

    // 窓が閉じるのは 5 秒の境目 (`/history` の標本と同じ切り方) なので最大 5 秒待つ
    wait_until(
        || metrics.history.closed.counts().0 >= 1,
        "the 5s window to close",
    );
    let (fine, _, total) = metrics.history.closed.counts();
    assert!(fine >= 1 && total >= 1, "{} 窓 / {} 本", fine, total);

    let json = endpoint_json(proxy_port, "/history?res=5");
    // 既存の形はそのまま (標本の後ろに別の配列)
    assert!(
        json.starts_with("{\"schema\":1,\"interval_secs\":5,\"keys\":[\"t\","),
        "{}",
        &json[..80]
    );
    let samples_at = json.find("\"samples\":").expect("標本がある");
    let closed_at = json.find("\"closed\":").expect("分布がある");
    assert!(samples_at < closed_at, "分布は標本の後ろ");
    let closed = &json[closed_at..];
    assert!(closed.contains("\"interval_secs\":5"), "{}", closed);
    assert!(
        closed.contains("\"reasons\":[\"client_eof\",\"server_eof\",\"idle_timeout\",\"keepalive_timeout\",\"evicted\",\"limit\",\"shutdown\",\"error\"]"),
        "{}",
        closed
    );
    assert!(
        closed.contains("\"life_bounds_secs\":[1,2,5,10,15,"),
        "{}",
        closed
    );
    assert!(closed.contains("\"byte_bounds\":[1024,"), "{}", closed);
    assert!(
        closed.contains("\"keys\":[\"t\",\"closed\",\"reasons\",\"life\","),
        "{}",
        closed
    );
    // 理由の 1 列目 (`client_eof`) が 1 以上、寿命の 1 段目 (1 秒以内) も 1 以上
    assert!(
        closed.contains("[1,0,0,0,0,0,0,0]"),
        "client_eof が 1 件無い: {}",
        closed
    );
    assert!(closed.contains("\"recorded\":1"), "{}", closed);
    // 1 分の窓も同じ形、1 時間は残していない
    let minute = endpoint_json(proxy_port, "/history?res=60");
    assert!(
        minute.contains("\"closed\":{\"interval_secs\":60,"),
        "{}",
        minute
    );
    let hour = endpoint_json(proxy_port, "/history?res=3600");
    // **末尾では見ない**: T14.10 の `canary` と T14.12 の `kernel` がこのうしろに
    // 別の配列として付く (どれも既存の `keys` / `samples` は 1 つも変えていない)
    assert!(hour.contains(",\"closed\":null"), "{}", hour);
}

/// `/snapshot` の `parts` に `bursts` が並ぶこと (T14.4 の 1 要求で全部取る口)。
#[test]
fn test_integration_snapshot_includes_the_bursts() {
    let (proxy_port, _metrics) = burst_proxy(8, 50);
    let json = endpoint_json(proxy_port, "/snapshot");
    assert!(json.contains("\"bursts\""), "{}", &json[..400]);
    let parts = &json[json.find("\"parts\":").unwrap()..];
    assert!(
        parts.starts_with("\"parts\":[") && parts[..200].contains("\"bursts\""),
        "{}",
        &parts[..200]
    );
    assert!(json.contains("\"bursts\":{\"bursts\":["), "{}", json);
}

/// `--lite` は個票を 1 つも記録しないので写真も撮らない。
#[test]
fn test_integration_lite_takes_no_photos() {
    let mut cfg = park_config();
    cfg.lite = true;
    cfg.max_conns = 4;
    cfg.burst_percent = 50;
    cfg.refresh_burst_at();
    assert_eq!(cfg.burst_at, usize::MAX, "--lite では閾そのものを置かない");
    let proxy_port = start_test_proxy(cfg);
    let json = endpoint_json(proxy_port, "/bursts");
    assert!(json.contains("\"bursts\":[]"), "{}", json);
    assert!(json.contains("\"lite\":true"), "{}", json);
}
