//! IPv4 / IPv6 デュアルスタックのためのネットワーク補助 (`PROXY_IPV6=off` で IPv4 のみ)。
//!
//! - 待ち受け: `[::]` と `0.0.0.0` の両方を試し、IPv6 が無い環境では IPv4 だけにフォールバック。
//!   IPv6 無効時は `0.0.0.0` のみ
//! - 接続: A / AAAA の両方を引き、IPv6 優先で 250 ms ずつずらして並行に試す (Happy Eyeballs,
//!   RFC 8305)。IPv6 無効時は A レコードだけ
//! - `[2001:db8::1]:8080` 形式のホスト・ポート解析と、v4-mapped アドレス (`::ffff:1.2.3.4`) の正規化

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crate::{log_debug, log_warn};

/// Happy Eyeballs で次の接続試行を始めるまでの間隔。
const STAGGER: Duration = Duration::from_millis(250);

/// IPv6 を使うか (既定 on)。起動時に設定から決める。
static IPV6_ENABLED: AtomicBool = AtomicBool::new(true);

/// IPv6 の試行がこれだけ**続けて**負けたら、初めて見るホストも IPv4 を先頭にする (T12.1)。
const IPV6_LOSS_LIMIT: u64 = 3;
/// IPv4 を先頭にしている間、この秒数に 1 回だけ IPv6 を先頭に戻して試す (生き返れば自然に戻る)。
const IPV6_PROBE_SECS: u64 = 600;

/// IPv6 の候補を実際に試した回数 (`/status` と `/metrics`)。
static IPV6_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
/// IPv6 の候補が最初に確立した回数。
static IPV6_WINS: AtomicU64 = AtomicU64::new(0);
/// IPv6 を試したのに IPv4 が勝った回数。
static IPV6_LOSSES: AtomicU64 = AtomicU64::new(0);
/// 連続で負けた数 (勝ったら 0 に戻す)。`IPV6_WINS == 0` のときだけ意味を持つ。
static IPV6_LOSS_STREAK: AtomicU64 = AtomicU64::new(0);
/// 次に IPv6 を先頭に戻して試す時刻 (epoch 秒)。
static IPV6_PROBE_AT: AtomicU64 = AtomicU64::new(0);
/// 切り替えの `warn` を出したか (1 回だけ)。
static IPV6_WARNED: AtomicBool = AtomicBool::new(false);

pub fn set_ipv6_enabled(on: bool) {
    IPV6_ENABLED.store(on, Ordering::Relaxed);
}

pub fn ipv6_enabled() -> bool {
    IPV6_ENABLED.load(Ordering::Relaxed)
}

/// `host:port` / `[v6]:port` / `[v6]` / `host` / 素の `v6` を (ホスト, ポート) に分ける。
/// 文字列を作らない版 ([`split_host_port`] は所有権が要るときに使う)。
#[inline]
pub fn split_host_port_ref(s: &str) -> (&str, Option<u16>) {
    // 要求ごとに何度も通るので、区切りの探索も空白の除去も ASCII だけで済ませる
    let s = s.trim_ascii();
    if let Some(rest) = s.strip_prefix('[') {
        if let Some(end) = rest.as_bytes().iter().position(|b| *b == b']') {
            let host = &rest[..end];
            let port = rest[end + 1..]
                .strip_prefix(':')
                .and_then(|p| p.parse::<u16>().ok());
            return (host, port);
        }
        return (s, None);
    }
    // ':' が 2 つ以上あれば括弧無しの IPv6 リテラル (ポート無し)
    if crate::ascii::count(s, b':') >= 2 {
        return (s, None);
    }
    match crate::ascii::rsplit_once(s, b':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(p) => (host, Some(p)),
            Err(_) => (s, None),
        },
        None => (s, None),
    }
}

/// [`split_host_port_ref`] のホストを複製して返す版。
#[inline]
pub fn split_host_port(s: &str) -> (String, Option<u16>) {
    let (host, port) = split_host_port_ref(s);
    (host.to_string(), port)
}

/// ホストとポートを `host:port` に組み立てる (IPv6 リテラルは括弧で囲む)。
pub fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

/// ポートが無ければ `default` を補った `host:port` を返す。
pub fn with_default_port(s: &str, default: u16) -> String {
    let (host, port) = split_host_port(s);
    join_host_port(&host, port.unwrap_or(default))
}

