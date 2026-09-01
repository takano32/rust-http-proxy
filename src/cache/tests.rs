//! `Cache` 全体 (両層 + 予算) の結合テスト。各層の単体テストはそれぞれのモジュールにある。

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::config::{CacheConfig, Limit, MIB};
use super::key::cache_key;
use super::memory::{BALLAST_CHUNK, MemTier};
use super::{Cache, CacheSource, format};

fn test_dir(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(name);
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// `$TMPDIR` は tmpfs のことが多いので、fallocate を試すテストは実ディスク上の `target/` を使う。
fn on_disk_test_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-cache")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn fresh(name: &str, mem: u64, disk: u64) -> Cache {
    Cache::new(CacheConfig::fixed(mem, disk, test_dir(name)))
}

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
    assert_eq!(&resp.bytes[..], b"payload");
    assert_eq!(src, CacheSource::Memory);
    assert_eq!(cache.hits_mem.load(Ordering::Relaxed), 1);
    assert!(
        cache.disk_path(key).exists(),
        "entry should also be on disk"
    );
}

#[test]
fn disk_hit_after_memory_eviction_promotes() {
    // メモリ 100 バイト上限 → 2 件目投入で 1 件目が L1 から追い出される
    let cache = fresh("shp-test-l2", 100, 4 * MIB);
    let k1 = cache_key("GET", "http://example.com/1");
    let k2 = cache_key("GET", "http://example.com/2");
    cache.put(
        k1,
        "http://example.com/1",
        vec![b'a'; 80],
        Duration::from_secs(60),
        1,
    );
    cache.put(
        k2,
        "http://example.com/2",
        vec![b'b'; 80],
        Duration::from_secs(60),
        2,
    );
    assert_eq!(cache.mem_usage().1, 1, "L1 should hold only one entry");

    let (resp, src) = cache.get(k1, 3).expect("expected disk hit");
    assert_eq!(src, CacheSource::Disk);
    assert_eq!(resp.bytes.len(), 80);
    assert_eq!(cache.hits_disk.load(Ordering::Relaxed), 1);
    // 昇格したので次はメモリヒット
    assert_eq!(cache.get(k1, 4).unwrap().1, CacheSource::Memory);
}

#[test]
fn expired_entry_is_a_miss() {
    let cache = fresh("shp-test-expire", MIB, MIB);
    let key = cache_key("GET", "http://example.com/exp");
    cache.put(
        key,
        "http://example.com/exp",
        b"old".to_vec(),
        Duration::from_secs(0),
        1,
    );
    assert!(cache.get(key, 1).is_none());
    assert_eq!(cache.misses.load(Ordering::Relaxed), 1);
}

#[test]
fn disk_capacity_is_enforced_by_lru() {
    let cache = fresh("shp-test-diskcap", 10 * MIB, 700);
    for i in 0..10 {
        let url = format!("http://example.com/{}", i);
        cache.put(
            cache_key("GET", &url),
            &url,
            vec![b'x'; 200],
            Duration::from_secs(60),
            i,
        );
    }
    let (bytes, entries) = cache.disk_usage();
    assert!(bytes <= 700, "disk usage {} exceeds limit", bytes);
    assert!(entries < 10 && entries > 0);
    assert!(cache.evictions.load(Ordering::Relaxed) > 0);
    // 最古のものからディスク上で消えている (メモリ層には残っていてよい)
    assert!(
        !cache
            .disk_path(cache_key("GET", "http://example.com/0"))
            .exists()
    );
    assert!(
        cache
            .disk_path(cache_key("GET", "http://example.com/9"))
            .exists()
    );
}

