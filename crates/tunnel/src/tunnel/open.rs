//! 宛先へつなぎ、`200` と先読みぶんを送る ([`open`])。つなげなければ 502 を書き、
//! WARN と統計まで出して [`Reported`] で包んだ失敗を返す。
use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{TunnelRequest, connect_with_timeout};
use crate::dns;
use crate::log::{Access, access};
use crate::metrics::{Detail, Failure, HostOutcome, Metrics, StageMs};
use crate::net;
use crate::recent::{CloseReason, ConnSlot, ConnTally, STAGE_CONNECT, STAGE_DNS, STAGES};
use crate::{log_debug, log_warn};

/// 「502 を書いて WARN と統計まで出し終えた」失敗の印 (T28.4)。
///
/// [`open`] が繋げなかったときの `io::Error` をこれで包んで返す (種類 (`kind`) と文面は
/// 元のまま)。呼び元 (`run_conn`) は [`is_reported`] で見分け、ERROR で出し直さない。
/// 今までは同じ失敗が WARN (宛先つき) と ERROR (宛先無し) の 2 行になり、
/// tokyo の `/log` 1,000 行の半分がその ERROR だった。
#[derive(Debug)]
pub(super) struct Reported(pub(super) io::Error);

impl std::fmt::Display for Reported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Reported {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// tunnel がもう報告した失敗か (502 を書き、WARN と `/errors` の個票まで出してある。T28.4)。
pub fn is_reported(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Reported>())
}

/// 宛先へつないで `200 Connection Established` と先読みぶんを送ったところ。
pub(super) struct Opened {
    pub(super) client: TcpStream,
    pub(super) server: TcpStream,
    pub(super) info: Info,
}

/// トンネル 1 本の素性 (アクセスログと統計に要るもの)。ソケットとは別に持ち運ぶ。
pub(super) struct Info {
    pub(super) conn_id: usize,
    /// `host:port` (`connect://` を付けてホスト別統計の鍵にする)
    pub(super) addr_str: String,
    pub(super) client_ip: String,
    /// CONNECT を受けた時刻 (アクセスログの所要時間はここから測る)
    pub(super) started: Instant,
    /// ホスト別の応答時間は接続確立まで (トンネル自体の寿命は応答時間ではない)
    pub(super) connect_took: Duration,
    /// 確立までの内訳 (名前解決 / 接続 / 勝った族。T12.4 (2)) と段階 (T14.3 (1))
    pub(super) detail: Detail,
    /// `200 Connection Established` を書き終えた時刻 (`first_relay` の起点。T14.3 (1))。
    /// `--lite` では `None` = 時計を読まない
    pub(super) established: Option<Instant>,
    pub(super) metrics: Arc<Metrics>,
    /// `/connections` の枠 (T13.4)。状態と運んだバイト数をここに書く。
    /// 登録と抹消は本体クレート (接続の開始と終了) の仕事で、ここは書くだけ
    pub(super) slot: Option<Arc<ConnSlot>>,
    /// この接続がトンネルになるまでに預かり所にいた ms (原子の読み 1 回。T14.25)。
    ///
    /// 枠の預かり秒は**接続**のもので、HTTP の keep-alive で預けられていた分も入って
    /// いる。中継の時間から引くのは**トンネルになってからの預け**だけなので、
    /// ここで印を取って差だけを使う (同じ接続で `GET` のあとに `CONNECT` が来る場合)
    pub(super) parked_ms_at_start: u64,
    /// 覗いた SNI が CONNECT のホストと食い違ったか (`PROXY_PEEK_SNI`。T14.38)。
    ///
    /// 名前そのものは覗いたその場で `/connections` の枠 ([`ConnSlot::set_sni`]) へ
    /// 書くので、ここに持つのは統計に足す旗 1 つだけ (`report` が `Detail` に載せる)
    pub(super) sni_mismatch: bool,
}

/// 宛先へつなぎ、`200` と先読みぶん (`prefix`) を送る。
/// つなげなければ 502 を書き、ログと統計を出して `Err` ([`Reported`] で包む = 呼び元は出し直さない)。
#[inline]
pub(super) fn open(req: TunnelRequest<'_>) -> io::Result<Opened> {
    let TunnelRequest {
        mut client,
        target,
        prefix,
        timeout,
        conn_id,
        metrics,
        client_ip,
        resolved,
        slot,
        mut stages,
        read_started,
        ..
    } = req;
    let started = Instant::now();
    // この時計より**前に**払われた名前解決 (入口の ACL。`PROXY_ALLOW_LOCAL=false` が
    // 既定なので普通はここに入る) を控える (T15.0 (1))。thread-local の読み 1 回で、
    // 時計もシステムコールも増えない。使い道は下の [`Detail::of_connect`] の引き算だけ
    let dns_before = dns::peek_resolve_cost().0;
    // `client_read` は「要求行が届いてからここまで」(入口で読んだ時計をそのまま使うので、
    // 1 本あたりの時計は増えない。T14.3 (1))
    if let Some(t) = read_started {
        stages.client_read = crate::profile::ms_u32(started.saturating_duration_since(t));
    }
    let addr_str = net::with_default_port(target, 443);

    log_debug!(Some(conn_id), "start CONNECT {}", addr_str);

    let mut server = match connect_with_timeout(&addr_str, resolved, timeout) {
        Ok(s) => s,
        Err(e) => {
            let attempt = Attempt {
                conn_id,
                addr_str: &addr_str,
                client_ip: &client_ip,
                metrics: &metrics,
                slot: slot.as_ref(),
                started,
            };
            refuse_connect(&mut client, &e, &attempt, stages, dns_before);
            // WARN と統計はここで出し終えた。呼び元が ERROR で出し直さないように印を付ける
            return Err(io::Error::new(e.kind(), Reported(e)));
        }
    };

    // ホスト別の応答時間は接続確立まで (トンネル自体の寿命は応答時間ではない)
    let connect_took = started.elapsed();
    let detail = Detail::of_connect(connect_took, None, stages, dns_before);
    // `/connections` の 1 行を「CONNECT の中継中」にする (1 本につき 1 回だけ。T13.4)
    if let Some(s) = &slot {
        s.begin_tunnel(&addr_str);
    }
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    client.flush()?;
    // トンネル越しの TLS 握手の往復を測る起点 (T14.3 (1))。`--lite` では読まない
    let established = crate::profile::mark();
    if !prefix.is_empty() {
        server.write_all(prefix)?;
    }
    Ok(Opened {
        client,
        server,
        info: Info {
            conn_id,
            addr_str,
            client_ip,
            started,
            connect_took,
            detail,
            established,
            metrics,
            parked_ms_at_start: slot.as_ref().map_or(0, |s| s.parked_ms()),
            slot,
            sni_mismatch: false,
        },
    })
}

