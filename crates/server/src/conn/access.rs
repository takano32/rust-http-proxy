//! 宛先の判定 ([`access_decision`]) と、判定で断る 2 つの応答 ([`refuse_blocked`] の 403・
//! [`loop_detected`] の 508)。T.34.12

use super::*;

use super::first_line::Peer;
use super::header_read::reject;

/// 宛先を断る理由と、ローカル宛ての判定で引いた答え (接続でもう一度引かないために持ち回す。T12.7)。
///
/// 先に当たったものが勝つ: ACL → 遮断リスト → CONNECT のポート → ローカル宛て。
/// 名前を引くのは最後のローカル宛ての判定だけで、`PROXY_ALLOW_LOCAL=on` なら引かない。
#[inline(always)]
pub(super) fn access_decision<'h>(
    config: &Config,
    target_host: &'h str,
    bare_host: &str,
    host_port: Option<u16>,
    is_connect: bool,
) -> (Option<metrics::BlockCause>, Option<dns::Resolved<'h>>) {
    let mut resolved: Option<dns::Resolved> = None;
    let denied = if !config.acl.is_allowed(target_host) {
        Some(metrics::BlockCause::Acl)
    } else if blocklist::is_blocked(bare_host) {
        Some(metrics::BlockCause::Blocklist)
    } else if is_connect && !config.connect_ports.allows(host_port.unwrap_or(443)) {
        Some(metrics::BlockCause::ConnectPort)
    } else if config.allow_local != acl::AllowLocal::On {
        // クラウドのメタデータ (169.254.169.254) 経由の SSRF を止める。
        // **判定に使った答えはそのまま接続へ渡す** (名前解決は 1 要求 1 回。T12.7)。
        // 名前解決の費用はここで払うので、前の要求がこのスレッドに残した分は
        // 解決の前に捨てる (次の `take` でこの要求のぶんだけが取れるように。T12.4 (2))
        let _ = dns::take_resolve_cost();
        let _ = dns::take_family();
        let _ = dns::take_syn_retrans();
        // `private` なら私設アドレスは通す (ループバック・リンクローカル・未指定は断る。T.28.41)
        let (local, r) = acl::resolve_target(target_host, config.allow_local.allows_private());
        resolved = r;
        local.then_some(metrics::BlockCause::Local)
    } else {
        None
    };
    (denied, resolved)
}

/// 判定で断った宛先に 403 を返して閉じる。
#[cold]
pub(super) fn refuse_blocked(
    peer: Peer<'_>,
    why: metrics::BlockCause,
    target_host: &str,
    bare_host: &str,
    is_connect: bool,
) -> io::Result<Step> {
    let Peer {
        client,
        metrics,
        ip: peer_ip,
        conn_id,
        ..
    } = peer;
    // 403 も宛先を選ぶのは相手なので、理由ごとに 1 分に 1 行 (T.28.9)。本数は
    // `/status` の宛先・接続元ごとの `blocked` と `/errors` の個票が持つ
    static BLOCK_WARN: [log::Throttle; 4] = [const { log::Throttle::new() }; 4];
    let gate = match why {
        metrics::BlockCause::Acl => 0,
        metrics::BlockCause::Blocklist => 1,
        metrics::BlockCause::ConnectPort => 2,
        metrics::BlockCause::Local => 3,
    };
    if let Some(more) = BLOCK_WARN[gate].pass(log::WARN_EVERY_SECS) {
        log_warn!(
            Some(conn_id),
            "403 Forbidden ({} blocked host: {}){}",
            why.label(),
            target_host,
            more
        );
    }
    // 読んだ壁時計は接続元の個票にも渡す (要求 1 本で 1 回。T.34.146)
    let now = metrics.record_host(
        &format!("blocked://{}", bare_host),
        metrics::HostOutcome::Blocked,
        0,
    );
    metrics.record_client_at(
        peer_ip,
        metrics::HostOutcome::Blocked,
        0,
        // 403 で断ったので運んだバイトは上りも下りも無い (T14.26)
        (0, 0),
        None,
        Some(target_host),
        now,
    );
    // 個票にも 1 件残す (`/errors`。T14.2 (4))。403 は集計では `blocked` に数えてあり、
    // `errors_by_cause` には乗らないので、**誰が何を拒否されたか**はここでしか読めない
    metrics.record_blocked(is_connect, target_host, peer_ip, why);
    (&*client).write_all(FORBIDDEN_RESPONSE)?;
    (&*client).flush()?;
    Ok(Step::Close(recent::CloseReason::Shutdown))
}

