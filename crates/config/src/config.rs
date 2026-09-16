use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::acl::{AclConfig, ClientAcl, PortSet};
use crate::cache::config::DEFAULT_TARGET_PERCENT;
use crate::cache::{CacheConfig, DiskQuota, Limit, MIB};
use crate::envfile;

/// 1 接続が最悪で使う記述子の数: クライアント 1 + オリジン 1 + 素通し中のパイプ 2 (`splice`)。
pub const FDS_PER_CONN: u64 = 4;

/// 接続以外で使う記述子の予備: 待ち受け (最大 2) + epoll + inotify + 状態ファイル +
/// ブロックリストの取得 + キャッシュのディスク I/O + 標準入出力。多めに見て 64。
const NOFILE_RESERVE: u64 = 64;

/// `PROXY_MAX_CONNS=auto` の頭打ち。
///
/// 記述子が余っていてもここで止める。上限は記述子だけの歯止めではなく、
/// **fd 以外の資源 (スレッド・RSS) の歯止め**でもあるため。T2.1 で入れた既定と同じ値で、
/// 根拠は T2.3 の実測 (同時 5,000 本のアイドル接続で RSS 198 MiB。動作環境のコンテナは小さい)。
/// これを超える値が要るなら数値で明示してもらう。
pub const MAX_CONNS_CAP: usize = 4096;

/// `RLIMIT_NOFILE` の soft limit から同時接続数の上限を決める (`PROXY_MAX_CONNS=auto`)。
///
/// `(soft - 予備 64) / 4` を [`MAX_CONNS_CAP`] で頭打ちにした値。1 接続あたり最悪 4 記述子なので、
/// `ulimit -n` が 1024 の環境なら 240、4096 なら 1008 で、**`accept` が `EMFILE` で失敗する前に
/// 503 で断れる**。`0` (無制限) には決してしない (記述子切れに戻ってしまうため下限は 1)。
pub fn auto_max_conns(nofile_soft: u64) -> usize {
    let usable = nofile_soft.saturating_sub(NOFILE_RESERVE) / FDS_PER_CONN;
    usable.clamp(1, MAX_CONNS_CAP as u64) as usize
}

/// `PROXY_MAX_CONNS` の既定 (= `auto`)。`RLIMIT_NOFILE` が読めない環境では [`MAX_CONNS_CAP`]。
pub fn default_max_conns() -> usize {
    #[cfg(target_os = "linux")]
    if let Some(soft) = crate::sys::max_open_files() {
        return auto_max_conns(soft);
    }
    MAX_CONNS_CAP
}

/// 山の写真 (`/bursts`) を撮る割合の既定 (`PROXY_BURST_PERCENT`。T14.6)。
///
/// `PROXY_MAX_CONNS` の半分を越えたら 1 枚撮る。半分なのは、デプロイ先の山 (218 / 240) が
/// 上限の 91% まで行った実測 (§2) に対して、**山が立ち始めた時点**を残したいため
/// (上限の間際まで待つと、T13.2 の追い出しが動いたあとの姿しか撮れない)。
pub const DEFAULT_BURST_PERCENT: usize = 50;

/// 「この本数を**越えた**瞬間に 1 枚撮る」の本数 (T14.6)。
///
/// [`usize::MAX`] は「撮らない」(`PROXY_BURST_PERCENT=0`、`PROXY_MAX_CONNS=0` = 無制限、
/// `--lite`)。**接続ごとの比較を 1 回で済ませるために先に計算しておく** — 撮らない設定でも
/// 比較の形は同じなので、accept の経路には分岐が 1 つ増えるだけで済む。
///
/// 割合を当てるのは `max_conns` だけで、**T13.2 の「上限の外の枠 4 本」は含めない**
/// (自分宛ての `/status` を受けるための枠なので、山の大きさの物差しに混ぜない)。
pub fn burst_threshold(max_conns: usize, percent: usize) -> usize {
    if max_conns == 0 || percent == 0 {
        return usize::MAX;
    }
    (max_conns.saturating_mul(percent.min(100)) / 100).max(1)
}

/// `PROXY_MAX_THREADS=auto` の 1 コアあたりの本数と、その下限・上限。
///
/// 接続スレッドは**ほとんどの時間 I/O で寝ている**ので、コア数そのものでは全く足りない
/// (1 本の接続を処理しているあいだずっと 1 本要る)。一方でいくら増やしても得るものは無く、
/// 1 本あたりスタック 256 KiB と切り替えの費用がかかる。
///
/// 実測 (T10.5、`--only idle-tunnels --conc 5000`、プロキシは 4 コアに固定)。
/// 暇なトンネルは 1 本ごとに猶予 100 ms のあいだワーカーを握るので、**確立できる速さは
/// おおよそ「上限 ÷ 100 ms」**になる。上限が無いと同じ場面で 4,721 スレッドまで跳ねる。
///
/// | 上限 | 5,000 本の確立 | スレッド最大 | ピーク RSS |
/// |---|---|---|---|
/// | 64 (コア数 × 16) | 8.4 s | 68 | 26.9 MB |
/// | 128 (コア数 × 32) | 4.2 s | 132 | 26.7 MB |
/// | 192 | 3.0 s | 196 | 27.6 MB |
/// | **256 (コア数 × 64、既定)** | **2.2〜2.6 s** | **260** | **28.1 MB** |
/// | 384 | 1.8 s | 388 | 31.4 MB |
/// | 無制限 (T10.5 以前) | 1.8 s | 4,721 | 72.6 MB |
///
/// 確立の速さがほぼ元に戻り、跳ね上がりも 1 桁小さいところとして **コア数 × 64** を採った。
const THREADS_PER_CORE: usize = 64;
const MIN_MAX_THREADS: usize = 128;
const MAX_MAX_THREADS: usize = 512;

