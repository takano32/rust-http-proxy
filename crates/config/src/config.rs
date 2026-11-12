//! 起動時の設定 ([`Config`])。環境変数と `.env` とコマンドライン引数から 1 つの `Config` を作る。
//!
//! 責務ごとに子のファイルへ分けてある (T34.3)。公開の名前はここで出し直すので、呼ぶ側の綴り
//! (`config::connect_timeout_for` など) は `config.rs` が 1 枚だった頃と同じ:
//!
//! - `limits`: 同時接続・スレッド・記述子の上限と既定 (`auto` の決め方)
//! - `timeouts`: 締め切りの既定と実効値の決め方 (CONNECT・要求行・1 要求目・Happy Eyeballs の間隔)
//! - `keepalive` / `slo`: `PROXY_TCP_KEEPALIVE` と `PROXY_SLO` の読み方と書き戻し
//! - `source`: 効いている値の出どころ (`/config` の `source`) と `/config` の 1 行
//! - `parse`: 環境変数の値の純粋な読み方 (数か `auto`、`PROXY_PEEK_SNI`、一覧)
//! - `problems`: 読めない値と知らない名前 (`--check` と `settings.invalid` / `settings.unknown`)
//! - `defaults`: [`Config::new`] の既定値
//! - `from_env`: [`Config::from_env`] (環境変数を鍵の群ごとの段で読む)
//! - `settings`: [`Config::settings`] (`/config` と `--check` が並べる効いている値)
//! - `cache_sources`: キャッシュの設定の出どころの印

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::acl::{AclConfig, AllowLocal, ClientAcl, PortSet};
use crate::cache::CacheConfig;

mod cache_sources;
mod defaults;
mod from_env;
mod keepalive;
mod limits;
mod parse;
mod problems;
mod settings;
mod slo;
mod source;
mod timeouts;

use cache_sources::mark_cache_sources;
pub(crate) use keepalive::DEFAULT_TCP_KEEPALIVE;
pub use keepalive::TcpKeepalive;
pub(crate) use limits::{
    DEFAULT_BURST_PERCENT, DEFAULT_MAX_REQUESTS_PER_CONN, DEFAULT_PEEK_SNI_PORT,
    DEFAULT_SNAPSHOT_DAYS, MAX_SNAPSHOT_DAYS, burst_threshold, default_max_conns,
};
pub use limits::{FDS_PER_CONN, auto_max_conns, default_max_threads};
pub(crate) use parse::parse_bind_list;
pub use problems::{EnvProblems, InvalidVar, UnknownVar};
pub use slo::{DEFAULT_SLO, Slo};
pub use source::{Setting, Source, Sources, printable_value};
#[cfg(test)]
use timeouts::{DEFAULT_CONNECT_TIMEOUT, DEFAULT_HEADER_TIMEOUT};
pub use timeouts::{connect_timeout_for, first_read_timeout};
pub(crate) use timeouts::{header_timeout_for, parse_stagger_ms};

