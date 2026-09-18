//! 重い口の同時実行を 1 本にする (T14.51) の結合テスト。
//!
//! 認証なしの公開ポートでは `/snapshot` (17 部・最大 4 MiB を組む。T14.4) を誰でも
//! 好きなだけ叩けるので、**同時に組むのは 1 本**にして 2 本目からは `503` +
//! `Retry-After: 1` で断る。**認証ではない**ので、順に引けば全部 200 で取れる。
//!
//! 旗は**プロセスに 1 つ**なので、この束の中のテストは互いに邪魔をしないよう
//! [`HEAVY`] で 1 本ずつ回す (テストは既定で並列に走る)。
//!
//! 計測 (`mx` / ベンチ) は要らない: 足したのは内部エンドポイントの経路だけで、
//! プロキシとしての転送 (forward / CONNECT) には 1 命令も入っていない。

mod common;
use common::*;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

/// 旗 (`HEAVY_BUSY`) はプロセスに 1 つなので、このファイルのテストは 1 本ずつ。
static HEAVY: Mutex<()> = Mutex::new(());

/// 自分宛ての 1 要求を投げて応答全文を返す (状態行と `Retry-After` を見るので本文だけでは足りない)。
fn get(port: u16, path: &str) -> String {
    raw_get(
        port,
        &format!(
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            path, port
        ),
    )
}

/// 先に `n` 本繋いでおき、**同じ時刻を狙って**一斉に `path` を送り、応答を全部読む。
///
/// 「同時」を作るのにこのテストがしていること 2 つ:
/// (1) **繋ぐのは race の外**。accept と worker への受け渡しを先に済ませておくと、
///     worker はどれも読み待ちで止まっているので、届いた瞬間に組み始める。
/// (2) **関門のあと `at` まで spin する**。関門だけだと futex で起こされる順に数十 us
///     ばらけ、その間に 1 本目が組み終わってしまう (実測で 4 本中 2 本が 200 になった)。
fn race(port: u16, path: &str, n: usize) -> Vec<String> {
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        path, port
    );
    let gate = Arc::new(Barrier::new(n));
    // 繋いでスレッドを起こすのに要る時間より十分あとの 1 点を狙う
    let at = Instant::now() + Duration::from_millis(100);
    let mut threads = Vec::with_capacity(n);
    for _ in 0..n {
        let mut sock = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let gate = Arc::clone(&gate);
        let req = req.clone();
        threads.push(std::thread::spawn(move || {
            gate.wait();
            while Instant::now() < at {
                std::hint::spin_loop();
            }
            sock.write_all(req.as_bytes()).unwrap();
            let mut out = String::new();
            let _ = sock.read_to_string(&mut out);
            out
        }));
    }
    threads.into_iter().map(|t| t.join().unwrap()).collect()
}

fn status_code(resp: &str) -> u16 {
    resp.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("状態行が読めない: {}", resp))
}

/// 断った応答の作法 (`503` + `Retry-After: 1` + `{"schema":1,"error":"busy"}`)。
fn assert_busy(resp: &str) {
    assert!(
        resp.starts_with("HTTP/1.1 503 Service Unavailable"),
        "{}",
        resp
    );
    assert!(resp.contains("\r\nRetry-After: 1\r\n"), "{}", resp);
    // エラーの応答にも形の版が付く (T14.49)
    assert!(
        resp.contains("{\"schema\":1,\"error\":\"busy\"}"),
        "{}",
        resp
    );
}

