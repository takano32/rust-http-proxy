//! `sys` の、実際にシステムコールを呼ぶ試験 (ソケット・パイプ・epoll・rlimit・procfs)。
//! 純粋な判断の試験 (`timeval` の丸め・`TCP_INFO` の緩衝の読み方・`sockaddr` の組み立て) は
//! それぞれの子に置いてある。

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;

use super::*;

/// glibc は閾と trim の閾を両方受け付ける (T32.3。上限の扱いは glibc の版で違うので見ない)。
#[test]
fn fixes_the_mmap_threshold() {
    assert!(fix_malloc_mmap_threshold(256 * 1024));
}

#[test]
fn splices_through_a_pipe() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut a = TcpStream::connect(addr).unwrap();
    let (b, _) = listener.accept().unwrap();
    let (c, _) = (TcpStream::connect(addr).unwrap(), ());
    let (mut d, _) = listener.accept().unwrap();

    a.write_all(b"hello splice").unwrap();
    let pipe = Pipe::new().unwrap();
    pipe.set_capacity(1 << 20);
    let n = splice_move(b.as_raw_fd(), pipe.write_fd, 64 * 1024).unwrap();
    assert_eq!(n, 12);
    let m = splice_move(pipe.read_fd, c.as_raw_fd(), n).unwrap();
    assert_eq!(m, 12);
    let mut buf = [0u8; 12];
    d.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello splice");
}

#[test]
fn peek_reports_eof_and_would_block() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let a = TcpStream::connect(addr).unwrap();
    let (b, _) = listener.accept().unwrap();
    let mut buf = [0u8; 1];
    assert_eq!(
        peek(a.as_raw_fd(), &mut buf).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(b);
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(peek(a.as_raw_fd(), &mut buf).unwrap(), 0, "peer closed");
}

#[test]
fn epoll_reports_readable_and_peer_close() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut a = TcpStream::connect(addr).unwrap();
    let (b, _) = listener.accept().unwrap();

    let ep = Epoll::new().unwrap();
    let token = 0xdead_beef_cafe_u64;
    ep.add(b.as_raw_fd(), EPOLLIN | EPOLLRDHUP, token).unwrap();

    let mut evs = [EpollEvent::default(); 8];
    // まだ何も来ていない
    assert_eq!(ep.wait(&mut evs, 10).unwrap(), 0);

    a.write_all(b"x").unwrap();
    assert_eq!(ep.wait(&mut evs, 1000).unwrap(), 1);
    assert_eq!(evs[0].token(), token);
    assert!(evs[0].events() & EPOLLIN != 0);

    drop(a);
    assert_eq!(ep.wait(&mut evs, 1000).unwrap(), 1);
    assert!(evs[0].events() & (EPOLLRDHUP | EPOLLIN) != 0);

    ep.delete(b.as_raw_fd()).unwrap();
    assert_eq!(ep.wait(&mut evs, 10).unwrap(), 0, "外したら報告されない");
}

#[test]
fn epoll_wakes_when_another_thread_adds_a_ready_fd() {
    // 監視スレッドが epoll_wait で待っている最中に別スレッドが足しても届くこと
    // (park は必ずワーカースレッド側から呼ばれる)
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut a = TcpStream::connect(addr).unwrap();
    let (b, _) = listener.accept().unwrap();
    a.write_all(b"already here").unwrap();

    let ep = std::sync::Arc::new(Epoll::new().unwrap());
    let ep2 = std::sync::Arc::clone(&ep);
    let fd = b.as_raw_fd();
    let adder = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        ep2.add(fd, EPOLLIN, 7).unwrap();
    });
    let mut evs = [EpollEvent::default(); 4];
    assert_eq!(ep.wait(&mut evs, 3000).unwrap(), 1);
    assert_eq!(evs[0].token(), 7);
    adder.join().unwrap();
}

#[test]
fn epoll_add_rejects_a_regular_file() {
    // epoll に入れられない記述子は EPERM。park は失敗を握りつぶさず旧経路へ落とす
    let ep = Epoll::new().unwrap();
    let f = std::fs::File::open("/proc/self/cmdline").unwrap();
    assert!(ep.add(f.as_raw_fd(), EPOLLIN, 0).is_err());
}

