//! 1 要求ぶんの内訳 ([`Detail`]) と段階ごとの待ち時間 ([`StageMs`])。どちらも**値を運ぶだけ**の型で、
//! 書くのは `Metrics::record` が既に取っている鍵の内側 (原子操作は 1 つも増えない)。

use std::time::Duration;

use super::{DnsCause, ErrCause, Failure};

/// 1 要求ぶんの内訳 (T12.4)。**`Metrics::record` が既に取っている鍵の内側で書く**ので、
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
    /// 段階ごとの待ち時間 (T14.3)。`--lite` では時計を読まないので全部 0
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
    /// オリジン側のカーネルの RTT (us。T28.45)。**CONNECT のトンネルの終わり**に
    /// `tunnel::report` が読んだ `tcpi_rtt` (`/recent` の `rtt_ms.origin` と同じ値) を運ぶだけで、
    /// 新しいシステムコールは無い。`0` は「読めなかった / forward」。
    /// `Metrics::record` が `dns_ms + connect_ms − これ` を 5 秒の標本の `excess` の窓に入れる
    pub origin_rtt_us: u32,
}

impl Detail {
    /// オリジン (forward) / 宛先 (CONNECT) を掴むまでの内訳を組み立てる (T12.4)。
    /// **接続を試した直後の 1 回だけ**呼ぶ (thread-local を読んで 0 に戻すので、2 回呼ぶと
    /// 2 回目が空になる)。T34.21 で `tunnel::detail_of` と `http::upstream::origin_detail`
    /// (同じ計算が 2 つあった) をここへ寄せた。
    ///
    /// 接続にかかった時間は「全体 (`total`) − 名前解決」で出す (`net` 側に測る口は足さない。
    /// forward の `connect_ms` から TLS の握手が抜けて、別の読み違いを生むため)。
    ///
    /// **`dns_before` は `total` の時計を始める前に既に払われていた名前解決 (ms)**
    /// (T15.0)。`PROXY_ALLOW_LOCAL=false` (既定) では入口の ACL がここより前に
    /// 名前を引くので、`take_resolve_cost` の値には `total` に入っていないぶんが混ざる。
    /// それを引かずに「全体 − 名前解決」をすると**接続の段が 0 に潰れる**
    /// (デプロイ先の実測: ミスした個票 54 本のうち 40 本が `connect` 0 ms で、その 40 本の
    /// カーネル RTT の中央値は 11.4 ms)。同じ理由で `client_read` にはそのぶんが丸ごと
    /// 入っているので、こちらは引く。**時計 (`started`) は動かしていない**ので、
    /// `connect_took` = `/history` の `connect` の窓・`recent_quantiles.connect`・`/slo` は
    /// 1 ms も動かない (Phase 12〜14 の p50 と比べられる値のまま)。
    /// **プールを使い回した forward の要求はこの関数を通らない**ので、そちらの
    /// `client_read` には ACL の名前解決が入ったままになる (要求の 1%。README に 1 行)。
    #[inline]
    pub fn of_connect(
        total: Duration,
        cause: Option<Failure>,
        stages: StageMs,
        dns_before: u64,
    ) -> Detail {
        let (dns_ms, dns_misses) = crate::dns::take_resolve_cost();
        let total_ms = total.as_millis().min(u64::MAX as u128) as u64;
        // `total` の窓の**内側**で払った名前解決だけを引く
        let dns_inside = dns_ms.saturating_sub(dns_before);
        Detail {
            dns_ms,
            dns_misses,
            connect_ms: total_ms.saturating_sub(dns_inside),
            family_v6: crate::dns::take_family(),
            // 確立までに SYN を送り直した回数 (T14.46)。読んだのは `net` の確立点の
            // `getsockopt` 1 回で、ここは thread-local を読んで 0 に戻すだけ。
            // **forward はプールが接続を張った要求だけ** 0 でない (使い回せた要求は
            // `net` の確立点を通らないので 0)
            syn_retrans: crate::dns::take_syn_retrans(),
            // 失敗のときは 8 つの原因と DNS の内訳を 1 回で受け取る (T25.0)
            cause: cause.map(|f| f.cause),
            dns_cause: cause.and_then(|f| f.dns),
            // CONNECT は「確立まで」がそのまま窓に入る値なので指定しない
            // (forward はあとで初バイトを測った側が入れる)
            first_byte_ms: None,
            // ここまでに測った段階 (`queue` / `client_read`) は引き継ぐ (T14.3)。
            // `client_read` には時計より前の名前解決が丸ごと入っているので除く (T15.0)
            stages: StageMs {
                client_read: stages
                    .client_read
                    .saturating_sub(dns_before.min(u32::MAX as u64) as u32),
                ..stages
            },
            // 向き別のバイト (T14.26) は終わってからでないと分からない
            // (forward は `Ctx::log`、CONNECT は `report` が入れる)
            ..Detail::default()
        }
    }
}