/// 自分の印の付いた `Via` を受けた = 自分を通った要求が自分に戻ってきた (T12.3 の保険) ので、
/// 508 を返して閉じる。
///
/// 印は起動ごとの乱数なので、rust-http-proxy を 2 段に並べた正当な構成 (別プロセス) は
/// 誤検出しない。同じプロセスの別の待ち受けへ回した形はここで 1 段目で止まる。
/// `proxied` は `clients[]` の母数に入れる旗 (T15.0 (12))。
#[cold]
pub(super) fn loop_detected(
    peer: Peer<'_>,
    host_header: Option<&str>,
    target: &str,
    proxied: &mut bool,
) -> io::Result<Step> {
    let Peer {
        client,
        metrics,
        ip: peer_ip,
        conn_id,
        ..
    } = peer;
    // 自分の印は外から付けられる (誰かが印を写して送ってくる) ので、これも 1 分に 1 行 (T.28.9)
    static LOOP_WARN: log::Throttle = log::Throttle::new();
    if let Some(more) = LOOP_WARN.pass(log::WARN_EVERY_SECS) {
        log_warn!(
            Some(conn_id),
            "508 Loop Detected (request carries our own Via: {}){}",
            via::token(),
            more
        );
    }
    reject(client, 508, "Loop Detected")?;
    // 原因つきで 1 件数える (T12.4 (2))。ここは要求ターゲットをまだ解いていないので、
    // 鍵は `Host` (無ければ要求ターゲット) から作る。この経路は 1 要求に 1 回しか
    // 通らないので `format!` の確保は熱い経路に乗らない
    let detail = metrics::Detail {
        cause: Some(metrics::ErrCause::Loop),
        ..metrics::Detail::default()
    };
    // 読んだ壁時計は接続元の個票にも渡す (要求 1 本で 1 回。T.34.146)
    let now = metrics.record_host_detail(
        &format!("loop://{}", host_header.unwrap_or(target)),
        metrics::HostOutcome::Error,
        0,
        None,
        detail,
    );
    // 個票にも 1 件残す (`/errors`。T13.4)
    metrics.record_error(false, host_header.unwrap_or(target), peer_ip, 508, &detail);
    // この枝は `endpoints::handle` より**手前**で return するので、その後の
    // `*proxied = true` を通らない。ここも `record_client` で `clients[]` の行を
    // 作る側なので、母数 (T15.0 (12)) にはこの枝も入れる。自分宛ての要求は
    // 自分の `Via` を持たないので、ここへ来るのはプロキシとして受けた要求だけ
    *proxied = true;
    metrics.record_client_at(
        peer_ip,
        metrics::HostOutcome::Error,
        0,
        // 508 で閉じたので運んだバイトは上りも下りも無い (T14.26)
        (0, 0),
        None,
        Some(host_header.unwrap_or(target)),
        now,
    );
    Ok(Step::Close(recent::CloseReason::Error(
        metrics::ErrCause::Loop,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::BlockCause as B;

    fn config(deny: Option<&str>, ports: &str, local: acl::AllowLocal) -> Config {
        let mut c =
            config::Config::new("8080", None, None, std::time::Duration::from_secs(5)).unwrap();
        c.acl = acl::AclConfig::new(None, deny);
        c.connect_ports = acl::PortSet::parse(ports);
        c.allow_local = local;
        c
    }

    fn decide(c: &Config, target_host: &str, is_connect: bool) -> Option<B> {
        let (bare, port) = net::split_host_port_ref(target_host);
        access_decision(c, target_host, bare, port, is_connect).0
    }

    #[test]
    fn nothing_is_refused_by_default_when_local_is_allowed() {
        let c = config(None, "", acl::AllowLocal::On);
        assert_eq!(decide(&c, "example.com:443", true), None);
        assert_eq!(decide(&c, "127.0.0.1:80", false), None);
    }

    #[test]
    fn the_acl_wins_over_the_connect_port_and_the_local_check() {
        let c = config(Some("127.0.0.1"), "443", acl::AllowLocal::Off);
        // 拒否の一覧・ポートの外・ループバックの 3 つに当たる宛先は ACL で数える
        assert_eq!(decide(&c, "127.0.0.1:25", true), Some(B::Acl));
    }

    #[test]
    fn the_connect_port_is_checked_only_for_connect_and_before_the_local_check() {
        let c = config(None, "443", acl::AllowLocal::Off);
        assert_eq!(decide(&c, "127.0.0.1:25", true), Some(B::ConnectPort));
        // 転送 (CONNECT でない) はポートを見ないので、ローカル宛てで断る
        assert_eq!(decide(&c, "127.0.0.1:25", false), Some(B::Local));
        // ポートの範囲の端は通す
        let c = config(None, "443,8000-8080", acl::AllowLocal::On);
        assert_eq!(decide(&c, "example.com:8000", true), None);
        assert_eq!(decide(&c, "example.com:8080", true), None);
        assert_eq!(decide(&c, "example.com:8081", true), Some(B::ConnectPort));
        assert_eq!(decide(&c, "example.com:7999", true), Some(B::ConnectPort));
    }

    #[test]
    fn the_local_check_refuses_loopback_unless_allowed() {
        let off = config(None, "", acl::AllowLocal::Off);
        assert_eq!(decide(&off, "127.0.0.1:8080", false), Some(B::Local));
        assert_eq!(decide(&off, "[::1]:8080", false), Some(B::Local));
        // `private` でもループバックは断る (T.28.41)
        let private = config(None, "", acl::AllowLocal::Private);
        assert_eq!(decide(&private, "127.0.0.1:8080", false), Some(B::Local));
        // `on` なら名前も引かずに通す
        let on = config(None, "", acl::AllowLocal::On);
        assert_eq!(decide(&on, "127.0.0.1:8080", false), None);
    }
}