#[test]
fn oversized_object_is_not_cached() {
    let mut cfg = CacheConfig::fixed(MIB, MIB, test_dir("shp-test-oversize"));
    cfg.max_object_size = 10;
    let cache = Cache::new(cfg);
    let key = cache_key("GET", "http://example.com/big");
    cache.put(
        key,
        "http://example.com/big",
        vec![b'z'; 100],
        Duration::from_secs(60),
        1,
    );
    assert!(cache.get(key, 1).is_none());
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
fn disk_index_is_restored_on_startup_without_reading_bodies() {
    let cfg = CacheConfig::fixed(MIB, MIB, test_dir("shp-test-restore"));
    let key = cache_key("GET", "http://example.com/persist");
    {
        let cache = Cache::new(cfg.clone());
        cache.put(
            key,
            "http://example.com/persist",
            b"persisted".to_vec(),
            Duration::from_secs(600),
            1,
        );
    }
    let cache2 = Cache::new(cfg);
    assert_eq!(cache2.disk_usage().1, 1);
    let (resp, src) = cache2.get(key, 1).expect("expected restored disk hit");
    assert_eq!(&resp.bytes[..], b"persisted");
    assert_eq!(src, CacheSource::Disk);
}

#[test]
fn legacy_flat_layout_is_migrated_into_shards() {
    let dir = test_dir("shp-test-migrate");
    fs::create_dir_all(&dir).unwrap();
    let key = cache_key("GET", "http://example.com/legacy");
    let now = super::now_epoch();
    fs::write(
        dir.join(format!("{}.cache", key)),
        format::encode("http://example.com/legacy", b"old-layout", now, now + 600),
    )
    .unwrap();
    // 期限切れの旧ファイルは消える
    let stale = cache_key("GET", "http://example.com/stale");
    fs::write(
        dir.join(format!("{}.cache", stale)),
        format::encode("http://example.com/stale", b"x", now - 10, now - 1),
    )
    .unwrap();

    let cache = Cache::new(CacheConfig::fixed(MIB, MIB, dir.clone()));
    assert!(cache.disk_path(key).exists());
    assert!(!dir.join(format!("{}.cache", key)).exists());
    assert!(!dir.join(format!("{}.cache", stale)).exists());
    assert_eq!(cache.disk_usage().1, 1);
    let (resp, src) = cache.get(key, 1).unwrap();
    assert_eq!(&resp.bytes[..], b"old-layout");
    assert_eq!(src, CacheSource::Disk);
}

#[test]
fn stale_mtime_falls_back_to_header() {
    let dir = test_dir("shp-test-stale-mtime");
    let key = cache_key("GET", "http://example.com/copied");
    let now = super::now_epoch();
    {
        let cache = Cache::new(CacheConfig::fixed(MIB, MIB, dir.clone()));
        cache.put(
            key,
            "http://example.com/copied",
            b"body".to_vec(),
            Duration::from_secs(600),
            1,
        );
        // コピー等で mtime が「現在」になってしまったケースを再現
        let f = fs::OpenOptions::new()
            .write(true)
            .open(cache.disk_path(key))
            .unwrap();
        f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(now - 100))
            .unwrap();
    }
    let cache = Cache::new(CacheConfig::fixed(MIB, MIB, dir));
    assert_eq!(cache.disk_usage().1, 1, "header says it is still valid");
    assert!(cache.get(key, 1).is_some());
}

#[test]
fn to_json_reports_limits_and_mode() {
    let cache = fresh("shp-test-json", 200 * MIB, 2048 * MIB);
    let json = cache.to_json();
    assert!(json.contains("\"limit_bytes\":209715200"), "{}", json);
    assert!(json.contains("\"limit_bytes\":2147483648"), "{}", json);
    assert!(json.contains("\"hit_ratio\":0.0000"), "{}", json);
    assert!(json.contains("\"mode\":\"fixed\""), "{}", json);
    assert!(json.contains("\"reserve\":false"), "{}", json);
    assert!(json.contains("\"quota_bytes\":null"), "{}", json);
}

#[test]
fn shrinking_budget_evicts_down_to_new_capacity() {
    let cache = fresh("shp-test-shrink", 10 * MIB, 10 * MIB);
    for i in 0..8 {
        let url = format!("http://example.com/{}", i);
        cache.put(
            cache_key("GET", &url),
            &url,
            vec![b'x'; MIB as usize],
            Duration::from_secs(60),
            i,
        );
    }
    assert_eq!(cache.mem_usage().1, 8);
    cache.mem.set_capacity(3 * MIB);
    cache.disk.set_capacity(2 * MIB);
    let evicted = cache.mem.enforce() + cache.disk.enforce();
    assert!(evicted > 0);
    assert!(cache.mem_usage().0 <= 3 * MIB);
    assert!(cache.disk_usage().0 <= 2 * MIB);
    assert_eq!(cache.mem_usage().1, 3);
}

#[test]
fn sweep_removes_expired_entries_from_both_tiers() {
    let cache = fresh("shp-test-sweep", MIB, MIB);
    let key = cache_key("GET", "http://example.com/short");
    cache.put(
        key,
        "http://example.com/short",
        b"soon".to_vec(),
        Duration::from_secs(0),
        1,
    );
    assert_eq!(cache.mem_usage().1, 1);
    let now = super::now_epoch() + 1;
    assert_eq!(
        cache.mem.sweep_expired(now) + cache.disk.sweep_expired(now),
        2
    );
    assert_eq!(cache.mem_usage().1, 0);
    assert_eq!(cache.disk_usage().1, 0);
    assert!(!cache.disk_path(key).exists());
}

