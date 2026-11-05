//! `/status` の組み立ての試験: 並べ替えの鍵・64 KiB の上限・軽い口 (`?hosts=0`)
//! (T.34.33 で `latency_tests.rs` から移した)。

use super::*;

/// `/status?sort=` が上位を切り出す鍵だけを変えること (T13.3)。
///
/// 鍵ごとに先頭が入れ替わり、**JSON の形は変わらない** (件数もキーもそのまま)。
#[test]
fn the_sort_key_only_changes_which_hosts_are_cut_out() {
    let m = Metrics::new();
    // 要求は多いが健全なホスト
    for _ in 0..10 {
        m.record_host_detail(
            "connect://busy:443",
            HostOutcome::Bypass,
            0,
            Some(Duration::from_millis(5)),
            Detail::default(),
        );
    }
    // 名前解決に時間を払っているホスト (要求は 2 件)
    for _ in 0..2 {
        m.record_host_detail(
            "connect://slow-dns:443",
            HostOutcome::Bypass,
            0,
            Some(Duration::from_millis(60)),
            Detail {
                dns_ms: 50,
                dns_misses: 1,
                connect_ms: 10,
                ..Detail::default()
            },
        );
    }
    // エラーだけのホスト (要求 1 件)
    m.record_host_detail(
        "connect://broken:443",
        HostOutcome::Error,
        0,
        None,
        Detail {
            cause: Some(ErrCause::Dns),
            dns_misses: 1,
            ..Detail::default()
        },
    );
    // 遠いホスト (1 件だけだが平均が飛び抜けて遅い)
    m.record_host_detail(
        "connect://far-away:443",
        HostOutcome::Bypass,
        0,
        Some(Duration::from_millis(900)),
        Detail {
            connect_ms: 900,
            ..Detail::default()
        },
    );
    let first = |sort| m.hosts_sorted_by(sort)[0].0.clone();
    assert_eq!(first(HostSort::Requests), "connect://busy:443");
    assert_eq!(first(HostSort::Errors), "connect://broken:443");
    assert_eq!(first(HostSort::Dns), "connect://slow-dns:443");
    assert_eq!(first(HostSort::Slow), "connect://far-away:443");
    // 知らない値と空は既定 (要求数順) に倒れる
    for v in ["", "requests", "REQUESTS", "errors?", "なにか"] {
        assert_eq!(
            m.hosts_sorted_by(HostSort::from_param(v))[0].0,
            "connect://busy:443",
            "{}",
            v
        );
    }
    // 同点は要求数 → 名前で崩すので、どの鍵でも順序は 1 つに決まる
    assert_eq!(
        m.hosts_sorted_by(HostSort::Errors),
        m.hosts_sorted_by(HostSort::Errors)
    );
    // JSON の形は鍵で変わらない (件数もキーも同じ。先頭のホストだけが違う)
    let of = |sort| {
        m.to_json_with_cache(
            None,
            StatusExtras {
                sort,
                ..StatusExtras::default()
            },
        )
    };
    let (a, b) = (of(HostSort::Requests), of(HostSort::Errors));
    // 数えるのは `hosts[]` の要素 (`{"host":` で始まる) だけ。`canary` にも
    // `host` の欄がある (T14.10) ので、鍵の名前だけで数えると 1 件多くなる
    assert_eq!(a.matches("{\"host\":").count(), 4);
    assert_eq!(
        a.matches("{\"host\":").count(),
        b.matches("{\"host\":").count()
    );
    assert_eq!(
        a.matches("\"errors_by_cause\":[").count(),
        b.matches("\"errors_by_cause\":[").count()
    );
    assert!(
        a.contains("\"hosts\":[{\"host\":\"connect://busy:443\""),
        "{}",
        a
    );
    assert!(
        b.contains("\"hosts\":[{\"host\":\"connect://broken:443\""),
        "{}",
        b
    );
}

