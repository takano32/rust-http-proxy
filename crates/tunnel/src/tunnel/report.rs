//! トンネルが終わったときのアクセスログと統計 ([`report`])。
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use super::open::Info;
use crate::log::{Access, access};
use crate::metrics::{Detail, HostOutcome};
use crate::recent::{
    CLIENT_SIDE, CloseReason, ConnSlot, ConnTally, ORIGIN_SIDE, SIDES, STAGE_CLIENT_READ,
    STAGE_CONNECT, STAGE_DNS, STAGE_FIRST_RELAY, STAGE_QUEUE, STAGES,
};

/// 両側のカーネルの RTT (us) と再送の通算を読む (`[クライアント側, オリジン側]`)。
///
/// **呼ぶのはトンネル 1 本の終わりだけ** (`getsockopt` 2 回)。読めなければ 0 で、
/// 個票では `null`、統計には足さない。Linux 以外はソケットを見ずに 0。
#[cfg(target_os = "linux")]
#[inline]
fn tcp_rtt(socks: Option<&[TcpStream; 2]>) -> ([u32; SIDES], [u32; SIDES]) {
    use std::os::fd::AsRawFd;
    let mut rtt = [0u32; SIDES];
    let mut retrans = [0u32; SIDES];
    if let Some(socks) = socks {
        for (i, s) in socks.iter().enumerate() {
            if let Some(info) = crate::sys::tcp_info(s.as_raw_fd()) {
                rtt[i] = info.rtt_us;
                retrans[i] = info.total_retrans;
            }
        }
    }
    (rtt, retrans)
}

#[cfg(not(target_os = "linux"))]
fn tcp_rtt(_socks: Option<&[TcpStream; 2]>) -> ([u32; SIDES], [u32; SIDES]) {
    ([0; SIDES], [0; SIDES])
}

/// トンネルが終わったときのアクセスログと統計 (どこで終わっても 1 回だけ通る)。
///
/// `up` はクライアント → 宛先、`down` は宛先 → クライアントのバイト数。
/// `reason` は閉じた理由 (`/recent`。T14.4) で、外から閉じられた (追い出し・監視の停止)
/// ときは枠に先に書いてある理由が勝つ。
pub(super) fn report(
    o: &Info,
    up: u64,
    down: u64,
    reason: CloseReason,
    socks: Option<&[TcpStream; 2]>,
    half_close: Option<Duration>,
    stall_ms: [u32; SIDES],
) {
    let transferred = up.saturating_add(down);
    // トンネルの寿命 (時計はここで 1 回だけ読み、アクセスログ・T14.3 の段階・
    // T14.25 の中継の時間で使い回す)
    let alive = o.started.elapsed();
    // 中継の合計 = 生きていた時間 − 確立まで − 預けられていた時間 (T14.3 (1))。
    // **時計は足さない** (アクセスログが読む `alive` をそのまま使う)
    let mut detail = o.detail;
    if crate::profile::on() {
        let total = alive.as_millis().min(u64::MAX as u128) as u64;
        let head =
            o.connect_took.as_millis().min(u64::MAX as u128) as u64 + detail.stages.park as u64;
        detail.stages.relay = total.saturating_sub(head).min(u32::MAX as u64) as u32;
    }
    // カーネルの RTT と再送を**両側 1 本ずつ** (`getsockopt` 2 回。トンネル 1 本の
    // 終わりだけで、中継のバイトごとにも要求ごとにも読まない。T14.5)
    let (rtt_us, retrans) = tcp_rtt(socks);
    // 閉じた接続の個票に 1 件ぶんの値を載せる (原子は接続の終わりのここだけ。T14.4)。
    // 実際にリングへ書くのは本体クレートの `ActiveGuard::drop` (= この直後)。
    // **段階は上で組み立てた `detail`** (T14.3 の `first_relay` まで入っている) から取る
    if let Some(s) = &o.slot {
        let end = Ending {
            up,
            down,
            transferred,
            alive,
            reason,
            half_close,
            stall_ms,
            rtt_us,
            retrans,
        };
        finish_slot(o, s, &detail, &end);
    }
    record_stats(o, detail, (up, down), transferred, rtt_us, retrans);
    log_access(o, transferred, alive);
}

/// トンネル 1 本の終わりに数えたもの ([`finish_slot`] に渡す。時計は `report` で 1 回だけ読む)。
///
/// 枠の無い `--lite` では作らない (枠の外の 2 段には値をそのまま渡す)。
struct Ending {
    /// クライアント → 宛先のバイト数
    up: u64,
    /// 宛先 → クライアントのバイト数
    down: u64,
    /// `up + down`
    transferred: u64,
    /// トンネルの寿命
    alive: Duration,
    reason: CloseReason,
    half_close: Option<Duration>,
    stall_ms: [u32; SIDES],
    /// 両側のカーネルの RTT (us) と再送の通算 (`[クライアント側, オリジン側]`)
    rtt_us: [u32; SIDES],
    retrans: [u32; SIDES],
}