#[test]
fn accepted_sockets_inherit_the_listener_options() {
    // カーネルの挙動を固定するテスト。accept したソケットが待ち受けの
    // TCP_NODELAY / SO_RCVTIMEO / SO_SNDTIMEO を引き継がない環境が出たら、
    // ここが落ちて「接続ごとに設定する」経路へ戻せる (T9.3)
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let timeout = std::time::Duration::from_secs(7);
    inherit_socket_options(listener.as_raw_fd(), timeout, timeout).unwrap();

    let _client = TcpStream::connect(addr).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    assert!(accepted.nodelay().unwrap(), "TCP_NODELAY を引き継ぐ");
    assert_eq!(accepted.read_timeout().unwrap(), Some(timeout));
    assert_eq!(accepted.write_timeout().unwrap(), Some(timeout));
}

#[test]
fn accepted_sockets_inherit_separate_read_and_write_timeouts() {
    // 読み (1 要求目の要求行を待つ長さ) と書き (`PROXY_TIMEOUT_SECS`) は別の値で継承する (T28.0)
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let read = std::time::Duration::from_secs(3);
    let write = std::time::Duration::from_secs(9);
    inherit_socket_options(listener.as_raw_fd(), read, write).unwrap();

    let _client = TcpStream::connect(addr).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    assert_eq!(accepted.read_timeout().unwrap(), Some(read));
    assert_eq!(accepted.write_timeout().unwrap(), Some(write));
}

#[test]
fn accept_times_out_with_the_receive_timeout() {
    // SO_RCVTIMEO は accept() にも効く。serve の accept ループはこれを
    // 「まだ来ていない」として読み飛ばす (ログも sleep も無しに continue)
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let t = std::time::Duration::from_millis(50);
    inherit_socket_options(listener.as_raw_fd(), t, t).unwrap();
    let started = std::time::Instant::now();
    let err = listener.accept().unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "無期限に待っていない"
    );
}

#[test]
fn zero_timeout_is_inherited_as_no_timeout() {
    // `PROXY_TIMEOUT_SECS=0` = 無期限 (T10.6)。Linux の `timeval {0, 0}` が
    // まさに「タイムアウト無し」なので、そのまま継承させられる
    // (接続ごとの `set_write_timeout(Some(ZERO))` は std が断るので使えない)。
    // カーネルがこの意味を変えたらここが落ちる
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let zero = std::time::Duration::ZERO;
    inherit_socket_options(listener.as_raw_fd(), zero, zero).unwrap();

    let _client = TcpStream::connect(addr).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    assert!(accepted.nodelay().unwrap(), "TCP_NODELAY は引き継ぐ");
    assert_eq!(accepted.read_timeout().unwrap(), None, "読みは無期限");
    assert_eq!(accepted.write_timeout().unwrap(), None, "書きも無期限");
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn reports_the_open_file_limit() {
    // 幅や番号が食い違っていれば、ここが 0 や桁違いの値になって落ちる
    // (下げる方はテストのプロセス全体に効くので、ここでは呼ばない)
    let soft = max_open_files().expect("getrlimit(RLIMIT_NOFILE)");
    assert!(soft >= 64, "soft limit が小さすぎる: {}", soft);
    let open = std::fs::File::open("/proc/self/cmdline").unwrap();
    assert!(
        (open.as_raw_fd() as u64) < soft,
        "開いている fd 番号 {} が soft limit {} を超えている",
        open.as_raw_fd(),
        soft
    );
}

#[test]
fn poll_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let mut fds = [PollFd::new(a.as_raw_fd(), POLLIN)];
    assert_eq!(poll_fds(&mut fds, 10).unwrap(), 0);
}

