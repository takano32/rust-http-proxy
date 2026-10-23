//! プロセスのメモリだけに持つ「個票」(T13.4)。
//!
//! `/status` は集計、`/history` は時系列で、どちらも「**誰が・いつ・なぜ**」を答えられない。
//! デプロイ後 58.6 時間の分析では、約 2 秒かかって失敗した名前解決の相手も、バーストで
//! 218 本を占めていた接続の中身も、エラー 99 件の相手と時刻も読めなかった (T13.0)。
//!
//! ここに置くのは**固定長のリング**で、統計の `.rrd` には書かない。代わりに
//! **隣の固定長ファイル `$HOME/.rust-http-proxy.recent`** に history スレッドが
//! 5 秒ごとに追記し、次の起動で読み戻す ([`crate::persist_recent`]。T14.9)。
//! 各リングが持つ「まだ書いていない位置」の印がその口で、**接続の経路は今までどおり
//! メモリのリングに書くだけ**。書く場所は 2 つだけ:
//!
//! - [`ErrorRing`]: エラーを 1 件返したときだけ ([`Metrics::record_error`] 経由)。
//!   **成功の熱い経路は 1 命令も通らない。**
//! - [`ConnTable`]: いまの接続の一覧 (`/connections`)。**表の鍵を取るのは
//!   接続の開始と終了の 2 回だけ**で、状態と転送バイトは [`ConnSlot`] の原子に書く
//!   (`PROXY_MAX_CONNS_PER_CLIENT` を設定したときだけ、accept の判定で 1 回増える。T14.13)。
//! - [`RecentRing`]: 閉じた接続の個票 (`/recent`。T14.4)。接続の終了で 1 回。
//! - [`BurstRing`]: 同時接続数が上限の一定割合を越えた瞬間の写真 (`/bursts`。T14.6)。
//!   **撮るのは history スレッド**で、越えた接続を受けたスレッドは旗を立てるだけ。
//! - [`ClosedCounts`]: 閉じた理由・寿命・バイト・預けの分布 (`/history` の `closed`。T14.6)。
//!   足すのは `/recent` に 1 件書くのと同じ場所で、**既に鍵の内側**。
//!   これだけは**メモリだけ** (T14.9 が残すのは上の 4 本のリングと `/log`)。
//!
//! **`PROXY_RECORDS` (T14.41)**: 上のどのリングも、書き込みの入口に
//! [`crate::records::recording`] の分岐が 1 つある。`off` なら**鍵も取らずに捨てる**
//! (`/hosts` と `/history` は接続元を含まないのでそのまま残る)。`hashed` のときは
//! 接続元 IP を [`crate::records::client_key`] で 16 桁の 16 進に置き換えて記録する —
//! 通すのは [`ErrorEntry::new`] と [`ConnTable::register`] の 2 か所だけで、
//! `/recent` (枠から作る) も `/bursts` (表から作る) もそこから流れてくる。
//!
//! **T25.2 で 3 ファイルに割った** (T24.8 の案 A)。エラーの個票とリングは
//! `recent/errors.rs`、接続の表とリングの 1 枠は `recent/conns.rs` にあり、ここには
//! 閉じた接続の個票・バースト・閉じた分布と、その 3 つの試験を置く。行を動かしただけで
//! 中身は変えていない。`pub use` で出し直すので、外からの `recent::…` の経路は変わらない。
//!
//! [`Metrics::record_error`]: crate::metrics::Metrics::record_error

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::metrics::{DnsCause, ErrCause, Failure};
use crate::sync::LockExt;

mod conns;
mod errors;

pub use conns::*;
pub use errors::*;

/// 閉じた接続の個票を何件覚えておくか (固定。T14.4)。
///
/// デプロイ先は 72.7 時間で接続 3,100 本 = 43 本/時 なので、2,000 件あれば
/// **山のあった時間帯を丸ごと** さかのぼれる。1 件は下の切り詰めで 256 B 以内に収まる。
pub const MAX_RECENT: usize = 2000;

/// 閉じた接続の個票に収める宛先の長さ (バイト)。
///
/// `/errors` (500 件) より短いのは、2,000 件 × 1 件 256 B の枠に**寿命やバイトや段階の ms も
/// 一緒に**入れるため。`host:port` は実際には 20〜40 B なので、切れるのは異様に長い名前だけ。
pub const MAX_RECENT_TARGET: usize = 48;

/// 個票に収める SNI の長さ (バイト。T14.38)。
///
/// 覗いた ClientHello の `server_name` はここで切る (DNS の名前の上限は 255 B だが、
/// 実際の名前は 10〜40 B)。個票のファイル ([`crate::persist_recent`]) の欄も
/// この長さ + 4 B で取ってある。
pub const MAX_SNI: usize = 64;

/// 段階の ms の数 ([`STAGE_NAMES`] と同じ並び)。
pub const STAGES: usize = 6;

/// 段階の ms の名前 (`/recent` の `ms` に出す鍵)。
///
/// 先頭 3 つ (`dns` / `connect` / `first_byte`) は [`crate::metrics::Detail`] にあるもの、
/// 残り 3 つ (`queue` / `client_read` / `first_relay`) は T14.3 が足した
/// [`crate::metrics::StageMs`] から入る (0 の段階は JSON に出さない)。
pub const STAGE_NAMES: [&str; STAGES] = [
    "dns",
    "connect",
    "first_byte",
    "queue",
    "client_read",
    "first_relay",
];

/// カーネルの RTT と再送を持つ側の数 (クライアント側とオリジン側。T14.5)。
pub const SIDES: usize = 2;
/// [`RecentEntry::rtt_us`] / [`ConnTally::rtt_us`] の添字。
pub const CLIENT_SIDE: usize = 0;
pub const ORIGIN_SIDE: usize = 1;
/// 側の名前 ([`CLIENT_SIDE`] / [`ORIGIN_SIDE`] の並び。`half_closed` の値。T15.0 (4))。
pub const SIDE_NAMES: [&str; SIDES] = ["client", "origin"];

/// [`STAGE_NAMES`] の添字。
pub const STAGE_DNS: usize = 0;
pub const STAGE_CONNECT: usize = 1;
pub const STAGE_FIRST_BYTE: usize = 2;
/// accept (または預かり所からの起床) からワーカーが動き出すまで (T14.3 の `StageMs::queue`)
pub const STAGE_QUEUE: usize = 3;
/// 要求行が届いてから `Host` まで読み終えるまで (T14.3 の `StageMs::client_read`)
pub const STAGE_CLIENT_READ: usize = 4;
/// CONNECT: `200` を書いてから最初の中継バイトまで (T14.3 の `StageMs::first_relay`)
pub const STAGE_FIRST_RELAY: usize = 5;

/// 接続が閉じた理由 (`/recent` の `reason`)。
///
/// 9 種類しかないのは、読む人が「次に何を見るか」を変えられる粒度で切ったため
/// ([`ErrCause`] と同じ方針)。`Error` だけは原因を連れて `error:refused` のように出す。
/// **`/history` の分布 ([`CLOSE_REASON_NAMES`]) は 8 列のまま**で、`ClientDead` は
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
}

