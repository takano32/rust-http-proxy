use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::thread;
use std::time::Duration;

pub fn handle_connect(
    mut client: TcpStream,
    target: &str,
    timeout: Duration,
    conn_id: usize,
) -> io::Result<()> {
    let addr_str = if target.contains(':') {
        target.to_string()
    } else {
        format!("{}:443", target)
    };

    println!("[Conn #{}] CONNECT {}", conn_id, addr_str);

    let server = match connect_with_timeout(&addr_str, timeout) {
        Ok(s) => s,
        Err(e) => {
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
            return Err(e);
        }
    };

    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    client.flush()?;

    client.set_read_timeout(None)?;
    client.set_write_timeout(None)?;
    server.set_read_timeout(None)?;
    server.set_write_timeout(None)?;

    tunnel(client, server)
}

pub fn connect_with_timeout(addr_str: &str, timeout: Duration) -> io::Result<TcpStream> {
    let addrs: Vec<SocketAddr> = addr_str.to_socket_addrs()?.collect();
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Could not resolve host",
        ));
    }

    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::ConnectionRefused, "Failed to connect")
    }))
}

pub fn tunnel(mut client: TcpStream, mut server: TcpStream) -> io::Result<()> {
    let mut client_clone = client.try_clone()?;
    let mut server_clone = server.try_clone()?;

    let t1 = thread::spawn(move || {
        let _ = io::copy(&mut client, &mut server);
        let _ = server.shutdown(std::net::Shutdown::Write);
    });

    let t2 = thread::spawn(move || {
        let _ = io::copy(&mut server_clone, &mut client_clone);
        let _ = client_clone.shutdown(std::net::Shutdown::Write);
    });

    let _ = t1.join();
    let _ = t2.join();
    Ok(())
}