/// `TCP_INFO` が loopback の接続で読めること (欄の位置が合っているかの確認。T14.5)。
///
/// 位置がずれていれば RTT が桁違いになるか、MTU が 65,536 (loopback) で出なくなる。
#[test]
fn reads_the_kernel_rtt_of_a_loopback_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();

    for s in [&client, &server] {
        let i = tcp_info(s.as_raw_fd()).expect("TCP_INFO が読めない");
        // loopback の往復は 1 ms 未満 (0 も含む。確立直後は RTT の標本が 1 つ)
        assert!(i.rtt_us < 1_000, "loopback の RTT が大きすぎる: {:?}", i);
        assert_eq!(i.retrans, 0, "{:?}", i);
        assert_eq!(i.total_retrans, 0, "{:?}", i);
        assert_eq!(i.lost, 0, "{:?}", i);
        // 輻輳窓と MTU は「欄の位置が合っているか」の目印 (ずれると 0 か桁違いになる)
        assert!(i.cwnd > 0 && i.cwnd < 100_000, "cwnd が変: {:?}", i);
        assert!((1_000..=70_000).contains(&i.pmtu), "経路 MTU が変: {:?}", i);
    }
}

/// TCP でない記述子は `None` (落ちない)。
#[test]
fn tcp_info_is_none_for_a_file() {
    let f = std::fs::File::open("/proc/self/cmdline").unwrap();
    assert_eq!(tcp_info(f.as_raw_fd()), None);
}

/// 自分で作った待ち受けが `std` の `TcpListener` と同じように使えること (T14.47)。
///
/// `sockaddr` の組み立て (族・ポートのバイト順) が違っていれば `bind` が `EINVAL` に
/// なるか、`local_addr` が別のアドレスで返る。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn listen_socket_binds_ipv4_with_the_given_backlog() {
    use std::os::fd::FromRawFd;

    let want: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let fd = listen_socket(want, 5).expect("listen_socket(127.0.0.1:0)");
    // SAFETY: listen まで済んだ記述子を 1 回だけ包む (以後は TcpListener が閉じる)。
    let listener = unsafe { TcpListener::from_raw_fd(fd) };
    let bound = listener.local_addr().unwrap();
    assert_eq!(bound.ip(), want.ip(), "{}", bound);
    assert_ne!(bound.port(), 0, "ephemeral port が割り当たっていない");

    let client = TcpStream::connect(bound).unwrap();
    let (accepted, peer) = listener.accept().unwrap();
    assert_eq!(peer.ip(), want.ip());
    drop((client, accepted));
}

/// IPv6 側も同じ (`sockaddr_in6` は 28 B で欄の並びが違う)。
/// `::1` が無い機械では黙って飛ばす。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn listen_socket_binds_ipv6_with_the_given_backlog() {
    use std::os::fd::FromRawFd;

    if TcpListener::bind("[::1]:0").is_err() {
        return;
    }
    let want: std::net::SocketAddr = "[::1]:0".parse().unwrap();
    let fd = listen_socket(want, 1024).expect("listen_socket([::1]:0)");
    // SAFETY: 上と同じ。
    let listener = unsafe { TcpListener::from_raw_fd(fd) };
    let bound = listener.local_addr().unwrap();
    assert_eq!(bound.ip(), want.ip(), "{}", bound);
    let client = TcpStream::connect(bound).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    drop((client, accepted));
}

/// 使用中のポートは `AddrInUse` で返り、記述子を漏らさない (失敗の枝で close している)。
///
/// `/proc/self/fd` は**プロセス全体**なので、同じバイナリで並行に走っている別のテストが
/// 開け閉てした記述子が乗る (T14.55 で幅を持たせたが、それでも 1 回揺れた)。そこで
/// **このテストだけを子プロセスで 1 本で**走らせ直し、数えるのは子の中でする (T17.14)。
/// 子の中には他のテストのスレッドが居ないので、前後の本数は 1 本も違わない
/// (Rust の記述子は `CLOEXEC` なので、親で開いている別のテストの記述子も子へ来ない)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn listen_socket_reports_address_in_use_without_leaking() {
    /// 子で走っている印 (テストの中だけで使う名前。設定の環境変数ではない)
    const CHILD: &str = "SYS_FD_LEAK_TEST_CHILD";
    const NAME: &str = "listen_socket_reports_address_in_use_without_leaking";
    if std::env::var_os(CHILD).is_none() {
        // 親: 同じテストバイナリを、このテスト 1 本だけ・1 スレッドで起こし直す。
        // テストの名前は `module_path!()` から crate 名を外したもの
        let module = module_path!().split_once("::").map_or("", |(_, rest)| rest);
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{}::{}", module, NAME),
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .expect("テストバイナリを子として起こせない");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "子のテストが通っていない: {}\n{}\n{}",
            out.status,
            stdout,
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap();
    // `SO_REUSEADDR` は「TIME_WAIT のポートを使える」だけで、生きている待ち受けとは共有しない
    let open_fds = || std::fs::read_dir("/proc/self/fd").unwrap().count();
    let before = open_fds();
    const TRIES: usize = 64;
    for _ in 0..TRIES {
        let err = listen_socket(addr, 128).expect_err("使用中のポートに bind できてしまった");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "{:?}", err);
    }
    // 子の中はこのテストだけなので、幅を持たせずに「1 本も増えていない」を見る
    let after = open_fds();
    assert_eq!(
        after, before,
        "失敗した {} 回ぶんの記述子が残っている ({} -> {})",
        TRIES, before, after
    );
}