impl CloseReason {
    /// リングに書く前の符号 (0 = まだ決まっていない)。[`ConnSlot`] の原子に入れ、
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
            _ => CloseReason::Shutdown,
        }
    }

    /// 分布の添字 ([`CLOSE_REASON_NAMES`] の並び。T14.6)。
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
            CloseReason::Error(_) | CloseReason::ErrorDns(_) => 7,
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
        }
    }
}

/// 1 接続ぶんの「個票に足す値」(T14.4)。
///
/// **原子ではない。** 1 本の接続を同時に触るスレッドは 1 つだけなので、要求ごとの
/// 積み上げは [`std::cell::Cell`] に置いた普通の変数で行い、**接続の終了で 1 回だけ**
/// [`ConnSlot::finish`] で個票へ移す (要求ごとの原子操作を 1 つも増やさないため)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnTally {
    /// クライアントから受けたバイト (要求のヘッダーと本文、トンネルは上り)
    pub up: u64,
    /// クライアントへ返したバイト (応答、トンネルは下り)
    pub down: u64,
    /// 最後の応答の状態コード (http だけ)
    pub status: u16,
    /// 段階の ms (いちばん大きかった要求の値)。並びは [`STAGE_NAMES`]
    pub stage_ms: [u64; STAGES],
    /// カーネルの平滑化 RTT (us) と再送の通算 (`[クライアント側, オリジン側]`。T14.5)。
    /// 書くのは**接続の終わりに `getsockopt` を呼んだ 1 回だけ**で、`0` は「読めなかった」
    pub rtt_us: [u32; SIDES],
    pub retrans: [u32; SIDES],
    /// 確立までに SYN を送り直した回数 (T14.46)。keep-alive の HTTP 接続では
    /// **いちばん多かった要求**の値 (プールが接続を張った要求だけ 0 でない)
    pub syn_retrans: u8,
    /// 中継が**書けるのを待った**合計 ms (`[クライアント側, オリジン側]`。T14.42)。
    /// トンネルだけが埋める (http の接続と、繋がらなかった CONNECT は 0)
    pub stall_ms: [u32; SIDES],
}

impl ConnTally {
    /// 1 要求ぶんを足す (段階の ms と SYN の再送は**いちばん大きかった要求**を採る)。
    pub fn add_request(
        &mut self,
        status: u16,
        up: u64,
        down: u64,
        stage_ms: [u64; STAGES],
        syn_retrans: u8,
    ) {
        self.up = self.up.saturating_add(up);
        self.down = self.down.saturating_add(down);
        self.status = status;
        for (slot, ms) in self.stage_ms.iter_mut().zip(stage_ms) {
            *slot = (*slot).max(ms);
        }
        self.syn_retrans = self.syn_retrans.max(syn_retrans);
    }
}

/// 閉じた接続 1 本の個票 (`/recent`)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentEntry {
    /// 接続の通し番号 (ログの `conn#N`、`/connections` の `id` と同じ)
    pub id: u64,
    /// 開いた時刻 (epoch 秒)
    pub at: u64,
    /// 接続元 IP
    pub client: String,
    /// 宛先 (`host:port`。CONNECT はトンネルの相手、http は最初の要求の宛先)
    pub target: String,
    /// CONNECT トンネルか (`false` = keep-alive の HTTP)
    pub connect: bool,
    /// 寿命 (秒)
    pub secs: u64,
    /// 処理した要求の数 (http だけ)
    pub requests: u32,
    /// クライアントから受けたバイト / クライアントへ返したバイト
    pub up: u64,
    pub down: u64,
    /// 閉じた理由
    pub reason: CloseReason,
    /// 最後の応答の状態コード (http だけ。0 = 無し)
    pub status: u16,
    /// 預かり所にいた合計秒と、預けられた回数
    pub parked_secs: u64,
    pub parks: u32,
    /// 段階の ms ([`STAGE_NAMES`] の並び)
    pub stage_ms: [u64; STAGES],
    /// カーネルの平滑化 RTT (us) と再送の通算 (`[クライアント側, オリジン側]`。T14.5)。
    /// `0` は「読めなかった」(Linux 以外・`--lite`・もう閉じていた) で JSON では `null`
    pub rtt_us: [u32; SIDES],
    pub retrans: [u32; SIDES],
    /// CONNECT の最初のバイトから覗いた SNI (`PROXY_PEEK_SNI`。T14.38)。
    /// `None` は「覗いていない (443 以外・`--lite`・`off`) / 読めなかった」で JSON では `null`。
    /// **IP リテラル宛ての CONNECT では、これが「本当の宛先」**
    pub sni: Option<Box<str>>,
    /// 確立までに SYN を送り直した回数 (T14.46)。`0` は「再送なし / 読めなかった /
    /// Linux 以外」。**1 回で確立が 1 秒、2 回で 3 秒に飛ぶ**ので、`ms.connect` が
    /// 1,000 ms 台の個票はここが 1 以上になる (どちら側の待ち受けが溢れたかの手掛かり)
    pub syn_retrans: u8,
    /// 中継が**書けるのを待った**合計 ms (`[クライアント側, オリジン側]`。T14.42)。
    /// `client` が大きい = 利用者の下り回線か端末が読んでいない、`origin` が大きい =
    /// オリジンか利用者の上りが詰まっている。CONNECT のトンネルだけが埋める
    pub stall_ms: [u32; SIDES],
    /// 中継の輪が「起こされたのに 1 バイトも進まなかった」回数の通算 (T15.0 (4))。
    /// 寿命に対して桁違いに大きければ空回り (T15.5 の形)。トンネルだけが埋める
    pub spins: u64,
    /// 半閉じの (向き, 半閉じから閉じるまでの ms)。半閉じしていなければ `None`。
    /// 向きは「**先に EOF を出した側**」で、閉じた理由 (`client_eof` / `server_eof`) と
    /// 同じ出どころ (`Idle::first_eof`)。T15.0 (4)
    pub half_closed: Option<(usize, u64)>,
}

/// `us` を ms の JSON にする (`0` = 読めなかった → `null`。T14.5)。
///
/// 桁は成り行きに任せる (`{}` は往復できる最短の表記を出すので、us を 1000 で割った値は
/// `0.052` / `30.1` / `4294967.295` のように**余計な 0 が付かない**)。us のままにしないのは、
/// `/hosts` の `rtt_ms` と `/recent` の `ms`(段階) が ms なので単位を揃えるため。
fn rtt_ms_json(us: u32) -> String {
    if us == 0 {
        return "null".to_string();
    }
    format!("{}", us as f64 / 1000.0)
}

impl RecentEntry {
    /// 並べ替えに使う転送量の合計。
    pub fn bytes(&self) -> u64 {
        self.up.saturating_add(self.down)
    }

    /// 並べ替えに使う確立の ms (`?sort=slow`)。
    pub fn connect_ms(&self) -> u64 {
        self.stage_ms[STAGE_CONNECT]
    }

