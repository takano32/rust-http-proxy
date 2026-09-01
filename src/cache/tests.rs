//! `Cache` 全体 (両層 + 予算 + 再検証) の結合テスト。各層の単体テストはそれぞれのモジュールにある。

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::config::{CacheConfig, DiskQuota, Limit, MIB};
use super::key::cache_key;
use super::memory::{BALLAST_CHUNK, MemTier};
use super::{Body, Cache, CacheSource, Meta, format};

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

/// ワイヤ形式のレスポンスを組み立てる。
fn wire(body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"t\"\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
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
    assert_eq!(src, CacheSource::Memory);
    assert!(resp.is_fresh(super::now_epoch()));
    assert_eq!(resp.read_all().unwrap(), b"payload");
    assert_eq!(cache.hits_mem.load(Ordering::Relaxed), 1);
    assert!(
        cache.disk_path(key).unwrap().exists(),
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
    let now = super::now_epoch();
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
    assert_eq!(src, CacheSource::Disk);
    assert_eq!(resp.read_all().unwrap(), b"persisted");
}

#[test]
fn legacy_flat_layout_is_migrated_into_shards() {
    let dir = test_dir("shp-test-migrate");
    fs::create_dir_all(&dir).unwrap();
    let key = cache_key("GET", "http://example.com/legacy");
    let now = super::now_epoch();
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
fn sweep_removes_garbage_from_both_tiers() {
    let cache = fresh("shp-test-sweep", MIB, MIB);
    let plain = cache_key("GET", "http://example.com/plain");
    let keep = cache_key("GET", "http://example.com/keep");
    cache.put(
        plain,
        "http://example.com/plain",
        b"soon".to_vec(),
        Duration::from_secs(0),
        1,
    );
    cache.put_with(
        keep,
        "http://example.com/keep",
        wire(b"k"),
        Duration::from_secs(0),
        true,
        1,
    );
    assert_eq!(cache.mem_usage().1, 2);
    let now = super::now_epoch() + 1;
    // 再検証できない方だけ消える
    assert_eq!(cache.mem.sweep(now, 3600) + cache.disk.sweep(now, 3600), 2);
    assert_eq!(cache.mem_usage().1, 1);
    assert_eq!(cache.disk_usage().1, 1);
    assert!(cache.disk_path(plain).is_none());
    // max_stale を過ぎれば再検証できるものも消える
    assert_eq!(
        cache.mem.sweep(now + 10, 5) + cache.disk.sweep(now + 10, 5),
        2
    );
    assert_eq!(cache.disk_usage().1, 0);
    assert!(cache.disk_path(keep).is_none());
}

#[test]
fn memory_ballast_gives_way_to_entries_and_budget_cuts() {
    let tier = MemTier::new(true);
    tier.set_capacity(BALLAST_CHUNK as u64);
    assert_eq!(tier.fill_ballast(), BALLAST_CHUNK as u64);
    assert_eq!(tier.owned(), BALLAST_CHUNK as u64);

    // 1 MiB のエントリが入るとバラストは 1 ブロック解放される
    let meta = Meta {
        stored_at: 0,
        expires_at: u64::MAX,
        validators: false,
    };
    let data = Arc::new(vec![1u8; MIB as usize]);
    tier.insert(cache_key("GET", "http://a/"), data, meta, 1);
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
fn quota_mode_budgets_against_allocation_minus_others_and_margin() {
    let root = test_dir("shp-test-quota");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("server.jar"), vec![0u8; 10 * MIB as usize]).unwrap();
    let cfg = CacheConfig {
        disk_limit: Limit::Auto { percent: 100 },
        disk_quota: DiskQuota::Fixed(100 * MIB),
        quota_root: Some(root.clone()),
        pterodactyl: true,
        ..CacheConfig::fixed(MIB, 0, root.join("cache"))
    };
    let cache = Cache::new(cfg);
    // 100 MiB − 床 (1/10 = 10 MiB) − 他ファイル 10 MiB = 80 MiB
    assert_eq!(cache.disk_capacity(), 80 * MIB);
    let json = cache.to_json();
    assert!(json.contains("\"quota_bytes\":104857600"), "{}", json);
    assert!(json.contains("\"disk_total_bytes\":104857600"), "{}", json);
    assert!(json.contains("\"keep_free_bytes\":10485760"), "{}", json);
    assert!(json.contains("\"quota_mode\":\"fixed\""), "{}", json);
}

#[cfg(target_os = "linux")]
#[test]
fn auto_quota_uses_df_of_the_quota_root_unless_it_is_the_host_disk() {
    // 割当ディレクトリが / と同じファイルシステムなら「ホストディスク」→ 不明扱いで安全上限
    let same_fs = CacheConfig {
        disk_limit: Limit::Auto { percent: 100 },
        disk_quota: DiskQuota::Auto,
        quota_root: Some(PathBuf::from("/")),
        pterodactyl: true,
        ..CacheConfig::fixed(MIB, 0, test_dir("shp-test-quota-auto-host"))
    };
    let cache = Cache::new(same_fs);
    assert_eq!(cache.disk_quota(), DiskQuota::Unknown);
    assert_eq!(
        cache.disk_capacity(),
        super::config::PTERODACTYL_UNKNOWN_QUOTA_DISK
    );

    // 別のファイルシステム (このテストでは $TMPDIR が / と別のときだけ) なら df の total を割当にする
    let root = env::temp_dir();
    let (Some(df), Some(rootfs)) = (
        crate::sysinfo::fs_info(&root),
        crate::sysinfo::fs_info(std::path::Path::new("/")),
    ) else {
        return;
    };
    if df.total == rootfs.total {
        eprintln!("skipping: temp dir is on the root filesystem");
        return;
    }
    let cfg = CacheConfig {
        disk_limit: Limit::Auto { percent: 100 },
        disk_quota: DiskQuota::Auto,
        quota_root: Some(root),
        pterodactyl: true,
        ..CacheConfig::fixed(MIB, 0, test_dir("shp-test-quota-auto"))
    };
    let cache = Cache::new(cfg);
    assert_eq!(cache.disk_quota(), DiskQuota::Auto);
    let snap = cache.snapshot();
    assert_eq!(snap.fs.map(|f| f.total), Some(df.total));
    assert!(cache.disk_capacity() < df.total);
    assert!(cache.to_json().contains("\"quota_mode\":\"auto\""));
}

#[test]
fn pterodactyl_without_quota_is_capped_and_never_reserves() {
    // quota_root を / にすると「ホストディスク」と判定され、割当不明のまま安全上限になる
    let cfg = CacheConfig {
        disk_limit: Limit::Auto { percent: 100 },
        pterodactyl: true,
        reserve: true,
        quota_root: Some(PathBuf::from("/")),
        ..CacheConfig::fixed(MIB, 0, test_dir("shp-test-ptero-unknown"))
    };
    let cache = Cache::new(cfg);
    assert_eq!(
        cache.disk_capacity(),
        super::config::PTERODACTYL_UNKNOWN_QUOTA_DISK
    );
    assert!(
        !cache.disk.reserve_active(),
        "no disk ballast without a known quota"
    );
    assert!(
        cache.snapshot().fs.is_none(),
        "host filesystem is not used as the denominator"
    );
    cache.probe_tick();
    assert_eq!(
        cache.disk_capacity(),
        super::config::PTERODACTYL_UNKNOWN_QUOTA_DISK
    );
    assert_eq!(cache.disk_reserved(), 0);

    // 割当が分かれば (0 = 無制限でも) 通常どおり
    let cfg = CacheConfig {
        disk_limit: Limit::Auto { percent: 100 },
        pterodactyl: true,
        disk_quota: DiskQuota::Unlimited,
        ..CacheConfig::fixed(MIB, 0, test_dir("shp-test-ptero-known"))
    };
    let cache = Cache::new(cfg);
    assert!(cache.snapshot().fs.is_some() || cache.disk_capacity() == super::config::FALLBACK_DISK);
}

#[cfg(target_os = "linux")]
#[test]
fn auto_mode_measures_the_system() {
    let cfg = CacheConfig {
        mem_limit: Limit::Auto { percent: 100 },
        disk_limit: Limit::Auto { percent: 100 },
        ..CacheConfig::fixed(0, 0, test_dir("shp-test-auto"))
    };
    let cache = Cache::new(cfg);
    let snap = cache.snapshot();
    let mem = snap.mem.expect("memory snapshot on linux");
    assert!(mem.total > 0);
    assert!(snap.mem_keep_free > 0, "dynamic margin is never zero");
    assert!(cache.mem_capacity() <= mem.total - snap.mem_keep_free);
    let json = cache.to_json();
    assert!(json.contains("\"mode\":\"auto\""), "{}", json);
    assert!(json.contains("\"target_percent\":100"), "{}", json);
    // プローブを回しても壊れない
    cache.probe_tick();
    cache.probe_tick();
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

#[test]
fn invalidate_removes_every_variant_of_a_url() {
    let cache = fresh("shp-test-invalidate", MIB, MIB);
    let url = "http://example.com/item";
    let plain = cache_key("GET", url);
    let gz = super::cache_key_variant("GET", url, "gzip");
    cache.put(plain, url, b"plain".to_vec(), Duration::from_secs(60), 1);
    cache.put(gz, url, b"gzip".to_vec(), Duration::from_secs(60), 1);
    assert!(cache.get(plain, 1).is_some() && cache.get(gz, 1).is_some());
    assert_eq!(cache.invalidate(url, 1), 2);
    assert!(cache.get(plain, 1).is_none() && cache.get(gz, 1).is_none());
    assert_eq!(cache.disk_usage().1, 0);
}

#[cfg(target_os = "linux")]
#[test]
fn unknown_quota_is_inferred_from_df_only_when_smaller_than_root() {
    let root = env::temp_dir();
    let (Some(df), Some(rootfs)) = (
        crate::sysinfo::fs_info(&root),
        crate::sysinfo::fs_info(std::path::Path::new("/")),
    ) else {
        return;
    };
    let cfg = CacheConfig {
        disk_limit: Limit::Auto { percent: 100 },
        quota_root: Some(root),
        pterodactyl: true,
        ..CacheConfig::fixed(MIB, 0, test_dir("shp-test-quota-infer"))
    };
    let cache = Cache::new(cfg);
    if df.total != rootfs.total && df.total < rootfs.total {
        assert_eq!(
            cache.disk_quota(),
            DiskQuota::Auto,
            "smaller separate fs is taken as the allocation"
        );
    } else {
        assert_eq!(cache.disk_quota(), DiskQuota::Unknown);
        assert_eq!(
            cache.disk_capacity(),
            super::config::PTERODACTYL_UNKNOWN_QUOTA_DISK
        );
    }
}
