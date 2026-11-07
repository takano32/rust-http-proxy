//! `Cache` の結合テスト (basic)。

#![allow(unused_imports)]
use super::{fresh, on_disk_test_dir, test_dir, wire};
use crate::cache::config::{CacheConfig, DiskQuota, Limit, MIB, Reserve};
use crate::cache::key::cache_key;
use crate::cache::memory::{BALLAST_CHUNK, MemTier};
use crate::cache::{Body, Cache, CacheSource, Meta, format};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn put_get_roundtrip_hits_memory() {
    let cache = fresh("shp-test-roundtrip", MIB, 4 * MIB);
    let key = cache_key("GET", "http://example.com/x");
    cache.put(
        key,
        "http://example.com/x",
        b"payload".to_vec(),
        Duration::from_secs(60),
        1,
    );

    let (resp, src) = cache.get(key, 1).expect("expected hit");
    assert_eq!(src, CacheSource::Memory);
    assert!(resp.is_fresh(crate::cache::now_epoch()));
    assert_eq!(resp.read_all().unwrap(), b"payload");
    assert_eq!(cache.counters().hits_mem(), 1);
    assert!(
        cache.disk_path(key).unwrap().exists(),
        "entry should also be on disk"
    );
}

#[test]
fn oversized_object_is_not_cached() {
    let mut cfg = CacheConfig::fixed(MIB, MIB, test_dir("shp-test-oversize"));
    cfg.max_object_size = 10;
    cfg.mem_max_object_size = 10;
    let cache = Cache::new(cfg);
    let key = cache_key("GET", "http://example.com/big");
    let out = cache.put_with(
        key,
        "http://example.com/big",
        vec![b'z'; 100],
        Duration::from_secs(60),
        false,
        1,
    );
    assert!(!out.memory && !out.disk);
    assert!(cache.get(key, 1).is_none());
    assert_eq!(cache.counters().stores(), 0);
}

#[test]
fn disabled_cache_is_noop() {
    let cache = Cache::new(CacheConfig::disabled());
    let key = cache_key("GET", "http://example.com/off");
    cache.put(
        key,
        "http://example.com/off",
        b"body".to_vec(),
        Duration::from_secs(60),
        1,
    );
    assert!(cache.get(key, 1).is_none());
    assert_eq!(cache.mem_capacity(), 0);
}

#[test]
fn to_json_reports_limits_and_mode() {
    let cache = fresh("shp-test-json", 200 * MIB, 2048 * MIB);
    let json = cache.to_json();
    assert!(json.contains("\"limit_bytes\":209715200"), "{}", json);
    assert!(json.contains("\"limit_bytes\":2147483648"), "{}", json);
    assert!(json.contains("\"hit_ratio\":0.0000"), "{}", json);
    assert!(json.contains("\"mode\":\"fixed\""), "{}", json);
    assert!(json.contains("\"reserve\":\"off\""), "{}", json);
    assert!(json.contains("\"quota_bytes\":null"), "{}", json);
    assert!(json.contains("\"revalidations\":0"), "{}", json);
    assert!(json.contains("\"not_stored_rotations\":0"), "{}", json);
    assert!(json.contains("\"keep_free_bytes\":0"), "{}", json);
}

#[test]
fn invalidate_removes_every_variant_of_a_url() {
    let cache = fresh("shp-test-invalidate", MIB, MIB);
    let url = "http://example.com/item";
    let plain = cache_key("GET", url);
    let gz = crate::cache::cache_key_variant("GET", url, "gzip");
    cache.put(plain, url, b"plain".to_vec(), Duration::from_secs(60), 1);
    cache.put(gz, url, b"gzip".to_vec(), Duration::from_secs(60), 1);
    assert!(cache.get(plain, 1).is_some() && cache.get(gz, 1).is_some());
    assert_eq!(cache.invalidate(url, 1), 2);
    assert!(cache.get(plain, 1).is_none() && cache.get(gz, 1).is_none());
    assert_eq!(cache.disk_usage().1, 0);
}