    /// `/recent` の 1 要素。
    ///
    /// `ms` の中身だけ可変で、**`dns` と `connect` は 0 でも必ず出す**
    /// (`?sort=slow` が読む値なので、無いのと 0 を区別させない)。残りの 4 つは
    /// 0 なら出さない — T14.3 が段階の時計を足すまで 0 のままなので、それまでは
    /// 1 バイトも増えない。
    pub fn to_json(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(256);
        let _ = write!(
            out,
            "{{\"id\":{},\"at\":{},\"client\":\"{}\",\"target\":\"{}\",\"kind\":\"{}\",\"secs\":{},\"reqs\":{},\"up\":{},\"down\":{},\"reason\":\"{}\",\"status\":{},\"parked_secs\":{},\"parks\":{},\"ms\":{{",
            self.id,
            self.at,
            crate::json::escape(&self.client),
            crate::json::escape(&self.target),
            if self.connect { "connect" } else { "http" },
            self.secs,
            self.requests,
            self.up,
            self.down,
            self.reason.text(),
            self.status,
            self.parked_secs,
            self.parks,
        );
        for (i, (name, ms)) in STAGE_NAMES.iter().zip(self.stage_ms).enumerate() {
            // `dns` と `connect` は必ず、残りは 0 でないときだけ
            if i > STAGE_CONNECT && ms == 0 {
                continue;
            }
            let _ = write!(out, "{}\"{}\":{}", if i == 0 { "" } else { "," }, name, ms);
        }
        out.push('}');
        // カーネルの RTT と再送 (T14.5)。**両側とも必ず出す**が、読めなかった側
        // (Linux 以外・`--lite`・もう閉じていた) は `null` — 0 ms と区別させないため
        let _ = write!(
            out,
            ",\"rtt_ms\":{{\"client\":{},\"origin\":{}}},\"retrans\":{{\"client\":{},\"origin\":{}}}",
            rtt_ms_json(self.rtt_us[CLIENT_SIDE]),
            rtt_ms_json(self.rtt_us[ORIGIN_SIDE]),
            self.retrans[CLIENT_SIDE],
            self.retrans[ORIGIN_SIDE],
        );
        // 覗いた SNI (T14.38)。**末尾に足した** (既存の鍵の順は変えない)。
        // 覗いていない (443 以外・`--lite`・`off`) と読めなかったときは `null`
        match &self.sni {
            Some(name) => {
                let _ = write!(out, ",\"sni\":\"{}\"", crate::json::escape(name));
            }
            None => out.push_str(",\"sni\":null"),
        }
        // 確立までの SYN の再送 (T14.46)。**末尾に足した** (既存の鍵の順は変えない)。
        // 0 でも必ず出す (「欄が無い」= 古い版と区別させるため)
        let _ = write!(out, ",\"syn_retrans\":{}", self.syn_retrans);
        // 中継の詰まりの向き (T14.42)。**末尾に足した** (既存の鍵の順は変えない)。
        // 両方 0 でも必ず出す (「詰まっていない」と「欄が無い」を区別させるため)
        let _ = write!(
            out,
            ",\"stall_ms\":{{\"client\":{},\"origin\":{}}}",
            self.stall_ms[CLIENT_SIDE], self.stall_ms[ORIGIN_SIDE],
        );
        // 空回りの回数と半閉じの向き (T15.0 (4))。**末尾に足した** (既存の鍵の順は
        // 変えない)。`/connections` と違ってここは件数で切る口なので、0 でも必ず出す
        // (`syn_retrans` / `stall_ms` と同じ扱い)。半閉じしていない接続は `null`
        let _ = write!(out, ",\"spins\":{}", self.spins);
        match self.half_closed {
            Some((side, ms)) => {
                let _ = write!(
                    out,
                    ",\"half_closed\":\"{}\",\"half_closed_ms\":{}}}",
                    SIDE_NAMES[side.min(SIDES - 1)],
                    ms
                );
            }
            None => out.push_str(",\"half_closed\":null,\"half_closed_ms\":null}"),
        }
        out
    }
}

/// 閉じた接続の固定長リング (`/recent`。T14.4)。
///
/// 書くのは**接続の終了で 1 回**だけ ([`ConnTable::unregister`] と同じ場所)。
/// `/connections` が「いま」しか見せないのに対し、ここは「起きたこと」を残す口で、
/// バーストのとき誰が何を開いたか・遅かった 1 本がどの段階で遅かったかを後から読む。
pub struct RecentRing {
    inner: Mutex<RecentBuf>,
}

#[derive(Default)]
struct RecentBuf {
    buf: Vec<RecentEntry>,
    /// 次に書く位置 (`buf` が満杯になってからだけ意味を持つ)
    next: usize,
    /// 起動からの通算 (捨てた分も含む)
    total: u64,
    /// **ファイルに書いた所までの通算** (T14.9)
    written: u64,
    /// 起動時に状態ファイルから読み戻した件数 (`/recent` の `"restored"`)
    restored: usize,
}

impl Default for RecentRing {
    fn default() -> Self {
        RecentRing::new()
    }
}

impl RecentRing {
    pub fn new() -> RecentRing {
        RecentRing {
            inner: Mutex::new(RecentBuf::default()),
        }
    }

    /// 1 件書く (満杯なら最も古いものを上書きする)。**接続の終了で 1 回だけ。**
    ///
    /// `PROXY_RECORDS=off` なら**鍵も取らずに捨てる** (T14.41)。
    pub fn push(&self, entry: RecentEntry) {
        if !crate::records::recording() {
            return;
        }
        let mut r = self.inner.locked();
        r.total += 1;
        if r.buf.len() < MAX_RECENT {
            r.buf.push(entry);
            return;
        }
        let at = r.next;
        r.buf[at] = entry;
        r.next = (at + 1) % MAX_RECENT;
    }

    /// 条件に合うものを**新しい順**で返す。2 つ目は起動からの通算 (捨てた分も含む)。
    ///
    /// 絞りはここ (鍵の内側) で済ませる。`since` は「開いた時刻がこれ以降」、
    /// `client` は接続元の完全一致 (空なら絞らない)。
    pub fn select(&self, since: u64, client: &str) -> (Vec<RecentEntry>, u64) {
        let r = self.inner.locked();
        let len = r.buf.len();
        let mut out = Vec::with_capacity(len.min(MAX_RECENT));
        for i in 0..len {
            let start = if len < MAX_RECENT { len } else { r.next };
            let at = (start + len - 1 - i) % len;
            let e = &r.buf[at];
            if e.at < since || (!client.is_empty() && e.client != client) {
                continue;
            }
            out.push(e.clone());
        }
        (out, r.total)
    }

