//! CONNECT トンネルへの入口。

use super::*;

/// CONNECT トンネルを始める。
///
/// Linux では、両方向とも暇になったトンネルを監視スレッド (`epoll`) へ預けて
/// スレッドを手放す (`park` があるときだけ)。それ以外の環境では従来どおり
/// 1 本のスレッドが最後まで面倒をみる。
#[cfg(target_os = "linux")]
pub(super) fn start_tunnel(
    req: tunnel::TunnelRequest<'_>,
    park: Option<Arc<idle::IdleWatch>>,
    grace: std::time::Duration,
    hold: Box<dyn Send>,
) -> io::Result<()> {
    let park = park.map(|w| (w as Arc<dyn tunnel::Park>, grace));
    tunnel::handle_connect_parked(req, park, hold)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn start_tunnel(
    req: tunnel::TunnelRequest<'_>,
    park: Option<Arc<idle::IdleWatch>>,
    grace: std::time::Duration,
    hold: Box<dyn Send>,
) -> io::Result<()> {
    // 預け先は Linux (epoll) だけ。持ち分はこの関数が終わるまで持っておく
    let _ = (park, grace);
    let result = tunnel::handle_connect(req);
    drop(hold);
    result
}