/// 閉じた接続の個票・速さと半閉じの分布・`/trace` の 1 行 (枠のある接続だけ = `--lite` ではない)。
#[inline(never)]
fn finish_slot(o: &Info, s: &Arc<ConnSlot>, detail: &Detail, end: &Ending) {
    let mut stage_ms = [0u64; STAGES];
    stage_ms[STAGE_DNS] = detail.dns_ms;
    stage_ms[STAGE_CONNECT] = detail.connect_ms;
    stage_ms[STAGE_QUEUE] = detail.stages.queue as u64;
    stage_ms[STAGE_CLIENT_READ] = detail.stages.client_read as u64;
    stage_ms[STAGE_FIRST_RELAY] = detail.stages.first_relay as u64;
    s.finish(
        end.reason,
        ConnTally {
            up: end.up,
            down: end.down,
            status: 0,
            stage_ms,
            rtt_us: end.rtt_us,
            retrans: end.retrans,
            // 確立までの SYN の再送 (T14.46)。確立の直後に読んだ値をそのまま運ぶ
            syn_retrans: detail.syn_retrans,
            // 中継が書けるのを待った ms (T14.42)。中継のループで数え終えた
            // ものをそのまま運ぶだけで、ここでは時計も割り算も無い
            stall_ms: end.stall_ms,
        },
        0,
    );
    // 速さと半閉じの分布に 1 本足す (T14.25)。**中継の時間 = 寿命 − 確立まで −
    // 預かり所にいた時間** (預けは利用者が待っていない時間なので中継ではない。
    // T14.3 の `stages.relay` と同じ引き算だが、あちらは `/profile` が止まっていると
    // 0 なので、ここは枠の預かり秒 (T14.4) から出す = プロファイルの設定に依らない)。
    // ここは既に「個票を残す接続」(= `--lite` ではない) の内側で、時計も上で 1 回
    // 読んだものを使うので、足すのは引き算と割り算 1 回ずつと窓の鍵 1 回だけ
    let parked = s.parked_ms().saturating_sub(o.parked_ms_at_start);
    let relay = end
        .alive
        .saturating_sub(o.connect_took)
        .saturating_sub(Duration::from_millis(parked));
    o.metrics
        .history
        .transfer
        .observe(end.transferred, relay, end.half_close, end.stall_ms);
    // 接続元 1 つの追跡 (`/trace`。T14.27)。**旗が立っているトンネルだけ** 1 行書く。
    // 立っていない本数の費用はこの分岐 1 回だけで、宛先・段階の ms・閉じた理由・
    // 寿命はすぐ上で既に組んだものをそのまま渡す (時計も確保も増やさない)
    if s.traced() {
        crate::trace::push(crate::trace::Line {
            conn_id: o.conn_id,
            client: &o.client_ip,
            method: "CONNECT",
            target: &o.addr_str,
            version: "HTTP/1.1",
            status: 200,
            took_ms: end.alive.as_millis().min(u64::MAX as u128) as u64,
            bytes: end.transferred,
            stage_ms,
            reason: Some(end.reason),
        });
    }
}

/// ホスト別と接続元別の統計 (`--lite` でも生きている)。
#[inline]
fn record_stats(
    o: &Info,
    mut detail: Detail,
    (up, down): (u64, u64),
    transferred: u64,
    rtt_us: [u32; SIDES],
    retrans: [u32; SIDES],
) {
    // 向き別のバイト (T14.26)。**数え直しはしていない**: 中継が方向ごとに持っている
    // `up` / `down` (すぐ上で個票に渡したのと同じ値) をホスト別統計の鍵の内側へ
    // 運ぶだけなので、足し算 2 回のほかに費用は無い。`--lite` (枠が無い = 上の
    // `if let` を通らない) でもホスト別統計は生きているので、ここは枠の外に置く
    detail.bytes_in = up;
    detail.bytes_out = down;
    // CONNECT のホストと SNI の食い違い (T14.38)。旗は中継の入口で 1 回だけ立ててあり、
    // ここはホスト別統計が既に取る鍵の内側へ運ぶだけ (原子もシステムコールも増えない)
    detail.sni_mismatch = o.sni_mismatch;
    // オリジン側のカーネルの RTT を区間の「確立 − RTT」の窓へ運ぶ (T28.45)。上で個票に
    // 渡したのと同じ値で、読み直さない
    detail.origin_rtt_us = rtt_us[ORIGIN_SIDE];
    // 自己ベンチ (T14.43) のトンネルは合計にも足さない (`/hosts` と同じ理由)
    if !crate::selfbench::is_target(&o.addr_str) {
        o.metrics.add_bytes(transferred);
    }
    let host_key = format!("connect://{}", o.addr_str);
    // 読んだ壁時計は接続元の個票にも渡す (トンネル 1 本で 1 回。T34.146)
    let now = o.metrics.record_host_detail(
        &host_key,
        HostOutcome::Bypass,
        transferred,
        Some(o.connect_took),
        detail,
    );
    o.metrics.record_client_at(
        &o.client_ip,
        HostOutcome::Bypass,
        transferred,
        (up, down),
        Some(o.connect_took),
        // 接続元の個票に宛先の種類とポートを数える (`/clients`。T14.7)。
        // トンネル 1 本の終わりに 1 回だけで、鍵は record_client のものをそのまま使う
        Some(&o.addr_str),
        now,
    );
    // オリジン側の RTT はホスト別、クライアント側は接続元別へ (どちらも行が
    // できたあと = 上の 2 つより後に呼ぶこと。読めなければ鍵も取らない。T14.5)
    o.metrics
        .record_host_rtt(&host_key, rtt_us[ORIGIN_SIDE], retrans[ORIGIN_SIDE]);
    o.metrics
        .record_client_rtt(&o.client_ip, rtt_us[CLIENT_SIDE], retrans[CLIENT_SIDE]);
}

/// アクセスログの 1 行。
#[inline]
fn log_access(o: &Info, transferred: u64, alive: Duration) {
    access(
        o.conn_id,
        &Access {
            client: &o.client_ip,
            method: "CONNECT",
            target: &o.addr_str,
            version: "HTTP/1.1",
            status: "200",
            bytes: transferred,
            duration_ms: alive.as_secs_f64() * 1000.0,
            cache: "BYPASS(tunnel)",
        },
    );
}
