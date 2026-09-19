//! 計測の**型と定数**。ホスト別統計 ([`HostStats`])、1 要求の内訳 ([`Detail`])、
//! エラーの原因 ([`ErrCause`])、`/status` の並べ替えの鍵、応答の形の版。
//!
//! [`crate::metrics::HostStats`] を持ち回る側 (`Metrics` 本体) は
//! `proxy-metrics-core` にある。ここを下の層に置いてあるのは、記録の個票
//! (`proxy-metrics-recent`) がこの型を使い、`Metrics` がその個票を持つ =
//! 間に挟まないと輪になるため (T14.55 で割った)。

use std::fmt::Write as _;
use std::time::Duration;

/// 応答の JSON の形の版 (T14.49)。**全エンドポイントの応答の先頭の鍵** `"schema"` に出る。
///
/// 読む道具 (`scripts/status-diff.py` `scripts/snapshot-diff.py` `scripts/check-dashboard.js`) は
/// 「この JSON はどの版か」を `parts` の有無などで**推測**していた。先頭に版を書いておけば
/// 推測が要らない。**形を変えた (鍵を消す・意味を変える・入れ子を変える) ときは +1** し、
/// README の「応答の形の版 (`schema`) の履歴」の表に 1 行足すこと
/// (**鍵を末尾に足すだけなら上げない** — 読む側は知らない鍵を無視できる)。
///
/// 版 1 = 2026-09-16 の Phase 14 の形。定義はこの 1 か所だけで、`crates/endpoints` は
/// ここを読む。
pub const SCHEMA: u32 = 1;

/// JSON を組み始める先頭 (`{` の代わりにこれを書く = `{"schema":1,`)。
///
/// 組み立ての熱くない経路でも、要求ごとに整形し直す理由が無いので定数にしてある。
/// [`SCHEMA`] と食い違ったら**ビルドが止まる** (下の `const _`)。
pub const SCHEMA_HEAD: &str = "{\"schema\":1,";

// `SCHEMA` と `SCHEMA_HEAD` が食い違わないように (片方だけ直したらここで止まる)。
// 版が 2 桁になったらこの検査ごと書き換えること
const _: () = assert!(
    SCHEMA < 10 && SCHEMA_HEAD.as_bytes()[10] == b'0' + SCHEMA as u8,
    "SCHEMA と SCHEMA_HEAD が食い違っている"
);

/// **入れ子にも使う JSON を、応答そのものとして返すとき**に先頭へ版を足す (T14.49)。
///
/// 使うのは `/blocklist` (引数なしなら `/status` の `blocklist` と同じ状態をそのまま返す)
/// のように、1 つの関数の出力が入れ子と応答の両方になる口だけ。**入れ子の側は版を持たない**
/// (版を持つのは応答の 1 番外側と、`/snapshot` の各部 = それぞれの口の出力そのもの)。
pub fn with_schema(body: &str) -> String {
    match body.strip_prefix('{') {
        // `{}` (空) は `{"schema":N}` に (末尾の `,` を残さない)
        Some("}") => format!("{{\"schema\":{}}}", SCHEMA),
        Some(rest) => format!("{}{}", SCHEMA_HEAD, rest),
        // `{` で始まらないもの (`null` など) は触らない
        None => body.to_string(),
    }
}

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

/// 接続元 IP ごとの統計を持つ上限。
pub const MAX_CLIENTS: usize = 1000;

/// 応答時間ヒストグラムの上限 (ms)。最後の区間は上限なし。
///
/// **1 ms 〜 10 s の 24 段、公比およそ 1.5** (T12.4 (1))。10 段だった頃は
/// デプロイ先の 50 ホスト中 29 (要求数で 74%) が `p50 = p95 = max` になっていた:
/// 257 ms が (250, 500] の 1 区間に全部入り、区間内を補間しても観測した最大値で
/// 頭打ちになるため。段を細かくすると同じ補間のままで分位点が意味を持つ
/// (実測: 257 ms × 90 + 290 ms × 10 の p50 が 290 → 262.5)。
///
/// 値は等比数列を整数に丸めたもので、**下の端 (1〜10 ms) はデプロイ先の
/// AAAA 無しホスト (p50 5.1 ms) が乗るところ**なので公比より細かく取ってある。
pub const LATENCY_BOUNDS_MS: [u64; 24] = [
    1, 2, 3, 4, 6, 9, 13, 20, 30, 45, 65, 95, 140, 210, 315, 470, 700, 1000, 1500, 2200, 3300,
    5000, 7500, 10000,
];

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

