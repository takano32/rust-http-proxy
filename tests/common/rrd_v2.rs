//! 版 2 (`SHPRRD02`) の状態ファイルを組む fixture (T14.14)。
//!
//! 初めは `tests/persist_test.rs` にあった (T34.122 で移した)。そこの
//! `test_integration_a_version_two_state_file_is_converted_to_version_three` だけが使うので、
//! `tests/common/mod.rs` には入れず、`#[path]` で取り込む (ほかの試験バイナリは組まない)。

/// 版 2 (`SHPRRD02`) の割り付け。**このテストが版 2 の形を持っている**ので、
/// 本体が版 3 になったあとでも fixture を組める (本体側の版 2 の定数は変換専用)。
pub mod v2 {
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
pub struct Row {
    pub name: String,
    pub requests: u64,
    pub bytes: u64,
    pub ms_sum: u64,
    pub rtt_samples: u64,
    pub bytes_in: u64,
}

/// `scripts/testdata/rrd-v2.tsv` を読んだもの。
#[derive(Default)]
pub struct Fixture {
    pub hosts: Vec<Row>,
    pub clients: Vec<Row>,
    pub fill_hosts: usize,
    pub fill_clients: usize,
    pub fill_requests: u64,
    /// `(本数, 最初の時刻)` を解像度の順 (5 / 60 / 3600) に
    pub samples: [(usize, u64); 3],
    /// `(宛先, block, 期限, 作った時刻)`
    pub overrides: Vec<(String, u64, u64, u64)>,
}

impl Fixture {
    pub fn load() -> Fixture {
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

    pub fn host_rows(&self) -> usize {
        self.hosts.len() + self.fill_hosts
    }

    pub fn client_rows(&self) -> usize {
        self.clients.len() + self.fill_clients
    }

    /// ホスト別の `requests` の合計 (**変換の前後でここが一致すること**が受け入れ基準)。
    pub fn host_requests(&self) -> u64 {
        self.hosts.iter().map(|r| r.requests).sum::<u64>()
            + self.fill_hosts as u64 * self.fill_requests
    }

    pub fn fill_row(&self, kind: &str, i: usize) -> Row {
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
pub fn stats_payload(r: &Row) -> Vec<u8> {
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
pub fn write_version_two_rrd(path: &std::path::Path, fx: &Fixture) {
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
            // (`evicted_idle` は T14.2 で 63 項目目に足したもの。T15.0 で今の
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
