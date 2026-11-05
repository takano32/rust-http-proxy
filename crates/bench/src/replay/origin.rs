//! 再生の相手 (内蔵オリジン) と、トンネルの先頭に流す合図の形。

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

/// 再生の合図の大きさ (magic 4 + 下りのバイト 8 + 応答遅延の ms 4)。
pub(super) const PREAMBLE: u64 = 16;
const MAGIC: &[u8; 4] = b"RPLY";
/// 1 本で流す下りの上限 (壊れた個票で何 GiB も流さないため)。
pub(super) const MAX_DOWN: u64 = 1 << 30;

/// 合図を組む: magic、下りのバイト数 (上限 [`MAX_DOWN`]、LE 8 バイト)、応答遅延の ms (LE 4 バイト)。
pub(super) fn encode_preamble(down: u64, delay_ms: u32) -> [u8; PREAMBLE as usize] {
    let mut head = [0u8; PREAMBLE as usize];
    head[..4].copy_from_slice(MAGIC);
    head[4..12].copy_from_slice(&down.min(MAX_DOWN).to_le_bytes());
    head[12..16].copy_from_slice(&delay_ms.to_le_bytes());
    head
}

/// 合図を読む (下りのバイト数, 応答遅延の ms)。magic が違えば `None`。
pub(super) fn decode_preamble(head: &[u8; PREAMBLE as usize]) -> Option<(u64, u32)> {
    if &head[..4] != MAGIC {
        return None;
    }
    let down = u64::from_le_bytes(head[4..12].try_into().expect("8 bytes")).min(MAX_DOWN);
    let delay_ms = u32::from_le_bytes(head[12..16].try_into().expect("4 bytes"));
    Some((down, delay_ms))
}

/// 再生の相手。トンネルの**先頭 16 バイト**を合図として読み、`delay_ms` 待ってから
/// `down` バイトを送り、あとは相手が閉じるまで読み捨てる。
///
/// `delay_ms` が `/hosts` の `rtt_ms` (T14.5) で、**内蔵オリジンの応答遅延**として
/// デプロイ先の往復を真似る。
pub(super) fn spawn_replay_origin() -> io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let _ = stream.set_nodelay(true);
                let _ = serve_replay(stream);
            });
        }
    });
    Ok(addr)
}

fn serve_replay(mut sock: TcpStream) -> io::Result<()> {
    let mut head = [0u8; PREAMBLE as usize];
    sock.read_exact(&mut head)?;
    let Some((down, delay_ms)) = decode_preamble(&head) else {
        return Ok(());
    };
    if delay_ms > 0 {
        thread::sleep(Duration::from_millis(delay_ms as u64));
    }
    if down > 0 {
        let chunk = vec![b'z'; (down.min(64 * 1024)) as usize];
        let mut left = down;
        while left > 0 {
            let want = left.min(chunk.len() as u64) as usize;
            sock.write_all(&chunk[..want])?;
            left -= want as u64;
        }
    }
    // 上りを読み捨てる (相手が閉じるまで)
    let mut buf = [0u8; 16 * 1024];
    while sock.read(&mut buf)? > 0 {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合図は 16 バイトで往復し、下りは上限で切られ、magic が違えば読まない。
    #[test]
    fn the_preamble_round_trips_and_caps_the_download() {
        let head = encode_preamble(2_000, 250);
        assert_eq!(&head[..4], b"RPLY");
        assert_eq!(decode_preamble(&head), Some((2_000, 250)));
        assert_eq!(decode_preamble(&encode_preamble(0, 0)), Some((0, 0)));
        // 壊れた個票の何 GiB もの下りは 1 GiB で止める (書く側でも読む側でも)
        assert_eq!(
            decode_preamble(&encode_preamble(u64::MAX, 1)),
            Some((MAX_DOWN, 1))
        );
        let mut raw = encode_preamble(5, 0);
        raw[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(decode_preamble(&raw), Some((MAX_DOWN, 0)));
        // magic が違えば合図ではない
        let mut other = encode_preamble(5, 0);
        other[0] = b'X';
        assert_eq!(decode_preamble(&other), None);
    }
}
