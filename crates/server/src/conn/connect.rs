//! CONNECT トンネルへの入口。

use super::*;

/// CONNECT トンネルを始める。
///
/// Linux では、両方向とも暇になったトンネルを監視スレッド (`epoll`) へ預けて
/// スレッドを手放す (`park` があるときだけ)。それ以外の環境では従来どおり
/// 1 本のスレッドが最後まで面倒をみる。
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
pub(super) fn start_tunnel(
    client: TcpStream,
    target: &str,
    prefix: &[u8],
    timeout: std::time::Duration,
    idle: Option<std::time::Duration>,
    conn_id: usize,
    metrics: Arc<Metrics>,
    client_ip: String,
    resolved: Option<&dns::Resolved<'_>>,
    park: Option<Arc<idle::IdleWatch>>,
    grace: std::time::Duration,
    hold: Box<dyn Send>,
    slot: Option<Arc<recent::ConnSlot>>,
    stages: metrics::StageMs,
    read_started: Option<Instant>,
) -> io::Result<()> {
    let park = park.map(|w| (w as Arc<dyn tunnel::Park>, grace));
    tunnel::handle_connect_parked(
        client,
        target,
        prefix,
        timeout,
        idle,
        conn_id,
        metrics,
        client_ip,
        resolved,
        park,
        hold,
        slot,
        stages,
        read_started,
    )
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn start_tunnel(
    client: TcpStream,
    target: &str,
    prefix: &[u8],
    timeout: std::time::Duration,
    idle: Option<std::time::Duration>,
    conn_id: usize,
    metrics: Arc<Metrics>,
    client_ip: String,
    resolved: Option<&dns::Resolved<'_>>,
    park: Option<Arc<idle::IdleWatch>>,
    grace: std::time::Duration,
    hold: Box<dyn Send>,
    slot: Option<Arc<recent::ConnSlot>>,
    stages: metrics::StageMs,
    read_started: Option<Instant>,
) -> io::Result<()> {
    // 預け先は Linux (epoll) だけ。持ち分はこの関数が終わるまで持っておく
    let _ = (park, grace);
    let result = tunnel::handle_connect(
        client,
        target,
        prefix,
        timeout,
        idle,
        conn_id,
        metrics,
        client_ip,
        resolved,
        slot,
        stages,
        read_started,
    );
    drop(hold);
    result
}