#[derive(Debug, Clone)]
pub struct Config {
    /// 待ち受けポート
    pub port: u16,
    /// 待ち受けアドレス。空ならデュアルスタック (`[::]` + `0.0.0.0`) を自動で試す
    pub bind_addrs: Vec<IpAddr>,
    /// IPv6 を使うか (待ち受けと AAAA での接続)。既定 on
    pub ipv6: bool,
    /// 待ち受けの受け入れ待ち行列の長さ (`PROXY_LISTEN_BACKLOG`、既定 `0` =
    /// `min(1024, /proc/sys/net/core/somaxconn)`。T14.47)。
    ///
    /// **持っているのは実効値** (`0` はここには残らない。`auto` を数に直す `max_conns` と同じ形)。
    /// `std` の `TcpListener::bind` は 128 固定で、accept ループが 1 本 (§4 の T4.3) なので
    /// ブラウザの同時 CONNECT で溢れ、**SYN が捨てられてクライアントが 1 秒後に再送する**。
    /// **`.env` の再読込では変わらない** (待ち受けは起動時に 1 回作るもの)
    pub listen_backlog: u32,
    pub acl: AclConfig,
    /// 接続・読み書きのタイムアウト (`PROXY_TIMEOUT_SECS`、既定 30 秒、`0` で無期限)。
    ///
    /// `0` は `PROXY_TUNNEL_IDLE_SECS` と同じく**無期限**の意味 (T10.6)。
    /// ソケットへ渡すときは [`proxy_base::timeout::for_socket`] で `None` に直す
    /// (`std` は `Duration::ZERO` を `InvalidInput` で断るため)。
    pub timeout: Duration,
    /// 転送中の最小の速さ (`PROXY_MIN_TRANSFER_BPS` / `PROXY_MIN_TRANSFER_WINDOW_SECS`。T29.34)。
    ///
    /// 相手を待った時間 (書けずに / 本文を読めずに待った時間) が窓に達するたびに、その間に
    /// 運べたバイトが `bps × 秒` に届かなければ切る。既定 30 秒の窓で 1 KiB/s、`bps = 0` で切らない。
    /// [`Config::timeout`] は無通信の締め切り (1 バイトでも動けば待ち直す) で、こちらとは別に効く
    pub min_transfer: proxy_base::minrate::MinRate,
    /// **オリジンへの TCP の確立だけ** (CONNECT と、T28.8 から forward) に効く締め切り (`PROXY_CONNECT_TIMEOUT_SECS`、`0` で無期限)。
    ///
    /// **既定は 3 秒** ([`DEFAULT_CONNECT_TIMEOUT`](timeouts::DEFAULT_CONNECT_TIMEOUT)。T15.6 (2)、T23.3、T28.43)。実効値は
    /// [`connect_timeout_for`] が 1 か所で決める (未設定なら 3 秒と [`Config::timeout`] の
    /// 小さい方。`PROXY_TIMEOUT_SECS=0` でも 3 秒)。`PROXY_TIMEOUT_SECS` は CONNECT の
    /// オリジン接続だけでなく forward の読み書き・クライアントソケット・キャッシュの
    /// 合流待ちにも効くので、「繋がらない相手を待つ時間」だけを縮めるにはこちらを使う。
    ///
    /// **持っているのは実効値** (`Option` にしない。明示の `0` = 無期限はそのまま写す。T10.6)。
    /// 効くのは `crates/server/src/conn.rs` の CONNECT から `start_tunnel` へ渡す所と、forward の
    /// `http::Shared::connect_timeout` (TCP の確立だけ。T28.8) の 2 か所で、forward の読み書きと
    /// blocklist の取得は `timeout` のまま。
    pub connect_timeout: Duration,
    /// 確立が続けて時間切れになった宛先 (`host:port`) を待たずに 504 で断る長さ
    /// (`PROXY_CONNECT_NEGATIVE_SECS`、既定 30 秒、`0` で覚えない。T36.8)。
    ///
    /// 同じ宛先が 2 回続けて時間切れになったら、この長さのあいだは繋ぎに行かない。過ぎたら 1 本だけ通して
    /// 試す。起動時と `.env` の再読込で `net::set_connect_negative` へ渡す (表は `proxy-net-conn` が持つ)
    pub connect_negative: Duration,
    /// 要求行とヘッダーを読み終えるまでの締め切り (`PROXY_HEADER_TIMEOUT_SECS`、既定 10 秒、`0` で無し。T28.0)。
    ///
    /// **1 バイト目が来てから最後のヘッダーまでの総時間**を縛る (`SO_RCVTIMEO` は読めるたびに
    /// 待ち直すので、1 バイトずつ送られると `PROXY_TIMEOUT_SECS` では切れない)。1 要求目は
    /// accept から 1 バイト目までもこの長さで切る ([`first_read_timeout`])。2 要求目以降の
    /// 1 バイト目までは keep-alive のアイドル (`keepalive`) のまま。越えたら `408` で閉じる。
    /// 実効値は [`header_timeout_for`] が 1 か所で決める (未設定なら 10 秒と `timeout` の小さい方)
    pub header_timeout: Duration,
    /// Happy Eyeballs で次の候補を試し始めるまでの間隔 (`PROXY_HE_STAGGER_MS`、既定 250 ms。T17.6)。
    ///
    /// 範囲は [`crate::net::STAGGER_RANGE_MS`] (10〜2,000 ms)。外れた値と読めない値は warn を 1 行出して
    /// 既定に戻す (出どころも `default` のまま)。**起動時に 1 回だけ** `net::set_stagger` へ渡し、
    /// `.env` の再読込では変わらない (変えたら `restart_required` に出る)
    pub he_stagger: Duration,
    /// クライアント接続を keep-alive で待つアイドル時間。0 なら 1 接続 1 要求
    pub keepalive: Duration,
    /// オリジンへのアイドル接続をホストごとに何本まで保持するか。0 で再利用しない
    pub pool_per_host: usize,
    /// アイドル接続の全体上限 (`PROXY_ORIGIN_POOL_TOTAL`)。ホスト数 × per_host の歯止め
    pub pool_total: usize,
    /// アイドルな keep-alive 接続をスレッドから外し、1 本の監視スレッド (epoll) に
    /// 預けるか (`PROXY_PARK_IDLE`、既定 on)。Linux 以外では自動的に無効。
    pub park_idle: bool,
    /// 預ける前に同じスレッドで待ってみる時間 (`PROXY_PARK_GRACE_MS`)。
    ///
    /// 続けて要求が来る忙しい接続に、預ける/戻すの往復 (epoll_ctl 2 回 + ワーカーの
    /// 受け渡し) を払わせないための猶予。0 なら猶予なしで即座に預ける。
    pub park_grace: Duration,
    /// malloc のアリーナ数の上限 (`PROXY_MALLOC_ARENAS`、`0` で glibc の既定のまま)。
    ///
    /// glibc の既定は「コア数 × 8」で、スレッドごとに別のアリーナを使う。接続ごとに
    /// スレッドが増えるので、アイドル接続を多く抱えると使われないアリーナが RSS に居座る。
    /// 実測 (2,000 本のアイドル keep-alive 接続): 既定 80.5 kB/接続 → 上限 8 で 26.8 kB (-67%)。
    /// 代償は高並列でのロック競合で、実測は conc=64 の CPU/要求 +4%、conc=8 では差なし。
    pub malloc_arenas: usize,
    /// glibc の mmap の閾を固定する値 (KiB。`PROXY_MALLOC_MMAP_THRESHOLD_KB`、`0` で glibc の既定 =
    /// 動く閾のまま。T32.3)。
    ///
    /// glibc は mmap で取った塊を free すると閾をその大きさまで上げる (起動中に 2 MiB、14 MiB の
    /// キャッシュの本文を捨てると 14 MiB)。以後は閾より小さい大きな確保がアリーナから取られ、
    /// 主でないアリーナの天辺の空きは `malloc_trim(0)` でも返らない (実測: 14 MiB の GET→PURGE ×3
    /// のあと trim しても `heap_free` 15.8 MB が残る)。値を固定すると動く閾が止まり、
    /// trim の閾 (`M_TRIM_THRESHOLD`) はその 4 倍にする。
    pub malloc_mmap_threshold_kb: usize,
    /// HTTPS のオリジンへ取得に行くか (システムの OpenSSL を使う)
    pub tls_enabled: bool,
    /// オリジンの証明書を検証するか
    pub tls_verify: bool,
    /// 追加の CA 証明書ファイル (PEM)。無ければシステムの CA ストア
    pub tls_ca_file: Option<PathBuf>,
    /// 名前解決の結果を保持する時間 (`PROXY_DNS_TTL_SECS`、0 で無効)
    pub dns_ttl: Duration,
    /// 解決の失敗を覚えておく時間 (`PROXY_DNS_NEGATIVE_SECS`、0 で覚えない)。
    ///
    /// デプロイ先では失敗する解決が 1 回 約 2 秒かかる (58.6 時間で 80 件)。5 秒では
    /// 「直後の再試行」しか捉えられていなかったので 60 秒にした (T13.1)
    pub dns_negative: Duration,
    /// 「熱い」と見なす窓 (`PROXY_DNS_WARM_SECS`、0 で keep-warm を止める)。
    ///
    /// この秒数に **2 回以上**使われた名前は、使われなくても 3/4 TTL ごとに裏で引き直す。
    /// TTL (60 秒) を熱さの物差しにすると、間隔が 2〜10 分のデプロイ先の主要ホストを
    /// 1 つも救えなかった (T14.1)
    ///
    /// 既定は 0 (止める。T22.4): serve-stale (`dns_stale`) のもとでは warm の引き直しがミスを
    /// ほとんど減らさない。3,600 で T21.3 までの動き (`proxy_net_dns::dns::WARM_DEFAULT` の doc)
    pub dns_warm: Duration,
    /// 期限切れの答えを同期で引かずに返してよい齢 (`PROXY_DNS_STALE_SECS`、0 で止める。T21.3)。
    ///
    /// この齢までは古い答えで繋ぎに行き、裏で 1 回引き直す (serve-stale、RFC 8767)。
    /// 既定 21,600 秒は `scripts/dns-replay.py --serve-stale` の再生で決めた
    pub dns_stale: Duration,
    /// 要求の経路が名前解決を待つ締め切り (`PROXY_DNS_TIMEOUT_SECS`、既定 10 秒、0 で締め切りなし。T32.11)。
    ///
    /// `getaddrinfo` はキャンセルできないので、上限付きの別のスレッド群 (`PROXY_MAX_THREADS` の 1/4)
    /// に引かせ、過ぎたら 504 で接続の枠を手放す。上限の本数がもう走っていれば待たずに 504
    pub dns_timeout: Duration,
    /// canary (`PROXY_CANARY`、`auto` | `off` | `host1,host2`、既定 `auto`)。
    ///
    /// 利用者の要求が無い時間帯も名前解決と TCP 接続の時間を測るための宛先 (T14.10)。
    /// 解釈するのは `proxy-metrics` の `canary` なので、ここでは文字列のまま持つ
    pub canary: String,
    /// canary の周期 (`PROXY_CANARY_SECS`、既定 60 秒、最小 1)。
    ///
    /// **試験で短くするための口**で、運用では触らない (60 秒に 1 回・1 ホスト 1 本)
    pub canary_secs: Duration,
    /// canary の IPv6 側 (`PROXY_CANARY_IPV6`、既定 on)。
    ///
    /// 同じ周期に canary の名前の **AAAA へ 1 本**だけ繋いでみて、`/status` の
    /// `canary.ipv6_connect_ms` に残す (T14.37)。この 1 本が試している間は利用者の経路で
    /// 600 秒に 1 回の IPv6 の探りをせず、繋がったら `v4_first` を解く (T17.7)
    pub canary_ipv6: bool,
    /// `/proxy.pac` で DIRECT にするホストの一覧 (`PROXY_PAC_DIRECT`、`*.example.com` 可)
    pub pac_direct: Vec<String>,
    /// ブロックリストのファイル (`PROXY_BLOCKLIST_FILE`、hosts 形式 / 1 行 1 ドメイン)
    pub blocklist_file: Option<PathBuf>,
    /// ブロックリストの URL (`PROXY_BLOCKLIST_URL`)
    pub blocklist_url: Option<String>,
    /// URL を取り直す間隔 (`PROXY_BLOCKLIST_REFRESH_SECS`)
    pub blocklist_refresh: Duration,
    /// ブロックリストの対象外にするホスト (`PROXY_BLOCKLIST_EXEMPT`、`*.example.com` 可)
    pub blocklist_exempt: Vec<String>,
    /// 統計と履歴を `$HOME/.rust-http-proxy.rrd` に残す (`PROXY_STATS_PERSIST`、既定 on)
    pub stats_persist: bool,
    /// 日次の `/snapshot` を `$HOME/.rust-http-proxy/snapshots/` に残す日数
    /// (`PROXY_SNAPSHOT_DAYS`、既定 [`DEFAULT_SNAPSHOT_DAYS`] = 30、**`0` で書かない**)。
    ///
    /// 書くのは履歴スレッドなので `PROXY_STATS_PERSIST=off` (と `--lite`) では 0 と同じ (T14.34)
    pub snapshot_days: usize,
    /// SLO の 4 つの閾 (`PROXY_SLO`、既定 [`DEFAULT_SLO`]。T14.50)。
    ///
    /// 履歴スレッドが 5 秒の標本ごとに判定し、達成率は `/slo` で読む。**再起動で反映**
    pub slo: Slo,
    /// 最速の素通しプロファイル (`PROXY_PROFILE=lite` / `--lite`)。
    /// キャッシュ・統計の永続化・ブロックリストを止め、ログを warn にする
    pub lite: bool,
    /// `/profile` のスレッドの標本を取る間隔 (`PROXY_PROFILE_SAMPLE_MS`、既定 1,000、
    /// `0` で止める)。**`--lite` では `/profile` ごと off** (T14.3 (2))
    pub profile_sample_ms: u64,
    /// 起動直後に **loopback だけで 3 秒**の自己ベンチを回すか
    /// (`PROXY_SELF_BENCH`、既定 off。T14.43)。
    ///
    /// `on` のときだけ、待ち受けを開いた直後に内蔵の小さなオリジンを立て、自分の待ち受けへ
    /// forward 8 並列と CONNECT 8 並列を 1.5 秒ずつ流して CPU/要求 と CPU/本 を測る
    /// (`/status` の `self_bench`)。**外へは 1 バイトも出さない。**
    /// `off` (既定) では `crates/run/src/startup.rs` の分岐 1 回だけで、自己ベンチのコードは 1 命令も走らない
    pub self_bench: bool,
    /// CONNECT を許すあて先ポート (`PROXY_CONNECT_PORTS`、既定は制限なし)
    pub connect_ports: PortSet,
    /// ループバック・リンクローカル・私設アドレス宛てのオリジンを許すか (`PROXY_ALLOW_LOCAL`、既定 off)。
    /// 既定ではクラウドのメタデータ (`169.254.169.254`) 経由の SSRF と、私設アドレス
    /// (`10/8`・`172.16/12`・`192.168/16`・`100.64/10`・`fc00::/7`) 宛てを 403 で止める (T28.41)。
    /// `private` は私設アドレスだけ通す ([`AllowLocal`])
    pub allow_local: AllowLocal,
    /// CONNECT トンネルのアイドル打ち切り時間 (`PROXY_TUNNEL_IDLE_SECS`、既定 300 秒、`0` で無期限)
    pub tunnel_idle: Duration,
    /// 同時に受ける接続数の上限 (`PROXY_MAX_CONNS`、既定 `auto`、`0` で無制限)。
    /// 超えた接続には 503 を返して閉じる (スレッドは起こさない)。`auto` の決め方は [`auto_max_conns`]
    pub max_conns: usize,
    /// 1 本のクライアント接続 (keep-alive) で処理する要求数の上限 (既定 [`DEFAULT_MAX_REQUESTS_PER_CONN`])。
    ///
    /// **環境変数では変えない** (運用で触る値ではなく、上限に当たったときの振る舞いを
    /// 試すための口。T14.2)。上限の要求には応答に `Connection: close` を付けてから閉じるので、
    /// クライアントは「次も使える」と思ったまま閉じられることがない。
    /// `0` は 1 接続 1 要求 (`PROXY_KEEPALIVE_SECS=0` と同じ形)。
    pub max_requests_per_conn: usize,
    /// 同時に生きていてよい接続スレッドの上限 (`PROXY_MAX_THREADS`、既定 `auto`、`0` で無制限)。
    ///
    /// 上限に達したら新しいスレッドを起こさず仕事を待たせる (捨てない)。
    /// **`.env` の再読込で変わる** (T11.6。`serve` が接続ごとにこの値と `Workers` の
    /// 上限を突き合わせ、食い違ったときだけ当て直す)。`auto` の決め方は [`default_max_threads`]
    pub max_threads: usize,
    /// 同時接続数が `max_conns` のこの割合を越えた瞬間に `/bursts` へ写真を 1 枚撮る
    /// (`PROXY_BURST_PERCENT`、既定 [`DEFAULT_BURST_PERCENT`]、`0` で撮らない。T14.6)
    pub burst_percent: usize,
    /// 上の割合を `max_conns` に当てた本数 ([`burst_threshold`])。**この本数を越えた瞬間**に 1 枚。
    /// [`usize::MAX`] なら撮らない (accept の経路はこの値との比較 1 回だけ)
    pub burst_at: usize,
    pub cache: CacheConfig,
    /// 内部エンドポイントの**書き換える口**を断る (`PROXY_ENDPOINTS_READONLY`、既定 off)。
    ///
    /// `on` にすると `/purge` / `PURGE` / `/blocklist?action=` が 405 になる。読む口
    /// (`/status` `/hosts` `/blocklist?host=` の判定だけ 等) は今までどおり。
    /// **認証ではない** (誰でも読める。公開ポートで「消せる口」だけ閉じるためのつまみ。T14.18)
    pub endpoints_readonly: bool,
    /// 受ける接続元の許可リスト (`PROXY_ALLOW_CLIENTS`、既定 空 = 全許可)。
    ///
    /// ここに無い接続元は **accept した直後に、要求を読まずに閉じる** (内部エンドポイントも
    /// 含めて閉じる = 公開ポートで個票を見せないため)。宛先の `PROXY_ALLOW_HOSTS` と
    /// `PROXY_ALLOW_LOCAL` とは無関係。**認証ではない** (T14.18)
    pub allow_clients: ClientAcl,
    /// 1 つの接続元から同時に受ける接続数の上限 (`PROXY_MAX_CONNS_PER_CLIENT`、既定 `0` = 無効)。
    ///
    /// 設定されている間だけ、accept 直後にその接続元の**生きている接続の本数**を数え、
    /// 上限以上なら `503` + `Retry-After: 1` を返して閉じる (断った数は `/status` の
    /// `rejected_per_client`)。自分宛て (内部エンドポイント) は T13.2 の「上限 + 4 本」の枠で
    /// 受けてから判定するので、上限に当たっている接続元からでも `/status` は取れる。
    /// **認証ではなく公平さの上限** (見知らぬ接続元が `PROXY_MAX_CONNS` を 1 人で使い切ると
    /// 本人が 503 になるため。T14.13)
    pub max_conns_per_client: usize,
    /// 追跡する接続元 (`PROXY_TRACE_CLIENT`、既定 `None` = 無効。T14.27)。
    ///
    /// 設定されている間だけ、accept 直後に接続元がこの IP と一致するかを 1 回見て、
    /// 一致した接続にだけ `ConnSlot` の旗を立てる。旗が立った接続は要求行 (パスの先頭
    /// 256 B) と応答の状態・段階の ms・CONNECT の閉じた理由を `/trace` のリングに残す。
    /// **要求ごとの費用は旗を読む分岐 1 回**で、既定 (`None`) では accept ごとの
    /// `is_some()` 1 回だけ。**v4-mapped IPv6 は IPv4 に直して覚える** (接続元の照合は
    /// `net::canonical_ip` を通った値と比べるため)
    pub trace_client: Option<IpAddr>,
    /// CONNECT の最初のバイトから SNI を覗くか (`PROXY_PEEK_SNI`、既定 `on` = 443 だけ。T14.38)。
    ///
    /// `off` は `None` (1 度も覗かない)、`on` は `Some(443)`、`on:<port>` は
    /// `Some(<port>)` で **443 に加えて**そのポートでも覗く (試験用の口)。
    /// 覗くのは `200 Connection Established` のあと**最初の中継の前に 1 回**
    /// (`recv(MSG_PEEK)` 1 回。バイトは消費しない)。`--lite` では個票の枠が無いので覗かない
    pub peek_sni: Option<u16>,
    /// 記録の一括 off とハッシュ化 (`PROXY_RECORDS`、既定 `on`。T14.41)。
    ///
    /// `on` は今までどおり、`off` は個票のリング (`/recent` `/errors` `/connections`
    /// `/clients` `/log` `/events` `/trace` `/bursts`) に 1 件も書かない、`hashed` は
    /// 接続元 IP を[起動ごとの乱数つき FNV-1a 64 ビットの 16 進 16 桁]に置き換えて残す。
    /// **止まるのは接続元の側だけ**で、ホスト別の統計 (`/hosts`) と `/history` は
    /// `off` でも残る。当てる先はプロセス全体の旗 ([`crate::records::set`]) で、
    /// `.env` で書き換えると**次の記録から**効く
    pub records: crate::records::Mode,
    /// 消えたクライアントの検知 (`PROXY_TCP_KEEPALIVE`、既定 `on` = 60:10:3。T14.52)。
    ///
    /// `Some` なら **accept 直後にクライアント側のソケットへ `setsockopt` を 4 回**当てる
    /// (`SO_KEEPALIVE` / `TCP_KEEPIDLE` / `TCP_KEEPINTVL` / `TCP_KEEPCNT`)。`None` (`off`) は
    /// 1 回も呼ばない。`--lite` でも当てる (枠が無くても「消えた相手」は閉じたい)。
    /// `.env` で書き換えると**次に受ける接続から**効く (開いている接続には当て直さない)
    pub tcp_keepalive: Option<TcpKeepalive>,
    /// 各値の出どころ (`/config` の `source`。T14.15)。効いた値にだけ印が付く
    pub sources: Sources,
    /// 読めなかった値と知らない名前 (`--check` と `settings.invalid` / `settings.unknown`。T28.6)
    pub problems: EnvProblems,
}

#[cfg(test)]
mod tests;