/// 1 要求 (1 本) の段階ごとの待ち時間 (ms。T14.3)。
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

/// T34.21 で `proxy-tunnel` の `tunnel::tests` から移した (`detail_of` の試験)。
#[cfg(test)]
mod tests {
    use super::*;

    /// 時計より前に払った名前解決 (入口の ACL) を作る。
    fn acl_paid(ms: u64) -> u64 {
        let _ = crate::dns::take_resolve_cost();
        crate::dns::note_resolve_cost(ms * 1_000, 1);
        crate::dns::peek_resolve_cost().0
    }

    /// 窓の**外**で払った名前解決を引くと接続の段が 0 に潰れる (T15.0)。
    ///
    /// `PROXY_ALLOW_LOCAL=false` (既定) では入口の ACL が `open()` の時計より前に
    /// 名前を引く。デプロイ先の実測では、ミスした個票 54 本のうち 40 本 (74%) が
    /// `connect` 0 ms で、その 40 本のカーネル RTT の中央値は 11.4 ms だった
    /// (TCP の握りが 0 ms のはずがない)。
    #[test]
    fn a_resolve_paid_before_the_clock_does_not_eat_the_connect_stage() {
        let dns_before = acl_paid(12);
        assert_eq!(dns_before, 12);
        let stages = StageMs {
            queue: 2,
            client_read: 15,
            ..StageMs::default()
        };
        let d = Detail::of_connect(Duration::from_millis(9), None, stages, dns_before);
        assert_eq!(
            (d.dns_ms, d.dns_misses),
            (12, 1),
            "払った費用はそのまま残す"
        );
        // 直す前は 9 − 12 → 0 (`saturating_sub`)
        assert_eq!(d.connect_ms, 9, "窓の外の名前解決は引かない");
        assert_eq!(d.stages.client_read, 3, "client_read からは除く (15 − 12)");
        assert_eq!(d.stages.queue, 2, "ほかの段は触らない");
    }

    /// 窓の**内側**で引いたぶんは今までどおり引く (T12.4 の引き算は変えない)。
    #[test]
    fn a_resolve_paid_inside_the_window_is_still_subtracted() {
        let _ = crate::dns::take_resolve_cost();
        crate::dns::note_resolve_cost(6_000, 1);
        let stages = StageMs {
            client_read: 1,
            ..StageMs::default()
        };
        let d = Detail::of_connect(Duration::from_millis(20), None, stages, 0);
        assert_eq!((d.dns_ms, d.connect_ms), (6, 14), "20 − 6");
        assert_eq!(d.stages.client_read, 1, "引く相手がいないので変わらない");
    }

    /// 両方あるとき: 引くのは**窓の内側のぶんだけ**。
    #[test]
    fn only_the_part_paid_inside_the_window_is_subtracted() {
        let dns_before = acl_paid(10);
        // `open()` の中でさらに 4 ms 引いた (`connect_with_timeout` の中のミス)
        crate::dns::note_resolve_cost(4_000, 1);
        let stages = StageMs {
            client_read: 12,
            ..StageMs::default()
        };
        let d = Detail::of_connect(Duration::from_millis(30), None, stages, dns_before);
        assert_eq!((d.dns_ms, d.dns_misses), (14, 2), "費用の合計は 2 回ぶん");
        assert_eq!(d.connect_ms, 26, "30 − 4 (窓の内側の 4 ms だけ引く)");
        assert_eq!(d.stages.client_read, 2, "12 − 10");
    }

    /// 名前解決を 1 度もしていないとき (当たりの経路) は今までと同じ値。
    #[test]
    fn a_cached_name_changes_nothing() {
        let _ = crate::dns::take_resolve_cost();
        let stages = StageMs {
            queue: 1,
            client_read: 4,
            ..StageMs::default()
        };
        let d = Detail::of_connect(Duration::from_millis(8), None, stages, 0);
        assert_eq!((d.dns_ms, d.dns_misses, d.connect_ms), (0, 0, 8));
        assert_eq!((d.stages.queue, d.stages.client_read), (1, 4));
    }
}