/// 要求を読めずに断った理由 (T14.28)。**6 種で固定**。
///
/// 公開ポートには走査 (scanner) の要求が来る。今までは 400 / 414 / 431 で閉じるだけで
/// **何が来たか**の数が無かったので、`/status` の `rejected_requests` (理由別) と
/// `/errors` の個票 (`cause` が `bad_request:<reason>`) に残す。
///
/// [`ErrCause`] にも [`BlockCause`] にも足さないのは [`BlockCause`] と同じ理由
/// (`errors_by_cause` を伸ばすと `.rrd` の標本が領域に収まらず、版を上げて統計を捨てることになる)。
/// **数えるのは断る経路だけ**なので、通した要求には 1 命令も足していない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum BadRequestReason {
    /// 要求行が読めない (空白で 2 つに割れない / `MAX_LINE` を越えて 414 で閉じた)
    RequestLine = 0,
    /// ヘッダーが長すぎる (1 行が長い / 合計が `MAX_HEADER_BYTES` 超 / 行数が多すぎる。431)
    HeaderTooLarge = 1,
    /// メソッドが HTTP のメソッド (token。RFC 9110 §5.6.2) として読めない
    Method = 2,
    /// オリジン形式 (`/path`) なのに `Host` が無い
    NoHost = 3,
    /// 絶対 URI (`http://…`) やマッピング形式 (`/https/…`) にホストが無い
    BadUri = 4,
    /// 本文の枠が不正 (`Content-Length` と `Transfer-Encoding: chunked` が両方ある = 要求の密輸)
    BodyFraming = 5,
}

/// [`BadRequestReason`] の数 (`rejected_requests` の配列の長さ)。
pub const BAD_REQUEST_REASONS: usize = 6;

/// `/status` の鍵と `/metrics` のラベルに使う名前 ([`BadRequestReason`] と同じ順)。
pub const BAD_REQUEST_REASON_NAMES: [&str; BAD_REQUEST_REASONS] = [
    "request_line",
    "header_too_large",
    "method",
    "no_host",
    "bad_uri",
    "body_framing",
];

impl BadRequestReason {
    /// `/status` の鍵と `/metrics` のラベルに使う名前。
    pub fn name(self) -> &'static str {
        BAD_REQUEST_REASON_NAMES[self as usize]
    }

    /// `/errors` の `cause` に出す名前 (`bad_request:<reason>`)。
    pub fn cause_name(self) -> &'static str {
        match self {
            BadRequestReason::RequestLine => "bad_request:request_line",
            BadRequestReason::HeaderTooLarge => "bad_request:header_too_large",
            BadRequestReason::Method => "bad_request:method",
            BadRequestReason::NoHost => "bad_request:no_host",
            BadRequestReason::BadUri => "bad_request:bad_uri",
            BadRequestReason::BodyFraming => "bad_request:body_framing",
        }
    }

    /// 符号 (0〜5) から戻す。知らない値は [`BadRequestReason::RequestLine`]。
    pub fn from_index(v: usize) -> BadRequestReason {
        match v {
            1 => BadRequestReason::HeaderTooLarge,
            2 => BadRequestReason::Method,
            3 => BadRequestReason::NoHost,
            4 => BadRequestReason::BadUri,
            5 => BadRequestReason::BodyFraming,
            _ => BadRequestReason::RequestLine,
        }
    }

    /// 空白で 2 つに割れなかった要求行の理由を決める。
    ///
    /// **断る経路からしか呼ばない**ので、文字列を見ても熱い経路には乗らない
    /// ([`ErrCause::from_io`] と同じ作法)。メソッドらしきものが token として
    /// 読めなければ [`BadRequestReason::Method`] (走査が投げるゴミ)、
    /// 読めるなら要求行そのものの形が足りない ([`BadRequestReason::RequestLine`])。
    pub fn of_request_line(line: &str) -> BadRequestReason {
        match line.split_whitespace().next() {
            Some(m) if !is_method_token(m) => BadRequestReason::Method,
            _ => BadRequestReason::RequestLine,
        }
    }

    /// 要求ターゲットが解けなかったときの理由を決める (**断る経路からだけ**)。
    ///
    /// `proxy-origin` の `parse_origin` / `target_host` が `Err` を返すのは 2 通りしか
    /// 無い: スキーム付き (`http://` `https://`) とマッピング形式 (`/http/` `/https/`) で
    /// ホストが空 = [`BadRequestReason::BadUri`]、オリジン形式 (`/path`) で `Host` が
    /// 無い = [`BadRequestReason::NoHost`]。メソッドが token として読めなければ
    /// そちらを先に返す (要求行ごとゴミなら理由は `method` の方が読む人に近い)。
    pub fn of_target(method: &str, target: &str) -> BadRequestReason {
        if !is_method_token(method) {
            return BadRequestReason::Method;
        }
        const WITH_HOST: [&str; 4] = ["http://", "https://", "/http/", "/https/"];
        if WITH_HOST.iter().any(|p| target.starts_with(p)) {
            BadRequestReason::BadUri
        } else {
            BadRequestReason::NoHost
        }
    }
}

