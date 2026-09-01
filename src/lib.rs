pub mod config;
pub mod http;
pub mod tunnel;

use std::io::{self, BufRead, BufReader};
use std::net::TcpStream;
use std::time::Duration;

pub fn handle_client(client: TcpStream, conn_id: usize) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    client.set_write_timeout(Some(Duration::from_secs(30)))?;

    let mut reader = BufReader::new(client.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }

    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }

    let method = parts[0];
    let target = parts[1];

    if method.eq_ignore_ascii_case("CONNECT") {
        tunnel::handle_connect(client, target, conn_id)?;
    } else {
        http::handle_http(client, request_line, reader, conn_id)?;
    }

    Ok(())
}
