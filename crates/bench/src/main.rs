//! `std` だけで書いたベンチ。Python 版 (`scripts/bench.py`) と同じ 3 種を測るが、
//! 負荷側もオリジンも Rust なのでプロキシ自身の限界が見える。
//!
//! ```text
//! cargo run --release --bin bench -- --proxy 127.0.0.1:18080 --conc 8 --seconds 5
//! ```
//!
//! 測るもの:
//!   1. direct  : オリジン直結の要求/秒 (ベンチが律速していないことの確認。5 万 req/s 以上出ること)
//!   2. forward : 平文 HTTP をプロキシ経由で転送したときの要求/秒と p50/p99
//!   3. tunnel  : CONNECT トンネル 1 本のスループット (MiB/s、`--seconds` 秒)。
//!      **この経路はベンチ側が律速する** (プロキシは `splice` でコピー 0 回、ベンチは送りと受けで
//!      コピー 2 回)。`scripts/cpu-per-request.sh --only tunnel` は両方を big コアに置いて測る (T10.8)
//!   4. connect : CONNECT の確立/秒 (短命トンネル)
//!   5. idle-tunnels: `--conc N` 本の CONNECT を張ったまま `--seconds` 秒握る
//!      (プロキシ側のスレッド数と RSS を見るためのモード。`--only idle-tunnels` でだけ走る)
//!   6. syscall-cost: この機械での `sendto` / `recvfrom` 1 回の実費 (プロキシは使わない。
//!      `--only syscall-cost` でだけ走る。**`taskset` で cpu を固定して使うこと**)

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// トンネルのスループット計測で 1 回に読む大きさ。
///
/// 送る側 ([`spawn_blaster`]) は相手が閉じるまで送り続け、受ける側は `--seconds` 秒で止める。
/// **転送量ではなく時間で終わらせる**のは、他のモードと揃えるためと、短すぎる計測だと
/// `/proc/<pid>/stat` の 10 ms 刻みが結果を丸めてしまうため。256 MiB 固定だった頃は
/// 0.2 秒しか走らず、プロキシの user CPU が 1 tick 未満で「0.00 us/MiB」に見えていた (T10.8)。
const TUNNEL_READ_BYTES: usize = 256 * 1024;

struct Args {
    proxy: Option<String>,
    conc: usize,
    seconds: u64,
    body_bytes: usize,
    /// オリジン応答を保存可能にする (キャッシュ HIT 側を測る)
    cacheable: bool,
    /// 測る種類 ("all" / "direct" / "forward" / "tunnel" / "connect")
    only: String,
    /// 1 要求ごとに接続を張り直す (接続あたりの固定費を測る)
    no_keepalive: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: bench [--proxy HOST:PORT] [--conc N] [--seconds N] [--body-bytes N]\n\
                     [--only direct|forward|tunnel|connect|idle-tunnels|syscall-cost|all]\n\
         \n\
         Without --proxy only the direct (origin) baseline is measured."
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut args = Args {
        proxy: None,
        conc: 8,
        seconds: 5,
        body_bytes: 1024,
        cacheable: false,
        only: "all".to_string(),
        no_keepalive: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--proxy" => args.proxy = Some(value()),
            "--conc" => args.conc = value().parse().unwrap_or_else(|_| usage()),
            "--seconds" => args.seconds = value().parse().unwrap_or_else(|_| usage()),
            "--body-bytes" => args.body_bytes = value().parse().unwrap_or_else(|_| usage()),
            "--cacheable" => args.cacheable = true,
            "--only" => args.only = value(),
            "--no-keepalive" => args.no_keepalive = true,
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }
    if args.conc == 0 || args.seconds == 0 {
        usage();
    }
    args
}

// ---------------------------------------------------------------- オリジン