/// メソッドが HTTP の token (RFC 9110 §5.6.2) として読めるか。**断る経路からだけ**呼ぶ。
fn is_method_token(m: &str) -> bool {
    !m.is_empty()
        && m.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
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
    /// ループバック・リンクローカル宛て (`PROXY_ALLOW_LOCAL=off` の SSRF 除け)
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
            K::TimedOut => ErrCause::Timeout,
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

/// 1 要求ぶんの内訳 (T12.4 (2))。**[`Metrics::record`] が既に取っている鍵の内側で書く**ので、
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
    /// 履歴の窓に入れる値 (ms)。forward は「初バイトまで」で、応答全体の時間
    /// (`took`) とは別。`None` なら `took` をそのまま使う (CONNECT の確立時間)
    pub first_byte_ms: Option<u64>,
    /// 段階ごとの待ち時間 (T14.3 (1))。`--lite` では時計を読まないので全部 0
    pub stages: StageMs,
    /// 向き別のバイト (T14.26)。`bytes_in` = クライアント → オリジン (上り)、
    /// `bytes_out` = オリジン → クライアント (下り)。
    ///
    /// **新しい計数は 1 つも足していない** ので費用は 0: CONNECT は中継が既に
    /// 方向ごとに持っている `up` / `down` ([`crate::recent::ConnTally`] に渡すのと
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
}

