//! ベンチの相手役 (全部 `127.0.0.1` の使い捨てポート): 固定応答の keep-alive オリジン、
//! 送り続ける blaster、すぐ閉じる sink、握ったままにする holder。

use std::io::{self, BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

/// 固定応答を 1 回の `write_all` で返す keep-alive オリジン。
pub(crate) fn spawn_origin(body_bytes: usize, cacheable: bool) -> io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let body = vec![b'x'; body_bytes];
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Cache-Control: {}\r\n\r\n",
        body.len(),
        if cacheable { "max-age=60" } else { "no-store" }
    )
    .into_bytes();
    response.extend_from_slice(&body);
    let response = Arc::new(response);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let response = Arc::clone(&response);
            thread::spawn(move || {
                let _ = stream.set_nodelay(true);
                let _ = serve_origin(stream, &response);
            });
        }
    });
    Ok(addr)
}

/// 要求ヘッダーを読み飛ばし、1 要求ごとに固定応答を返す (本文付きの要求は来ない前提)。
fn serve_origin(mut stream: TcpStream, response: &[u8]) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    loop {
        let mut headers = 0usize;
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            headers += 1;
            if headers > 256 {
                return Ok(());
            }
        }
        stream.write_all(response)?;
    }
}

/// 接続されたら相手が閉じるまで送り続ける (トンネルのスループット用)。
///
/// 受ける側が `--seconds` 秒で切るので、この関数は量では止まらない。
pub(crate) fn spawn_blaster() -> io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let _ = stream.set_nodelay(true);
                let chunk = vec![b'y'; 1 << 20];
                let mut stream = stream;
                while stream.write_all(&chunk).is_ok() {
                    // 受ける側が切るまで送り続ける (量では止まらない)
                }
                let _ = stream.shutdown(Shutdown::Both);
            });
        }
    });
    Ok(addr)
}

/// 接続されたらすぐ閉じる (短命トンネルの相手)。
pub(crate) fn spawn_sink() -> io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    });
    Ok(addr)
}

/// 接続を受けて持ち続けるだけのリスナー (アイドルトンネルの相手)。
/// 読みも書きも閉じもしないので、トンネルは両方向とも暇なまま残る。
pub(crate) fn spawn_holder() -> io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    thread::spawn(move || {
        // 1 本 1 スレッドにすると 5,000 スレッドになるので、受けたら Vec に積むだけ
        let mut held: Vec<TcpStream> = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    Ok(addr)
}
