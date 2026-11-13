//! この機械での「1 回のシステムコールの実費」を測る隠しモード (T10.3 用。`--only syscall-cost`)。
//!
//! プロキシもオリジンも使わない。loopback の TCP ソケット対を自分で作り、
//! `sendto` / `recvfrom` を何もしないループで回して 1 回あたりの CPU を出す。
//! 見るのは **`/proc/self/task/<tid>/stat` の utime / stime** で、`strace` は使わない
//! (`strace` は 1 回のコストを大きく変えてしまうので、回数を数える用)。
//!
//! **必ず `taskset` で固定して走らせること。** この機械は big.LITTLE で、
//! システムコールのコストが cpu0-3 (Cortex-A55) と cpu4-7 (Cortex-A78) で 2 倍以上違う。
//!
//! ```text
//! taskset -c 4-7 bench --only syscall-cost --seconds 3
//! ```

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{c_int, c_void};
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::os::fd::AsRawFd;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    // 外部クレートは使わない (仕様 S25)。crates/sys/src/sys.rs と同じ形で宣言する。
    unsafe extern "C" {
        fn getppid() -> c_int;
        fn gettid() -> c_int;
        fn sysconf(name: c_int) -> i64;
        fn setsockopt(
            fd: c_int,
            level: c_int,
            name: c_int,
            value: *const c_void,
            len: u32, // socklen_t
        ) -> c_int;
        fn sched_setaffinity(pid: c_int, size: usize, mask: *const u64) -> c_int;
    }

    /// `_SC_CLK_TCK` (glibc)。`/proc/<pid>/stat` の utime / stime の単位。
    const SC_CLK_TCK: c_int = 2;
    const SOL_SOCKET: c_int = 1;
    const SO_SNDBUF: c_int = 7;
    const SO_RCVBUF: c_int = 8;

    /// ソケットバッファを広げる。既定の送信バッファは 16 KiB しかないので、
    /// 64 KiB を 1 回で送るモードが「相手が読むまで待つ」形になってしまう
    /// (同じスレッドで送って受けるので、待たれると止まる)。
    fn set_bufs(s: &TcpStream, bytes: c_int) {
        for name in [SO_SNDBUF, SO_RCVBUF] {
            // SAFETY: 有効な fd と、c_int 1 つぶんの正しい長さを渡している。
            unsafe {
                setsockopt(
                    s.as_raw_fd(),
                    SOL_SOCKET,
                    name,
                    (&raw const bytes).cast::<c_void>(),
                    size_of::<c_int>() as u32,
                );
            }
        }
    }

    /// 呼んだスレッドを `mask` の cpu に固定する (0x0f = cpu0-3、0xf0 = cpu4-7)。
    fn pin(mask: u64) -> bool {
        // SAFETY: pid 0 = 自スレッド。mask は 8 バイトの有効な領域で、長さを渡している。
        unsafe { sched_setaffinity(0, size_of::<u64>(), &raw const mask) == 0 }
    }

    /// このスレッドの utime + stime (tick)。プロセス全体ではなくスレッド単位で見るので、
    /// 相手役のスレッドの CPU が混ざらない。
    fn cpu_ticks() -> (u64, u64) {
        // SAFETY: 引数の無いシステムコール。
        let tid = unsafe { gettid() };
        let stat =
            std::fs::read_to_string(format!("/proc/self/task/{}/stat", tid)).unwrap_or_default();
        // comm に空白や ')' が入りうるので、最後の ')' から後ろを見る
        let rest = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
        let f: Vec<&str> = rest.split_whitespace().collect();
        // rest の先頭は 3 番目のフィールド (state) なので、utime は 14 - 3、stime は 15 - 3
        let get = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        (get(11), get(12))
    }

    fn us_per_tick() -> f64 {
        // SAFETY: 定数を渡すだけ。
        let tck = unsafe { sysconf(SC_CLK_TCK) };
        if tck > 0 { 1e6 / tck as f64 } else { 10_000.0 }
    }

    /// 1 行ぶんの計測結果。`calls` は 1 周で発行するシステムコールの数。
    struct Row {
        what: String,
        bytes: usize,
        calls: usize,
        iters: u64,
        user_us: f64,
        kern_us: f64,
        wall_us: f64,
    }

    impl Row {
        fn print(&self) {
            let cpu = self.user_us + self.kern_us;
            println!(
                "{:<24} {:>6} B  {} call  {:>7.3} us/iter (user {:>6.3} / kernel {:>6.3})  \
                 {:>7.3} us/call  wall {:>7.3}  {} iters",
                self.what,
                self.bytes,
                self.calls,
                cpu,
                self.user_us,
                self.kern_us,
                cpu / self.calls as f64,
                self.wall_us,
                self.iters,
            );
        }
    }

    /// `dur` のあいだ `f` を回し、1 周あたりの CPU を返す。
    /// 時計を毎回見ると (vDSO でも) 数十 ns の下駄をはくので、64 周ごとに見る。
    fn measure(
        what: String,
        bytes: usize,
        calls: usize,
        dur: Duration,
        mut f: impl FnMut(),
    ) -> Row {
        let us = us_per_tick();
        let (u0, s0) = cpu_ticks();
        let t0 = Instant::now();
        let mut iters = 0u64;
        loop {
            for _ in 0..64 {
                f();
            }
            iters += 64;
            if t0.elapsed() >= dur {
                break;
            }
        }
        let wall = t0.elapsed();
        let (u1, s1) = cpu_ticks();
        let n = iters as f64;
        Row {
            what,
            bytes,
            calls,
            iters,
            user_us: (u1 - u0) as f64 * us / n,
            kern_us: (s1 - s0) as f64 * us / n,
            wall_us: wall.as_secs_f64() * 1e6 / n,
        }
    }

    /// loopback の TCP 接続を 1 本作る (両端を返す)。
    fn pair(bufs: c_int) -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let a = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (b, _) = listener.accept().expect("accept");
        for s in [&a, &b] {
            let _ = s.set_nodelay(true);
            set_bufs(s, bufs);
            // 保険。相手が読んでくれないと止まってしまう形の測り方をするので、
            // 詰まったら待ち続けずに落とす (プロキシ本体も時間制限つきで読み書きしている)
            let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        }
        (a, b)
    }

    /// 送って受ける相手役のスレッド。`mask` が 0 でなければその cpu に固定する。
    fn echo(
        mut b: TcpStream,
        bytes: usize,
        mask: u64,
        stop: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            if mask != 0 {
                pin(mask);
            }
            let mut buf = vec![0u8; bytes];
            while !stop.load(Ordering::Relaxed) {
                let mut got = 0usize;
                while got < bytes {
                    match b.read(&mut buf[got..]) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got += n,
                    }
                }
                if b.write_all(&buf).is_err() {
                    return;
                }
            }
        })
    }

    pub fn run(seconds: u64) {
        let dur = Duration::from_secs(seconds.max(1));
        println!(
            "syscall-cost: {}s per row, {} tick/s (このスレッドの utime+stime を周回数で割った値)",
            dur.as_secs(),
            (1e6 / us_per_tick()).round() as u64
        );
        floor(dur);
        peek_empty(dur);
        send_recv_same_thread(dur);
        send_recv_split(dur);
        pingpong(dur);
    }

    /// 出入りだけの床 (T34.102 で `run` から出した。以下の 4 つも同じ)。
    fn floor(dur: Duration) {
        // 1. 何もしないシステムコール = 出入りだけの床
        measure("getppid".into(), 0, 1, dur, || {
            // SAFETY: 引数の無いシステムコール。
            unsafe {
                getppid();
            }
        })
        .print();
    }

    /// 空のソケットを覗く 1 回。
    fn peek_empty(dur: Duration) {
        // 2. プールの生存確認と同じ形: 空のソケットへの recv(MSG_PEEK|MSG_DONTWAIT)
        {
            let (a, _b) = pair(1 << 20);
            a.set_nonblocking(true).expect("nonblocking");
            let mut byte = [0u8; 1];
            measure("recv peek EAGAIN".into(), 1, 1, dur, || {
                let _ = a.peek(&mut byte);
            })
            .print();
        }
    }

    /// 同じスレッドで送って受ける (大きさ 5 通り)。
    fn send_recv_same_thread(dur: Duration) {
        // 3. 送って受ける (同じスレッド)。loopback は sendto の中で受信側の TCP 処理まで
        //    済ませるので、続く recvfrom は待たずに全部返る = 寝起きが入っていない値
        for bytes in [64usize, 1024, 4096, 16384, 65536] {
            let (mut a, mut b) = pair(1 << 20);
            let out = vec![b'x'; bytes];
            let mut inbuf = vec![0u8; bytes];
            let mut recvs = 0u64;
            let row = measure("sendto+recvfrom".into(), bytes, 2, dur, || {
                a.write_all(&out).expect("send");
                let mut got = 0usize;
                while got < bytes {
                    match b.read(&mut inbuf[got..]) {
                        Ok(0) => break,
                        Ok(n) => got += n,
                        Err(e) => panic!("recv: {}", e),
                    }
                    recvs += 1;
                }
            });
            row.print();
            // 1 周で recvfrom が 2 回以上に分かれていたら、us/call の分母が違う
            if recvs > row.iters {
                println!("  (recvfrom {:.2} 回/周)", recvs as f64 / row.iters as f64);
            }
            let _ = a.shutdown(Shutdown::Both);
            let _ = b.shutdown(Shutdown::Both);
        }
    }

    /// 送りと受けの 1 回ぶんを、片方だけ回数を増やした傾きで切り分ける。
    fn send_recv_split(dur: Duration) {
        // 3b. sendto と recvfrom の切り分け。1 周の中で片方だけ回数を増やし、
        //     増えたぶんの傾きから 1 回ぶんを出す (64 B なのでコピー量の差は無視できる)。
        //     8 回送って 1 回で受ける → 傾きは sendto 1 回ぶん
        {
            let (mut a, mut b) = pair(1 << 20);
            let out = vec![b'x'; 64];
            let mut inbuf = vec![0u8; 64 * 8];
            measure("sendto x8 + recvfrom".into(), 64, 9, dur, || {
                for _ in 0..8 {
                    a.write_all(&out).expect("send");
                }
                let mut got = 0usize;
                while got < 64 * 8 {
                    match b.read(&mut inbuf[got..]) {
                        Ok(0) => break,
                        Ok(n) => got += n,
                        Err(e) => panic!("recv: {}", e),
                    }
                }
            })
            .print();
            let _ = a.shutdown(Shutdown::Both);
        }
        // 1 回で送って 8 回に分けて受ける → 傾きは recvfrom 1 回ぶん
        {
            let (mut a, mut b) = pair(1 << 20);
            let out = vec![b'x'; 64 * 8];
            let mut inbuf = [0u8; 64];
            measure("sendto + recvfrom x8".into(), 64, 9, dur, || {
                a.write_all(&out).expect("send");
                for _ in 0..8 {
                    let mut got = 0usize;
                    while got < 64 {
                        match b.read(&mut inbuf[got..]) {
                            Ok(0) => break,
                            Ok(n) => got += n,
                            Err(e) => panic!("recv: {}", e),
                        }
                    }
                }
            })
            .print();
            let _ = a.shutdown(Shutdown::Both);
        }
    }

    /// 2 スレッドの往復 (相手役の置き場所 2 通り × 大きさ 2 通り)。
    fn pingpong(dur: Duration) {
        // 4. 2 スレッドの往復。測るのはこのスレッドだけなので 1 周 = sendto 1 + recvfrom 1
        //    + 「寝て起こされる」1 回。3. との差がそのぶん。
        //    相手役を cpu0-3 に置くと、プロキシ (cpu4-7) がベンチ (cpu0-3) を起こす本番と同じ形になる
        for (label, mask) in [("pingpong echo@4-7", 0xf0u64), ("pingpong echo@0-3", 0x0f)] {
            for bytes in [64usize, 1024] {
                let (mut a, b) = pair(1 << 20);
                let stop = Arc::new(AtomicBool::new(false));
                let h = echo(b, bytes, mask, Arc::clone(&stop));
                let out = vec![b'x'; bytes];
                let mut inbuf = vec![0u8; bytes];
                measure(label.into(), bytes, 2, dur, || {
                    a.write_all(&out).expect("send");
                    let mut got = 0usize;
                    while got < bytes {
                        match a.read(&mut inbuf[got..]) {
                            Ok(0) => panic!("echo closed"),
                            Ok(n) => got += n,
                            Err(e) => panic!("recv: {}", e),
                        }
                    }
                })
                .print();
                stop.store(true, Ordering::Relaxed);
                let _ = a.shutdown(Shutdown::Both);
                let _ = h.join();
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::run;

/// Linux 以外では測らない (`/proc/self/task/<tid>/stat` が無い)。
#[cfg(not(target_os = "linux"))]
pub(crate) fn run(_seconds: u64) {
    println!("syscall-cost: linux only");
}