/// 1 要求 (1 本) の段階ごとの待ち時間 (ms。T14.3 (1))。
///
/// [`Detail`] の中に置いてあるので、書くのは [`Metrics::record`] が既に取っている
/// 鍵の内側だけ = **原子操作は 1 つも増えない**。熱い経路で増えるのは境目の
/// `Instant::now()` だけ (要求ごとに forward 2 回 / CONNECT 3 回 + 接続ごとに 1 回)。
/// `--lite` では時計も読まない。
///
/// `dns` / `connect` は [`Detail::dns_ms`] / [`Detail::connect_ms`]、forward の `ttfb` は
/// [`Detail::first_byte_ms`] がそのまま段階になるので、ここには持たない
/// (段階の並びは [`crate::profile::CONNECT_STAGES`] / [`crate::profile::FORWARD_STAGES`])。
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

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostStats {
    pub requests: u64,
    pub hits: u64,
    pub misses: u64,
    pub bypass: u64,
    pub errors: u64,
    /// ACL / ブロックリストで拒否した数
    pub blocked: u64,
    pub bytes: u64,
    /// 応答時間を記録した要求数と、その合計・最大 (ms)。CONNECT は接続確立までの時間
    pub timed: u64,
    pub duration_ms_sum: u64,
    pub duration_ms_max: u64,
    /// `LATENCY_BOUNDS_MS` の区間ごとの件数 (+ 上限なしの区間)
    pub buckets: [u64; LATENCY_BOUNDS_MS.len() + 1],
    /// 最後に要求を受けた時刻 (epoch 秒)
    pub last_seen: u64,
    /// 名前解決にかかった時間の合計 (ms) と、OS へ問い合わせた回数 (T12.4 (2))
    pub dns_ms_sum: u64,
    pub dns_misses: u64,
    /// 接続 (SYN → 確立) にかかった時間の合計 (ms)。名前解決のぶんは含まない
    pub connect_ms_sum: u64,
    /// 確立した族の内訳 (T12.1 の全体の統計と二重にならないよう、ここはホスト別)
    pub v4_wins: u64,
    pub v6_wins: u64,
    /// エラーの原因別の件数 ([`ErrCause`] の順)
    pub errors_by_cause: [u64; ERR_CAUSES],
    /// カーネルの平滑化 RTT (`TCP_INFO`) の標本 (T14.5)。**要求ごとではなく接続の
    /// 終わりに 1 本 1 回**なので `timed` とは数が合わない。ホスト別はオリジン側、
    /// 接続元別 ([`ClientStats`]) はクライアント側の値が入る
    pub rtt_us_sum: u64,
    pub rtt_us_min: u64,
    pub rtt_samples: u64,
    /// その接続たちが再送したセグメントの通算 (`tcpi_total_retrans`)
    pub retrans: u64,
    /// 向き別の転送バイト (T14.26)。`bytes_in` = クライアント → オリジン (上り)、
    /// `bytes_out` = オリジン → クライアント (下り)。**ここまでが `.rrd` に残る欄**
    /// ([`HostStats::encode`] の並びと同じ順)。
    ///
    /// CONNECT は `bytes == bytes_in + bytes_out` だが、**forward の `bytes` は
    /// 今までどおり応答のぶんだけ**なので (欄の意味は変えない)、上りを足すと
    /// `bytes_in + bytes_out` の方が大きくなる
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// ホスト別の時系列 ([`crate::hostseries`]) の枠の番号。**直近 1 時間の要求数で
    /// 上位 16 に居る間だけ** `Some` で、入れ替えるのは history スレッド (T14.22)。
    /// 要求の経路はこの旗を見るだけ。`.rrd` には書かない欄なので
    /// [`HostStats::encode`] / [`HostStats::decode`] は 1 バイトも変えていない
    pub series_slot: Option<u8>,
    /// CONNECT のホストと SNI が食い違った本数 (`PROXY_PEEK_SNI`。T14.38)。
    /// **`.rrd` には書かない** (スロットの余白は 4 B しか無い。T14.26) ので
    /// 再起動で 0 に戻る = `series_slot` と同じ扱い。合計は `/status` の `sni_mismatches`
    pub sni_mismatch: u64,
    /// そのホストへの接続が **SYN を送り直した回数の合計** (T14.46)。
    /// **`.rrd` には書かない** ([`HostStats::sni_mismatch`] と同じ扱い。スロットの
    /// 余白は 4 B しか無い) ので再起動で 0 に戻る。合計は `/status` の `syn_retrans_total`。
    ///
    /// [`HostStats::retrans`] (T14.5) は**接続の終わり**に読んだ通算 (データの再送を含む)、
    /// こちらは**確立の直後**なので SYN の再送だけ = 「繋ぐのに何回やり直したか」
    pub syn_retrans: u64,
}

impl HostStats {
    /// `now` は epoch 秒。**呼ぶ側が 1 回だけ読んだ値**を渡す (直近の標本 (T14.31) と
    /// `last_seen` で同じ値を使い回すため。壁時計を読む回数は今までと変わらない)。
    ///
    /// **呼ぶのは上の層 (`proxy-metrics-core` の `Metrics`) だけ** (T14.55 でクレートを
    /// 割るまでは同じクレートの中の `fn` だった)。
    pub fn count(
        &mut self,
        now: u64,
        outcome: HostOutcome,
        bytes: u64,
        took: Option<Duration>,
        detail: &Detail,
    ) {
        self.requests += 1;
        self.bytes += bytes;
        self.last_seen = now;
        match outcome {
            HostOutcome::Hit => self.hits += 1,
            HostOutcome::Miss => self.misses += 1,
            HostOutcome::Bypass => self.bypass += 1,
            HostOutcome::Error => self.errors += 1,
            HostOutcome::Blocked => self.blocked += 1,
        }
        if let Some(d) = took {
            self.observe(d);
        }
        self.add_detail(detail);
    }

    /// 内訳を足す (鍵の内側。全部 0 の [`Detail::default`] でも同じ道を通る)。
    fn add_detail(&mut self, d: &Detail) {
        self.dns_ms_sum += d.dns_ms;
        self.dns_misses += d.dns_misses;
        self.connect_ms_sum += d.connect_ms;
        match d.family_v6 {
            Some(true) => self.v6_wins += 1,
            Some(false) => self.v4_wins += 1,
            None => {}
        }
        if let Some(c) = d.cause {
            self.errors_by_cause[c as usize] += 1;
        }
        // 向き別のバイト (T14.26)。既に数えてあるものを 2 つに分けて運んできただけなので、
        // ここで増えるのは足し算 2 回だけ (鍵も原子操作もシステムコールも増えない)
        self.bytes_in += d.bytes_in;
        self.bytes_out += d.bytes_out;
        // CONNECT のホストと SNI の食い違い (T14.38)。**旗は `tunnel::report` が
        // 立てたもの**で、ここは鍵の内側の足し算 1 回 (メモリだけの欄)
        if d.sni_mismatch {
            self.sni_mismatch += 1;
        }
        // 確立までの SYN の再送 (T14.46)。**読んだのは `net` の確立点**で、
        // ここは鍵の内側の足し算 1 回 (メモリだけの欄)
        self.syn_retrans += d.syn_retrans as u64;
    }