#[test]
fn admission_gates_first_sightings_only_when_the_tier_is_full() {
    // ディスク層なし、メモリ 1 MiB: 余裕があるうちは初見でも通す
    let mut cfg = CacheConfig::fixed(MIB, 0, test_dir("shp-test-admission"));
    cfg.reserve = Reserve::Off;
    let cache = Cache::new(cfg);
    let k1 = cache_key("GET", "http://example.com/1");
    assert!(cache.admit(k1), "empty cache admits a first sighting");
    // 90% まで埋める
    for i in 0..10 {
        let url = format!("http://example.com/fill/{}", i);
        cache.put(
            cache_key("GET", &url),
            &url,
            vec![b'x'; 96 * 1024],
            Duration::from_secs(60),
            1,
        );
    }
    let (used, _) = cache.mem_usage();
    assert!(
        used * 10 >= cache.mem_capacity() * 9,
        "tier is full (used {})",
        used
    );
    let k2 = cache_key("GET", "http://example.com/2");
    assert!(!cache.admit(k2), "first sighting is refused under pressure");
    assert!(cache.admit(k2), "second sighting is admitted");
    assert_eq!(cache.counters().admission_rejected(), 1);
    assert!(
        cache.admit(k1),
        "a key seen before the pressure is admitted"
    );

    let mut off = CacheConfig::fixed(MIB, 0, test_dir("shp-test-admission-off"));
    off.admission = false;
    let cache = Cache::new(off);
    assert!(cache.admit(k2));
}

#[test]
fn coalescing_is_skipped_for_keys_that_were_not_stored() {
    // 既定の TTL = 入れ替えの周期 (`CacheConfig::fixed` の既定は 300 秒)
    let cache = fresh("shp-test-not-stored", MIB, 0);
    let k = cache_key("GET", "http://example.com/no-store");
    let other = cache_key("GET", "http://example.com/cacheable");
    let now = 1_000_000;
    // 初めて見る鍵は今までどおり合流する
    assert!(cache.may_coalesce(k, now));
    // leader が「保存しなかった」で終わったら、次からは合流を通さない
    cache.remember_not_stored(k, now);
    assert!(!cache.may_coalesce(k, now));
    // 他の鍵は巻き添えにならない
    assert!(cache.may_coalesce(other, now));
    // 1 周期では「直前」の側に残るのでまだ覚えている
    assert!(!cache.may_coalesce(k, now + 301));
    assert_eq!(cache.not_stored_rotations(), 1);
    // 入れ替えた回数は `/status` の `cache` にも出る (T12.6)
    assert!(
        cache.to_json().contains("\"not_stored_rotations\":1"),
        "{}",
        cache.to_json()
    );
    // 2 周期で忘れて合流に戻る (オリジンが Cache-Control を変えることがある)
    assert!(cache.may_coalesce(k, now + 602));
    assert_eq!(cache.not_stored_rotations(), 2);
    assert!(
        cache.to_json().contains("\"not_stored_rotations\":2"),
        "{}",
        cache.to_json()
    );
}

/// 名前の違うキャッシュ可能な GET を 50,000 本保存して追い出しても、バリアントの表は
/// 掃除の上限 (両層の件数の 2 倍、下限 4,096) を越えず、無効化は効いたまま (T32.6)。
/// 0.30.4 までは 1 本ごとに 1 行足して消さなかった (50,000 行)。
#[test]
fn variants_table_stays_bounded_after_evictions() {
    let mut cfg = CacheConfig::fixed(MIB, 0, test_dir("shp-test-variants-bound"));
    cfg.reserve = Reserve::Off;
    let cache = Cache::new(cfg);
    let mut max_len = 0;
    for i in 0..50_000 {
        let url = format!("http://example.com/v/{}", i);
        // URL そのもののキーは覚えない。Vary で分かれたキーだけが表に入る
        let plain = cache_key("GET", &url);
        let gz = crate::cache::cache_key_variant("GET", &url, "gzip");
        cache.put(plain, &url, vec![b'p'; 512], Duration::from_secs(60), 1);
        cache.put(gz, &url, vec![b'g'; 512], Duration::from_secs(60), 1);
        max_len = max_len.max(cache.variants_len());
    }
    let live = cache.mem_usage().1 + cache.disk_usage().1;
    assert!(
        live < 50_000,
        "メモリ 1 MiB なので追い出されている ({})",
        live
    );
    assert!(
        max_len <= (live * 2).max(4096),
        "表 {} 行 (両層 {} 件)",
        max_len,
        live
    );
    // 最後の 1 本は両層に居るので、表から無効化できる
    let url = "http://example.com/v/49999";
    let gz = crate::cache::cache_key_variant("GET", url, "gzip");
    assert!(cache.get(gz, 1).is_some());
    assert_eq!(cache.invalidate(url, 1), 2);
    assert!(cache.get(gz, 1).is_none());
}

