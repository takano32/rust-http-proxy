use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::log::{Access, access};
use crate::metrics::{HostOutcome, Metrics};
use crate::net;
use crate::{log_debug, log_trace, log_warn};

/// `prefix` はリクエストヘッダーの直後に既に読み込んでしまったバイト列 (先にサーバーへ渡す)。
pub fn handle_connect(
    mut client: TcpStream,
    target: &str,
    prefix: &[u8],
    timeout: Duration,
    conn_id: usize,
    metrics: Arc<Metrics>,
) -> io::Result<()> {
    let started = Instant::now();
    let client_ip = client
        .peer_addr()
        .map(|a| net::canonical_ip(a.ip()).to_string())
        .unwrap_or_else(|_| "-".to_string());
    let addr_str = net::with_default_port(target, 443);

    log_debug!(Some(conn_id), "start CONNECT {}", addr_str);

    let mut server = match connect_with_timeout(&addr_str, timeout) {
        Ok(s) => s,
        Err(e) => {
            log_warn!(
                Some(conn_id),
                "502 Bad Gateway: connect {} failed: {}",
                addr_str,
                e
            );
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
            metrics.record_host(&format!("connect://{}", addr_str), HostOutcome::Error, 0);
            access(
                conn_id,
                &Access {
                    client: &client_ip,
                    method: "CONNECT",
                    target: &addr_str,
                    version: "HTTP/1.1",
                    status: "502",
                    bytes: 0,
                    duration_ms: started.elapsed().as_secs_f64() * 1000.0,
                    cache: "BYPASS",
                },
            );
            return Err(e);
        }
    };

    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    client.flush()?;
    if !prefix.is_empty() {
        server.write_all(prefix)?;
    }

    client.set_read_timeout(None)?;
    client.set_write_timeout(None)?;
    server.set_read_timeout(None)?;
    server.set_write_timeout(None)?;

    let transferred = tunnel(client, server)?;
    metrics.add_bytes(transferred);
    metrics.record_host(
        &format!("connect://{}", addr_str),
        HostOutcome::Bypass,
        transferred,
    );
    access(
        conn_id,
        &Access {
            client: &client_ip,
            method: "CONNECT",
            target: &addr_str,
            version: "HTTP/1.1",
            status: "200",
            bytes: transferred,
            duration_ms: started.elapsed().as_secs_f64() * 1000.0,
            cache: "BYPASS(tunnel)",
        },
    );
    Ok(())
}

/// 名前解決して接続する (IPv6 / IPv4 を Happy Eyeballs で並行に試す)。
pub fn connect_with_timeout(addr_str: &str, timeout: Duration) -> io::Result<TcpStream> {
    net::connect(addr_str, timeout)
}

/// 双方向にデータを中継し、転送した合計バイト数を返す。
pub fn tunnel(mut client: TcpStream, mut server: TcpStream) -> io::Result<u64> {
    let mut client_clone = client.try_clone()?;
    let mut server_clone = server.try_clone()?;
    let total = Arc::new(AtomicU64::new(0));

    let up = Arc::clone(&total);
    let t1 = thread::spawn(move || {
        let n = io::copy(&mut client, &mut server).unwrap_or(0);
        up.fetch_add(n, Ordering::Relaxed);
        let _ = server.shutdown(std::net::Shutdown::Write);
        n
    });

    let down = Arc::clone(&total);
    let t2 = thread::spawn(move || {
        let n = io::copy(&mut server_clone, &mut client_clone).unwrap_or(0);
        down.fetch_add(n, Ordering::Relaxed);
        let _ = client_clone.shutdown(std::net::Shutdown::Write);
        n
    });

    let sent = t1.join().unwrap_or(0);
    let received = t2.join().unwrap_or(0);
    log_trace!(None, "tunnel finished: {}B up / {}B down", sent, received);
    Ok(total.load(Ordering::Relaxed))
}