    /// 状態ファイルのレコード (名前 128 バイト + 数値)。
    pub fn encode(&self, name: &str) -> Vec<u8> {
        let mut e = crate::rrd::Enc::new();
        e.str(name, 128)
            .u64(self.last_seen)
            .u64(self.requests)
            .u64(self.hits)
            .u64(self.misses)
            .u64(self.bypass)
            .u64(self.errors)
            .u64(self.blocked)
            .u64(self.bytes)
            .u64(self.timed)
            .u64(self.duration_ms_sum)
            .u64(self.duration_ms_max);
        for b in self.buckets {
            e.u64(b);
        }
        e.u64(self.dns_ms_sum)
            .u64(self.dns_misses)
            .u64(self.connect_ms_sum)
            .u64(self.v4_wins)
            .u64(self.v6_wins);
        for c in self.errors_by_cause {
            e.u64(c);
        }
        // 欄は**末尾に足す**だけ (T14.5 の 4 欄で名前 128 B + 53 項目 = 552 B)
        e.u64(self.rtt_us_sum)
            .u64(self.rtt_us_min)
            .u64(self.rtt_samples)
            .u64(self.retrans);
        // T14.26 の 2 欄も同じ作法で末尾へ (552 → 568 B)。版 3 の 1 スロットは 640 B =
        // ペイロード 636 B なので**ここから下が予備の 68 B** (T14.14。8 項目ぶん)。
        // 足しても版は上がらない (古いファイルはその位置がゼロ埋めなので 0 で読み戻る)
        e.u64(self.bytes_in).u64(self.bytes_out);
        e.0
    }

    pub fn decode(payload: &[u8]) -> Option<(String, HostStats)> {
        let mut d = crate::rrd::Dec(payload);
        let name = d.str(128);
        if name.is_empty() {
            return None;
        }
        let mut s = HostStats {
            last_seen: d.u64(),
            requests: d.u64(),
            hits: d.u64(),
            misses: d.u64(),
            bypass: d.u64(),
            errors: d.u64(),
            blocked: d.u64(),
            bytes: d.u64(),
            timed: d.u64(),
            duration_ms_sum: d.u64(),
            duration_ms_max: d.u64(),
            ..HostStats::default()
        };
        for b in s.buckets.iter_mut() {
            *b = d.u64();
        }
        s.dns_ms_sum = d.u64();
        s.dns_misses = d.u64();
        s.connect_ms_sum = d.u64();
        s.v4_wins = d.u64();
        s.v6_wins = d.u64();
        for c in s.errors_by_cause.iter_mut() {
            *c = d.u64();
        }
        // 短いレコード (T14.5 より前に書かれた行、版 2 から詰め直した行) は
        // ここで尽きて 0 が返る
        s.rtt_us_sum = d.u64();
        s.rtt_us_min = d.u64();
        s.rtt_samples = d.u64();
        s.retrans = d.u64();
        // 同じく T14.26 より前のファイルはここで尽きて 0 が返る (向きの分からない
        // 昔の転送量は `bytes` にだけ入っている)。**ここから下が予備** — 欄を足す
        // ならこの位置から (T14.14)
        s.bytes_in = d.u64();
        s.bytes_out = d.u64();
        Some((name, s))
    }

    /// カーネルの RTT と再送を 1 標本足す (**接続の終わりに 1 回だけ**。T14.5)。
    /// 呼び出し側が既に鍵を取っているので、原子操作もシステムコールも増えない。
    /// **呼ぶのは上の層 (`proxy-metrics-core` の `Metrics`) だけ** (T14.55 でクレートを
    /// 割るまでは同じクレートの中の `fn` だった)。
    pub fn observe_rtt(&mut self, rtt_us: u32, retrans: u32) {
        let us = rtt_us as u64;
        self.rtt_us_sum = self.rtt_us_sum.saturating_add(us);
        self.rtt_samples += 1;
        if self.rtt_us_min == 0 || us < self.rtt_us_min {
            self.rtt_us_min = us;
        }
        self.retrans = self.retrans.saturating_add(retrans as u64);
    }

