//! オリジンへの接続プール (keep-alive の再利用)。
//!
//! 上流 (`scheme://host:port`) ごとにアイドル接続を保持し、再利用前に生存確認をする。
//! 最後に返された接続から使う (LIFO) ので、古い接続は自然に期限切れで捨てられる。

use std::collections::{HashMap, VecDeque};
use std::io::BufReader;
use std::net::TcpStream;

use crate::origin::OriginStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Idle {
    stream: BufReader<OriginStream>,
    since: Instant,
}

pub struct Pool {
    idle: Mutex<HashMap<String, VecDeque<Idle>>>,
    max_per_host: usize,
    idle_timeout: Duration,
}

impl Pool {
    pub fn new(max_per_host: usize, idle_timeout: Duration) -> Self {
        Self {
            idle: Mutex::new(HashMap::new()),
            max_per_host,
            idle_timeout,
        }
    }

    pub fn enabled(&self) -> bool {
        self.max_per_host > 0 && !self.idle_timeout.is_zero()
    }

    /// 使えるアイドル接続があれば取り出す。
    pub fn get(&self, host: &str) -> Option<BufReader<OriginStream>> {
        if !self.enabled() {
            return None;
        }
        loop {
            let candidate = {
                let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
                let queue = idle.get_mut(host)?;
                let c = queue.pop_back();
                if queue.is_empty() {
                    idle.remove(host);
                }
                c?
            };
            if candidate.since.elapsed() < self.idle_timeout
                && is_alive(candidate.stream.get_ref().tcp())
            {
                return Some(candidate.stream);
            }
        }
    }

    /// 応答を読み切った接続を戻す。読み残しがあるものは捨てる。
    pub fn put(&self, host: &str, stream: BufReader<OriginStream>) {
        if !self.enabled() || !stream.buffer().is_empty() {
            return;
        }
        let now = Instant::now();
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        let queue = idle.entry(host.to_string()).or_default();
        queue.retain(|i| now.duration_since(i.since) < self.idle_timeout);
        while queue.len() >= self.max_per_host {
            queue.pop_front();
        }
        queue.push_back(Idle { stream, since: now });
    }

    /// 期限切れを捨てる (定期的に呼ぶ)。
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        idle.retain(|_, q| {
            q.retain(|i| now.duration_since(i.since) < self.idle_timeout);
            !q.is_empty()
        });
    }

    pub fn idle_count(&self) -> usize {
        let idle = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        idle.values().map(|q| q.len()).sum()
    }
}

/// 相手が閉じていないか、読み残しが無いかを非ブロッキングの peek で確かめる。
fn is_alive(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let mut byte = [0u8; 1];
    let alive = match stream.peek(&mut byte) {
        Ok(0) => false,
        Ok(_) => false,
        Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
    };
    alive && stream.set_nonblocking(false).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn reuses_live_connections_and_drops_dead_ones() {
        let pool = Pool::new(2, Duration::from_secs(5));
        let (c1, s1) = pair();
        pool.put("h", BufReader::new(OriginStream::Plain(c1)));
        assert_eq!(pool.idle_count(), 1);
        assert!(pool.get("h").is_some());
        assert!(pool.get("h").is_none());
        drop(s1);

        let (c2, s2) = pair();
        pool.put("h", BufReader::new(OriginStream::Plain(c2)));
        drop(s2); // 相手が閉じた
        std::thread::sleep(Duration::from_millis(50));
        assert!(pool.get("h").is_none(), "dead connection is discarded");
    }

    #[test]
    fn stray_bytes_make_a_connection_unusable() {
        let pool = Pool::new(2, Duration::from_secs(5));
        let (c, mut s) = pair();
        pool.put("h", BufReader::new(OriginStream::Plain(c)));
        s.write_all(b"junk").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(pool.get("h").is_none());
    }

    #[test]
    fn respects_limits_and_expiry() {
        let pool = Pool::new(1, Duration::from_millis(30));
        let (c1, _s1) = pair();
        let (c2, _s2) = pair();
        pool.put("h", BufReader::new(OriginStream::Plain(c1)));
        pool.put("h", BufReader::new(OriginStream::Plain(c2)));
        assert_eq!(pool.idle_count(), 1, "max per host");
        std::thread::sleep(Duration::from_millis(60));
        pool.sweep();
        assert_eq!(pool.idle_count(), 0);
        let disabled = Pool::new(0, Duration::from_secs(1));
        let (c3, _s3) = pair();
        disabled.put("h", BufReader::new(OriginStream::Plain(c3)));
        assert!(!disabled.enabled() && disabled.get("h").is_none());
    }
}