/// 受け入れ基準の本体: `/snapshot` を**同時に 4 本**引くと 1 本だけ 200 で残りは 503、
/// 順に引けば全部 200、`/status` の `heavy_rejected` が 3。
///
/// 「同時」は timing なので、**同時にならなかった回は数え直す**: 1 本目が組み終わって
/// から 2 本目が届けば、旗が正しくても 2 本とも 200 になる (spin で狙って 20 回に 1 回)。
/// 旗が壊れているのと取り違えないよう、**数え直す回も**「200 + 503 = 4 本」と
/// 「503 の本数 = `heavy_rejected`」は必ず見る。
#[test]
fn test_integration_only_one_snapshot_is_built_at_a_time() {
    let _one_at_a_time = HEAVY.lock().unwrap_or_else(|e| e.into_inner());

    let mut port = 0;
    let mut resps = Vec::new();
    for round in 1..=20 {
        // `heavy_rejected` を「3」で見たいので、回ごとに数えていないプロキシから始める
        port = start_test_proxy(proxy_config());
        assert_eq!(
            status_number(&endpoint_json(port, "/status"), "heavy_rejected"),
            0,
            "まだ誰も断っていない"
        );
        resps = race(port, "/snapshot", 4);
        let ok = resps.iter().filter(|r| status_code(r) == 200).count();
        let busy = resps.iter().filter(|r| status_code(r) == 503).count();
        let codes = || resps.iter().map(|r| status_code(r)).collect::<Vec<_>>();
        assert_eq!(ok + busy, 4, "200 か 503 のどちらか: {:?}", codes());
        // 断った数は `/status` の末尾に出る (組んだ 1 本は数えない)
        assert_eq!(
            status_number(&endpoint_json(port, "/status"), "heavy_rejected"),
            busy as u64,
            "断った本数がそのまま `heavy_rejected`: {:?}",
            codes()
        );
        if (ok, busy) == (1, 3) {
            break;
        }
        assert!(
            round < 20,
            "20 回とも 4 本が同時にならなかった (最後の回: {:?})",
            codes()
        );
    }

    for r in resps.iter().filter(|r| status_code(r) == 503) {
        assert_busy(r);
    }
    // 200 の方は本当に `/snapshot` (断った応答と取り違えていない)
    let built = resps.iter().find(|r| status_code(r) == 200).unwrap();
    assert!(
        built.contains("\"parts\":["),
        "{}",
        &built[..200.min(built.len())]
    );

    // **順に引けば全部 200** (認証ではないので、待てば誰でも取れる)
    for i in 0..4 {
        let resp = get(port, "/snapshot");
        assert_eq!(status_code(&resp), 200, "{} 本目: {}", i + 1, resp);
    }
    assert_eq!(
        status_number(&endpoint_json(port, "/status"), "heavy_rejected"),
        3,
        "順に引いたぶんは 1 本も断っていない"
    );
}

/// 軽い口 (`/status`) は同時 4 本とも 200 で、`heavy_rejected` も動かない。
#[test]
fn test_integration_the_light_endpoints_are_not_limited() {
    let _one_at_a_time = HEAVY.lock().unwrap_or_else(|e| e.into_inner());
    let port = start_test_proxy(proxy_config());

    for resp in race(port, "/status", 4) {
        assert_eq!(status_code(&resp), 200, "{}", resp);
        assert!(!resp.contains("Retry-After"), "{}", resp);
    }
    // `/healthz` `/metrics` も同時に叩かれる口 (監視が 5 秒ごとに来る)
    for resp in race(port, "/healthz", 4) {
        assert_eq!(status_code(&resp), 200, "{}", resp);
    }
    for resp in race(port, "/metrics", 4) {
        assert_eq!(status_code(&resp), 200, "{}", resp);
    }
    assert_eq!(
        status_number(&endpoint_json(port, "/status"), "heavy_rejected"),
        0,
        "軽い口は旗に触らない"
    );
}

