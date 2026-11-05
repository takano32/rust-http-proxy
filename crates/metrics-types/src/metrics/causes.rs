//! 1 要求の結末の分け方: ホスト別の結果 ([`HostOutcome`])、エラーの原因 ([`ErrCause`])、
//! 名前解決の失敗の内訳 ([`DnsCause`])、その 2 つを一度に決める [`Failure`]、
//! 403 で拒否した理由 ([`BlockCause`])。どれも**エラーの経路でしか文字列を見ない**。

/// ホスト別に数える結果の分類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOutcome {
    Hit,
    Miss,
    Bypass,
    Error,
    /// ACL / ブロックリストで拒否した (403)
    Blocked,
}

impl HostOutcome {
    /// アクセスログの `cache=` の値とステータスから分類する。
    pub fn from_access(cache_state: &str, status: u16) -> Self {
        if status >= 500 {
            return HostOutcome::Error;
        }
        if cache_state.starts_with("HIT")
            || cache_state.starts_with("REVALIDATED")
            || cache_state.starts_with("REFRESHING")
            || cache_state.starts_with("COALESCED")
            || cache_state.starts_with("STALE")
        {
            HostOutcome::Hit
        } else if cache_state.starts_with("MISS") {
            HostOutcome::Miss
        } else {
            HostOutcome::Bypass
        }
    }
}

/// エラーの原因 (T12.4 (2))。デプロイ先で「エラー 12 件、原因は不明」だったのを
/// **8 つに畳んで**数える。細かく分けても読む人が増やせないので、対処が変わる粒度で切る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum ErrCause {
    /// 名前解決に失敗した
    Dns = 0,
    /// 接続を拒まれた (RST / ECONNREFUSED)
    Refused = 1,
    /// 経路が無い (ENETUNREACH / EHOSTUNREACH / EADDRNOTAVAIL)
    Unreachable = 2,
    /// 締め切りに間に合わなかった (`PROXY_TIMEOUT_SECS` / CONNECT のオリジン接続は
    /// `PROXY_CONNECT_TIMEOUT_SECS`。T15.6)
    Timeout = 3,
    /// つないだあとで切られた (ECONNRESET / EPIPE / EOF)
    Reset = 4,
    /// TLS の握手や証明書で失敗した
    Tls = 5,
    /// 自分の `Via` が付いた要求 = ループ (508 Loop Detected。T12.3)
    Loop = 6,
    /// 上のどれでもない
    Other = 7,
}

/// [`ErrCause`] の数。
pub const ERR_CAUSES: usize = 8;

/// `/status` と `/metrics` のラベルに使う名前 ([`ErrCause`] と同じ順)。
pub const ERR_CAUSE_NAMES: [&str; ERR_CAUSES] = [
    "dns",
    "refused",
    "unreachable",
    "timeout",
    "reset",
    "tls",
    "loop",
    "other",
];

/// `errors_by_cause` の**うしろに足す** DNS の内訳の数 (T25.0)。
///
/// 先頭 8 つ ([`ERR_CAUSE_NAMES`]) の意味と並びは**変えない**: 過去の雪像 (`schema` 1) の
/// `errors_by_cause` は 8 要素で、読む側は先頭 8 つをそのまま読む。`dns` (添字 0) は
/// 今までどおり**名前解決の失敗の合計**で、9・10 番目がその内訳
/// (`errors_by_cause[0] == 9 番目 + 10 番目`)。
pub const DNS_CAUSES: usize = 2;

/// `errors_by_cause` の 9・10 番目に入る名前 ([`DNS_CAUSES`] と同じ順)。
pub const DNS_CAUSE_NAMES: [&str; DNS_CAUSES] = ["nxdomain", "dns_temp"];

