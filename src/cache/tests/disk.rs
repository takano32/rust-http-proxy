//! `Cache` の結合テスト (disk)。

#![allow(unused_imports)]
use super::{fresh, on_disk_test_dir, test_dir, wire};
use crate::cache::config::{CacheConfig, DiskQuota, Limit, MIB};
use crate::cache::key::cache_key;
use crate::cache::memory::{BALLAST_CHUNK, MemTier};
use crate::cache::{Body, Cache, CacheSource, Meta, format};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

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
    assert_eq!(resp.size, 80);
    assert_eq!(resp.read_all().unwrap().len(), 80);
    assert_eq!(cache.hits_disk.load(Ordering::Relaxed), 1);
    // 昇格したので次はメモリヒット
    assert_eq!(cache.get(k1, 4).unwrap().1, CacheSource::Memory);
}

#[test]
fn expired_entry_without_validators_is_a_miss() {
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
    assert!(cache.disk_path(key).is_none());
}

#[test]
fn stale_entry_with_validators_survives_and_can_be_refreshed() {
    let cfg = CacheConfig::fixed(MIB, MIB, test_dir("shp-test-stale"));
    let key = cache_key("GET", "http://example.com/stale");
    let now = crate::cache::now_epoch();
    {
        let cache = Cache::new(cfg.clone());
        let out = cache.put_with(
            key,
            "http://example.com/stale",
            wire(b"v1"),
            Duration::ZERO,
            true,
            1,
        );
        assert!(out.memory && out.disk);
        let (resp, _) = cache.get(key, 1).expect("stale entry is still returned");
        assert!(!resp.is_fresh(now));
        assert!(resp.meta.validators);
        assert!(
            cache
                .disk_path(key)
                .unwrap()
                .to_string_lossy()
                .ends_with(".vcache")
        );

        let expires = cache.refresh(key, Duration::from_secs(600), 0, 1);
        assert!(expires > now);
        let (resp, _) = cache.get(key, 1).unwrap();
        assert!(resp.is_fresh(now));
        assert_eq!(cache.revalidations.load(Ordering::Relaxed), 1);
    }
    // 再起動後も (mtime が延びているので) 新鮮なまま復元される
    let cache = Cache::new(cfg.clone());
    let (resp, src) = cache.get(key, 2).expect("restored");
    assert_eq!(src, CacheSource::Disk);
    assert!(resp.is_fresh(now));
    assert!(resp.meta.validators);
    assert_eq!(resp.read_all().unwrap(), wire(b"v1"));
}

#[test]
fn stale_revalidatable_entry_survives_restart_via_header() {
    let cfg = CacheConfig::fixed(MIB, MIB, test_dir("shp-test-stale-restart"));
    let key = cache_key("GET", "http://example.com/s");
    {
        let cache = Cache::new(cfg.clone());
        cache.put_with(
            key,
            "http://example.com/s",
            wire(b"x"),
            Duration::ZERO,
            true,
            1,
        );
    }
    // 期限切れ + バリデータ付き → mtime は過去なのでヘッダーを読んで残す
    let cache = Cache::new(cfg.clone());
    assert_eq!(cache.disk_usage().1, 1);
    assert!(cache.get(key, 1).is_some());

    // max_stale を超えたら捨てる
    let mut old = cfg;
    old.max_stale = Duration::ZERO;
    let cache = Cache::new(old);
    assert_eq!(cache.disk_usage().1, 0);
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
        cache
            .disk_path(cache_key("GET", "http://example.com/0"))
            .is_none()
    );
    assert!(
        cache
            .disk_path(cache_key("GET", "http://example.com/9"))
            .unwrap()
            .exists()
    );
}

