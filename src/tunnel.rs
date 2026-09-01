use std::io::{self, Write};
use std::net::TcpStream;
use std::thread;

pub fn handle_connect(
    mut client: TcpStream,
    target: &str,
    conn_id: usize,
) -> io::Result<()> {
    let addr = if target.contains(':') {
        target.to_string()
    } else {
        format!("{}:443", target)
    };

    println!("[Conn #{}] CONNECT {}", conn_id, addr);

    let server = match TcpStream::connect(&addr) {
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
