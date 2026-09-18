//! 状態ファイル (`.rust-http-proxy.rrd`) の版上げの結合テスト (T12.4 (1)、T14.14)。
//!
//! 実バイナリを起こして 3 つを見る:
//!
//! - **版 1 のファイルが置いてあっても落ちない** (読み捨てて作り直す。T12.4)
//! - **版 2 のファイルは捨てずに版 3 へ詰め直す** (T14.14。統計が 1 件も消えない)
//! - 大きさが新しい固定値 (8 MiB) になり、書込エラーが出ない

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::{endpoint_json, status_json};

/// `"key":` に続く数を取る (テスト用の雑な取り出し。キーは前後の `"` を含めて渡す)。
fn status_number(json: &str, key: &str) -> u64 {
    let pat = format!("{}:", key);
    let at = json
        .find(&pat)
        .unwrap_or_else(|| panic!("no {} in {}", key, json))
        + pat.len();
    json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("{} is not a number", key))
}

/// 版 3 の固定の大きさ (`proxy_rrd::rrd::FILE_SIZE`。T14.14 で 4 → 8 MiB)。
const FILE_SIZE: u64 = 8 * 1024 * 1024;

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `.env` を置いてプロキシを起こし、`(子, 待ち受けポート, 最後の `state file ...` の行)`。
fn start_proxy(dir: &std::path::Path) -> (KillOnDrop, u16, String) {
    // 既定プロファイル (lite だと統計を永続化しない)。ログは info で起動行を読む
    std::fs::write(
        dir.join(".env"),
        "SERVER_PORT=0\nPROXY_BIND=127.0.0.1\nPROXY_LOG_LEVEL=info\nPROXY_CACHE_RESERVE=off\n",
    )
    .unwrap();
    let mut child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_rust-http-proxy"))
            .env("HOME", dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("could not start the proxy binary"),
    );
    let stdout = child.0.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut port = None;
    let mut state_line = String::new();
    while let Ok(line) = rx.recv_timeout(Duration::from_secs(30)) {
        if line.contains("state file ") {
            state_line = line.clone();
        }
        if let Some(rest) = line.split_once("listening on ") {
            port = rest
                .1
                .split_whitespace()
                .next()
                .and_then(|a| a.rsplit(':').next())
                .and_then(|p| p.parse::<u16>().ok());
            break;
        }
    }
    (
        child,
        port.expect("the proxy did not log its listening port"),
        state_line,
    )
}

/// 版 1 のファイルを模して置く (識別子 `SHPRRD01` + 約 1 MiB の中身)。
fn write_version_one_rrd(path: &std::path::Path) {
    let mut buf = vec![0u8; 1_064_960];
    buf[..8].copy_from_slice(b"SHPRRD01");
    // 中身は「前の版で書かれたレコード」のつもりの適当なバイト列 (CRC は合わない)
    for (i, b) in buf[4096..].iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    std::fs::write(path, &buf).unwrap();
}