/// 名前解決の失敗の内訳 (T25.0)。**引き直せば変わるか**で 2 つに割る。
///
/// 2026-10-23 の雪像では `/errors` の 500 件のうち 447 件が `dns` で、`/log` の文言で
/// 割ると `Name or service not known` (`EAI_NONAME`) が大半、`Temporary failure in
/// name resolution` (`EAI_AGAIN`) が 16 件だった。メール系の総当たりのように
/// **名前が無い**のか、リゾルバが落ちているのかで次にやることが違うので分ける。
///
/// [`ErrCause`] に足さないのは、`CloseReason` の符号 (`8 + cause`) と `errors_by_cause`
/// の先頭 8 列を動かさないため。[`ErrCause::Dns`] は合計として今までどおり残る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum DnsCause {
    /// 名前 (または住所) が無いと言われた = **引き直しても変わらない**
    /// (`EAI_NONAME` / `EAI_NODATA` 相当)
    Nxdomain = 0,
    /// 一時的な失敗 = 引き直せば変わりうる (`EAI_AGAIN` 相当)。文言を読めないものも
    /// 「変わらない」と言い切れないのでこちらに入れる
    DnsTemp = 1,
}

impl DnsCause {
    /// `errors_by_cause` の 9・10 番目の名前 (`/explain` と `/recent` も同じ綴り)。
    pub fn name(self) -> &'static str {
        DNS_CAUSE_NAMES[self as usize]
    }

    /// `io::Error` の文言から内訳を決める。**呼ぶのは [`ErrCause::from_io`] が
    /// [`ErrCause::Dns`] を返したときだけ** ([`Failure::from_io`] が 1 か所で面倒を見る)。
    ///
    /// `getaddrinfo` の失敗は Rust では `io::ErrorKind` が `NotFound` にも
    /// `Uncategorized` にもなり (`ToSocketAddrs` の実装依存)、**EAI の符号は文言にしか
    /// 残らない**ので文字列で見る。エラーの経路からしか通らないので熱い道には乗らない。
    pub fn from_io(e: &std::io::Error) -> DnsCause {
        DnsCause::of_text(&e.to_string())
    }

    /// 文言から内訳を決める ([`DnsCause::from_io`] の本体。試験から直接叩ける)。
    pub fn of_text(msg: &str) -> DnsCause {
        // 「名前が無い / 住所が無い」と言い切れる文言だけ `nxdomain`。
        // ここに無いもの (一時的な失敗、読めない文言) は `dns_temp` に寄せる
        const NO_NAME: [&str; 6] = [
            "Name or service not known",                  // EAI_NONAME (glibc)
            "No address associated with hostname",        // EAI_NODATA (glibc)
            "nodename nor servname provided",             // EAI_NONAME (macOS)
            "Non-recoverable failure in name resolution", // EAI_FAIL (glibc)
            "Could not resolve host", // 引けたが住所が 0 本 (net::connect_addrs)
            "host has no IPv4 address", // IPv6 を切っていて使える住所が無い
        ];
        if NO_NAME.iter().any(|p| msg.contains(p)) {
            DnsCause::Nxdomain
        } else {
            DnsCause::DnsTemp
        }
    }
}

/// 失敗 1 件の分け方 (T25.0)。[`ErrCause`] と、DNS のときだけ入る内訳。
///
/// **1 つの `io::Error` から 2 つを一度に決める**ための小さな型。呼ぶ側が
/// [`ErrCause::from_io`] と [`DnsCause::from_io`] を別々に呼ぶと、`dns` が `Some` なのに
/// `cause` が [`ErrCause::Dns`] でない組み合わせを作れてしまう。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    pub cause: ErrCause,
    /// `cause` が [`ErrCause::Dns`] のときだけ `Some`
    pub dns: Option<DnsCause>,
}

impl Failure {
    /// 名前解決ではない原因 1 つ (`dns` は `None`。T.29.15 の WARN の絞りの鍵などに)。
    pub fn of(cause: ErrCause) -> Failure {
        Failure { cause, dns: None }
    }

    pub fn from_io(e: &std::io::Error) -> Failure {
        let cause = ErrCause::from_io(e);
        Failure {
            cause,
            dns: (cause == ErrCause::Dns).then(|| DnsCause::from_io(e)),
        }
    }
}

