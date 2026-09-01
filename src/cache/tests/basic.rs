//! `Cache` の結合テスト (basic)。

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
    assert_eq!(cache.hits_mem.load(Ordering::Relaxed), 1);
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
    assert_eq!(cache.stores.load(Ordering::Relaxed), 0);
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
    assert!(json.contains("\"reserve\":false"), "{}", json);
    assert!(json.contains("\"quota_bytes\":null"), "{}", json);
    assert!(json.contains("\"revalidations\":0"), "{}", json);
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