/// `/status` の応答は上位 50 ホスト + 上位 50 接続元でも 64 KiB に収まること (T13.3)。
///
/// 監視が 5 秒ごとに引く口なので、太らせない。長い名前 (RFC の上限に近い 200 バイト) を
/// 並べた最悪に近い形で測る。
#[test]
fn the_status_json_stays_under_64_kib() {
    let m = Metrics::new();
    for i in 0..200 {
        let host = format!("connect://{}{:03}.example.net:443", "n".repeat(180), i);
        m.record_host_detail(
            &host,
            HostOutcome::Error,
            u64::MAX / 2,
            Some(Duration::from_millis(1234)),
            Detail {
                dns_ms: 9999,
                dns_misses: 7,
                connect_ms: 8888,
                family_v6: Some(true),
                cause: Some(ErrCause::Dns),
                first_byte_ms: None,
                ..Detail::default()
            },
        );
        m.record_client(
            &format!("2001:db8:{:04x}:{:04x}::{:04x}", i, i, i),
            HostOutcome::Error,
            u64::MAX / 2,
            // 向き別も桁を振り切らせる (1 行の JSON を最悪にする。T14.26)
            (u64::MAX / 2, u64::MAX / 2),
            Some(Duration::from_millis(1234)),
            Some("connect://very-long-host-name.example.com:443"),
        );
    }
    for sort in [
        HostSort::Requests,
        HostSort::Errors,
        HostSort::Dns,
        HostSort::Slow,
    ] {
        let json = m.to_json_with_cache(
            None,
            StatusExtras {
                sort,
                ..StatusExtras::default()
            },
        );
        assert!(json.len() <= 64 * 1024, "{} バイト", json.len());
    }
}

/// `?hosts=0` の軽い口 (T25.1 段 2): **`hosts` の鍵だけ**が消え、ほかの欄は変わらないこと。
///
/// 既定 (`hosts: true`) は今までどおり `hosts[]` を出すので、その側も一緒に見る。
/// **2 回の組み立てをバイトで比べない**: `uptime_secs` や `/proc` の値は呼ぶたびに動く
/// (この repo の他の試験と同じ作法で、動かない欄だけを見る)。
#[test]
fn the_light_status_drops_only_the_hosts_table() {
    let m = Metrics::new();
    m.record_host_detail(
        "connect://light.example.net:443",
        HostOutcome::Hit,
        100,
        Some(Duration::from_millis(12)),
        Detail::default(),
    );
    m.record_client(
        "10.0.0.1",
        HostOutcome::Hit,
        100,
        (40, 60),
        None,
        Some("connect://light.example.net:443"),
    );

    let full = m.to_json_with_cache(None, StatusExtras::default());
    let light = m.to_json_with_cache(
        None,
        StatusExtras {
            hosts: false,
            ..StatusExtras::default()
        },
    );

    // 既定は今までどおり `hosts[]` を出す
    assert!(
        full.contains("\"hosts\":[{\"host\":\"connect://light.example.net:443\""),
        "{}",
        full
    );
    // 軽い方には `hosts` の鍵が無い (`clients[]` の `host` の欄は別物なので残る)
    assert!(!light.contains("\"hosts\":"), "{}", light);
    // 表の外は同じ: 接続元の行と、通算の始まり (restored_since) が一致する
    assert!(
        light.contains("\"clients\":[{\"client\":\"10.0.0.1\""),
        "{}",
        light
    );
    let at = number_of(&full, "\"restored_since\":");
    assert_ne!(at, 0, "表に 1 行あるので 0 ではない: {}", full);
    assert_eq!(at, number_of(&light, "\"restored_since\":"), "{}", light);
    // 軽い方が小さい
    assert!(
        light.len() < full.len(),
        "light {} B / full {} B",
        light.len(),
        full.len()
    );
    // どの `?sort=` でも軽い方の形は同じ (並べ替える行が無いので鍵だけが違う)
    for sort in [HostSort::Errors, HostSort::Dns, HostSort::Slow] {
        let light_sorted = m.to_json_with_cache(
            None,
            StatusExtras {
                hosts: false,
                sort,
                ..StatusExtras::default()
            },
        );
        assert!(!light_sorted.contains("\"hosts\":"), "{}", light_sorted);
        assert!(
            light_sorted.contains("\"clients\":[{\"client\":\"10.0.0.1\""),
            "{}",
            light_sorted
        );
    }
}

/// `restored_since` の値が無いときは 0 で、`clients[]` の行だけが「表が空ではない」を
/// 語る — **表が空 (`hosts[]` も `clients[]` も空) の軽い口**も壊れないこと。
#[test]
fn the_light_status_of_an_empty_table_is_still_valid() {
    let m = Metrics::new();
    let light = m.to_json_with_cache(
        None,
        StatusExtras {
            hosts: false,
            ..StatusExtras::default()
        },
    );
    assert!(!light.contains("\"hosts\":"), "{}", light);
    assert_eq!(number_of(&light, "\"restored_since\":"), 0, "{}", light);
    // 鍵は `clients` で途切れず、次の鍵へ続く (`,` の付け忘れ・二重をここで見る)
    assert!(light.contains("\"clients\":[],"), "{}", light);
}