/// v4-mapped IPv6 (`::ffff:1.2.3.4`) を IPv4 に戻す。
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

#[inline]
pub fn canonical_addr(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(canonical_ip(addr.ip()), addr.port())
}

/// 待ち受けソケットを作る。`addrs` が空なら IPv6 有効時はデュアルスタック (`[::]` + `0.0.0.0`)、
/// 無効時は `0.0.0.0` だけ。`port` が 0 のときは最初に取れたポートを残りにも使う。
pub fn bind_all(addrs: &[IpAddr], port: u16) -> io::Result<Vec<TcpListener>> {
    bind_all_with(addrs, port, ipv6_enabled())
}

pub fn bind_all_with(addrs: &[IpAddr], port: u16, ipv6: bool) -> io::Result<Vec<TcpListener>> {
    let candidates: Vec<IpAddr> = if addrs.is_empty() {
        if ipv6 {
            vec![
                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ]
        } else {
            vec![IpAddr::V4(Ipv4Addr::UNSPECIFIED)]
        }
    } else {
        addrs.to_vec()
    };
    let auto = addrs.is_empty();
    let mut out: Vec<TcpListener> = Vec::new();
    let mut port = port;
    let mut first_err = None;
    for ip in candidates {
        match TcpListener::bind(SocketAddr::new(ip, port)) {
            Ok(l) => {
                if port == 0 {
                    port = l.local_addr().map(|a| a.port()).unwrap_or(0);
                }
                out.push(l);
            }
            // `[::]` がデュアルスタックで v4 も受けているときは `0.0.0.0` が使用中になる
            Err(e) if auto && e.kind() == io::ErrorKind::AddrInUse && !out.is_empty() => {
                log_debug!(None, "{} already covered by the dual-stack socket", ip);
            }
            Err(e) if auto && ip.is_ipv6() => {
                log_debug!(
                    None,
                    "IPv6 listener unavailable ({}), falling back to IPv4",
                    e
                );
                first_err.get_or_insert(e);
            }
            Err(e) if auto => {
                log_warn!(None, "failed to bind {}:{}: {}", ip, port, e);
                first_err.get_or_insert(e);
            }
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("bind {}:{}: {}", ip, port, e),
                ));
            }
        }
    }
    if out.is_empty() {
        return Err(first_err.unwrap_or_else(|| io::Error::other("no listener could be bound")));
    }
    Ok(out)
}

/// 待ち受けアドレスの表示用文字列 (`[::]:8080 (dual-stack)` など)。
pub fn describe_listener(l: &TcpListener) -> String {
    match l.local_addr() {
        Ok(a) if a.ip() == IpAddr::V6(Ipv6Addr::UNSPECIFIED) => format!("{} (IPv6 + IPv4)", a),
        Ok(a) => a.to_string(),
        Err(_) => "?".to_string(),
    }
}

/// 名前解決の結果を IPv6 優先で交互に並べる (RFC 8305 §4)。
pub fn interleave(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    interleave_from(addrs, true)
}

/// [`interleave`] の、**先頭に置く族だけ**を選べる版 (RFC 8305 §8)。交互に並べること自体も、
/// 同じ族の中の順も変えない。入れ替えるのは「どちらを先に置くか」だけ。
pub fn interleave_from(addrs: Vec<SocketAddr>, v6_first: bool) -> Vec<SocketAddr> {
    let (v6, v4): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|a| a.is_ipv6());
    let mut out = Vec::with_capacity(v6.len() + v4.len());
    let (mut a, mut b) = if v6_first {
        (v6.into_iter(), v4.into_iter())
    } else {
        (v4.into_iter(), v6.into_iter())
    };
    loop {
        match (a.next(), b.next()) {
            (None, None) => break,
            (x, y) => {
                out.extend(x);
                out.extend(y);
            }
        }
    }
    out
}

/// 上流ソケットの Nagle を切る (小さな応答が delayed ACK 待ちで止まらないように)。失敗は無視。
fn nodelay(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
}

/// 1 つのアドレスへ接続する。`timeout` が `None` なら締め切り無し (OS 既定に任せる)。
fn connect_one(addr: &SocketAddr, timeout: Option<Duration>) -> io::Result<TcpStream> {
    match timeout {
        Some(t) => TcpStream::connect_timeout(addr, t),
        None => TcpStream::connect(addr),
    }
}