/// 生きている接続スレッドの上限の既定 (`PROXY_MAX_THREADS=auto`)。
///
/// `コア数 × 64` を 128〜512 に収め、`PROXY_MAX_CONNS` があればそれも超えない
/// (受けない接続のためのスレッドは要らない)。コア数は `available_parallelism` なので、
/// `taskset` で絞られていればその数になる (使える資源に合わせる)。
/// **`0` (無制限) には決してしない** — 上限が無いと、預けた接続が一斉に切れたときに
/// スレッドが数千まで跳ねる (T8.1 で 4,621、T10.5 で 4,721 の実測)。
pub fn default_max_threads(max_conns: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let n = cores
        .saturating_mul(THREADS_PER_CORE)
        .clamp(MIN_MAX_THREADS, MAX_MAX_THREADS);
    if max_conns > 0 { n.min(max_conns) } else { n }
}

/// 1 本のクライアント接続で処理する要求数の既定の上限 (keep-alive)。
///
/// 認証なしの開放プロキシなので、1 本の接続を無限に使い回されないところで切る。
/// 上限の要求は普通に応答し、その応答に `Connection: close` を付けてから閉じる。
pub const DEFAULT_MAX_REQUESTS_PER_CONN: usize = 1000;

/// 設定 1 つ 1 つの**効いている値がどこから来たか** (`/config` の `source`。T14.15)。
///
/// 「書いてある場所」ではなく「**効いた値の出どころ**」を指す: 読めない書き方 (`abc` を
/// 秒数に書いたなど) は既定に落ちるので、そのキーは `Default` のままになる
/// (書いたのに効いていないことが `/config` で分かる)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Source {
    /// このコードの既定値 (どこにも書かれていない、または書いたが読めなかった)
    #[default]
    Default,
    /// 実際の環境変数 (Pterodactyl のパネルが渡すもの)
    Env,
    /// `$HOME/.env`
    EnvFile,
    /// コマンドライン引数
    Cli,
}

impl Source {
    /// いま `key` がどの層から読めるか ([`envfile::var`] と同じ優先順)。
    pub fn of(key: &str) -> Source {
        match envfile::var_source(key) {
            Some(envfile::VarSource::Cli) => Source::Cli,
            Some(envfile::VarSource::File) => Source::EnvFile,
            Some(envfile::VarSource::Env) => Source::Env,
            None => Source::Default,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Env => "env",
            Source::EnvFile => "env_file",
            Source::Cli => "cli",
        }
    }
}

/// キーごとの出どころ。**効いた値にだけ印が付く** (印の無いキーは [`Source::Default`])。
///
/// 数十件しか無く、引くのは `/config` と `--check` と再読込のときだけなので、
/// 連想配列ではなく小さな配列で足りる (設定は接続ごとに複製されるので軽い方がよい)。
#[derive(Debug, Clone, Default)]
pub struct Sources(Vec<(&'static str, Source)>);

impl Sources {
    /// `key` の値が効いたので、いまどの層から来ているかを覚える。
    pub fn mark(&mut self, key: &'static str) {
        let src = Source::of(key);
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = src,
            None => self.0.push((key, src)),
        }
    }

    pub fn get(&self, key: &str) -> Source {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map_or(Source::Default, |(_, s)| *s)
    }

    /// 再読込で作り直した設定から 1 つだけ写す (**当てた項目だけ**呼ぶ。T14.15)。
    /// 起動時に固定される項目 (ポートや TLS) は `.env` が変わっても効かないので、
    /// 写さずに起動時の出どころを残す。
    pub fn adopt(&mut self, fresh: &Sources, key: &'static str) {
        let src = fresh.get(key);
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = src,
            None if src != Source::Default => self.0.push((key, src)),
            None => {}
        }
    }
}

/// `/config` と `--check` が並べる 1 行。
#[derive(Debug, Clone)]
pub struct Setting {
    /// 環境変数名 (`PROXY_*` / `SERVER_*`)
    pub key: &'static str,
    /// **いま効いている値**を JSON の値として書いたもの (数・真偽・文字列・配列・`null`)
    pub value: String,
    /// その値の出どころ
    pub source: Source,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// 待ち受けポート
    pub port: u16,
    /// 待ち受けアドレス。空ならデュアルスタック (`[::]` + `0.0.0.0`) を自動で試す
    pub bind_addrs: Vec<IpAddr>,
    /// IPv6 を使うか (待ち受けと AAAA での接続)。既定 on
    pub ipv6: bool,
    pub acl: AclConfig,
    /// 接続・読み書きのタイムアウト (`PROXY_TIMEOUT_SECS`、既定 30 秒、`0` で無期限)。
    ///
    /// `0` は `PROXY_TUNNEL_IDLE_SECS` と同じく**無期限**の意味 (T10.6)。
    /// ソケットへ渡すときは [`proxy_base::timeout::for_socket`] で `None` に直す
    /// (`std` は `Duration::ZERO` を `InvalidInput` で断るため)。
    pub timeout: Duration,
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
    pub dns_warm: Duration,
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
    /// `canary.ipv6_connect_ms` に残す。デプロイ先のコンテナは IPv6 が黒穴で、
    /// `v4_first` の解除は 600 秒に 1 回の探りだけに頼っているため (T14.37)
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
    /// 最速の素通しプロファイル (`PROXY_PROFILE=lite` / `--lite`)。
    /// キャッシュ・統計の永続化・ブロックリストを止め、ログを warn にする
    pub lite: bool,
    /// `/profile` のスレッドの標本を取る間隔 (`PROXY_PROFILE_SAMPLE_MS`、既定 1,000、
    /// `0` で止める)。**`--lite` では `/profile` ごと off** (T14.3 (2))
    pub profile_sample_ms: u64,
    /// CONNECT を許すあて先ポート (`PROXY_CONNECT_PORTS`、既定は制限なし)
    pub connect_ports: PortSet,
    /// ループバック・リンクローカル宛てのオリジンを許すか (`PROXY_ALLOW_LOCAL`、既定 off)。
    /// 既定ではクラウドのメタデータ (`169.254.169.254`) 経由の SSRF を 403 で止める
    pub allow_local: bool,
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
    /// 各値の出どころ (`/config` の `source`。T14.15)。効いた値にだけ印が付く
    pub sources: Sources,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        // **効いた値にだけ**出どころの印を付ける (`/config` の `source`。T14.15)。
        // 読めない書き方は既定に落ちるので、その行は `default` のままになる
        let mut src = Sources::default();
        let port_str = envfile::var("SERVER_PORT").unwrap_or_else(|| "8080".to_string());
        let allow_hosts = envfile::var("PROXY_ALLOW_HOSTS");
        let deny_hosts = envfile::var("PROXY_DENY_HOSTS");
        // `0` は無期限。1 秒に切り上げないのは、切り上げると**無期限を表す手段が
        // 設定から無くなる**ため (`PROXY_TUNNEL_IDLE_SECS` と意味を揃えた。T10.6)
        let timeout_secs = envfile::var("PROXY_TIMEOUT_SECS")
            .and_then(|s| s.parse::<u64>().ok())
            .inspect(|_| src.mark("PROXY_TIMEOUT_SECS"))
            .unwrap_or(30);