/// 固定応答を 1 回の `write_all` で返す keep-alive オリジン。
fn spawn_origin(body_bytes: usize, cacheable: bool) -> io::Result<SocketAddr> {
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
fn spawn_blaster() -> io::Result<SocketAddr> {
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
fn spawn_sink() -> io::Result<SocketAddr> {
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
fn spawn_holder() -> io::Result<SocketAddr> {
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

// ---------------------------------------------------------------- 集計

struct Report {
    ops: u64,
    bytes: u64,
    elapsed: Duration,
    latencies_us: Vec<u32>,
}

impl Report {
    fn percentile(&self, p: f64) -> f64 {
        if self.latencies_us.is_empty() {
            return 0.0;
        }
        let idx = ((self.latencies_us.len() - 1) as f64 * p).round() as usize;
        self.latencies_us[idx] as f64 / 1000.0
    }

    fn print(&self, label: &str) {
        let secs = self.elapsed.as_secs_f64().max(1e-9);
        println!(
            "{:<8} {:>9.0} op/s  {:>8.1} MiB/s  p50 {:>7.3} ms  p99 {:>7.3} ms  ({} ops in {:.1}s)",
            label,
            self.ops as f64 / secs,
            self.bytes as f64 / secs / (1024.0 * 1024.0),
            self.percentile(0.50),
            self.percentile(0.99),
            self.ops,
            secs,
        );
    }
}

/// `conc` 本のスレッドで `seconds` 秒だけ `work` を回す。`work` は 1 回の操作の
/// (バイト数) を返し、レイテンシは呼び出し側で測る。
fn run_load<F>(conc: usize, seconds: u64, work: F) -> Report
where
    F: Fn(&AtomicBool, &mut Vec<u32>, &mut u64) + Send + Sync + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let work = Arc::new(work);
    let started = Instant::now();
    let mut handles = Vec::with_capacity(conc);
    for _ in 0..conc {
        let stop = Arc::clone(&stop);
        let work = Arc::clone(&work);
        handles.push(thread::spawn(move || {
            let mut lat = Vec::new();
            let mut bytes = 0u64;
            work(&stop, &mut lat, &mut bytes);
            (lat, bytes)
        }));
    }
    thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    let mut latencies_us = Vec::new();
    let mut bytes = 0u64;
    for h in handles {
        if let Ok((lat, b)) = h.join() {
            latencies_us.extend(lat);
            bytes += b;
        }
    }
    let elapsed = started.elapsed();
    latencies_us.sort_unstable();
    Report {
        ops: latencies_us.len() as u64,
        bytes,
        elapsed,
        latencies_us,
    }
}

// ---------------------------------------------------------------- HTTP

/// 応答を 1 つ読み切る。戻り値は読んだバイト数と keep-alive 可否。
fn read_response(reader: &mut BufReader<TcpStream>, line: &mut String) -> io::Result<(u64, bool)> {
    let mut total = 0u64;
    let mut length: Option<u64> = None;
    let mut keep = true;
    loop {
        line.clear();
        let n = reader.read_line(line)?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        total += n as u64;
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().ok();
            } else if k.eq_ignore_ascii_case("connection") && v.trim().eq_ignore_ascii_case("close")
            {
                keep = false;
            }
        }
    }
    let mut body = vec![0u8; 64 * 1024];
    let mut left = length.unwrap_or(0);
    while left > 0 {
        let want = left.min(body.len() as u64) as usize;
        reader.read_exact(&mut body[..want])?;
        left -= want as u64;
        total += want as u64;
    }
    Ok((total, keep))
}

/// keep-alive で HTTP 要求を投げ続ける負荷。`request` は 1 要求ぶんのバイト列。
fn http_load(
    target: SocketAddr,
    request: Vec<u8>,
    conc: usize,
    seconds: u64,
    keepalive: bool,
) -> Report {
    run_load(conc, seconds, move |stop, lat, bytes| {
        let mut conn: Option<(TcpStream, BufReader<TcpStream>)> = None;
        let mut line = String::new();
        while !stop.load(Ordering::Relaxed) {
            if conn.is_none() {
                let Ok(s) = TcpStream::connect(target) else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let _ = s.set_nodelay(true);
                let Ok(clone) = s.try_clone() else { continue };
                conn = Some((s, BufReader::new(clone)));
            }
            let (sock, reader) = conn.as_mut().expect("connected");
            let t0 = Instant::now();
            let outcome = sock
                .write_all(&request)
                .and_then(|()| read_response(reader, &mut line));
            match outcome {
                Ok((n, keep)) => {
                    lat.push(t0.elapsed().as_micros().min(u32::MAX as u128) as u32);
                    *bytes += n;
                    if !keep || !keepalive {
                        conn = None;
                    }
                }
                Err(_) => conn = None,
            }
        }
    })
}

/// CONNECT を張って 200 応答まで読む。
fn open_tunnel(
    proxy: SocketAddr,
    target: SocketAddr,
) -> io::Result<(TcpStream, BufReader<TcpStream>)> {
    let mut sock = TcpStream::connect(proxy)?;
    sock.set_nodelay(true)?;
    let req = format!("CONNECT {0} HTTP/1.1\r\nHost: {0}\r\n\r\n", target);
    sock.write_all(req.as_bytes())?;
    let mut reader = BufReader::new(sock.try_clone()?);
    let mut line = String::new();
    let mut status_ok = false;
    let mut first = true;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        if first {
            status_ok = line.contains(" 200 ");
            first = false;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    if !status_ok {
        return Err(io::Error::other("CONNECT was refused"));
    }
    Ok((sock, reader))
}

/// CONNECT を張り、`200` を読み切ったソケットだけを返す (`BufReader` を残さない)。
///
/// アイドルトンネルを 5,000 本握るモード用。[`open_tunnel`] は 1 本につき
/// `try_clone` した記述子と 8 KiB の `BufReader` を持つので、本数ぶん積むと
/// ベンチ側が先に重くなる。CONNECT の応答のあとには何も続かないので 1 バイトずつ読む。
fn open_tunnel_bare(proxy: SocketAddr, target: SocketAddr) -> io::Result<TcpStream> {
    let mut sock = TcpStream::connect(proxy)?;
    sock.set_nodelay(true)?;
    let req = format!("CONNECT {0} HTTP/1.1\r\nHost: {0}\r\n\r\n", target);
    sock.write_all(req.as_bytes())?;
    let mut head = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if sock.read(&mut byte)? == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        head.push(byte[0]);
        if head.len() > 4096 {
            return Err(io::Error::other("CONNECT response header too long"));
        }
    }
    if !head.starts_with(b"HTTP/1.1 200") {
        return Err(io::Error::other("CONNECT was refused"));
    }
    Ok(sock)
}

/// `conc` 本の CONNECT を張り、`seconds` 秒そのまま握る。
///
/// プロキシ側の「アイドルなトンネル 1 本あたりのスレッドと RSS」を見るためのモード。
/// 5,000 本張るので、ベンチ側は 1 スレッドで接続を `Vec` に持つ (5,000 スレッドを作らない)。
/// プロキシの `PROXY_MAX_CONNS` (既定 4096) に当たるので、測るときは `0` か 8192 にする。
fn idle_tunnels(proxy: SocketAddr, conc: usize, seconds: u64) {
    let holder = spawn_holder().expect("holder");
    let mut held: Vec<TcpStream> = Vec::with_capacity(conc);
    let mut latencies_us: Vec<u32> = Vec::with_capacity(conc);
    let mut failed = 0usize;
    let started = Instant::now();
    for _ in 0..conc {
        let t0 = Instant::now();
        match open_tunnel_bare(proxy, holder) {
            Ok(sock) => {
                latencies_us.push(t0.elapsed().as_micros().min(u32::MAX as u128) as u32);
                held.push(sock);
            }
            Err(_) => failed += 1,
        }
    }
    let opened = started.elapsed();
    println!(
        "idle-tun {} tunnels open in {:.1}s ({} failed); holding {}s",
        held.len(),
        opened.as_secs_f64(),
        failed,
        seconds
    );
    // 握ったまま待つ (この間にプロキシのスレッド数と RSS を見る)
    thread::sleep(Duration::from_secs(seconds));
    // 握れた本数を「操作数」として出す (scripts/cpu-per-request.sh がこの行から読む)
    latencies_us.sort_unstable();
    Report {
        ops: held.len() as u64,
        bytes: 0,
        elapsed: started.elapsed(),
        latencies_us,
    }
    .print("idle-tun");
    drop(held);
}

// ---------------------------------------------- システムコールの実費 (--only syscall-cost)

/// この機械での「1 回のシステムコールの実費」を測る隠しモード (T10.3 用)。
///
/// プロキシもオリジンも使わない。loopback の TCP ソケット対を自分で作り、
/// `sendto` / `recvfrom` を何もしないループで回して 1 回あたりの CPU を出す。
/// 見るのは **`/proc/self/task/<tid>/stat` の utime / stime** で、`strace` は使わない
/// (`strace` は 1 回のコストを大きく変えてしまうので、回数を数える用)。
///
/// **必ず `taskset` で固定して走らせること。** この機械は big.LITTLE で、
/// システムコールのコストが cpu0-3 (Cortex-A55) と cpu4-7 (Cortex-A78) で 2 倍以上違う。
///
/// ```text
/// taskset -c 4-7 bench --only syscall-cost --seconds 3
/// ```
#[cfg(target_os = "linux")]
mod syscost {
    use std::ffi::{c_int, c_void};
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::os::fd::AsRawFd;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    // 外部クレートは使わない (TASKS.md §0)。crates/sys/src/sys.rs と同じ形で宣言する。
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

        // 1. 何もしないシステムコール = 出入りだけの床
        measure("getppid".into(), 0, 1, dur, || {
            // SAFETY: 引数の無いシステムコール。
            unsafe {
                getppid();
            }
        })
        .print();

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

#[cfg(not(target_os = "linux"))]
mod syscost {
    /// Linux 以外では測らない (`/proc/self/task/<tid>/stat` が無い)。
    pub fn run(_seconds: u64) {
        println!("syscall-cost: linux only");
    }
}

fn parse_addr(s: &str) -> SocketAddr {
    use std::net::ToSocketAddrs;
    s.to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .unwrap_or_else(|| {
            eprintln!("bench: cannot resolve {}", s);
            std::process::exit(2);
        })
}

fn main() {
    let args = parse_args();

    // システムコールの実費だけを測るモード (プロキシもオリジンも使わない)
    if args.only == "syscall-cost" {
        syscost::run(args.seconds);
        return;
    }

    let origin = spawn_origin(args.body_bytes, args.cacheable).expect("origin");
    println!(
        "bench: conc={} seconds={} body={}B cacheable={} keep-alive={} origin={}",
        args.conc, args.seconds, args.body_bytes, args.cacheable, !args.no_keepalive, origin
    );

    let want = |name: &str| args.only == "all" || args.only == name;

    if want("direct") {
        let direct_req = format!("GET / HTTP/1.1\r\nHost: {}\r\n\r\n", origin).into_bytes();
        http_load(
            origin,
            direct_req,
            args.conc,
            args.seconds,
            !args.no_keepalive,
        )
        .print("direct");
    }

    let Some(proxy) = args.proxy.as_deref() else {
        return;
    };
    let proxy = parse_addr(proxy);

    if want("forward") {
        let forward_req =
            format!("GET http://{0}/ HTTP/1.1\r\nHost: {0}\r\n\r\n", origin).into_bytes();
        http_load(
            proxy,
            forward_req,
            args.conc,
            args.seconds,
            !args.no_keepalive,
        )
        .print("forward");
    }

    // トンネル 1 本のスループット (他のモードと同じく `--seconds` 秒で終わる)
    if want("tunnel") {
        let blaster = spawn_blaster().expect("blaster");
        match open_tunnel(proxy, blaster) {
            Ok((sock, mut reader)) => {
                let mut buf = vec![0u8; TUNNEL_READ_BYTES];
                let deadline = Duration::from_secs(args.seconds);
                let t0 = Instant::now();
                let mut got = 0u64;
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => got += n as u64,
                    }
                    if t0.elapsed() >= deadline {
                        break;
                    }
                }
                let secs = t0.elapsed().as_secs_f64().max(1e-9);
                // 先に切る。切らないと blaster が送り続けたまま次のモードの計測に混ざる。
                let _ = sock.shutdown(Shutdown::Both);
                println!(
                    "{:<8} {:>9} op/s  {:>8.1} MiB/s  ({} MiB through one tunnel)",
                    "tunnel",
                    1,
                    got as f64 / secs / (1024.0 * 1024.0),
                    got / (1 << 20)
                );
            }
            Err(e) => println!("tunnel   skipped: {}", e),
        }
    }

    // アイドルなトンネルを握り続ける (--only idle-tunnels のときだけ。all には入れない)
    if args.only == "idle-tunnels" {
        idle_tunnels(proxy, args.conc, args.seconds);
        return;
    }

    // CONNECT の確立/秒
    if !want("connect") {
        return;
    }
    let sink = spawn_sink().expect("sink");
    run_load(args.conc, args.seconds, move |stop, lat, _bytes| {
        while !stop.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            match open_tunnel(proxy, sink) {
                Ok((sock, _)) => {
                    lat.push(t0.elapsed().as_micros().min(u32::MAX as u128) as u32);
                    let _ = sock.shutdown(Shutdown::Both);
                }
                Err(_) => thread::sleep(Duration::from_millis(2)),
            }
        }
    })
    .print("connect");
}