    pub fn len(&self) -> usize {
        self.inner.locked().buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// まだファイルに書いていない件を古い順で取り出す (**history スレッドだけが呼ぶ**。T14.9)。
    pub fn take_unwritten(&self, max: usize) -> (Vec<RecentEntry>, u64) {
        let mut r = self.inner.locked();
        let g = &mut *r;
        drain_unwritten(&g.buf, g.next, MAX_RECENT, g.total, &mut g.written, max)
    }

    /// 置き場を [`MAX_RECENT`] 件ぶん確保して触る (**起動時に 1 回だけ**。T17.8)。
    ///
    /// 触るのは 1 件の固定部だけで、文字列は書くときに確保する。返すのは触ったバイト数。
    pub fn prefault(&self) -> usize {
        crate::prefault::vec(&mut self.inner.locked().buf, MAX_RECENT)
    }

    /// 状態ファイルから読み戻す (**起動時に 1 回だけ**。T14.9)。
    pub fn restore(&self, entries: Vec<RecentEntry>) {
        let n = entries.len().min(MAX_RECENT);
        for e in entries {
            self.push(e);
        }
        let mut r = self.inner.locked();
        r.written = r.total;
        r.restored = n;
    }

    /// 再起動前から引き継いだ件数 (`/recent` の `"restored"`)。
    pub fn restored(&self) -> usize {
        self.inner.locked().restored
    }
}
/// 山の写真 (`/bursts`) を何枚覚えておくか (固定。T14.6)。
///
/// 1 枚 4 KiB 以下 (下のテストで見る) なので 50 枚で 200 KiB。デプロイ先の山は
/// 1 日に数回なので、50 枚あれば 1 週間以上さかのぼれる。
pub const MAX_BURSTS: usize = 50;

/// 1 枚に載せる接続元の数 (それ以外は `clients_other` に本数だけ)。
pub const MAX_SHOT_CLIENTS: usize = 16;

/// 1 枚に載せる宛先の数 (それ以外は `targets_other` に本数だけ)。
pub const MAX_SHOT_TARGETS: usize = 10;

/// [`ConnState`] の数と、その名前 (`ConnState as usize` が添字)。
pub const CONN_STATES: usize = 5;
pub const CONN_STATE_NAMES: [&str; CONN_STATES] =
    ["serving", "reading", "parked", "queued", "relaying"];

/// 山の写真 1 枚 (`/bursts`。T14.6)。
///
/// 撮るのは **history スレッド**で、中身は `/connections` の表から作る (鍵 1 回)。
/// 越えた接続を受けたスレッドは [`BurstRing::request`] で旗を立てるだけなので、
/// **accept の経路に鍵は 1 つも増えない**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BurstShot {
    /// 撮った時刻 (epoch 秒)
    pub at: u64,
    /// 何枚目か (捨てた分も含む通し番号)
    pub seq: u64,
    /// 撮った瞬間の同時接続数 (`/status` の `active_connections`)
    pub active: usize,
    /// 旗が立った瞬間の本数 (撮るまでに最大 1 周期あるので `active` とは違いうる)
    pub trigger_active: usize,
    /// そのときの上限と閾 (`max_conns` × `PROXY_BURST_PERCENT`)
    pub max_conns: usize,
    pub threshold: usize,
    /// 接続元ごとの本数 (多い順、上位 [`MAX_SHOT_CLIENTS`])
    pub clients: Vec<(String, u32)>,
    pub clients_distinct: usize,
    pub clients_other: u32,
    /// 宛先ごとの本数 (多い順、上位 [`MAX_SHOT_TARGETS`])
    pub targets: Vec<(String, u32)>,
    pub targets_distinct: usize,
    pub targets_other: u32,
    /// 状態別 ([`CONN_STATE_NAMES`] の並び) と種類別
    pub states: [u32; CONN_STATES],
    pub connects: u32,
    pub https: u32,
    /// そのときの累計 (T13.2 の追い出しと、上限で断った数)
    pub evicted_idle: u64,
    pub rejected_overload: u64,
    /// プロセスのスレッド数と記述子 (`/proc`。history スレッドが読む)
    pub threads: u64,
    pub fds: u64,
    pub max_fds: u64,
}

impl BurstShot {
    /// `/connections` の表から 1 枚作る (`ConnTable::snapshot` の鍵 1 回のあと)。
    #[allow(clippy::too_many_arguments)]
    pub fn take(
        rows: &[Arc<ConnSlot>],
        seq: u64,
        active: usize,
        trigger_active: usize,
        max_conns: usize,
        threshold: usize,
        evicted_idle: u64,
        rejected_overload: u64,
    ) -> BurstShot {
        let mut by_client: HashMap<&str, u32> = HashMap::new();
        let mut by_target: HashMap<String, u32> = HashMap::new();
        let mut states = [0u32; CONN_STATES];
        let (mut connects, mut https) = (0u32, 0u32);
        for slot in rows {
            *by_client.entry(slot.client.as_str()).or_insert(0) += 1;
            let target = slot.target.locked().clone();
            if !target.is_empty() {
                *by_target.entry(target).or_insert(0) += 1;
            }
            states[slot.state() as usize] += 1;
            if slot.is_connect() {
                connects += 1;
            } else {
                https += 1;
            }
        }
        let (clients, clients_other) = top_counts(
            by_client.into_iter().map(|(k, n)| (k.to_string(), n)),
            MAX_SHOT_CLIENTS,
        );
        let clients_distinct = clients.len() + usize::from(clients_other > 0);
        let (targets, targets_other) = top_counts(by_target.into_iter(), MAX_SHOT_TARGETS);
        let targets_distinct = targets.len() + usize::from(targets_other > 0);
        let (threads, fds, max_fds) = process_counts();
        BurstShot {
            at: crate::cache::now_epoch(),
            seq,
            active,
            trigger_active,
            max_conns,
            threshold,
            clients,
            clients_distinct,
            clients_other,
            targets,
            targets_distinct,
            targets_other,
            states,
            connects,
            https,
            evicted_idle,
            rejected_overload,
            threads,
            fds,
            max_fds,
        }
    }

    /// `/bursts` の 1 要素。
    pub fn to_json(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(2048);
        let _ = write!(
            out,
            "{{\"at\":{},\"seq\":{},\"active\":{},\"trigger_active\":{},\"max_conns\":{},\"threshold\":{},\"clients\":",
            self.at, self.seq, self.active, self.trigger_active, self.max_conns, self.threshold,
        );
        push_counts(&mut out, "client", &self.clients);
        let _ = write!(
            out,
            ",\"clients_distinct\":{},\"clients_other\":{},\"targets\":",
            self.clients_distinct, self.clients_other
        );
        push_counts(&mut out, "target", &self.targets);
        let _ = write!(
            out,
            ",\"targets_distinct\":{},\"targets_other\":{},\"states\":{{",
            self.targets_distinct, self.targets_other
        );
        for (i, name) in CONN_STATE_NAMES.iter().enumerate() {
            let _ = write!(
                out,
                "{}\"{}\":{}",
                if i == 0 { "" } else { "," },
                name,
                self.states[i]
            );
        }
        let _ = write!(
            out,
            "}},\"kinds\":{{\"connect\":{},\"http\":{}}},\"evicted_idle\":{},\"rejected_overload\":{},\"threads\":{},\"fds\":{},\"max_fds\":{}}}",
            self.connects,
            self.https,
            self.evicted_idle,
            self.rejected_overload,
            self.threads,
            self.fds,
            self.max_fds,
        );
        out
    }
}