/// 消えたクライアントを見つけるための TCP keepalive が、当てたとおりに
/// カーネルへ届いていること (T14.52)。`setsockopt` の定数が間違っていれば
/// `ENOPROTOOPT` になるか、読み戻した値が食い違う。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn keepalive_lands_on_the_socket_and_reads_back() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (accepted, _) = listener.accept().unwrap();

    // 何も当てていないソケットは off (既定の idle は `/proc/sys/net/ipv4/tcp_keepalive_time`)
    let before = keepalive_of(accepted.as_raw_fd()).expect("getsockopt");
    assert!(
        !before.on,
        "はじめから SO_KEEPALIVE が立っている: {:?}",
        before
    );

    // 既定と違う値を選ぶ (既定の 60/10/3 と取り違えないように)
    set_keepalive(accepted.as_raw_fd(), 7, 3, 5).expect("setsockopt");
    assert_eq!(
        keepalive_of(accepted.as_raw_fd()).expect("getsockopt"),
        Keepalive {
            on: true,
            idle_secs: 7,
            intvl_secs: 3,
            count: 5,
        }
    );
    // 当てていない方 (クライアント側) は素のまま
    assert!(!keepalive_of(client.as_raw_fd()).expect("getsockopt").on);
    drop((client, accepted));
}

/// 0 を渡しても「カーネルの既定のまま」にはならず 1 に上がること (T14.52)。
///
/// `TCP_KEEPIDLE=0` は `setsockopt` が `EINVAL` を返す値なので、丸めずに
/// 渡すと 4 回のうち 1 回が失敗して keepalive が中途半端に当たる。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn keepalive_rounds_zero_up_to_one() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (accepted, _) = listener.accept().unwrap();

    set_keepalive(accepted.as_raw_fd(), 0, 0, 0).expect("setsockopt");
    assert_eq!(
        keepalive_of(accepted.as_raw_fd()).expect("getsockopt"),
        Keepalive {
            on: true,
            idle_secs: 1,
            intvl_secs: 1,
            count: 1,
        }
    );
    drop((client, accepted));
}

/// TCP でない記述子には当たらない (失敗を握りつぶさずに `Err` で返すこと)。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn keepalive_fails_on_something_that_is_not_a_socket() {
    let f = std::fs::File::open("/proc/self/stat").unwrap();
    assert!(set_keepalive(f.as_raw_fd(), 60, 10, 3).is_err());
    assert!(keepalive_of(f.as_raw_fd()).is_err());
}

/// `boot_id` は 36 文字の UUID で、同じ起動の間は何度読んでも同じ (T24.0)。
///
/// 値そのものは機械ごとに違うので、形と「変わらないこと」だけを見る
/// (**ホストの再起動とプロセスだけの再起動を切り分ける物差しになる**)。
#[test]
fn boot_id_is_a_stable_uuid() {
    let id = boot_id().expect("/proc/sys/kernel/random/boot_id should be readable");
    assert_eq!(id.len(), 36, "{id}");
    assert_eq!(id.matches('-').count(), 4, "{id}");
    assert!(
        id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()),
        "{id}"
    );
    assert_eq!(boot_id().as_deref(), Some(id.as_str()));
}