        let mut cfg = Self::new(
            &port_str,
            allow_hosts.as_deref(),
            deny_hosts.as_deref(),
            Duration::from_secs(timeout_secs),
        )?;
        if envfile::var("SERVER_PORT").is_some() {
            src.mark("SERVER_PORT");
        }
        if allow_hosts.is_some() {
            src.mark("PROXY_ALLOW_HOSTS");
        }
        if deny_hosts.is_some() {
            src.mark("PROXY_DENY_HOSTS");
        }
        // lite は「既定をまとめて off にする」だけなので、後続の環境変数が上書きできる
        cfg.lite =
            envfile::var("PROXY_PROFILE").is_some_and(|v| v.trim().eq_ignore_ascii_case("lite"));
        if cfg.lite {
            cfg.stats_persist = false;
            src.mark("PROXY_PROFILE");
        }
        if let Some(bind) = envfile::var("PROXY_BIND") {
            cfg.bind_addrs = parse_bind_list(&bind)?;
            src.mark("PROXY_BIND");
        }
        if let Some(secs) =
            envfile::var("PROXY_KEEPALIVE_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.keepalive = Duration::from_secs(secs);
            src.mark("PROXY_KEEPALIVE_SECS");
        }
        if let Some(n) =
            envfile::var("PROXY_ORIGIN_POOL").and_then(|s| s.trim().parse::<usize>().ok())
        {
            cfg.pool_per_host = n;
            src.mark("PROXY_ORIGIN_POOL");
        }
        if let Some(n) =
            envfile::var("PROXY_ORIGIN_POOL_TOTAL").and_then(|s| s.trim().parse::<usize>().ok())
        {
            cfg.pool_total = n;
            src.mark("PROXY_ORIGIN_POOL_TOTAL");
        }
        if let Some(n) =
            envfile::var("PROXY_MALLOC_ARENAS").and_then(|s| s.trim().parse::<usize>().ok())
        {
            cfg.malloc_arenas = n;
            src.mark("PROXY_MALLOC_ARENAS");
        }
        if let Some(ms) =
            envfile::var("PROXY_PARK_GRACE_MS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.park_grace = Duration::from_millis(ms);
            src.mark("PROXY_PARK_GRACE_MS");
        }
        let off = |v: String| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        };
        if let Some(v) = envfile::var("PROXY_PARK_IDLE") {
            cfg.park_idle = !off(v);
            src.mark("PROXY_PARK_IDLE");
        }
        if let Some(v) = envfile::var("PROXY_IPV6") {
            cfg.ipv6 = !off(v);
            src.mark("PROXY_IPV6");
        }
        if let Some(v) = envfile::var("PROXY_TLS") {
            cfg.tls_enabled = !off(v);
            src.mark("PROXY_TLS");
        }
        if let Some(v) = envfile::var("PROXY_TLS_VERIFY") {
            cfg.tls_verify = !off(v);
            src.mark("PROXY_TLS_VERIFY");
        }
        // `auto` (既定) は記述子の上限から決める。数値ならその値、`0` は無制限。
        // 読めない書き方は既定 (auto) のままにする
        if let Some(v) = envfile::var("PROXY_MAX_CONNS") {
            let v = v.trim();
            if v.eq_ignore_ascii_case("auto") {
                cfg.max_conns = default_max_conns();
                src.mark("PROXY_MAX_CONNS");
            } else if let Ok(n) = v.parse::<usize>() {
                cfg.max_conns = n;
                src.mark("PROXY_MAX_CONNS");
            }
        }
        // スレッドの上限は接続数の上限にも従うので、**PROXY_MAX_CONNS の後に**決める
        cfg.max_threads = default_max_threads(cfg.max_conns);
        if let Some(v) = envfile::var("PROXY_MAX_THREADS") {
            let v = v.trim();
            if let Ok(n) = v.parse::<usize>() {
                cfg.max_threads = n;
                src.mark("PROXY_MAX_THREADS");
            }
            // `auto` と読めない書き方は既定のまま
        }
        // 山の写真の閾 (T14.6)。**`PROXY_MAX_CONNS` の後に**決める (割合を当てる相手が上限)。
        // `--lite` では個票を 1 つも記録しないので、写真も撮らない (T1.4 の方針)
        if let Some(n) =
            envfile::var("PROXY_BURST_PERCENT").and_then(|s| s.trim().parse::<usize>().ok())
        {
            cfg.burst_percent = n.min(100);
            src.mark("PROXY_BURST_PERCENT");
        }
        cfg.refresh_burst_at();
        if let Some(secs) =
            envfile::var("PROXY_TUNNEL_IDLE_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.tunnel_idle = Duration::from_secs(secs);
            src.mark("PROXY_TUNNEL_IDLE_SECS");
        }
        if let Some(v) = envfile::var("PROXY_CONNECT_PORTS") {
            cfg.connect_ports = PortSet::parse(&v);
            src.mark("PROXY_CONNECT_PORTS");
        }
        if let Some(v) = envfile::var("PROXY_ALLOW_LOCAL") {
            cfg.allow_local = !off(v);
            src.mark("PROXY_ALLOW_LOCAL");
        }
        if let Some(v) = envfile::var("PROXY_ENDPOINTS_READONLY") {
            cfg.endpoints_readonly = !off(v);
            src.mark("PROXY_ENDPOINTS_READONLY");
        }
        if let Some(v) = envfile::var("PROXY_ALLOW_CLIENTS") {
            cfg.allow_clients = ClientAcl::parse(&v);
            src.mark("PROXY_ALLOW_CLIENTS");
        }
        if let Some(n) =
            envfile::var("PROXY_MAX_CONNS_PER_CLIENT").and_then(|s| s.trim().parse::<usize>().ok())
        {
            cfg.max_conns_per_client = n;
            src.mark("PROXY_MAX_CONNS_PER_CLIENT");
        }
        if let Some(v) = envfile::var("PROXY_STATS_PERSIST") {
            cfg.stats_persist = !off(v);
            src.mark("PROXY_STATS_PERSIST");
        }
        if let Some(path) = envfile::var("PROXY_TLS_CA_FILE").filter(|p| !p.trim().is_empty()) {
            cfg.tls_ca_file = Some(PathBuf::from(path.trim()));
            src.mark("PROXY_TLS_CA_FILE");
        }
        if let Some(secs) =
            envfile::var("PROXY_DNS_TTL_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.dns_ttl = Duration::from_secs(secs);
            src.mark("PROXY_DNS_TTL_SECS");
        }
        if let Some(secs) =
            envfile::var("PROXY_DNS_NEGATIVE_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.dns_negative = Duration::from_secs(secs);
            src.mark("PROXY_DNS_NEGATIVE_SECS");
        }
        if let Some(secs) =
            envfile::var("PROXY_DNS_WARM_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.dns_warm = Duration::from_secs(secs);
            src.mark("PROXY_DNS_WARM_SECS");
        }
        if let Some(v) = envfile::var("PROXY_CANARY") {
            cfg.canary = v.trim().to_ascii_lowercase();
        }
        if let Some(secs) =
            envfile::var("PROXY_CANARY_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.canary_secs = Duration::from_secs(secs.max(1));
        }
        if let Some(v) = envfile::var("PROXY_CANARY_IPV6") {
            cfg.canary_ipv6 = !off(v);
            src.mark("PROXY_CANARY_IPV6");
        }
        if let Some(ms) =
            envfile::var("PROXY_PROFILE_SAMPLE_MS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.profile_sample_ms = ms;
        }
        let list = |v: String| -> Vec<String> {
            v.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        };
        if let Some(v) = envfile::var("PROXY_PAC_DIRECT") {
            cfg.pac_direct = list(v);
            src.mark("PROXY_PAC_DIRECT");
        }
        cfg.blocklist_file = envfile::var("PROXY_BLOCKLIST_FILE")
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .inspect(|_| src.mark("PROXY_BLOCKLIST_FILE"));
        cfg.blocklist_url = envfile::var("PROXY_BLOCKLIST_URL")
            .map(|u| u.trim().to_string())
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .inspect(|_| src.mark("PROXY_BLOCKLIST_URL"));
        if let Some(secs) =
            envfile::var("PROXY_BLOCKLIST_REFRESH_SECS").and_then(|s| s.trim().parse::<u64>().ok())
        {
            cfg.blocklist_refresh = Duration::from_secs(secs.max(60));
            src.mark("PROXY_BLOCKLIST_REFRESH_SECS");
        }
        if let Some(v) = envfile::var("PROXY_BLOCKLIST_EXEMPT") {
            cfg.blocklist_exempt = list(v);
            src.mark("PROXY_BLOCKLIST_EXEMPT");
        }
        // ログの高さは `Config` に持たないが (`crate::log` の全体の状態)、
        // `/config` には効いている値と出どころを並べる
        if envfile::var("PROXY_LOG_LEVEL").is_some_and(|v| crate::log::Level::parse(&v).is_some()) {
            src.mark("PROXY_LOG_LEVEL");
        }
        let mut cache = CacheConfig::from_env();
        if cfg.lite {
            // lite ではブロックリストの取得もキャッシュもしない (明示指定があればそちらが勝つ)
            cfg.blocklist_file = None;
            cfg.blocklist_url = None;
            if envfile::var("PROXY_CACHE_ENABLED").is_none() {
                cache.enabled = false;
            }
        }
        mark_cache_sources(&mut src);
        cfg.sources = src;
        Ok(cfg.with_cache(cache))
    }

    /// `max_conns` か `burst_percent` を手で書き換えたあとに山の写真の閾を計算し直す (T14.6)。
    ///
    /// `from_env` は最後にこれと同じことをしている。**構造体の欄を直に書き換える場所**
    /// (テストと、設定を組み立てる道具) はここを呼ぶこと。
    pub fn refresh_burst_at(&mut self) {
        // `--lite` は個票を 1 つも記録しない (`/connections` の表も空) ので写真も撮らない
        self.burst_at = if self.lite {
            usize::MAX
        } else {
            burst_threshold(self.max_conns, self.burst_percent)
        };
    }

    /// 全 `PROXY_*` / `SERVER_*` の**効いている値**と、その出どころ (`/config` と `--check`。T14.15)。
    ///
    /// 値はいまプロキシが使っているもの (既定・`.env`・環境変数・引数のどれから来たかは
    /// `source`)。**このプロキシに秘密の設定は無い**ので、そのまま出してよい
    /// (`PROXY_TLS_CA_FILE` はパスだけで、証明書の中身は読まない)。
    /// 並べる順は「待ち受け → 接続 → 名前解決 → 遮断 → TLS → 記録 → キャッシュ」で、
    /// README の表と同じ流れにしてある。
    pub fn settings(&self) -> Vec<Setting> {
        let c = &self.cache;
        let mut out: Vec<Setting> = Vec::with_capacity(64);
        let mut add = |key: &'static str, value: String| {
            out.push(Setting {
                key,
                value,
                source: self.sources.get(key),
            })
        };
        let mib = |bytes: u64| (bytes / MIB).to_string();
        let secs = |d: Duration| d.as_secs().to_string();
        let path = |p: Option<&PathBuf>| {
            crate::json::quote_opt(p.map(|p| p.display().to_string()).as_deref())
        };
        // 待ち受け
        add("SERVER_PORT", self.port.to_string());
        add(
            "PROXY_BIND",
            list_value(
                &self
                    .bind_addrs
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>(),
            ),
        );
        add("PROXY_IPV6", self.ipv6.to_string());
        // 接続と上限
        add("PROXY_TIMEOUT_SECS", secs(self.timeout));
        add("PROXY_KEEPALIVE_SECS", secs(self.keepalive));
        add("PROXY_TUNNEL_IDLE_SECS", secs(self.tunnel_idle));
        add("PROXY_MAX_CONNS", self.max_conns.to_string());
        add(
            "PROXY_MAX_CONNS_PER_CLIENT",
            self.max_conns_per_client.to_string(),
        );
        add("PROXY_MAX_THREADS", self.max_threads.to_string());
        add("PROXY_ORIGIN_POOL", self.pool_per_host.to_string());
        add("PROXY_ORIGIN_POOL_TOTAL", self.pool_total.to_string());
        add("PROXY_PARK_IDLE", self.park_idle.to_string());
        add(
            "PROXY_PARK_GRACE_MS",
            self.park_grace.as_millis().to_string(),
        );
        add("PROXY_MALLOC_ARENAS", self.malloc_arenas.to_string());
        // 名前解決
        add("PROXY_DNS_TTL_SECS", secs(self.dns_ttl));
        add("PROXY_DNS_NEGATIVE_SECS", secs(self.dns_negative));
        add("PROXY_DNS_WARM_SECS", secs(self.dns_warm));
        // canary の IPv6 側 (T14.37)。宛先と周期は `proxy-metrics` の持ち物なので
        // ここには出さない (この層は文字列を運ぶだけ)
        add("PROXY_CANARY_IPV6", self.canary_ipv6.to_string());
        // あて先の許可・拒否
        add("PROXY_ALLOW_HOSTS", list_value(&self.acl.allow_hosts));
        add("PROXY_DENY_HOSTS", list_value(&self.acl.deny_hosts));
        add(
            "PROXY_CONNECT_PORTS",
            crate::json::quote(&self.connect_ports.spec()),
        );
        add("PROXY_ALLOW_LOCAL", self.allow_local.to_string());
        // 公開プロキシの止め方 (T14.18) と山の写真の閾 (T14.6)。マージのときに親が足した
        add(
            "PROXY_ENDPOINTS_READONLY",
            self.endpoints_readonly.to_string(),
        );
        add(
            "PROXY_ALLOW_CLIENTS",
            crate::json::quote(&self.allow_clients.to_string()),
        );
        add("PROXY_BURST_PERCENT", self.burst_percent.to_string());
        add("PROXY_BLOCKLIST_FILE", path(self.blocklist_file.as_ref()));
        add(
            "PROXY_BLOCKLIST_URL",
            crate::json::quote_opt(self.blocklist_url.as_deref()),
        );
        add("PROXY_BLOCKLIST_REFRESH_SECS", secs(self.blocklist_refresh));
        add("PROXY_BLOCKLIST_EXEMPT", list_value(&self.blocklist_exempt));
        add("PROXY_PAC_DIRECT", list_value(&self.pac_direct));
        // TLS (パスだけ。証明書の中身は読まない)
        add("PROXY_TLS", self.tls_enabled.to_string());
        add("PROXY_TLS_VERIFY", self.tls_verify.to_string());
        add("PROXY_TLS_CA_FILE", path(self.tls_ca_file.as_ref()));
        // 記録とプロファイル
        add("PROXY_STATS_PERSIST", self.stats_persist.to_string());
        add(
            "PROXY_PROFILE",
            crate::json::quote_opt(self.lite.then_some("lite")),
        );
        // `.env` にそのまま書き戻せる形で出す (読むのは大小を問わない)
        add(
            "PROXY_LOG_LEVEL",
            crate::json::quote(
                &crate::log::current_level()
                    .as_str()
                    .trim()
                    .to_ascii_lowercase(),
            ),
        );
        // キャッシュ (値は `CacheConfig` が持っているもの)
        add("PROXY_CACHE_ENABLED", c.enabled.to_string());
        add(
            "PROXY_CACHE_DIR",
            crate::json::quote(&c.dir.display().to_string()),
        );
        add("PROXY_MEM_CACHE_MB", limit_value(c.mem_limit));
        add("PROXY_DISK_CACHE_MB", limit_value(c.disk_limit));
        add(
            "PROXY_MEM_TARGET_PERCENT",
            c.mem_limit
                .target_percent()
                .unwrap_or(DEFAULT_TARGET_PERCENT)
                .to_string(),
        );
        add(
            "PROXY_DISK_TARGET_PERCENT",
            c.disk_limit
                .target_percent()
                .unwrap_or(DEFAULT_TARGET_PERCENT)
                .to_string(),
        );
        add("PROXY_MEM_KEEP_FREE_MB", mib(c.mem_keep_free));
        add("PROXY_DISK_KEEP_FREE_MB", mib(c.disk_keep_free));
        add(
            "PROXY_CACHE_RESERVE",
            crate::json::quote(&c.reserve.to_string()),
        );
        add("PROXY_CACHE_PROBE_SECS", secs(c.probe_interval));
        add("PROXY_CACHE_TTL_SECS", secs(c.default_ttl));
        add(
            "PROXY_CACHE_HEURISTIC_PERCENT",
            c.heuristic_percent.to_string(),
        );
        add("PROXY_CACHE_HEURISTIC_MAX_SECS", secs(c.heuristic_max));
        add("PROXY_CACHE_MAX_STALE_SECS", secs(c.max_stale));
        add("PROXY_CACHE_GRACE_SECS", secs(c.grace));
        add("PROXY_STALE_WAIT_SECS", secs(c.stale_wait));
        add("PROXY_CACHE_MAX_OBJECT_MB", mib(c.max_object_size));
        add("PROXY_MEM_CACHE_MAX_OBJECT_MB", mib(c.mem_max_object_size));
        add("PROXY_DISK_MAX_ENTRIES", c.disk_max_entries.to_string());
        add("PROXY_CACHE_ADMISSION", c.admission.to_string());
        add("PROXY_NEGATIVE_TTL_SECS", secs(c.negative_ttl));
        // ディスク割当 (`SERVER_DISK` は `PROXY_DISK_QUOTA_MB` の別名。効いた方にだけ
        // `source` が付き、値はどちらの行にも同じ「効いている割当」が出る)
        add("PROXY_DISK_QUOTA_MB", quota_value(c.disk_quota));
        add("SERVER_DISK", quota_value(c.disk_quota));
        add("PROXY_DISK_QUOTA_ROOT", path(c.quota_root.as_ref()));
        add("PROXY_DISK_PROBE", c.disk_probe.to_string());
        add(
            "SERVER_MEMORY",
            c.mem_alloc
                .map(|b| (b / MIB).to_string())
                .unwrap_or_else(|| "null".to_string()),
        );
        out
    }

    /// キャッシュ設定を差し替える。
    pub fn with_cache(mut self, cache: CacheConfig) -> Self {
        self.cache = cache;
        self
    }

    pub fn new(
        port_str: &str,
        allow_hosts: Option<&str>,
        deny_hosts: Option<&str>,
        timeout: Duration,
    ) -> Result<Self, String> {
        let port: u16 = port_str
            .parse()
            .map_err(|e| format!("Invalid SERVER_PORT '{}': {}", port_str, e))?;
        let acl = AclConfig::new(allow_hosts, deny_hosts);
        let max_conns = default_max_conns();
        Ok(Self {
            port,
            bind_addrs: Vec::new(),
            ipv6: true,
            acl,
            timeout,
            keepalive: Duration::from_secs(15),
            pool_per_host: 64,
            pool_total: 256,
            park_idle: true,
            park_grace: Duration::from_millis(3),
            malloc_arenas: 8,
            tls_enabled: true,
            tls_verify: true,
            tls_ca_file: None,
            dns_ttl: Duration::from_secs(60),
            dns_negative: crate::dns::NEGATIVE,
            dns_warm: crate::dns::WARM,
            // 既定は `auto` = 直近 1 時間で最も使われた CONNECT の宛先を 60 秒に 1 回
            // (値の意味は `proxy-metrics` の `canary`。この層は文字列を運ぶだけ)
            canary: "auto".to_string(),
            canary_secs: Duration::from_secs(60),
            canary_ipv6: true,
            pac_direct: Vec::new(),
            blocklist_file: None,
            blocklist_url: None,
            blocklist_refresh: Duration::from_secs(86400),
            blocklist_exempt: Vec::new(),
            stats_persist: true,
            lite: false,
            // 既定 1,000 ms (`proxy_metrics::profile::DEFAULT_SAMPLE_MS` と同じ値。
            // この層は計測クレートに依存しないので数値で持つ)
            profile_sample_ms: 1000,
            connect_ports: PortSet::default(),
            allow_local: false,
            tunnel_idle: Duration::from_secs(300),
            max_conns,
            max_requests_per_conn: DEFAULT_MAX_REQUESTS_PER_CONN,
            max_threads: default_max_threads(max_conns),
            burst_percent: DEFAULT_BURST_PERCENT,
            burst_at: burst_threshold(max_conns, DEFAULT_BURST_PERCENT),
            cache: CacheConfig::default(),
            endpoints_readonly: false,
            allow_clients: ClientAcl::default(),
            max_conns_per_client: 0,
            sources: Sources::default(),
        })
    }
}