/// 多い順に `max` 件まで取り、残りの本数を返す (同数は名前の順で安定させる)。
fn top_counts(items: impl Iterator<Item = (String, u32)>, max: usize) -> (Vec<(String, u32)>, u32) {
    let mut all: Vec<(String, u32)> = items.collect();
    all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let other = all.iter().skip(max).map(|(_, n)| *n).sum();
    all.truncate(max);
    (all, other)
}

/// `[{"<key>":"…","conns":N},…]`。
fn push_counts(out: &mut String, key: &str, rows: &[(String, u32)]) {
    use std::fmt::Write as _;
    out.push('[');
    for (i, (name, n)) in rows.iter().enumerate() {
        let _ = write!(
            out,
            "{}{{\"{}\":\"{}\",\"conns\":{}}}",
            if i == 0 { "" } else { "," },
            key,
            crate::json::escape(name),
            n
        );
    }
    out.push(']');
}

/// プロセスのスレッド数 / 記述子 / その上限。**写真を撮るときだけ**読む (`/proc` 2 つ)。
fn process_counts() -> (u64, u64, u64) {
    let threads = crate::sysinfo::process_threads().unwrap_or(0);
    let (fds, max_fds) = crate::sysinfo::process_fds().unwrap_or((0, 0));
    (threads, fds, max_fds)
}

/// 山の写真のリングと、「撮ってくれ」の旗 (T14.6)。
///
/// **accept の経路がするのは、閾を越えたときに原子の読み 1 回** ([`BurstRing::request`])
/// だけ。撮るのは history スレッド ([`BurstRing::take_pending`] → [`BurstShot::take`] →
/// [`BurstRing::push`]) で、5 秒以内に 1 枚撮れる。
///
/// **同じ山では 1 枚だけ**: 撮ると旗を下ろし、同時接続数が閾の 80% を下回るまで
/// 次の 1 枚を撮らない ([`BurstRing::rearm_if_calm`])。
pub struct BurstRing {
    /// 次の山で 1 枚撮ってよいか
    armed: AtomicBool,
    /// history スレッドへの「撮ってくれ」= 旗が立った瞬間の本数 (0 = 頼まれていない)
    pending: AtomicUsize,
    /// 旗を立てた接続が見ていた閾と上限 (写真と、山が引いたかの判定に使う)
    threshold: AtomicUsize,
    max_conns: AtomicUsize,
    inner: Mutex<BurstBuf>,
}

#[derive(Default)]
struct BurstBuf {
    buf: Vec<BurstShot>,
    /// 次に書く位置 (`buf` が満杯になってからだけ意味を持つ)
    next: usize,
    /// 起動からの通算 (捨てた分も含む)
    total: u64,
    /// **ファイルに書いた所までの通算** (T14.9)
    written: u64,
    /// 起動時に状態ファイルから読み戻した枚数 (`/bursts` の `"restored"`)
    restored: usize,
}

impl Default for BurstRing {
    fn default() -> Self {
        BurstRing::new()
    }
}

impl BurstRing {
    pub fn new() -> BurstRing {
        BurstRing {
            armed: AtomicBool::new(true),
            pending: AtomicUsize::new(0),
            threshold: AtomicUsize::new(0),
            max_conns: AtomicUsize::new(0),
            inner: Mutex::new(BurstBuf::default()),
        }
    }

    /// 閾を**越えた瞬間**に accept の経路から呼ぶ (T14.6)。
    ///
    /// 呼び出し側は `open > cfg.burst_at` の**比較 1 回**で来るかどうかを決める。
    /// ここでも原子の読みが 1 回で、旗が下りていれば (= 同じ山の 2 本目以降) すぐ戻る。
    pub fn request(&self, active: usize, threshold: usize, max_conns: usize) {
        if !self.armed.load(Ordering::Relaxed) {
            return;
        }
        // 同じ瞬間に複数のスレッドが越えても、旗を取れるのは 1 つだけ
        if !self.armed.swap(false, Ordering::Relaxed) {
            return;
        }
        self.threshold.store(threshold, Ordering::Relaxed);
        self.max_conns.store(max_conns, Ordering::Relaxed);
        // 0 は「頼まれていない」の印なので 1 未満にはしない
        self.pending.store(active.max(1), Ordering::Relaxed);
    }