#[test]
fn memory_ballast_gives_way_to_entries_and_budget_cuts() {
    let tier = MemTier::new(true);
    tier.set_capacity(BALLAST_CHUNK as u64);
    assert_eq!(tier.fill_ballast(), BALLAST_CHUNK as u64);
    assert_eq!(tier.owned(), BALLAST_CHUNK as u64);

    // 1 MiB のエントリが入るとバラストは 1 ブロック解放される
    let data = std::sync::Arc::new(vec![1u8; MIB as usize]);
    tier.insert(cache_key("GET", "http://a/"), data, 0, u64::MAX, 1);
    assert_eq!(tier.ballast_bytes(), 0);
    assert_eq!(tier.usage(), (MIB, 1));
    assert_eq!(tier.fill_ballast(), 0, "63 MiB left is less than one chunk");

    // 予算が増えれば埋め直し、減ればバラストから返す
    tier.set_capacity(2 * BALLAST_CHUNK as u64 + 2 * MIB);
    assert_eq!(tier.fill_ballast(), 2 * BALLAST_CHUNK as u64);
    tier.set_capacity(BALLAST_CHUNK as u64);
    assert_eq!(
        tier.enforce(),
        0,
        "entries still fit; only ballast is released"
    );
    assert_eq!(tier.ballast_bytes(), 0);
    assert_eq!(tier.usage().1, 1);
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
#[test]
fn disk_ballast_is_preallocated_and_shrinks_for_writes() {
    let mut cfg = CacheConfig::fixed(MIB, 48 * MIB, on_disk_test_dir("shp-test-disk-ballast"));
    cfg.reserve = true;
    let cache = Cache::new(cfg);
    if !cache.disk.reserve_active() {
        eprintln!("skipping: fallocate unsupported on this filesystem");
        return;
    }
    let ballast = cache.cfg.dir.join("ballast.reserve");
    let added = cache.disk.fill_ballast();
    if added == 0 {
        eprintln!("skipping: could not preallocate (probably no space)");
        return;
    }
    assert_eq!(added, 48 * MIB);
    assert_eq!(fs::metadata(&ballast).unwrap().len(), 48 * MIB);

    let key = cache_key("GET", "http://example.com/one");
    cache.put(
        key,
        "http://example.com/one",
        vec![b'y'; 4 * MIB as usize],
        Duration::from_secs(60),
        1,
    );
    let (bytes, _) = cache.disk_usage();
    assert!(bytes > 4 * MIB);
    assert!(cache.disk_reserved() + bytes <= 48 * MIB);
    assert_eq!(fs::metadata(&ballast).unwrap().len(), cache.disk_reserved());
    drop(cache);
    assert!(!ballast.exists(), "ballast is removed on shutdown");
    let _ = fs::remove_dir_all(on_disk_test_dir("shp-test-disk-ballast"));
}

#[test]
fn quota_mode_budgets_against_allocation_minus_others() {
    let root = test_dir("shp-test-quota");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("server.jar"), vec![0u8; 10 * MIB as usize]).unwrap();
    let cfg = CacheConfig {
        disk_limit: Limit::Auto { percent: 90 },
        disk_quota: Some(100 * MIB),
        quota_root: Some(root.clone()),
        pterodactyl: true,
        ..CacheConfig::fixed(MIB, 0, root.join("cache"))
    };
    let cache = Cache::new(cfg);
    // 90 MiB − 他ファイル 10 MiB = 80 MiB
    assert_eq!(cache.disk_capacity(), 80 * MIB);
    let json = cache.to_json();
    assert!(json.contains("\"quota_bytes\":104857600"), "{}", json);
    assert!(json.contains("\"disk_total_bytes\":104857600"), "{}", json);
}

#[test]
fn pterodactyl_without_quota_falls_back_to_fixed_disk_limit() {
    let cfg = CacheConfig {
        disk_limit: Limit::Auto { percent: 90 },
        pterodactyl: true,
        ..CacheConfig::fixed(MIB, 0, test_dir("shp-test-ptero-fallback"))
    };
    let cache = Cache::new(cfg);
    assert_eq!(cache.disk_capacity(), super::config::FALLBACK_DISK);
}

#[cfg(target_os = "linux")]
#[test]
fn auto_mode_measures_the_system() {
    let cfg = CacheConfig {
        mem_limit: Limit::Auto { percent: 90 },
        disk_limit: Limit::Auto { percent: 90 },
        ..CacheConfig::fixed(0, 0, test_dir("shp-test-auto"))
    };
    let cache = Cache::new(cfg);
    let snap = cache.snapshot();
    let mem = snap.mem.expect("memory snapshot on linux");
    assert!(mem.total > 0);
    assert!(cache.mem_capacity() <= super::budget::percent_of(mem.total, 90));
    assert!(snap.fs.is_some() || cache.disk_capacity() == super::config::FALLBACK_DISK);
    let json = cache.to_json();
    assert!(json.contains("\"mode\":\"auto\""), "{}", json);
    assert!(json.contains("\"target_percent\":90"), "{}", json);
    // プローブを回しても壊れない
    cache.probe_tick();
}
