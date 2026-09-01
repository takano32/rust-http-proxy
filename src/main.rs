use std::io;
use std::net::TcpListener;
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use sorahost_http_proxy::cache::Cache;
use sorahost_http_proxy::config::Config;
use sorahost_http_proxy::handle_client;
use sorahost_http_proxy::log;
use sorahost_http_proxy::metrics::Metrics;
use sorahost_http_proxy::{log_debug, log_error, log_info};

static CONN_COUNTER: AtomicUsize = AtomicUsize::new(1);

const MIB: u64 = 1024 * 1024;

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

    log_info!(
        None,
        "sorahost-http-proxy listening on {} (log level: {})",
        config.bind_addr,
        log::current_level().as_str().trim()
    );
    log_info!(
        None,
        "cache: {} (memory {} MiB / disk {} MiB, dir {}, default TTL {}s, max object {} MiB)",
        if config.cache.enabled { "enabled" } else { "disabled" },
        config.cache.mem_capacity / MIB,
        config.cache.disk_capacity / MIB,
        config.cache.dir.display(),
        config.cache.default_ttl.as_secs(),
        config.cache.max_object_size / MIB
    );
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