/// 繋ぎに行った 1 本の素性 (失敗の報告に要るもの。繋がれば [`Info`] へ移る)。
struct Attempt<'a> {
    conn_id: usize,
    /// `host:port`
    addr_str: &'a str,
    client_ip: &'a str,
    metrics: &'a Metrics,
    slot: Option<&'a Arc<ConnSlot>>,
    /// CONNECT を受けた時刻 (アクセスログの所要時間はここから測る)
    started: Instant,
}

/// 繋げなかった CONNECT を断る: 502 (名前解決の締め切りと、続けて時間切れの宛先を待たずに断ったときは 504) を書き、WARN と統計・
/// `/errors` と `/recent` の個票・アクセスログを出す (呼び元は [`Reported`] で包んで返す)。
fn refuse_connect(
    client: &mut TcpStream,
    e: &io::Error,
    a: &Attempt<'_>,
    stages: StageMs,
    dns_before: u64,
) {
    // 名前解決が締め切りを過ぎた (または上限で断った) ときは 504 (T32.11)。
    // 確立が続けて時間切れになった宛先を、繋ぎに行かずに断ったときも 504 (T36.8)
    let (code, reason): (u16, &str) = if dns::is_timeout(e) || net::is_skipped(e) {
        (504, "Gateway Timeout")
    } else {
        (502, "Bad Gateway")
    };
    // 理由ごとに 1 分に 1 行 (T29.15。本数は `/errors` と `/hosts` が持つ)
    if let Some(more) = a.metrics.origin_warn(Failure::from_io(e)) {
        log_warn!(
            Some(a.conn_id),
            "{} {}: connect {} failed: {}{}",
            code,
            reason,
            a.addr_str,
            e,
            more
        );
    }
    // `Date` と `Content-Length: 0` も付ける (RFC 9110 §6.6.1・§8.6。T32.15 (5)。T29.9 の漏れ)
    let mut head = [0u8; crate::httpdate::ERROR_HEAD_MAX];
    let _ = client.write_all(crate::httpdate::error_head(code, reason, &mut head));
    let detail = Detail::of_connect(
        a.started.elapsed(),
        Some(Failure::from_io(e)),
        stages,
        dns_before,
    );
    // 読んだ壁時計は接続元の個票にも渡す (要求 1 本で 1 回。T34.146)
    let now = a.metrics.record_host_detail(
        &format!("connect://{}", a.addr_str),
        HostOutcome::Error,
        0,
        Some(a.started.elapsed()),
        detail,
    );
    // 個票にも 1 件残す (`/errors`。誰の・いつ・なぜ。T13.4)
    a.metrics
        .record_error(true, a.addr_str, a.client_ip, code, &detail);
    // 閉じた接続の個票にも相手を残す (`/recent`。**エラーの経路だけ**。T14.4)
    if let Some(s) = a.slot {
        s.failed_tunnel(a.addr_str);
        let mut stage_ms = [0u64; STAGES];
        stage_ms[STAGE_DNS] = detail.dns_ms;
        stage_ms[STAGE_CONNECT] = detail.connect_ms;
        s.finish(
            CloseReason::error_from_io(e),
            ConnTally {
                up: 0,
                down: 0,
                status: code,
                stage_ms,
                // 繋がらなかったのでカーネルに聞ける相手がいない (T14.5)
                ..ConnTally::default()
            },
            0,
        );
    }
    a.metrics.record_client_at(
        a.client_ip,
        HostOutcome::Error,
        0,
        // 繋がらなかったので運んだバイトは上りも下りも無い (T14.26)
        (0, 0),
        Some(a.started.elapsed()),
        Some(a.addr_str),
        now,
    );
    access(
        a.conn_id,
        &Access {
            client: a.client_ip,
            method: "CONNECT",
            target: a.addr_str,
            version: "HTTP/1.1",
            status: &code.to_string(),
            bytes: 0,
            duration_ms: a.started.elapsed().as_secs_f64() * 1000.0,
            cache: "BYPASS",
        },
    );
}