    /// RTT の平均 (ms)。標本が無ければ `None` (Linux 以外・`--lite` では出ない)。
    pub fn rtt_avg_ms(&self) -> Option<f64> {
        (self.rtt_samples > 0).then(|| self.rtt_us_sum as f64 / self.rtt_samples as f64 / 1000.0)
    }

    /// RTT の最小 (ms)。標本が無ければ `None`。
    pub fn rtt_min_ms(&self) -> Option<f64> {
        (self.rtt_samples > 0).then(|| self.rtt_us_min as f64 / 1000.0)
    }

    fn observe(&mut self, d: Duration) {
        let ms = d.as_millis().min(u64::MAX as u128) as u64;
        self.timed += 1;
        self.duration_ms_sum += ms;
        self.duration_ms_max = self.duration_ms_max.max(ms);
        let idx = LATENCY_BOUNDS_MS
            .iter()
            .position(|&b| ms <= b)
            .unwrap_or(LATENCY_BOUNDS_MS.len());
        self.buckets[idx] += 1;
    }

    pub fn avg_ms(&self) -> f64 {
        if self.timed == 0 {
            0.0
        } else {
            self.duration_ms_sum as f64 / self.timed as f64
        }
    }

    /// 区間内を線形に補間した分位点 (ms)。最後の区間は最大値で頭打ち。
    pub fn quantile_ms(&self, q: f64) -> f64 {
        if self.timed == 0 {
            return 0.0;
        }
        let rank = (q.clamp(0.0, 1.0) * self.timed as f64).max(1.0);
        let mut seen = 0u64;
        for (i, &n) in self.buckets.iter().enumerate() {
            if n == 0 {
                continue;
            }
            if (seen + n) as f64 >= rank {
                let lo = if i == 0 {
                    0.0
                } else {
                    LATENCY_BOUNDS_MS[i - 1] as f64
                };
                let hi = if i < LATENCY_BOUNDS_MS.len() {
                    LATENCY_BOUNDS_MS[i] as f64
                } else {
                    (self.duration_ms_max as f64).max(lo)
                };
                let frac = (rank - seen as f64) / n as f64;
                // 観測した最大値は超えない (件数が少ないとき区間の上端が出ないように)
                return (lo + (hi - lo) * frac).min(self.duration_ms_max as f64);
            }
            seen += n;
        }
        self.duration_ms_max as f64
    }

    pub fn error_rate(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.errors as f64 / self.requests as f64
        }
    }
}

/// 接続元ごとに覚えておく `User-Agent` の種類 (T14.7)。
///
/// 1 つの IP の裏に複数の端末が居る (NAT) ことがあるので 1 つでは足りず、
/// 並べても読めないので 4 つ。あふれた分は数えるだけ (`agents_dropped`)。
pub const MAX_CLIENT_AGENTS: usize = 4;

/// `User-Agent` を覚えておく長さ (バイト)。
///
/// 個票に入れてよい**唯一のヘッダー**で、先頭 128 バイトだけ (Phase 14 の共通の決まり:
/// URL のパスも問い合わせ文字列も本文も入れない)。
pub const MAX_AGENT_BYTES: usize = 128;

/// 接続元ごとに数える宛先の種類の上限。超えたら `distinct_targets_capped` を立てる。
pub const MAX_CLIENT_TARGETS: usize = 256;

/// 接続元ごとに覚えておくポートの種類。あふれた分は `ports_other` にまとめる。
pub const MAX_CLIENT_PORTS: usize = 8;

/// **初めて見た接続元** 1 件の写し ([`Metrics::clients_first_seen_in`] が返す。T14.54)。
///
/// [`ClientStats`] を丸ごと写すと宛先の集合 (最大 [`MAX_CLIENT_TARGETS`] 件) まで
/// 付いてくるので、`/events` の 1 行に要るものだけを持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewClient {
    /// 接続元 (**表の鍵そのまま**。あふれた分の `other` もここに来る。
    /// `PROXY_RECORDS=hashed` (T14.41) では鍵が 16 進なので、この値もそれになる)
    pub client: String,
    /// 初めて見た時刻 (epoch 秒)
    pub first_seen: u64,
    /// 最後に名乗った `User-Agent` (無ければ `None`)
    pub agent: Option<String>,
    /// ここまでに数えた要求数
    pub requests: u64,
    /// **最初の宛先の種類**: 最初に使ったポート ([`ClientStats::ports`] は出た順に
    /// 並ぶので先頭が最初の宛先のもの。1 つも無ければ `None`)
    pub port: Option<u16>,
    /// 同じく、IP リテラル宛てがあったか (名前を引かずに繋いでいる = 普通の閲覧ではない)
    pub literal: bool,
}