/// 一覧 1 つを書くのに使ってよいバイト数。
const LIST_CAP: usize = 1024;

/// 一覧の値 (`PROXY_PAC_DIRECT` など)。**[`LIST_CAP`] バイトで切り**、切ったときは
/// 最後の要素に `"+N more"` を入れる (JSON としては正しいまま)。
///
/// 一覧は書こうと思えばいくらでも長くできる (除外 1,000 件など) ので、`/config` が
/// 1 つの設定で埋まらないようにここで止める。全部を見たいときは `.env` そのものを読む
/// (`/config` の `env_file` にパスが出ている)。
fn list_value(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        let quoted = crate::json::quote(item);
        if out.len() + quoted.len() + 24 > LIST_CAP {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("\"+{} more\"]", items.len() - i));
            return out;
        }
        if i > 0 {
            out.push(',');
        }
        out.push_str(&quoted);
    }
    out.push(']');
    out
}

/// 上限を書いたときと同じ形に戻す (`auto` / `auto:85` / MiB 数)。
fn limit_value(limit: Limit) -> String {
    match limit {
        Limit::Auto {
            percent: DEFAULT_TARGET_PERCENT,
        } => "\"auto\"".to_string(),
        Limit::Auto { percent } => format!("\"auto:{}\"", percent),
        Limit::Fixed(bytes) => (bytes / MIB).to_string(),
    }
}

