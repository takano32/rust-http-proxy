//! `.env` の再読込で `PROXY_MAX_THREADS` が変わることの結合テスト (T11.6)。
//!
//! 2 本立てにしてある。1 本目は**実バイナリを起こして `.env` を書き換える**もので、
//! 「ファイル → inotify → `reload::Live` → `serve` → `Workers`」の配線を丸ごと見る。
//! 2 本目は上限が上がったときに**待たせていた接続が動き出す**ことを見る (値だけでなく
//! 効き目が変わることの確認)。

mod common;

use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::{
    ProxyProcess, one_keepalive_request, proxy_config, scratch_dir, start_counting_origin,
    start_test_proxy_with_live_config, status_json, wait_until,
};

/// テスト用の `.env` を書く (変えるのは `PROXY_MAX_THREADS` だけ)。
fn write_env(dir: &std::path::Path, max_threads: usize) {
    common::write_env(
        dir,
        &format!("PROXY_PROFILE=lite\nPROXY_MAX_THREADS={}\n", max_threads),
    );
}

/// `.env` を書き換えると `PROXY_MAX_THREADS` が変わること (T11.6)。
///
/// `HOME` を一時ディレクトリにして実バイナリを起こし、`/status` の `max_threads` を見る。
/// 上限は `Workers` が持っている値そのものなので、これが変われば当たっている。
#[test]
fn test_integration_max_threads_changes_when_the_env_file_is_rewritten() {
    let dir = scratch_dir("rhp-t116");
    write_env(&dir, 5);

    // 起動ログは標準出力へ出る (読み続けないとパイプが詰まるので、`ProxyProcess` が専用の
    // スレッドで全部引き取る)。"rust-http-proxy 0.18.0+144b992 listening on 127.0.0.1:PORT
    // (log level: info)" から待ち受けポートを取る。版もこの行に出る (T12.6)
    let child = ProxyProcess::start(&dir);
    let (port, banner) = (child.port, child.banner.clone());
    assert!(
        banner.contains(&format!(
            "rust-http-proxy {} listening on",
            rust_http_proxy::VERSION
        )),
        "起動ログに版が出ていない: {}",
        banner
    );

    let status = status_json(port);
    assert!(
        status.contains("\"max_threads\":5"),
        "起動時は .env の値: {}",
        status
    );

    // 書き換える (エディタと同じく上書き。inotify の IN_CLOSE_WRITE で気づく)
    write_env(&dir, 9);
    // `/status` を引くこと自体が「接続を 1 本受ける」ことなので、そこで上限が当たる
    wait_until(
        || status_json(port).contains("\"max_threads\":9"),
        "PROXY_MAX_THREADS が 9 になる",
    );
    let status = status_json(port);
    assert!(
        status.contains("PROXY_MAX_THREADS"),
        "再読込で当てたキーとして出る: {}",
        status
    );
    assert!(
        status.contains("\"max_conns\":"),
        "他の上限はそのまま: {}",
        status
    );

    drop(child);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 上限を上げると、待ち行列で待っていた接続がその場で動き出すこと (T11.6)。
///
/// 上限 1 で 1 本目がスレッドを握ったまま (keep-alive) にし、2 本目を待たせる。
/// 設定を差し替えてから 3 本目を受けさせると、`serve` が新しい上限を当てて待ち行列が片づく。
#[test]
fn test_integration_raising_max_threads_starts_the_waiting_connections() {
    let counter = Arc::new(AtomicUsize::new(0));
    let (origin_port, _origin) = start_counting_origin(Arc::clone(&counter), "");
    let host = format!("127.0.0.1:{}", origin_port);

    let mut cfg = proxy_config();
    // 預けられると席が空いてしまうので、接続がスレッドを握ったままになる設定にする
    cfg.park_idle = false;
    cfg.keepalive = Duration::from_secs(30);
    cfg.max_threads = 1;
    let (proxy_port, live) = start_test_proxy_with_live_config(cfg.clone());

    // 1 本目が唯一のスレッドを握る (応答後も keep-alive で握り続ける)
    let mut first = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
    first
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let (head, _) = one_keepalive_request(&mut first, &host, "/first");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);

    // 2 本目は上限に当たって待ち行列で待つ (捨てられはしない)
    let (tx, rx) = mpsc::channel();
    let waiting_host = host.clone();
    thread::spawn(move || {
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", proxy_port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let (head, _) = one_keepalive_request(&mut s, &waiting_host, "/waiting");
        let _ = tx.send(head);
        // 応答を受け取ったあとも接続は開けておく (相手が読むまで閉じない)
        thread::sleep(Duration::from_secs(1));
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "上限 1 なので 2 本目は待たされる"
    );

    // `.env` の再読込に相当する差し替え
    let mut raised = cfg;
    raised.max_threads = 4;
    *live.write().unwrap() = Arc::new(raised);

    // 次に受けた接続で新しい上限が当たり、待たせていた仕事が動き出す
    let status = status_json(proxy_port);
    assert!(
        status.contains("\"max_threads\":4"),
        "受けた接続で当たる: {}",
        status
    );
    let head = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("上限を上げたら待っていた接続が動き出す");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{}", head);
}