/// `/clients` を並べる鍵 (`?sort=`。T14.7)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClientSort {
    /// 要求数の多い順 (既定。`/status` の `clients[]` と同じ並び)
    #[default]
    Requests,
    /// 最後に要求を受けた時刻の新しい順
    Recent,
    /// 宛先ホストの種類が多い順
    Targets,
    /// IP リテラル宛ての要求が多い順
    Literal,
}

impl ClientSort {
    /// `?sort=` の値から決める。**知らない値は既定 (`requests`) に倒す**
    /// ([`HostSort::from_param`] と同じ方針)。
    pub fn from_param(v: &str) -> Self {
        match v {
            "recent" => ClientSort::Recent,
            "targets" => ClientSort::Targets,
            "literal" => ClientSort::Literal,
            _ => ClientSort::Requests,
        }
    }

    /// `/clients` の `"sort"` に出す名前。
    pub fn name(self) -> &'static str {
        match self {
            ClientSort::Requests => "requests",
            ClientSort::Recent => "recent",
            ClientSort::Targets => "targets",
            ClientSort::Literal => "literal",
        }
    }
}

/// `/status` の JSON に埋め込む、上の層が用意する部品。
///
/// 既定 (`Default`) は `"null"`。単体テストのように上の層が居ないときに使う。
pub struct StatusExtras<'a> {
    /// `.env` の再読込の状態 (`reload::status_json()`)
    pub settings: &'a str,
    /// ブロックリストの状態 (`blocklist::status_json()`)
    pub blocklist: &'a str,
    /// 状態ファイルの状態 (`persist::status_json()`)
    pub state_file: &'a str,
    /// 動いているバイナリの版 (本体クレートの `VERSION`。`0.1.0+144b992` の形)
    pub version: &'a str,
    /// 上限といまのスレッドの数 ([`Concurrency`])
    pub concurrency: Concurrency,
    /// `hosts[]` の上位 50 をどの鍵で切り出すか (`/status?sort=`。T13.3)
    pub sort: HostSort,
}

impl Default for StatusExtras<'_> {
    fn default() -> Self {
        StatusExtras {
            settings: "null",
            blocklist: "null",
            state_file: "null",
            version: "unknown",
            concurrency: Concurrency::default(),
            sort: HostSort::Requests,
        }
    }
}

/// 上限と、いまの接続スレッドの数 (`/status` 用)。
///
/// `auto` で決まった上限を**起動ログを見なくても確かめられる**ようにするためのもの
/// (`PROXY_MAX_CONNS` は T8.5、`PROXY_MAX_THREADS` と待ち行列は T10.5 のやり残し)。
/// 値を決めるのは上の層 (`Config` と `Workers`) で、ここは受け取って並べるだけ。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Concurrency {
    /// 同時接続数の上限 (`PROXY_MAX_CONNS` が `auto` なら決まった値。`0` で無制限)
    pub max_conns: usize,
    /// 生きていてよい接続スレッドの上限 (`PROXY_MAX_THREADS`。`0` で無制限)
    pub max_threads: usize,
    /// いま生きている接続スレッドの数
    pub live_threads: usize,
    /// そのうち空き置き場に積んである数 (仕事を待っているスレッド)
    pub idle_threads: usize,
    /// 上限に達して待たせている仕事の数 (捨てていない)
    pub queued_jobs: usize,
}

/// `/status` の `hosts[]` から上位 50 を切り出す鍵 (`?sort=`。T13.3)。
///
/// **切り出す鍵だけ**を変えるもので、JSON の形も件数も変わらない。要求数の上位 50 には
/// 「悪いホスト」が出てこないのが動機で、デプロイ後 58.6 時間の実測では
/// **エラー 99 件のうち 80 件 (名前解決の失敗) を抱えたホストが 1 つも上位 50 に居なかった**
/// (上位 50 のエラーは全部 0)。`clients[]` は要求数順のまま (接続元には内訳が無いので、
/// 名前解決や確立の鍵で並べても全部 0 の同点になるだけ)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostSort {
    /// 要求数の多い順 (既定。今までの順)
    #[default]
    Requests,
    /// エラー件数の多い順
    Errors,
    /// 名前解決に費やした合計 (`dns_ms_sum`) の大きい順
    Dns,
    /// 応答 (CONNECT は確立) の平均 (`avg_ms`) の遅い順
    Slow,
}