/// 注入した時計だけが「いま」を決める (T35.1): 手で進めるまで新鮮、進めれば期限切れ、
/// `refresh` は進めた「いま」から延ばす。
#[test]
fn freshness_follows_the_injected_clock() {
    use crate::clock::ManualClock;
    let clock = ManualClock::at(1_000_000);
    let cfg = CacheConfig::fixed(MIB, 4 * MIB, test_dir("shp-test-manual-clock"));
    let cache = Cache::with_clock(cfg, clock.clone());
    assert_eq!(cache.now(), 1_000_000);
    let url = "http://example.com/clock";
    let key = cache_key("GET", url);
    cache.put_with(key, url, wire(b"tick"), Duration::from_secs(10), true, 1);

    let (resp, _) = cache.get(key, 1).expect("hit");
    assert_eq!(
        resp.meta.stored_at, 1_000_000,
        "保存の時刻も注入した時計から"
    );
    assert!(resp.is_fresh(cache.now()));
    assert_eq!(resp.age_at(cache.now()), 0);

    clock.advance(Duration::from_secs(11));
    let (resp, _) = cache
        .get(key, 1)
        .expect("期限切れでもバリデータ付きなら返る");
    assert!(!resp.is_fresh(cache.now()), "進めた時計で期限が切れる");
    assert_eq!(resp.age_at(cache.now()), 11);

    let expires = cache.refresh(key, Duration::from_secs(10), 0, 1);
    assert_eq!(expires, 1_000_011 + 10, "延命は進めた「いま」から");
    let (resp, _) = cache.get(key, 1).expect("hit");
    assert!(resp.is_fresh(cache.now()));
    assert_eq!(cache.peek(key).unwrap().meta.expires_at, 1_000_021);
}

/// 長さの分からない (chunked の) 本文は 64 KiB から倍々で育つ `Vec` に受けるが、保存の最後で
/// 縮めるので、メモリに置く本文の確保は長さと同じ (T35.11 の案 2。前は 3 MiB の本文で 4 MiB 確保していた)。
#[test]
fn chunked_body_is_kept_without_the_doubling_slack() {
    let cache = fresh("shp-test-chunked-shrink", 8 * MIB, 16 * MIB);
    let url = "http://example.com/chunked";
    let key = cache_key("GET", url);
    let payload = wire(&vec![b'C'; 3 * MIB as usize]);
    let mut sink = cache.begin_store(key, url, Duration::from_secs(60), 0, false, None, 1);
    for chunk in payload.chunks(70_000) {
        sink.write(chunk);
    }
    assert!(sink.finish().memory);

    let (resp, src) = cache.get(key, 1).expect("hit");
    assert_eq!(src, CacheSource::Memory);
    let Body::Memory { data, .. } = &resp.body else {
        panic!("メモリの本文のはず");
    };
    assert_eq!(data.len(), payload.len());
    assert_eq!(data.capacity(), data.len(), "倍々に育てた余りを返している");
}

/// `/status` の `cache.memory.alloc_bytes` は本文の確保の大きさの合計で、保存の最後で縮めるので
/// 長さの分からない本文を入れても `used_bytes` と一致する (T35.11 の案 1。判定表 phase31 が読む)。
#[test]
fn status_reports_alloc_bytes_equal_to_used_bytes() {
    let cache = fresh("shp-test-alloc-bytes", 8 * MIB, 16 * MIB);
    assert!(
        cache
            .to_json()
            .contains("\"cgroup_keep_free_bytes\":0,\"alloc_bytes\":0},\"disk\""),
        "{}",
        cache.to_json()
    );
    for (i, len) in [1_000usize, 200_000, 3 * MIB as usize]
        .into_iter()
        .enumerate()
    {
        let url = format!("http://example.com/alloc/{}", i);
        let key = cache_key("GET", &url);
        let payload = wire(&vec![b'A'; len]);
        let mut sink = cache.begin_store(key, &url, Duration::from_secs(60), 0, false, None, 1);
        for chunk in payload.chunks(7_000) {
            sink.write(chunk);
        }
        assert!(sink.finish().memory);
    }
    cache.put(
        cache_key("GET", "http://example.com/alloc/cl"),
        "http://example.com/alloc/cl",
        wire(b"known length"),
        Duration::from_secs(60),
        1,
    );
    let (used, entries) = cache.mem_usage();
    assert_eq!(entries, 4);
    assert_eq!(cache.mem_alloc_bytes(), used);
    let json = cache.to_json();
    assert!(
        json.contains(&format!("\"alloc_bytes\":{}}},\"disk\"", used)),
        "{}",
        json
    );
    assert!(
        json.contains(&format!("\"memory\":{{\"used_bytes\":{},", used)),
        "{}",
        json
    );
}
