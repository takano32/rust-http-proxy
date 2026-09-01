use std::io;
use std::net::TcpListener;
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use sorahost_http_proxy::config::Config;
use sorahost_http_proxy::handle_client;
use sorahost_http_proxy::metrics::Metrics;

static CONN_COUNTER: AtomicUsize = AtomicUsize::new(1);

fn main() {
    let config = match Config::from_env() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("Configuration error: {}", e);
            process::exit(1);
        }
    };

    let listener = match TcpListener::bind(config.bind_addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind on {}: {}", config.bind_addr, e);
            process::exit(1);
        }
    };

    let metrics = Arc::new(Metrics::new());

    println!("HTTP/HTTPS Proxy listening on {}", config.bind_addr);
    if config.auth.is_enabled() {
        println!("Proxy authentication is ENABLED");
    }
    if !config.acl.allow_hosts.is_empty() {
        println!("Allowed hosts: {:?}", config.acl.allow_hosts);
    }
    if !config.acl.deny_hosts.is_empty() {
        println!("Denied hosts: {:?}", config.acl.deny_hosts);
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let conn_id = CONN_COUNTER.fetch_add(1, Ordering::Relaxed);
                let cfg = Arc::clone(&config);
                let m = Arc::clone(&metrics);
                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, cfg, m, conn_id) {
                        if e.kind() != io::ErrorKind::UnexpectedEof
                            && e.kind() != io::ErrorKind::ConnectionReset
                            && e.kind() != io::ErrorKind::BrokenPipe
                        {
                            eprintln!("[Conn #{}] Error: {}", conn_id, e);
                        }
                    }
                });
            }
            Err(e) => {
                eprintln!("Accept failed: {}", e);
            }
        }
    }
}
