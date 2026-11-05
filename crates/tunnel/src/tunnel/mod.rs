//! CONNECT のトンネル (入口)。
//!
//! - `open`: 宛先へつなぎ、`200` と先読みぶんを送る (つなげなければ 502 を書いて報告する)。
//! - `report`: トンネルが終わったときのアクセスログと統計。
//! - `relay` (Linux): `poll` + `splice` の 1 スレッド中継と、暇なときの預け入れ。
//! - `portable` (Linux 以外): 2 スレッドで `io::copy` する従来版。
use std::io;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::dns;
use crate::metrics::{Metrics, StageMs};
use crate::net;
use crate::recent::ConnSlot;
#[cfg(not(target_os = "linux"))]
use crate::recent::{CloseReason, SIDES};
use proxy_base::minrate::MinRate;

mod open;
#[cfg(not(target_os = "linux"))]
mod portable;
#[cfg(target_os = "linux")]
mod relay;
mod report;
#[cfg(test)]
mod tests;

#[cfg(not(target_os = "linux"))]
use open::Opened;
pub use open::is_reported;
use open::open;
#[cfg(not(target_os = "linux"))]
pub use portable::tunnel;
#[cfg(target_os = "linux")]
pub use relay::{Idle, Park, expire, resume};
#[cfg(not(target_os = "linux"))]
use report::report;

/// CONNECT 1 本を始めるのに要るもの (入口で読んだ素性と設定。T34.14)。
///
/// 本体クレートの `start_tunnel` が組み、[`handle_connect`] / [`handle_connect_parked`] を
/// 通って `open` まで運ぶ (引数 13〜16 個の並びを 1 つにした)。
pub struct TunnelRequest<'a> {
    pub client: TcpStream,
    /// CONNECT の宛先 (`host[:port]`。ポートが無ければ 443)
    pub target: &'a str,
    /// リクエストヘッダーの直後に既に読み込んでしまったバイト列 (先にサーバーへ渡す)
    pub prefix: &'a [u8],
    /// オリジンへの接続の締め切り
    pub timeout: Duration,
    /// 双方向とも動きが無ければ閉じるまでの時間 (`None` で無期限)
    pub idle: Option<Duration>,
    /// 書き側の最小の速さ (T29.34。Linux の `poll` + `splice` の中継だけが見る)
    pub min_transfer: MinRate,
    pub conn_id: usize,
    pub metrics: Arc<Metrics>,
    /// 接続元 IP の文字列。接続を受けたときに 1 回だけ作ったものを運ぶ
    /// (ここで `peer_addr()` を引き直すと、トンネル 1 本ごとに `getpeername` が 1 回増える)
    pub client_ip: String,
    /// ACL の判定が引いた答え。あれば接続のときに解決し直さない (T12.7)
    pub resolved: Option<&'a dns::Resolved<'a>>,
    /// `/connections` の枠 (T13.4)
    pub slot: Option<Arc<ConnSlot>>,
    /// 本体クレートが測った段階 (`queue` など。T14.3 (1))
    pub stages: StageMs,
    /// 要求行が届いた時刻 (`client_read` の起点)
    pub read_started: Option<Instant>,
}

/// CONNECT を 1 本まかなう (`req.prefix` は先にサーバーへ渡す)。
///
/// 暇なトンネルを監視スレッドへ預けたい呼び出し側は [`handle_connect_parked`] を使う (Linux)。
pub fn handle_connect(req: TunnelRequest<'_>) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        handle_connect_parked(req, None, Box::new(()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        // 最小の速さは Linux の `poll` + `splice` の中継だけが見る (T29.34)
        let idle = req.idle;
        let Opened {
            client,
            server,
            info,
        } = open(req)?;
        let (up, down) = tunnel(client, server, idle)?;
        // Linux 以外は片方向ずつ `io::copy` するだけなので、どちらが先に EOF を出したかは
        // 分からない (`/recent` の理由は `shutdown` になる)
        // Linux 以外は `io::copy` が終わった時点でソケットを手放しているので読めない
        // 半閉じ (片側 EOF) がいつ起きたかも分からない (T14.25)
        // 詰まりの向き (T14.42) も `io::copy` の中で待つので数えられない (両方 0)
        report(
            &info,
            up,
            down,
            CloseReason::Shutdown,
            None,
            None,
            [0; SIDES],
        );
        Ok(())
    }
}

/// 暇になったら監視スレッドへ預けられる CONNECT (Linux)。
///
/// `park` は預け先と「預ける前に同じスレッドで待ってみる猶予」。`None` なら
/// 従来どおりこのスレッドが最後まで面倒をみる。
/// `hold` は本体クレートの持ち分 (同時接続数と `active_connections` のガード) で、
/// 中身は見ないがトンネルが終わるまで落とさずに運ぶ (預けた瞬間に数が減ると
/// `PROXY_MAX_CONNS` の意味が壊れる)。
#[cfg(target_os = "linux")]
pub fn handle_connect_parked(
    req: TunnelRequest<'_>,
    park: Option<(Arc<dyn Park>, Duration)>,
    hold: Box<dyn Send>,
) -> io::Result<()> {
    let (idle, min_transfer) = (req.idle, req.min_transfer);
    let opened = open(req)?;
    // トンネルの猶予は HTTP の keep-alive より長く取る (下限 [`relay::MIN_PARK_GRACE`])
    let park = park.map(|(w, grace)| (w, grace.max(relay::MIN_PARK_GRACE)));
    relay::start(opened, idle, min_transfer, park, hold)
}

/// 名前解決して接続する (IPv6 / IPv4 を Happy Eyeballs で並行に試す)。
/// `resolved` は ACL の判定が引いた答え。あればここでは解決しない (T12.7)。
pub fn connect_with_timeout(
    addr_str: &str,
    resolved: Option<&dns::Resolved<'_>>,
    timeout: Duration,
) -> io::Result<TcpStream> {
    net::connect_with(addr_str, resolved, timeout)
}
