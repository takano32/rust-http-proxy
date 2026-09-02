use std::io;
use std::net::TcpListener;
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use sorahost_http_proxy::cache::{Cache, MIB};
use sorahost_http_proxy::config::Config;
use sorahost_http_proxy::handle_client;
use sorahost_http_proxy::log;
use sorahost_http_proxy::metrics::Metrics;
use sorahost_http_proxy::net;
use sorahost_http_proxy::pool::Pool;
use sorahost_http_proxy::reload;
use sorahost_http_proxy::signal;
use sorahost_http_proxy::tls::TlsClient;
use sorahost_http_proxy::{Upstream, log_warn};
use sorahost_http_proxy::{log_debug, log_error, log_info};

static CONN_COUNTER: AtomicUsize = AtomicUsize::new(1);

/// オリジンへのアイドル接続を保持する時間。
const ORIGIN_IDLE: std::time::Duration = std::time::Duration::from_secs(30);

fn main() {
    log::init_from_env();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            log_error!(None, "configuration error: {}", e);
            process::exit(1);
        }
    };

    net::set_ipv6_enabled(config.ipv6);
    sorahost_http_proxy::dns::set_ttl(config.dns_ttl);
    let listeners = match net::bind_all(&config.bind_addrs, config.port) {
        Ok(l) => l,
        Err(e) => {
            log_error!(None, "failed to bind port {}: {}", config.port, e);
            process::exit(1);
        }
    };

    let live = reload::Live::new(config);
    let config = live.config();
    let _reload = reload::spawn(Arc::clone(&live));
    let metrics = Arc::new(Metrics::new());
    let cache = Arc::new(Cache::new(config.cache.clone()));
    let _probe = Cache::spawn_probe(&cache);
    let _history = sorahost_http_proxy::history::spawn(Arc::clone(&metrics), Arc::clone(&cache));
    let tls = if !config.tls_enabled {
        log_info!(
            None,
            "TLS: disabled (PROXY_TLS=off); https:// origins are unavailable"
        );
        None
    } else {
        match TlsClient::load(config.tls_verify, config.tls_ca_file.as_deref()) {
            Ok(Some(t)) => {
                log_info!(
                    None,
                    "TLS: {} (certificate verification {}{})",
                    t.version(),
                    if t.verifies() { "on" } else { "OFF" },
                    config
                        .tls_ca_file
                        .as_ref()
                        .map(|p| format!(", CA file {}", p.display()))
                        .unwrap_or_default()
                );
                Some(t)
            }
            Ok(None) => {
                log_warn!(
                    None,
                    "TLS: libssl not found; https:// origins are unavailable"
                );
                None
            }
            Err(e) => {
                log_error!(None, "TLS setup failed: {}", e);
                process::exit(1);
            }
        }
    };
    let pool = Arc::new(Upstream {
        pool: Pool::new(config.pool_per_host, ORIGIN_IDLE),
        tls,
    });
    if config.cache.enabled && config.cache.reserve {
        // 停止シグナルで ballast.reserve を空にしてから終わる (Wings のディスク計測に残さない)
        signal::install(&cache.ballast_path());
    }

    log_info!(
        None,
        "sorahost-http-proxy listening on {} (log level: {})",
        listeners
            .iter()
            .map(net::describe_listener)
            .collect::<Vec<_>>()
            .join(", "),
        log::current_level().as_str().trim()
    );
    if let Some(path) = sorahost_http_proxy::envfile::loaded_path() {
        log_info!(
            None,
            "settings file {} loaded ({} variables; file values override the real environment)",
            path.display(),
            sorahost_http_proxy::envfile::loaded_count()
        );
    }
    let c = &config.cache;
    log_info!(
        None,
        "cache: {} (memory {}, disk {}, reserve {}, probe every {}s, dir {}, default TTL {}s, max object {} MiB)",
        if c.enabled { "enabled" } else { "disabled" },
        c.mem_limit,
        c.disk_limit,
        if c.reserve { "on" } else { "off" },
        c.probe_interval.as_secs(),
        c.dir.display(),
        c.default_ttl.as_secs(),
        c.max_object_size / MIB
    );
    if c.pterodactyl {
        log_info!(
            None,
            "Pterodactyl detected (memory allocation {}, disk quota {})",
            c.mem_alloc
                .map(|m| format!("{} MiB", m / MIB))
                .unwrap_or_else(|| "unknown".to_string()),
            format!(
                "{} under {}",
                cache.disk_quota(),
                c.quota_root
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            )
        );
    }
    log_info!(
        None,
        "timeout: {}s, keep-alive: {}s, origin pool: {} per host, DNS cache: {}s, IPv6: {}",
        config.timeout.as_secs(),
        config.keepalive.as_secs(),
        config.pool_per_host,
        config.dns_ttl.as_secs(),
        if config.ipv6 {
            "on"
        } else {
            "off (PROXY_IPV6=on to enable)"
        }
    );
    if !config.acl.allow_hosts.is_empty() {
        log_info!(None, "allowed hosts: {:?}", config.acl.allow_hosts);
    }
    if !config.acl.deny_hosts.is_empty() {
        log_info!(None, "denied hosts: {:?}", config.acl.deny_hosts);
    }

    // 待ち受けソケットごとに accept スレッドを持つ (最後の 1 つはこのスレッドで回す)
    let mut listeners = listeners.into_iter();
    let last = listeners.next_back().expect("at least one listener");
    for listener in listeners {
        let shared = (
            Arc::clone(&live),
            Arc::clone(&metrics),
            Arc::clone(&cache),
            Arc::clone(&pool),
        );
        thread::spawn(move || serve(listener, shared.0, shared.1, shared.2, shared.3));
    }
    drop(config);
    serve(last, live, metrics, cache, pool);
}

fn serve(
    listener: TcpListener,
    live: Arc<reload::Live>,
    metrics: Arc<Metrics>,
    cache: Arc<Cache>,
    pool: Arc<Upstream>,
) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let conn_id = CONN_COUNTER.fetch_add(1, Ordering::Relaxed);
                let cfg = live.config();
                let m = Arc::clone(&metrics);
                let c = Arc::clone(&cache);
                let p = Arc::clone(&pool);
                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, cfg, m, c, p, conn_id) {
                        if e.kind() != io::ErrorKind::UnexpectedEof
                            && e.kind() != io::ErrorKind::ConnectionReset
                            && e.kind() != io::ErrorKind::BrokenPipe
                        {
                            log_error!(Some(conn_id), "{}", e);
                        } else {
                            log_debug!(Some(conn_id), "connection ended: {}", e);
                        }
                    }
                });
            }
            Err(e) => {
                log_error!(None, "accept failed: {}", e);
            }
        }
    }
}
