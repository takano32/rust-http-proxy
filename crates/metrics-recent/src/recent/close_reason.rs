//! 接続が閉じた理由 ([`CloseReason`]。`recent` の一部。T34.2 で `recent/mod.rs` から移した)。
//!
//! 符号 ([`CloseReason::code`] / [`CloseReason::from_code`]) は [`ConnSlot`](super::ConnSlot) の
//! 原子と個票のファイル (T14.9) に書くもので、**既存の値は動かさない**。分布の添字
//! ([`CloseReason::index`]) は [`CLOSE_REASON_NAMES`](super::CLOSE_REASON_NAMES) の並び。
//! **時計も鍵も触らない純粋な対応表**なので、単体試験は符号と綴りの往復だけで済む。
//!
//! 行を動かしただけで中身は変えていない。外から見える名前は `recent` の `pub use` でそのまま。

use crate::metrics::{DnsCause, ErrCause, Failure};

/// 接続が閉じた理由 (`/recent` の `reason`)。
///
/// 9 種類しかないのは、読む人が「次に何を見るか」を変えられる粒度で切ったため
/// ([`ErrCause`] と同じ方針)。`Error` だけは原因を連れて `error:refused` のように出す。
/// **`/history` の分布 ([`CLOSE_REASON_NAMES`](super::CLOSE_REASON_NAMES)) は 8 列のまま**で、`ClientDead` は
/// `shutdown` に畳む (標本の余白の都合。T14.52)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// クライアントが先に EOF を送った (ふつうの終わり方)
    ClientEof,
    /// 宛先が先に EOF を送った
    ServerEof,
    /// トンネルが無通信で打ち切られた (`PROXY_TUNNEL_IDLE_SECS`)
    IdleTimeout,
    /// keep-alive の接続が次の要求を待ちきれずに閉じた (`PROXY_KEEPALIVE_SECS`)
    KeepaliveTimeout,
    /// 同時接続の上限に当たった accept が席を作るために閉じた (T13.2)
    Evicted,
    /// 1 接続あたりの要求数の上限に達した (`Config::max_requests_per_conn`。T14.2)
    Limit,
    /// 上のどれでもない、プロキシ側の都合 (監視スレッドの停止、4xx で断った、内部エンドポイント)
    Shutdown,
    /// クライアントが**黙って消えた** (TCP keepalive が尽きて `ETIMEDOUT`。T14.52)。
    ///
    /// 端末のスリープや回線の切断で FIN も RST も来なかったぶん。`idle_timeout` の
    /// うち「本当に暇」ではなかった本数がこれで分かる
    ClientDead,
    /// 入出力が失敗した (原因は [`ErrCause`] と同じ 8 つ)
    Error(ErrCause),
    /// 名前解決の失敗の内訳 (T25.0)。`/recent` の `reason` は `error:nxdomain` /
    /// `error:dns_temp` になり、`/history` の分布では `error:*` の 1 列に畳む
    /// ([`CloseReason::index`] は 7)。**符号は 17・18** で [`CloseReason::ClientDead`]
    /// (16) の後ろに足す (既存の符号は動かさない)
    ErrorDns(DnsCause),
    /// 要求行とヘッダーが期限 (`PROXY_HEADER_TIMEOUT_SECS`) までに揃わず 408 で閉じた
    /// (T28.0)。`/recent` の `reason` は `error:header_timeout` (T29.10。それまでは CONNECT の
    /// 確立の締め切りと同じ `error:timeout` で分けられなかった)。`/history` の分布では
    /// 今までどおり `error:*` の 1 列 ([`CloseReason::index`] は 7)。**符号は 19**
    HeaderTimeout,
}

impl CloseReason {
    /// リングに書く前の符号 (0 = まだ決まっていない)。[`ConnSlot`](super::ConnSlot) の原子に入れ、
    /// ファイル (T14.9) にもこの符号で書く。
    pub fn code(self) -> u16 {
        match self {
            CloseReason::ClientEof => 1,
            CloseReason::ServerEof => 2,
            CloseReason::IdleTimeout => 3,
            CloseReason::KeepaliveTimeout => 4,
            CloseReason::Evicted => 5,
            CloseReason::Limit => 6,
            CloseReason::Shutdown => 7,
            CloseReason::Error(c) => 8 + c as u16,
            // `Error` の 8 種 (8〜15) の次。**末尾に足す**ので既存の符号は動かない
            // (個票ファイル (T14.9) の大きさも変わらない。T14.52)
            CloseReason::ClientDead => 16,
            // DNS の内訳 (T25.0) は `ClientDead` の後ろ (17・18)。`Error(c)` の
            // `8 + c` の続きに置くと 16 が `ClientDead` とぶつかる
            CloseReason::ErrorDns(DnsCause::Nxdomain) => 17,
            CloseReason::ErrorDns(DnsCause::DnsTemp) => 18,
            CloseReason::HeaderTimeout => 19,
        }
    }