/// 名前解決して接続する。IPv6 無効時は A レコードだけ、有効時は Happy Eyeballs。全体の締め切りは `timeout`
/// (`Duration::ZERO` は無期限 = OS 既定の接続タイムアウトに任せる)。
pub fn connect(addr_str: &str, timeout: Duration) -> io::Result<TcpStream> {
    // 解決と一緒に「最後に勝った族」も受け取る (表を引くのは 1 回だけ。T12.1)
    let (resolved, preferred) = crate::dns::resolve_with_pref(addr_str)?;
    let ipv6 = ipv6_enabled();
    let addrs: Vec<SocketAddr> = if ipv6 {
        resolved
    } else {
        resolved.into_iter().filter(|a| a.is_ipv4()).collect()
    };
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            if ipv6 {
                "Could not resolve host"
            } else {
                "host has no IPv4 address (IPv6 is disabled; set PROXY_IPV6=on)"
            },
        ));
    }
    let (host, _) = split_host_port_ref(addr_str);
    connect_candidates(host, addrs, preferred, timeout)
}

/// 解決済みのアドレス列に Happy Eyeballs で接続する (`host` はホストごとの記憶の鍵。空なら覚えない)。
pub fn connect_resolved(
    host: &str,
    addrs: Vec<SocketAddr>,
    timeout: Duration,
) -> io::Result<TcpStream> {
    // 候補が 1 つなら並べ替えも記憶も要らない (熱い経路。ここでは鍵を取らない)
    let preferred = (addrs.len() > 1)
        .then(|| crate::dns::preferred_family(host))
        .flatten();
    connect_candidates(host, addrs, preferred, timeout)
}

/// 「IPv6 が負けた」の定義 (T12.1):
///
/// **IPv6 の候補を試し始めたのに、最後に確立できたのが IPv4 の候補だったとき**を 1 回の負けと数える。
/// これは 2 つの場合をまとめたもので、どちらも「IPv6 を先頭に置いたせいで `STAGGER` (250 ms) 待たされた」
/// という同じ結果になるので区別しない:
///
/// - IPv6 の試行が先に失敗し、そのあと IPv4 が勝った (デプロイ先の「経路はあるが約 1 秒で失敗」)
/// - IPv6 の試行が `STAGGER` を過ぎても返らないうちに IPv4 が勝った (SYN が黙って落ちる網)
///
/// **どの候補も確立できなかったときは勝ちにも負けにも数えない** (IPv6 の善し悪しではなく、
/// 宛先か網が落ちているだけなので)。勝敗を書くのは確立した時点で 1 回だけ。
fn note_ipv6_win() {
    IPV6_WINS.fetch_add(1, Ordering::Relaxed);
    // 定常状態 (毎回 IPv6 が勝つ) では store を増やさない
    if IPV6_LOSS_STREAK.load(Ordering::Relaxed) != 0 {
        IPV6_LOSS_STREAK.store(0, Ordering::Relaxed);
    }
}

fn note_ipv6_loss() {
    IPV6_LOSSES.fetch_add(1, Ordering::Relaxed);
    let streak = IPV6_LOSS_STREAK.fetch_add(1, Ordering::Relaxed) + 1;
    if streak == IPV6_LOSS_LIMIT && IPV6_WINS.load(Ordering::Relaxed) == 0 {
        IPV6_PROBE_AT.store(
            crate::clock::now_epoch() + IPV6_PROBE_SECS,
            Ordering::Relaxed,
        );
        if !IPV6_WARNED.swap(true, Ordering::Relaxed) {
            log_warn!(
                None,
                "IPv6 attempts never succeed; trying IPv4 first (set PROXY_IPV6=off to skip IPv6)"
            );
        }
    }
}

/// 起動から 1 度も IPv6 が勝たず、連続 [`IPV6_LOSS_LIMIT`] 回負けている状態か。
pub fn ipv6_v4_first() -> bool {
    IPV6_WINS.load(Ordering::Relaxed) == 0
        && IPV6_LOSS_STREAK.load(Ordering::Relaxed) >= IPV6_LOSS_LIMIT
}

