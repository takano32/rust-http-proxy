//! `Cache` の結合テスト (budget)。

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
    let now = crate::cache::now_epoch() + 1;
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
        crate::cache::config::PTERODACTYL_UNKNOWN_QUOTA_DISK
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
    // $TMPDIR は tmpfs のことが多く先行確保が無効になるので、その場合は固定上限
    if cache.disk.reserve_active() {
        assert!(cache.disk_capacity() <= crate::cache::diskprobe::START);
        assert_eq!(cache.disk.entry_capacity(), 0, "nothing confirmed yet");
    } else {
        assert_eq!(
            cache.disk_capacity(),
            crate::cache::config::PTERODACTYL_UNKNOWN_QUOTA_DISK
        );
    }
    assert!(
        cache.snapshot().fs.is_none(),
        "host filesystem is not used as the denominator"
    );
    cache.probe_tick();
    assert!(cache.disk_capacity() <= crate::cache::diskprobe::START);

    // 割当が分かれば (0 = 無制限でも) 通常どおり
    let cfg = CacheConfig {
        disk_limit: Limit::Auto { percent: 100 },
        pterodactyl: true,
        disk_quota: DiskQuota::Unlimited,
        ..CacheConfig::fixed(MIB, 0, test_dir("shp-test-ptero-known"))
    };
    let cache = Cache::new(cfg);
    assert!(
        cache.snapshot().fs.is_some()
            || cache.disk_capacity() == crate::cache::config::FALLBACK_DISK
    );
}

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
            crate::cache::config::PTERODACTYL_UNKNOWN_QUOTA_DISK
        );
    }
}