/// 403 で拒否した理由 (T14.2 (4))。**個票 (`/errors`) にだけ出す。**
///
/// [`ErrCause`] に足さないのは、`errors_by_cause` の配列が伸びると履歴の標本 1 本が
/// `.rrd` の領域 (508 B) に収まらなくなり、**版を上げて統計を全部捨てる**ことになるため
/// (63 項目 × 8 B = 504 B で、余白はもう 4 B しかない)。403 はそもそもエラー (5xx) ではなく
/// `HostOutcome::Blocked` として `blocked` に数えてあるので、集計はそのまま。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockCause {
    /// `PROXY_ALLOW_HOSTS` / `PROXY_DENY_HOSTS` で拒否した
    Acl,
    /// ブロックリストに載っていた
    Blocklist,
    /// `PROXY_CONNECT_PORTS` の外のポートへの CONNECT だった
    ConnectPort,
    /// ループバック・リンクローカル・私設アドレス宛て (`PROXY_ALLOW_LOCAL=off` の SSRF 除け。
    /// 私設アドレスは T.28.41 から。`private` ではループバック・リンクローカルだけ)
    Local,
}

impl BlockCause {
    /// `/errors` の `cause` に出す名前。
    pub fn name(self) -> &'static str {
        match self {
            BlockCause::Acl => "acl",
            BlockCause::Blocklist => "blocklist",
            BlockCause::ConnectPort => "connect_port",
            BlockCause::Local => "local",
        }
    }

    /// 警告ログに出す文言 (`403 Forbidden (... blocked host: ...)`)。
    pub fn label(self) -> &'static str {
        match self {
            BlockCause::Acl => "ACL",
            BlockCause::Blocklist => "blocklist",
            BlockCause::ConnectPort => "CONNECT port",
            BlockCause::Local => "local address",
        }
    }
}

impl ErrCause {
    /// `io::Error` から原因を決める。**この判定はエラーのときにしか通らない**ので、
    /// 文字列を見るところがあっても熱い経路には乗らない。
    pub fn from_io(e: &std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        match e.kind() {
            K::ConnectionRefused => ErrCause::Refused,
            K::NetworkUnreachable | K::HostUnreachable | K::AddrNotAvailable => {
                ErrCause::Unreachable
            }
            // ブロッキングの読み書きに `SO_RCVTIMEO` / `SO_SNDTIMEO` を当てていると、Linux では
            // 締め切りが切れたときに `WouldBlock` (EAGAIN = os error 11) が返る (T.28.5。
            // 今までは `other` に数えていた)。ここを通るのは終端のエラーだけなので、
            // 非ブロッキングの「まだ読めない」が紛れ込むことはない
            K::TimedOut | K::WouldBlock => ErrCause::Timeout,
            K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe | K::UnexpectedEof => {
                ErrCause::Reset
            }
            // `getaddrinfo` の失敗は Linux では `NotFound` にも `Uncategorized` にもなる
            // (`ToSocketAddrs` の実装依存) ので、文言も見る
            K::NotFound => ErrCause::Dns,
            _ => {
                let msg = e.to_string();
                if msg.contains("lookup address") || msg.contains("resolve host") {
                    ErrCause::Dns
                } else if msg.contains("TLS") || msg.contains("tls") || msg.contains("certificate")
                {
                    ErrCause::Tls
                } else {
                    ErrCause::Other
                }
            }
        }
    }