#[test]
fn large_object_streams_to_disk_only() {
    let mut cfg = CacheConfig::fixed(8 * MIB, 32 * MIB, test_dir("shp-test-stream"));
    cfg.mem_max_object_size = MIB;
    let cache = Cache::new(cfg);
    let key = cache_key("GET", "http://example.com/large");
    let payload = wire(&vec![b'L'; 3 * MIB as usize]);
    let mut sink = cache.begin_store(
        key,
        "http://example.com/large",
        Duration::from_secs(60),
        0,
        false,
        None,
        1,
    );
    for chunk in payload.chunks(70_000) {
        sink.write(chunk);
    }
    let out = sink.finish();
    assert!(out.disk && !out.memory);
    assert_eq!(cache.mem_usage().1, 0);
    // ディスク上のサイズは形式ヘッダーの分だけ大きい
    let (disk_bytes, disk_entries) = cache.disk_usage();
    assert_eq!(disk_entries, 1);
    assert!(disk_bytes > payload.len() as u64 && disk_bytes < payload.len() as u64 + 1024);

    let (resp, src) = cache.get(key, 2).expect("disk hit");
    assert_eq!(src, CacheSource::Disk);
    assert!(matches!(resp.body, Body::File(_)));
    assert!(resp.head.ends_with(b"\r\n\r\n"));
    assert_eq!(resp.size, payload.len() as u64);
    assert_eq!(resp.read_all().unwrap(), payload);
    assert_eq!(cache.mem_usage().1, 0, "large objects are not promoted");
}

#[test]
fn aborted_store_leaves_nothing_behind() {
    let cache = fresh("shp-test-abort", MIB, MIB);
    let key = cache_key("GET", "http://example.com/abort");
    let mut sink = cache.begin_store(
        key,
        "http://example.com/abort",
        Duration::from_secs(60),
        0,
        false,
        None,
        1,
    );
    sink.write(b"partial");
    sink.abort();
    assert!(cache.get(key, 1).is_none());
    assert_eq!(cache.disk_usage().1, 0);
    let shard_files: Vec<_> = fs::read_dir(cache.config().dir.join(format!("{:02x}", key.shard())))
        .unwrap()
        .flatten()
        .collect();
    assert!(
        shard_files.is_empty(),
        "no temp file left: {:?}",
        shard_files
    );
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
    assert_eq!(src, CacheSource::Disk);
    assert_eq!(resp.read_all().unwrap(), b"persisted");
}

#[test]
fn legacy_flat_layout_is_migrated_into_shards() {
    let dir = test_dir("shp-test-migrate");
    fs::create_dir_all(&dir).unwrap();
    let key = cache_key("GET", "http://example.com/legacy");
    let now = crate::cache::now_epoch();
    let meta = |stored_at, expires_at| Meta {
        stored_at,
        expires_at,
        validators: false,
    };
    fs::write(
        dir.join(format!("{}.cache", key)),
        format::encode(
            "http://example.com/legacy",
            b"old-layout",
            &meta(now, now + 600),
        ),
    )
    .unwrap();
    // 期限切れの旧ファイルは消える
    let stale = cache_key("GET", "http://example.com/stale");
    fs::write(
        dir.join(format!("{}.cache", stale)),
        format::encode("http://example.com/stale", b"x", &meta(now - 10, now - 1)),
    )
    .unwrap();

    let cache = Cache::new(CacheConfig::fixed(MIB, MIB, dir.clone()));
    assert!(cache.disk_path(key).unwrap().exists());
    assert!(!dir.join(format!("{}.cache", key)).exists());
    assert!(!dir.join(format!("{}.cache", stale)).exists());
    assert_eq!(cache.disk_usage().1, 1);
    let (resp, src) = cache.get(key, 1).unwrap();
    assert_eq!(src, CacheSource::Disk);
    assert_eq!(resp.read_all().unwrap(), b"old-layout");
}

#[test]
fn stale_mtime_falls_back_to_header() {
    let dir = test_dir("shp-test-stale-mtime");
    let key = cache_key("GET", "http://example.com/copied");
    let now = crate::cache::now_epoch();
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
            .open(cache.disk_path(key).unwrap())
            .unwrap();
        f.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(now - 100))
            .unwrap();
    }
    let cache = Cache::new(CacheConfig::fixed(MIB, MIB, dir));
    assert_eq!(cache.disk_usage().1, 1, "header says it is still valid");
    assert!(cache.get(key, 1).is_some());
}

