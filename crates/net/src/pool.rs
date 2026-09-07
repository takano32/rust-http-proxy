//! オリジンへの接続プール (keep-alive の再利用)。
//!
//! 上流 (`scheme://host:port`) ごとにアイドル接続を保持し、再利用前に生存確認をする。
//! 最後に返された接続から使う (LIFO) ので、古い接続は自然に期限切れで捨てられる。

use crate::sync::LockExt;
use std::collections::{HashMap, VecDeque};
use std::io::BufReader;
use std::net::TcpStream;

use crate::origin::OriginStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct Idle {
    stream: BufReader<OriginStream>,
    since: Instant,
    /// このソケットに今設定してあるタイムアウト (同じなら setsockopt を省く)
    timeout: Duration,
}

pub struct Pool {
    idle: Mutex<HashMap<String, VecDeque<Idle>>>,
    max_per_host: usize,
    /// 全ホスト合計のアイドル接続の上限 (ホスト数 × max_per_host に歯止めを掛ける)。
    /// 1 本あたり読み取りバッファを抱えるので、多数のホストへ行くと青天井になるため
    max_total: usize,
    /// 今持っているアイドル接続の数 (上限判定を O(1) にするため別に数える)
    total: AtomicUsize,
    idle_timeout: Duration,
}

impl Pool {
    pub fn new(max_per_host: usize, idle_timeout: Duration) -> Self {
        Self::with_total(
            max_per_host,
            max_per_host.saturating_mul(32).max(64),
            idle_timeout,
        )
    }

    pub fn with_total(max_per_host: usize, max_total: usize, idle_timeout: Duration) -> Self {
        Self {
            idle: Mutex::new(HashMap::new()),
            max_per_host,
            max_total,
            total: AtomicUsize::new(0),
            idle_timeout,
        }
    }

    pub fn enabled(&self) -> bool {
        self.max_per_host > 0 && !self.idle_timeout.is_zero()
    }

    /// 使えるアイドル接続があれば取り出す。`want` は使いたいタイムアウトで、
    /// 前回と同じなら `setsockopt` を呼ばない (要求ごとの 2 回を消す)。
    pub fn get(&self, host: &str, want: Duration) -> Option<BufReader<OriginStream>> {
        if !self.enabled() {
            return None;
        }
        loop {
            let candidate = {
                let mut idle = self.idle.locked();
                let queue = idle.get_mut(host)?;
                let c = queue.pop_back();
                if c.is_some() {
                    self.total.fetch_sub(1, Ordering::Relaxed);
                }
                if queue.is_empty() {
                    idle.remove(host);
                }
                c?
            };
            if candidate.since.elapsed() < self.idle_timeout
                && is_alive(candidate.stream.get_ref().tcp())
            {
                if candidate.timeout != want {
                    candidate.stream.get_ref().set_timeouts(want).ok()?;
                }
                return Some(candidate.stream);
            }
        }
    }

    /// 応答を読み切った接続を戻す。読み残しがあるものは捨てる。
    /// `timeout` は今このソケットに設定してある値。
    pub fn put(&self, host: &str, stream: BufReader<OriginStream>, timeout: Duration) {
        if !self.enabled() || !stream.buffer().is_empty() {
            return;
        }
        // 全体の上限に達していたら、この接続はプールに入れずに閉じる
        if self.total.load(Ordering::Relaxed) >= self.max_total {
            return;
        }
        let now = Instant::now();
        let mut idle = self.idle.locked();
        // ロックを持つ時間を短くする: 既にある行はキーを作り直さず、期限切れの掃除は
        // sweep() に任せて上限だけ見る (このロックは全接続スレッドが 1 要求に 2 回取る)
        let queue = match idle.get_mut(host) {
            Some(q) => q,
            None => idle.entry(host.to_string()).or_default(),
        };
        let mut dropped = 0usize;
        while queue.len() >= self.max_per_host {
            if queue.pop_front().is_some() {
                dropped += 1;
            }
        }
        queue.push_back(Idle {
            stream,
            since: now,
            timeout,
        });
        drop(idle);
        self.total.fetch_add(1, Ordering::Relaxed);
        if dropped > 0 {
            self.total.fetch_sub(dropped, Ordering::Relaxed);
        }
    }

    /// 期限切れを捨てる (`env-reload` スレッドが 30 秒ごとに呼ぶ)。戻り値は捨てた本数。
    pub fn sweep(&self) -> usize {
        let now = Instant::now();
        let mut idle = self.idle.locked();
        let before: usize = idle.values().map(|q| q.len()).sum();
        idle.retain(|_, q| {
            q.retain(|i| now.duration_since(i.since) < self.idle_timeout);
            !q.is_empty()
        });
        let after: usize = idle.values().map(|q| q.len()).sum();
        drop(idle);
        let removed = before.saturating_sub(after);
        if removed > 0 {
            self.total.fetch_sub(removed, Ordering::Relaxed);
        }
        removed
    }

    pub fn idle_count(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }
}