    /// `io::Error` から閉じた理由を決める (T25.0)。
    ///
    /// **エラーの経路からだけ呼ぶ。** DNS の失敗だけ [`Failure`] が内訳
    /// ([`DnsCause`]) を持つので、`/recent` の `reason` が `error:nxdomain` /
    /// `error:dns_temp` になる。それ以外は今までどおり `error:<原因>`。
    pub fn error_from_io(e: &std::io::Error) -> CloseReason {
        let f = Failure::from_io(e);
        match f.dns {
            Some(d) => CloseReason::ErrorDns(d),
            None => CloseReason::Error(f.cause),
        }
    }

    /// 符号から戻す。知らない値と 0 (未設定) は [`CloseReason::Shutdown`]。
    pub fn from_code(v: u16) -> CloseReason {
        match v {
            1 => CloseReason::ClientEof,
            2 => CloseReason::ServerEof,
            3 => CloseReason::IdleTimeout,
            4 => CloseReason::KeepaliveTimeout,
            5 => CloseReason::Evicted,
            6 => CloseReason::Limit,
            8 => CloseReason::Error(ErrCause::Dns),
            9 => CloseReason::Error(ErrCause::Refused),
            10 => CloseReason::Error(ErrCause::Unreachable),
            11 => CloseReason::Error(ErrCause::Timeout),
            12 => CloseReason::Error(ErrCause::Reset),
            13 => CloseReason::Error(ErrCause::Tls),
            14 => CloseReason::Error(ErrCause::Loop),
            15 => CloseReason::Error(ErrCause::Other),
            16 => CloseReason::ClientDead,
            17 => CloseReason::ErrorDns(DnsCause::Nxdomain),
            18 => CloseReason::ErrorDns(DnsCause::DnsTemp),
            19 => CloseReason::HeaderTimeout,
            _ => CloseReason::Shutdown,
        }
    }

    /// 分布の添字 ([`CLOSE_REASON_NAMES`](super::CLOSE_REASON_NAMES) の並び。T14.6)。
    ///
    /// `Error` は原因を落として 1 つにまとめる (原因別は `/status` の `errors_by_cause` と
    /// `/errors` にあるので、窓の列を 8 つ増やさない)。**`ClientDead` も `shutdown` に
    /// 畳む** (T14.52。窓の列を 9 つにすると標本の余白に入らないため。1 本ずつ見たい
    /// ときは `/recent` の `reason` を読む)。
    pub fn index(self) -> usize {
        match self {
            CloseReason::ClientEof => 0,
            CloseReason::ServerEof => 1,
            CloseReason::IdleTimeout => 2,
            CloseReason::KeepaliveTimeout => 3,
            CloseReason::Evicted => 4,
            CloseReason::Limit => 5,
            CloseReason::Shutdown | CloseReason::ClientDead => 6,
            CloseReason::Error(_) | CloseReason::ErrorDns(_) | CloseReason::HeaderTimeout => 7,
        }
    }

    /// `/recent` の `reason` に出す綴り (`error:` は原因を連れる)。
    pub fn text(self) -> std::borrow::Cow<'static, str> {
        use std::borrow::Cow;
        match self {
            CloseReason::ClientEof => Cow::Borrowed("client_eof"),
            CloseReason::ServerEof => Cow::Borrowed("server_eof"),
            CloseReason::IdleTimeout => Cow::Borrowed("idle_timeout"),
            CloseReason::KeepaliveTimeout => Cow::Borrowed("keepalive_timeout"),
            CloseReason::Evicted => Cow::Borrowed("evicted"),
            CloseReason::Limit => Cow::Borrowed("limit"),
            CloseReason::Shutdown => Cow::Borrowed("shutdown"),
            CloseReason::ClientDead => Cow::Borrowed("client_dead"),
            CloseReason::Error(c) => Cow::Owned(format!("error:{}", c.name())),
            CloseReason::ErrorDns(c) => Cow::Owned(format!("error:{}", c.name())),
            CloseReason::HeaderTimeout => Cow::Borrowed("error:header_timeout"),
        }
    }
}

/// 閉じた理由の単体試験 (T34.2 で別のファイルに移した)。
#[cfg(test)]
#[path = "close_reason_tests.rs"]
mod tests;