#[test]
fn switching_validators_replaces_the_old_file_extension() {
    let cache = fresh("shp-test-ext-switch", MIB, MIB);
    let key = cache_key("GET", "http://example.com/switch");
    cache.put(
        key,
        "http://example.com/switch",
        b"plain".to_vec(),
        Duration::from_secs(60),
        1,
    );
    let plain_path = cache.disk_path(key).unwrap();
    assert!(plain_path.to_string_lossy().ends_with(".cache"));

    cache.put_with(
        key,
        "http://example.com/switch",
        wire(b"v"),
        Duration::from_secs(60),
        true,
        1,
    );
    let v_path = cache.disk_path(key).unwrap();
    assert!(v_path.to_string_lossy().ends_with(".vcache"));
    assert!(!plain_path.exists(), "old extension must not linger");
    assert_eq!(cache.disk_usage().1, 1);

    cache.put(
        key,
        "http://example.com/switch",
        b"plain2".to_vec(),
        Duration::from_secs(60),
        1,
    );
    assert!(!v_path.exists());
    assert!(cache.disk_path(key).unwrap().exists());
}

#[test]
fn in_flight_room_is_accounted_until_the_writer_finishes() {
    let cache = fresh("shp-test-inflight", MIB, 10 * MIB);
    let meta = Meta {
        stored_at: 1,
        expires_at: u64::MAX,
        validators: false,
    };
    let k1 = cache_key("GET", "http://example.com/a");
    let k2 = cache_key("GET", "http://example.com/b");
    // 6 MiB 予定の書き込みが場所を取ると (先読み分も含めて) 空きが無くなる
    let w1 = cache
        .disk
        .begin(k1, "http://example.com/a", meta, 1, Some(6 * MIB), u64::MAX)
        .unwrap()
        .expect("first writer gets room");
    assert!(cache.disk.in_flight_bytes() >= 6 * MIB);
    assert_eq!(cache.disk.free_room(), 0);
    // 2 本目は入らない (静かにスキップ)
    let w2 = cache
        .disk
        .begin(k2, "http://example.com/b", meta, 2, Some(6 * MIB), u64::MAX)
        .unwrap();
    assert!(w2.is_none(), "no room while the first write is in flight");
    // 中止すれば戻る
    w1.abort();
    assert_eq!(cache.disk.in_flight_bytes(), 0);
    assert_eq!(cache.disk.free_room(), 10 * MIB);
    let mut w3 = cache
        .disk
        .begin(k2, "http://example.com/b", meta, 3, Some(6 * MIB), u64::MAX)
        .unwrap()
        .expect("room is back");
    w3.write(&vec![b'x'; 6 * MIB as usize]).unwrap();
    let out = w3.finish(4).unwrap();
    assert!(out.stored);
    assert_eq!(cache.disk.in_flight_bytes(), 0);
    assert_eq!(cache.disk_usage().1, 1);
}

#[test]
fn truncated_disk_file_is_rejected_and_removed() {
    let cache = fresh("shp-test-truncated", 100, MIB);
    let key = cache_key("GET", "http://example.com/t");
    cache.put(
        key,
        "http://example.com/t",
        wire(b"0123456789"),
        Duration::from_secs(60),
        1,
    );
    let path = cache.disk_path(key).unwrap();
    cache.mem.remove(key);
    let len = fs::metadata(&path).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(len - 3)
        .unwrap();
    // L1 から外してディスクから読ませる → 長さ不一致で捨てる
    assert!(cache.get(key, 1).is_none());
    assert!(!path.exists());
    assert_eq!(cache.disk_usage().1, 0);
}

#[test]
fn disk_entry_cap_evicts_least_recently_used() {
    let mut cfg = CacheConfig::fixed(MIB, 10 * MIB, test_dir("shp-test-entry-cap"));
    cfg.disk_max_entries = 3;
    let cache = Cache::new(cfg);
    for i in 0..5 {
        let url = format!("http://example.com/{}", i);
        cache.put(
            cache_key("GET", &url),
            &url,
            b"x".to_vec(),
            Duration::from_secs(60),
            i,
        );
    }
    assert_eq!(cache.disk_usage().1, 3);
    assert!(
        cache
            .disk_path(cache_key("GET", "http://example.com/0"))
            .is_none()
    );
    assert!(
        cache
            .disk_path(cache_key("GET", "http://example.com/4"))
            .is_some()
    );
}