/// 相手が閉じていないか、読み残しが無いかを確かめる。
///
/// Linux では `recv(MSG_PEEK | MSG_DONTWAIT)` の 1 回で済ませる
/// (`set_nonblocking` → `peek` → `set_nonblocking` は 3 システムコールだった)。
#[cfg(target_os = "linux")]
fn is_alive(stream: &TcpStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut byte = [0u8; 1];
    match crate::sys::peek(stream.as_raw_fd(), &mut byte) {
        // 読めるものがある = 前の応答の読み残し、0 = 相手が閉じた。どちらも使えない
        Ok(_) => false,
        Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
    }
}

#[cfg(not(target_os = "linux"))]
fn is_alive(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let mut byte = [0u8; 1];
    let alive = match stream.peek(&mut byte) {
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
        pool.put(
            "h",
            BufReader::new(OriginStream::Plain(c1)),
            Duration::from_secs(5),
        );
        assert_eq!(pool.idle_count(), 1);
        assert!(pool.get("h", Duration::from_secs(5)).is_some());
        assert!(pool.get("h", Duration::from_secs(5)).is_none());
        drop(s1);

        let (c2, s2) = pair();
        pool.put(
            "h",
            BufReader::new(OriginStream::Plain(c2)),
            Duration::from_secs(5),
        );
        drop(s2); // 相手が閉じた
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            pool.get("h", Duration::from_secs(5)).is_none(),
            "dead connection is discarded"
        );
    }

    #[test]
    fn stray_bytes_make_a_connection_unusable() {
        let pool = Pool::new(2, Duration::from_secs(5));
        let (c, mut s) = pair();
        pool.put(
            "h",
            BufReader::new(OriginStream::Plain(c)),
            Duration::from_secs(5),
        );
        s.write_all(b"junk").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(pool.get("h", Duration::from_secs(5)).is_none());
    }

    #[test]
    fn counts_stay_accurate_and_the_global_cap_holds() {
        // 全体上限 3、ホストあたり 2
        let pool = Pool::with_total(2, 3, Duration::from_secs(5));
        let mut keep = Vec::new();
        for host in ["a", "b"] {
            for _ in 0..2 {
                let (c, s) = pair();
                keep.push(s);
                pool.put(
                    host,
                    BufReader::new(OriginStream::Plain(c)),
                    Duration::from_secs(5),
                );
            }
        }
        // a に 2 本、b は 1 本目まで入って上限 3 に達し、2 本目は入らない
        assert_eq!(pool.idle_count(), 3, "全体上限で頭打ちになる");

        // 取り出すと減る
        assert!(pool.get("a", Duration::from_secs(5)).is_some());
        assert_eq!(pool.idle_count(), 2);
        assert!(pool.get("a", Duration::from_secs(5)).is_some());
        assert_eq!(pool.idle_count(), 1);
        assert!(pool.get("a", Duration::from_secs(5)).is_none());
        assert_eq!(pool.idle_count(), 1, "空振りでは減らない");

        // ホストあたりの上限で押し出されたぶんも数え違えない
        let pool = Pool::with_total(1, 10, Duration::from_secs(5));
        let mut keep = Vec::new();
        for _ in 0..3 {
            let (c, s) = pair();
            keep.push(s);
            pool.put(
                "h",
                BufReader::new(OriginStream::Plain(c)),
                Duration::from_secs(5),
            );
        }
        assert_eq!(pool.idle_count(), 1, "ホストあたり 1 本");

        // 期限切れの掃除でも数え違えない
        let pool = Pool::with_total(4, 10, Duration::from_millis(30));
        let mut keep = Vec::new();
        for _ in 0..3 {
            let (c, s) = pair();
            keep.push(s);
            pool.put(
                "h",
                BufReader::new(OriginStream::Plain(c)),
                Duration::from_secs(5),
            );
        }
        assert_eq!(pool.idle_count(), 3);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(pool.sweep(), 3, "3 本とも期限切れ");
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.sweep(), 0, "空なら何も捨てない");
    }

    #[test]
    fn respects_limits_and_expiry() {
        let pool = Pool::new(1, Duration::from_millis(30));
        let (c1, _s1) = pair();
        let (c2, _s2) = pair();
        pool.put(
            "h",
            BufReader::new(OriginStream::Plain(c1)),
            Duration::from_secs(5),
        );
        pool.put(
            "h",
            BufReader::new(OriginStream::Plain(c2)),
            Duration::from_secs(5),
        );
        assert_eq!(pool.idle_count(), 1, "max per host");
        std::thread::sleep(Duration::from_millis(60));
        pool.sweep();
        assert_eq!(pool.idle_count(), 0);
        let disabled = Pool::new(0, Duration::from_secs(1));
        let (c3, _s3) = pair();
        disabled.put(
            "h",
            BufReader::new(OriginStream::Plain(c3)),
            Duration::from_secs(1),
        );
        assert!(!disabled.enabled() && disabled.get("h", Duration::from_secs(1)).is_none());
    }
}