    pub fn name(self) -> &'static str {
        ERR_CAUSE_NAMES[self as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// アクセスログの `cache=` と状態コードからの分類: 5xx はまずエラー、HIT 系 5 つは HIT、
    /// `MISS…` は MISS、それ以外は BYPASS。
    #[test]
    fn the_outcome_follows_the_status_first_and_the_cache_state_second() {
        assert_eq!(
            HostOutcome::from_access("HIT(memory)", 502),
            HostOutcome::Error
        );
        assert_eq!(HostOutcome::from_access("MISS", 500), HostOutcome::Error);
        for hit in [
            "HIT(memory) age=1s",
            "REVALIDATED",
            "REFRESHING",
            "COALESCED",
            "STALE(grace)",
        ] {
            assert_eq!(
                HostOutcome::from_access(hit, 200),
                HostOutcome::Hit,
                "{hit}"
            );
        }
        assert_eq!(
            HostOutcome::from_access("MISS stored ttl=1s", 404),
            HostOutcome::Miss
        );
        assert_eq!(HostOutcome::from_access("BYPASS", 200), HostOutcome::Bypass);
        assert_eq!(HostOutcome::from_access("", 200), HostOutcome::Bypass);
    }

    /// `io::Error` から 8 つの原因へ畳めること (T12.4 (2))。
    #[test]
    fn io_errors_fold_into_eight_causes() {
        use std::io::{Error, ErrorKind};
        let c = |k: ErrorKind| ErrCause::from_io(&Error::new(k, "x"));
        assert_eq!(c(ErrorKind::ConnectionRefused), ErrCause::Refused);
        assert_eq!(c(ErrorKind::NetworkUnreachable), ErrCause::Unreachable);
        assert_eq!(c(ErrorKind::HostUnreachable), ErrCause::Unreachable);
        assert_eq!(c(ErrorKind::TimedOut), ErrCause::Timeout);
        // `SO_RCVTIMEO` の締め切りは Linux では EAGAIN で返る (T.28.5)
        assert_eq!(c(ErrorKind::WouldBlock), ErrCause::Timeout);
        #[cfg(target_os = "linux")]
        assert_eq!(
            ErrCause::from_io(&Error::from_raw_os_error(11)),
            ErrCause::Timeout
        );
        assert_eq!(c(ErrorKind::ConnectionReset), ErrCause::Reset);
        assert_eq!(c(ErrorKind::BrokenPipe), ErrCause::Reset);
        assert_eq!(c(ErrorKind::NotFound), ErrCause::Dns);
        assert_eq!(c(ErrorKind::PermissionDenied), ErrCause::Other);
        // `getaddrinfo` の失敗は種別が付かないことがあるので文言も見る
        assert_eq!(
            ErrCause::from_io(&Error::other("failed to lookup address information: x")),
            ErrCause::Dns
        );
        assert_eq!(
            ErrCause::from_io(&Error::other("TLS handshake failed")),
            ErrCause::Tls
        );
        assert_eq!(ErrCause::Loop.name(), "loop");
        assert_eq!(ERR_CAUSE_NAMES.len(), ERR_CAUSES);
    }

    /// 名前解決の内訳は `dns` のときだけ付く (組み合わせがずれない。T25.0)。
    #[test]
    fn the_dns_breakdown_is_only_attached_to_dns_failures() {
        use std::io::{Error, ErrorKind};
        let nx = Error::other("failed to lookup address information: Name or service not known");
        assert_eq!(
            Failure::from_io(&nx),
            Failure {
                cause: ErrCause::Dns,
                dns: Some(DnsCause::Nxdomain)
            }
        );
        let temp = Error::other(
            "failed to lookup address information: Temporary failure in name resolution",
        );
        assert_eq!(Failure::from_io(&temp).dns, Some(DnsCause::DnsTemp));
        assert_eq!(
            Failure::from_io(&Error::new(ErrorKind::ConnectionRefused, "x")),
            Failure::of(ErrCause::Refused)
        );
        for (text, want) in [
            ("No address associated with hostname", DnsCause::Nxdomain),
            ("nodename nor servname provided", DnsCause::Nxdomain),
            ("Could not resolve host", DnsCause::Nxdomain),
            ("host has no IPv4 address", DnsCause::Nxdomain),
            ("Temporary failure in name resolution", DnsCause::DnsTemp),
            ("something new", DnsCause::DnsTemp),
        ] {
            assert_eq!(DnsCause::of_text(text), want, "{text}");
            assert_eq!(want.name(), DNS_CAUSE_NAMES[want as usize]);
        }
    }

    /// 403 の理由の名前 (`/errors`) と文言 (WARN) は 4 つとも別の綴り。
    #[test]
    fn block_causes_have_a_record_name_and_a_log_label() {
        let all = [
            BlockCause::Acl,
            BlockCause::Blocklist,
            BlockCause::ConnectPort,
            BlockCause::Local,
        ];
        assert_eq!(
            all.map(BlockCause::name),
            ["acl", "blocklist", "connect_port", "local"]
        );
        assert_eq!(
            all.map(BlockCause::label),
            ["ACL", "blocklist", "CONNECT port", "local address"]
        );
    }
}