    /// 頼まれていれば旗を下ろし、立った瞬間の本数を返す (history スレッド)。
    pub fn take_pending(&self) -> Option<usize> {
        match self.pending.swap(0, Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    /// 山が引いたら次の 1 枚に備える (history スレッド)。
    ///
    /// 戻す境目を閾そのものではなく **80%** にしてあるのは、閾のすぐ上で出入りする
    /// 接続が 1 本あるだけで写真がリングを埋めてしまうため。
    pub fn rearm_if_calm(&self, active: usize) {
        let threshold = self.threshold.load(Ordering::Relaxed);
        if threshold == 0 || self.pending.load(Ordering::Relaxed) != 0 {
            return;
        }
        if active.saturating_mul(100) < threshold.saturating_mul(80) {
            self.armed.store(true, Ordering::Relaxed);
        }
    }

    /// 1 枚書く (満杯なら最も古いものを上書きする)。
    ///
    /// `PROXY_RECORDS=off` なら**鍵も取らずに捨てる** (T14.41)。写真の中身は
    /// `/connections` の表から作るので、`off` ではそもそも接続元が 1 つも入らない
    pub fn push(&self, shot: BurstShot) {
        if !crate::records::recording() {
            return;
        }
        let mut r = self.inner.locked();
        r.total += 1;
        if r.buf.len() < MAX_BURSTS {
            r.buf.push(shot);
            return;
        }
        let at = r.next;
        r.buf[at] = shot;
        r.next = (at + 1) % MAX_BURSTS;
    }

    /// 次に撮る 1 枚の通し番号 (1 始まり)。
    pub fn next_seq(&self) -> u64 {
        self.inner.locked().total + 1
    }

    /// 直近 `n` 枚を**新しい順**で返す。2 つ目は起動からの通算 (捨てた分も含む)。
    pub fn recent(&self, n: usize) -> (Vec<BurstShot>, u64) {
        let r = self.inner.locked();
        let len = r.buf.len();
        let n = n.min(len);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let start = if len < MAX_BURSTS { len } else { r.next };
            let at = (start + len - 1 - i) % len;
            out.push(r.buf[at].clone());
        }
        (out, r.total)
    }

    pub fn len(&self) -> usize {
        self.inner.locked().buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// いまの閾と上限 (`/bursts` に出す。旗が 1 度も立っていなければ 0)。
    pub fn threshold(&self) -> usize {
        self.threshold.load(Ordering::Relaxed)
    }

    pub fn max_conns(&self) -> usize {
        self.max_conns.load(Ordering::Relaxed)
    }

    /// 次の山で撮る用意ができているか / 撮ってくれと頼まれているか (`/bursts` に出す)。
    pub fn armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    pub fn pending(&self) -> bool {
        self.pending.load(Ordering::Relaxed) != 0
    }

    /// まだファイルに書いていない枚数を古い順で取り出す (**history スレッドだけが呼ぶ**。T14.9)。
    pub fn take_unwritten(&self, max: usize) -> (Vec<BurstShot>, u64) {
        let mut r = self.inner.locked();
        let g = &mut *r;
        drain_unwritten(&g.buf, g.next, MAX_BURSTS, g.total, &mut g.written, max)
    }

    /// 置き場を [`MAX_BURSTS`] 枚ぶん確保して触る (**起動時に 1 回だけ**。T17.8)。
    ///
    /// 触るのは 1 枚の固定部だけで、写真の中の一覧は撮るときに確保する。返すのは触ったバイト数。
    pub fn prefault(&self) -> usize {
        crate::prefault::vec(&mut self.inner.locked().buf, MAX_BURSTS)
    }

    /// 状態ファイルから読み戻す (**起動時に 1 回だけ**。T14.9)。
    ///
    /// 通し番号 (`seq`) は `total` から作るので、読み戻すと再起動前の続きから振られる。
    pub fn restore(&self, shots: Vec<BurstShot>) {
        let n = shots.len().min(MAX_BURSTS);
        for shot in shots {
            self.push(shot);
        }
        let mut r = self.inner.locked();
        r.written = r.total;
        r.restored = n;
    }

    /// 再起動前から引き継いだ枚数 (`/bursts` の `"restored"`)。
    pub fn restored(&self) -> usize {
        self.inner.locked().restored
    }
}

/// 閉じた理由の数 ([`CLOSE_REASON_NAMES`] と同じ。T14.6)。
///
/// [`CloseReason`] は 9 種類で、`Error` だけは原因を連れている。分布では
/// **`error:*` を 1 つにまとめる** (原因別は `/status` の `errors_by_cause` と
/// `/errors` にあるので、ここで 8 + 8 = 15 列に増やさない)。**`client_dead` も
/// `shutdown` に畳む** (T14.52。標本の余白の都合で列は 8 つのまま)。
pub const CLOSE_REASONS: usize = 8;
pub const CLOSE_REASON_NAMES: [&str; CLOSE_REASONS] = [
    "client_eof",
    "server_eof",
    "idle_timeout",
    "keepalive_timeout",
    "evicted",
    "limit",
    "shutdown",
    "error",
];

/// 寿命の区間 (秒)。**12 段** (T14.6)。
///
/// `PROXY_KEEPALIVE_SECS` (15) と `PROXY_TUNNEL_IDLE_SECS` (300) を**区間の境目に
/// 置いてある**: 「その設定で切られた接続がどれだけ居るか」を読むのがこの分布の仕事なので、
/// 境目が設定とずれていると読めない。
pub const LIFE_BOUNDS_SECS: [u64; 12] = [1, 2, 5, 10, 15, 30, 60, 120, 300, 900, 3600, 21600];

/// 上り・下りバイトの区間。**12 段、1 KiB から 4 倍ずつ** (T14.6)。
///
/// 1 KiB (要求 1 本ぶんのヘッダー) から 1 GiB (10 段目) までを 4 倍刻みで覆う。
/// 最後の 1 段 (4 GiB) は「桁が違う 1 本」を上限なしの区間に落とさないための余白。
pub const BYTE_BOUNDS: [u64; 12] = [
    1 << 10,
    1 << 12,
    1 << 14,
    1 << 16,
    1 << 18,
    1 << 20,
    1 << 22,
    1 << 24,
    1 << 26,
    1 << 28,
    1 << 30,
    1 << 32,
];

/// 区間の数 (上限なしの 1 段を足す)。
pub const LIFE_BUCKETS: usize = LIFE_BOUNDS_SECS.len() + 1;
pub const BYTE_BUCKETS: usize = BYTE_BOUNDS.len() + 1;

/// 「その窓に閉じた接続」の分布 (T14.6)。**累計ではない。**
///
/// 足すのは [`crate::metrics::Metrics::record_closed`] = 接続の終了で 1 回だけで、
/// **既に鍵の内側**なので原子操作もシステムコールも増えない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClosedCounts {
    /// 閉じた接続の数
    pub closed: u64,
    /// 閉じた理由別 ([`CLOSE_REASON_NAMES`] の並び)
    pub reasons: [u64; CLOSE_REASONS],
    /// 寿命の区間 ([`LIFE_BOUNDS_SECS`] + 上限なし)
    pub life: [u64; LIFE_BUCKETS],
    /// 上り / 下りバイトの区間 ([`BYTE_BOUNDS`] + 上限なし)
    pub up: [u64; BYTE_BUCKETS],
    pub down: [u64; BYTE_BUCKETS],
    /// 合計 (平均を出すため)
    pub life_secs_sum: u64,
    pub parked_secs_sum: u64,
    pub parks: u64,
    pub up_bytes: u64,
    pub down_bytes: u64,
}

impl Default for ClosedCounts {
    fn default() -> Self {
        ClosedCounts {
            closed: 0,
            reasons: [0; CLOSE_REASONS],
            life: [0; LIFE_BUCKETS],
            up: [0; BYTE_BUCKETS],
            down: [0; BYTE_BUCKETS],
            life_secs_sum: 0,
            parked_secs_sum: 0,
            parks: 0,
            up_bytes: 0,
            down_bytes: 0,
        }
    }
}

/// `bounds` の何段目か (どれにも収まらなければ最後の「上限なし」)。
fn bucket_of(bounds: &[u64], v: u64) -> usize {
    bounds.iter().position(|&b| v <= b).unwrap_or(bounds.len())
}

impl ClosedCounts {
    /// 閉じた接続 1 本を足す (`/recent` に残す 1 件と同じもの)。
    pub fn observe(&mut self, e: &RecentEntry) {
        self.closed += 1;
        self.reasons[e.reason.index()] += 1;
        self.life[bucket_of(&LIFE_BOUNDS_SECS, e.secs)] += 1;
        self.up[bucket_of(&BYTE_BOUNDS, e.up)] += 1;
        self.down[bucket_of(&BYTE_BOUNDS, e.down)] += 1;
        self.life_secs_sum = self.life_secs_sum.saturating_add(e.secs);
        self.parked_secs_sum = self.parked_secs_sum.saturating_add(e.parked_secs);
        self.parks = self.parks.saturating_add(e.parks as u64);
        self.up_bytes = self.up_bytes.saturating_add(e.up);
        self.down_bytes = self.down_bytes.saturating_add(e.down);
    }

    /// 粗い解像度へ畳むときは足し合わせる (区間の値なので平均でも最後の値でもない)。
    pub fn merge(&mut self, o: &ClosedCounts) {
        self.closed += o.closed;
        for (a, b) in self.reasons.iter_mut().zip(o.reasons.iter()) {
            *a += *b;
        }
        for (a, b) in self.life.iter_mut().zip(o.life.iter()) {
            *a += *b;
        }
        for (a, b) in self.up.iter_mut().zip(o.up.iter()) {
            *a += *b;
        }
        for (a, b) in self.down.iter_mut().zip(o.down.iter()) {
            *a += *b;
        }
        self.life_secs_sum = self.life_secs_sum.saturating_add(o.life_secs_sum);
        self.parked_secs_sum = self.parked_secs_sum.saturating_add(o.parked_secs_sum);
        self.parks = self.parks.saturating_add(o.parks);
        self.up_bytes = self.up_bytes.saturating_add(o.up_bytes);
        self.down_bytes = self.down_bytes.saturating_add(o.down_bytes);
    }

