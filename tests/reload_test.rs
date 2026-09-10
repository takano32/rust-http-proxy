//! `.env` の再読込で `PROXY_MAX_THREADS` が変わることの結合テスト (T11.6)。
//!
//! 2 本立てにしてある。1 本目は**実バイナリを起こして `.env` を書き換える**もので、
//! 「ファイル → inotify → `reload::Live` → `serve` → `Workers`」の配線を丸ごと見る。
//! 2 本目は上限が上がったときに**待たせていた接続が動き出す**ことを見る (値だけでなく
//! 効き目が変わることの確認)。

mod common;

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::{
    one_keepalive_request, proxy_config, start_counting_origin, start_test_proxy_with_live_config,
    status_json, wait_until,
};

/// 落ちても子プロセスを残さない番人。
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// テスト用の `.env` を書く (変えるのは `PROXY_MAX_THREADS` だけ)。
fn write_env(dir: &std::path::Path, max_threads: usize) {
    std::fs::write(
        dir.join(".env"),
        format!(
            "SERVER_PORT=0\n\
             PROXY_BIND=127.0.0.1\n\
             PROXY_PROFILE=lite\n\
             PROXY_LOG_LEVEL=info\n\
             PROXY_MAX_THREADS={}\n",
            max_threads
        ),
    )
    .unwrap();
}

/// `.env` を書き換えると `PROXY_MAX_THREADS` が変わること (T11.6)。
///
/// `HOME` を一時ディレクトリにして実バイナリを起こし、`/status` の `max_threads` を見る。
/// 上限は `Workers` が持っている値そのものなので、これが変われば当たっている。
#[test]
fn test_integration_max_threads_changes_when_the_env_file_is_rewritten() {
    let dir = std::env::temp_dir().join(format!("rhp-t116-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_env(&dir, 5);

    let mut child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
            .env("HOME", &dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("could not start the proxy binary"),
    );
    // 起動ログは標準出力へ出る。読み続けないとパイプが詰まるので、専用スレッドで全部引き取る
    let stdout = child.0.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    // "rust-http-proxy 0.1.0+144b992 listening on 127.0.0.1:PORT (log level: info)" から
    // 待ち受けポートを取る。版もこの行に出る (T12.6)
    let mut port = None;
    let mut banner = String::new();
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(20)) {
        if let Some(rest) = line.split_once("listening on ") {
            port = rest
                .1
                .split_whitespace()
                .next()
                .and_then(|a| a.rsplit(':').next())
                .and_then(|p| p.parse::<u16>().ok());
            banner = line;
            break;
        }
    }
    let port = port.expect("the proxy did not log its listening port");
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