/// 重い口の一覧 (`/snapshot` `/profile` `/explain` と、**大きく引いたときだけ**の
/// `/hosts` `/recent` `/history`) が旗を取り合うこと。
///
/// 旗はテストの側から握れる (同じプロセスなので `HEAVY_BUSY` は 1 つ) ので、
/// **timing に頼らずに**「握られている間はどれが断られるか」を 1 本ずつ確かめられる。
#[test]
fn test_integration_which_endpoints_are_heavy() {
    let _one_at_a_time = HEAVY.lock().unwrap_or_else(|e| e.into_inner());
    let port = start_test_proxy(proxy_config());

    let heavy = [
        "/snapshot",
        "/profile",
        "/explain?host=heavy.example.com",
        "/hosts?limit=1000",
        "/recent?n=2000",
        "/history?res=5&n=4320",
    ];
    let light = [
        "/status",
        "/healthz",
        "/metrics",
        "/",
        "/hosts",
        "/hosts?limit=200",
        "/recent",
        "/recent?n=500",
        "/history",
        "/history?res=5&n=720",
        // 標本を組まない要約は軽い (T15.0 (11))
        "/profile?summary=1",
        "/hosts/series?top=16",
        // 読み手の表 (T14.53)。最大 256 行なので軽い
        "/readers",
        "/connections",
        "/errors",
        "/dns",
        "/config",
    ];

    // 誰も握っていなければ、重い口も 200 (断るのは 2 本目から)
    for path in heavy {
        assert_eq!(status_code(&get(port, path)), 200, "{}", path);
    }

    {
        // 1 本目のふりをして旗を握る
        let _built_by_someone_else =
            rust_http_proxy::endpoints::begin_heavy().expect("旗は空いているはず");
        assert!(
            rust_http_proxy::endpoints::begin_heavy().is_none(),
            "2 本目は取れない"
        );
        for path in heavy {
            let resp = get(port, path);
            assert_eq!(status_code(&resp), 503, "{} は重い口: {}", path, resp);
            assert_busy(&resp);
        }
        for path in light {
            let resp = get(port, path);
            assert_eq!(status_code(&resp), 200, "{} は軽い口: {}", path, resp);
        }
    }

    // 番人が落ちたら旗は戻る
    assert!(
        rust_http_proxy::endpoints::begin_heavy().is_some(),
        "番人が落ちれば次が取れる"
    );
    for path in heavy {
        assert_eq!(status_code(&get(port, path)), 200, "{}", path);
    }
    assert_eq!(
        status_number(&endpoint_json(port, "/status"), "heavy_rejected"),
        heavy.len() as u64,
        "断ったのは重い口のぶんだけ"
    );
}

/// `/profile?summary=1` は**重い口が 1 本走っている最中でも 200** (T15.0 (11))。
///
/// `/profile` は標本を 256 KiB ぶん組むので無条件に重い口だが、`?summary=1` は
/// 3 段に畳んだ数字だけで標本を 1 本も組まない。雪像を取っている最中でも
/// 「この機械がいま 1 要求に何 us 使っているか」だけは読めるようにしておくための口。
#[test]
fn test_integration_profile_summary_is_not_a_heavy_endpoint() {
    let _one_at_a_time = HEAVY.lock().unwrap_or_else(|e| e.into_inner());
    let port = start_test_proxy(proxy_config());

    {
        // 1 本目のふりをして旗を握る (`/snapshot` を組んでいる最中と同じ状態)
        let _built_by_someone_else =
            rust_http_proxy::endpoints::begin_heavy().expect("旗は空いているはず");
        // 標本を返す方は今までどおり断る
        assert_eq!(status_code(&get(port, "/profile")), 503, "標本つきは重い口");
        assert_eq!(status_code(&get(port, "/profile?res=60")), 503);
        // 要約は通る (`summary=0` は「立てていない」= 今までどおり重い)
        for path in ["/profile?summary=1", "/profile?res=60&summary=1"] {
            let resp = get(port, path);
            assert_eq!(status_code(&resp), 200, "{} -> {}", path, resp);
            assert!(resp.contains("\"summary\":true"), "{} -> {}", path, resp);
            assert!(!resp.contains("\"samples\":["), "{} -> {}", path, resp);
        }
        assert_eq!(status_code(&get(port, "/profile?summary=0")), 503);
    }

    // 断ったのは標本つきの 3 本だけ (要約は旗に触っていない)
    assert_eq!(
        status_number(&endpoint_json(port, "/status"), "heavy_rejected"),
        3,
        "要約が旗を取っている"
    );
}