    pub fn is_empty(&self) -> bool {
        self.closed == 0
    }

    /// 1 窓を配列 1 行として書く ([`CLOSED_KEYS`] の順。先頭は窓の始まりの時刻)。
    pub fn push_row(&self, out: &mut String, t: u64) {
        use std::fmt::Write as _;
        let _ = write!(out, "[{},{},", t, self.closed);
        push_u64_array(out, &self.reasons);
        out.push(',');
        push_u64_array(out, &self.life);
        out.push(',');
        push_u64_array(out, &self.up);
        out.push(',');
        push_u64_array(out, &self.down);
        let _ = write!(
            out,
            ",{},{},{},{},{}]",
            self.life_secs_sum, self.parked_secs_sum, self.parks, self.up_bytes, self.down_bytes
        );
    }
}

/// [`ClosedCounts::push_row`] が並べる列の名前 (`/history` の `closed.keys`)。
pub const CLOSED_KEYS: [&str; 11] = [
    "t",
    "closed",
    "reasons",
    "life",
    "up",
    "down",
    "life_secs_sum",
    "parked_secs_sum",
    "parks",
    "up_bytes",
    "down_bytes",
];

fn push_u64_array(out: &mut String, v: &[u64]) {
    use std::fmt::Write as _;
    out.push('[');
    for (i, n) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}", n);
    }
    out.push(']');
}

/// 山の写真と、閉じた接続の分布 (T14.6)。
#[cfg(test)]
mod burst_tests {
    use super::*;
    use std::time::Instant;

    fn closed(reason: CloseReason, secs: u64, up: u64, down: u64) -> RecentEntry {
        RecentEntry {
            id: 1,
            at: 1_700_000_000,
            client: "198.51.100.7".to_string(),
            target: "example.net:443".to_string(),
            connect: true,
            secs,
            requests: 0,
            up,
            down,
            reason,
            status: 0,
            parked_secs: 3,
            parks: 2,
            stage_ms: [0; STAGES],
            stall_ms: [0; SIDES],
            rtt_us: [0; SIDES],
            retrans: [0; SIDES],
            sni: None,
            syn_retrans: 0,
            spins: 0,
            half_closed: None,
        }
    }

    /// **閾を越えた瞬間の 1 本だけ**が旗を立て、同じ山の 2 本目以降は立てない。
    #[test]
    fn only_the_connection_that_crosses_raises_the_flag() {
        let ring = BurstRing::new();
        assert!(ring.armed());
        assert!(!ring.pending());
        // 閾 4 (上限 8 × 50%) を越えるのは 5 本目
        ring.request(5, 4, 8);
        assert_eq!(ring.take_pending(), Some(5), "越えた本数が旗に乗る");
        assert!(!ring.armed(), "撮ると旗は下りる");
        // 6〜8 本目は何も頼まない
        for n in 6..=8 {
            ring.request(n, 4, 8);
        }
        assert_eq!(ring.take_pending(), None, "同じ山では 1 枚だけ");
        assert_eq!(ring.threshold(), 4);
        assert_eq!(ring.max_conns(), 8);
    }

    /// 閾の 80% を下回るまでは次の 1 枚を撮らない。
    #[test]
    fn the_next_shot_waits_until_the_spike_is_over() {
        let ring = BurstRing::new();
        ring.request(5, 4, 8);
        assert_eq!(ring.take_pending(), Some(5));
        // 閾の 80% (4 × 0.8 = 3.2 本) 以上のあいだは戻らない
        for active in [8, 5, 4] {
            ring.rearm_if_calm(active);
            assert!(!ring.armed(), "{} 本ではまだ戻らない", active);
        }
        ring.rearm_if_calm(3);
        assert!(ring.armed(), "3 本 (< 3.2) まで減ったら次の 1 枚に備える");
        ring.request(5, 4, 8);
        assert_eq!(ring.take_pending(), Some(5), "2 枚目が撮れる");
    }

    /// まだ撮っていない旗を、山が引いたからといって消さない。
    #[test]
    fn a_pending_shot_is_not_lost_when_the_spike_passes() {
        let ring = BurstRing::new();
        ring.request(5, 4, 8);
        ring.rearm_if_calm(0);
        assert!(!ring.armed(), "頼んだ 1 枚を撮るまでは戻らない");
        assert_eq!(ring.take_pending(), Some(5));
        ring.rearm_if_calm(0);
        assert!(ring.armed());
    }

    /// 写真は `/connections` の表から作る (接続元・宛先・状態・種類の内訳)。
    #[test]
    fn a_shot_summarises_the_connection_table() {
        let t = ConnTable::new();
        let now = Instant::now();
        for i in 0..5u64 {
            let slot = t.register(i, "198.51.100.7", now).unwrap();
            slot.begin_tunnel("mtalk.google.com:5228");
            slot.set_state(ConnState::Parked);
        }
        let other = t.register(9, "203.0.113.9", now).unwrap();
        other.set_first_target("example.net:80");
        other.set_state(ConnState::Reading);

        let shot = BurstShot::take(&t.snapshot(), 1, 6, 5, 8, 4, 2, 7);
        assert_eq!(shot.seq, 1);
        assert_eq!(shot.active, 6);
        assert_eq!(shot.trigger_active, 5);
        assert_eq!(shot.max_conns, 8);
        assert_eq!(shot.threshold, 4);
        assert_eq!(shot.clients.len(), 2);
        assert_eq!(shot.clients[0], ("198.51.100.7".to_string(), 5), "多い順");
        assert_eq!(shot.clients_distinct, 2);
        assert_eq!(shot.targets[0], ("mtalk.google.com:5228".to_string(), 5));
        assert_eq!(shot.targets_distinct, 2);
        assert_eq!(shot.states[ConnState::Parked as usize], 5);
        assert_eq!(shot.states[ConnState::Reading as usize], 1);
        assert_eq!((shot.connects, shot.https), (5, 1));
        assert_eq!((shot.evicted_idle, shot.rejected_overload), (2, 7));
        let json = shot.to_json();
        assert!(json.contains("\"active\":6"), "{}", json);
        assert!(
            json.contains(
                "\"states\":{\"serving\":0,\"reading\":1,\"parked\":5,\"queued\":0,\"relaying\":0}"
            ),
            "{}",
            json
        );
        assert!(
            json.contains("\"kinds\":{\"connect\":5,\"http\":1}"),
            "{}",
            json
        );
        assert!(
            json.contains("{\"client\":\"198.51.100.7\",\"conns\":5}"),
            "{}",
            json
        );
        assert!(
            json.contains("{\"target\":\"mtalk.google.com:5228\",\"conns\":5}"),
            "{}",
            json
        );
        assert!(json.contains("\"evicted_idle\":2"), "{}", json);
        println!("burst shot: typical {} B", json.len());
    }