impl HostSort {
    /// `?sort=` の値から決める。**知らない値は既定 (`requests`) に倒す**
    /// (綴り違いで 400 を返すより、今までどおりの応答を返す方が監視の口として安全)。
    pub fn from_param(v: &str) -> Self {
        match v {
            "errors" => HostSort::Errors,
            "dns" => HostSort::Dns,
            "slow" => HostSort::Slow,
            _ => HostSort::Requests,
        }
    }
}

/// ホスト別統計の上限。超えた分は `other` にまとめる。
pub const MAX_HOSTS: usize = 1000;

/// ホスト別 / 接続元別に共通の統計フィールド (先頭・末尾の波括弧なし)。
///
/// `detail` はホスト別だけ (T12.4 (2))。接続元別には名前解決も接続も族も無いので、
/// 全部 0 の列を 50 行ぶん並べても `/status` が太るだけになる。
/// 向き別のバイト (`bytes_in` / `bytes_out`。T14.26) は**両方に出す**: 接続元にも
/// 「この端末は上りが主か下りが主か」があり、どちらも実際に数えた値が入る。
///
/// 外から呼べるのは `/hosts` (T13.4) が **`/status` の `hosts[]` と同じ形**で
/// 全ホストを出すため。形が 2 つに分かれると `scripts/status-diff.py` が両方を
/// 読めなくなるので、組み立てはこの 1 か所に置く。
pub fn stats_json(s: &HostStats, detail: bool) -> String {
    let mut out = format!(
        "\"requests\":{},\"hits\":{},\"misses\":{},\"bypass\":{},\"errors\":{},\"blocked\":{},\"bytes\":{},\"bytes_in\":{},\"bytes_out\":{},\"timed\":{},\"avg_ms\":{:.1},\"p50_ms\":{:.1},\"p95_ms\":{:.1},\"max_ms\":{},\"last_seen\":{}",
        s.requests,
        s.hits,
        s.misses,
        s.bypass,
        s.errors,
        s.blocked,
        s.bytes,
        s.bytes_in,
        s.bytes_out,
        s.timed,
        s.avg_ms(),
        s.quantile_ms(0.5),
        s.quantile_ms(0.95),
        s.duration_ms_max,
        s.last_seen
    );
    // カーネルの RTT (T14.5)。標本が無ければ `null` (Linux 以外・`--lite`・
    // まだ 1 本も閉じていない)。ホスト別はオリジン側、接続元別はクライアント側
    match (s.rtt_avg_ms(), s.rtt_min_ms()) {
        (Some(avg), Some(min)) => {
            let _ = write!(
                out,
                ",\"rtt_ms\":{{\"avg\":{:.3},\"min\":{:.3},\"samples\":{}}},\"retrans\":{}",
                avg, min, s.rtt_samples, s.retrans
            );
        }
        _ => out.push_str(",\"rtt_ms\":null,\"retrans\":0"),
    }
    if detail {
        let _ = write!(
            out,
            ",\"dns_ms_sum\":{},\"dns_misses\":{},\"connect_ms_sum\":{},\"v4_wins\":{},\"v6_wins\":{},\"errors_by_cause\":[",
            s.dns_ms_sum, s.dns_misses, s.connect_ms_sum, s.v4_wins, s.v6_wins
        );
        for (i, c) in s.errors_by_cause.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}", c);
        }
        out.push(']');
        // CONNECT のホストと SNI の食い違い (T14.38)。**ホスト別だけ** (接続元別には
        // 宛先が無い)。**末尾に足した** ので既存の鍵の順は変わらない
        let _ = write!(out, ",\"sni_mismatch\":{}", s.sni_mismatch);
        // 確立までの SYN の再送 (T14.46)。**ホスト別だけ** (`sni_mismatch` と同じく
        // `detail` の内側 — 接続元別には「繋ぎに行く先」が無い)。
        // **末尾に足した**ので既存の鍵の順は変わらない
        let _ = write!(out, ",\"syn_retrans\":{}", s.syn_retrans);
    }
    out
}
