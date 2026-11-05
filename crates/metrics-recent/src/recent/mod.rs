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
//! **T25.2 で 3 ファイルに割った** (T24.8 の案 A) のを、**T.34.2 で関心ごとに割り直した**:
//! 閉じた理由は `recent/close_reason.rs`、閉じた接続の個票 (と接続ごとの積み上げ) は
//! `recent/entry.rs`、その固定長リングは `recent/ring.rs`、山の写真は `recent/burst.rs`、
//! 閉じた分布は `recent/closed.rs`、エラーの個票とリングは `recent/errors.rs`、接続の表と
//! リングの 1 枠は `recent/conns.rs`。ここには**共通の定数 (段階と側の添字) だけ**を置く。
//! 行を動かしただけで中身は変えていない。`pub use` で出し直すので、外からの `recent::…` の
//! 経路は変わらない。
//!
//! [`Metrics::record_error`]: crate::metrics::Metrics::record_error

mod burst;
mod close_reason;
mod closed;
mod conns;
mod entry;
mod errors;
mod ring;

pub use burst::*;
pub use close_reason::*;
pub use closed::*;
pub use conns::*;
pub use entry::*;
pub use errors::*;
pub use ring::*;

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