#[test]
fn test_integration_an_old_state_file_is_replaced_by_the_new_fixed_size_one() {
    let dir = std::env::temp_dir().join(format!("rhp-t124-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rrd = dir.join(".rust-http-proxy.rrd");
    write_version_one_rrd(&rrd);
    assert_eq!(std::fs::metadata(&rrd).unwrap().len(), 1_064_960);

    let (child, port, state_line) = start_proxy(&dir);
    assert!(
        state_line.contains("created"),
        "版 1 のファイルは読み捨てて作り直すはず: {:?}",
        state_line
    );

    let status = status_json(port);
    // 作り直しは「変換」ではない (新規起動の `converted_from` は null。T14.14)
    assert!(
        status.contains("\"version\":3,\"converted_from\":null"),
        "新規起動の版と変換元: {}",
        status
    );
    assert!(
        status.contains(&format!("\"bytes\":{}", FILE_SIZE)),
        "state_file.bytes が新しい固定の大きさでない: {}",
        status
    );
    assert!(
        status.contains("\"write_errors\":0"),
        "書込エラーが出ている: {}",
        status
    );
    assert_eq!(
        std::fs::metadata(&rrd).unwrap().len(),
        FILE_SIZE,
        "ファイルの実サイズも固定"
    );

    // ---- `/status` の窓とプロセスの数え物 (T12.4 (4)) ----
    assert!(
        status.contains("\"since_start_secs\":") && status.contains("\"restored_since\":"),
        "窓の目印が無い: {}",
        status
    );
    // まだ 1 件も要求を通していないので、通算の始まりは 0 (= `hosts[]` が空)
    assert!(status.contains("\"restored_since\":0"), "{}", status);
    let pid = child.0.id();
    let fds = status_number(&status, "\"fds\"");
    let max_fds = status_number(&status, "\"max_fds\"");
    let threads = status_number(&status, "\"threads\"");
    let ls = std::process::Command::new("ls")
        .arg(format!("/proc/{}/fd", pid))
        .output()
        .expect("ls");
    let counted = String::from_utf8_lossy(&ls.stdout).lines().count() as u64;
    assert!(
        fds.abs_diff(counted) <= 2,
        "/status の fds {} と ls /proc/{}/fd {} が ±2 で一致しない",
        fds,
        pid,
        counted
    );
    assert!(max_fds >= fds, "max_fds {} < fds {}", max_fds, fds);
    assert!(threads >= 2, "スレッド数 {}", threads);
    assert!(
        status.len() <= 64 * 1024,
        "/status が {} B (64 KiB 超)",
        status.len()
    );

    drop(child);
    let _ = std::fs::remove_dir_all(&dir);
}

/// T14.5 より前の形 (`HostStats` が 49 項目) で書かれた行を置いて起動しても落ちず、
/// あとから足した 4 欄 (RTT と再送) が 0 で読み戻ること。
///
/// 欄は**末尾に足す**決まりなので、短いレコードは読み捨てられずにそのまま復元される
/// (`Dec` は足りなければ 0 を返す)。版 2 から詰め直した行も同じ形になる (T14.14)。
#[test]
fn test_integration_a_pre_rtt_state_file_restores_with_zero_rtt() {
    use rust_http_proxy::rrd::{Enc, Rrd};

    let dir = std::env::temp_dir().join(format!("rhp-t145-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".rust-http-proxy.rrd");

    // いまの版のファイルを作り、**T14.5 より前の形** (名前 128 B + 49 項目) で 1 行ずつ書く
    {
        let (rrd, opened) = Rrd::open(&path).unwrap();
        assert!(opened.created);
        for (region, name, requests) in [
            (rrd.layout.hosts, "connect://mtalk.google.com:5228", 920u64),
            (rrd.layout.clients, "198.51.100.7", 463),
        ] {
            let mut e = Enc::new();
            e.str(name, 128)
                .u64(1_700_000_000) // last_seen
                .u64(requests)
                .u64(0) // hits
                .u64(0) // misses
                .u64(requests) // bypass
                .u64(0) // errors
                .u64(0) // blocked
                .u64(1024) // bytes
                .u64(requests) // timed
                .u64(requests * 30) // duration_ms_sum
                .u64(31); // duration_ms_max
            for _ in 0..25 {
                e.u64(0); // buckets (24 段 + 上限なし)
            }
            e.u64(0).u64(0).u64(requests * 30).u64(requests).u64(0); // dns/connect/族
            for _ in 0..8 {
                e.u64(0); // errors_by_cause
            }
            assert_eq!(e.0.len(), 128 + 49 * 8, "T14.5 より前の 1 行は 520 B");
            rrd.write(region, 0, &e.0).unwrap();
        }
    }

    let (child, port, state_line) = start_proxy(&dir);
    assert!(
        !state_line.contains("created"),
        "いま書いた版 3 のファイルは作り直さないはず: {:?}",
        state_line
    );
    assert!(
        state_line.contains("1 hosts, 1 clients restored"),
        "古いファイルの行が読み戻っていない: {:?}",
        state_line
    );

    let status = status_json(port);
    assert!(
        status.contains("\"host\":\"connect://mtalk.google.com:5228\""),
        "{}",
        status
    );
    assert!(status.contains("\"requests\":920"), "{}", status);
    assert!(status.contains("\"client\":\"198.51.100.7\""), "{}", status);
    // 末尾に足した 4 欄は 0 で読み戻る = RTT は「標本なし」
    assert_eq!(
        status.matches("\"rtt_ms\":null,\"retrans\":0").count(),
        2,
        "古い行の RTT が null で出ていない: {}",
        status
    );
    assert!(status.contains("\"write_errors\":0"), "{}", status);

    drop(child);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- T14.14: 版 2 のファイルを置いて起動し、版 3 へ詰め直させる ----

/// 版 2 (`SHPRRD02`) の割り付け。**このテストが版 2 の形を持っている**ので、
/// 本体が版 3 になったあとでも fixture を組める (本体側の版 2 の定数は変換専用)。
mod v2 {
    pub const MAGIC: &[u8; 8] = b"SHPRRD02";
    pub const FILE_SIZE: usize = 4 * 1024 * 1024;
    const HEADER: usize = 4096;
    const SAMPLE_RECORD: usize = 512;
    /// 版 2 の標本の固定の欄の数 (62 項目 × 8 B = 496 B。残りが CRC までの余白)。
    /// `evicted_idle` は T14.2 で 63 項目目に足したので、版 2 には入っていない。
    pub const SAMPLE_ITEMS: usize = 62;
    const STATS_RECORD: usize = 576;
    const OVERRIDE_RECORD: usize = 160;
    /// `(レコード長, 本数)` を割り付けの順に。
    const REGIONS: [(usize, usize); 6] = [
        (SAMPLE_RECORD, 720),   // 5 秒 × 1 時間
        (SAMPLE_RECORD, 1440),  // 1 分 × 1 日
        (SAMPLE_RECORD, 720),   // 1 時間 × 30 日
        (STATS_RECORD, 1000),   // ホスト別
        (STATS_RECORD, 1000),   // 接続元別
        (OVERRIDE_RECORD, 256), // ブロックリストの上書き
    ];
    pub const HOSTS: usize = 3;
    pub const CLIENTS: usize = 4;
    pub const OVERRIDES: usize = 5;

    /// 領域 `region` の `idx` 番目に置く (残りはゼロ、末尾 4 B が CRC-32)。
    pub fn put(buf: &mut [u8], region: usize, idx: usize, payload: &[u8]) {
        let mut off = HEADER;
        for (rec, count) in REGIONS.iter().take(region) {
            off += rec * count;
        }
        let (rec, count) = REGIONS[region];
        assert!(idx < count, "領域 {} に {} 番目は無い", region, idx);
        let payload_size = rec - 4;
        assert!(
            payload.len() <= payload_size,
            "{} B は入らない",
            payload.len()
        );
        let at = off + idx * rec;
        buf[at..at + payload.len()].copy_from_slice(payload);
        let crc = rust_http_proxy::rrd::crc32(&buf[at..at + payload_size]);
        buf[at + payload_size..at + rec].copy_from_slice(&crc.to_le_bytes());
    }
}

/// fixture の 1 行 (ホスト別 / 接続元別)。
struct Row {
    name: String,
    requests: u64,
    bytes: u64,
    ms_sum: u64,
    rtt_samples: u64,
    bytes_in: u64,
}

/// `scripts/testdata/rrd-v2.tsv` を読んだもの。
#[derive(Default)]
struct Fixture {
    hosts: Vec<Row>,
    clients: Vec<Row>,
    fill_hosts: usize,
    fill_clients: usize,
    fill_requests: u64,
    /// `(本数, 最初の時刻)` を解像度の順 (5 / 60 / 3600) に
    samples: [(usize, u64); 3],
    /// `(宛先, block, 期限, 作った時刻)`
    overrides: Vec<(String, u64, u64, u64)>,
}

impl Fixture {
    fn load() -> Fixture {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/testdata/rrd-v2.tsv");
        let text = std::fs::read_to_string(&path).expect("fixture が読めない");
        let mut fx = Fixture::default();
        for line in text.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            let n = |i: usize| -> u64 { f[i].parse().expect(line) };
            match f[0] {
                "host" | "client" => {
                    let row = Row {
                        name: f[1].to_string(),
                        requests: n(2),
                        bytes: n(3),
                        ms_sum: n(4),
                        rtt_samples: n(5),
                        bytes_in: n(6),
                    };
                    if f[0] == "host" {
                        fx.hosts.push(row);
                    } else {
                        fx.clients.push(row);
                    }
                }
                "fill" => {
                    fx.fill_hosts = n(1) as usize;
                    fx.fill_clients = n(2) as usize;
                    fx.fill_requests = n(3);
                }
                "sample" => {
                    let i = match n(1) {
                        5 => 0,
                        60 => 1,
                        _ => 2,
                    };
                    fx.samples[i] = (n(2) as usize, n(3));
                }
                "override" => fx.overrides.push((f[1].to_string(), n(2), n(3), n(4))),
                other => panic!("知らない種類 {}", other),
            }
        }
        fx
    }

    fn host_rows(&self) -> usize {
        self.hosts.len() + self.fill_hosts
    }

    fn client_rows(&self) -> usize {
        self.clients.len() + self.fill_clients
    }

    /// ホスト別の `requests` の合計 (**変換の前後でここが一致すること**が受け入れ基準)。
    fn host_requests(&self) -> u64 {
        self.hosts.iter().map(|r| r.requests).sum::<u64>()
            + self.fill_hosts as u64 * self.fill_requests
    }

    fn fill_row(&self, kind: &str, i: usize) -> Row {
        Row {
            name: format!("connect://{}-{:03}.fixture.invalid:443", kind, i),
            requests: self.fill_requests,
            bytes: self.fill_requests * 4096,
            ms_sum: self.fill_requests * 9,
            rtt_samples: self.fill_requests,
            bytes_in: self.fill_requests * 512,
        }
    }
}

/// `HostStats::encode` と同じ並びで 1 行を組む (末尾の欄をどこまで書くかは行しだい)。
fn stats_payload(r: &Row) -> Vec<u8> {
    use rust_http_proxy::rrd::Enc;
    let mut e = Enc::new();
    e.str(&r.name, 128)
        .u64(1_700_000_000) // last_seen
        .u64(r.requests)
        .u64(0) // hits
        .u64(0) // misses
        .u64(r.requests) // bypass
        .u64(0) // errors
        .u64(0) // blocked
        .u64(r.bytes)
        .u64(r.requests) // timed
        .u64(r.ms_sum) // duration_ms_sum
        .u64(63); // duration_ms_max
    for _ in 0..25 {
        e.u64(0); // buckets (24 段 + 上限なし)
    }
    e.u64(0).u64(0).u64(r.ms_sum).u64(r.requests).u64(0); // dns / connect / 族
    for _ in 0..8 {
        e.u64(0); // errors_by_cause
    }
    if r.rtt_samples == 0 && r.bytes_in == 0 {
        assert_eq!(e.0.len(), 128 + 49 * 8, "T14.5 より前の形");
        return e.0;
    }
    // T14.5 の 4 欄 (RTT と再送)
    e.u64(r.rtt_samples * 9_000)
        .u64(8_000)
        .u64(r.rtt_samples)
        .u64(0);
    if r.bytes_in == 0 {
        assert_eq!(e.0.len(), 128 + 53 * 8, "T14.5 のあとの形");
        return e.0;
    }
    // T14.26 の 2 欄 (向き別のバイト) = 版 2 の最後の形
    e.u64(r.bytes_in).u64(r.bytes - r.bytes_in);
    assert_eq!(e.0.len(), 128 + 55 * 8, "T14.26 のあとの形");
    e.0
}

/// fixture から版 2 のファイルを 1 本書く。
fn write_version_two_rrd(path: &std::path::Path, fx: &Fixture) {
    use rust_http_proxy::history::Sample;
    use rust_http_proxy::rrd::Enc;

    let mut buf = vec![0u8; v2::FILE_SIZE];
    buf[..8].copy_from_slice(v2::MAGIC);
    for (region, rows, fill) in [
        (v2::HOSTS, &fx.hosts, fx.fill_hosts),
        (v2::CLIENTS, &fx.clients, fx.fill_clients),
    ] {
        for (i, r) in rows.iter().enumerate() {
            v2::put(&mut buf, region, i, &stats_payload(r));
        }
        let kind = if region == v2::HOSTS { "fill" } else { "peer" };
        for i in 0..fill {
            let r = fx.fill_row(kind, i);
            v2::put(&mut buf, region, rows.len() + i, &stats_payload(&r));
        }
    }
    for (res, (count, first)) in fx.samples.iter().enumerate() {
        let step = [5u64, 60, 3600][res];
        for i in 0..*count {
            let s = Sample {
                t: first + i as u64 * step,
                requests: 1000 + i as u64,
                bytes: 4096 * (i as u64 + 1),
                active: i % 7,
                ..Sample::default()
            };
            // 版 2 の 1 レコードは 512 B (payload 508 B) で、標本は **62 項目**だった
            // (`evicted_idle` は T14.2 で 63 項目目に足したもの。T15.0 (10) で今の
            // `encode` は 83 項目 = 664 B になったので、版 2 のぶんだけ切って書く)
            v2::put(&mut buf, res, i, &s.encode()[..v2::SAMPLE_ITEMS * 8]);
        }
    }
    for (i, (host, block, expires, created)) in fx.overrides.iter().enumerate() {
        let mut e = Enc::new();
        e.str(host, 128).u64(*block).u64(*expires).u64(*created);
        v2::put(&mut buf, v2::OVERRIDES, i, &e.0);
    }
    std::fs::write(path, &buf).unwrap();
}

/// `"key":<数>` を全部足す (キーは前後の `"` を含めて渡す)。
fn sum_numbers(json: &str, key: &str) -> u64 {
    let mut total = 0u64;
    let mut rest = json;
    while let Some(at) = rest.find(key) {
        rest = &rest[at + key.len()..];
        let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        total += n.parse::<u64>().unwrap_or(0);
    }
    total
}

/// `"samples":[[..],[..]]` の行数 (入れ子の配列があるので深さで数える)。
fn count_samples(json: &str) -> usize {
    let head = "\"samples\":[";
    let at = json.find(head).expect("samples が無い") + head.len();
    let (mut depth, mut rows) = (0usize, 0usize);
    for c in json[at..].chars() {
        match c {
            '[' => {
                if depth == 0 {
                    rows += 1;
                }
                depth += 1;
            }
            ']' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    rows
}

/// 版 2 の `.rrd` を置いて起動すると、**読み捨てずに版 3 へ詰め直す** (T14.14)。
///
/// 見るのは受け入れ基準のとおり: ホスト別の `requests` の合計と `/history?res=3600` の
/// 標本数が変換の前後で一致、`write_errors` が 0、`state_file.converted_from` が 2、
/// ファイルは 8,388,608 B 固定。2 回目の起動では変換しない (`converted_from` は null)。
#[test]
fn test_integration_a_version_two_state_file_is_converted_to_version_three() {
    let dir = std::env::temp_dir().join(format!("rhp-t1414-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".rust-http-proxy.rrd");
    let fx = Fixture::load();
    write_version_two_rrd(&path, &fx);
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        v2::FILE_SIZE as u64
    );

    let (child, port, state_line) = start_proxy(&dir);
    assert!(
        state_line.contains("converted from version 2"),
        "版 2 を詰め直していない: {:?}",
        state_line
    );
    assert!(
        state_line.contains(&format!(
            "{} hosts, {} clients restored",
            fx.host_rows(),
            fx.client_rows()
        )),
        "読み戻した行数が合わない: {:?}",
        state_line
    );
    // 変換の所要 (debug ビルド。報告に貼るので必ず出す)
    println!(
        "T14.14 変換: {} 行 + 標本 {} 本, {}",
        fx.host_rows() + fx.client_rows(),
        fx.samples.iter().map(|(n, _)| n).sum::<usize>(),
        state_line
    );

    let status = status_json(port);
    assert!(
        status.contains("\"version\":3,\"converted_from\":2"),
        "版と変換元: {}",
        status
    );
    assert!(
        status.contains(&format!("\"bytes\":{}", FILE_SIZE)),
        "state_file.bytes が 8 MiB でない: {}",
        status
    );
    assert!(status.contains("\"write_errors\":0"), "{}", status);
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        FILE_SIZE,
        "ファイルの実サイズも 8,388,608 B 固定"
    );
    // ブロックリストの上書き (最後の領域) も移っている
    assert!(status.contains("blocked.fixture.invalid"), "{}", status);

    // ---- ホスト別の通算: 件数も合計も 1 つも欠けていない ----
    let hosts = endpoint_json(port, "/hosts?limit=1000");
    assert!(
        hosts.contains(&format!("\"count\":{}", fx.host_rows()))
            && hosts.contains("\"truncated\":false"),
        "{}",
        &hosts[hosts.len().saturating_sub(300)..]
    );
    assert_eq!(
        sum_numbers(&hosts, "\"requests\":"),
        fx.host_requests(),
        "ホスト別の requests の合計が変換で変わった"
    );
    // 版 2 の 3 通りの行 (55 / 53 / 49 項目) がそれぞれ正しく読み戻る
    assert!(
        hosts.contains("\"host\":\"connect://news.fixture.invalid:443\",\"requests\":4210,"),
        "55 項目の行"
    );
    assert!(
        hosts.contains("\"bytes_in\":8123456,\"bytes_out\":804222222"),
        "向き別のバイト"
    );
    assert!(
        hosts.contains("\"host\":\"connect://cdn.fixture.invalid:443\",\"requests\":1204,"),
        "53 項目の行"
    );
    assert!(
        hosts.contains("\"host\":\"connect://ntp.fixture.invalid:123\",\"requests\":132,"),
        "49 項目の行"
    );
    // 末尾の欄が無い行は 0 で読み戻る (RTT は「標本なし」)
    assert!(hosts.contains("\"rtt_ms\":null"), "49 項目の行の RTT");

    // ---- 履歴: 標本の数が変換の前後で一致 ----
    for (res, secs) in [(0usize, 5u64), (1, 60), (2, 3600)] {
        let h = endpoint_json(port, &format!("/history?res={}", secs));
        assert_eq!(
            count_samples(&h),
            fx.samples[res].0,
            "res={} の標本数が変わった",
            secs
        );
    }

    // ---- 2 回目の起動: もう版 3 なので変換しない ----
    drop(child);
    let (child2, port2, line2) = start_proxy(&dir);
    assert!(
        !line2.contains("converted") && !line2.contains("created"),
        "2 回目は開くだけのはず: {:?}",
        line2
    );
    let status2 = status_json(port2);
    assert!(
        status2.contains("\"version\":3,\"converted_from\":null"),
        "2 回目の変換元は null: {}",
        status2
    );
    let hosts2 = endpoint_json(port2, "/hosts?limit=1000");
    assert_eq!(
        sum_numbers(&hosts2, "\"requests\":"),
        fx.host_requests(),
        "詰め直した版 3 のファイルからも同じ合計が読める"
    );
    drop(child2);
    let _ = std::fs::remove_dir_all(&dir);
}