/// `"key":` の直後の整数を読む (試験の中だけ。status の JSON は 1 行)。
fn number_of(json: &str, key: &str) -> u64 {
    let at = json
        .find(key)
        .unwrap_or_else(|| panic!("{} が無い: {}", key, json))
        + key.len();
    json[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap()
}

/// 試験のための固定のプロセスの値 (T.34.41)。`full` が偽なら全部読めなかった形。
struct FixedFacts {
    full: bool,
}

impl super::facts::ProcessFacts for FixedFacts {
    fn threads(&self) -> Option<u64> {
        self.full.then_some(7)
    }

    fn fds(&self) -> Option<(u64, u64)> {
        self.full.then_some((9, 1024))
    }

    fn rss(&self) -> Option<crate::sysinfo::ProcessRss> {
        self.full.then_some(crate::sysinfo::ProcessRss {
            rss: 1_000_000,
            anon: Some(600_000),
            file: Some(300_000),
            shmem: Some(100_000),
        })
    }

    fn stacks_resident(&self) -> Option<u64> {
        self.full.then_some(49_152)
    }

    fn heap(&self) -> Option<crate::sysinfo::MallocInfo> {
        self.full.then_some(crate::sysinfo::MallocInfo {
            arena: 800_000,
            used: 500_000,
            free: 200_000,
            mmap: 65_536,
            mmap_blocks: 1,
        })
    }

    fn boot_json(&self) -> String {
        "{\"id\":\"b00t\",\"pid\":42}".to_string()
    }
}

/// `/status` のうちプロセスの値から来る欄を、固定の値でバイト単位に縛る (T.34.41)。
/// `threads`・`fds`・`max_fds`・`boot`・`memory` (rss・heap・スタック・常駐の内訳)。
#[test]
fn the_process_facts_land_byte_for_byte_in_the_status() {
    use super::memory::RingBytes;
    let m = Metrics::new();
    let extra = StatusExtras {
        concurrency: Concurrency {
            live_threads: 3,
            ..Concurrency::default()
        },
        ..StatusExtras::default()
    };
    let mut rings = String::new();
    RingBytes::full().push_json(&mut rings);
    let mut used = String::new();
    RingBytes::used(&m).push_json(&mut used);
    let tail = format!(
        ",\"rings_touched\":false,\"baseline\":null,\"trim\":{},",
        crate::sysinfo::trim_stats().to_json()
    );

    let json = m.to_json_with_facts(None, extra, &FixedFacts { full: true });
    assert!(
        json.contains(
            "\"threads\":7,\"fds\":9,\"max_fds\":1024,\"active_connections\":0,\"max_conns\":0,"
        ),
        "{}",
        json
    );
    // スタックの予約 = 接続スレッド 3 本 × 256 KiB + 残り 4 本 × 2 MiB
    let memory = format!(
        concat!(
            "\"boot\":{{\"id\":\"b00t\",\"pid\":42}},\"memory\":{{\"rss\":1000000,",
            "\"heap_used\":500000,\"heap_free\":200000,\"mmap\":65536,",
            "\"stacks_estimate\":9175040,\"cache_memory\":0,\"rings\":{},\"arenas\":{},",
            "\"rings_used\":{}{}\"rss_anon\":600000,\"rss_file\":300000,\"rss_shmem\":100000,",
            "\"stacks_resident\":49152}},\"recent_quantiles\":"
        ),
        rings,
        crate::sysinfo::arena_max(),
        used,
        tail
    );
    assert!(json.contains(&memory), "{}\n---\n{}", memory, json);

    // 何も読めない環境: 数は 0、内訳は `null` (`/status` は落とさない)
    let json = m.to_json_with_facts(None, StatusExtras::default(), &FixedFacts { full: false });
    assert!(
        json.contains("\"threads\":0,\"fds\":0,\"max_fds\":0,"),
        "{}",
        json
    );
    let memory = format!(
        concat!(
            "\"memory\":{{\"rss\":null,\"heap_used\":null,\"heap_free\":null,\"mmap\":null,",
            "\"stacks_estimate\":null,\"cache_memory\":0,\"rings\":{},\"arenas\":{},",
            "\"rings_used\":{}{}\"rss_anon\":null,\"rss_file\":null,\"rss_shmem\":null,",
            "\"stacks_resident\":null}}"
        ),
        rings,
        crate::sysinfo::arena_max(),
        used,
        tail
    );
    assert!(json.contains(&memory), "{}\n---\n{}", memory, json);
}