/// 記憶の無いホストで IPv6 を先頭に置くか。IPv4 を先頭にしている間も
/// [`IPV6_PROBE_SECS`] に 1 回だけ `true` を返して IPv6 を試す (勝てば解除される)。
fn ipv6_first_for_new_host() -> bool {
    if !ipv6_v4_first() {
        return true;
    }
    let now = crate::clock::now_epoch();
    let at = IPV6_PROBE_AT.load(Ordering::Relaxed);
    now >= at
        && IPV6_PROBE_AT
            .compare_exchange(
                at,
                now + IPV6_PROBE_SECS,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
}

/// `/status` の `"ipv6"` 要素 (組み立ては `/status` のときだけ)。
pub fn ipv6_status_json() -> String {
    format!(
        "{{\"attempts\":{},\"wins\":{},\"losses\":{},\"v4_first\":{}}}",
        IPV6_ATTEMPTS.load(Ordering::Relaxed),
        IPV6_WINS.load(Ordering::Relaxed),
        IPV6_LOSSES.load(Ordering::Relaxed),
        ipv6_v4_first(),
    )
}

/// Prometheus 用のカウンタ (attempts, wins, losses)。
pub fn ipv6_counters() -> [u64; 3] {
    [
        IPV6_ATTEMPTS.load(Ordering::Relaxed),
        IPV6_WINS.load(Ordering::Relaxed),
        IPV6_LOSSES.load(Ordering::Relaxed),
    ]
}

/// 候補列に Happy Eyeballs で接続する本体。
///
/// `timeout` が `Duration::ZERO` なら**締め切りを置かない** (`PROXY_TIMEOUT_SECS=0` = 無期限。
/// T10.6)。`TcpStream::connect_timeout` は 0 を `InvalidInput` で断るので、そのときは
/// 素の `connect` を使い、OS 既定の接続タイムアウト (Linux はおよそ 130 秒) に任せる。
///
/// `preferred` はこのホストで最後に勝った族。`None` のときは全体の状態で決める
/// (IPv6 が 1 度も勝っていなければ IPv4 を先頭に。T12.1)。
fn connect_candidates(
    host: &str,
    addrs: Vec<SocketAddr>,
    preferred: Option<bool>,
    timeout: Duration,
) -> io::Result<TcpStream> {
    if addrs.len() == 1 {
        // 候補が 1 つなら Happy Eyeballs は要らない (この経路は T12.1 でも変えていない)
        let v6 = addrs[0].is_ipv6();
        return connect_one(&addrs[0], proxy_base::timeout::for_socket(timeout)).inspect(|s| {
            nodelay(s);
            // 確立した族を控える (thread-local への書き込み 1 回。T12.4 (2))
            crate::dns::note_family(v6);
        });
    }

    let addrs = interleave_from(addrs, preferred.unwrap_or_else(ipv6_first_for_new_host));
    let deadline = (!timeout.is_zero()).then(|| Instant::now() + timeout);
    let (tx, rx) = mpsc::channel::<(usize, io::Result<TcpStream>)>();
    let mut launched = 0usize;
    let mut pending = 0usize;
    let mut tried_v6 = false;
    let mut next_launch = Instant::now();
    let mut last_err: Option<io::Error> = None;

    loop {
        let now = Instant::now();
        if launched < addrs.len() && (pending == 0 || now >= next_launch) {
            let addr = addrs[launched];
            let index = launched;
            let tx = tx.clone();
            let remaining = deadline.map(|d| {
                d.saturating_duration_since(now)
                    .max(Duration::from_millis(1))
            });
            if addr.is_ipv6() && !tried_v6 {
                tried_v6 = true;
                IPV6_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
            }
            thread::spawn(move || {
                let _ = tx.send((index, connect_one(&addr, remaining)));
            });
            launched += 1;
            pending += 1;
            next_launch = now + STAGGER;
        }
        // 締め切りが無いときは、まだ launch していないぶんの間隔だけ待ち、
        // 全部 launch したあとは結果が出るまで待つ (無期限)
        let wait = if launched < addrs.len() {
            Some(next_launch.saturating_duration_since(now))
        } else {
            deadline.map(|d| d.saturating_duration_since(now))
        };
        let got = match wait {
            Some(w) => rx.recv_timeout(w),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match got {
            Ok((index, Ok(stream))) => {
                nodelay(&stream);
                let won_v6 = addrs[index].is_ipv6();
                // ホスト別の内訳 (`v4_wins` / `v6_wins`) に使う (T12.4 (2))
                crate::dns::note_family(won_v6);
                if tried_v6 {
                    if won_v6 {
                        note_ipv6_win();
                    } else {
                        note_ipv6_loss();
                    }
                }
                // ホストごとの記憶は**答えが変わるときだけ**書く (毎回書くと鍵を取ることになる)
                if preferred != Some(won_v6) {
                    crate::dns::remember_family(host, won_v6);
                }
                return Ok(stream);
            }
            Ok((_, Err(e))) => {
                pending -= 1;
                last_err = Some(e);
                if pending == 0 && launched == addrs.len() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::TimedOut, "connection attempts timed out")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_host_and_port_forms() {
        assert_eq!(
            split_host_port("example.com:8080"),
            ("example.com".into(), Some(8080))
        );
        assert_eq!(split_host_port("example.com"), ("example.com".into(), None));
        assert_eq!(
            split_host_port("[2001:db8::1]:443"),
            ("2001:db8::1".into(), Some(443))
        );
        assert_eq!(
            split_host_port("[2001:db8::1]"),
            ("2001:db8::1".into(), None)
        );
        assert_eq!(split_host_port("2001:db8::1"), ("2001:db8::1".into(), None));
        assert_eq!(
            split_host_port("host:notaport"),
            ("host:notaport".into(), None)
        );
        assert_eq!(with_default_port("example.com", 80), "example.com:80");
        assert_eq!(with_default_port("[::1]", 80), "[::1]:80");
        assert_eq!(with_default_port("::1", 443), "[::1]:443");
        assert_eq!(with_default_port("[::1]:8080", 80), "[::1]:8080");
        assert_eq!(join_host_port("1.2.3.4", 1), "1.2.3.4:1");
    }

    #[test]
    fn canonicalizes_mapped_addresses() {
        let mapped: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
        assert_eq!(canonical_ip(mapped), "192.0.2.1".parse::<IpAddr>().unwrap());
        let real: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(canonical_ip(real), real);
        let sa: SocketAddr = "[::ffff:10.0.0.1]:5".parse().unwrap();
        assert_eq!(canonical_addr(sa).to_string(), "10.0.0.1:5");
    }

    #[test]
    fn interleaves_v6_first() {
        let v4a: SocketAddr = "192.0.2.1:80".parse().unwrap();
        let v4b: SocketAddr = "192.0.2.2:80".parse().unwrap();
        let v6a: SocketAddr = "[2001:db8::1]:80".parse().unwrap();
        assert_eq!(interleave(vec![v4a, v4b, v6a]), vec![v6a, v4a, v4b]);
        assert_eq!(interleave(vec![v4a]), vec![v4a]);
        // 先頭に置く族を入れ替えても、同じ族の中の順は変わらない (T12.1)
        assert_eq!(
            interleave_from(vec![v4a, v4b, v6a], false),
            vec![v4a, v6a, v4b]
        );
        assert_eq!(interleave_from(vec![v6a], false), vec![v6a]);
    }

    #[test]
    fn binds_dual_stack_or_falls_back() {
        assert!(ipv6_enabled(), "on by default");
        let v4 = bind_all_with(&[], 0, false).expect("v4-only listener");
        assert_eq!(v4.len(), 1);
        assert!(v4[0].local_addr().unwrap().ip().is_ipv4());
        let listeners = bind_all(&[], 0).expect("at least one listener");
        assert!(!listeners.is_empty() && listeners.len() <= 2);
        let port = listeners[0].local_addr().unwrap().port();
        assert!(
            listeners
                .iter()
                .all(|l| l.local_addr().unwrap().port() == port)
        );
        // 明示指定なら指定どおり
        let explicit = bind_all(&[IpAddr::V4(Ipv4Addr::LOCALHOST)], 0).unwrap();
        assert_eq!(explicit.len(), 1);
        assert!(explicit[0].local_addr().unwrap().ip().is_loopback());
    }

    /// IPv6 を絡めるテストは**全体の状態 (連敗と勝敗の数) を共有する**ので直列に回す。
    static IPV6_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 起動直後の状態に戻す (テスト同士が互いの連敗を見ないように)。ホストごとの記憶は
    /// 消さない (テストごとに違うホスト名を使っている。`dns::clear()` は同じ表を使う
    /// `dns` のテストと衝突する)。
    fn reset_ipv6_state() {
        for c in [&IPV6_ATTEMPTS, &IPV6_WINS, &IPV6_LOSSES, &IPV6_LOSS_STREAK] {
            c.store(0, Ordering::Relaxed);
        }
        IPV6_PROBE_AT.store(0, Ordering::Relaxed);
        IPV6_WARNED.store(false, Ordering::Relaxed);
    }

    /// `[::1]` の黒穴 (backlog 0 の待ち受けを 1 本で埋める) と、生きている `127.0.0.1` の 2 候補。
    ///
    /// `listen(fd, 0)` にすると受け入れ待ち行列が 1 本で埋まり、以後の SYN は黙って捨てられる
    /// (`tcp_abort_on_overflow = 0` の既定)。次の `connect` は 1.5 秒以上返らないので、
    /// デプロイ先の「IPv6 は経路があるのに繋がらない」を手元で再現できる (2026-09-10 に確認)。
    #[cfg(target_os = "linux")]
    fn blackhole_v6_and_live_v4() -> Option<(TcpListener, TcpStream, TcpListener, Vec<SocketAddr>)>
    {
        use std::os::fd::AsRawFd;
        // `libc` は使わない (§0)。`listen(2)` だけ直接宣言する
        unsafe extern "C" {
            fn listen(fd: i32, backlog: i32) -> i32;
        }
        let hole = TcpListener::bind("[::1]:0").ok()?;
        if unsafe { listen(hole.as_raw_fd(), 0) } != 0 {
            return None;
        }
        let hole_addr = hole.local_addr().ok()?;
        // 1 本つないで待ち行列を埋める (accept しない)
        let filler = TcpStream::connect_timeout(&hole_addr, Duration::from_secs(1)).ok()?;
        let live = TcpListener::bind("127.0.0.1:0").ok()?;
        let live_addr = live.local_addr().ok()?;
        Some((hole, filler, live, vec![hole_addr, live_addr]))
    }

    /// T12.1 の受け入れ基準 (手元): 黒穴 `[::1]` + 生きている `127.0.0.1` で
    /// **1 回目 ≥ 250 ms (設計どおり `STAGGER` を待つ)、同じホストの 2 回目 < 50 ms**。
    #[cfg(target_os = "linux")]
    #[test]
    fn happy_eyeballs_skips_unreachable_first_candidate() {
        let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_ipv6_state();
        let Some((_hole, _filler, live, addrs)) = blackhole_v6_and_live_v4() else {
            eprintln!("no IPv6 loopback; skipping");
            return;
        };
        let port = live.local_addr().unwrap().port();
        let host = "t121-learn.invalid";

        let started = Instant::now();
        let stream =
            connect_resolved(host, addrs.clone(), Duration::from_secs(5)).expect("v4 should win");
        let first = started.elapsed();
        assert_eq!(stream.peer_addr().unwrap().port(), port);
        assert!(
            first >= STAGGER && first < Duration::from_secs(3),
            "1 回目は STAGGER を待って IPv4 で確立するはず: {:?}",
            first
        );
        assert_eq!(crate::dns::preferred_family(host), Some(false));

        let started = Instant::now();
        let stream = connect_resolved(host, addrs, Duration::from_secs(5)).expect("v4 should win");
        let second = started.elapsed();
        assert_eq!(stream.peer_addr().unwrap().port(), port);
        assert!(
            second < Duration::from_millis(50),
            "2 回目は覚えた族 (IPv4) を先頭にするので待たないはず: {:?}",
            second
        );
        assert!(
            ipv6_status_json().contains("\"losses\":1"),
            "{}",
            ipv6_status_json()
        );
    }

    /// 全体の記憶: 連続 3 回負けたら**初めて見るホストも** IPv4 を先頭にする (T12.1 の 2)。
    #[cfg(target_os = "linux")]
    #[test]
    fn unseen_hosts_try_ipv4_first_after_three_losses() {
        let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_ipv6_state();
        let Some((_hole, _filler, live, addrs)) = blackhole_v6_and_live_v4() else {
            eprintln!("no IPv6 loopback; skipping");
            return;
        };
        let port = live.local_addr().unwrap().port();
        for i in 0..IPV6_LOSS_LIMIT {
            let host = format!("t121-loser-{}.invalid", i);
            let started = Instant::now();
            connect_resolved(&host, addrs.clone(), Duration::from_secs(5)).expect("v4 should win");
            assert!(started.elapsed() >= STAGGER, "{} 回目: 待つはず", i);
        }
        assert!(ipv6_v4_first(), "{}", ipv6_status_json());

        // 初めて見るホスト (記憶なし) でも待たない
        let started = Instant::now();
        let stream = connect_resolved("t121-fresh.invalid", addrs, Duration::from_secs(5))
            .expect("v4 should win");
        let elapsed = started.elapsed();
        assert_eq!(stream.peer_addr().unwrap().port(), port);
        assert!(
            elapsed < Duration::from_millis(50),
            "3 回負けたあとは初めて見るホストも IPv4 が先頭のはず: {:?}",
            elapsed
        );
        let [attempts, wins, losses] = ipv6_counters();
        assert_eq!(
            (attempts, wins, losses),
            (IPV6_LOSS_LIMIT, 0, IPV6_LOSS_LIMIT)
        );
        // 4 本目で attempts が増えていないのは、IPv4 が先頭で即勝ったから
        // (IPv6 の候補は残っているが起動されない = 負けた試行のスレッドと fd も残らない。T12.2)

        // IPv6 をやめたわけではない: IPv4 が死んでいれば IPv6 で拾い、勝った時点で IPv4 優先は解ける
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);
        let v6 = TcpListener::bind("[::1]:0").unwrap();
        let stream = connect_resolved(
            "t121-v4dead.invalid",
            vec![dead_addr, v6.local_addr().unwrap()],
            Duration::from_secs(5),
        )
        .expect("IPv6 should pick it up");
        assert!(stream.peer_addr().unwrap().is_ipv6());
        assert!(!ipv6_v4_first(), "1 度勝ったら解除: {}", ipv6_status_json());
    }

    /// IPv6 が生きている条件では今までどおり IPv6 が勝ち、IPv4 優先には落ちない。
    #[cfg(target_os = "linux")]
    #[test]
    fn ipv6_still_wins_when_it_works() {
        let _guard = IPV6_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_ipv6_state();
        let Ok(v6) = TcpListener::bind("[::1]:0") else {
            eprintln!("no IPv6 loopback; skipping");
            return;
        };
        let v4 = TcpListener::bind("127.0.0.1:0").unwrap();
        let addrs = vec![v6.local_addr().unwrap(), v4.local_addr().unwrap()];
        let host = "t121-v6ok.invalid";
        let stream = connect_resolved(host, addrs.clone(), Duration::from_secs(5)).unwrap();
        assert!(stream.peer_addr().unwrap().is_ipv6(), "IPv6 が勝つはず");
        assert_eq!(crate::dns::preferred_family(host), Some(true));
        let stream = connect_resolved(host, addrs, Duration::from_secs(5)).unwrap();
        assert!(stream.peer_addr().unwrap().is_ipv6(), "2 回目も IPv6");
        assert!(!ipv6_v4_first());
        assert_eq!(ipv6_counters(), [2, 2, 0]);
    }

    /// Linux 以外は黒穴 (`listen(fd, 0)`) を作れないので、到達不能アドレスで従来どおり見る。
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn happy_eyeballs_skips_unreachable_first_candidate() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // 到達不能 (TEST-NET-1) → ループバックの順で試させる
        let addrs = vec![
            SocketAddr::new("192.0.2.1".parse().unwrap(), 9),
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
        ];
        let started = Instant::now();
        let stream = connect_resolved("t121-unreachable.invalid", addrs, Duration::from_secs(5))
            .expect("loopback should win");
        assert_eq!(stream.peer_addr().unwrap().port(), port);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "did not wait for the dead address"
        );
    }

    #[test]
    fn connect_reports_error_when_all_fail() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let err = connect(&format!("127.0.0.1:{}", port), Duration::from_secs(2)).unwrap_err();
        assert!(matches!(
            err.kind(),
            io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
        ));
    }
}
