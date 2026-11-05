//! `reload` の判断の単体試験 (T34.58)。`Config` を 2 つ作って比べるだけで、`.env` も inotify も触らない。

use super::merge::{merge_live, restart_required};
use crate::config::Config;
use std::path::PathBuf;
use std::time::Duration;

fn base() -> Config {
    Config::new("8080", None, None, Duration::from_secs(5)).unwrap()
}

/// 当てた項目の名前。ログの水準 (`PROXY_LOG_LEVEL`) だけは `merge_live` が `.env` とプロセスの
/// 水準から決める (手元の `.env` で変わる) ので除いて比べる。
fn names(applied: &[&str]) -> Vec<String> {
    applied
        .iter()
        .filter(|n| **n != "PROXY_LOG_LEVEL")
        .map(|n| n.to_string())
        .collect()
}

#[test]
fn nothing_changed_needs_no_restart() {
    assert!(restart_required(&base(), &base()).is_empty());
}

#[test]
fn each_boot_time_key_is_named_on_its_own() {
    type Change = fn(&mut Config);
    let cases: Vec<(&str, Change)> = vec![
        ("SERVER_PORT", |c| c.port = 9090),
        ("PROXY_BIND/PROXY_IPV6", |c| {
            c.bind_addrs = vec!["127.0.0.1".parse().unwrap()]
        }),
        ("PROXY_BIND/PROXY_IPV6", |c| c.ipv6 = !c.ipv6),
        ("PROXY_HE_STAGGER_MS", |c| {
            c.he_stagger += Duration::from_millis(1)
        }),
        ("PROXY_LISTEN_BACKLOG", |c| c.listen_backlog += 1),
        ("PROXY_TLS*", |c| c.tls_enabled = !c.tls_enabled),
        ("PROXY_TLS*", |c| c.tls_verify = !c.tls_verify),
        ("PROXY_TLS*", |c| c.tls_ca_file = Some(PathBuf::from("/x"))),
        ("PROXY_ORIGIN_POOL", |c| c.pool_per_host += 1),
        ("PROXY_STATS_PERSIST", |c| {
            c.stats_persist = !c.stats_persist
        }),
        (
            "cache settings (SERVER_MEMORY/SERVER_DISK/PROXY_CACHE_*)",
            |c| c.cache.enabled = !c.cache.enabled,
        ),
    ];
    for (want, change) in cases {
        let mut fresh = base();
        change(&mut fresh);
        assert_eq!(restart_required(&base(), &fresh), vec![want], "{want}");
    }
}

#[test]
fn several_boot_time_keys_come_in_a_fixed_order() {
    let mut fresh = base();
    fresh.cache.enabled = !fresh.cache.enabled;
    fresh.tls_enabled = !fresh.tls_enabled;
    fresh.port = 9090;
    fresh.ipv6 = !fresh.ipv6;
    fresh.bind_addrs = vec!["127.0.0.1".parse().unwrap()];
    assert_eq!(
        restart_required(&base(), &fresh),
        vec![
            "SERVER_PORT",
            "PROXY_BIND/PROXY_IPV6",
            "PROXY_TLS*",
            "cache settings (SERVER_MEMORY/SERVER_DISK/PROXY_CACHE_*)",
        ],
        "BIND と IPV6 が両方変わっても 1 行"
    );
}

#[test]
fn live_keys_are_not_a_reason_to_restart() {
    let mut fresh = base();
    fresh.timeout = Duration::from_secs(9);
    fresh.keepalive += Duration::from_secs(1);
    fresh.endpoints_readonly = !fresh.endpoints_readonly;
    assert!(restart_required(&base(), &fresh).is_empty());
}

#[test]
fn merge_live_applies_the_timeouts_and_keeps_the_rest() {
    let old = base();
    let mut fresh = base();
    fresh.timeout = Duration::from_secs(9);
    fresh.keepalive += Duration::from_secs(1);
    // 起動時に固定される項目は当てない (読み直した値が違っても前のまま)
    fresh.port = 9090;
    let (next, applied) = merge_live(&old, &fresh);
    assert_eq!(
        names(&applied),
        ["PROXY_TIMEOUT_SECS", "PROXY_KEEPALIVE_SECS"]
    );
    assert_eq!(next.timeout, Duration::from_secs(9));
    assert_eq!(next.keepalive, fresh.keepalive);
    assert_eq!(next.port, 8080);
}

#[test]
fn merge_live_applies_the_access_lists_in_order() {
    let old = base();
    let mut fresh = Config::new(
        "8080",
        Some("Example.com"),
        Some("bad.com"),
        Duration::from_secs(5),
    )
    .unwrap();
    fresh.endpoints_readonly = !old.endpoints_readonly;
    fresh.max_conns_per_client = old.max_conns_per_client + 3;
    fresh.connect_ports = crate::acl::PortSet::parse("443");
    let (next, applied) = merge_live(&old, &fresh);
    assert_eq!(
        names(&applied),
        [
            "PROXY_ALLOW_HOSTS/PROXY_DENY_HOSTS",
            "PROXY_CONNECT_PORTS",
            "PROXY_ENDPOINTS_READONLY",
            "PROXY_MAX_CONNS_PER_CLIENT",
        ]
    );
    assert_eq!(next.acl, fresh.acl);
    assert!(next.connect_ports.allows(443) && !next.connect_ports.allows(80));
    assert_eq!(next.max_conns_per_client, fresh.max_conns_per_client);
}

#[test]
fn merge_live_with_nothing_changed_applies_nothing() {
    let (next, applied) = merge_live(&base(), &base());
    assert!(names(&applied).is_empty());
    assert_eq!(next.timeout, base().timeout);
}

/// **T34.153**: `/events` の `reload` の前後の値は `--check` と同じ外し方 (空の文字列は `""` のまま。
/// 前はここだけ外していて `PROXY_ALLOW_CLIENTS  → 127.0.0.1` と空白が 2 つ並んだ)。
#[test]
fn changed_values_unquote_like_the_check_listing() {
    let old = base();
    let mut fresh = base();
    fresh.allow_clients = crate::acl::ClientAcl::parse("127.0.0.1");
    fresh.timeout = Duration::from_secs(9);
    let changed = [
        "PROXY_ALLOW_CLIENTS".to_string(),
        "PROXY_TIMEOUT_SECS".to_string(),
        "PROXY_NO_SUCH_KEY_T1415".to_string(),
    ];
    assert_eq!(
        super::changed_values(&changed, &old, &fresh),
        "PROXY_ALLOW_CLIENTS \"\" \u{2192} 127.0.0.1, PROXY_TIMEOUT_SECS 5 \u{2192} 9, PROXY_NO_SUCH_KEY_T1415"
    );
}
