//! 1 回の試行: **名前解決 (表を通さない) → TCP 接続 → 即 `close`** と、IPv6 側だけの 1 本。

use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::metrics::ErrCause;

use super::{DEADLINE, DEFAULT_PORT, IPV6_RUNS, IPV6_SKIPPED, MAX_ERROR, ipv6_enabled};

/// 1 回の試行の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// いつ (epoch 秒)
    pub at: u64,
    /// 相手 (`host:port`)
    pub host: String,
    /// 名前解決にかかった ms (IP リテラルなら 0)
    pub dns_ms: u64,
    /// TCP 接続 (SYN → 確立) にかかった ms。名前解決で失敗したら 0
    pub connect_ms: u64,
    /// IPv6 側だけの 1 本にかかった ms (T14.37)。**繋がらなければ `null`**
    /// (AAAA が無い名前、IPv6 を切ってある、経路が黒穴、`PROXY_CANARY_IPV6=off`)
    pub ipv6_connect_ms: Option<u64>,
    /// 失敗の理由 (成功なら `None`)
    pub error: Option<String>,
    /// 失敗の原因 (利用者のエラーと同じ物差し。成功なら `None`)
    pub cause: Option<ErrCause>,
}

impl Probe {
    /// `/status` の `canary` の中身 (波括弧なし)。
    pub(super) fn push_json(&self, out: &mut String) {
        let _ = write!(
            out,
            "\"at\":{},\"host\":\"{}\",\"dns_ms\":{},\"connect_ms\":{},\"ipv6_connect_ms\":{},\"error\":",
            self.at,
            crate::json::escape(&self.host),
            self.dns_ms,
            self.connect_ms,
            num_or_null(self.ipv6_connect_ms),
        );
        match &self.error {
            Some(e) => {
                let _ = write!(out, "\"{}\"", crate::json::escape(e));
            }
            None => out.push_str("null"),
        }
    }
}

/// 1 ホストを測る: **名前解決 (表を通さない) → TCP 接続 → 即 `close`**。
pub(super) fn probe(target: &str) -> Probe {
    let at = crate::cache::now_epoch();
    let (host, port) = crate::net::split_host_port_ref(target);
    let port = port.unwrap_or(DEFAULT_PORT);
    let started = Instant::now();
    let addrs = match crate::dns::resolve_uncached(host, port) {
        Ok(a) => a,
        Err(e) => {
            return Probe {
                at,
                host: target.to_string(),
                dns_ms: ms(started.elapsed()),
                connect_ms: 0,
                // 名前が引けなければ AAAA も無い (試さなかった周に数える。T18.1)
                ipv6_connect_ms: probe_ipv6(None),
                error: Some(clip(&e.to_string())),
                // 名前解決で終わったのだから原因は `dns` (文言から当てない)
                cause: Some(ErrCause::Dns),
            };
        }
    };
    let dns_ms = ms(started.elapsed());
    let (v6, addrs) = split_addrs(addrs, port, crate::net::ipv6_enabled());
    if addrs.is_empty() {
        return Probe {
            at,
            host: target.to_string(),
            dns_ms,
            connect_ms: 0,
            // ここに来るのは `PROXY_IPV6=off` で AAAA しか無い名前 (利用者も繋げない)。
            // `probe_ipv6` は `PROXY_IPV6=off` では試さないので、数えるだけで `None` が返る (T18.1)
            ipv6_connect_ms: probe_ipv6(v6),
            error: Some("no address for this family".to_string()),
            cause: Some(ErrCause::Dns),
        };
    }
    let started = Instant::now();
    // 握れたら**その場で捨てる** (`drop` = FIN)。TLS も HTTP も送らない
    let (error, cause) = match crate::net::connect_resolved(host, addrs, DEADLINE) {
        Ok(_stream) => (None, None),
        // 原因は `io::Error` から決める (利用者のエラーと同じ `refused` / `timeout` …)
        Err(e) => (Some(clip(&e.to_string())), Some(ErrCause::from_io(&e))),
    };
    let connect_ms = ms(started.elapsed());
    Probe {
        at,
        host: target.to_string(),
        dns_ms,
        connect_ms,
        ipv6_connect_ms: probe_ipv6(v6),
        error,
        cause,
    }
}

/// 引けた答えを (IPv6 側だけの 1 本の相手, 本体の 1 本で試す宛先) に分ける (T34.45)。
///
/// IPv6 側だけの 1 本の相手 (T14.37) は、**族で絞る前**に AAAA の先頭を控える。
/// IPv6 を切ってある (`ipv6 = false`) ときは、利用者の経路と同じく A レコードだけを試す。
pub(super) fn split_addrs(
    addrs: Vec<IpAddr>,
    port: u16,
    ipv6: bool,
) -> (Option<SocketAddr>, Vec<SocketAddr>) {
    let v6 = addrs
        .iter()
        .find(|ip| ip.is_ipv6())
        .map(|&ip| SocketAddr::new(ip, port));
    let addrs = addrs
        .into_iter()
        .filter(|ip| ipv6 || ip.is_ipv4())
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    (v6, addrs)
}

/// IPv6 側だけの 1 本 (T14.37)。繋がった ms、繋がらなければ `None`。
///
/// 繋ぐのは [`crate::net::connect_addr`] (Happy Eyeballs も勝敗の記録も族の記憶も通らない) で、
/// 失敗しても `/errors` には 1 件も残さない — デプロイ先の IPv6 は黒穴なので、
/// 個票に残すと 1 分に 1 件ずつ `/errors` が埋まってしまう。生死は
/// `/status` の `canary.ipv6_connect_ms` が `null` かどうかで読む。
///
/// **試した結果は [`crate::net::note_canary_ipv6`] で下の層に渡す** (T17.7): 試している間は
/// 利用者の経路が 600 秒に 1 回の探り (利用者を `stagger()` ぶん待たせる) をやめ、
/// 繋がったら `v4_first` が解ける。勝敗の数 (`ipv6.attempts` / `wins` / `losses`) には足さない。
///
/// **試さなかった回は印を更新しない** (試していないのに「探っている」とは言えない)。
/// 試したかどうかは `/status` の `canary.ipv6_runs` / `ipv6_skipped` に数える (T18.1):
/// [`probe`] の 1 回につき、必ずここを 1 回通ってどちらかが 1 増える。
fn probe_ipv6(addr: Option<SocketAddr>) -> Option<u64> {
    // `PROXY_IPV6=off` のときは利用者も AAAA を使わないので測らない
    let Some(addr) = addr.filter(|_| ipv6_enabled() && crate::net::ipv6_enabled()) else {
        IPV6_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    IPV6_RUNS.fetch_add(1, Ordering::Relaxed);
    let started = Instant::now();
    // 握れたら**その場で捨てる** (`drop` = FIN)。TLS も HTTP も送らない
    let connected = crate::net::connect_addr(&addr, DEADLINE).is_ok();
    crate::net::note_canary_ipv6(connected);
    connected.then(|| ms(started.elapsed()))
}

/// JSON の数 (繋がらなかった IPv6 側は `null`。T14.37)。
fn num_or_null(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

/// ms に丸める (0.5 ms 以上は 1 ms。手元の loopback は 0 ms になる)。
fn ms(d: Duration) -> u64 {
    (d.as_micros() as u64 + 500) / 1000
}

fn clip(s: &str) -> String {
    crate::recent::clip(s, MAX_ERROR)
}
