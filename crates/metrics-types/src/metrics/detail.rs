//! 1 要求ぶんの内訳 ([`Detail`]) と段階ごとの待ち時間 ([`StageMs`])。どちらも**値を運ぶだけ**の型で、
//! 書くのは `Metrics::record` が既に取っている鍵の内側 (原子操作は 1 つも増えない)。

use super::{DnsCause, ErrCause};

/// 1 要求ぶんの内訳 (T12.4 (2))。**`Metrics::record` が既に取っている鍵の内側で書く**ので、
/// 原子操作は 1 つも増えない。統計を持たない経路は [`Detail::default`] を渡す (全部 0)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Detail {
    /// 名前解決 (`getaddrinfo`) にかかった時間の合計 (ms)
    pub dns_ms: u64,
    /// そのうち実際に OS へ問い合わせた回数 (キャッシュに当たったら 0)
    pub dns_misses: u64,
    /// 接続 (SYN → 確立) にかかった時間 (ms)。名前解決のぶんは含まない
    pub connect_ms: u64,
    /// 確立した族 (`Some(true)` = IPv6)。分からなければ `None`
    pub family_v6: Option<bool>,
    /// エラーの原因 (エラーでなければ `None`)
    pub cause: Option<ErrCause>,
    /// 名前解決の失敗の内訳 (T25.0)。[`Detail::cause`] が [`ErrCause::Dns`] のときだけ
    /// `Some` ([`Failure::from_io`](super::Failure::from_io) が作る)。`errors_by_cause` の 9・10 番目に足す
    pub dns_cause: Option<DnsCause>,
    /// 履歴の窓に入れる値 (ms)。forward は「初バイトまで」で、応答全体の時間
    /// (`took`) とは別。`None` なら `took` をそのまま使う (CONNECT の確立時間)
    pub first_byte_ms: Option<u64>,
    /// 段階ごとの待ち時間 (T14.3 (1))。`--lite` では時計を読まないので全部 0
    pub stages: StageMs,
    /// 向き別のバイト (T14.26)。`bytes_in` = クライアント → オリジン (上り)、
    /// `bytes_out` = オリジン → クライアント (下り)。
    ///
    /// **新しい計数は 1 つも足していない** ので費用は 0: CONNECT は中継が既に
    /// 方向ごとに持っている `up` / `down` (`crate::recent::ConnTally` に渡すのと
    /// 同じ値)、forward は要求本文と応答のバイト (どちらもアクセスログが既に数えている)
    /// を、ホスト別統計が既に取っている鍵の内側へ運ぶだけ
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// CONNECT のホストと、覗いた SNI が食い違ったか (`PROXY_PEEK_SNI`。T14.38)。
    ///
    /// 立てるのは `tunnel::report` (トンネル 1 本の終わりの 1 回) だけで、forward は
    /// 常に `false`。IP リテラル宛ての CONNECT (T14.7 の `literal_targets`) は
    /// 必ず食い違うので、domain fronting と「宛先を IP で書くクライアント」の両方が入る
    pub sni_mismatch: bool,
    /// この接続を確立するまでに **SYN を送り直した回数** (T14.46)。
    ///
    /// 確立した直後に `getsockopt(TCP_INFO)` を 1 回読んだ `tcpi_total_retrans` で
    /// (まだデータを送っていないので SYN の再送しか入っていない)、`0` は「再送なし /
    /// 読めなかった / Linux 以外」。1 回の再送で確立は 1 秒、2 回で 3 秒に飛ぶので、
    /// **`connect_ms` が 1,000 ms 台の接続はここが 1 以上**になる。
    /// forward は**プールが接続を張ったときだけ**値が入る (使い回した要求は 0)
    pub syn_retrans: u8,
    /// オリジン側のカーネルの RTT (us。T.28.45)。**CONNECT のトンネルの終わり**に
    /// `tunnel::report` が読んだ `tcpi_rtt` (`/recent` の `rtt_ms.origin` と同じ値) を運ぶだけで、
    /// 新しいシステムコールは無い。`0` は「読めなかった / forward」。
    /// `Metrics::record` が `dns_ms + connect_ms − これ` を 5 秒の標本の `excess` の窓に入れる
    pub origin_rtt_us: u32,
}

/// 1 要求 (1 本) の段階ごとの待ち時間 (ms。T14.3 (1))。
///
/// [`Detail`] の中に置いてあるので、書くのは `Metrics::record` が既に取っている
/// 鍵の内側だけ = **原子操作は 1 つも増えない**。熱い経路で増えるのは境目の
/// `Instant::now()` だけ (要求ごとに forward 2 回 / CONNECT 3 回 + 接続ごとに 1 回)。
/// `--lite` では時計も読まない。
///
/// `dns` / `connect` は [`Detail::dns_ms`] / [`Detail::connect_ms`]、forward の `ttfb` は
/// [`Detail::first_byte_ms`] がそのまま段階になるので、ここには持たない
/// (段階の並びは `crate::profile::CONNECT_STAGES` / `crate::profile::FORWARD_STAGES`)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageMs {
    /// accept (または預かり所からの起床) してからワーカーが動き出すまで
    pub queue: u32,
    /// 要求行を読んでから `Host` まで読み終えるまで
    /// (**接続を開けたまま黙っている時間は含めない**)
    pub client_read: u32,
    /// CONNECT: `200 Connection Established` を書いてから最初の中継バイトまで
    pub first_relay: u32,
    /// CONNECT: 中継の合計 (確立から終わりまで − 預けられていた時間)
    pub relay: u32,
    /// CONNECT: 預けられていた合計
    pub park: u32,
    /// forward: 要求をオリジンへ送り終えるまで (名前解決と接続は含めない)
    pub send: u32,
    /// forward: 初バイトから本文を流し終えるまで
    pub body: u32,
}
