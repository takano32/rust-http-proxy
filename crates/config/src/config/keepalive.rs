//! クライアント側のソケットに当てる TCP keepalive (`PROXY_TCP_KEEPALIVE`。T14.52) の読み方と書き戻し。

/// 消えたクライアントを見つける TCP keepalive の既定 (`PROXY_TCP_KEEPALIVE`。T14.52)。
///
/// 60 秒無通信で探りを出し、10 秒おきに 3 回返事が無ければ**約 90 秒**で
/// `ETIMEDOUT` にする。`PROXY_TUNNEL_IDLE_SECS` (300 秒) より十分短いので、
/// 「本当に暇」で閉じる前に「相手が消えた」を見分けられる。
pub const DEFAULT_TCP_KEEPALIVE: TcpKeepalive = TcpKeepalive {
    idle_secs: 60,
    intvl_secs: 10,
    count: 3,
};

/// クライアント側のソケットに当てる TCP keepalive (`PROXY_TCP_KEEPALIVE`。T14.52)。
///
/// 端末がスリープしたり回線が切れたりすると FIN も RST も来ないので、トンネルは
/// `PROXY_TUNNEL_IDLE_SECS` の期限切れまで残り、閉じた理由は `idle_timeout` になる。
/// keepalive を当てておけば消えた相手は `idle_secs + intvl_secs * count` 秒ほどで
/// `ETIMEDOUT` になり、`client_dead` として数えられる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpKeepalive {
    /// 無通信がこれだけ続いたら最初の探りを送る (秒。`TCP_KEEPIDLE`)
    pub idle_secs: u32,
    /// 探りと探りの間隔 (秒。`TCP_KEEPINTVL`)
    pub intvl_secs: u32,
    /// 返事の無い探りを何回まで送るか (`TCP_KEEPCNT`)
    pub count: u32,
}

impl TcpKeepalive {
    /// `on` / `off` / `on:<idle>:<intvl>:<cnt>` を読む。読めない書き方は `None`
    /// (呼び出し側が既定のまま使う)。**`off` は `Some(None)`** で返す。
    pub fn parse(v: &str) -> Option<Option<TcpKeepalive>> {
        let v = v.trim().to_ascii_lowercase();
        let mut it = v.split(':');
        let head = it.next()?;
        if matches!(head, "0" | "false" | "off" | "no") {
            return Some(None);
        }
        if !matches!(head, "1" | "true" | "on" | "yes") {
            return None;
        }
        let rest: Vec<&str> = it.collect();
        if rest.is_empty() {
            return Some(Some(DEFAULT_TCP_KEEPALIVE));
        }
        // 3 つ揃っていて全部 1 以上のときだけ採る (半端な指定は既定に落とす)
        if rest.len() != 3 {
            return None;
        }
        let mut n = [0u32; 3];
        for (o, s) in n.iter_mut().zip(rest) {
            *o = s.trim().parse::<u32>().ok().filter(|v| *v > 0)?;
        }
        Some(Some(TcpKeepalive {
            idle_secs: n[0],
            intvl_secs: n[1],
            count: n[2],
        }))
    }

    /// 消えた相手が `ETIMEDOUT` になるまでのおおよその秒数 (README と起動ログ用)。
    pub fn dead_after_secs(&self) -> u32 {
        self.idle_secs
            .saturating_add(self.intvl_secs.saturating_mul(self.count))
    }

    /// 効いている値を `.env` に書き戻せる形で返す (T14.52。`None` = `off`)。
    /// [`TcpKeepalive::parse`] の裏返しで、`parse(&spec(k)) == Some(k)`。
    pub fn spec(keepalive: Option<TcpKeepalive>) -> String {
        match keepalive {
            None => "off".to_string(),
            Some(k) => format!("on:{}:{}:{}", k.idle_secs, k.intvl_secs, k.count),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PROXY_TCP_KEEPALIVE` の書き方 (T14.52)。半端な指定は既定に落とす。
    #[test]
    fn tcp_keepalive_parses_on_off_and_three_numbers() {
        assert_eq!(TcpKeepalive::parse("on"), Some(Some(DEFAULT_TCP_KEEPALIVE)));
        assert_eq!(
            TcpKeepalive::parse(" ON "),
            Some(Some(DEFAULT_TCP_KEEPALIVE))
        );
        assert_eq!(TcpKeepalive::parse("1"), Some(Some(DEFAULT_TCP_KEEPALIVE)));
        assert_eq!(TcpKeepalive::parse("off"), Some(None));
        assert_eq!(TcpKeepalive::parse("0"), Some(None));
        assert_eq!(TcpKeepalive::parse("no"), Some(None));
        assert_eq!(
            TcpKeepalive::parse("on:1:1:2"),
            Some(Some(TcpKeepalive {
                idle_secs: 1,
                intvl_secs: 1,
                count: 2,
            }))
        );
        // 既定は約 90 秒で `ETIMEDOUT` (`PROXY_TUNNEL_IDLE_SECS` の 300 秒より十分短い)
        assert_eq!(DEFAULT_TCP_KEEPALIVE.dead_after_secs(), 90);
        // 読めない書き方は `None` = 既定のまま
        for bad in [
            "on:1", "on:1:2", "on:0:1:2", "on:1:1:0", "on:a:b:c", "maybe", "",
        ] {
            assert_eq!(TcpKeepalive::parse(bad), None, "{}", bad);
        }
    }

    /// 書き戻しは読み方の裏返し (`off` と `on:<idle>:<intvl>:<cnt>`)。
    #[test]
    fn spec_round_trips_through_parse() {
        for k in [
            None,
            Some(DEFAULT_TCP_KEEPALIVE),
            Some(TcpKeepalive {
                idle_secs: 7,
                intvl_secs: 3,
                count: 5,
            }),
        ] {
            assert_eq!(TcpKeepalive::parse(&TcpKeepalive::spec(k)), Some(k));
        }
        assert_eq!(TcpKeepalive::spec(None), "off");
        assert_eq!(
            TcpKeepalive::spec(Some(DEFAULT_TCP_KEEPALIVE)),
            "on:60:10:3"
        );
    }
}
