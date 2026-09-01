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
use sorahost_http_proxy::{log_debug, log_error, log_info};

static CONN_COUNTER: AtomicUsize = AtomicUsize::new(1);

fn main() {
    log::init_from_env();

    let config = match Config::from_env() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            log_error!(None, "configuration error: {}", e);
            process::exit(1);
        }
    };

    let listener = match TcpListener::bind(config.bind_addr) {
        Ok(l) => l,
        Err(e) => {
            log_error!(None, "failed to bind on {}: {}", config.bind_addr, e);
            process::exit(1);
        }
    };

    let metrics = Arc::new(Metrics::new());
    let cache = Arc::new(Cache::new(config.cache.clone()));
    let _probe = Cache::spawn_probe(&cache);

    log_info!(
        None,
        "sorahost-http-proxy listening on {} (log level: {})",
        config.bind_addr,
        log::current_level().as_str().trim()
    );
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
            c.disk_quota
                .map(|q| format!(
                    "{} MiB under {}",
                    q / MIB,
                    c.quota_root
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ))
                .unwrap_or_else(|| "not set".to_string())
        );
    }
    log_info!(None, "timeout: {}s", config.timeout.as_secs());
    if !config.acl.allow_hosts.is_empty() {
        log_info!(None, "allowed hosts: {:?}", config.acl.allow_hosts);
    }
    if !config.acl.deny_hosts.is_empty() {
        log_info!(None, "denied hosts: {:?}", config.acl.deny_hosts);
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let conn_id = CONN_COUNTER.fetch_add(1, Ordering::Relaxed);
                let cfg = Arc::clone(&config);
                let m = Arc::clone(&metrics);
                let c = Arc::clone(&cache);
                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, cfg, m, c, conn_id) {
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