/// ディスク割当を書いたときと同じ形に戻す (分からなければ `null`、無制限は `0`)。
fn quota_value(quota: DiskQuota) -> String {
    match quota {
        DiskQuota::Unknown => "null".to_string(),
        DiskQuota::Unlimited => "0".to_string(),
        DiskQuota::Fixed(bytes) => (bytes / MIB).to_string(),
        DiskQuota::Auto => "\"auto\"".to_string(),
    }
}

/// キャッシュの設定は [`CacheConfig::from_env`] が読むので、ここでは**書いてあって読めた**
/// キーにだけ印を付ける (読み方は `crates/cachecfg` と同じ: 前後の空白を落として空でないこと、
/// 数値のものは `u64` として読めること、範囲のあるものはその中であること)。
fn mark_cache_sources(src: &mut Sources) {
    let raw = |k: &str| {
        envfile::var(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let num = |k: &str| raw(k).and_then(|v| v.parse::<u64>().ok());
    // 書いてあれば必ず効くもの (真偽・文字列・パス)
    for key in [
        "PROXY_CACHE_ENABLED",
        "PROXY_CACHE_RESERVE",
        "PROXY_CACHE_DIR",
        "PROXY_DISK_QUOTA_ROOT",
        "PROXY_DISK_PROBE",
        "PROXY_CACHE_ADMISSION",
    ] {
        if raw(key).is_some() {
            src.mark(key);
        }
    }
    // 数値として読めたときだけ効くもの
    for key in [
        "PROXY_CACHE_PROBE_SECS",
        "PROXY_MEM_KEEP_FREE_MB",
        "PROXY_DISK_KEEP_FREE_MB",
        "PROXY_CACHE_TTL_SECS",
        "PROXY_CACHE_HEURISTIC_MAX_SECS",
        "PROXY_CACHE_MAX_STALE_SECS",
        "PROXY_CACHE_GRACE_SECS",
        "PROXY_STALE_WAIT_SECS",
        "PROXY_CACHE_MAX_OBJECT_MB",
        "PROXY_MEM_CACHE_MAX_OBJECT_MB",
        "PROXY_NEGATIVE_TTL_SECS",
    ] {
        if num(key).is_some() {
            src.mark(key);
        }
    }
    // 範囲のあるもの
    for key in ["PROXY_MEM_TARGET_PERCENT", "PROXY_DISK_TARGET_PERCENT"] {
        if num(key).is_some_and(|p| (1..=100).contains(&p)) {
            src.mark(key);
        }
    }
    if num("PROXY_CACHE_HEURISTIC_PERCENT").is_some_and(|p| p <= 100) {
        src.mark("PROXY_CACHE_HEURISTIC_PERCENT");
    }
    if num("PROXY_DISK_MAX_ENTRIES").is_some_and(|v| v > 0) {
        src.mark("PROXY_DISK_MAX_ENTRIES");
    }
    if num("SERVER_MEMORY").is_some_and(|v| v > 0) {
        src.mark("SERVER_MEMORY");
    }
    // 上限は `auto` / `auto:85` / `85%` / MiB 数
    for key in ["PROXY_MEM_CACHE_MB", "PROXY_DISK_CACHE_MB"] {
        if raw(key).is_some_and(|v| Limit::parse(&v, DEFAULT_TARGET_PERCENT).is_some()) {
            src.mark(key);
        }
    }
    // ディスク割当は `PROXY_DISK_QUOTA_MB` が先で、**空でなければ**別名の `SERVER_DISK` は見ない
    let quota_key = if raw("PROXY_DISK_QUOTA_MB").is_some() {
        "PROXY_DISK_QUOTA_MB"
    } else {
        "SERVER_DISK"
    };
    if raw(quota_key).is_some_and(|v| DiskQuota::parse(&v).is_some()) {
        src.mark(quota_key);
    }
}

/// `PROXY_BIND` のカンマ区切りアドレス (`::`, `0.0.0.0`, `127.0.0.1`, `[::1]`)。空なら自動。
pub fn parse_bind_list(s: &str) -> Result<Vec<IpAddr>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| {
            item.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .map_err(|e| format!("Invalid PROXY_BIND entry '{}': {}", item, e))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_port() {
        let cfg = Config::new("9090", None, None, Duration::from_secs(10)).unwrap();
        assert_eq!(cfg.port, 9090);
        assert!(cfg.bind_addrs.is_empty());
        assert!(cfg.ipv6, "IPv6 on by default");
        assert_eq!(cfg.timeout, Duration::from_secs(10));
        assert_eq!(cfg.keepalive, Duration::from_secs(15));
        assert_eq!(cfg.pool_per_host, 64);
        assert_eq!(cfg.pool_total, 256);
        assert_eq!(cfg.malloc_arenas, 8);
        assert_eq!(cfg.max_conns, default_max_conns());
        assert_eq!(cfg.tunnel_idle, Duration::from_secs(300));
        assert!(cfg.connect_ports.is_empty() && !cfg.allow_local);
        // 接続元ごとの同時接続の上限は既定で無効 (T14.13)
        assert_eq!(cfg.max_conns_per_client, 0);
    }

    #[test]
    fn test_auto_max_conns() {
        // 1 接続 4 記述子 + 予備 64。記述子切れ (accept の EMFILE) より先に 503 で断れる値
        assert_eq!(auto_max_conns(256), 48);
        assert_eq!(auto_max_conns(1024), 240);
        assert_eq!(auto_max_conns(4096), 1008);
        // 記述子が余っていても頭打ち (fd 以外の資源の歯止め)
        assert_eq!(auto_max_conns(524_288), MAX_CONNS_CAP);
        assert_eq!(auto_max_conns(1_048_576), MAX_CONNS_CAP);
        assert_eq!(auto_max_conns(u64::MAX), MAX_CONNS_CAP);
        // 予備にも足りない極端な環境でも 0 (= 無制限) にはしない
        assert_eq!(auto_max_conns(64), 1);
        assert_eq!(auto_max_conns(0), 1);
        // 既定はこの計算そのもので、上限を超えない
        assert!((1..=MAX_CONNS_CAP).contains(&default_max_conns()));
    }

    #[test]
    fn test_default_max_threads() {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let want = (cores * THREADS_PER_CORE).clamp(MIN_MAX_THREADS, MAX_MAX_THREADS);
        // 接続数に余裕があればコア数から決まる
        assert_eq!(default_max_threads(0), want, "無制限のときもスレッドは有限");
        assert_eq!(default_max_threads(MAX_CONNS_CAP), want);
        // 接続数の上限の方が小さければそちらに従う (受けない接続のスレッドは要らない)
        assert_eq!(default_max_threads(16), 16);
        assert_eq!(default_max_threads(1), 1);
        // 何があっても無制限にはしない
        assert!(default_max_threads(0) > 0 && default_max_threads(0) <= MAX_MAX_THREADS);
        let cfg = Config::new("8080", None, None, Duration::from_secs(30)).unwrap();
        assert_eq!(cfg.max_threads, default_max_threads(cfg.max_conns));
    }

    #[test]
    fn test_bind_list() {
        let list = parse_bind_list(" ::, 0.0.0.0 ,[::1]").unwrap();
        assert_eq!(list.len(), 3);
        assert!(list[0].is_ipv6() && list[1].is_ipv4() && list[2].is_loopback());
        assert!(parse_bind_list("nope").is_err());
        assert!(parse_bind_list("").unwrap().is_empty());
    }

    #[test]
    fn test_cache_defaults() {
        let cfg = Config::new("8080", None, None, Duration::from_secs(30)).unwrap();
        assert!(cfg.cache.mem_limit.is_auto());
        assert!(cfg.cache.disk_limit.is_auto());
        assert_eq!(cfg.cache.mem_limit.target_percent(), Some(100));
        assert!(cfg.cache.enabled);
        assert_eq!(cfg.cache.reserve, proxy_cache::cache::Reserve::Staged);
    }

    #[test]
    fn sources_only_mark_values_that_took_effect() {
        let mut src = Sources::default();
        // 印が無ければ既定
        assert_eq!(src.get("PROXY_DNS_TTL_SECS"), Source::Default);
        // どこにも書かれていないキーは、印を付けても既定のまま
        // (「効いた」と呼べる値がそもそも無い)
        src.mark("PROXY_DNS_TTL_SECS");
        assert_eq!(src.get("PROXY_DNS_TTL_SECS"), Source::Default);
        // 実環境にあるキーは `env` として読める (配線の確認。`PATH` はどこでもある)
        assert_eq!(Source::of("PATH"), Source::Env);
        assert_eq!(Source::of("PROXY_NO_SUCH_KEY_T1415"), Source::Default);
        // 再読込では**当てた項目だけ**写す
        let mut fresh = Sources::default();
        fresh.0.push(("PROXY_DNS_TTL_SECS", Source::EnvFile));
        fresh.0.push(("SERVER_PORT", Source::EnvFile));
        let mut next = Sources::default();
        next.adopt(&fresh, "PROXY_DNS_TTL_SECS");
        assert_eq!(next.get("PROXY_DNS_TTL_SECS"), Source::EnvFile);
        assert_eq!(next.get("SERVER_PORT"), Source::Default, "写していない項目");
        // 消えたら既定に戻る
        next.adopt(&Sources::default(), "PROXY_DNS_TTL_SECS");
        assert_eq!(next.get("PROXY_DNS_TTL_SECS"), Source::Default);
        assert_eq!(
            [
                Source::Default.as_str(),
                Source::Env.as_str(),
                Source::EnvFile.as_str(),
                Source::Cli.as_str()
            ],
            ["default", "env", "env_file", "cli"]
        );
    }

    #[test]
    fn settings_show_the_effective_value_of_every_key() {
        let mut cfg =
            Config::new("9090", Some("*.example.com"), None, Duration::from_secs(5)).expect("port");
        cfg.lite = true;
        cfg.connect_ports = PortSet::parse("443,8080-8099");
        cfg.dns_warm = Duration::from_secs(0);
        let settings = cfg.settings();
        let find = |key: &str| {
            settings
                .iter()
                .find(|s| s.key == key)
                .unwrap_or_else(|| panic!("{} が無い", key))
                .value
                .clone()
        };
        assert_eq!(find("SERVER_PORT"), "9090");
        assert_eq!(find("PROXY_TIMEOUT_SECS"), "5");
        assert_eq!(find("PROXY_DNS_WARM_SECS"), "0");
        assert_eq!(find("PROXY_ALLOW_HOSTS"), "[\"*.example.com\"]");
        assert_eq!(find("PROXY_CONNECT_PORTS"), "\"443,8080-8099\"");
        assert_eq!(find("PROXY_PROFILE"), "\"lite\"");
        assert_eq!(find("PROXY_TLS_CA_FILE"), "null");
        assert_eq!(find("PROXY_MEM_CACHE_MB"), "\"auto\"");
        assert_eq!(find("SERVER_DISK"), find("PROXY_DISK_QUOTA_MB"), "別名");
        // 出どころは既定 (このテストは環境変数を触っていない)
        assert!(
            settings
                .iter()
                .all(|s| s.source == Source::Default || Source::of(s.key) != Source::Default),
            "書かれていないキーに出どころが付いている"
        );
        // 全部のキーが `PROXY_` か `SERVER_` で、重複しない (別名の 2 行を除く)
        assert!(
            settings
                .iter()
                .all(|s| s.key.starts_with("PROXY_") || s.key.starts_with("SERVER_"))
        );
        let mut keys: Vec<&str> = settings.iter().map(|s| s.key).collect();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), total, "同じキーが 2 度出ている");
        assert!(total >= 50, "{} 件しか出ていない", total);
    }

    #[test]
    fn long_lists_are_cut_with_a_marker() {
        let many: Vec<String> = (0..100)
            .map(|i| format!("host-{}.example.net", i))
            .collect();
        let out = list_value(&many);
        assert!(out.len() <= LIST_CAP, "{} B", out.len());
        assert!(out.ends_with("more\"]"), "{}", out);
        assert!(out.starts_with("[\"host-0.example.net\","), "{}", out);
        // 短い一覧はそのまま
        assert_eq!(
            list_value(&["a".to_string(), "b".to_string()]),
            "[\"a\",\"b\"]"
        );
        assert_eq!(list_value(&[]), "[]");
    }

    #[test]
    fn test_invalid_port() {
        assert!(Config::new("invalid", None, None, Duration::from_secs(30)).is_err());
        assert!(Config::new("99999", None, None, Duration::from_secs(30)).is_err());
    }

    /// 山の写真の閾 (T14.6): 上限 × 割合。撮らない設定は [`usize::MAX`] (比較 1 回のまま)。
    #[test]
    fn the_burst_threshold_is_a_share_of_the_connection_limit() {
        assert_eq!(burst_threshold(8, 50), 4);
        assert_eq!(burst_threshold(240, 50), 120);
        assert_eq!(burst_threshold(4096, 90), 3686);
        // 100% は上限そのもの (T13.2 の「上限の外の枠 4 本」は含めない)
        assert_eq!(burst_threshold(240, 100), 240);
        assert_eq!(
            burst_threshold(240, 150),
            240,
            "100 を超える割合は 100 に倒す"
        );
        // 小さすぎる上限でも 0 にはしない (0 だと 1 本目から撮ってしまう)
        assert_eq!(burst_threshold(1, 50), 1);
        // 撮らない設定
        assert_eq!(burst_threshold(240, 0), usize::MAX);
        assert_eq!(burst_threshold(0, 50), usize::MAX, "無制限には閾が無い");

        let mut cfg = Config::new("8080", None, None, Duration::from_secs(30)).unwrap();
        assert_eq!(cfg.burst_percent, DEFAULT_BURST_PERCENT);
        assert_eq!(cfg.burst_at, burst_threshold(cfg.max_conns, 50));
        cfg.max_conns = 8;
        cfg.refresh_burst_at();
        assert_eq!(cfg.burst_at, 4);
        // `--lite` は個票を記録しないので閾そのものを置かない
        cfg.lite = true;
        cfg.refresh_burst_at();
        assert_eq!(cfg.burst_at, usize::MAX);
    }
}