    /// 1 枚は 4 KiB 以内 (50 枚で 200 KiB。`/bursts` の 256 KiB に収まる大きさ)。
    #[test]
    fn one_shot_fits_in_4_kib() {
        let t = ConnTable::new();
        let now = Instant::now();
        // 接続元も宛先も上限いっぱいの長さで、載せる数より多い種類を作る
        for i in 0..(MAX_SHOT_CLIENTS as u64 + MAX_SHOT_TARGETS as u64 + 20) {
            let slot = t
                .register(
                    i,
                    &format!("2001:0db8:0000:0000:0000:ff00:0042:{:04x}%enp0s31f6xx", i),
                    now,
                )
                .unwrap();
            // **通し番号は先頭に置く**: 宛先は 80 B で切られるので、末尾に置くと
            // 切られたあとが全部同じ名前になって「種類が 1 つ」になってしまう
            slot.begin_tunnel(&format!("{:03}{}example.net:65535", i, "sub.".repeat(20)));
        }
        let shot = BurstShot::take(
            &t.snapshot(),
            u64::MAX,
            usize::MAX,
            usize::MAX,
            4096,
            2048,
            u64::MAX,
            u64::MAX,
        );
        assert_eq!(shot.clients.len(), MAX_SHOT_CLIENTS);
        assert_eq!(shot.targets.len(), MAX_SHOT_TARGETS);
        assert!(
            shot.clients_other > 0 && shot.targets_other > 0,
            "残りは本数だけ"
        );
        let json = shot.to_json();
        assert!(json.len() <= 4096, "1 枚が {} B", json.len());
        println!("burst shot: worst {} B", json.len());
    }

    /// リングは 50 枚で頭打ち、新しい順に返る。
    #[test]
    fn the_burst_ring_keeps_the_newest_fifty() {
        let ring = BurstRing::new();
        let t = ConnTable::new();
        for i in 0..(MAX_BURSTS as u64 + 3) {
            assert_eq!(ring.next_seq(), i + 1);
            ring.push(BurstShot::take(&t.snapshot(), i + 1, 5, 5, 8, 4, 0, 0));
        }
        let (got, total) = ring.recent(MAX_BURSTS);
        assert_eq!(total, MAX_BURSTS as u64 + 3);
        assert_eq!(got.len(), MAX_BURSTS);
        assert_eq!(got[0].seq, MAX_BURSTS as u64 + 3, "新しい順");
        assert_eq!(got[MAX_BURSTS - 1].seq, 4, "最古は 50 枚前");
        assert_eq!(ring.recent(2).0.len(), 2);
    }

    /// 閉じた理由・寿命・バイト・預けの分布が 1 本ずつ積み上がること。
    #[test]
    fn the_distribution_counts_reasons_lifetimes_and_bytes() {
        let mut c = ClosedCounts::default();
        assert!(c.is_empty());
        // 寿命 12 秒 (10 < 12 <= 15 = `PROXY_KEEPALIVE_SECS` の段)、上り 2 KiB、下り 3 MiB
        c.observe(&closed(CloseReason::KeepaliveTimeout, 12, 2048, 3 << 20));
        c.observe(&closed(CloseReason::Error(ErrCause::Reset), 0, 0, 0));
        c.observe(&closed(
            CloseReason::Error(ErrCause::Dns),
            100_000,
            u64::MAX,
            1,
        ));
        assert_eq!(c.closed, 3);
        assert_eq!(c.reasons[CloseReason::KeepaliveTimeout.index()], 1);
        assert_eq!(c.reasons[7], 2, "`error:*` は 1 つにまとめる");
        assert_eq!(c.life[4], 1, "12 秒は 15 秒の段");
        assert_eq!(c.life[0], 1, "0 秒は 1 秒の段");
        assert_eq!(c.life[LIFE_BUCKETS - 1], 1, "100,000 秒は上限なしの段");
        assert_eq!(c.up[1], 1, "2 KiB は 4 KiB の段");
        assert_eq!(c.up[0], 1, "0 B は 1 KiB の段");
        assert_eq!(c.up[BYTE_BUCKETS - 1], 1, "20 桁は上限なしの段");
        assert_eq!(c.down[6], 1, "3 MiB は 4 MiB の段");
        assert_eq!(c.life_secs_sum, 12 + 100_000);
        assert_eq!(c.parked_secs_sum, 9);
        assert_eq!(c.parks, 6);
        assert_eq!(c.down_bytes, (3 << 20) + 1);
        // 畳むと足し合わせ
        let mut agg = ClosedCounts::default();
        agg.merge(&c);
        agg.merge(&c);
        assert_eq!(agg.closed, 6);
        assert_eq!(agg.reasons[7], 4);
        assert_eq!(agg.life[4], 2);
        assert_eq!(agg.parks, 12);

        let mut out = String::new();
        c.push_row(&mut out, 1_700_000_005);
        assert!(
            out.starts_with("[1700000005,3,[0,0,0,1,0,0,0,2],"),
            "{}",
            out
        );
        assert!(
            out.ends_with(",100012,9,6,18446744073709551615,3145729]"),
            "{}",
            out
        );
        // 列の数が `CLOSED_KEYS` と合っていること (入れ子の配列は 1 列)
        let mut depth = 0;
        let cols = 1 + out
            .chars()
            .filter(|ch| {
                match ch {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
                *ch == ',' && depth == 1
            })
            .count();
        assert_eq!(cols, CLOSED_KEYS.len(), "{}", out);
    }

    /// 8 つの理由が分布の 8 列に 1 対 1 で並ぶこと (名前も同じ綴り)。
    #[test]
    fn every_reason_has_its_own_column() {
        let all = [
            CloseReason::ClientEof,
            CloseReason::ServerEof,
            CloseReason::IdleTimeout,
            CloseReason::KeepaliveTimeout,
            CloseReason::Evicted,
            CloseReason::Limit,
            CloseReason::Shutdown,
            CloseReason::Error(ErrCause::Refused),
        ];
        for (i, reason) in all.into_iter().enumerate() {
            assert_eq!(reason.index(), i, "{}", reason.text());
            // `error:*` 以外は `/recent` の綴りと同じ名前
            if i < 7 {
                assert_eq!(CLOSE_REASON_NAMES[i], reason.text());
            }
        }
        assert_eq!(CLOSE_REASON_NAMES[7], "error");
        // 区間は 12 段ずつで、単調に増える
        assert_eq!(LIFE_BOUNDS_SECS.len(), 12);
        assert_eq!(BYTE_BOUNDS.len(), 12);
        assert!(LIFE_BOUNDS_SECS.windows(2).all(|w| w[0] < w[1]));
        assert!(BYTE_BOUNDS.windows(2).all(|w| w[0] < w[1]));
        // 設定の値そのものが境目にある (T14.6 の目的)
        assert!(LIFE_BOUNDS_SECS.contains(&15), "PROXY_KEEPALIVE_SECS");
        assert!(LIFE_BOUNDS_SECS.contains(&300), "PROXY_TUNNEL_IDLE_SECS");
        assert_eq!(BYTE_BOUNDS[0], 1024);
        assert_eq!(BYTE_BOUNDS[10], 1 << 30);
    }
}
