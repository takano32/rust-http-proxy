# rust-http-proxy

依存クレートゼロ (Rust 標準ライブラリ `std` だけ) の、**認証不要**な HTTP/HTTPS プロキシです。
バイナリを 1 つ置いて起動するだけで使えます。

**目次**

- [クイックスタート](#クイックスタート)
- [性能](#性能)
- [特徴](#特徴)
- [コマンドライン引数](#コマンドライン引数)
- [プロファイル](#プロファイル)
- [環境変数](#環境変数)
- [ログ](#ログ)
- [キャッシュ](#キャッシュ)
- [クレート構成](#クレート構成)
- [ビルド・テスト](#ビルドテスト)
- [配布 (Docker / Release バイナリ)](#配布-docker--release-バイナリ)
- [起動方法](#起動方法)
- [動作確認 (curl)](#動作確認-curl)
- [運用 (置いた先での手順と、そのあと見るもの)](#運用-置いた先での手順とそのあと見るもの)
  - [0. リリース (版をそろえてタグを打つ)](#0-リリース-版をそろえてタグを打つ)
  - [0.5 置く (release のバイナリを使うとき)](#05-置く-release-のバイナリを使うとき)
  - [1. 置く前 (--check)](#1-置く前---check)
  - [2. 起動直後 (0〜5 分)](#2-起動直後-05-分)
  - [3. 1 時間後](#3-1-時間後)
  - [3.5 ふだんの見張り (watch.yml)](#35-ふだんの見張り-watchyml)
  - [4. 24 時間後](#4-24-時間後)
  - [5. 何かおかしいとき](#5-何かおかしいとき)
  - [6. 止めるとき](#6-止めるとき)
  - [7. そのあと](#7-そのあと)
- 詳しい説明: [`docs/features.md`](docs/features.md) (特徴)・[`docs/perf.md`](docs/perf.md) (性能の測り方とデプロイ先の数字)

## クイックスタート

```bash
cargo build --release                    # ビルド (約 3.3 MB のバイナリが 1 つ。配布用の dist は約 2.1 MB)
./target/release/rust-http-proxy --lite -p 8080   # 起動 (最速の素通し設定)
curl -x localhost:8080 http://example.com/        # 動作確認
```

ブラウザに設定するなら、自動設定スクリプトの URL `http://<ホスト>:8080/proxy.pac` を
プロキシ設定の「自動プロキシ設定 URL」に入れるだけです (このプロキシが落ちていれば DIRECT に落ちます)。

`cargo install --git https://github.com/takano32/rust-http-proxy` でも入ります
(ビルドはメモリ約 90 MB で通ります。下の「ビルド・テスト」を参照)。

キャッシュ・ダッシュボード・統計の保存と時系列・接続ごとの個票まで使うなら `--lite` を外します
(`--lite` でも `/status` `/hosts` `/clients` `/errors` の集計はメモリに積みます。下の「プロファイル」)。

```bash
SERVER_PORT=8080 ./target/release/rust-http-proxy
# http://localhost:8080/dashboard  … グラフとホスト別統計
# http://localhost:8080/status     … JSON、/metrics … Prometheus 形式
```

## 性能

8 コア / 6.6 GiB (aarch64、big.LITTLE)、loopback で測った値です。

**どう測ったか**: `scripts/cpu-per-request.sh` が全部やります。プロキシを big (cpu4-7)・ベンチを LITTLE (cpu0-3) に
`taskset` で固定し、`--lite` + `PROXY_LOG_LEVEL=warn`、応答本文 1 KiB、10 秒 × 3 回以上の中央値
(「CONNECT トンネル 1 本」だけプロキシ cpu4-5 / ベンチ cpu6-7)。**この機械は固定の仕方で結果が最大 2 倍動く**ので、
比べるときは必ず同じ固定の仕方で測ってください。数字は 2026-09-08 の実測で、`TODO.md` の §2 と同じものです
(出典: T.10.11 / T.11.4)。括弧の **(NN%)** は**プロキシが使った 1 コアぶんの割合** (割り当ては 4 コア = 400%)。
これが 100% に遠い行は**プロキシが律速していない** = その req/s はプロキシの上限ではありません。

| 項目 | `release` (`cargo build --release`) | `dist` (配布用) |
|---|---|---|
| forward, 8 並列 | **38,619 req/s, p50 0.177 ms** (157%) | **39,368 req/s, p50 0.175 ms** (154%) |
| forward, 8 並列 (キャッシュ HIT) | 76,964 req/s, p50 0.085 ms (169%) | **77,501 req/s, p50 0.085 ms** (165%) |
| forward, 64 並列 | 31,819 req/s, p50 1.79 ms, p99 6.5 ms (184%) | 31,443 req/s, p50 1.75 ms, p99 7.3 ms (181%) |
| 1 接続 1 要求 (keep-alive 無し) | 14,105 req/s, p50 0.308 ms (145%) | 13,995 req/s, p50 0.307 ms (141%) |
| CONNECT 確立 (**TIME_WAIT 律速** → 下) | 7,951 tunnels/s, p50 0.186 ms (111%) | 7,297 /s, p50 0.186 ms (100%) |
| CONNECT トンネル 1 本 (**ベンチ律速** → 下の注) | **3,960 MiB/s** (73%) | 3,950 MiB/s (72%) |
| CPU/要求 (forward / HIT / 64 並列) | **41.4 / 22.0 / 58.1 us** | **39.6 / 21.4 / 58.0 us** |
| CPU (1 接続 1 要求 / CONNECT 確立 / トンネル) | 103.3 us/接続 / 140.0 us/本 / 185.5 us/MiB | 101.3 / 138.5 / 183.3 |
| 1 要求あたりのシステムコール | **5.02** (`recvfrom` 3 + `sendto` 2。T.26.0 で GET / HEAD の覗きを省いて 4.03、T.29.2 で全要求に戻したので今は 5 前後 (測り直していない)) | 5.02 |
| 同時 5,000 トンネル (アイドル) | **68 スレッド (山 260) / RSS 29.9 MB** | 68 (260) / 28.7 MB |
| 暇な keep-alive 接続 2,000 本 | **25 スレッド / RSS 25.2 MB** | 28 スレッド / 25.4 MB |
| 起動直後のスレッド | `--lite` で 4 本、既定で 7 本 (2026-09-08。今は 9 本 → 「起動直後のスレッド数」) | 同左 |
| バイナリ | 1,381,872 B | **1,054,184 B** |

`release` は `cargo build --release` で作るもの、`dist` は配布用 (fat LTO、`cargo build --profile dist`) です。
違いは下の「プロファイルの設定」を見てください。

**ベンチ自身の上限** (`--only direct` = プロキシを通さないオリジン直結) は LITTLE に固定して **55,112 req/s**、
固定しなければ 263,821 req/s です。forward・HIT・64 並列・1 接続 1 要求はいずれもベンチが先に頭打ちになるので、
表の req/s はプロキシの上限ではありません。**CONNECT 確立はカーネルの TIME_WAIT が律速**で
(`tcp_max_tw_buckets = 32768` を 10 秒で使い切る)、同じ設定でも tunnels/s は ±25% 暴れます
(比べてよいのは CPU/本。こちらは ±4%)。

着手前 (2026-09-07 の Python ベンチ) は forward 8 並列 188 req/s / p50 42.0 ms、CONNECT 確立 3,506 tunnels/s、
バイナリ 857 KB でした (forward は **205 倍**)。当時の値と、コアを固定せずに測っていた頃の値は
`TODO.md` の §2 と付録 A に残してあります。

**置いた先での測り方** (`PROXY_SELF_BENCH=on`)、**デプロイ先 (tokyo) の数字と判定表 (`--criteria phaseNN`) の定義**、
手元での再現 (`scripts/deployed-like.sh`・`bench --only replay`)、トンネルの MiB/s の注は
[`docs/perf.md`](docs/perf.md) に分けました (T.29.32)。

## 特徴

- **認証不要（No Auth）**: 事前設定なしで誰でも即座に利用可能
- **依存クレートゼロ**: 外部クレート依存がないため、ビルド負荷が最小限で高速にビルド可能
- **超軽量バイナリ**: `cargo build --release` で約 3.3 MB、配布用の `dist` (fat LTO) で約 2.1 MB (x86_64、v0.29.35)
- **HTTP / HTTPS (CONNECT トンネリング) 対応**
- **同時ミスの合流 (collapsed forwarding)**: 同じ URL を同時に頼まれても、オリジンへ行くのは最初の 1 本だけ
- **keep-alive と接続プール**: クライアント側は HTTP/1.1 の持続接続、オリジン側はつないだ接続を使い回す
- **Range / HEAD**: キャッシュ済みの完全な表現から `206` (単一範囲・`If-Range`・範囲外は `416`) と `HEAD` を切り出す
- **HTTPS のオリジンもキャッシュ (CA 不要)**: `GET /https/example.com/path` か `GET https://example.com/path` で頼めばプロキシが TLS を張って保存する
- **IPv4 / IPv6 デュアルスタック**: `[::]` と `0.0.0.0` で待ち受け、オリジンへは IPv6 優先の Happy Eyeballs (IPv6 が通らない所では IPv4 first に切り替える)
- **RFC 7230 / RFC 9110 準拠**: hop-by-hop の除去・`Via`・ループの検知 (`508`)・枠組みの検査・`Max-Forwards` など
- **ACL / ホストフィルタリング**: 接続元の許可・拒否、宛先のホストとポートの許可・拒否、私設アドレス宛ては既定で断る
- **2 段キャッシュ (メモリ + ディスク) — 固まらない限界まで使う**: メモリとディスクの空きを測り、余白を残して残りをすべてキャッシュに充てる
- **詳細なアクセスログ**: 1 要求 1 行 (キャッシュの判定・時間・バイト数)、同じ失敗の WARN は理由ごとに 1 分 1 行
- **`.env` の自動再読込**: `$HOME/.env` の保存を inotify で検知し、ACL・タイムアウト・ログレベルを再起動なしで反映
- **DNS キャッシュ**: 名前解決の結果を 60 秒保持し、CONNECT ごとの解決をなくす
- **入場制御 & ネガティブキャッシュ**: 層が埋まったら 2 回目に見た URL だけ保存
- **ドメインのブロックリスト**: hosts 形式のファイルや URL (1 日 1 回自動更新) から読み、広告・トラッカーを CONNECT の段階で 403 にする
- **統計と履歴の永続化**: 固定サイズ (8 MiB) の状態ファイルに、履歴 3 解像度 (5 秒・1 分・1 時間) とホスト別・接続元別の統計を残す
- **日次の snapshot**: 1 日 1 回 (UTC 0 時)、その時点の `/snapshot` をまるごと残す (既定 30 日ぶん、起動と停止の 1 枚も)
- **個票の永続化**: 固定サイズ (4 MiB) の個票のファイルに `/recent` (閉じた接続) を残し、再起動しても読める
- **状態ファイルの版 (`.rrd`)**: 先頭 8 バイトが版の印で、いまは `SHPRRD03` (版 3)
- **接続元別の統計**: 接続元 IP ごとの要求数・転送量・拒否数・応答時間を `/status` `/metrics` とダッシュボードに出す
- **`/proxy.pac`**: ブラウザの自動設定スクリプト
- **ヘルスチェック & メトリクス & 操作**: `/healthz`・`/status`・`/metrics` (Prometheus)・ダッシュボード・`/purge`・`/blocklist` など (一覧は「動作確認 (curl)」)
- **タイムアウト制御**: 確立 (CONNECT は既定 3 秒)・ヘッダーの締め切り (408)・無通信の締め切り・転送中の最小の速さ (T.29.34)

それぞれの詳しい説明 (数字・既定・試した経緯) は [`docs/features.md`](docs/features.md) にあります (T.29.32)。

## コマンドライン引数

環境変数を書かなくても、よく使う設定は引数で渡せます (優先順位は **引数 > `$HOME/.env` > 環境変数**)。

```
  -p, --port <PORT>    待ち受けポート        (SERVER_PORT)
      --bind <ADDRS>   待ち受けアドレス      (PROXY_BIND、カンマ区切り)
      --no-cache       キャッシュを止める    (PROXY_CACHE_ENABLED=off)
      --quiet          警告以上だけ出す      (PROXY_LOG_LEVEL=warn)
      --lite           最速の素通しプロファイル (PROXY_PROFILE=lite。止めるものと残るものは下の「プロファイル」)
      --check          起動せずに環境と効く設定を出して終了 (下記)
  -h, --help           使い方を出して終了 (終了コード 0)
  -V, --version        版を出して終了
```

`--port=3128` の形式も使えます。知らない引数は使い方を出して終了コード 2 になります。

`--check` は**起動せずに**「この環境で何が読めるか」(`capabilities` の 7 項目) と
「この設定で起動したら何が効くか」(`/config` と同じ全 `PROXY_*` / `SERVER_*` と出どころ) を印字して終わります。
Pterodactyl のように触れないコンテナで、**起動前の確認**と**統計の `null` の理由の切り分け**に使えます
(他の引数も一緒に効くので `--check -p 3128 --lite` のように「その設定なら何が効くか」も見られます)。
終了コードは `capabilities` の 7 項目 (`resolver_ms` と `cgroup_cpu_path` を除く) が全部読めれば **0**、
1 つでも読めなければ **1** です (名前解決を外すのは、リゾルバが遅い環境でもプロキシとしては動く —
そしてそれ自体が測りたい数字 — ため。`cgroup_cpu_path` は真偽ではなく「どの階層の `cpu.stat` を読むか」を
見せるだけの行です。T.15.0 (6))。
`.env` に読めない値や知らない名前があれば、設定の一覧の後ろに `not readable` と `unknown names` の段が出ます
(正しい設定では出ません。終了コードには入れません。T.28.6)。

```
$ rust-http-proxy --check
rust-http-proxy 0.29.35+b2b26c1 --check
build profile: release
settings file: /home/container/.env (3 variables)

capabilities (what this environment lets the proxy read):
  [ok] proc_syscall     /proc/self/task/<tid>/syscall (per-thread state)
  [ok] tcp_info         getsockopt(SOL_TCP, TCP_INFO) (kernel RTT and retransmits)
  [ok] cgroup_cpu       cgroup cpu.stat (CPU throttling)
  [ok] cgroup_pressure  cgroup cpu.pressure (PSI: waiting for the CPU)
  [ok] ipv6_route       a default route in /proc/net/ipv6_route
  [ok] home_writable    $HOME is writable (statistics file, blocklist)
  [ok] proc_schedstat   /proc/self/task/<tid>/schedstat (time runnable but not running)
  [ok] resolver_ms      9 ms for one lookup (not part of the exit code)
  [ok] cgroup_cpu_path  /sys/fs/cgroup/cpu.stat (where nr_throttled is read)

settings (source, name, effective value):
  default   SERVER_PORT                    8080
  env_file  PROXY_DNS_TTL_SECS             30
  ...

not readable (ignored; the default is in effect):
  PROXY_KEEPALIVE_SECS           "abc" (expected a whole number)

unknown names in the settings file (not read by this proxy):
  PROXY_TIMOUT_SECS              did you mean PROXY_TIMEOUT_SECS?

check: ok (everything this proxy reads is readable)
```

`-V` が出す版は `0.29.35+b2b26c1` のように **`Cargo.toml` の版 + ビルドしたときの git の短いハッシュ**です
(作業ツリーに未コミットの変更があれば `0.29.35+b2b26c1-dirty`)。`git` や `.git` の無いところでビルドすると
`0.29.35+unknown` になります (ビルドは通ります)。同じ文字列を**起動ログの `rust-http-proxy ... listening on ...` の行**と
**`/status` のトップレベルの `version`** にも出すので、デプロイ先でどのコミットが動いているかが分かります。

**ビルドの種類** (cargo のプロファイル名) も出します (T.28.42): `-V` は版のあとに `(release)` のように添え
(`rust-http-proxy 0.29.35+b2b26c1 (release)`)、`--check` は 2 行目に `build profile: release`、`/status` は
**末尾の `build`** (`"build":"release"`。既存の鍵の順は変えていません) です。値は `cargo build --release` なら `release`、
`cargo build --profile dist` (GitHub の release の配布物) なら `dist`、`cargo build` / `cargo test` なら `debug` で、
どの最適化で作ったバイナリかが外から見えます。cargo がビルドスクリプトに渡す `PROFILE` は `dist` でも `release` なので、
`crates/server/build.rs` は `OUT_DIR` の道のり (`target/[<三つ組>/]<プロファイル>/build/…/out`) から読みます
(読めない形なら `PROFILE`、それも無ければ `unknown`)。

## プロファイル

`--lite` (= `PROXY_PROFILE=lite`) は「認証なし・手軽・最速」の素通し設定です。**「統計を取らない」設定ではありません**
(T.28.44): 要求ごとに数えるメモリの中の集計は既定と同じに積み、止めるのは次のものです。

| `--lite` で止めるもの | 明示すれば戻るか | 口の見え方 |
|---|---|---|
| キャッシュ | `PROXY_CACHE_ENABLED=on` で戻る | `/status` の `cache` は無効の形、`/lookup` は見つからない |
| ブロックリストの取得 (ファイルも URL も) | **戻らない** (`PROXY_BLOCKLIST_FILE` / `PROXY_BLOCKLIST_URL` を書いても読まない) | `/blocklist` は 0 件 (手動の上書きは効く) |
| 統計の保存と履歴スレッド (`PROXY_STATS_PERSIST` が既定で `off`) | `PROXY_STATS_PERSIST=on` で戻る | `/history` `/slo` `/daily` `/snapshots` は空、canary・状態ファイル・日次の雪像も無し |
| 接続ごとの個票 (接続の枠を作らない) | 戻らない | `/connections` `/recent` `/trace` `/bursts` は空 (`"lite":true`)、`/clients` の `agent` / `agents` も空、SNI も覗かない |
| 段階の時計と `/profile` のスレッド | 戻らない | `/profile` は `{"profile":"off"}` |
| `/dashboard` | 戻らない | 1 行の `lite mode: ...` |
| 記録のリングの先触り (T.17.8) | 戻らない | `/status` の `memory.rings_touched` が `false` |
| ログの高さ (`info` → `warn`) | `PROXY_LOG_LEVEL` で戻る | `/log` に残るのも `warn` 以上 |

**`--lite` でも残るもの**: `/status` の数 (`total_requests`・`errors`・`errors_by_cause`・転送量・断った数…)、
**ホスト別の `/hosts` と接続元別の `/clients`** (要求数・バイト数・エラー・応答時間の分布)、`/errors` の個票、
`/events` (履歴スレッドが拾う種類を除く)、`/readers`、`/metrics`、`/dns`、`/healthz`、`/config`。どれもメモリだけで、
再起動で消えます (保存は `PROXY_STATS_PERSIST=on` のときだけ)。
`/dashboard` は 1 行の `lite mode: ...` を返し、起動ログの 1 行目に `profile: lite` が出ます
(**「調査」ページ `/inspect` と「端末から測る」ページ `/probe.html` は `--lite` でも 200 で開けます** —
中の表と図は「記録していません」と出ます)。
起動直後のスレッドは 4 本 (待ち受け + `env-reload` + `shutdown` + `idle-watch`) だけです
(`PROXY_PARK_IDLE=off` なら 3 本)。

```bash
./rust-http-proxy --lite -p 8080
```

## 環境変数

まず**探すための一覧表**です (並びは `/config` と `--check` が出す順)。「ひとこと」は 1 行の要約なので、
条件や根拠まで要るときは下の「[変数ごとの説明](#変数ごとの説明)」を見てください (分類ごとに、変数 1 つ = 箇条書き 1 つ)。

| 環境変数名 | デフォルト値 | ひとこと |
|---|---|---|
| `SERVER_PORT` | `8080` | 待ち受けポート (Pterodactyl が自動設定) |
| `PROXY_BIND` | 自動 (`::` + `0.0.0.0`) | 待ち受けアドレスのカンマ区切り。未設定でデュアルスタック |
| `PROXY_IPV6` | `on` | IPv6 を使うか (待ち受けと AAAA での接続) |
| `PROXY_LISTEN_BACKLOG` | `0` (= `min(1024, somaxconn)`) | 待ち受けの受け入れ待ち行列の長さ (`listen(2)`) |
| `PROXY_TIMEOUT_SECS` | `30` | 接続と無通信の締め切り (秒。1 バイトでも動けば待ち直す)。`0` で無期限 |
| `PROXY_MIN_TRANSFER_BPS` | `1024` | 転送中の最小の速さ (バイト/秒)。下回り続けたら切る。`0` で切らない |
| `PROXY_MIN_TRANSFER_WINDOW_SECS` | `30` | 最小の速さを判定する窓 (相手を待った秒) |
| `PROXY_CONNECT_TIMEOUT_SECS` | `3` | オリジンへの TCP の確立だけの締め切り (秒。`CONNECT` と forward) |
| `PROXY_HEADER_TIMEOUT_SECS` | `10` | 要求行とヘッダーを読み終えるまでの総時間の締め切り (秒)。越えたら `408`。`0` で無し |
| `PROXY_HE_STAGGER_MS` | `250` | Happy Eyeballs で次の候補を試し始めるまでの間隔 (ms、10〜2000) |
| `PROXY_KEEPALIVE_SECS` | `15` | クライアント接続を次の要求まで待つ時間 (秒) |
| `PROXY_TUNNEL_IDLE_SECS` | `300` | CONNECT トンネルのアイドル打ち切り (秒) |
| `PROXY_MAX_CONNS` | `auto` | 同時に受ける接続数の上限 |
| `PROXY_MAX_CONNS_PER_CLIENT` | `0` (無効) | 1 つの接続元から同時に受ける接続数の上限 |
| `PROXY_MAX_THREADS` | `auto` | 同時に生きていてよい接続スレッドの上限 |
| `PROXY_ORIGIN_POOL` | `64` | オリジンへのアイドル接続をホストごとに保つ本数 |
| `PROXY_ORIGIN_POOL_TOTAL` | `256` | アイドル接続の全ホスト合計の上限 |
| `PROXY_PARK_IDLE` | `on` | 暇な接続をスレッドから外して監視スレッドに預ける |
| `PROXY_PARK_GRACE_MS` | `3` | 預ける前に同じスレッドで待ってみる時間 (ms) |
| `PROXY_MALLOC_ARENAS` | `8` | malloc のアリーナ数の上限 |
| `PROXY_DNS_TTL_SECS` | `60` | 名前解決の結果を保持する秒数 |
| `PROXY_DNS_NEGATIVE_SECS` | `60` | 名前解決の失敗を覚えておく秒数 (`0` で覚えない) |
| `PROXY_DNS_WARM_SECS` | `0` | keep-warm: よく使う名前を裏で引き直し続ける窓 (秒。`0` で止める = 既定。T.21.3 までの既定は `3600`) |
| `PROXY_DNS_STALE_SECS` | `21600` | serve-stale: 期限切れでもこの秒数未満の答えは待たずに使い、裏で引き直す (`0` で止める) |
| `PROXY_CANARY` | `auto` | canary: 要求が無い時間帯も待ちを測る宛先 |
| `PROXY_CANARY_SECS` | `60` | canary の周期 (秒、最小 1) |
| `PROXY_CANARY_IPV6` | `on` | canary の IPv6 側 (AAAA へ 1 本だけ繋いでみる) |
| `PROXY_ALLOW_HOSTS` | なし (全許可) | 接続を許すあて先ホストのカンマ区切り |
| `PROXY_DENY_HOSTS` | なし | 接続を拒むあて先ホストのカンマ区切り |
| `PROXY_CONNECT_PORTS` | なし (制限なし) | `CONNECT` を許すあて先ポート |
| `PROXY_ALLOW_LOCAL` | `off` | ループバック・リンクローカル・私設アドレス宛てを許すか (`private` で私設アドレスだけ) |
| `PROXY_ENDPOINTS_READONLY` | `off` | 内部エンドポイントの書き換える口だけを断る |
| `PROXY_ALLOW_CLIENTS` | なし (全許可) | 受ける接続元のカンマ区切り (CIDR 可) |
| `PROXY_TRACE_CLIENT` | なし (追跡しない) | 1 つの接続元だけを追って `/trace` に残す |
| `PROXY_PEEK_SNI` | `on` | CONNECT の先頭から SNI を読んで個票に残す |
| `PROXY_BURST_PERCENT` | `50` | 同時接続がこの割合を越えた瞬間に写真を 1 枚撮る |
| `PROXY_BLOCKLIST_FILE` | なし | ドメインのブロックリスト (hosts 形式も可) |
| `PROXY_BLOCKLIST_URL` | なし | ブロックリストを取りに行く URL |
| `PROXY_BLOCKLIST_REFRESH_SECS` | `86400` | URL を取り直す間隔 (秒、最小 60) |
| `PROXY_BLOCKLIST_EXEMPT` | なし | ブロックリストの対象外にするホスト |
| `PROXY_PAC_DIRECT` | なし | `/proxy.pac` で DIRECT にするホスト |
| `PROXY_TLS` | `on` | HTTPS のオリジンから取得するか |
| `PROXY_TLS_VERIFY` | `on` | オリジンの証明書を検証するか |
| `PROXY_TLS_CA_FILE` | なし (システムの CA) | 追加で信頼する CA 証明書 (PEM) |
| `PROXY_TCP_KEEPALIVE` | `on` (= `on:60:10:3`) | 消えたクライアントを見つける TCP keepalive |
| `PROXY_RECORDS` | `on` | 個票に何を残すかの旗 |
| `PROXY_STATS_PERSIST` | `on` | 統計・履歴・個票をファイルに残して読み直すか |
| `PROXY_SNAPSHOT_DAYS` | `30` | 日次の snapshot を残す日数 |
| `PROXY_SLO` | `connect_p50_ms=5,connect_p95_ms=50,error_rate=0.005,dns_miss_per_connect=0.2` | SLO の 4 つの閾 |
| `PROXY_PROFILE` | なし | `lite` で最速の素通しプロファイル |
| `PROXY_PROFILE_SAMPLE_MS` | `1000` | `/profile` のスレッドの標本を取る間隔 (ms) |
| `PROXY_SELF_BENCH` | `off` | 起動直後に loopback だけで 3 秒の自己ベンチを回す |
| `PROXY_LOG_LEVEL` | `info` | ログレベル |
| `PROXY_CACHE_ENABLED` | `true` | キャッシュを無効化する旗 |
| `PROXY_CACHE_DIR` | 自動選択 (後述) | ディスクキャッシュ格納先 |
| `PROXY_MEM_CACHE_MB` | `auto` | メモリキャッシュ上限 |
| `PROXY_DISK_CACHE_MB` | `auto` | ディスクキャッシュ上限 |
| `PROXY_MEM_TARGET_PERCENT` | `100` | `auto` 時にメモリ使用率を頭打ちにする割合 |
| `PROXY_DISK_TARGET_PERCENT` | `100` | `auto` 時にディスク使用率を頭打ちにする割合 |
| `PROXY_MEM_KEEP_FREE_MB` | `0` | 手動で必ず空けておくメモリ量 (MiB) |
| `PROXY_DISK_KEEP_FREE_MB` | `0` | 手動で必ず空けておくディスク量 (MiB) |
| `PROXY_CACHE_RESERVE` | `staged` | 先行確保 (バラスト) の仕方 |
| `PROXY_CACHE_PROBE_SECS` | `5` | 使用量を測り直して予算を更新する間隔 (秒) |
| `PROXY_CACHE_TTL_SECS` | `300` | 鮮度の手がかりが無いときの TTL (秒) |
| `PROXY_CACHE_HEURISTIC_PERCENT` | `10` | `Last-Modified` からの経過時間のうち TTL にする割合 |
| `PROXY_CACHE_HEURISTIC_MAX_SECS` | `604800` | 経験則 TTL の上限 (秒) |
| `PROXY_CACHE_MAX_STALE_SECS` | `2592000` | 再検証できるエントリを保持する最長時間 (秒) |
| `PROXY_CACHE_GRACE_SECS` | `60` | 期限切れ後すぐ返して裏で再検証する秒数 |
| `PROXY_STALE_WAIT_SECS` | `5` | 期限切れのときオリジンを待つ上限 (秒) |
| `PROXY_CACHE_MAX_OBJECT_MB` | `4096` | ディスク層に置く 1 オブジェクトの最大 (MiB) |
| `PROXY_MEM_CACHE_MAX_OBJECT_MB` | `32` | メモリ層に置く 1 オブジェクトの最大 (MiB) |
| `PROXY_DISK_MAX_ENTRIES` | `2000000` | ディスク層の索引に保持するエントリ数の上限 |
| `PROXY_CACHE_ADMISSION` | `on` | 入場制御 (1 回目は置かない) |
| `PROXY_NEGATIVE_TTL_SECS` | `60` | 否定応答 (404 / 410 など) の TTL 上限 (秒) |
| `PROXY_DISK_QUOTA_MB` (別名 `SERVER_DISK`) | なし | コンテナのディスク割当 (別名 `SERVER_DISK`) |
| `PROXY_DISK_QUOTA_ROOT` | `$HOME` | ディスク割当が適用されるディレクトリ |
| `PROXY_DISK_PROBE` | `on` | 割当が分からないとき Wings の挙動から探るか |
| `SERVER_MEMORY` | なし | コンテナのメモリ割当 (MB)。Pterodactyl が自動設定 |

環境変数を設定できない環境 (Pterodactyl で egg 変数を追加する権限が無い等) では、`$HOME/.env` に `KEY=VALUE` を
1 行ずつ書けば同じ効果になります (`#` はコメント、ファイルの値が実際の環境変数より優先)。Pterodactyl ならファイルマネージャで
`/home/container/.env` を置くだけです。

`.env` は起動後も監視していて、保存すると再起動なしで読み直します (`$HOME` を inotify で監視、使えないファイルシステムでは
30 秒ごとの mtime 確認)。即時に反映されるのは `PROXY_ALLOW_HOSTS` / `PROXY_DENY_HOSTS` / `PROXY_TIMEOUT_SECS` /
`PROXY_CONNECT_TIMEOUT_SECS` / `PROXY_HEADER_TIMEOUT_SECS` /
`PROXY_KEEPALIVE_SECS` / `PROXY_LOG_LEVEL` / `PROXY_MAX_CONNS` / `PROXY_MAX_THREADS` /
`PROXY_ALLOW_CLIENTS` / `PROXY_ENDPOINTS_READONLY` / `PROXY_MAX_CONNS_PER_CLIENT` などで、
既存の keep-alive 接続には次の接続から効きます (どの値を当てたかは `/status` の `settings.applied` に出ます)。ポート・bind・
待ち受けの backlog (`PROXY_LISTEN_BACKLOG`)・TLS・
オリジンプール・キャッシュ予算 (`SERVER_MEMORY` / `SERVER_DISK` / `PROXY_CACHE_*`) は起動時に固定なので、変更を検知すると
`/status` の `settings.restart_required` と `/dashboard` の帯に「再起動が必要」と出ます。
**読めない値** (`PROXY_KEEPALIVE_SECS=abc` のように数の欄に数でないもの、範囲の外の値) は、起動時も再読込でも
WARN を 1 行出して**その項目だけ既定値**で動きます (前の値を維持するのではありません)。書いた名前と値は
`/status` の `settings.invalid` (`[{"key":"PROXY_KEEPALIVE_SECS","value":"abc"}]`) に出ます。
真偽の鍵 (`PROXY_ALLOW_LOCAL` など) は `0` / `false` / `off` / `no` が off で、それ以外は**今までどおり on** ですが、
`1` / `true` / `on` / `yes` と空の外 (`maybe` など) は WARN 1 行と `settings.invalid` に出ます (`PROXY_ALLOW_LOCAL` は
`private` も読める値です。T.28.41)。`PROXY_PROFILE` の `lite` 以外と、
`PROXY_SLO` の読めない項目 (読めた項目だけ当てる) も同じです (T.28.31)。
**`.env` に書いてあるが、このプロキシが読まない `PROXY_*` / `SERVER_*` の名前** (`PROXY_TIMOUT_SECS` のような打ち間違い) も
WARN を 1 行出し (綴りの近い名前があれば `did you mean PROXY_TIMEOUT_SECS?` を添える)、`settings.unknown`
(`[{"key":"PROXY_TIMOUT_SECS","suggest":"PROXY_TIMEOUT_SECS"}]`。近い名前が無ければ `suggest` は `null`) に出ます。
どちらも**いまの `.env` のもの**で、直して保存すれば再読込で `[]` に戻ります (`/config` の `reload` にも同じものが出ます。
知らない名前は `.env` だけを見ます — 実際の環境変数はパネルが別の `SERVER_*` を渡すため)。T.28.6。
`/dashboard` の帯にも 1 つ 1 行で出ます (「読めない値: `PROXY_KEEPALIVE_SECS=abc` (既定のまま動いています)」、
「知らない鍵: `PROXY_TIMOUT_SECS` (もしかして `PROXY_TIMEOUT_SECS`)」。T.29.27)。
`settings.error` に入るのは、`SERVER_PORT` や `PROXY_BIND` のように**設定全体が組めない**値を書いたときだけで、
そのときは前の設定を維持します。

**いま何が効いているかは `/config` で 1 枚に出ます** (JSON)。全 `PROXY_*` / `SERVER_*` について
`{"PROXY_DNS_TTL_SECS":{"value":30,"source":"env_file"}, ...}` の形で、`value` は**いま効いている値**、
`source` はそれが来た層 (`default` = このコードの既定 / `env` = 実際の環境変数 / `env_file` = `$HOME/.env` /
`cli` = コマンドライン引数) です。**書いたのに読めない書き方だった行は `default` のまま**出るので、
「`.env` に書いたのに効かない」がその場で分かります (再起動が要る項目は値が古いままなので、
同じ応答の `reload.restart_required` を見てください)。別名のあるキー (`SERVER_DISK` は
`PROXY_DISK_QUOTA_MB` の別名) は、実際に効いた方にだけ `source` が付きます。
一覧の値 (`PROXY_PAC_DIRECT` など) は 1 KiB で切って `"+N more"` を付けます (全部見たいときは `.env` を読む)。
同じ応答に `capabilities` (上記) と `.env` の監視の状態も入るので、**データを取るときは `/config` を 1 枚
一緒に保存しておけば「そのときの設定」が後から読めます**。応答は 64 KiB 以下です。

`PROXY_CACHE_DIR` を指定しない場合は、書き込める最初の候補を使います:
`$XDG_CACHE_HOME/rust-http-proxy` (または `~/.cache/rust-http-proxy`) → `/var/cache/rust-http-proxy` → `$TMPDIR/rust-http-proxy-cache`。
Pterodactyl 以外で root 実行の場合は `/var/cache` を優先します。`$TMPDIR` は tmpfs (RAM) のことが多いので最後の手段です。

### 変数ごとの説明

並びは `/config` と `--check` が出す順 (`Config::settings()`) と同じです。説明の文面は上の表にあったものをそのまま移しました。

#### 待ち受け

- **`SERVER_PORT`** (既定 `8080`)
  プロキシが待受を行うポート番号 (Pterodactyl が自動設定)
- **`PROXY_BIND`** (既定 自動 (`::` + `0.0.0.0`))
  待ち受けアドレスのカンマ区切りリスト (例: `127.0.0.1,[::1]`)。
  未設定ならデュアルスタックで自動
- **`PROXY_IPV6`** (既定 `on`)
  IPv6 を使う (待ち受けと AAAA での接続)。`off` で `0.0.0.0` のみ・A レコードのみ。
  `on` のままでも、IPv6 が黙って落ちる環境では自動で IPv4 を先に試す (上記。確実に避けたいなら `off`)
- **`PROXY_LISTEN_BACKLOG`** (既定 `0` (= `min(1024, somaxconn)`))
  **待ち受けの受け入れ待ち行列の長さ** (`listen(2)` の backlog)。
  Rust の `TcpListener::bind` は **128 固定**ですが、ブラウザがページを 1 枚開くと数十本の CONNECT が**同時に**来るのに accept ループは 1 本しかありません (増やしても速くならないことを測って決めた設計)。
  行列が溢れると SYN は**黙って捨てられ**、クライアントは 1 秒後に再送するので、**利用者には 1 秒の待ち**として見えます (統計には「遅い接続」としてすら残りません)。
  `0` / `auto` は `min(1024, /proc/sys/net/core/somaxconn)`、数値を書けばその値。
  **カーネルが `somaxconn` で頭打ちにする**ので、それより大きく書いても `somaxconn` までしか効きません (この機械の `somaxconn` は `4096`、既定は `1024`)。
  効いている値は起動ログの `listening on ... (backlog N, ...)` と `/config` の `PROXY_LISTEN_BACKLOG` に出ます。
  実際にカーネルに届いたかは `ss -ltn` の `Send-Q` (待ち受けの行) で見られます。
  溢れた回数は `/status` の `kernel.last_5m.listen_overflows` (Linux)。
  **`.env` では即時反映されません** (待ち受けは起動時に 1 回作るため。変更を検知すると `restart_required` に出ます)。
  Linux 以外では `std` のまま 128 です

#### 接続と上限

- **`PROXY_TIMEOUT_SECS`** (既定 `30`)
  接続と**無通信の締め切り** (秒)。読み書きの 1 回が、これだけ 1 バイトも動かなければ閉じる (**1 バイトでも動けば待ち直す**ので、転送全体の締め切りではない。遅い相手は下の `PROXY_MIN_TRANSFER_BPS` が見る)。`0` で**無期限** (`PROXY_TUNNEL_IDLE_SECS` と同じ意味): 何も送ってこないクライアント接続を閉じず、オリジンへの接続と読み書きにも締め切りを置かない (OS 既定に任せる)。
  **ただし何も送ってこない接続は `PROXY_HEADER_TIMEOUT_SECS` (未設定でも 10 秒) で閉じます** (T.28.0)。黙った接続を無期限に置くには両方 `0` にしてください。
  `PROXY_KEEPALIVE_SECS` (要求と要求の間) と `PROXY_TUNNEL_IDLE_SECS` は別に効く
- **`PROXY_CONNECT_TIMEOUT_SECS`** (既定 `3`)
  **オリジンへの TCP の確立だけ**の締め切り (秒)。`CONNECT` と、**T.28.8 (2026-10-25) から forward のオリジン接続にも**効きます。
  **未設定なら 3 秒と `PROXY_TIMEOUT_SECS` の小さい方**です (全体を 2 秒にしていれば 2 秒)。
  **`PROXY_TIMEOUT_SECS=0` (無期限) でも 3 秒**です — 繋がらない相手を無期限に待つ意味が無いためで、**無期限にしたいときは `0` と明示**してください。
  **既定を 5 → 3 秒にした根拠 (2026-10-26、T.28.43。利用者の決定 6)**: タスク 28 の洗い出し (2026-10-25) の見積もりで、
  **縮む待ちは 4.6〜9.0 分/日**、**落とす成功は 0〜0.23 本/日**でした。下の T.23.3 の戻す条件「切った成功が 1 日 1 本以上」に届かないので 3 秒にしました。
  forward の確立も同じ値です (T.28.8)。**5 秒に戻すには `.env` に `PROXY_CONNECT_TIMEOUT_SECS=5`** の 1 行 (再読込で効きます)。
  **既定を 10 → 5 秒にした根拠 (2026-10-20、T.23.3)**: 雪像 4 枚 (tokyo 3 枚 + nagoya 1 枚) の
  `connect_buckets` を数えると、**5 秒を越えて成功した確立は tokyo の 12 日ぶん 91,258 本で 0 本**、
  nagoya の 28 日ぶん (258,951 本、失敗も区間に入れた古い版) でも 22 本 (= 1 日 0.79 本の上限、
  `timeout` 18 本を引けば 4 本) だけでした。2.5〜5 秒も tokyo の 12 日で 4 本 (= 1 日 0.33 本) です。
  一方 `timeout` は tokyo の 62 時間に 3,869 本あり、1 件 10 秒を丸ごと待っていました
  (**待ち 10.4 分/時 = 4.2 時間/日**)。5 秒にすると待ちが半分 (**2.1 時間/日**) になります。
  T.15.6 (2) が「5 秒は攻めすぎ」と言った根拠は、遠いオリジンで SYN の再送 (1 秒 → 3 秒) が重なると
  4 秒台まで伸びうることでした。実測では 4 秒台の成功が 12 日で 4 本しかなく、**5 秒なら 4 秒台は切らずに残ります**
  (切るのは 5 秒を越えた分だけ)。数え直す道具は
  **`scripts/connect-timeout-replay.py SNAP.json …`** (`--secs` / `--hours` / `--json`)。
  (T.23.3 のときの戻し方は `.env` に `PROXY_CONNECT_TIMEOUT_SECS=10` の 1 行。)
  戻す条件は「切った接続のうち本当は繋がったはずのものが 1 日 1 本以上」(T.23.99 で見ました。3 秒でも同じ条件で見ます)。
  **効くのは確立だけです**: オリジンとの読み書き (forward の応答待ちも)・TLS のハンドシェイク・クライアントソケットの読み書き・ブロックリストの取得は `PROXY_TIMEOUT_SECS` のままです。
  **forward も T.28.8 で確立をこちらにしました** (0.27.8 までは「forward は `PROXY_TIMEOUT_SECS` のまま」で、繋がらない宛先への
  forward 1 本がワーカーを 30 秒握っていました。tokyo の `/errors` に `http://10.255.255.1:80` の `timeout`・`connect_ms` 30,005 の 1 件)。
  見直しの根拠は tokyo の雪像 20 枚の `/recent` の forward の行 (重複を除いて 52 本) で、**確立の最大は 244 ms** (p50 86 ms) でした。
  forward の確立は CONNECT と同じ宛先の距離なので、CONNECT の 3 秒の根拠 (上) もそのまま当てはまります。
  期限切れの表現を持っていてオリジンを長く待たない回 (`PROXY_STALE_WAIT_SECS`) は、確立もその短い方に合わせます。
  `PROXY_CONNECT_TIMEOUT_SECS=0` (明示の無期限) のとき、forward の確立は `PROXY_TIMEOUT_SECS` に戻ります。
  繋がらない相手を待つ時間 (利用者から見える「固まっている」時間) だけを縮めるための鍵です — `PROXY_TIMEOUT_SECS` を短くすると大きな本文の転送や遅い応答まで切ってしまうためです。
  Happy Eyeballs の締め切りも同じ値なので、締め切りで抜けた接続は `/errors` に `cause: "timeout"` で残ります。
  **名前解決の時間は含みません** (名前を引いたあとの `connect` に効きます)。
  `.env` で即時反映
- **`PROXY_HEADER_TIMEOUT_SECS`** (既定 `10`)
  **要求行とヘッダーを読み終えるまでの総時間**の締め切り (秒)。越えたら `408 Request Timeout` を返して閉じます。
  `PROXY_TIMEOUT_SECS` の読み取りタイムアウトは「無通信の締め切り」で、**読めるたびに待ち直す**ので、
  1 バイトずつ送り続ける接続 (slowloris) は何秒たっても切れず、1 本でワーカー 1 本と接続枠 1 つを握れました
  (実測 0.27.8: `PROXY_TIMEOUT_SECS=3` で 1 バイト / 2 秒を送ると 20 秒たっても切れない。黙った接続も 30 秒放されない)。
  縛り方は 2 つに分かれます:
  - **1 バイト目が来てから最後のヘッダーまで**: 1 要求目も 2 要求目以降も、この長さの総時間で切ります
    (時計を見るのは、ヘッダーが 1 つの塊で届かずに続きをソケットから読むときだけ。普通の要求では 1 回も読みません)
  - **accept から 1 バイト目まで (1 要求目)**: 読み取りタイムアウトをこの値と `PROXY_TIMEOUT_SECS` の小さい方にして切ります
    (待ち受けから継承させるので接続ごとの `setsockopt` は増えません)。待ち受けに当てる前 (起動直後や `.env` で値を変えた直後) に
    握手を終えて accept 待ちの列に載っていた接続は継承していないので、そのとき列にある分を先に引き取って接続ごとに当てます (T.28.30)
    **2 要求目以降の 1 バイト目までは `PROXY_KEEPALIVE_SECS` のまま**で、この締め切りには入れません (静かに閉じ、408 は返しません)
  本文 (`Content-Length` / chunked) の転送は締め切りの外で、今までどおり `PROXY_TIMEOUT_SECS` です。
  **未設定なら 10 秒と `PROXY_TIMEOUT_SECS` の小さい方** (`PROXY_TIMEOUT_SECS=0` でも 10 秒)。`0` と明示すれば締め切り無し (今までの動き)。
  **既定を 10 秒にした根拠 (2026-10-25、T.28.0)**: tokyo の雪像 (`2026-10-25T203134Z`、1 時間ぶん) の `/profile` で、要求行から
  `Host` まで (`client_read`) は CONNECT 1,105 本で**最大 0 ms** (ヘッダーは 1 つの塊で届く)。accept から要求行までは計器に無い
  (`wait` の外) が、正規のクライアントは握手の直後に要求を送るので 1 RTT (tokyo の接続元は約 248 ms) で届きます。10 秒はその 40 倍で、
  落ちた塊の再送が 3〜4 回重なっても間に合います。
  **上限に当たっているときの `/status`**: 上限の外の 4 枠 (T.13.2) で受けた接続は、ワーカーが全部塞がっていても
  待ち行列に積まずに上限の外のスレッドで動かします (T.28.0。`PROXY_MAX_CONNS` ぶんの沈黙を張られても `/status` は待たずに返る)。
  `408` で閉じた接続は `/recent` の `reason` が **`error:header_timeout`** (T.29.10。それまでは CONNECT の確立の締め切りと同じ
  `error:timeout` で分けられませんでした)、`/status` の `rejected_requests.header_timeout` と `/errors` の 1 行
  (`bad_request:header_timeout`、状態 408) に数えます。ログは debug のままです (本数は相手が決めるので)。
  要求行の前から黙ったままの接続は宛先も要求も無いので `/recent` には残らず、数だけが `rejected_requests` に入ります。
  `.env` で即時反映 (次の接続から)
- **`PROXY_HE_STAGGER_MS`** (既定 `250`)
  Happy Eyeballs (RFC 8305) で、名前が複数のアドレスを返したときに**先頭の候補が返らなければ次の候補を試し始めるまでの間隔** (ms)。
  受けるのは **10〜2000** (RFC 8305 の範囲) で、外れた値と数でない値は起動ログに `WARN` を 1 行出して既定の 250 に戻します (`/config` の出どころも `default` のまま)。
  **起動時に 1 回だけ読みます** (`.env` を書き換えても効かず、`/status` の `settings.restart_required` に出ます)。
  IPv6 が黙って落ちる網ではホストごとに勝った族を覚えるので (上の「250 ms が消えたのは」)、この間隔を払うのは初めて見るホストと 600 秒に 1 回の IPv6 の探りだけです。
  縮めると SYN が遅いだけの相手にも 2 本目を同時に張る回数が増えるので、既定から動かす理由が無ければそのままにしてください
- **`PROXY_KEEPALIVE_SECS`** (既定 `15`)
  クライアント接続を次の要求まで待つアイドル時間 (秒)。`0` で 1 接続 1 要求。
  **待つ長さとは別に、1 本の接続で 1,000 要求を捌いたらその接続は閉じます** (下記)
- **`PROXY_TUNNEL_IDLE_SECS`** (既定 `300`)
  CONNECT トンネルのアイドル打ち切り。双方向とも無通信がこれだけ続いたら両側を閉じる (`PROXY_PARK_IDLE=on` なら、預かり所が期限を見て引き上げる)。
  `0` で無期限。`.env` で即時反映
- **`PROXY_MIN_TRANSFER_BPS`** (既定 `1024`) / **`PROXY_MIN_TRANSFER_WINDOW_SECS`** (既定 `30`)
  **転送中の最小の速さ** (T.29.34。利用者の決定 2026-10-27)。1 B ずつしか読まない (slow-read) / 送らない (slow-POST) 相手は、
  無通信の締め切り (`PROXY_TIMEOUT_SECS`) では切れず (1 バイト動くたびに待ち直す)、それだけで接続の枠を握り続けられる
  (手元で `PROXY_TIMEOUT_SECS=3 PROXY_MAX_CONNS=8`、10 MiB の応答を 512 B / 0.6 秒で読むクライアント 8 本で、正常な要求が 70 秒間ずっと 503)。
  見張るのは 3 か所: **forward の応答をクライアントへ書く**・**要求の本文をクライアントから読む**・**トンネルの書き側** (両向き)。
  数えるのは**相手を待った時間**だけで、待った時間が窓 (`PROXY_MIN_TRANSFER_WINDOW_SECS`) に達するたびに、その間に**相手に届いたバイト**
  (書いた量から送信キューに残っている分を引く) が `PROXY_MIN_TRANSFER_BPS` × 秒に届いていなければ切る (届いていれば数え直す)。
  **送るものが無くて待った時間は数えない**: オリジンが遅い応答 (少しずつ流れてくる配信など) と、**片側が黙っているだけのトンネル (half-idle)** は
  今までどおり `PROXY_TIMEOUT_SECS` / `PROXY_TUNNEL_IDLE_SECS` (300 秒) で閉じる。
  上の 8 本の再現では 30 秒で 8 本とも切り、そのあと正常な要求が通る。1.7 KB/s で読むクライアントは 75 秒たっても切らない。
  切った本数は `/status` の `slow_transfer_closed` (`response` / `request_body` / `tunnel`)、WARN は向きごとに 1 分 1 行
  (`closing a slow response: transfer slower than 1024 B/s (12672 B in 30.0s of waiting)`。続きは `(+N more …)`)。
  トンネルを切った理由は `/recent` で `error:timeout`。
  判定できるのは書き / 読みの 1 回が戻ったときなので、forward の応答 (splice でない経路) では最大 `PROXY_TIMEOUT_SECS` 遅れる
  (splice の経路とトンネルは待たない書きと `poll` で窓どおりに見る)。小さな書き (16 KiB 未満で、まだ待っていない相手) は測らない
  (1 KiB の応答のような熱い経路に時計を足さないため)。
  `0` (どちらか) で切らない (前と同じ振る舞い)。トンネルの見張りは Linux だけ。`.env` で次に始まる転送から反映
- **`PROXY_MAX_CONNS`** (既定 `auto`)
  同時に受ける接続数の上限。上限に当たったら、まず**預かり所の暇な CONNECT トンネルを最古から 1 本閉じて**席を作り、その接続を受ける (閉じた数は `/status` の `evicted_idle` と `/metrics` の `sorahost_evicted_idle_total`。**暇な keep-alive 接続は閉じない** — 次の要求を待っているだけなので、閉じると入れ違いで届いた要求を取りこぼすため)。
  閉じるものが無い (トンネルが全部中継中、または預かり所が空) ときは、スレッドを起こさず `503 Service Unavailable` + `Retry-After: 1` を返して閉じる。
  ただし**自分宛て (`/status` `/metrics` などの内部エンドポイント) は上限 + 4 本まで受ける**: accept の時点では要求が読めないので、4 本までは受けて要求行と `Host` を読み、自分宛てなら普通に応答、それ以外は 503 で閉じる (上限に当たっている最中でも監視が取れるようにするため。この枠で受けた接続は要求行が 2 秒来なければ 503 で閉じる)。
  `auto` は記述子の上限から `min(4096, (RLIMIT_NOFILE の soft − 予備 64) ÷ 4)` (1 接続が最悪で使う記述子は クライアント 1 + オリジン 1 + 素通しのパイプ 2 = 4 本。`ulimit -n` が 1024 の環境なら 240、4096 なら 1008)。
  記述子が余っていても 4096 で頭打ちにするのは、上限が fd 以外の資源 (スレッド・RSS) の歯止めでもあるため (同時 5,000 本で RSS 198 MiB の実測)。
  数値を書けばその値、`0` で無制限。決まった値は起動ログの `max connections:` と `/status` の `max_conns` (`/metrics` は `sorahost_max_connections`) に出る。
  `.env` で即時反映。断った数は `/status` の `rejected_overload` と `/metrics` の `rejected_overload_total`
- **`PROXY_MAX_CONNS_PER_CLIENT`** (既定 `0` (無効))
  **1 つの接続元から同時に受ける接続数の上限**。認証なしの公開ポートで、見知らぬ接続元 1 人が `PROXY_MAX_CONNS` (既定 240) を使い切ると**本人が 503 になる**ため、その手前で頭を押さえるつまみです。
  **認証ではなく公平さの上限**です (同じアドレスから来られれば誰でも通ります)。
  設定すると accept の直後にその接続元の**いま生きている接続の本数**を数え、上限以上なら `503 Service Unavailable` + `Retry-After: 1` を返して閉じます。
  断った数は `/status` の `rejected_per_client` と `/metrics` の `sorahost_rejected_per_client_total`、接続元ごとの内訳は `/clients` の行の `rejected`。
  **自分宛て (`/status` などの内部エンドポイント) は数えません**: accept の時点では要求が読めないので、`PROXY_MAX_CONNS` と同じ「上限 + 4 本」の枠で受けてから要求行を読み、自分宛てなら普通に応答、それ以外は 503 で閉じます (上限に当たっている接続元からでも監視が取れるように)。
  **数え方**: 数えるのは `/connections` の表と同じ「接続の開始と終了」で ±1 する本数で、鍵は接続元 IP (v4-mapped IPv6 は IPv4 として数えます)。
  NAT の内側の複数台は 1 人として数えられます。数えるのは**上限を設定している間だけ**で、`0` に戻すと表ごと捨てます (既定の費用は accept ごとの分岐 1 回)。
  `--lite` でも効きます (`/connections` の行は作らずに本数だけ数えます)。
  同時に来た数本は上限を少し超えて通ることがあります (数えるのは登録済みの本数のため)。
  `.env` で即時反映 (次に受ける接続から。あとから入れたときは、そのとき生きている接続から数え直します)
- **`PROXY_MAX_THREADS`** (既定 `auto`)
  同時に生きていてよい接続スレッドの上限。上限に達したら**新しいスレッドを起こさず、その仕事を待たせる** (捨てない。空いたスレッドが順に引き取る)。
  `auto` は `min(PROXY_MAX_CONNS, コア数 × 64 を 128〜512 に収めた値)` で、コア数は `taskset` で絞られていればその数。
  数値を書けばその値、`0` で無制限 (T.10.5 以前の動き)。
  上限があるのは、預けた接続が一斉に切れたときにスレッドが跳ねないようにするため (暇なトンネル 5,000 本の一斉 close で、上限なしだと一時的に 4,400〜4,700 スレッド・RSS 65 MB、上限 256 なら 260 スレッド・RSS 27 MB)。
  `.env` で即時反映 (次に受ける接続から効く。**下げても走っているスレッドは殺さず**、仕事を終えたスレッドから順に減ります。`auto` のときは `PROXY_MAX_CONNS` を変えるとこちらも決め直します)。
  決まった値は起動ログの `max connection threads:` と `/status` の `max_threads` に出る (いまの本数は `/status` の `live_threads` / `idle_threads`、上限に当たって待たせている仕事は `queued_jobs`。`/metrics` にも `sorahost_max_threads` / `sorahost_live_threads` / `sorahost_idle_threads` / `sorahost_queued_jobs` として出る)。
  **裏側の再検証 (stale-while-revalidate) もこの上限の内側で走ります**が、こちらは待たせず捨てます (`/status` の `revalidations_dropped`)
- **`PROXY_ORIGIN_POOL`** (既定 `64`)
  オリジンへのアイドル接続をホストごとに保持する本数。`0` で再利用しない。
  少ないと同時要求数が多いときに張り直しが増える (実測: 8 本だと 64 並列で p99 が 40 ms 台、64 本なら 10 ms 前後)
- **`PROXY_ORIGIN_POOL_TOTAL`** (既定 `256`)
  アイドル接続の全ホスト合計の上限。多数のホストへ行くときに `ホスト数 × PROXY_ORIGIN_POOL` まで増えないようにする
- **`PROXY_PARK_IDLE`** (既定 `on`)
  アイドルな keep-alive 接続と、両方向とも暇な CONNECT トンネルをスレッドから外し、1 本の監視スレッド (epoll) に預ける。
  `off` で「1 接続 = 1 スレッドが専任」の動きに戻る。
  Linux 専用 (それ以外では自動的に無効)。実測: 暇な接続 2,000 本でスレッド 2,004 → 25 本、RSS 68.0 → 25.2 MB、暇なトンネル 5,000 本でスレッド 5,005 → 68 本、RSS 93.9 → 29.9 MB。
  忙しいときの CPU/要求とシステムコール数は変わらない
- **`PROXY_PARK_GRACE_MS`** (既定 `3`)
  預ける前に同じスレッドで待ってみる時間 (ミリ秒。CONNECT トンネルは下限 100 ms)。
  続けて要求が来る忙しい接続に、預ける/戻すの往復 (`epoll_ctl` 2 回 + ワーカーの受け渡し、実測 18 µs/要求) を払わせないための猶予。
  読み取りタイムアウトをこの長さにして空振りを「暇だ」と解釈するので、システムコールは増えない。
  `0` なら猶予なしで即座に預ける
- **`PROXY_MALLOC_ARENAS`** (既定 `8`)
  malloc のアリーナ数の上限。`0` で glibc の既定 (コア数 × 8) のまま。
  接続ごとにスレッドが増えるため、既定のままだとアイドル接続を多く抱えたときに使われないアリーナが RSS に居座る (実測: 2,000 本のアイドル接続で 80.5 → 26.8 kB/接続)。
  代償は高並列でのロック競合 (実測: 64 並列で CPU/要求 +4%、8 並列では差なし)。
  `2` も測りました (T.29.25、2026-10-27): 400 並列の forward のあとの `heap_free` 12.4 → 10.3 MiB、`malloc_trim` 後の RSS 33.7 → 32.9 MiB と小さく、
  8 並列の CPU/要求は +1.1% (user +3.0%) なので既定は `8` のままにしています。
  実際に掛かった値は `/status` の `memory.arenas` に出ます

#### 名前解決

- **`PROXY_DNS_TTL_SECS`** (既定 `60`)
  名前解決の結果を保持する秒数 (この長さが妥当かは `/dns` の `changes` ÷ (`misses` + `refreshes`) で見ます。答えがほとんど変わらないなら延ばせます)。
  `0` で毎回解決 (それでも 1 要求につき 1 回。`/status` の `dns.misses` がその回数)。
  **直近この秒数以内に使われた名前は期限の 3/4 を過ぎたところで裏で 1 回だけ引き直す**ので、使い続けているホストはミスになりません (`/status` の `dns.refreshes`)。
  解決に失敗したら 1 時間以内の古い結果を使う。`.env` で即時反映
- **`PROXY_DNS_NEGATIVE_SECS`** (既定 `60`)
  名前解決の**失敗**を覚えておく秒数 (`0` で覚えない)。
  覚えている間は OS に問い合わせずに同じエラーを返します (`/status` の `dns.negative_hits`)。
  引けない名前 1 つで 2 秒待たされることがあるための蓋で、古い答えが 1 時間以内にあるときはエラーより古い答えを優先します。
  延ばしたときの効きは **`scripts/dns-negative-replay.py SNAP.json`** が雪像 1 枚の `/log` (NXDOMAIN の 502 の行) で数えます (T.27.3)。
  2026-10-24 の tokyo (8.3 時間、NXDOMAIN 453 本) では 3,600 秒にしても縮む待ちは 1 日 0.4 分で、既定は 60 秒のままです。
  `.env` で即時反映
- **`PROXY_DNS_WARM_SECS`** (既定 `0` = 止める。T.22.4 で `3600` から変えた)
  **keep-warm**: 直近この秒数に **2 回以上**使われた名前 (warm) は、使われていなくても `PROXY_DNS_TTL_SECS` の 3/4 ごとに裏で引き直し続けます (`0` で無効 = 直近 TTL 内に使われた名前だけ 1 回先回りする動きに戻る)。
  **既定で止めている理由 (T.22.4)**: serve-stale (`PROXY_DNS_STALE_SECS`) のもとでは、warm が救っていた期限切れのミスは待たない古い答えの当たりになるので、warm の引き直し (1 名前 80 回/時、tokyo では 32 件で約 2,000 回/時) はミスをほとんど減らしません。
  `scripts/dns-replay.py --grid` の再生 (雪像 3 枚) で、止めると引き直しは今の 5〜27%、`expired` + `warm_stale` のミスは増えませんでした。代わりに古い答えで繋ぎに行く回 (`dns.stale_hits`) が増えます。
  T.21.3 までの動きに戻すときは `PROXY_DNS_WARM_SECS=3600`。
  TTL (60 秒) を熱さの物差しにすると、間隔が TTL より長いホストは 1 つも救えません (デプロイ先の主要 3 件は 2〜10 分間隔で、ミス率は 0.31 / 0.76 / 1.00 でした)。
  1 回だけ使われた名前は warm にしません (引き直しても二度と来ない)。
  答えを持っている名前だけが warm になります (引けない名前は負のキャッシュの担当)。
  同時に warm でいられるのは **32 件**まで (最後の使用がいちばん古いものから外す) で、最後の使用からこの秒数を過ぎたら引き直しを止めます。
  最悪でも 32 件 ÷ 45 秒 ≈ 0.7 回/秒。いま warm な名前の数は `/status` の `dns.warm`、名前ごとの予定は `/dns` の `warm` / `next_refresh_secs`。
  **逆に、この窓より長い間隔で来る名前は救えません**: デプロイ先の実測 (2026-09-18、36.6 時間) では上の 3 件が 0.02 / 0.15 / 0.01 に下がりましたが、`discord.com` だけ 0.15 が残りました — 2〜4 本の塊が**約 1,800 秒おき**に来るので**(当時の既定) 900 秒**の窓をいつも外れ、塊の 1 本目が必ずミスするためです (`2026-09-18T053213Z-snapshot.json` の `$.hosts.hosts` の差分と `$.hosts_series`)。
  **そこで T.15.4 で既定を 900 → 3,600 秒に広げました** (デプロイ先での効きの判定は T.15.99)。
  `.env` で即時反映
- **`PROXY_CANARY`** (既定 `auto`)
  **canary** (利用者の要求が無い時間帯も待ちを測る): `PROXY_CANARY_SECS` 秒に 1 回、宛先へ**名前解決と TCP 接続だけ** (と `PROXY_CANARY_IPV6=on` なら AAAA へもう 1 本) を行って時間を残します (握ったらすぐ閉じ、TLS も HTTP も送りません。相手に届くのは 1 分に 1 回の SYN と FIN だけ)。
  `auto` は**直近 1 時間で最も要求の多い CONNECT の宛先** (`/status` の上位ホストの先頭で、最後に使ってから 1 時間以内のもの) を毎周期選び直します (1 件も無ければ何もしません = 誰も使っていないプロキシは誰にも繋ぎません)。
  `off` で止める。ホストをカンマ区切りで書けばその全部 (最大 8、ポートを省くと 443)。
  結果は `/status` の `canary`、`/history?res=5|60` の `canary` の配列、`/metrics` の `sorahost_canary_seconds{stage="dns"|"connect"|"ipv6_connect"}`、失敗は `/errors` に `kind: "canary"` で 1 件。
  名前解決は表を通さず OS に聞くので利用者の `dns` の数字には混ざりません。
  **履歴スレッドが動いているときだけ回ります** (`--lite` と `PROXY_STATS_PERSIST=off` では回りません)。
  `.env` で即時反映
- **`PROXY_CANARY_SECS`** (既定 `60`)
  canary の周期 (秒、最小 1)。**試験で短くするための口**で、運用では触りません (60 秒に 1 回・1 宛先 1 本なら、相手にも自分にも負荷はありません)。
  `.env` で即時反映
- **`PROXY_CANARY_IPV6`** (既定 `on`)
  canary の **IPv6 側**: 同じ周期に、canary の名前の **AAAA へ 1 本だけ**繋いでみて `/status` の `canary.ipv6_connect_ms` と `/history` の `canary` の 5 列目 (`canary_ipv6_connect_ms`) に残します。
  繋がれば ms、**繋がらなければ `null`** (AAAA が無い名前、`PROXY_IPV6=off`、経路が黒穴、ここが `off`)。
  コンテナの IPv6 が黙って落ちる環境で「生き返ったか」を 1 分の粒度で読むための観測で、**IPv6 の探りも引き受けます** (T.17.7):
  この 1 本が試している間は利用者の要求で 600 秒に 1 回 IPv6 を先頭に戻す探りをやめ (`/status` の `ipv6.probe_by` が `"canary"`)、繋がれば `v4_first` を解きます。
  **Happy Eyeballs も `ipv6` の勝敗 (`attempts` / `wins` / `losses`) もホストごとの族の記憶も動かしません**。`off` にすると、最後の 1 本から 600 秒で探りは利用者の要求に戻ります。
  失敗は `/errors` に残しません (黒穴のままだと 1 分に 1 件ずつ埋まるため)。
  `off` にすると 1 本も出しません。`.env` で即時反映

#### あて先の許可・拒否 (遮断)

- **`PROXY_ALLOW_HOSTS`** (既定 なし (全許可))
  接続許可ホストのカンマ区切りリスト (例: `*.example.com,api.github.com`)
- **`PROXY_DENY_HOSTS`** (既定 なし)
  接続拒否ホストのカンマ区切りリスト (例: `bad.com,*.blocked.org`)
- **`PROXY_CONNECT_PORTS`** (既定 なし (制限なし))
  `CONNECT` を許すあて先ポート。`443,80,8080-8099` のようにカンマ区切り (範囲可)。
  ここに無いポートは 403。`.env` で即時反映
- **`PROXY_ALLOW_LOCAL`** (既定 `off`)
  ループバック (`127.0.0.0/8`, `::1`)・リンクローカル (`169.254.0.0/16`, `fe80::/10`)・未指定 (`0.0.0.0`, `::`) と、
  **私設アドレス** (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, CGNAT の `100.64.0.0/10`, `fc00::/7`) 宛てのオリジンを許すか。
  既定では 403 にしてクラウドのメタデータ (`169.254.169.254`) 経由の SSRF と、プロキシの置き場所の内側の網
  (同じ私設網の機械・NAT の網) へ外から入られるのを防ぐ。値は 3 つ:

  | 値 | ループバック・リンクローカル・未指定 | 私設アドレス |
  |---|---|---|
  | `off` (既定) | 403 | 403 |
  | `private` | 403 | 通す |
  | `on` | 通す | 通す |

  判定は**名前解決のあとの IP** で、CONNECT と forward の両方、IP リテラル (`http://10.0.0.1/`) も同じです。
  名前の答えに 1 つでも当たる IP があれば 403。v4-mapped IPv6 (`[::ffff:10.0.0.1]`) は IPv4 として見ます。
  断った数は今までどおり `/status` の `blocked` と `/errors` の個票 (理由 `local`) に出ます。
  `/config` と `--check` には `on` / `off` は今までどおり `true` / `false`、`private` は `"private"` と出ます。
  ローカルのサービスへプロキシしたいときは `on`、同じ私設網の機械にだけ届けばよいときは `private`。`.env` で即時反映。
  **私設アドレスを既定で断るのは T.28.41 から** (2026-10-26 の利用者の決定)。それまでは T.15.10 の
  「既定で宛先を制限しない」に従って私設アドレスは通していましたが、デプロイ先の記録に `192.168.x.x:27042/27043`
  (Frida の既定ポート)・`10.10.34.35/36`・`10.255.255.1:80` (プロキシ自身の NAT 網) 宛てがあったので、
  **私設アドレスについてだけ**見直しました (それ以外の宛先は今も既定で制限しません)。
  前と同じに動かしたいときは `PROXY_ALLOW_LOCAL=private` です
- **`PROXY_ENDPOINTS_READONLY`** (既定 `off`)
  `on` にすると内部エンドポイントの**書き換える口だけ**を `405 Method Not Allowed` で断ります (`/purge?url=` / `/purge?all=1` / `PURGE <url>` / `/blocklist?...&action=block|allow|clear`)。
  読む口 (`/status` `/healthz` `/history` `/daily` `/slo` `/snapshots` `/metrics` `/hosts` `/hosts/series` `/clients` `/readers` `/explain` `/errors` `/connections` `/recent` `/bursts` `/events` `/trace` `/dns` `/log` `/lookup` `/proxy.pac` `/dashboard` `/inspect` `/probe.html` と、判定だけの `/blocklist?host=`) は今までどおりです。
  **認証ではありません** (読める人は読めます)。公開ポートに出していて「誰でもキャッシュを消せる」のだけを止めたいときのつまみです。
  `.env` で即時反映
- **`PROXY_ALLOW_CLIENTS`** (既定 なし (全許可))
  **受ける接続元**のカンマ区切りリスト (`1.2.3.4,10.0.0.0/8,2001:db8::/32`。1 つの IP は `/32` `/128` と同じ)。
  ここに無い相手は **accept した直後に、要求を 1 バイトも読まずに閉じます** (応答も返しません)。
  **内部エンドポイントも含めて閉じる**ので、公開ポートで `/status` や `/clients` の個票が見られることもありません。
  `PROXY_MAX_CONNS` の 「上限 + 4 本」の枠より**前**で判定します。
  断った数は `/status` の `rejected_client_acl` と `/metrics` の `sorahost_rejected_client_acl_total`。
  v4-mapped IPv6 (`::ffff:1.2.3.4`) は IPv4 として照合するので、デュアルスタックで 待ち受けていても `1.2.3.4` の 1 行で書けます。
  書式が違う項目は読み飛ばします (起動ログの `allowed clients:` に実際に読めた項目が出るので、書き損じはそこで分かります)。
  **宛先の `PROXY_ALLOW_HOSTS` / `PROXY_ALLOW_LOCAL` とは無関係**で、**認証でもありません** (同じアドレスから来られれば誰でも通ります)。
  `.env` で即時反映 (次に受ける接続から)
- **`PROXY_TRACE_CLIENT`** (既定 なし (追跡しない))
  **1 つの接続元だけ**を追いかけて、その要求を `/trace` に残します (T.14.27)。
  値は IP を 1 つ (`198.51.100.7` / `2001:db8::1`。v4-mapped IPv6 は IPv4 として照合します)。
  設定すると **accept した直後に接続元がこの IP かどうかを 1 回だけ**見て、一致した接続に旗を立てます。
  旗の立った接続は、要求ごとに**要求行** (メソッド + **URL の先頭 256 バイト** + HTTP の版)・応答の状態・所要 ms・バイト・**段階の ms**を、CONNECT はトンネルの終わりに**宛先と閉じた理由**を、**1,000 行の環状バッファ**に 1 行書きます (`/trace?n=200&since=<epoch>`、既定 200 行・最大 1,000)。
  **全体のログ水準を `trace` に上げるのとは違い、費用が乗るのはその 1 人だけ**です (アクセスログ 1 行は 7.2 us/要求。既定 (空) では accept ごとの分岐 1 回、設定していても一致しない接続元は要求ごとに旗を読む分岐 1 回だけ)。
  **この口だけは URL のパスが入ります** (他の個票は入れません)。
  認証なしで誰でも読めるので、**調べ終わったら空に戻してください**。
  リングは**メモリだけ**で、個票のファイル (`$HOME/.rust-http-proxy.recent`) にも `/snapshot` にも入れません (パスが保存したファイルに残らないように)。
  `--lite` は接続の枠を作らないので**追跡しません**。
  `.env` で即時反映 (次に受ける接続から。いま開いている接続の旗は動きません)
- **`PROXY_PEEK_SNI`** (既定 `on`)
  CONNECT の最初のバイト (TLS の ClientHello) から **SNI** を読んで個票 (`/recent` の `sni`) に残します (T.14.38)。
  `200 Connection Established` を書いたあと**最初の中継の前に 1 回だけ** `recv(MSG_PEEK)` で 1,024 バイト覗くだけで、**バイトは消費しません** (そのあとの `splice` は今までどおり)。
  費用は**トンネル 1 本にシステムコール 1 回**で、要求ごとにも中継のバイトごとにも増えません。
  覗くのは **443 宛ての CONNECT だけ** (それ以外は TLS とは限らないため)。
  `off` で 1 度も覗きません。`on:<port>` はそのポートも 443 扱いにします (**試験用**。試験のオリジンを 443 に立てられないため)。
  `--lite` は接続の枠を作らないので覗きません。CONNECT の宛先のホストと SNI が食い違った本数は `/status` の `sni_mismatches` と `/hosts` の `sni_mismatch` (どちらもメモリだけ。**IP リテラル宛ては必ず食い違います**)。
  再起動が要ります (`.env` では反映しません)
- **`PROXY_BURST_PERCENT`** (既定 `50`)
  同時接続数が `PROXY_MAX_CONNS` のこの割合を**越えた瞬間**に `/connections` の写真を 1 枚撮って `/bursts` に残す (T.14.6)。
  `0` で撮らない。**同じ山では 1 枚だけ**で、閾の 80% を下回るまで次は撮りません。
  撮るのは履歴スレッド (5 秒周期) なので、接続を受ける経路に増えるのは比較 1 回だけです。
  割合を当てるのは `PROXY_MAX_CONNS` だけで、上限の外の枠 4 本 (自分宛て用) は含めません。
  `PROXY_MAX_CONNS=0` (無制限) と `--lite` では撮りません。
  **履歴スレッドが撮るので `PROXY_STATS_PERSIST=off` でも撮りません**。
  `.env` で即時反映
- **`PROXY_BLOCKLIST_FILE`** (既定 なし)
  ドメインのブロックリスト (hosts 形式 `0.0.0.0 host` または 1 行 1 ドメイン)。
  親ドメインの登録で子ドメインも落ちる。`.env` で即時反映、ファイルの更新は 1 分以内に反映
- **`PROXY_BLOCKLIST_URL`** (既定 なし)
  ブロックリストを取りに行く URL (StevenBlack の hosts など)。
  `$HOME/.rust-http-proxy.blocklist` に保存して再起動後も使う。
  ファイルと両方あれば和集合
- **`PROXY_BLOCKLIST_REFRESH_SECS`** (既定 `86400`)
  URL を取り直す間隔 (最小 60)。失敗したら 10 分後に再試行し、その間は前の一覧を使う
- **`PROXY_BLOCKLIST_EXEMPT`** (既定 なし)
  ブロックリストの対象外にするホストのカンマ区切り (`*.example.com` 可)
- **`PROXY_PAC_DIRECT`** (既定 なし)
  `/proxy.pac` でプロキシを通さず DIRECT にするホストのカンマ区切り (`*.example.com` 可)。
  `.env` で即時反映

#### TLS と消えたクライアントの検知

- **`PROXY_TLS`** (既定 `on`)
  HTTPS のオリジンから取得するか (システムの OpenSSL を実行時に読み込む)。
  `off` で無効
- **`PROXY_TLS_VERIFY`** (既定 `on`)
  オリジンの証明書を検証するか。`off` は自己署名の内部オリジン向け (推奨しない)
- **`PROXY_TLS_CA_FILE`** (既定 なし (システムの CA))
  追加で信頼する CA 証明書 (PEM)
- **`PROXY_TCP_KEEPALIVE`** (既定 `on` (= `on:60:10:3`))
  **消えたクライアントを見つける** TCP keepalive です (T.14.52)。
  端末のスリープや回線の切断では FIN も RST も来ないので、CONNECT トンネルは `PROXY_TUNNEL_IDLE_SECS` (既定 300 秒) の期限切れまで残り、閉じた理由が `idle_timeout` になって**「本当に暇」と「相手が消えた」が区別できません**。
  `on` にしておくと **accept した直後にクライアント側のソケットへ `SO_KEEPALIVE` / `TCP_KEEPIDLE` 60 秒 / `TCP_KEEPINTVL` 10 秒 / `TCP_KEEPCNT` 3 を当てる**ので、消えた相手は **60 + 10 × 3 = 約 90 秒**で `ETIMEDOUT` になり、`/recent` の `reason` が **`client_dead`** になります (`/history` の `closed` の集計では `shutdown` に畳みます)。
  `on:<idle>:<intvl>:<cnt>` で 3 つとも秒 / 回を指定できます (`on:1:1:2` なら約 3 秒。試験用)。
  **費用は接続あたり `setsockopt` 4 回**で、要求ごと・中継のバイトごとには 1 つも増えません (`off` なら accept ごとの分岐 1 回だけ)。
  `TCP_NODELAY` などと違って**待ち受けから継承させていない**のは、`.env` で変えたときに次の接続から効くようにするためです。
  **`--lite` でも当てます**。当てるのは**クライアント側だけ**で、オリジンへ出ていく接続には当てません (そちらは要求が終われば閉じるか接続プールが捨てるため)。
  半端な書き方 (`on:1:2` など) は既定に落とします。
  Linux 以外では何もしません。効いている値は `/config` の `PROXY_TCP_KEEPALIVE`。
  `.env` で即時反映 (次に受ける接続から。いま開いている接続には当て直しません)

#### 記録とプロファイル

- **`PROXY_RECORDS`** (既定 `on`)
  個票 (`/recent` `/errors` `/connections` `/clients` `/log` `/events` `/trace` `/bursts` `/readers`) に**何を残すか**の旗です (T.14.41)。
  `on` は今までどおり、**`off` はその 9 つに 1 件も書かず**個票のファイルにも書きません (`"persisted": false`。**ホスト別の統計 `/hosts` と時系列 `/history` `/daily`、`/status` の数字は残ります** — 個人に結びつくのは接続元の側だけなので)、**`hashed` は接続元 IP を 16 桁の 16 進に置き換えて**残します (**起動ごとの乱数つき FNV-1a 64 ビット**。同じ起動の中では同じ接続元が同じ値になり、再起動すると変わります。`/clients` の鍵も同じ値)。
  **認証を入れない方針なので、公開ポートで個票を誰にも見せたくないときのつまみ**です。
  **判定は必ず生の IP**で行うので `PROXY_ALLOW_CLIENTS` / `PROXY_MAX_CONNS_PER_CLIENT` / `PROXY_TRACE_CLIENT` は `off` でも効きます。
  標準出力のアクセスログは止まりません (`PROXY_LOG_LEVEL=warn` で)。
  費用は記録の入口ごとに分岐 1 回 (既定では変わりません)。
  `.env` で即時反映 (次の記録から。溜まった個票は書き換えません)
- **`PROXY_STATS_PERSIST`** (既定 `on`)
  統計と履歴を `$HOME/.rust-http-proxy.rrd` (固定 8 MiB) に、**個票 (`/recent` `/errors` `/bursts` `/events` `/log`) を `$HOME/.rust-http-proxy.recent` (固定 4 MiB)** に残し、再起動後に読み戻す。
  **1 日 1 行の要約 `$HOME/.rust-http-proxy.daily.jsonl` (追記のみ、上限 2 MiB) もこの設定で書きます** (`/daily`)。
  `off` で無効 (どちらの固定長ファイルも作らず、履歴の収集スレッドも起動しないので `/history` とダッシュボードのグラフ、**カーネルと cgroup の窓** (`/status` の `kernel`)、**ホスト別の時系列** (`/hosts/series`) は空になり、個票の `"persisted"` は `false`、日次の要約も **日次の snapshot** も書きません)
- **`PROXY_SNAPSHOT_DAYS`** (既定 `30`)
  **日次の snapshot** を残す日数 (T.14.34)。
  履歴スレッドが **UTC の日付をまたいだ瞬間**にその時点の `/snapshot` をまるごと `$HOME/.rust-http-proxy/snapshots/<YYYY-MM-DD>.json` (名前は**終わった日**) に書き、**31 個目を書いたら最古を 1 つ消します**。
  1 ファイルは `/snapshot` と同じ **4 MiB** まで (30 日で最大 120 MiB、静かなプロキシなら 1 日 20 KB 前後)。
  `0` で書きません。読む口は `/snapshots` と `/snapshots/<date>`。
  **起動と停止の 1 枚** (`<UTC 時刻>-start.json` / `-shutdown.json`。T.28.20) も同じ日数で、種類ごとに
  `PROXY_SNAPSHOT_DAYS` 日より古いものと新しい方から `PROXY_SNAPSHOT_DAYS` 枚を越えたものを消します (`0` で書きません)。
  `PROXY_STATS_PERSIST=off` と `--lite` では履歴スレッドごと無いので書きません。
  ディスクの空きが `PROXY_DISK_KEEP_FREE_MB` のマージンを割り込むときは書かずに `/events` に 1 件 (`state_file`) 残します。
  **再起動で反映**
- **`PROXY_SLO`** (既定 `connect_p50_ms=5,connect_p95_ms=50,error_rate=0.005,dns_miss_per_connect=0.2`)
  **SLO の 4 つの閾** (T.14.50)。履歴スレッドが 5 秒の標本 1 本ごとに確立の 2 つを、**1 時間の合計で比の 2 つを** (T.29.33) 判定し、**4 つとも満たした標本の割合**を `/slo` で返します (`connect_p50_ms` / `connect_p95_ms` = その 5 秒に閉じた CONNECT 1 本ごとの **「確立 − オリジン側のカーネルの RTT」** の p50 / p95 (ms。T.29.12 から名前解決を除く)、`error_rate` = **名前解決に成功した試みあたり**のエラー ((エラー − `dns`) ÷ (確立 + 転送 + エラー − `dns`))、`dns_miss_per_connect` = 名前解決のミス (**失敗した試みのぶんを除く**) ÷ 確立。**満たす = 閾以下**)。
  **T.28.45 で 3 つの読み方と確立の既定 (10 / 100 → 5 / 50 ms) を変えました** (利用者の決定。鍵の綴りは今までのままなので、前に書いた `PROXY_SLO` はそのまま読めますが、確立の 2 つの値は「確立 − RTT」の閾として効きます)。
  **確立も (名前が引けたうえでの) エラーも 1 本も無い 5 秒は「判定なし」**で分母に入れません (名前解決の失敗だけの 5 秒もここです。T.28.45) (誰も使っていない夜中を「達成」と数えると、達成率が「動いていた割合」に化けるため)。
  **「確立」は繋がった CONNECT の本数**です (T.20.4。`0.18.0` までは失敗した CONNECT も確立に数えていて、失敗 1 件が「確立」と「エラー」の両方に居ました)。試み = 確立 + 転送 + エラー は失敗 1・成功 1 で 2 になり、失敗の「失敗するまでの時間」は `connect_p50_ms` / `connect_p95_ms` に入りません。**繋がらなかった CONNECT しか無い 5 秒 (確立 0 本・エラー 1 件以上) は、`error_rate` だけを外した 5 秒として数えます** (速さとミスの 3 つは 0 として外しません。`0.18.0` までは `error_rate` と `connect_p95_ms` の 2 つを外した 5 秒でした)。宛先が全滅した時間帯も達成率に出ます。
  書いた閾だけが効き、書いていない閾・知らない綴り・数として読めない値・負の値は既定のままです (例: `PROXY_SLO=connect_p50_ms=6` だけ書けば p50 の閾だけ 6 ms になる)。
  効いている値は `/config` の `PROXY_SLO` と `/slo` の `thresholds`。
  判定するのは履歴スレッドなので**要求の経路の費用は 0** で、`PROXY_STATS_PERSIST=off` と `--lite` では 1 本も判定しません。
  **再起動で反映**
- **`PROXY_PROFILE`** (既定 なし)
  `lite` で最速の素通しプロファイル (`--lite` と同じ)。
  キャッシュ・ブロックリスト・統計の保存と履歴スレッド・接続ごとの個票・`/profile`・`/dashboard` を止め、ログを `warn` にする。
  `/status` `/hosts` `/clients` `/errors` の集計はメモリに積む (止めるものと残るものの表は「プロファイル」。T.28.44)
- **`PROXY_PROFILE_SAMPLE_MS`** (既定 `1000`)
  `/profile` のスレッドの標本を取る間隔 (ms)。
  `profile-sample` スレッド 1 本が この間隔で `/proc/self/task/*/stat` と `/proc/self/task/*/syscall` と `/proc/self/task/*/schedstat` を読み、**役割ごと** (`accept` / `conn` / `idle-watch` / `dns-refresh` / `history` / `persist` / `cache-probe` / `profile-sample` / `other`) に「CPU」と「いま走っているか・どのシステムコールで待っているか・休眠か」と「走れずに待った時間」を数え、**CPU を多く使ったスレッド上位 8 本** (`threads_top`) を残します。
  `0` で標本を止める (段階の窓は 5 秒ごとに畳み続けます)。
  下限 50 ms・上限 60,000 ms に丸めます。標本の費用は 1 スレッドにつき `/proc` を 3 つ開くぶん (実測 約 84 us。`schedstat` を足す前は同じ測り方で 68 us) で、**140 スレッド・1 秒間隔で 1 コアの 1.2%** (60 秒で 700 ms)。
  スレッド数に比例するので、多いときは間隔を延ばしてください (自分の CPU は `/profile` の `profile-sample` 役に出るので、そこで確かめられます)。
  **休眠 (`S`) のまま CPU の ticks が前回と同じスレッドは `syscall` と `schedstat` を開かず前回の値を使います** (T.29.21。暇なときの open は 1 本 3 → 1、手元の 9 スレッドで 1 秒 27 → 11 回)。そのため **10 ms (1 tick) 未満だけ走って別のシステムコールで寝直したスレッドは、状態が 1 標本古く見えます** (走れずに待った時間は次に読んだときに累計の差でまとめて入るので消えません)。
  `/proc/self/task/*/syscall` が読めない環境 (seccomp や `hidepid` のコンテナ) では状態が `running` / `sleeping` だけになり `/profile` の `sampler` が `partial` に、`/proc` ごと読めなければ `off` になります。
  x86_64 の `poll` (7) と `epoll_wait` (232) は `ppoll` / `epoll_pwait` の枠に数えます (T.29.14。std の待ちはこちらを使うので、今までは `unknown_syscalls` の `sys_7` / `sys_232` に落ちて idle-watch が `other` 100% に見えていました。状態の数は変えていません)。
  `schedstat` だけが無いカーネル (`CONFIG_SCHEDSTATS` 無し) では `run_delay_us` が `null` になるだけで、ほかは変わりません。
  **`--lite` では `/profile` ごと off**
- **`PROXY_SELF_BENCH`** (既定 `off`)
  **起動直後に loopback だけで 3 秒の自己ベンチ**を回して CPU/要求 と CPU/本 を測ります (T.14.43)。
  待ち受けを開いた直後に、このプロセスの中へ**固定 1 KiB を返すオリジン** (`no-store`) と**すぐ閉じる sink** を `127.0.0.1` の使い捨てポートに立て、**自分の待ち受けへ** forward 8 並列と CONNECT 8 並列を打ちます。
  **外へは 1 バイトも出しません**。結果は `/status` の `self_bench` と、起動ログ・`/events` の 1 行 (`self_bench forward 43 us, connect 140 us …`)。
  **本数に上限があり** (forward **20,000 要求** / CONNECT **2,000 本**)、秒数より先に当たればそこで終わります (上限が無いと CONNECT を 1.5 秒で 22,000 本張り、TIME_WAIT が 44,000 残ります)。
  **自分で打ったぶんはどの統計にも入れません** (`total_requests` / `bytes_forwarded` / `/hosts` / `/clients` / `/recent` / `/connections` / `/history`)。
  測る 3 秒だけログ水準を `warn` に下げ (既定の `info` のままだとアクセスログが数万行出て CPU/要求 に乗るため)、**`PROXY_ALLOW_LOCAL=off` (既定) のままでもこの 2 つのポート宛てだけ**を 3 秒間通します。
  用途は「上の『性能』の表 (この機械の big コア、41.4 us/要求) と**置いた先のコンテナ**を同じ物差しで並べる」ことなので、**普段は `off`** のままにして、再デプロイの直後に 1 回だけ `on` にしてください。
  `off` では起動時の分岐 1 つ以外何も走りません (`--lite` でも明示すれば回ります)。
  **再起動で反映**
- **`PROXY_LOG_LEVEL`** (既定 `info`)
  ログレベル (`error` / `warn` / `info` / `debug` / `trace`)

#### キャッシュとディスク割当

- **`PROXY_CACHE_ENABLED`** (既定 `true`)
  `0` / `false` / `off` / `no` でキャッシュを無効化
- **`PROXY_CACHE_DIR`** (既定 自動選択 (後述))
  ディスクキャッシュ格納先
- **`PROXY_MEM_CACHE_MB`** (既定 `auto`)
  メモリキャッシュ上限。`auto` (動的マージンだけ残して限界まで) か固定値 (MiB)
- **`PROXY_DISK_CACHE_MB`** (既定 `auto`)
  ディスクキャッシュ上限。`auto` か固定値 (MiB)
- **`PROXY_MEM_TARGET_PERCENT`** (既定 `100`)
  `auto` 時に使用率をこの割合で頭打ちにする (任意のキャップ)。
  `PROXY_MEM_CACHE_MB=auto:85` の形でも指定可
- **`PROXY_DISK_TARGET_PERCENT`** (既定 `100`)
  同上 (ディスク)
- **`PROXY_MEM_KEEP_FREE_MB`** (既定 `0`)
  動的マージンに加えて手動で必ず空けておく量 (MiB)
- **`PROXY_DISK_KEEP_FREE_MB`** (既定 `0`)
  同上 (ディスク)
- **`PROXY_CACHE_RESERVE`** (既定 `staged`)
  先行確保 (バラスト) の仕方。`staged` (既定。**使われるまで確保しない**: 保存 0 件なら 0、以後は実使用量の 2 倍まで) / `eager` (予算の未使用分を最初から全部) / `off` (`0` / `false` / `no` も同じ。上限管理のみ)
- **`PROXY_CACHE_PROBE_SECS`** (既定 `5`)
  使用量を測り直して予算を更新する間隔 (秒)。`0` で起動時の 1 回だけ。
  既定は T.29.22 で 1 → 5 秒 (暇なときの `cache-probe` の CPU が 1/5。tokyo では CPU の 23% がこのスレッドだった)。
  保存の上限は保存する側 (メモリは入れるとき、ディスクは書きながら) が自分で守るので、間隔を延ばしても上限は越えません。
  遅れるのは「他のプロセスが資源を使った・圧迫が来た」ときに予算を縮めて手放すまでの間 (最大でこの秒数)。
  動的マージンの窓 (60 秒)・バックオフの減衰 (30 秒)・倍にする間隔 (10 秒) は秒のまま保ちます
- **`PROXY_CACHE_TTL_SECS`** (既定 `300`)
  `Cache-Control` も `Last-Modified` も無い場合の TTL（秒）。
  経験則 TTL の下限でもある
- **`PROXY_CACHE_HEURISTIC_PERCENT`** (既定 `10`)
  `Last-Modified` からの経過時間のこの割合を TTL にする (RFC 9111 4.2.2)。
  `0` で無効
- **`PROXY_CACHE_HEURISTIC_MAX_SECS`** (既定 `604800`)
  経験則 TTL の上限 (既定 7 日)
- **`PROXY_CACHE_MAX_STALE_SECS`** (既定 `2592000`)
  期限切れでも再検証できるエントリを保持しておく最長時間 (既定 30 日)
- **`PROXY_CACHE_GRACE_SECS`** (既定 `60`)
  期限切れ後この秒数以内なら、保存済みの表現をすぐ返して裏で再検証する (stale-while-revalidate)。
  `0` で無効
- **`PROXY_STALE_WAIT_SECS`** (既定 `5`)
  期限切れの表現があるとき、オリジンの接続と最初の応答を待つ上限 (秒)。
  超えたら stale を返す
- **`PROXY_CACHE_MAX_OBJECT_MB`** (既定 `4096`)
  ディスク層に置く 1 オブジェクトの最大サイズ（MiB）
- **`PROXY_MEM_CACHE_MAX_OBJECT_MB`** (既定 `32`)
  メモリ層に置く 1 オブジェクトの最大サイズ（MiB）。これを超えるものはディスクからストリーミング配信
- **`PROXY_DISK_MAX_ENTRIES`** (既定 `2000000`)
  ディスク層の索引に保持するエントリ数の上限 (1 件あたり RAM 約 100 バイト)。
  超えた分は LRU で追い出す
- **`PROXY_CACHE_ADMISSION`** (既定 `on`)
  入場制御。最後の層 (ディスク、無ければメモリ) が 90% 埋まったら、2 回目に要求された URL だけ保存する (一度きりの URL で追い出しを起こさない)。
  見たキーは 512 KiB のブルームフィルタで覚える
- **`PROXY_NEGATIVE_TTL_SECS`** (既定 `60`)
  404 / 410 などの否定応答に `max-age` / `Expires` が無いときの TTL 上限 (明示があればそちらを使う)
- **`PROXY_DISK_QUOTA_MB` (別名 `SERVER_DISK`)** (既定 なし)
  コンテナのディスク割当。**Pterodactyl はこれを渡してくれない**ので、egg 変数として設定する。
  MB 数 = パネルの Disk Space、`0` = 無制限、`auto` = `df -B1 /home/container` の total を割当とみなす (下記)。
  Pterodactyl で未設定ならディスクキャッシュは 512 MiB 固定・先行確保なし
- **`PROXY_DISK_QUOTA_ROOT`** (既定 `$HOME`)
  ディスク割当が適用されるディレクトリ (Pterodactyl では `/home/container`)
- **`PROXY_DISK_PROBE`** (既定 `on`)
  割当が分からないとき Wings の挙動から割当を探るか (後述)。
  `off` なら 512 MiB 固定
- **`SERVER_MEMORY`** (既定 なし)
  コンテナのメモリ割当 (MB)。Pterodactyl が自動設定し、メモリキャッシュの上限として尊重される

## ログ

すべてのログはレベルによらず **標準出力 (stdout)** へ出力されます。既定の `info` レベルで
1 リクエストにつき 1 行のアクセスログが出ます。

```
2026-09-02T02:48:22.900Z INFO  [main] disk cache ready at /home/container/.cache/rust-http-proxy (12034 entries restored, 210 expired removed, 0 migrated, 0 stray files removed)
2026-09-02T02:48:22.901Z INFO  [main] memory cache budget: 0 MiB -> 3482 MiB (container memory 12.1% used)
2026-09-02T02:48:22.901Z INFO  [main] disk cache budget: 0 MiB -> 97800 MiB (disk quota 2.0% used)
2026-09-02T02:48:24.310Z INFO  [main] reserved +3456 MiB memory / +97792 MiB disk (ballast now 3456 MiB / 97792 MiB)
2026-09-02T02:48:23.044Z INFO  [conn#1] ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 200 204B 22.4ms cache=MISS stored ttl=300s
2026-09-02T02:48:23.087Z INFO  [conn#2] ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 200 260B 0.4ms cache=HIT(memory) age=0s ttl_left=300s
2026-09-02T02:53:40.512Z INFO  [conn#3] ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 200 260B 18.9ms cache=REVALIDATED(memory) age=317s ttl_left=300s
2026-09-02T02:53:41.002Z INFO  [conn#4] ACCESS 127.0.0.1 "GET http://example.com/ HTTP/1.1" 304 171B 0.2ms cache=HIT(memory,304) age=318s
2026-09-02T02:48:24.143Z INFO  [conn#5] ACCESS 127.0.0.1 "CONNECT example.com:443 HTTP/1.1" 200 7070B 1010.8ms cache=BYPASS(tunnel)
2026-09-02T03:10:01.502Z INFO  [main] memory pressure detected (PSI some=24.3% full=1.0%): releasing reservations
2026-09-02T03:10:01.502Z INFO  [main] memory cache budget: 3482 MiB -> 2790 MiB (container memory 89.7% used)
```

| レベル | 主な内容 |
|---|---|
| `error` | 起動失敗、致命的な接続エラー |
| `warn` | 400 系 (400 / 403 / 414 / 417 / 431 / 505 / 508。**理由ごとに 1 分に 1 行**、下を参照)、502 とオリジンの時間切れ (**原因ごとに 1 分に 1 行**。T.29.15)、キャッシュ I/O 失敗、資源が計測できない環境、設定の警告 |
| `info` | 起動時の設定サマリ、予算の大きな変化、まとまった先行確保、圧迫 / ENOSPC の検知、アクセスログ |
| `debug` | 接続の受付・切断、自分宛ての要求 (`/status` などの 1 行)、キャッシュ HIT/MISS/STORE/EVICT/REFRESH の詳細、予算の小さな変化 |
| `trace` | リクエスト / 転送 / レスポンスの全ヘッダー、トンネル転送量、L2 書き込み |

**`warn` 以上の行は直近 1,000 行だけプロセスのメモリにも残り、`/log?n=200` で読めます** (1 行 256 B まで、新しい順)。コンソールが流れて消える環境 (Pterodactyl) で「さっき何を警告したか」を後から見るための口です。`info` のアクセスログは写しません (1 行 7.2 us/要求 の熱い経路を重くしないため)。

**ログの量は相手に決めさせません** (T.28.9)。不正な要求 (400 / 414 / 431 / 505) の WARN は
**`/status` の `rejected_requests` の理由ごとに 1 分に 1 行**だけ出し、その 1 分に抑えた本数は次の行の終わりに
` (+N more since the previous line, not logged)` と添えます (本数そのものは `rejected_requests` と `/errors` が全部持っています)。
403 (ACL・ブロックリスト・CONNECT のポート・ローカル宛て) は拒否の理由ごとに、417 と 508 はそれぞれ 1 分に 1 行です
(接続数の上限の WARN と同じ間隔)。今までは 1 本 1 行で上限が無く、走査が来ると `/log` の 1,000 行が攻撃の行で流れていました
(実測: 不正な要求 3 種 × 100 本で WARN 300 行 → 3 行)。**自分宛ての要求** (`/status`・`/healthz`・ダッシュボードが引く口) の
`GET /status -> 200 (internal endpoint)` の行は `debug` です (ダッシュボードを開いたままにすると約 1.2 行/秒でコンソールを埋めていたため。
`PROXY_LOG_LEVEL=debug` で今までどおり見えます)。
**オリジン側の失敗** (CONNECT と転送の `502 Bad Gateway: connect … failed`、`origin response timed out`・
`failed to read origin response`、壊れた `Content-Length`・本文の途中で切れた応答・HTTP/1.0 へ渡せない符号化) の WARN も
**原因ごとに 1 分に 1 行**です (T.29.15。原因は `/status` の `errors_by_cause` と同じ `dns`〜`other` の 8 つと名前解決の内訳
`nxdomain` / `dns_temp`。抑えた本数は同じく次の行の終わりに添えます)。本数は `/errors` (`repeats`)・`/hosts`・`errors_by_cause` が
全部持っています。0.28.48 の tokyo の `/log` は 1,000 行 (19.95 時間) が全部メール宛の
`502 Bad Gateway: connect <mail>:993/995 failed: … lookup …` で、24 時間で 2,228 行ありました
(実測: 繋がらない宛先への CONNECT と転送 100 本ずつ + 本文の途中で切るオリジン 100 本で WARN 300 行 → 2 行)。

アクセスログの `cache=` の値: `HIT(層)` 新鮮なヒット / `HIT(層,304)` クライアントの条件付き要求に 304 で応答 /
`REVALIDATED(層)` オリジンに再検証して 304 を受け延命 / `REFRESHING(層)` 期限切れ直後の表現を返し裏で再検証 /
`COALESCED(層)` 同時に進行中だった取得の完了を待ってキャッシュから配信 /
`STALE(層)` オリジン障害・待ち切れ時に期限切れを配信 /
`MISS stored` 取得して保存 / `MISS` 取得したが保存対象外 / `BYPASS` キャッシュ対象外の要求。

## キャッシュ

GET リクエストのみを対象に、レスポンスをワイヤ形式そのままで 2 段キャッシュへ格納します。

- 保存するのは解読済みの本文 (chunked を外したもの) と、枠組み・接続管理のヘッダーを除いた先頭部分。配信時に
  `Content-Length` / `Connection` を付け直す。形式は `SHPC2` で、旧形式のエントリは起動時に捨てる
- L1 (メモリ) → L2 (ディスク) の順に探索し、`PROXY_MEM_CACHE_MAX_OBJECT_MB` 以下の L2 ヒットは L1 へ昇格。
  それより大きいものはディスクからそのままストリーミング配信
- どちらも LRU で上限を超えた分から追い出し (参照・追い出しともに O(log n))。書き込み中の一時ファイルの分も
  容量計算に含めるので、同時ダウンロードやバラストの再充填で上限を超えることはない
- ディスク上のエントリはペイロード長をヘッダーに記録し、読むときにファイルサイズと照合して途中で切れたものを弾く
- ディスクキャッシュは 256 分割ディレクトリに 1 エントリ 1 ファイルで置き、ファイルの mtime に有効期限を
  記録するので、起動時の走査はファイルを開かずに済む。書き込みは一時ファイルへのストリーミング
- キャッシュキーは メソッド + URL + 正規化した `Accept-Encoding`。`Vary` に `Accept-Encoding` 以外があれば保存しない
- `Range` 付きの要求は、キャッシュ済みの完全な表現があれば `206` で切り出し、無ければそのまま転送して保存しない。
  `HEAD` はキャッシュがあればヘッダーだけ返し、無ければ転送 (保存はしない)
- 次の場合は保存しない: GET 以外 / `Authorization` 付き / `Range` 付き / クライアントの `no-store` /
  レスポンスの `no-store`・`private` / `Set-Cookie` / 非対応の `Vary` / 非キャッシュ対象ステータス / 上限超過
- クライアントの `no-cache` / `max-age=0` は「バイパス」ではなく「オリジンで再検証」として扱う

### HTTPS のオリジン

HTTPS の中身をキャッシュするには TLS を終端する必要があり、CONNECT トンネル (クライアントが `https://` を普通にプロキシ
経由で開く形) では暗号化されたまま素通しになります。代わりに、クライアント側で URL を次の形にしてプロキシへ平文 HTTP
で頼めば、プロキシが HTTPS で取得して保存・配信します。

```bash
# プロキシをオリジンとして叩く形 (スクリプトやダウンロードツール向け)
curl http://127.0.0.1:8080/https/example.com/file.zip -o file.zip
```

要求行に `GET https://example.com/file.zip HTTP/1.1` と絶対 URL を書いて平文で送るクライアント (自作スクリプト等) も
同じように扱います。普通のプロキシ設定で `https://` を開くクライアントは CONNECT を使うので対象外です。
**絶対 URI のスキームは大小を区別しません** (RFC 9110 §4.2.3。T.28.33): `GET HTTP://example.com/x` や `Https://…` も
`http` / `https` に畳んで小文字と同じ要求としてオリジンへ送り、同じキャッシュの項目に当たります (自分宛ての判定と
`bad_request:bad_uri` の数え方も同じ)。0.27.8 までは大文字のスキームを authority として読み、`[HTTP://…]:80` を引きに行って
502 にしていました。マッピング形式 (`/https/…`) はパスなので今までどおり小文字だけです。

- TLS はシステムの OpenSSL (`libssl.so.3` / `libssl.so.1.1`) を実行時に読み込んで使います。クレートもビルド時の依存もなく、
  ライブラリが無い環境では HTTPS 取得だけが無効になります (起動ログに出ます)
- 証明書は既定で検証します (システムの CA ストア、`SSL_CERT_FILE` / `SSL_CERT_DIR`、または `PROXY_TLS_CA_FILE`)
- 応答の `Location` / `Content-Location` が絶対 URL なら `/https/host/path` 形式に書き換えるので、リダイレクトを追っても
  プロキシから外れません
- キャッシュキーには `https://host:443/path` の形で保存されるので、`/https/...` 形式と絶対 URL 形式は同じエントリに当たります。
  **スキームとホストの大小は区別しません** (T.28.10 (a)。`http://LOCALHOST:8080/x` は `http://localhost:8080/x` と同じエントリ。
  パスと問い合わせの大小は区別します)。もともと小文字の URL のキーは変わらないので、ディスクに残った項目はそのまま使えます

### 鮮度と再検証 (RFC 9111)

TTL は `s-maxage` → `max-age` → `Expires` → `Last-Modified` からの経験則 (経過時間の 10%、下限 `PROXY_CACHE_TTL_SECS`、
上限 7 日) → 既定 TTL の順で決めます。**`max-age=abc` のように指示があって値が読めないときは「既に古い」(TTL 0)** として扱います
(RFC 9111 §4.2.1。T.28.10 (d)。バリデータが無ければ保存せず、あれば毎回再検証。0.27.8 までは指示が無いのと同じに扱い、
既定 TTL の 300 秒で新鮮にしていました)。

- ETag か Last-Modified を持つレスポンスは、期限切れでも `PROXY_CACHE_MAX_STALE_SECS` (既定 30 日) の間は残します。
  次の要求では `If-None-Match` / `If-Modified-Since` を付けてオリジンに問い合わせ、304 なら本文を転送せずに
  保存済みの表現を延命して配信します (`cache=REVALIDATED`)。`no-cache` や `max-age=0` のレスポンスも
  バリデータがあれば「毎回再検証」として保存します。
  **304 が返したヘッダーで保存済みの先頭を書き直します** (RFC 9111 §3.2・§4.3.4。T.28.2): 304 にある名前
  (`Cache-Control`・`Expires`・`Date`・`ETag`・`Last-Modified`・`Vary`・独自のヘッダーも) は同じ名前の行を
  置き換え、無い名前の行と状態行はそのまま、本文は 1 バイトも変えません。写さないのは枠組み
  (`Content-Length`・`Transfer-Encoding`)・hop-by-hop と `Connection` で指名された名前・`Age`・
  `Content-Encoding`・`Content-Range` です。メモリとディスクの両方を置き直し (ディスクは本文を写した
  新しいファイルに置き換えます)、裏側の再検証 (`REFRESHING`) でも同じです。0.27.8 までは期限だけを延ばし、
  配る先頭は古いまま (`max-age=0` のまま、など) でした。
  **再検証した応答 (`REVALIDATED`) の `Age` は置き直した時刻から数えます** (304 に `Age` があればその分を足します。T.28.34)。
  0.27.8 までは最初に保存した時刻から数えていたので、保存から N 秒たって再検証した直後の応答が `Age: N` になっていました
  (置き直した項目の次の HIT からは正しい値でした)。
  **304 の `Set-Cookie` / `Set-Cookie2` は保存した先頭へ写さず、再検証した要求のクライアントにだけ渡します** (T.29.0)。
  裏側の再検証の 304 なら誰にも渡しません (その要求は利用者のものではないので)。
  **304 が保存を禁じたら** (`no-store`・`private`・`Accept-Encoding` 以外の `Vary`)、その要求には 304 のヘッダーを
  載せて配り、項目は消します (次の要求はオリジンから取り直します。裏側の再検証なら消すだけ)。
  0.28.48 までは T.28.2 の書き直しが 304 の `Set-Cookie` も写して、**次の利用者の `REVALIDATED` と `HIT`
  (メモリにもディスクにも) に他人の `Set-Cookie` が付いていました**。`Vary: Cookie`・`private`・`no-store` も
  写したまま配り続けていました (期限が 0 になるだけで項目は消えませんでした)。前の版が写してしまった
  `Set-Cookie` は、その項目の次の再検証で落ちます。
  **クライアント自身の条件付き要求 (`If-None-Match` など) に返ったオリジンの 304 でも、保存済みの表現を延ばします** (T.29.6。
  RFC 9111 §4.3.4): 304 はそのまま中継し、304 の `ETag` (無ければ `Last-Modified`) が保存済みの表現と一致すれば、
  上と同じく先頭を書き直して置き直します (一致しない 304 は別の表現のものなので触りません。保存を禁じた 304 なら消します)。
  0.29.35 までは中継するだけで、続く素の GET が古い先頭のままもう一度オリジンへ行っていました
- 期限切れから `PROXY_CACHE_GRACE_SECS` (既定 60 秒) 以内なら、保存済みの表現をすぐ返して裏で再検証します
  (`cache=REFRESHING`、RFC 5861 の stale-while-revalidate)。オリジンが `stale-while-revalidate=N` を付けていれば
  その値も使います。`max-age=0` (毎回再検証) の表現は、オリジンが明示したときだけ対象です。同じ URL の裏側の
  再検証は同時に 1 本、全体で 32 本まで。**裏側の再検証は接続スレッドと同じ置き場で走ります**
  (`PROXY_MAX_THREADS` の内側)。空いているスレッドが無ければ**待たせずに捨て**、その要求はそのまま
  同期の再検証に回ります (捨てても正しさは崩れません。その項目は次の要求で取り直されます)。
  捨てた回数は `/status` の `revalidations_dropped` と `/metrics` の `cache_revalidations_dropped_total` に出ます
- 期限切れの表現があるのにオリジンが遅いときは、接続と最初の応答を `PROXY_STALE_WAIT_SECS` (既定 5 秒) までしか
  待たず、超えたら期限切れの表現を配信します (`cache=STALE`)
- オリジンに繋がらない・5xx を返す場合は、`must-revalidate` でない限り期限切れの表現を配信します (`cache=STALE`)
- クライアントが `If-None-Match` / `If-Modified-Since` を付けてきて保存済みの表現と一致すれば、キャッシュから 304 を返します
- `no-cache` / `s-maxage` / `must-revalidate` 付きの表現と、クライアントが `no-cache` を付けた要求では stale を配信しません
- **要求の `Cache-Control` も見ます** (RFC 9111 §5.2.1。T.28.10 (c)): `max-age=N` (N > 0) は保存から N 秒より古い表現を、
  `min-fresh=N` は期限まで N 秒ない表現を、そのままでは返さずに再検証します (`no-cache` と同じく stale も配信しません)。
  `only-if-cached` で保存から返せないとき (無い・古い) はオリジンへ行かずに `504 Gateway Timeout` を返します
  (キャッシュが無効のときは今までどおり素通しです)。0.27.8 までは `max-age=0` と `no-cache` しか見ていませんでした
- 上流のキャッシュを経てきた応答は `Age` と `Date` から経過時間を差し引いて保存し、配信時の `Age` はプロキシが付け直します。
  **保存から返さない応答 (MISS / BYPASS) にも `Age` を残します** (T.29.4。`Age` は end-to-end、RFC 9110 §7.6・9111 §4.2.3):
  オリジンが `Age` を付けてきた応答だけに、保存するときと同じ `max(受けた Age, now − Date)` を付けます
  (`Date` だけの応答には足しません。時計のずれがそのまま `Age` に出るため)。304 の中継は対象外です。
  0.29.35 までは落としていたので、`max-age=600, Age: 500` の表現を下流は残り 600 秒新鮮と読んでいました
- POST / PUT / DELETE などへの成功応答 (2xx/3xx) を受けたら、その URL (と同じサーバーへの `Location` / `Content-Location`)
  のキャッシュを無効化します (RFC 9111 §4.4)。本文付きの GET と `Range` 要求の応答は保存しません
  (**`Content-Length: 0` の GET は本文なしと同じに扱い、保存も参照もします**。T.29.5。0.29.35 までは毎回 `BYPASS`
  でした。オリジンへは `Content-Length: 0` をそのまま転送します)

### 自動モードの予算と動的マージン

`auto` では 5 秒ごと (`PROXY_CACHE_PROBE_SECS`) に使用量を測り直し、各層の上限 (予算) を次の式で更新します。

```
予算 = 自分が保持しているバイト数 (エントリ + バラスト) + (全体 − 安全マージン − 現在の使用量)
```

「現在の使用量」には自分の分も含まれるので、他プロセスが資源を使えばその分だけ予算が縮み、
バラスト → LRU 追い出しの順に手放します。逆に空けばまた育ちます。安全マージンは固定値ではなく、
次の最大値として測るたびに決め直します。

- **変動幅**: 他プロセスの使用量が直近 60 秒の窓で 1 回の測定の間にどれだけ動いたかの最大値 × 2 (次の測定までに他者が伸びても吸収できる量)
- **床**: メモリは `vm.min_free_kbytes` × 4 と全体の 1% (最低 64 MiB、ただし全体の 1/10 まで) の大きい方、ディスクは全体の 1% (最低 256 MiB、同上)
- **活性ページキャッシュ**: ホストの `Active(file)` (他者が実際に使っているキャッシュ) は奪わない
- **バックオフ**: PSI (`/proc/pressure/memory` または cgroup の `memory.pressure`) で `some avg10 >= 20%` か `full avg10 >= 5%`、
  ディスクは自分の書き込みが ENOSPC になったら、全体の 5% から始めて倍々に増やす。平穏が 30 秒続くごとに 3/4 に減衰。
  圧迫を検知した後 60 秒はバラストを再確保しない
- **手動の最低値**: `PROXY_*_KEEP_FREE_MB`

上限を割合で抑えたいときは `PROXY_*_TARGET_PERCENT` (既定 100 = 抑えない) を下げます。

計測の分母は次のとおりです。

- **メモリ**: `/proc/meminfo` の `MemTotal` / `MemAvailable` に加え、所属する cgroup (v1/v2) の制限と
  `SERVER_MEMORY` (Pterodactyl の割当) をそれぞれ同じ式で計算し、最も厳しい値を採用。
  cgroup の使用量は Docker / Wings と同じ「usage − inactive_file」で見るので、パネルの表示と一致します
- **ディスク**: `statvfs` で `df` と同じ流儀の使用率を見る。`PROXY_DISK_QUOTA_MB` があれば代わりに
  「割当 − 割当ディレクトリ内の自分以外のファイル」を分母にします (60 秒ごとに再計測)
- 計測できない環境 (Linux 以外など) では固定値 (メモリ 200 MiB / ディスク 2048 MiB) にフォールバックして警告します

### 先行確保 (バラスト)

**既定 (`staged`) は「使われるまで確保しない」**です。保存が 1 件も無い間はバラストを 0 に保ち、
最初の保存以降は「エントリ + バラスト ≤ 実使用量 × 2」の範囲でだけ (もちろん予算の範囲でも) 先に押さえます。
`PROXY_CACHE_RESERVE=eager` にすると従来どおり、起動直後から予算の未使用分を全部確保します。

`eager` を既定から外したのは、**256 MiB のコンテナで測ったら何も買っていなかった**からです
(2026-09-10。`scripts/cgroup-run.sh` で再現できます)。

- バラスト無し (`off`) でもメモリ層は `limit_bytes` (197.6 MiB) まで埋まり、`oom_kill` は 0。
  **先に押さえなくても予算は取れる** (64 KiB × 4,160 URL を保存させた実測)
- 逆に `eager` は 192 MiB を先に押さえるので、256 MiB のコンテナに残る空きが 43 MiB しかありません。
  そこへコンテナの中の別プロセスが 80 MiB 取ろうとすると、**1 秒周期のプローブがバラストを返す前に
  カーネルの OOM killer がプロキシを殺します** (同じ条件を 4 回試して 2 回。`off` / `staged` なら
  同じ 80 MiB でも 142 MiB でも平気で、cgroup のピークは 86〜147 MiB に収まりました)
- 段階化すると、要求 0 件のコンテナの RSS は **215.9 MB → 23.1 MB**、
  `disk.reserved_bytes` は **2.9 GB → 0** になります (どちらも 256 MiB の cgroup で 60 秒後の実測)

確保するときの動きは `staged` も `eager` も同じです。

- メモリ: 64 MiB 単位の領域を 0 以外で埋めて全ページをコミット。エントリの追加時にその分だけ解放
- ディスク: `ballast.reserve` ファイルを `fallocate` で伸ばす。エントリの書き込み前に必要分だけ縮める
- 予算が縮んだときはまずバラストを返し、それでも足りないときだけエントリを追い出す
- Pterodactyl で割当を探っている最中 (`PROXY_DISK_PROBE`) だけは、探索そのものが
  `fallocate` で上限を確かめる仕組みなので段階化しません
- tmpfs / ramfs 上のディレクトリでは「ディスク」の先行確保が RAM を食うだけなので無効化して警告
- SIGTERM / SIGINT を受けたら `ballast.reserve` を切り詰めてから終了します (Pterodactyl の停止中にディスク使用量として
  残らない)。強制終了で残っても、次回起動時に必ず空にしてから予算に合わせて作り直します

### Pterodactyl での注意

- メモリは cgroup と `SERVER_MEMORY` の両方で上限が決まるので追加設定は不要です。コンテナ内の `/proc/meminfo` は
  ホスト全体を示しますが、実際に効くのは cgroup 側の予算です
- **ディスク割当は Pterodactyl がコンテナに渡してくれません。** Wings は `/home/container` の合計サイズを監視していて、
  割当を超えると **即座にプロセスを停止し** (`Server is exceeding the assigned disk space limit, stopping process now`)、
  減らすまで起動もできません。egg 変数 `PROXY_DISK_QUOTA_MB` (または `SERVER_DISK`) を用意し、サーバーの
  Disk Space (MB) と同じ値にしてください。無制限なら `0`。**未設定のときはディスクキャッシュを 512 MiB 固定・
  先行確保なしに抑えます** (起動時に警告します)。egg の Configuration Files 機能で
  `{{server.build.disk_space}}` を書き出せる環境なら、それを起動スクリプトで環境変数に渡す手もあります
- **未設定でも自動判断します**: `df -B1 /home/container` 相当 (statvfs) が `/` と別のファイルシステムで、かつ `/` より
  小さければ、それを割当とみなして使います (ボリュームが別ディスクにあるだけのホストで、そのディスク丸ごとを割当と
  誤認しないための条件)。条件に合わなければ、**割当を探ります**:
  - まず 512 MiB ずつ素早くバラスト (fallocate) を伸ばします (実データの上限は 512 MiB のまま)。fallocate が失敗する、
    またはファイルシステムの空きの都合で伸びなくなったら、そこを割当の候補にします。候補のまま 10 分止められなければ
    確定し、実データもそこまで使います。ファイルシステム側で割当が効いているホストなら数分で終わります
  - Wings がディレクトリの合計サイズで止めるタイプのホストでは、素早く伸ばしている間に止められます。停止シグナルで
    バラストは切り詰められるので使用量は 512 MiB 以下に戻り、そのまま再起動できます。再起動時に「起動 10 分以内に
    途切れた」と分かるので、以後は緩やかな探索に切り替えます: 10 分止められないごとに 512 MiB ずつ上げ、増えた分は
    バラストだけで埋め (実データは確認済みの上限まで)、上げた直後に止められたら確認済みの値を割当として記憶します
    (7 日は探りません)。例えば割当 3 GiB なら約 1 時間で 2.5〜3 GiB に落ち着きます
  - つまり最悪 2 回 Wings に止められて再起動が必要になりますが、以降は無設定で割当いっぱいまで使います。
    状態は `/home/container/.rust-http-proxy.state` に残ります。`PROXY_DISK_PROBE=off` で探索を止められます
    (その場合は 512 MiB 固定)。割当が分かっているなら `.env` に `SERVER_DISK` を書く方が早いです
- `SERVER_DISK=auto` を明示すると、`df -B1 /home/container` 相当 (statvfs) の total を割当として使います。
  これが正しいのは、ホストが XFS のプロジェクトクォータや ZFS データセットなどでサーバーごとに領域を切っていて、
  `df` の total がパネルの Disk Space と一致する場合だけです。単なる bind mount ではホストのディスク全体が
  見えるので、起動時に `df /` と比べて同じファイルシステムなら「不明」扱い (512 MiB 上限) に落として警告します。
  Pterodactyl では判断しやすいよう、起動ログに `df -B1 /home/container: total ... used ...` を常に出します
- 超過で止められてしまったら、ファイルマネージャか SFTP で `/home/container/.cache/rust-http-proxy/`
  (特に `ballast.reserve`) を削除すれば起動できます
- 既定のキャッシュ先は `/home/container/.cache/rust-http-proxy` (ボリューム内なので再起動後も残る)

## クレート構成

**外部クレートは 1 つも使っていません** (すべて `std` のみ)。`crates/` にあるのは全部このリポジトリのコードで、
責務ごとの層に分けてあります (41 + 本体)。分けている理由は 2 つで、責務を 1 つに保つことと、`rustc` がクレート単位で
全部を一度に抱えるためビルドのメモリがそのまま行数に比例すること (動作環境の `SERVER_MEMORY` は 256 MiB)。

**クレートの版の決まり** (2026-10-23 に T.26.3 の「デプロイの番号」を撤回して、T.21.6 の形に戻しました): 各クレートの版は
**そのクレートの中身を最後に変えたタスクの番号** `0.<タスク>.<番号>` です (T.19.1 → `0.19.1`、T.25.3 → `0.25.3`)。
本体 (`rust-http-proxy`) と `proxy-server` は**そのビルドに入っている最後のタスク** = 全クレートの版と `src/` を最後に変えたタスクのうち最大です
(`-V` の版は `proxy-server` の `build.rs` が `CARGO_PKG_VERSION` から作ります)。**タグは本体の版**です (`v0.25.3` ↔ `0.25.3`)。
デプロイは複数のタスクをまとめてよく、そのときのタグは**まとめた中で一番大きいタスクの番号**になります (番号はタスクの番号なので、前に出したタグより小さくなることもあります)。
タスクの中で上げるときは `scripts/crate-versions.py --task T.19.1` (分かれ目から中身が変わったクレートを `0.19.1` にし、本体と `proxy-server` を最大にします)。
「中身が変わった」は `crates/<名前>/` の差分から `Cargo.toml` の `version` の 1 行を除いたもの、本体は `src/` の下と**ルートの `Cargo.toml`** (`[profile.*]` など。版の行と `members` は除く。T.29.28) です。
`--check` は `Cargo.lock` の各メンバーの版が `Cargo.toml` と同じことも見ます (手で上げて `Cargo.lock` を直し忘れても、CI は `--locked` なしでビルドが通るため。T.29.28)。

| クレート | 責務 |
|---|---|
| `proxy-sys` | Linux のシステムコールを直接叩く薄い層 (`poll`/`epoll`/`splice`/`pipe2`/`recv`) とシグナル |
| `proxy-base` | ロック、壁時計、JSON の組み立て、`.env` の読み取り、ログ、HTTP 日付、`host:port` の分解と組み立て、コマンドライン引数、ASCII だけを見る文字列の分割、起動ごとの `Via` の印 |
| `proxy-rrd` | 固定長のリングバッファ (状態ファイルと個票のファイルに共通の保存形式) |
| `proxy-sysinfo` | 機械の観測 (メモリ、ディスク、cgroup の上限、`inotify`) |
| `proxy-workers` | 接続スレッドの使い回し |
| `proxy-tls` | システムの OpenSSL を `dlopen` で使う TLS クライアント |
| `proxy-msg` | HTTP メッセージの表現 (ヘッダー、本文の枠、読み取りバッファ、応答の先頭) |
| `proxy-net-dns` | 名前解決 (`getaddrinfo` の答えのキャッシュ、族の記憶、よく使う名前の先読み) |
| `proxy-net-conn` | 接続 (Happy Eyeballs で A / AAAA を並行に試す) と待ち受けソケットの用意 |
| `proxy-net` | アドレスの判定 (ACL とローカル宛ての拒否)、上の 2 つをまとめた facade |
| `proxy-origin` | オリジンへの接続とその使い回し、要求 URL の解釈 |
| `proxy-cachekey` | キャッシュの保存形式と鍵 |
| `proxy-cachecfg` | キャッシュの設定 |
| `proxy-diskprobe` | ディスクの実測 (実際に書いてみて、どれだけ入るかを確かめる) |
| `proxy-capacity` | 使ってよい量の見積もり (空きメモリ・cgroup の上限・コンテナの quota) |
| `proxy-cachemem` | キャッシュのメモリ側 (LRU、エントリ、合流、受け入れ判定、保存されない鍵の記憶) |
| `proxy-cachedisk` | キャッシュのディスク側 (ファイル形式、走査、書き出し) |
| `proxy-cache` | キャッシュ本体 (メモリ側とディスク側を束ねる) |
| `proxy-config` | 起動時の設定 |
| `proxy-metrics-types` | 計測の型と定数 (ホスト別統計・1 要求の内訳・エラーの原因)、分位点、ホスト別の時系列、カーネルと cgroup の統計 |
| `proxy-metrics-recent` | 記録の個票 (閉じた接続・1 つの接続元の要求の並び・出来事の時系列) |
| `proxy-metrics-profile` | 時系列の窓の土台 (解像度と 1 区間の応答時間)、段階ごとの待ちとスレッドの標本 |
| `proxy-metrics-window` | 転送の速さ、接続元の個票、履歴のリング。`proxy-metrics-profile` の 2 つも今までの名前で出し直す |
| `proxy-metrics-core` | 計測の本体 (`Metrics`。`/status` の組み立て) |
| `proxy-metrics-slo` | 窓を読んで要約するもの (SLO・日次の要約・日次の雪像) |
| `proxy-metrics-watch` | 窓を読んで判定するもの (異常の検知・利用者が居ない時間帯の様子見 (canary))。`proxy-metrics-slo` の 3 つも今までの名前で出し直す |
| `proxy-metrics` | 状態ファイルへの読み書きと、5 秒ごとの記録スレッド。上の 7 つを今までの名前で出し直す口 |
| `proxy-blocklist` | ドメインのブロックリスト |
| `proxy-reload` | `$HOME/.env` の再読込 (`inotify` で見張る) |
| `proxy-prom` | Prometheus 形式の出力 |
| `proxy-freshness` | RFC 9111 の鮮度判定 |
| `proxy-http` | 中継の本体 |
| `proxy-tunnel` | CONNECT トンネル |
| `proxy-endpoints-core` | エンドポイントの共通の土台 (要求 1 本ぶんの文脈、問い合わせ `?a=1&b=2` の読み方) |
| `proxy-endpoints-explain` | 1 相手の説明 (`/explain?host=` `?client=`) |
| `proxy-web` | ブラウザで開くページの HTML (`/dashboard` `/inspect` `/probe.html`) の実体。const 3 つだけの葉 |
| `proxy-endpoints` | プロキシ自身のエンドポイント (`/dashboard` `/status` `/metrics` …) |
| `proxy-selfbench` | 起動直後の自己ベンチ (`PROXY_SELF_BENCH=on`)。内蔵の小さなオリジンと打ち手、CPU の測り方。依存は `proxy-base` だけ |
| `proxy-bench` | 計測用の道具 (既定のビルド対象から外してあります。`cargo build --release -p proxy-bench`) |
| `proxy-server` | 接続の受け付け、keep-alive、アイドル接続の預かり。下の層を今までの名前で出し直す口でもある |
| `proxy-run` | 起動の全部 (設定の読み取り、待ち受け、スレッドの立ち上げ、起動ログ)。`main` の中身 |
| `rust-http-proxy` | facade (`proxy-server` の再輸出) と実行ファイル (`main` は `proxy-run` を呼ぶだけ) |

## ビルド・テスト


> **メモリの小さい環境向けの設定**: `.cargo/config.toml` で `jobs = 1` にしてあります。
> このリポジトリは **約 90 MB のメモリでリリースビルドが通ります** (2026-09-26、手元の aarch64・rustc 1.98.1 で実測。
> 90 MB は測った 4 回とも通り、85 MB 以下は 2 回とも落ちます。
> CI が毎回確かめているのは **120 MB の関門を通るかどうかだけ**で、CI で通る最小は週 1 回だけ測っています)。
> LTO を既定で切っているのと、全クレートを `codegen-units = 4` にしてあるのもこのためです (下の「プロファイルの設定」)。
> `rustc` はクレート単位で全部を一度に抱えるため、いちばん大きいクレート (`proxy-endpoints`) の 1 プロセスで
> **RssAnon 82.5 MB** 使い (通る最小はここに 7〜20 MB 足した値になります。`/usr/bin/time -v` の最大 RSS は
> 246 MB と出ますが、そのうち 110〜122 MB は `librustc_driver` のファイル由来のページで、ページキャッシュに
> 載っていれば cgroup には課金されません)、既定の並列数だと
> その合計がコンテナのメモリ上限を超えて OOM killer に落とされます (実測: 180 MB の cgroup で、
> 並列だと落ち `-j 1` なら通る)。潤沢な機械で急ぐときは `cargo build --release -j 8` で上書きできます
> (8 コアで 32.3 秒 → 14.9 秒)。
>
> 手元で確かめるなら `cargo clean && scripts/build-memory.sh 120` (通る最小を探すなら
> `scripts/build-memory.sh --find 80 85 90 95 100 110 120`)。このスクリプトは **実際にその上限の
> cgroup の中でビルドします** — RSS を測るだけだと、メモリ圧のかかっていない機械ほど大きく出て
> 機械をまたいだ判定にならないためです。cgroup は systemd に作らせます (システムの `systemd-run --scope`、
> 無ければ**ユーザーの** `systemd-run --user --scope`。どちらも無い環境では参考の RSS だけ出して判定しません)。
```bash
# テスト実行
cargo test

# 通常ビルド
cargo build

# 最適化リリースビルド
cargo build --release

# 起動 (本体のパッケージの bin は 1 つ。ベンチは別のクレート `proxy-bench` で、`default-members = ["."]` なので
# `-p proxy-bench` を付けたときだけビルドされる)
cargo run --release

# ベンチ (オリジンもベンチ内で起動する。プロキシは別端末で PROXY_ALLOW_LOCAL=on を付けて先に上げておく)
cargo run --release --bin bench -- --proxy 127.0.0.1:18080 --conc 8 --seconds 5
# --conc 並列数 / --seconds 測定秒数 / --body-bytes 応答本文の大きさ
# --only direct|forward|tunnel|connect|connect-multi|idle-tunnels|idle-conns|syscall-cost|
#        replay|all で 1 種だけ測れる
# direct 行はプロキシを通さないオリジン直結 (ベンチ自身の上限。固定しなければ 26〜30 万 req/s、
#   LITTLE に固定すると 5.5 万 req/s)
# tunnel 行も --seconds 秒だけ 1 本のトンネルに流す (この行はベンチ律速。上の「性能」の注)
# idle-tunnels / idle-conns は --conc 本を張ったまま --seconds 秒握る (スレッド数と RSS 用)。
#   idle-conns は 1 要求ずつ通してから握る。どちらも PROXY_MAX_CONNS=8192 を付けて測る
# connect-multi と replay は scripts/deployed-like.sh の中で回す (上の「デプロイ先に似せた条件」)。
#   replay は /recent の個票を同じ間隔・同じ本数で張り直す (--replay-file / --speed / --rtt-file)

# この機械での sendto / recvfrom 1 回の実費 (プロキシは使わない。CPU の固定が要る)
taskset -c 4-7 cargo run --release --bin bench -- --only syscall-cost --seconds 3
```

### CI (GitHub Actions)

push と Pull Request で `.github/workflows/ci.yml` が回ります。`check` は `cargo fmt --check` →
`clippy -D warnings` → **文書とコードの整合** (`scripts/check-docs.sh`) → `scripts/` の単体テスト →
**クレートの版の決まり** (`scripts/crate-versions.py --check`) → **`shellcheck scripts/*.sh scripts/mx`** (切った指摘とその理由は `scripts/.shellcheckrc`) → `cargo test --workspace` →
リリースビルド → 短いベンチ、の順です。**120 MB の cgroup でビルドが通るか** (`scripts/build-memory.sh 120`) は
別のジョブ `build-memory` で並べて回します。それと並べてもう 1 つ、`deployed-like-snapshot` が
**デプロイ先に似せた条件** (上の `scripts/deployed-like.sh`) を runner の中に作り、`scripts/ci-snapshot.sh` で
`--only connect-multi` と forward を 10 秒ずつ回して `/snapshot` を**成果物 (artifact) の `snapshot.json`**
に残します。見張るのは**形の退行だけ**で、数字の絶対値は比べません (runner は世代も負荷も毎回違うため):
`connect-multi` の **p50 が 50 ms 以上なら CI が落ちます** (= Happy Eyeballs の 250 ms がまた毎回乗っている)、
`/snapshot` が 1 要求で取れて JSON として読めること、その中に `/status` の `ipv6.v4_first` と `/profile` の段階が
入っていて**全部 0 ではない**こと。runner でユーザー名前空間 (`unshare -rmnC`) が作れないときは
**IPv6 の黒穴だけ諦めて**残りを回します (`ipv6_blackhole: false` と印字し、CONNECT は IP リテラル宛て)。
落ちたときこそ中身が要るので、`snapshot.json` は成功しても失敗しても成果物に付きます (14 日保存)。
手元で同じものを回すなら `cargo build --release && cargo build --release -p proxy-bench` のあとに
`scripts/ci-snapshot.sh` (手元で 26 秒。閾は `CI_SNAPSHOT_P50_MAX_MS`、秒数は `--seconds` で変えられます)。

**関門 (120 MB) の余裕は週 1 回だけ測ります** (`build-memory-find`。月曜 03:17 UTC に上限を小さい順へ上げながら
リリースビルドを試し、**通った最小 = N MB** と関門までの余裕をジョブの step summary に数字で残します。push では
回しません — 段ごとに `cargo clean` からのビルドが要り、runner を長く占有するためです)。いま知りたいときは
`gh workflow run ci.yml` (`-f ladder="95 100 105 110"` で段も指定できます)。

**コミットの前に手元で全部を確かめるのは `scripts/check-all.sh`** (T.29.29)。fmt・clippy・`check-docs.sh`・
`check-dashboard.js`・`scripts/` の unittest・`crate-versions.py --check`・shellcheck を**1 つ落ちても止めずに全部回し**、
最後に落ちたものの名前を並べて exit 1 にします (`&&` でつなぐと最初に落ちた所で止まって残りが見えず、T.28.48 では
fmt に落ちるコミットを push しました)。`--only fmt,versions` / `--skip clippy` で絞れ、入っていない命令 (node・shellcheck) は
「飛ばした」と出します。`cargo test` は入れていないので別に回してください。

**shellcheck を手元で回すのも同じ 1 行です**: `shellcheck scripts/*.sh scripts/mx`
(切った指摘とその理由は `scripts/.shellcheckrc` にあり、shellcheck が自動で読みます。入っていなければ
`apt-get install shellcheck` / `brew install shellcheck`。CI の `check` ジョブも同じ命令を回すので、
引数は CI と揃えてください)。

**文書とコードが同じ集合かを見るのは `scripts/check-docs.sh`** (T.14.56)。エンドポイントも環境変数も
別々の作業で増えていくので、「**コードにあるのに文書に無い / 文書にあるのにコードに無い**」を機械で見ます。
突き合わせるのは 9 つで、(a) `crates/endpoints/src/endpoints/mod.rs` の `path == "/…"` と
上の「動作確認 (curl)」の一覧と `/` の案内 (`endpoint_list`)、(b) コードの `"PROXY_…"` / `"SERVER_…"` の
文字列と上の「環境変数」の表の鍵の欄、(c) `/snapshot` の `parts` と個票のエンドポイント、
(d) `scripts/check-dashboard.js` が画面の HTML から切り出す関数名と、そこで実際に呼んでいる名前、
(e) 上の「クレート構成」の表と `crates/*/Cargo.toml` の `[package] name` (表の前の「(41 + 本体)」の数も見ます)、
**(f) この README が `scripts/<名前>.py` と書いている道具に実行ビットがあるか** (そのまま叩ける道具か。
README にあるのにファイルが無いものも NG。共通部品の `proxydata.py`、判定の部の `snapshot_criteria.py`、
unittest が名前で読む `test_*.py` は、叩かないものとして除外表に理由つきで入れてあります。T.24.7)、
**(g) 台帳** (`TASKS.md` の**未着手**、`TODO.md` の §5 (タスクの見出しと索引だけ)、`tasks/TNN.md` の
3 つ。**移し忘れ** (`TASKS.md` に `- [x]` が無いこと。済んだタスクは `tasks/` へ移します)、
**リンク** (`tasks/TNN.md` と §5 の `### タスク NN (…)` の見出しが両方向で
1 対 1)、**番号** (`T.24.10` が台帳全体で 1 回だけ。番号のタスクが置き場所と合うこと)、
**1 行目** (`# タスク NN — …`) を見ます。中身は `scripts/check-ledger.py` で、台帳だけを回すなら
これ 1 本です。T.24.10 / T.24.13)、
**(h) 判定表の既定** (`collect-deployed.sh` の `CRITERIA` の既定と、文書の「既定は phaseNN」。T.28.23)、
**(i) `-h` の既定** (`crates/base/src/cli.rs` の `-h` が括弧で書く既定と、上の環境変数の表の既定。T.28.23) です。
(f) と (h) は README のほかに `docs/*.md` (性能・特徴の本文。T.29.32) も読みます。
**意図的に外してあるものはスクリプトの中の除外表に理由つきで持ちます** (`/snapshot` に入れない 18 の口、
ビルド時にしか無い `PROXY_VERSION`、`tasks/README.md` = `tasks/` の索引 など)。差分が 1 件でもあれば
その名前を印字して終了コード 1 です。
bash と grep / sed / awk / python3 の標準ライブラリだけで動くので、`cargo` も Node も要りません
(`scripts/check-docs.sh` を引数なしで回すだけ。CI の `check` が毎回回します)。

### 起動直後のスレッド数

「使わない機能にはコストを払わない」方針なので、無効にした機能のスレッドは起動しません。

| 設定 | スレッド | 内訳 |
|---|---|---|
| 既定 (キャッシュ・統計あり) | 9 | 待ち受け + `env-reload` + `cache-probe` + `persist` + `history` + `shutdown` + `idle-watch` + `canary` + `profile-sample` (0.29.x の x86_64 で数えた) |
| `PROXY_CACHE_ENABLED=off PROXY_STATS_PERSIST=off` (= `--lite`) | 4 | 待ち受け + `env-reload` + `shutdown` + `idle-watch` |
| 上記 + `PROXY_PARK_IDLE=off` | 3 | `idle-watch` が減る |

ブロックリストの取得スレッドは `PROXY_BLOCKLIST_FILE` / `PROXY_BLOCKLIST_URL` が設定されたときだけ動きます
(`$HOME/.env` の再読込で後から設定された場合もその時点で起動します)。

接続 1 本ごとのスレッドはこれとは別です (CONNECT トンネルは Linux では中継中 1 本あたり 1 スレッド、
暇なあいだは 0 本)。

**接続スレッドは使い回します**: 仕事を終えたスレッドは空き置き場に戻り、30 秒使われなければ自分で
終わります (空きは最大 64 本まで)。生成と破棄のシステムコール (実測で 1 接続あたり約 16 回) を
接続ごとに払わない形です。

**同時に生きているスレッドには上限があります** (`PROXY_MAX_THREADS`、既定 `auto`)。
「空いている数」(最大 64 本) と「生きている数」は別のもので、後者がこの上限です。
上限に達したら新しいスレッドを起こさず、その仕事を待たせます (**捨てません**。空いたスレッドが
順に引き取ります)。上限が無かったころは、預けた 5,000 本のトンネルが一斉に切れると 1 本ずつ
ワーカーへ渡すので**一時的に 4,400〜4,700 スレッド**まで増えていました (閉じるのに shutdown・
アクセスログ・統計が要るので、監視スレッドの中では落とせません)。上限 256 なら同じ場面で
**260 スレッド・ピーク RSS 65 → 27 MB** で、閉じきるまでの時間は変わりません (0.66 秒)。

上限は `.env` の再読込で変えられます。当て直すのは接続を受けたときなので、書き換えてから
**次の接続 1 本**で効きます。**上げた場合**は待たせていた仕事をその場で起こし、**下げた場合は
走っているスレッドを殺しません** (代理の途中で切らないため)。新しいスレッドを起こさなくなり、
仕事を終えたスレッドから順に終わるので、しばらくは `/status` の `live_threads` が
`max_threads` を上回ったままになります。

**裏側の再検証 (stale-while-revalidate) もこの置き場で走ります**。以前は再検証のたびに
スレッドを起こしていたので上限の外にいました。ただし待ち方は接続と逆で、**空きが無ければ
待ち行列に積まずに捨てます**。再検証は「後でやればいい仕事」で、捨てても次の要求で
普通に取り直されるだけなのに対し、積むと (1) 新しい接続の処理がその後ろに並び、
(2) 順番が来るまで「このキーは再検証中」の印を握り続けるためです。捨てた要求はそのまま
同期の再検証に回るので、クライアントには新しい表現が返ります。

代償は「暇なトンネルを預かる速さ」です。トンネルは 1 本ごとに猶予 100 ms のあいだワーカーを
握るので、**次々に張られる CONNECT を受けられる速さの天井が「上限 ÷ 100 ms」**になります
(上限 256 で毎秒 2,560 本。実測で 5,000 本の確立が 1.9 → 2.45 秒)。それより速く暇なトンネルが
増える環境では `PROXY_MAX_THREADS` を上げてください。短命な CONNECT (張ってすぐ閉じる) と
HTTP の代理はワーカーをすぐ手放すので、この天井には当たりません (実測でも変化なし)。

**要求を処理していない keep-alive 接続はスレッドを握りません** (`PROXY_PARK_IDLE`、既定 on、
Linux のみ)。次の要求が猶予 (既定 3 ms) のあいだ来なければ、その接続は `idle-watch` スレッドの
epoll に預けられ、ワーカースレッドは解放されます。読めるようになったら空いているワーカーに戻します。
実測で暇な接続 2,000 本のとき 2,004 スレッド・RSS 68.0 MB → **25 スレッド・25.2 MB**、
忙しいときの CPU/要求とシステムコール数は変わりません。`off` にすると
「1 接続 = 1 スレッドが専任する」元の動きに戻ります
(2026-09-08、`release`、`PROXY_MAX_CONNS=8192 scripts/cpu-per-request.sh --only idle-conns --conc 2000`
の 3 回の中央値。`off` 側は同じコマンドに `PROXY_PARK_IDLE=off PROXY_MAX_THREADS=0` を足したもの)。
**預かりが 0 の間、`idle-watch` は眠ったまま起きません** (T.29.23。以前は 1 秒ごとに起きていて、
暇な `--lite` の CPU 51 us/s のうち 43 us/s がこれだった → 8 us/s)。預かっているものがあれば先頭の期限で起き、
眠っている所へ預けたとき・先頭より早い期限で預けたとき (`.env` で keepalive を縮めた直後) は、預ける側が
eventfd の呼び鈴で起こして待ち時間を計り直させます。

**暇な CONNECT トンネルも同じ預かり所に預けます** (同じ `PROXY_PARK_IDLE`、Linux のみ)。
両方向とも 100 ms 動きが無ければ、クライアント側とサーバー側の記述子 2 本を epoll に預けて
スレッドを手放します (中継に使うパイプもこのとき手放します)。どちらかが動いたら空いている
ワーカーへ戻し、`PROXY_TUNNEL_IDLE_SECS` を過ぎたものは預かり所が引き上げて閉じます。
預けているあいだも同時接続数には数えます (`PROXY_MAX_CONNS` の意味は変わりません)。数えているので、
上限に当たった接続はこの中の**最古の 1 本を閉じて**席を作ります (`PROXY_MAX_CONNS` の行を参照)。
実測で暇なトンネル 5,000 本のとき 5,005 スレッド・RSS 93.9 MB → **68 スレッド・29.9 MB**
(`/status` の `parked_tunnels` で本数が見えます)。片方向だけ閉じたトンネルは預けません。

中継に使う `splice(2)` のパイプは、**空になったものをワーカースレッドが 2 本まで持ち越して
次のトンネルで使い回します**。短命なトンネル (CONNECT を張ってすぐ閉じる) は 1 本あたり
`pipe2` 2 回・`fcntl(F_SETPIPE_SZ)` 2 回・`close` 4 回を払っていたので、CONNECT 確立の
システムコールが 29.1 → 20.1 回/本、CPU/本 が 163.5 → 139.7 us になりました。
中身が残ったままのパイプ (送り先が途中で消えたとき) は使い回さずに閉じます。

### 開発者向け: プロファイル取得

```bash
# シンボル付きでビルドする (profile.release は strip = true なので strip も止める)
CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none cargo build --release
perf record -g -p $(pgrep -f 'rust-http-proxy$') -- sleep 10   # 別端末でベンチを回している間に
perf report --stdio | head -40                                 # ホットパスを見る
strace -c -f -p $(pgrep -f 'rust-http-proxy$')                 # perf が無ければシステムコールの回数で見る
strace -f -e trace=write,sendto -c -p $(pgrep -f 'rust-http-proxy$')  # 1 要求あたりの write 回数
```

`[profile.release] debug = 1` は入れません (バイナリが太るため)。必要なときだけ上の環境変数で付けます。

## 配布 (Docker / Release バイナリ)

```bash
# Docker (2 段ビルド。実行イメージは debian-slim + libssl3)
docker build -t rust-http-proxy .
docker run -p 8080:8080 rust-http-proxy
curl -x localhost:8080 http://example.com/
```

イメージに入れるのは `Cargo.toml` / `Cargo.lock` / `.cargo` / `crates` / `src` だけです
(本体は 41 個のクレートに分かれているので `crates` が要ります)。
版の文字列を作る `build.rs` は `crates/server/` にあるので `crates` に含まれます (T.15.12 段 6')。`.git` は入れないので、
イメージの中の版は `rust-http-proxy 0.29.35+unknown` のように `+unknown` になります。
`.dockerignore` で `target/` と `**/target/` を送らないようにしてあります
(イメージには元から入りませんが、ビルドコンテキストとして daemon へ送ると
`cargo build` 済みの作業ツリーでは 1 GB を超えて遅くなるため。除いたコンテキストは約 2 MB)。

タグ `v*` を push すると (か `release` を `workflow_dispatch` で起動してタグを打たせると。「運用」の 0) `.github/workflows/release.yml` が x86_64 / aarch64 の
バイナリ (glibc 2.35 以上) を作って Release に添付します。

### musl 静的リンクを採らない理由 (実測)

「1 ファイルでどこにでも置ける」ので一度は入れましたが、実測して外しました。

| | forward, 8 並列 | CONNECT 確立 |
|---|---|---|
| glibc (通常ビルド) | **32,169 req/s, p50 0.204 ms** | 8,071 tunnels/s |
| musl 静的リンク | 10,846 req/s, p50 0.602 ms (**1/3**) | 7,277 tunnels/s |

加えて、

1. **TLS が使えない**: `dlopen` が機能しないので `libssl` を実行時に読み込めず、起動ログに
   `TLS: libssl not found` が出ます。`https://` オリジンの取得とキャッシュが無効になります
   (`CONNECT` トンネル = ブラウザの HTTPS は影響なし)
2. **名前解決が NSS を通らない**: musl は `/etc/resolv.conf` だけを見ます (systemd-resolved / mDNS が効かない)
3. **ビルドが 120 MB に収まらない**: 静的リンクにしてもビルドが軽くなるわけではありません

### プロファイルの設定 (実測で決めたもの)

| | `release` (既定) | `dist` (配布用) |
|---|---|---|
| `opt-level` | `3` | `"s"` |
| `lto` | `false` | `true` (fat) |
| `codegen-units` / `strip` | `4` (全クレート) / あり | `1` / あり |
| ビルドが通る最小のメモリ | **約 90 MB** (手元。90 は 4 回とも通り、85 以下は毎回落ちる。2026-09-26。CI では未測定) | 350 MB 以上 |
| バイナリ | 2,760,504 B | **1,842,976 B** (852 KB 小さい) |
| CPU/要求 (forward 8 並列) | 41.4 us | **39.6 us** |

(バイナリの大きさと通る最小のメモリは 2026-09-26 (T.17.11 = T.15.12 段 8) の実測です。2026-09-24 (段 7) の時点は
`release` 2,694,968 B・通る最小 105〜110 MB、2026-09-19 (段 5・段 6') の時点は
`release` 2,563,896 B・通る最小 120〜125 MB、2026-09-08 の時点は `release` 1,381,872 B / `dist` 1,054,184 B (327 KB 小さい) でした。
CPU/要求 の 2 つは 2026-09-08 の値で、段 5 / 段 7 では測り直していません。速さの測り方は上の「性能」と同じです)

**LTO を既定で切っている理由**: `cargo build --release` を **メモリ 120 MB のコンテナで通す** ためです。
LTO の重さは最終リンクの 1 回にかかり、クレートを 26 に割ってもそこは小さくなりません。
26 クレート・`jobs = 1` で 3 つの選択肢を同じ日に測った結果 (2026-09-07。手元の aarch64 で、その上限の cgroup に
入れて実際にビルドしたもの。**バイナリと CPU はこのときのコードの値**で、いまの `release` は上の表のとおりです。
CI で通る最小は測っていません):

| `[profile.release]` | 通る最小のメモリ (手元) | クリーンビルド | バイナリ | CPU/要求 (forward) |
|---|---|---|---|---|
| `lto = false` (既定) | **100 MB** | 38.7 秒 | 1,316,336 B | 50.19 us |
| `lto = "thin"` | 170 MB | 55.4 秒 | 1,316,328 B | 50.09 us (-0.2%) |
| `lto = "fat"` | 350 MB | 48.8 秒 | 1,185,248 B | 47.95 us (-4.5%) |

`fat` は効きますが 120 MB の約 3 倍のメモリが要ります。`thin` は 170 MB 払ってもバイナリが
8 バイトしか変わらず、速さもぶれの中です。配布バイナリだけ `dist` (fat LTO) で作ります。

**`codegen-units` は全クレート `4`** (2026-09-24、T.15.12 段 7): ビルドの山はコード生成 (LLVM) の段で、
`codegen-units = 1` はクレート全体の LLVM IR を 1 つのモジュールとして一度に抱えます。
全クレートに当てて測った実測 (手元 aarch64、`jobs = 1`、RssAnon の最大) は
`proxy-metrics-watch` 121.7 → 92.5 MB (`= 4`) → 84.3 MB (`= 16`)、クリーンビルド 95 → 106 → 133 秒で、
**4 より上げても減りません** (コード生成の前の段 = 型検査とメタデータの床が約 80 MB)。
全クレート `4` のいまの上位は `proxy-endpoints` 82.5 > `proxy-server` 81.5 > `proxy-metrics-recent` 76.9 >
`proxy-http` 74.7 MB です (2026-09-26、T.17.11 で `proxy-metrics-watch` 95.4 MB と `proxy-metrics-window` 91.1 MB を
2 つずつに割った後。段 7 の前は `proxy-metrics-window` 107.8 MB がいちばん上)。
`codegen-units` を増やすとクレートの中でのインライン化の範囲が狭まりますが、手元もデプロイ先も共有の機械で
CPU/要求 が数 % ぶれるので手元の A/B は取らず、**デプロイ先で「桁で悪くなっていないか」だけ**を見ます。
悪くなっていたら `Cargo.toml` の `[profile.release]` の `codegen-units` を `1` に戻す 1 行で戻ります。
`--lite` のシステムコール 5.022 → 5.023 回/要求、確保 10.006 → 10.006 回/要求 で、要求の経路の費用は動いていません。
2026-09-19 (段 5) から 2026-09-24 (段 7) までは、要求の経路に乗らない 9 クレートだけを
`[profile.release.package.<名前>]` で `4` にしていました。
`dist` は `codegen-units = 1` を明示しています (release の `4` を継ぐと fat LTO でも `.text` が 16.9 KB 太るため)。

**最適化レベル**: `release` は `opt-level = 3`、`dist` は `"s"` です (`dist` はサイズを優先)。
fat LTO と合わせた `dist` の実測 (2026-09-08) は `release` に対して forward 8 並列 **+1.9%** (CPU/要求 41.4 → 39.6 us で -4.4%)、
キャッシュ HIT +0.7%、64 並列 -1.2%、CONNECT 確立 -8% (この経路のぶれ ±25% の中) で、**CPU 以外はほぼぶれの中**です。
それでも `dist` は 721 KB 小さい (2026-09-19 の実測) ので、配布は `dist` のままにしています。

## 起動方法

```bash
# 基本起動 (メモリ・ディスクとも動的マージンだけ残して限界まで自動確保)
SERVER_PORT=8080 ./target/release/rust-http-proxy

# ACL・タイムアウト付きで起動
SERVER_PORT=8080 \
PROXY_ALLOW_HOSTS="*.example.com,*.github.com" \
PROXY_DENY_HOSTS="evil.example.com" \
PROXY_TIMEOUT_SECS=15 \
./target/release/rust-http-proxy

# 使用率を 80% / 90% で頭打ちにし、先行確保をやめて上限管理だけにする
SERVER_PORT=8080 \
PROXY_MEM_TARGET_PERCENT=80 \
PROXY_DISK_TARGET_PERCENT=90 \
PROXY_CACHE_RESERVE=off \
./target/release/rust-http-proxy

# 従来どおり固定上限 (メモリ 200MB / ディスク 2GB) で起動
SERVER_PORT=8080 \
PROXY_LOG_LEVEL=debug \
PROXY_MEM_CACHE_MB=200 \
PROXY_DISK_CACHE_MB=2048 \
PROXY_CACHE_DIR=/var/cache/rust-http-proxy \
./target/release/rust-http-proxy

# Pterodactyl (SERVER_PORT / SERVER_MEMORY / P_SERVER_UUID はパネルが渡す。ディスク割当だけ egg 変数で)
PROXY_DISK_QUOTA_MB=51200 ./rust-http-proxy
```

## 動作確認 (curl)

```bash
# HTTP プロキシ経由のリクエスト
curl -x http://127.0.0.1:8080 http://example.com/ -I

# HTTPS (CONNECT) プロキシ経由のリクエスト
curl -x http://127.0.0.1:8080 https://example.com/ -I

# キャッシュヒットの確認 (2 回目は X-Cache: HIT ヘッダーが付く)
curl -x http://127.0.0.1:8080 http://example.com/ -I
curl -x http://127.0.0.1:8080 http://example.com/ -I | grep -i x-cache

# 条件付き要求にはキャッシュから 304
curl -x http://127.0.0.1:8080 http://example.com/ -H 'If-None-Match: "<ETag>"' -I

# エンドポイントの一覧 (ブラウザでプロキシの URL を開いたときと同じ案内)
curl http://127.0.0.1:8080/

# ヘルスチェック (200 = 健康、503 = どれかの検査が偽。軽い JSON)
curl -i http://127.0.0.1:8080/healthz

# メトリクス確認 (キャッシュ統計・予算・マージン・先行確保量・システム使用量・カーネルと cgroup の窓を含む)
curl http://127.0.0.1:8080/status
curl "http://127.0.0.1:8080/status?sort=errors"         # 上位 50 をエラーの多い順で切り出す (dns / slow も)
curl "http://127.0.0.1:8080/status?hosts=0"             # hosts[] を省いた軽い形 (監視のポーリング用。T.25.1)
curl http://127.0.0.1:8080/metrics                      # Prometheus 形式
curl http://127.0.0.1:8080/config                       # 効いている設定とその出どころ (下記「環境変数」)
curl "http://127.0.0.1:8080/profile?res=5"              # 待ちの段階・スレッドの CPU と状態・ロックの取り合い
curl "http://127.0.0.1:8080/profile?res=5&n=180&offset=180"   # 続きの 180 標本 (next_offset が null になるまで)
curl "http://127.0.0.1:8080/profile?summary=1"          # 標本なし。5 分 / 1 時間 / 全部 の CPU/要求 だけ (重い口ではない)

# 個票 (誰が・いつ・なぜ)
curl "http://127.0.0.1:8080/connections"                # いま開いている接続
curl "http://127.0.0.1:8080/errors?n=100"               # 直近のエラー (誰が・いつ・どの原因で)
curl "http://127.0.0.1:8080/log?n=200"                  # 直近の警告とエラーのログ行
curl "http://127.0.0.1:8080/recent?n=200"               # 閉じた接続 (新しい順)
curl "http://127.0.0.1:8080/recent?sort=slow&n=20"      # 確立のいちばん遅かった 20 本
curl "http://127.0.0.1:8080/recent?client=198.51.100.7" # ある接続元だけ
curl "http://127.0.0.1:8080/recent?since=$(( $(date +%s) - 3600 ))"   # 直近 1 時間に開いたもの
curl "http://127.0.0.1:8080/recent?n=500&offset=500"    # 続きの 500 本 (next_offset が null になるまで)
curl "http://127.0.0.1:8080/bursts?n=50"                # 山が立った瞬間の写真 (新しい順)
curl "http://127.0.0.1:8080/trace?n=200"                # 追跡中の 1 接続元の要求 (PROXY_TRACE_CLIENT。URL が入る)
curl "http://127.0.0.1:8080/events?n=200"               # 起動・再読込・圧迫などの出来事 (新しい順)
curl "http://127.0.0.1:8080/events?since=$(( $(date +%s) - 86400 ))"  # 直近 1 日の出来事だけ
curl -s "http://127.0.0.1:8080/events?n=512" | grep -o '"kind":"anomaly","text":"[^"]*"'  # 自動検知だけ
curl "http://127.0.0.1:8080/history?res=5"              # 時系列 (既定 720 標本 = 1 時間) + closed + transfer
curl "http://127.0.0.1:8080/history?res=5&n=4320"       # 5 秒刻みで 6 時間ぶん (T.14.32)
curl "http://127.0.0.1:8080/history?since=restart&summary=1&normal_hours_only=1"  # 起動からの要約 1 行
curl "http://127.0.0.1:8080/dns?sort=misses"            # 名前解決の表 (age / host / misses で並べ替え)
curl "http://127.0.0.1:8080/dns?limit=4096&offset=600"  # 続きの名前 (next_offset が null になるまで。T.27.1)
curl "http://127.0.0.1:8080/hosts?sort=slow&limit=200"  # 全ホスト (/status の hosts[] は上位 50 だけ)
curl "http://127.0.0.1:8080/hosts?limit=500&offset=500" # 続きの 500 件 (1,000 件は 1 枚に入らない)
curl "http://127.0.0.1:8080/clients?sort=requests&limit=200"  # 全接続元 (User-Agent・宛先の種類・ポート)
curl "http://127.0.0.1:8080/hosts/series?top=16"        # 上位 16 ホストの 5 分 × 24 時間
curl "http://127.0.0.1:8080/hosts/series?host=connect://mtalk.google.com:5228"  # 1 ホストだけ
curl "http://127.0.0.1:8080/explain?host=mtalk.google.com"   # 1 宛先を 1 枚に (統計 + /dns + 個票 + 判定)
curl -s "http://127.0.0.1:8080/explain?host=mtalk.google.com" | python3 -c 'import json,sys;print(json.load(sys.stdin)["summary"])'
curl "http://127.0.0.1:8080/explain?client=198.51.100.7"    # 1 接続元を 1 枚に
curl "http://127.0.0.1:8080/readers"                    # この口を読んでいる相手 (走査か、自分の監視か)
curl "http://127.0.0.1:8080/daily?n=365"                # 1 日 1 行の要約 (永久に残る。古い順)
curl "http://127.0.0.1:8080/slo?days=7"                 # SLO の達成率 (日ごと・時間ごと・外れた時間帯)
curl -s http://127.0.0.1:8080/snapshot > snap.json      # 上の全部を 1 要求で (4 MiB まで)
curl "http://127.0.0.1:8080/snapshots"                  # 日次で残した snapshot の一覧 (30 日ぶん)
curl -s "http://127.0.0.1:8080/snapshots/2026-09-15" > day.json   # その日ぶんをそのまま
curl -s "http://127.0.0.1:8080/snapshots/2026-10-25T203134Z-shutdown" > before.json   # 停止の 1 枚 (`moments[]` の `name`)
# ブラウザで開くなら http://127.0.0.1:8080/dashboard (「いま」を見るコントロールパネル)
# ブラウザで読むなら http://127.0.0.1:8080/inspect (上の個票を時間軸で描く「調査」ページ)
# 端末から測るなら http://127.0.0.1:8080/probe.html (往復とプロキシ経由の取得をブラウザで測る)

# ブラウザの自動設定と、ブロックリストの判定
curl http://127.0.0.1:8080/proxy.pac                    # 自動設定スクリプト (ブラウザにこの URL を設定する)
curl "http://127.0.0.1:8080/blocklist?host=example.com" # ブロックしているか (action=block|allow|clear で上書き)

# キャッシュの操作・確認
curl -X PURGE -x http://127.0.0.1:8080 http://example.com/file.zip     # 1 URL (全バリアント) を消す
curl "http://127.0.0.1:8080/purge?url=http://example.com/file.zip"     # 同じことを GET で
curl "http://127.0.0.1:8080/purge?all=1"                               # 全消去
curl "http://127.0.0.1:8080/lookup?url=http://example.com/file.zip"    # 保存状態 (層・サイズ・期限)
```

**応答の形の版 (`schema`)**: JSON を返す口はどれも、**応答の先頭に `"schema":N`** を持ちます (整数。`/proxy.pac` と
`/metrics` と HTML のページは JSON ではないので付きません)。エラーの応答 (400 / 404 / 405) にも
同じ版が入ります。`/snapshot` は**外側と各部の両方**に持ちます (各部はそれぞれの口の出力
そのものなので、切り出して 1 本の応答として読めます)。

```bash
curl -s http://127.0.0.1:8080/status | head -c 32     # {"schema":1,"status":"ok","versi
```

読む道具 (`scripts/status-diff.py` / `scripts/snapshot-diff.py` / `scripts/check-dashboard.js`) は
この版で分岐します。**版を持たない古い出力は「版 0」として今までどおり読めます**
(鍵の有無から形を推測する道)。定義は `crates/metrics/src/metrics.rs` の `SCHEMA` の 1 か所です。

| 版 | 日付 | 何が変わったか |
|---|---|---|
| 0 | 〜2026-09-16 | `schema` が無い。読む道具は鍵の有無 (`parts` があるか、`hosts` が object か) から形を推測する |
| 1 | 2026-09-16 | 全 JSON の先頭に `"schema"` を足した (タスク 14 の形。`/status` `/history` `/recent` `/snapshot` … の他の鍵と並びはそのまま) |

**形を変えるとき (鍵を消す・意味を変える・入れ子を変える) は `SCHEMA` を +1 して、この表に
1 行足す決まりです。鍵を末尾に足すだけなら上げません** (読む側は知らない鍵を無視できるので、
版を上げると「変わっていないのに分岐が増える」ことになります)。

`/history` の応答には、上の標本 (`keys` / `samples`) とは**別に** `canary` の配列が付きます
(`res=5` と `res=60` のときだけ中身が入ります。T.14.10)。

`/dashboard` はブラウザで開くコントロールパネルです (依存なしの 1 ページ。2 秒ごとに `/status`、5 秒ごとに `/history`、
**30 秒ごとに `/status?sort=errors` と `?sort=dns`** を取って描きます (ほかに 5 秒ごとに `/errors?n=20`・`/connections`、30 秒ごとに `/profile?res=…`・`/slo?days=7`・`/events?n=200`。`/profile` は重い口 (同時 1 本) なので T.28.25 で 5 → 30 秒にした — 開いたままで `collect-deployed.sh` を回しても `busy` で断られにくい)。**起動**のカードは、最後の起動が「普通の再起動 (直前に `shutdown`)」か「無言の再起動」かと、`boot.id` の先頭・断った要求 (`rejected_requests`・重い口・上限) の数を出します (T.28.25)。並べ替えた 2 本だけ間隔を空けているのは、
2 秒ごとに引くとプロキシ側で 50 ホストぶんの組み立てが 3 倍になるためです)。`/history` は 5 秒間隔の JSON で、既定は直近 1 時間分 (720 標本) です
(リングは 6 時間ぶん持っていて `?n=4320` で遡れます。T.14.32)。**標本は配列の配列**で、列名は先頭の `keys` に
1 回だけ出ます (項目が 32 に増えたため)。要求数・転送量・命中/ミス・メモリ/ディスク使用量・RSS・
**上限に当たって暇なトンネルを追い出した回数** (`evicted_idle`) は累計 (ブラウザ側で
差分を取ってレートにする)、応答時間の分布 (`connect_*` / `forward_*` の件数・合計 ms・最大・12 段の区間) と
エラー (`errors` / `errors_by_cause`)・名前解決 (`dns_misses` / `dns_ms_sum`) は**その区間だけ**の値、
接続数・スレッド数・記述子数 (`active` / `threads` / `fds`) はゲージで、粗い解像度へ畳むときは平均と
**最大** (`active_max` / `threads_max` / `fds_max`) の両方を残します (平均に畳むと山が消えるため)。
`/history` の応答には**別の配列** `"kernel":{"keys":[...],"samples":[[...]]}` が付きます
(上の「カーネルと cgroup の窓」。`res=3600` は `null` = この解像度の窓は持っていません)。
`?summary=1` を付けると**標本を返さず、その期間を畳んだ 1 行**になります (上の `/history?...&summary=1`。T.14.24)。
ブラウザの HTTP プロキシにこのプロキシを設定した状態で `http://ホスト:ポート/dashboard` を開くと要求は
絶対形式で届きますが、ポートが自分の待ち受けポートなら自分宛てとして応答します (自分へ転送してループしません)。

**自分宛てかどうかはポートだけで決めます**: 絶対形式 (`GET http://host:PORT/status`) は URL の、
**オリジン形式 (`GET /status` + `Host:`) は `Host` のポート (無ければ 80) が自分の待ち受けポートと同じときだけ**
自分宛てです。自分宛てで知らないパスは 404、`/` は 200 でエンドポイントの一覧を返します
(ブラウザでプロキシの URL を直接開いた人への案内。`--lite` でも出ますが、持っていない `/dashboard` は載せません)。
**自分宛ての `HEAD` は `GET` と同じヘッダー (`Content-Length` も GET の本文の長さ) で本文だけ返しません**
(外の生死監視やロードバランサが `HEAD /healthz` を投げても 200 になる。RFC 9110 §9.3.2)。
`GET` / `HEAD` / `PURGE` 以外のメソッド (`POST` など) は `405` + `Allow: GET, HEAD`、書き換える口
(`/purge` / `/blocklist?action=`) への `HEAD` は何もせず `405` + `Allow: GET`、`/favicon.ico` は本文なしの `204` です (T.28.7。
それまではどれも 404 と案内 2.8 KB でした)。
`Host` のポートが違うオリジン形式は今までどおり `Host` 宛てに転送します (透過プロキシの使い方を壊さないため)。
`Host` の無い HTTP/1.0 のオリジン形式は今までどおり 400 です。
以前はオリジン形式を無条件に自分宛て扱いにしていたため、知らないパスが `Host` 宛ての転送に落ち、
**`Host` が自分自身だと自分へ `max_conns` 本つないでループしていました** (手元の再現: 1 要求で 33 本)。
保険として、転送する要求には `Via: 1.1 rust-http-proxy/<起動ごとの 8 桁 16 進>` を付け、
自分の印が付いた要求を受けたら `508 Loop Detected` で閉じます。

**書き換える口 (`/purge?url=` / `/purge?all=1` / `/blocklist?...&action=`) も、読む口と同じくポートだけで自分宛てにします**
(T.29.35、利用者の決定「すべて外から使える」)。公開の名前 (例 `tokyo.sorahost.net`) で開いたダッシュボードの「消す」と
ブロックリストの上書きのボタンも効きます。0.28.48 と 0.29.0 では、宛先の名前が `localhost`・ループバック・自分の IP
(と `PROXY_SELF_NAMES` に書いた名前) のときだけ受け、それ以外は転送に回していました (T.28.40・T.28.47。戻したので
`PROXY_SELF_NAMES` はもう読みません)。そのため、ポート 80 で動かすと外のサイトが `http://任意の名前/purge?all=1` を
踏ませるだけで全消去やブロックリストの改変ができます。止めたいときは `PROXY_ENDPOINTS_READONLY=on` です

これらのパスはプロキシ自身が応答し、同じポート宛てのオリジン形式の要求より優先します。認証は無いので、
到達できる人は誰でも purge できます (公開ポートで動かすなら到達制御を)。
**`PROXY_ENDPOINTS_READONLY=on`** にすると、書き換える口 (`/purge` / `PURGE` / `/blocklist?action=`) だけを
405 で断ります (読む口はそのまま)。**公開ポートで見知らぬ接続元が増えたら `PROXY_ALLOW_CLIENTS` で絞れます**
(一覧に無い相手は accept 直後に閉じるので、内部エンドポイントにも届きません)。
**`PROXY_MAX_CONNS_PER_CLIENT=N`** は、1 つの接続元が同時に開ける本数を N 本に抑えます (超えた接続は 503。
自分宛ては上限の外で受けるので監視は取れます)。どれも**認証ではありません** — このプロキシに `Proxy-Authorization` は
無く、入れる予定もありません。経路を絞る (`PROXY_ALLOW_CLIENTS`) か、消せる口を閉じる (`PROXY_ENDPOINTS_READONLY`) か、
1 人の取り分を抑える (`PROXY_MAX_CONNS_PER_CLIENT`) かの 3 つだけです。

**重い口は同時に 1 本だけ組みます** (つまみはありません。いつもこの動作です)。`/snapshot` は 17 本ぶんの口を
1 つにまとめた**最大 4 MiB** の JSON で、認証が無いので誰でも好きなだけ叩けます。1 秒に 10 回引かれると、それだけで
CPU と統計の鍵の時間を食います。そこで**大きい応答を組む口は同時に 1 本**とし、組んでいる最中に来た 2 本目からは
**`503 Service Unavailable` + `Retry-After: 1`** (本文は `{"schema":1,"error":"busy"}`) で断ります。重い口はこの 6 つです:

| 口 | 重いと見なす条件 |
|---|---|
| `/snapshot` | いつも (17 部・最大 4 MiB を組む) |
| `/profile` | `?summary=1` **以外**はいつも (段階・スレッドの CPU と状態・ロックの取り合いの窓。要約は標本を 1 本も組まないので軽い口です。T.15.0 (11)) |
| `/explain` | いつも (上の口を横断して 1 枚に組む) |
| `/hosts` | `limit` が **200 より大きい**とき (`/hosts?limit=1000` など) |
| `/recent` | `n` が **500 より大きい**とき (`/recent?n=2000` など) |
| `/history` | `n` が **720 より大きい**とき (`/history?res=5&n=4320` は 1.9 MB) |

既定の `limit` / `n` のまま引くぶんには重い口になりません。`/status` `/healthz` `/metrics` のような軽い口は
**この判定にも旗にも触りません** (監視が 5 秒ごとに叩く口を断ったら、断ったこと自体が事故になるため)。
断った累計は `/status` の末尾の **`heavy_rejected`**。**走査に対する最小限の保護で、認証ではありません** —
順に引けば誰でも全部 200 で取れますし、`Retry-After: 1` のとおり 1 秒後に引き直せばまず通ります。
旗は**組み終わったところ**で戻すので、読むのが遅い相手 1 人が全員に 503 を返させることはありません。
費用は重い口に来たときの原子 1 回だけで、プロキシとしての転送 (forward / CONNECT) には 1 命令も足していません。

ホスト別統計には応答時間 (平均・p50・p95・最大 ms、CONNECT は接続確立までの時間) も入り、`/metrics` では
`sorahost_host_request_duration_seconds` ヒストグラムとして出ます。ダッシュボードのホスト表は要求数・遅い順 (p95)・
エラー率・**名前解決 (合計)**・**確立の平均**・転送量で並べ替えられ、「名前解決 / 接続 (ms)」と「v4 / v6」の列で
**待ちの内訳**が読めます (この並べ替えは受け取った 50 件の中での並び替えで、**どの 50 件を切り出すか**は
`/status?sort=` の側です)。

ダッシュボードの図は 11 枚で、上段の KPI に「**CONNECT 確立 p50 (直近 1,024 本)**」「**利用者が待つ時間 p50**」
「**名前解決**」(ミス率 = `misses ÷ (hits + misses)`、ミス 1 回の値段 `miss_avg_ms`、あれば先回りの回数)
「**CPU の絞り**」が出ます。
**p50 の KPI は `/status` の `recent_quantiles.connect` (直近 1,024 本の実測。T.14.31)** で、
札に本数・下に p90 / p99 / 最大とその窓の秒数が付き、**1 ms 未満も丸めません**。
`recent_quantiles` を持たない版の `/status` (や `--lite`) では、今までどおり
**直近 5 分 (60 標本) の区間のヒストグラムを足し合わせて補間した値**に落ち、札が「直近 5 分」に戻ります
(図「CONNECT 確立 p50 / p95」は今までどおり区間の補間です — 時系列は標本ごとの分布からしか描けないため)。「接続中」のカードには
`上限 240 · 山 218 (直近 5 分) · 503 0` の行が出ます (山は `/history` の直近 5 分の `active_max` の最大。
上限の 80% で黄、95% で赤)。その下の「**悪いホスト (上位 10)**」の表は `/status?sort=errors` と `?sort=dns` の
2 枚を混ぜたもので、エラー数・原因の内訳・名前解決の平均 (`dns_ms_sum ÷ dns_misses`) と合計・確立の平均
(`connect_ms_sum ÷ timed`) が読めます (エラーも名前解決も無いホストは出しません)。
ヘッダーには動いている版と、**どちらの窓か** (起動から / 通算) が出ます。
外部ライブラリは 1 つも読み込まない 1 ページのままです。ブラウザの無い環境では
`node scripts/check-dashboard.js [/history の出力] [/status の出力] [/profile の出力] [/snapshot の出力]` が
「JS の構文」と「`/history` の配列の配列・`/status` の読み方が実出力と合っていること」を確かめます
(Node があるときだけの補助的な確認。引数を省くと `scripts/testdata/` の見本を読みます)。
最後に**匿名化したデプロイ先の実データ** (`scripts/testdata/deployed-2026-09-16.anon.json`。T.14.35) でも
同じ読み方を回すので、本物の分布 (ホスト 817 件・1,440 標本) で壊れたらここで気づきます。
**`dashboard.html` の大きさの上限は 96 KiB** です (T.28.25 で 80 KiB から上げた。起動のカードを足すと余白 1,405 B を越えたため。inspect と同じ上限)。それより前の経緯: (T.15.0 (14) で下の 5 枚を足して 81,673 B。T.21.5 で重複したコメントを
削り、「最近のエラー」の回数と最後の時刻を足して 80,515 B。`node scripts/check-dashboard.js` が見張ります。外部ライブラリを
読み込まない 1 ページという方針を守るための歯止めなので、超えたら中身を削るか上限を上げるかを先に決めてください。T.28.25 の後は 82,762 B、
T.29.27 (設定の帯の読めない値と知らない鍵・版の隣のビルド `(release)`・`/slo` の tooltip) の後は 85,222 B で、**余白は 13,082 B です**)。

**次の判断に要る 5 枚 (T.15.0 (14))**。どれも**新しい `fetch` を 1 本も増やしていません** (既にある
2 秒 / 5 秒 / 30 秒の取り直しに相乗り) し、**欄を持たない版の応答では「記録していません」と断って
空のまま描きます** (`--lite` も同じ)。

| カード | 読む口 |
|---|---|
| **CPU の絞り** (KPI + 図) | `/status` の `kernel.cgroup_cpu`、`/history` の `kernel` |
| **CPU を使っているスレッド** (プロファイルの節の表) | `/profile` の `threads_top` |
| **動かないトンネル** (「いまの接続」の下の表) | `/connections` |
| **名前解決の内訳** | `/status` の `dns`、`/history` の `dns_warm` |
| **受付待ち** | `/profile` の `stages.connect` の `queue` と `run_delay_us` |
| **キャッシュ命中率** (KPI) | `/status` の `cache.hits` / `cache.misses`。**母数 (`n=`) を添え、1 日あたり 100 本未満なら率は「–」** (T.29.19。要約 (`snapshot-summary.py`) の「キャッシュ」の行も同じ) |
| **利用者が待つ時間 p50** (KPI + 図) | KPI は `/recent?rtt_client=lt100&n=500` (近い接続元。T.29.18。30 秒ごと)、無い版は `/status` の `recent_quantiles.wait`。図は `/history` の `wait_*` |

それぞれの**欄**と、**無い版のふるまい**:

- **CPU の絞り** (KPI + 図) — `nr_throttled` ÷ `nr_periods` を % で (**札のとおり cgroup の通算**で、プロセスが起きる前を含みます)、下に直近 5 分・待たされた合計 (`throttled_usec`)・`quota_cores` (null なら「割り当て なし」)・**`since_start` (起動からの増分)**・`path`。図は `cpu_nr_periods` / `cpu_nr_throttled` を周期 / 秒で 2 系列
  - 無い版のふるまい: `kernel.cgroup_cpu` か **分母の `nr_periods`** (T.15.0 (6) で足した欄) が無い版と、cgroup v2 の読めない環境では KPI が「–」
- **CPU を使っているスレッド** (プロファイルの節の表) — `tid` (= `/connections` の `tid` と同じ番号)・`comm`・役割・CPU %・走っていた標本数。直近 5 分を **tid で束ねて**上位 8
  - 無い版のふるまい: 列を持たない版では 0 行
- **動かないトンネル** (「いまの接続」の下の表) — **`idle_secs` が 300 秒以上の CONNECT** だけを、止まっている長い順に 20 本。`target` `age_secs` `idle_secs` `bytes` `half_closed` (+ 秒) `spins` `revents` (client / origin) `tid`。下の行に「空回りの合計」と「1 度も空回りしていない本数」(**0 のままなら待っている・増え続けるなら回っている**)
  - 無い版のふるまい: `idle_secs` を持たない版では 0 件
- **名前解決の内訳** — warm な名前の数の推移 (図) と、ミスの種類別の横棒 1 本 (`misses_by_kind` = `cold` / `expired` / `warm_stale` / `negative` の**起動からの通算**。画面では差分を溜めません)。KPI の下には `refresh_failures` / `refresh_late` / 引き直し 1 回の平均も出ます
  - 無い版のふるまい: `misses_by_kind` を持たない版では横棒を出さない
- **受付待ち** — `queue` の段を **12 段の区間**に開いた横棒 1 本 (二峰ならワーカー待ちで待たされています) と、件数・平均・p50 / p95 / 最大。下は `run_delay_us` (全役割の合計) を us / 秒 の折れ線で
  - 無い版のふるまい: `run_delay_us` が `null` (schedstat の読めない環境) の標本では折れ線が切れる
- **利用者が待つ時間 p50** (KPI + 図) — `queue + client_read + dns + connect` の和 (T.15.0 (2))。**T.29.18 から KPI の数字は
  近い接続元 (接続元の RTT < 100 ms) の成功した CONNECT だけ**で出し (`/recent?rtt_client=lt100` の個票の実測の分位点)、下に全体
  (遠い接続元を含む) の p50 を添えます (NAT の向こうの 2 集団を混ぜた値は、近い利用者の待ちとして読めないため。`rtt_client` を知らない版では今までどおり)。札に本数・下に p90 / p99 / 最大。**まだ外にあるのは accept から要求行が届くまでと、200 の後の `first_relay`**
  - 無い版のふるまい: `recent_quantiles.wait` が無ければ `/history` の `wait_*` の区間の補間に落ち、それも無ければ「–」

**`/inspect` は「調査」ページ**です (`/dashboard/inspect` も同じもの。T.14.8)。`/dashboard` が「いま」を見る画面なのに対して、
こちらは**起きたことを時間軸で読む**ための別のページで、外部ライブラリなしの 1 ページのままです。
**大きさの上限は 96 KiB** です (T.14.8 は 64 KiB でしたが、T.14.44 で「今日」「今週」「出来事と異常」の
3 枚を足して **58,076 B** になり、64 KiB までの余白が 7 KiB しか残らなかったため上げました。
外部ライブラリを読み込まない 1 枚という方針は変えていません。`node scripts/check-dashboard.js` が見張ります)。
更新は**個票 (`/status` `/recent` `/events` `/history?summary=1`) が 10 秒ごと、表と散布 (`/bursts` `/clients`
`/hosts` `/hosts/series` `/daily` `/snapshots`) は 30 秒ごと**です (`/hosts` は上位 200 まで。
`/daily` と `/snapshots` は日に 1 回しか変わらないので重い方に置いています。開いたままにしても監視より重くならないように)。
描くのは 9 枚:

- **タイムライン** (`/recent`): 横 = 時刻、縦 = 接続元 (宛先にも切り替えられます)、線 1 本が接続 1 本で、
  長さ = 寿命・色 = 閉じた理由 (9 種。`error:<原因>` は 1 色に畳みます)・太さ = 運んだバイト。
  範囲は 1 時間 / 6 時間 / 24 時間 (`/recent?since=`)。**`/events` の出来事**は縦の破線の印で重ねます
  (種類が増えても既定の色で描くだけなので壊れません)
- **出来事と異常** (`/events?n=200`。T.14.11 / T.14.23): タイムラインに重ねた印と同じ出来事を、
  **新しい順の表**で読みます (印は `eventMarks`、表は `eventRows` で別の関数です)。`anomaly` は色を変え、
  **`cleared: <種類>` は立った出来事と対にして**続いた長さを両方の行に出します
  (対の見つからない異常は「まだ続いています」)。知らない綴りが増えても既定の色で並ぶだけです
- **起動からの窓**: `/history?since=restart&summary=1` (T.14.24) を 1 要求で読み、「起動から」と「通算」を
  切り替えます。`?summary=1` を持たない版や `--lite` では `/history` の標本を `since_start_secs` で切って
  手元で畳みます (どちらでも同じ 1 行になります)
- **今日** (`/daily?n=14`。T.14.20): 日別の要求・転送・CONNECT・p50 / p95・名前解決のミス率・ミス 1 回・
  エラー・山・RSS・版の表 (新しい順) と、いちばん新しい日の主な数字。`/daily` は**終わった日**しか書かないので、
  今日ぶんは上の「起動からの窓」で読みます。「見ていた」の列は `secs ÷ 24 時間` で、
  再起動した日は 100% に届きません。**この口を持たない版と `--lite` では「記録していません」**と出ます
- **今週** (`/daily` の 7 行 + `/snapshots`。T.14.40 / T.14.34): サーバーに `weekly-report.py` は無いので、
  **`/daily` の 7 行をこのページの中で足して**要求・転送・CONNECT・エラー (と率)・名前解決のミス率・
  ミス 1 回 (ミスの数で重みを付けた平均)・最大同時・山・RSS を出します。**p50 / p95 は日をまたいで
  足せない** (区間ヒストグラムが `/daily` に無い) ので、日ごとの値をそのまま並べて幅だけ書きます
  (`weekly-report.py` と同じ判断)。各日の行には**その日の雪像** (`/snapshots`。T.14.34) へのリンクが付きます
- **遅い接続 (上位 50)** (`/recent?sort=slow`): 段階 (`queue` / `client_read` / `dns` / `connect` / `first_relay`。T.14.3) の
  積み上げ横棒と、**確立 − RTT** の列 (T.14.5。RTT では説明できない待ちがどれだけかを 1 列で読むため)。
  宛先は `/explain?host=` (T.14.36) へのリンクです (1 相手ぶんを 1 枚で読む口)
- **山の写真** (`/bursts`): 1 枚ずつ、接続元別・宛先別・状態別・種類別の内訳と、そのときのスレッド / fd
- **接続元** (`/clients`): `User-Agent`・宛先の種類・IP リテラル宛て・RTT・初回 / 最終
  (RTT は標本が無ければ「–」です)
- **RTT × 接続時間の散布** (`/hosts`): x = カーネルの平滑化 RTT の平均、y = 接続 1 回の平均
  (`connect_ms_sum ÷ timed`)。対角線 (y = x) から上に離れた点が「RTT では説明できない待ち」で、
  右の表に差の大きい順で 12 件出ます (`rtt_ms` が `null` = 標本 0 のホストと、接続を 1 度も測っていない
  ホストは描きません。接続の平均が 0 ms = 1 ms 未満のホストは y = 0 に描きます)
- **ホスト別の折れ線** (`/hosts/series?top=8`。T.14.22): この口を持っている版でだけ出る枠です
  (無ければ枠ごと出しません)
- (ページの先頭に `/snapshot` と `/daily` `/snapshots` へのリンクがあります。個票を 1 要求で持ち帰るときはそちら)

描画関数 (`timeline` / `slowRows` / `burstCards` / `clientRows` / `rttScatter` / `sinceStart` / `eventMarks` /
`seriesLines` / `dailyRows` / `weeklyRows` / `eventRows`) は DOM に触らないので、
`node scripts/check-dashboard.js` が `scripts/testdata/snapshot-local.json` (手元のベンチで取った
`/snapshot` の実出力。宛先は `127.0.0.1` と `localhost` だけです) と
`scripts/testdata/history-summary.json` (同じく `?summary=1` の実出力)、
それに渡せばデプロイ先の `/status` `/history` でも回します。
「今日」「今週」「出来事と異常」の 3 つは、`/daily` と `/snapshots` の作り置き 9 日ぶん・
`snapshot-local.json` の本物の `/events` (異常入り)・**匿名化した実データ** (T.14.35。この版の雪像には
`/daily` も `/events` も無いので**「無い版」の分岐**を実データで通し、`/history?res=3600` を UTC の日で
束ねた週の足し算が `weekly-report.py` (T.14.40) の週の数字と一致することも見ます) の 3 つで回ります。

**`/probe.html` は「端末から測る」ページ**です (T.14.33)。プロキシ側の計測は「プロキシに届いてから」しか見えないので、
**利用者のブラウザから**測ってプロキシ側の数字と突き合わせるための 1 ページ (外部ライブラリなし、64 KiB 以下) です。
開くと 3 つを順に測ります (回数は 3 / 5 / 9 回。読むのは**中央値**です — 1 回目は接続の確立を含んで遅く出るため):

1. **この端末 → プロキシ**: `fetch('/status', {cache:'no-store'})` を n 回。往復の中央値が
   「端末 → プロキシの RTT + `/status` を組んで返す時間」です (応答の頭が来るまでの時間と、読んだ文字数も出します)
2. **プロキシ経由の取得**: `http://` の小さな URL (既定は `http://example.com/`) を n 回
   (`mode: no-cors` なので**中身は読みません**。測るのは時間だけです)
3. **プロキシ側から見たこの端末**: `/clients` の自分の行 (T.14.7) と `rtt_ms` (T.14.5) を並べ、
   (1) の往復との差 (往復 − カーネルの RTT) を出します

**「このブラウザはこのプロキシを経由しているか」の判定は、プロキシの記録で見ます**:
このプロキシは**自分宛ての要求 (`/status` など) を `clients[]` に数えない**ので、(2) の前後で `/status` の
`clients[]` を比べて、**自分の行の `requests` が増えていれば「経由している」**、**1 行も増えていなければ
「経由していない」** (ブラウザが自分で取りに行った)、**取得が 1 回も成功しなければ「判定できない」**です。
**自分の IP は増えた行から分かります** (増えた行が無いときは `/clients?sort=recent` の「最終が今」(10 秒以内) の
行を見当にします。同じ時間に他の接続元も動いていたときは、いちばん増えた行を採ったうえで「他にも増えた行があった」と
断ります)。「経由していない」と出たら、ブラウザのプロキシ設定か `/proxy.pac` を入れてもう一度測ってください。

**測った値はサーバーへ送りません** (端末の中だけです。このページがプロキシに出す要求は `/status` と `/clients`、
それに (2) の URL の取得だけで、結果を書き戻す口はありません)。`rtt_ms` は**接続を閉じたときに 1 本ぶん**読んだ
カーネルの平滑化 RTT (T.14.5) なので、標本が無ければ「–」です (このページを開いている接続はまだ閉じていません)。
`--lite` でも 200 で開けますが、接続元を記録していないので (3) は空、(2) は判定できません。
関数 (`median` / `pick` / `render`) は DOM に触らないので、`node scripts/check-dashboard.js` の 11 番目の検査が
作り物の数列と `/snapshot` の中の `/status` の実出力で通します。

`/status` のトップレベルの `version` には動いているバイナリの版が出ます (`-V` と起動ログと同じ文字列)。
末尾の `build` はビルドの種類 (`release` / `dist` / `debug`。T.28.42) です。

**`/status` は 2 つの窓が混ざっている**ので、どちらの窓かが分かるように目印を出します:
`since_start_secs` から下 (`total_requests` / `bytes_forwarded` / `cache_hits` …) は**この起動から**、
`hosts[]` と `clients[]` は状態ファイルに残る**通算**で、`restored_since` がその通算の始まり
(最も古い `last_seen` の epoch 秒。`0` なら表が空) です。あわせてプロセス全体の数え物
`threads` (`/proc/self/status` の `Threads`。接続スレッドだけを数える `live_threads` とは別で、
監視・履歴・接続試行のスレッドも入ります) と `fds` / `max_fds` (`/proc/self/fd` の数と `RLIMIT_NOFILE`) も出ます。
`/proc` を読むのは **`/status` と `/metrics` に来たときと 5 秒ごとの履歴の標本のときだけ**です。
ただし **`boot`** (`id` = `/proc/sys/kernel/random/boot_id`、`pid` = このプロセスの PID) は
**起動に 1 回だけ**読みます (値はプロセスの生存中に変わりません。読めない環境では `id` が `null` で、
`/status` は落ちません)。**`id` が前の起動と変わっていればホストの再起動、同じならプロセスだけの再起動**
なので、`/events` の `start` の直前に `shutdown` が無い起動 (無言の再起動) をこの 2 つに切り分けられます
(T.24.0。`/events` の `start` の文面にも `boot` の先頭 12 文字が入ります。1 件 128 B の枠のため)。

`/status` には上限といまの混み具合も出ます: `max_conns` / `max_threads` (`auto` で決まった値。`0` は無制限)、
`live_threads` (生きている接続スレッド) / `idle_threads` (そのうち仕事待ち) / `queued_jobs` (上限に当たって
待たせている仕事。捨てていません)。`auto` が何を選んだかは起動ログを見なくてもここで分かります。
同じ 5 つは `/metrics` にも gauge で出ます (`sorahost_max_connections` / `sorahost_max_threads` /
`sorahost_threads{state="live"|"idle"}` / `sorahost_queued_jobs`)。
**接続スレッドの 2 つはラベル付きの 1 系列にそろえました**。旧名 `sorahost_live_threads` /
`sorahost_idle_threads` も**残してあります** (はじめは 1 版だけのつもりでしたが、消す理由が無いので両方出し続けます。T.28.23)。新しく組む監視はラベル付きの方を使ってください。
`/metrics` にはこのほか `sorahost_connect_seconds`(`_bucket{le=}` / `_sum` / `_count`。CONNECT 確立の
ヒストグラム。区間は `/history` と同じ 12 段。**繋がった CONNECT だけ**を数えます — T.20.4。`0.18.0` までは失敗した 1 本も入っていました)、`sorahost_dns_seconds_sum` / `_count` (名前解決のミスに
かかった時間)、`sorahost_errors_total{cause="dns|refused|unreachable|timeout|reset|tls|loop|other"}`、
**`sorahost_rejected_requests_total{reason="request_line|header_too_large|method|no_host|bad_uri|body_framing|obs_fold|dup_host|bad_header|header_timeout"}`**
(読めずに 400 / 408 / 414 / 431 で断った要求。10 本で固定。T.14.28。`header_timeout` は T.29.10)、
`sorahost_fds` / `sorahost_max_fds` / `sorahost_process_threads`、
**`sorahost_rtt_seconds_sum` / `_count`** (`{side="client"|"origin"}`。カーネルの平滑化 RTT。
標本は接続 1 本の終わりに 1 つで、ホスト別は出しません) が出ます。

**`sorahost_errors_total` には DNS の失敗の内訳も出します** (T.26.1)。`cause="other"` の**うしろ**に
`cause="nxdomain"` (`EAI_NONAME` など = 引き直しても変わらない) と `cause="dns_temp"` (`EAI_AGAIN` = 引き直せば
変わりうる) が並び、値は `/status` の `errors_by_cause` の 9・10 番目と同じです (系列の名前・HELP・先頭 8 つの
並びは変えていないので、既にある式はそのまま使えます)。`dns` (合計) が動いていても**リゾルバの一時的な失敗なのか、
名前が無いのか**を監視側から切り分けられるようにするためで、まだ 1 件も無い内訳は行ごと出しません
(0 を出すと「見えている」と読めてしまうため。`host_blocked_total` と同じ約束です)。

**起動の切り分けと heap の基準も `/metrics` に出します** (T.25.3)。`sorahost_boot_id_info{id="…"}` は
`/status` の `boot.id` と同じ値 (`/proc/sys/kernel/random/boot_id`) を **info 型** (値は常に `1`) で出したもので、
「ホストが起動し直すと変わる・プロセスだけの起動し直しでは変わらない」はここでも同じに読めます。
**ラベルは `id` の 1 つだけなので系列は 1 本**で、増えるのはホストを再起動したときだけです
(読めない環境では行ごと出しません — 0 や空の `id` を出すと「起動が変わっていない」と読めてしまうため。
`sorahost_kernel_*` と同じ判断)。
**版とビルドの種類も 1 行**: `sorahost_build_info{version="0.29.35+b2b26c1",build="release"} 1` (T.29.19。`/status` の
`version` と `build` (T.28.42) と同じ値の **info 型**。版を入れ替えると古い系列は止まるので、監視から「いつ版が変わったか」が読めます)。
heap は `sorahost_heap_bytes` (いまの値。`/status` の `memory` と同じ `heap_used + mmap − cache_memory`)、
`sorahost_heap_baseline_bytes` / `sorahost_heap_baseline_at_seconds` (起動 60 秒の基準と、それを取った epoch 秒で、
`/status` の `memory.baseline` と同じ値) の 3 本で、**増えは `sorahost_heap_bytes − sorahost_heap_baseline_bytes`** です。
基準の 2 本は、まだ 60 秒たっていない起動・`mallinfo2` が読めない環境・状態ファイルを開いていない
(`PROXY_STATS_PERSIST=off` / `--lite`) では出しません (`/status` が `baseline: null` になるのと同じ判断)。
**Prometheus に出すのは、起動ごとの 60 秒の値が時系列として残るから**です: `/status` は「いまの起動」の
基準しか持たず (前の起動の値は状態ファイルに残っても読まない — T.24.4)、版を上げたときに起動直後の heap が
増えたかを比べられるのは監視側だけです。基準の 2 本だけでは**増え**が読めない (いまの heap が `/metrics` に
無い) ので、`sorahost_heap_bytes` も出しています。
ホスト別の応答時間ヒストグラム (`sorahost_host_request_duration_seconds`) は区間が 24 段になったので
**上位 50 ホストまで**です (数え上げの系列はこれまでどおり上位 100 ホスト)。ダッシュボードの「接続中」にも
`スレッド 7 / 256 (空き 3) · 接続上限 1008` の 1 行が出ます。数えるには接続スレッドで共有している鍵が要るので、
**`/status` と `/metrics` に来たときだけ**数えます (要求ごとの仕事は増えません)。
`cache` の `revalidations_dropped` は、上限に当たって捨てた裏側の再検証の数です
(こちらは「後でやればいい仕事」なので待たせません)。`cache` の `not_stored_rotations` は
「保存されないと分かった URL」の記憶 (ブルームフィルタ) を入れ替えた回数です。

**Grafana で段階と待ちを見る**: `/profile` の段階は `/metrics` にも
`sorahost_stage_seconds{kind="connect"|"forward",stage=...}` の `_bucket{le=}` / `_sum` / `_count` として出ます
(CONNECT 7 段 = `queue` / `client_read` / `dns` / `connect` / `first_relay` / `relay` / `park`、
転送 6 段 = `queue` / `client_read` / `origin` / `send` / `ttfb` / `body`。区間は `/history` と同じ 12 段 + `+Inf` で、
`le` も `_sum` も**秒**)。**累計は起動から**です: Prometheus のヒストグラムは単調増加が前提なので、
`/profile` の窓 (1 時間 / 1 日で古い標本から落ちる) の和ではなく、起動からの累計を別に持っています
(再起動で 0 に戻るので、`rate()` / `histogram_quantile()` はそのまま使えます)。段階が累計に入るのは
**5 秒ごと**なので、最後の 5 秒ぶんは次の収集で入ります。**`--lite` では段階を測っていないので 1 系列も出ません**
(`sorahost_kernel_*` と同じ判断で、源が無いときに 0 を出しません)。**ホスト別には出しません**
(系列が増えすぎるため。ホスト別は `/hosts` と `/status` の `hosts[]` で見ます)。
あわせて `sorahost_warm_names` (keep-warm で裏から引き直し続けている名前の数。gauge) と
`sorahost_lock_contention_total{lock="stats"|"dns"|"park"|"workers"}`
(そのロックを取るときに**本当に待たされた**回数の累計。counter) が出ます。
canary の `sorahost_canary_seconds{stage="dns"|"connect"|"ipv6_connect"}` (最後の値。
`ipv6_connect` は**繋がった回だけ**出ます = 行が消えていること自体が「IPv6 が死んでいる」の印) と
`sorahost_rtt_seconds_sum` / `_count{side=}` は前からあるものです。
段階の 13 本で `/metrics` は **約 14 KB 増えます** (ホスト表と接続元表が満杯のときで 243 → 257 KB。上限は 400 KiB)。

`/status` の **`rejected_requests`** は、**要求を読めずに断った数**を理由別に並べた欄です (T.14.28)。
`{"request_line":0,"header_too_large":0,"method":0,"no_host":0,"bad_uri":0,"body_framing":0,"obs_fold":0,"dup_host":0,"bad_header":0,"header_timeout":0,"total":0}` の形で、
理由は上の `/errors` の `bad_request:<reason>` と同じ 10 種 (`header_timeout` = 408 は T.29.10 から)、`total` はその合計です。公開ポートには走査 (scanner) の
要求が来るので、**何が来たか**をこの 1 行で読むためのものです。エラー (5xx) ではないので `errors` /
`errors_by_cause` には混ぜていません。**数えるのは断る経路だけ**で、通した要求は原子を 1 つも触りません。
誰がいつ送ってきたかは `/errors` の個票 (`cause` が `bad_request:<reason>`) で読めます。
ログの WARN は理由ごとに 1 分に 1 行です (T.28.9。上の「ログ」の節)。
この 2 つを足すときに応答が 2 か所だけ変わりました: **壊れた要求行**には今までは何も返さずに閉じていましたが
`400 Bad Request` を返してから閉じます (返した状態コードを個票に正しく書くため)、
**`Content-Length` と `Transfer-Encoding: chunked` が両方ある要求**は今までは chunked として中継していましたが
`400` で断ります (RFC 9112 §6.1 は中継してはならないと書いています。前段と後段で本文の切れ目が食い違う
「要求の密輸」を通さないため)。判定はどちらも既にある分岐の中なので、通る要求の費用は変わりません。
**`Host` が 2 本以上ある要求**も `400` で断ります (T.19.2。RFC 9110 §7.2。今までは**最後の 1 本を黙って採って**
200 で通し、オリジンには要求先から作り直した `Host` を 1 本だけ送っていました = 前段と後段で「どの `Host` を
信じるか」が食い違う形)。印は 2 本目を見たときだけ立つので、通る要求の費用はこちらも変わりません。

`/status` の `hosts` にはホスト (`scheme://host:port`、CONNECT は `connect://host:port`) ごとの要求数・ヒット・ミス・
バイパス・エラー・バイト数が要求数順に最大 50 件入ります (1000 ホストを超えた分は `other` にまとめます)。
バイト数は合計 (`bytes`) のほかに**向き別の `bytes_in` / `bytes_out`** が並びます (T.14.26。`bytes_in` =
クライアント → オリジン = 上り、`bytes_out` = オリジン → クライアント = 下り)。上り主体の相手 (ログ送信) は
「利用者の回線の上り」が律速で、プロキシでもオリジンでもない — その切り分けに使います。
**CONNECT は `bytes == bytes_in + bytes_out`** ですが、**forward の `bytes` は今までどおり応答のぶんだけ**なので
(欄の意味は変えていません)、要求本文のある相手では `bytes_in + bytes_out` の方が大きくなります。
数え直しはしていません (中継が方向ごとに持っている値と、要求本文 / 応答のバイトをそのまま渡すだけです)。
**`?sort=requests|errors|dns|slow` でこの 50 件の切り出し方を変えられます** (既定は `requests` = 今までどおりの要求数順。
`errors` はエラー件数、`dns` は名前解決に費やした合計 `dns_ms_sum`、`slow` は応答 (CONNECT は確立) の平均 `avg_ms` の
多い / 遅い順。**知らない値は既定に倒します**)。JSON の形も件数も変わらず、変わるのは `hosts[]` の並びだけです
(`clients[]` は要求数順のまま、`/healthz` は問い合わせを読みません)。要求数の上位 50 には「悪いホスト」が出てこない
のが動機で、デプロイ先の実測 (58.6 時間) では**エラー 99 件のうち 80 件 (名前解決の失敗) を抱えたホストが
1 つも上位 50 に居ませんでした**。`curl "http://127.0.0.1:8080/status?sort=errors"` で見られます。

**`?hosts=0` で `hosts[]` を丸ごと省けます** (T.25.1)。既定 (書かない・`1`・知らない値) は今までどおりで、
消えるのは `hosts` の鍵だけで、**ほかの鍵も `schema` も 1 バイトも変わりません**。`/status` の応答は
`hosts[]` が 8 割を占め (2026-10-23 の実測で 33,280 B のうち 26,917 B。手元の模型 50 宛先でも
25,182 B のうち 19,351 B)、**行は 50 件で頭打ち**なものの、1 行あたり最大 538 B
(エラーや名前解決の欄が埋まった行) まで太ります。30 秒ごとに引く監視ではこの `hosts[]` が効いてくるので、
**ポーリングする監視は `curl "http://127.0.0.1:8080/status?hosts=0"` の軽い形を使ってください**
(同じ模型で 5,831 B = 77% 減)。`hosts[]` の中身が要るときは `/hosts` を `sort=` `limit=` `offset=` で
別に引けます。1 要求で全部取る `/snapshot` の `parts` と各部の形は今までどおりで、この問い合わせの
影響を受けません。
ホスト別の行にはさらに**待ちの内訳**が入ります: `dns_ms_sum` / `dns_misses` (名前解決を OS に聞いた合計時間と回数)、
`connect_ms_sum` (接続にかかった合計時間。名前解決のぶんは含みません)、`v4_wins` / `v6_wins` (確立した族)、
`errors_by_cause` (`[dns, refused, unreachable, timeout, reset, tls, loop, other, nxdomain, dns_temp]` の順の件数。
**先頭 8 つは今までどおり**で、9・10 番目が `dns` の内訳 (`nxdomain` = 名前か住所が無い =
引き直しても変わらない / `dns_temp` = 一時的な失敗 = 引き直せば変わりうる。`dns` は合計のまま
残るので `dns == nxdomain + dns_temp`)。T.25.0 より前の雪像は 8 要素なので、9・10 番目が
無ければ `dns` の合計だけを読んでください。
`loop` は自分の `Via` が付いて `508` で閉じたもの。**複数の候補を並行に試して (Happy Eyeballs)
締め切りまで 1 本も確立できなかったときは `timeout`** です — 2026-09-18 より前は、その間に 1 本だけ先に
返ってきた `ECONNREFUSED` などが最後のエラーとして残り、締め切りいっぱい待った接続が `refused` に
数えられていました。T.15.0 (3)。**オリジンの応答を `PROXY_TIMEOUT_SECS` 待っても先頭が届かなかったときも
`timeout`** で、ログは `origin response timed out after N s` です — 0.27.8 までは Linux の締め切りが返す
`EAGAIN` を `other` に数え、`failed to read origin response: Resource temporarily unavailable (os error 11)`
と出していました。T.28.5)、**`rtt_ms`** (`{"avg":…,"min":…,"samples":N}`。
カーネルの平滑化 RTT (`TCP_INFO`)。標本は**接続 1 本の終わりに 1 つ**なので `timed` (要求数) とは数が合いません。
1 本も閉じていなければ `null`) と **`retrans`** (その接続たちが再送したセグメントの通算)、
**`sni_mismatch`** (そのホスト宛ての CONNECT で、宛先のホストと覗いた SNI が食い違った本数。
T.14.38。**メモリだけ**なので再起動で 0 に戻ります。合計は `/status` の末尾の `sni_mismatches`)、
**`syn_retrans`** (そのホストへ繋ぐときに **SYN を送り直した回数の合計**。T.14.46。確立の直後に
`TCP_INFO` を 1 回読んだ値で、`retrans` (接続の終わりに読んだ通算) と違って**確立の失敗だけ**を数えます。
1 回で確立が約 1 秒、2 回で約 3 秒よけいにかかるので、`avg_ms` が飛んでいるホストはここを見てください。
**メモリだけ**なので再起動で 0 に戻ります。合計は `/status` の末尾の `syn_retrans_total`)。
**測るための費用は熱い経路に乗せていません**:
名前解決の時計はキャッシュを外したときだけ読み、内訳はホスト別統計が既に取っている鍵の内側で足します
(原子操作もシステムコールも増えません。実測: forward の確保 8.03 → 8.03 回/要求、
`--lite` のシステムコール 5.00 → 5.00 回/要求)。`dns` には `miss_ms_sum` / `miss_avg_ms` (ミス 1 回の値段) と
`refreshes` (期限前に裏で引き直した回数) / `negative_ttl_secs` (失敗を覚えておく秒数) /
`warm_secs` (keep-warm の窓) / **`warm` (いま warm な名前の数)** /
**`changes` (引き直しで答えの集合が変わった回数の合計)** が出ます。
**裏の引き直しはミスに数えません** (利用者は待っていないので、`misses` と `miss_avg_ms` に混ぜると
「ミス 1 回の値段」が読めなくなる)。`/metrics` では `sorahost_dns_lookups_total{result="refresh"}` です。
さらに **`misses_by_kind`** (ミスの内訳。`cold` = 表にまだ答えが無い / `expired` = 答えは持っていたが
TTL を過ぎた / `warm_stale` = **warm なのにミスした** = 裏の引き直しが間に合っていない /
`negative` = 覚えている失敗の期限が切れたので引き直した。**4 つの和は必ず `misses` と一致します**) と、
裏の引き直しの様子 **`refresh_failures`** (引き直しが失敗した回数) / **`refresh_ms_sum`** /
**`refresh_ms_max`** (引き直しの `getaddrinfo` にかかった時間の合計と最大 ms) /
**`refresh_late`** (予定の時刻から **5 秒以上**遅れて始まった引き直しの回数。引き直しは
`dns-refresh` スレッド 1 本が順にやるので、1 回 約 2 秒かかる名前が続くと後ろが詰まります。
要求の経路が頼んだ先回りは予定の時刻を持たないので数えません) が出ます。
末尾の **`warm_evicted`** (T.16.0) は warm の枠 (同時に 32 件) が満杯で、新しく warm になる名前のために
**最後の使用がいちばん古い名前を外した**回数の起動からの通算です (`/history` の `dns_warm` は 5 秒ごとの瞬間値なので、
枠が埋まって押し出しが起きていたかはこれで読みます)。**要求の経路の例外**: 数えるのはその名前が窓の中で 2 回目に
使われ、かつ枠が満杯のときの枝だけで、既に握っている鍵の内側の原子の加算 1 つです (システムコールも確保も増えません)。
**読み方**: `expired` が主なら窓 (`PROXY_DNS_WARM_SECS`) の外で期限が切れている =
窓を延ばすと効く、`warm_stale` が出ていれば引き直しが間に合っていない = 窓ではなく
`refresh_failures` / `refresh_late` を先に見る、という順で読みます (どちらに手を入れるかは
この内訳でしか決まりません)。
`clients[]` にも同じ `rtt_ms` / `retrans` が出ますが、**こちらはクライアント側** (利用者 → プロキシの往復) です。
`bytes_in` / `bytes_out` も同じ向き (`bytes_in` = その端末が上げた量) で出るので、「この端末は上りが主か下りが主か」が読めます。
`/status` の `clients[]` には接続元 IP ごとの要求数・転送量・拒否数・応答時間が要求数順に最大 50 件入り、
末尾に **`first_seen`** (初めて見た時刻。`0` = この起動より前から居る) / **`agent`** (最後に見た `User-Agent` 1 つ。
無ければ `null`) / **`distinct_targets`** (宛先ホストの種類) / **`literal_targets`** (IP リテラル宛ての要求数) が付きます
(既存の欄の順は変えていません)。`User-Agent` を読むのは**接続の最初の要求だけ**で、2 要求目からは旗で飛ばすので
要求ごとの費用は 0 です (`--lite` では読みません)。全接続元と残りの欄は `/clients` で見られます。
`/metrics` も同じ内容を `sorahost_*` 系列で出します。`origin_connections` にオリジンへの新規接続数と再利用回数、`cache` には各層の `used_bytes` / `limit_bytes` (現在の予算) / `reserved_bytes` (バラスト) /
`keep_free_bytes` (動的マージン) / `mode` (`auto` か `fixed`) と、`system` に直近の計測値 (メモリ総量と空き、
活性ページキャッシュ、cgroup 制限と使用量、PSI の有無、ディスク総量と空き、自プロセスの RSS) が入ります。

## 運用 (置いた先での手順と、そのあと見るもの)

新しい版を置いたあと**いつ何を見ればよいか**を 1 か所にまとめたものです。
手順の決まりは `TODO.md` の §0 の 9 (再デプロイの直前と直後に雪像を撮る) と `TASKS.md` の「決まり」(版・タグ・締め)、数字の出どころと期待値は締め (`tasks/TNN.md` の `Tn.99`) の結果にあります。

### 0. リリース (版をそろえてタグを打つ)

版は**そのクレートの中身を最後に変えたタスクの番号** `0.<タスク>.<番号>` です (T.21.6。2026-10-23 に T.26.3 の「デプロイの番号」を撤回して戻しました)。
小タスクを済ませるたびに、そのタスクの番号で中身が変わったクレートだけを上げ、本体と `proxy-server` は全クレートの最大にそろえます。
**デプロイの版 (= タグ) はデプロイに入れた中でいちばん大きいタスクの番号**です。

```bash
scripts/crate-versions.py --check            # 0 なら決まりに合う (本体 = 全クレートの最大、タグの上では本体 = タグ、Cargo.lock = Cargo.toml)
scripts/crate-versions.py --task T.27.8      # 分かれ目から中身が変わったクレートを 0.27.8 にし、本体を最大に (Cargo.lock も)
scripts/crate-versions.py --list             # クレートごとに 今の版・中身を最後に変えたコミット・版の行を書いたコミット
```

**タグは `release.yml` の `workflow_dispatch` で打ちます** (2026-10-24 から)。GitHub の Actions の画面 (か GitHub の API・ツール) で
`release` を **入力 `version` = 本体の版 (`0.27.8` のように `v` は付けない)** で起動すると、workflow の中で本体の版と
`crate-versions.py --check` を確かめ、注釈付きのタグ `v<版>` を `main` の先頭に打って push し、そのタグで x86_64 と aarch64 を
`dist` プロファイル (fat LTO) でビルドして release に載せます。すでに同じタグがあれば打ち直さずに落ちます。
**入力 `ref` (任意。既定 `main`) でタグを打つコミットを選べます** (T.29.30): 短いハッシュ (`ab12cd3`)・完全な SHA・
ブランチ名・`main~1` のどれでもよく、workflow の中で `git rev-parse` が 1 つのコミットに解きます (前は `actions/checkout` に
そのまま渡していたので短いハッシュでは落ちました。T.28.99 の 1 回目)。**`main` の上に無いコミットは打たずに落ちます**
(置いた先は `main` の先頭でビルドしていることがあるため)。打ったコミット・`main` の先頭・その差は step summary の表に出ます。
置いた先で**ソースから作るなら `git checkout vX.Y.Z` してからビルド**してください (`main` の先頭はタグより先に進んでいることがあり、
`-V` の `+<コミット>` がタグと違って見えます。0.28.39 は `+a84923a`、タグは `a01dd0c` でした)。
手元から `git tag -a vX.Y.Z && git push origin vX.Y.Z` しても同じ release が走ります (`push: tags` の口)。
タグは**デプロイの合図の直前**に打ちます (前に打って取り消したことがあるため)。

`ci.yml` と `release.yml` は同じ `--check` を回し、合わなければ名前と理由を出して落ちます。

### 0.5 置く (release のバイナリを使うとき)

```bash
V=0.27.8; T=x86_64-unknown-linux-gnu         # aarch64 なら aarch64-unknown-linux-gnu
curl -fLo rust-http-proxy "https://github.com/takano32/rust-http-proxy/releases/download/v$V/rust-http-proxy-$T"
curl -fLo rust-http-proxy.sha256 "https://github.com/takano32/rust-http-proxy/releases/download/v$V/rust-http-proxy-$T.sha256"
sed "s/ .*/  rust-http-proxy/" rust-http-proxy.sha256 | sha256sum -c   # v0.27.8 までの release には .sha256 がありません
chmod +x rust-http-proxy
./rust-http-proxy -V                         # rust-http-proxy <版>+<コミット> (dist) と出ることを確かめる
./rust-http-proxy --check; echo "exit=$?"    # 下の 1 へ
./rust-http-proxy -p "$SERVER_PORT"          # Pterodactyl では SERVER_PORT が割り当てのポート
```

release のバイナリは ubuntu-22.04 (glibc 2.35) でビルドしているので、それより新しい glibc の環境で動きます。
ソースからビルドするときは `cargo build --release` (`release` プロファイル。fat LTO の `dist` よりビルドのメモリが少なくて済み、
速さは数 % 劣ります。「ビルド・テスト」の節)。

### 1. 置く前 (`--check`)

**止める直前に `scripts/collect-deployed.sh` で雪像を 1 枚**撮っておきます (次の版の判定の「前」になります。T.17.19)。
撮り忘れても、**プロキシ自身が停止シグナルで `<UTC 時刻>-shutdown.json` を 1 枚残します** (T.28.20。`recent` は全部入り)。
再デプロイのあとに `scripts/collect-deployed.sh --from-server <host>:<port>` で取り寄せると、`BEFORE=…` の 1 行が出ます。
**「前」は時刻ではなく版で選びます** (T.29.26): いまの版と**系列 (`0.<Phase>`) の違う版の最後の停止**です。同じ系列の
途中のデプロイ (T.28.99 で 53 分挟んだ 0.28.39 など) の停止は飛ばし、候補を全部、外した理由つきで並べます。

```bash
# 起動せずに「この環境で何が読めるか」と「効いている設定」を印字して終わる
./rust-http-proxy --check; echo "exit=$?"
```

`capabilities` の 9 行のうち**終了コードに入る 7 行**が `[ok]` で終了コードが `0` なら、
`/profile` (`proc_syscall` と `proc_schedstat`)・カーネルの RTT (`tcp_info`)・
CPU の絞りと PSI (`cgroup_cpu` / `cgroup_pressure`)・IPv6 (`ipv6_route`)・
統計ファイル (`home_writable`) が全部読めます (残る 2 行 `resolver_ms` と `cgroup_cpu_path` は
真偽ではないので終了コードに入りません)。
`[NO]` の行があると終了コードは `1` で、最後に `check: N of 7 not readable (...)` と出ます。
起動はできますが、その項目は `/status` や `/history` で `null` になります
(`resolver_ms` は 2 秒で答えが来なければ `[--]` です)。`settings` の一覧は `source` (`default` /
`env` / `env_file` / `cli`) つきなので、**書いたのに効いていない設定**がここで分かります。

### 2. 起動直後 (0〜5 分)

起動から **60 秒たってから** `scripts/collect-deployed.sh` で 1 枚 (0 時間の雪像) 撮り、`/status` の `version` と起動時刻 (UTC)、`boot.id` を `TASKS.md` のそのタスクの `Tn.99` の前提に書きます (T.17.19)。**すぐ撮ると `memory.baseline` が `null` のまま**なので、heap の判定が「判定できず」になります (T.24.4。baseline は起動 60 秒で 1 回だけ書かれる)。
同じ 60 秒に**プロキシ自身も `<UTC 時刻>-start.json` を 1 枚残す**ので (T.28.20)、撮り忘れたら `--from-server` で取り寄せて `ZERO` に渡せます
(`BEFORE=… ZERO=…` の 1 行が出ます。0 時間は**いまの版の最初の起動**の 1 枚。T.29.26)。
前提の 1 行 (版・ビルド・起動時刻・`boot.id`・前の版の停止時刻) は、下の 4 の `--closeout` が要約の頭に組んで出します。

```bash
# 起動ログの 2 行 (待ち受けと統計ファイル)
#   rust-http-proxy 0.29.35+<commit> listening on [::]:8080 (IPv6 + IPv4) (backlog 1024, log level: INFO)
#   state file ~/.rust-http-proxy.rrd (8192 KiB, converted from version 2 in 66 ms): history 720/1440/720 samples, 800 hosts, 2 clients restored
curl -i http://127.0.0.1:8080/healthz     # 200 と "ok":true (503 ならどの検査が偽かが本文に出る)
curl http://127.0.0.1:8080/               # エンドポイントの案内 (この版で読める口の一覧)
curl http://127.0.0.1:8080/config > config-$(date -u +%Y%m%dT%H%M%SZ).json   # そのときの設定を 1 枚残す
```

`backlog` は `PROXY_LISTEN_BACKLOG` の実効値です (既定は `min(1024, somaxconn)`)。
`state file` の括弧が `converted from version 2 in N ms` なら**版 2 の統計を版 3 に詰め直して引き継いだ**ということで、
`created` と出たら引き継げずに作り直しています (通算の統計は消えています)。
隣の `recent file ... (4096 KiB, opened): N closed connections, ... restored` が前の版の個票です。
**起動から 60 秒たつと `/status` の `memory.baseline` に `{"at":…,"bytes":…}` が出ます** (T.24.4。
`heap_used + mmap − cache_memory`)。これがこの起動の heap の基準で、**後の雪像 1 枚で増えが読めます**
(0 時間の雪像は要りません。起動し直すと 60 秒たつまで `null` です)。
**1 時間に 1 回の `malloc_trim` が返したかは `/status` の `memory.trim` で読みます** (T.30.1。
`count` = 呼んだ回数、`skipped` = 1 時間たったが `heap_free ≤ heap_used` で呼ばなかった回数、
`last_at` (epoch 秒)・`last_returned_bytes` (前後の RSS の差)・`total_returned_bytes`・`last_us`、`every_secs` = 間隔)。
info の 1 行 (`returned free heap to the OS`) は `/log` の 1,000 行が warn で埋まると流れて見えないので、こちらが正です。
起動から 1 時間は `count` も `skipped` も 0 で、`--lite` / `PROXY_STATS_PERSIST=off` (`history` スレッドが動かない) では
ずっと 0 と `null` のまま (欄の形は同じ) です。

### 3. 1 時間後

```bash
# ブラウザ: http://<proxy>/dashboard (グラフとホスト別統計) と http://<proxy>/inspect (個票の時間軸)
curl http://127.0.0.1:8080/status | python3 -m json.tool | less    # dns / ipv6 / evicted_idle / recent_quantiles
curl "http://127.0.0.1:8080/dns?sort=misses"                       # warm と next_refresh_secs
curl "http://127.0.0.1:8080/profile?res=5"                         # 待ちの段階・スレッド・ロック
curl "http://127.0.0.1:8080/connections"                           # いま開いている接続 (tid と spins)
```

見るのは `dns.warm` (keep-warm が掴んでいる名前の数。既定 (`PROXY_DNS_WARM_SECS=0`、T.22.4) では常に `0`。窓を設定して `0` なら効いていません) と
`dns.misses ÷ CONNECT の本数`、`ipv6` の `v4_first` (`true` なら IPv6 が落ちている環境と判断済み)、
`evicted_idle` と `rejected_overload` (山が無い時間帯は `0`)、`recent_quantiles`
(直近 1,024 本の**実測**の分位点。区間の補間ではありません)。
`/profile` の `stages` は CONNECT が `queue` / `client_read` / `dns` / `connect` / `first_relay` / `relay` / `park`、
forward が `queue` / `client_read` / `origin` / `send` / `ttfb` / `body` で、**1 本の待ちがどこに消えたか**が読めます。

**T.15.0 で足した欄** (次の判断 — 窓を伸ばすか・空回りが直ったか・確立の尾は誰のせいか — に要るもの):

| 口 | 欄 | 読み方 |
|---|---|---|
| `/status` | `recent_quantiles.wait` | **利用者が待つ時間** (`queue + client_read + dns + connect`)。`connect` は要求行を読んだ後からなので、**基準線はこちら** |
| `/status` | `dns.misses_by_kind` | `expired` が主なら窓 (`PROXY_DNS_WARM_SECS`) が短い、`warm_stale` が出ていれば裏の引き直しが間に合っていない。隣の `refresh_failures` / `refresh_late` / `refresh_ms_max` を先に見ます |
| `/status` | `kernel.cgroup_cpu` | **割合で読む** (`nr_throttled ÷ nr_periods`)。`since_start` はこのプロセスが始まってからの増分、`path` は読んでいる階層 |
| `/status` | `boot` | `id` (`/proc/sys/kernel/random/boot_id`) と `pid`。**前の起動と `id` が変わっていればホストの再起動、同じならプロセスだけの再起動** (T.24.0。読めない環境では `id` が `null`) |
| `/status` | `memory.rings_used` | `rings` と同じ鍵で「いま埋まっているぶん」。`rings` は 0 時間でも 36.6 時間でも変わりません |
| `/profile` | `threads_top` / `run_delay_us` | どの tid が回っているか (`/connections` の `tid` と同じ番号) と、**走れるのに CPU に乗れなかった**時間 |
| `/connections` | `idle_secs` / `spins` / `revents` / `half_closed` | 「回っているのか・待っているのか」。**既定のままの欄は出さない**ので「無ければ既定値」と読みます |
| `/history` | `key_kinds` | `keys` と同じ長さで、列ごとの読み方 (`cumulative` = 通算 / `delta` = 区間 / `gauge` / `peak` / `buckets`) |
| `/healthz` | `checks.cpu` | 直近 5 分に絞られた期間の割合。**起動から 5 分に満たない間は `null`** |

### 3.5 ふだんの見張り (`watch.yml`)

`.github/workflows/watch.yml` が **1 時間に 1 回** (毎時 17 分) `scripts/watch-deployed.py tokyo.sorahost.net:60357` を回し、
`/healthz` が ok でない・直近 75 分に**無言の再起動** (直前に `shutdown` の無い `start`) がある・口が読めない、のどれかで
job を落とします (GitHub が失敗を知らせます。T.28.26)。読むのは `/healthz`・`/events?n=200`・`/status?hosts=0` の 3 つだけで、
**実行ログには結論・版・起動からの時間・落ちた検査の名前しか出しません** (リポジトリは public で、Actions のログは誰でも読めるため)。
別の置き先を見るときは Actions の画面から `workflow_dispatch` の `target` に `host:port` を渡します。手元からは
`scripts/watch-deployed.py HOST:PORT` (CONNECT しか通さないプロキシの内側からは届きません)。

### 4. 24 時間後

```bash
# 雪像を 1 枚取り、前回との差分と判定表つきの Markdown を 1 枚にする
scripts/collect-deployed.sh <host>:<port>
# 判定表の相手を再デプロイ直前の雪像にし、0 時間の雪像 (直後の 1 枚) も渡す (上の 1 と 2 で撮った 2 枚)
BEFORE=status/<前>-snapshot.json ZERO=status/<0 時間>-snapshot.json scripts/collect-deployed.sh <host>:<port>
# 締めを 1 コマンドで (T.29.26): 取り寄せ → 前と 0 時間を版で選ぶ → 上の 1 回 → 要約の頭に前提の 1 行
scripts/collect-deployed.sh --closeout <host>:<port> status/tokyo
curl http://127.0.0.1:8080/snapshots    # プロキシ自身が 1 日 1 回残した雪像 (30 日ぶん)
curl "http://127.0.0.1:8080/daily?n=365"  # 1 日 1 行の要約 (永久)
curl "http://127.0.0.1:8080/slo?days=7"   # しきい (PROXY_SLO) を満たした時間の割合
```

`collect-deployed.sh` は `/snapshot` を 1 回で取って保存し、前回の雪像があれば
`snapshot-diff.py` の差分 (再起動で切った平常時の前後・ホスト別・接続元別・名前解決・エラー・バースト) と、
手元から見た待ち (`probe-deployed.sh`) を続けて回します。**保存先は既定でリポジトリの `status/`** で、
個票には接続元 IP と宛先が並ぶので `.gitignore` に入れてあり、コミットしません。
**雪像の前に `/status` を 1 本取り、要約の RSS だけそちらの値を使います** (`/snapshot` は 17 部・最大 4 MiB を
1 つの文字列に組むので、雪像を配ること自体が RSS を約 1.0 MB 押し上げます。ほかの通算は 1 秒差で
意味が変わらないので雪像の値のままです。T.15.0 (15))。判定表の既定は **`CRITERIA=phase27`** (T.30.0・T.30.1。phase26 の 26 行のうち、接続のエラーの行の下限を分母 (名前解決に成功した試行) の 1,000 本に掛け、RSS の返していない分の行を固定の 10 MB ではなく「前の版より下がる」に読み替えたもの。phase26 は T.29.10 で phase25 の 17 行に、408 の件数・`/log` の error・`/errors` の `local`・3 秒で切った確立・`rejected_requests`・RSS の返していない分・cgroup の CPU・裏の役割の CPU の割合・`/log` の 502 の 1 分あたりの行数の 9 行を足したもの。phase25 は T.27.0 で phase24 の 17 行のうち 3 行の読み方を直したもの。phase24 は T.24.2:phase23 の 16 行をそのまま持ち越し、heap の行を「起動から 60 秒の基準 (`/status` の `memory.baseline`) からの増え」に、無言の再起動の行を「こちらが原因 (同じ `boot` のまま起動し直した回) が 0 回」に読み替え、T.24.3 で 17 番目に**確立の尾を同じ窓のカーネル RTT + 5 ms で読む行**を足したもの (尾が 5 ms 未満なら「距離」。`/recent` の connect の行の `ms.connect` と `rtt_ms.origin` を同じ行で対にした差の中央値がプロキシの取り分、p95 が尾)。`memory.baseline` の無い雪像は phase23 の読み方に戻り、`rtt_ms.origin` の無い雪像 (Linux 以外・`--lite`) は確立の尾の行が「判定できず」になります) です。**`CRITERIA=phase23`** (T.23.1。phase22 の 14 行のうち接続のエラーの行を「名前解決に成功した試行あたり」にし、heap の「判定できず」に `/events` の `start` の時刻を添え、無言の再起動の行を足した 16 行)・**`CRITERIA=phase22`** (T.22.1。phase21 の 10 行 (`/events` の行は表示だけ) に、`dns_slow` が合計 1,000 ms 未満の窓で立っていないこと (T.22.2) と keep-warm の既定を戻す条件 2 つ (`stale_retries` の割合、接続のエラーの接続 1 本あたり) と T.22.4 の目安の 4 行を足し、平常時を「300 本と、その期間の 1 時間の本数の中央値の 3 倍の大きい方」未満で切るもの) です。**`CRITERIA=phase21`** (T.21.2。phase20 と同じ並びの 10 行で、平常時のミスを「表示だけ」(日ごとの幅と `cold` / `expired` の内訳)、`conn` 役の CPU/要求 を前か後の `/profile` が 12 時間に満たなければ「参考」、`ipv6.request_probes` を 0 時間の雪像から 3 時間以内の探りなら「満たした」にしたもの)・**`CRITERIA=phase20`** (T.20.1。下の phase18 と同じ並びの 10 行で、`heap_used + mmap` の増えから `cache_memory` を引き、`discord.com` のミス率・`timeout`・`dns_warm` の最大と `warm_evicted` の 3 行を「表示だけ」にしたもの)・**`CRITERIA=phase18`**
(phase17 の 8 行 = phase15 の 4 行 + T.17.0a の 4 行 に、T.18.2 の 2 行 = `heap_used + mmap` の 0 時間の雪像からの増えが 5 MB 未満・`ipv6.request_probes` が 0 を足した 10 行。`conn` 役の CPU/要求 の行には上で繋いだ `-profile_res_60.json` を `--profile` で、相手の雪像の隣にあればそれを `--profile-before` で渡します)・`CRITERIA=phase17`・`CRITERIA=phase15` (T.15.4 / T.15.5 / T.15.6 の 6 行)・`CRITERIA=phase14` も今までどおり使えます。
**判定表の相手は既定では直前の 1 枚**です。再デプロイのあとはそれが 0 時間の雪像になるので、**`BEFORE=<再デプロイ直前の雪像>`** を渡すと判定表だけ相手がそちらになります (隣の同じ時刻の `-profile_res_60.json` が `--profile-before`。「前回との差分」とホスト別の相手は直前の 1 枚のまま)。**`ZERO=<0 時間の雪像>`** は `snapshot-diff.py --zero` に渡り、無ければ `heap_used + mmap` の行 (phase24 は `memory.baseline` の無い雪像のときだけこれで読み、phase23・phase22 と phase21 では 0 回でない `ipv6.request_probes` の行も) は「判定できず」です。phase24・phase23・phase22・phase21・phase20・phase18 のときは保存先を `--daily-snapshots` にも渡すので、`--from-server` で取り寄せた日次の雪像のうち同じ版のものの表 (`rss`・`heap_used`・`heap_free`・`mmap`・`rings_used.total`・`ipv6.attempts`) が判定表の下に付きます。`BEFORE` と `ZERO` の相対パスはリポジトリの根からです (`DIR` と同じ)。取り忘れた日は
`scripts/collect-deployed.sh --from-server <host>:<port>` でプロキシ側の雪像から埋められます。
**`--closeout` は締めの手作業を 1 回にまとめます** (T.29.26): `--from-server` で取り寄せ、いまの版 (`/status` の `version`) で
「前」(系列の違う版の最後の停止) と「0 時間」(いまの版の最初の起動) を選んで `BEFORE` / `ZERO` に渡し、普段の 1 回を回して、
要約の頭に**前提の 1 行** (版・ビルド・起動時刻・`boot.id`・前の版の停止時刻) と候補の一覧を置きます。`BEFORE` / `ZERO` を
自分で渡せばそちらを使います (選んだ方も並べて出します)。24 時間後の予約は今までどおり別です。
**`--full` を付けると、雪像で `truncated` が立った部 (`recent` / `hosts` / `profile`) の続きを `offset=` で
`next_offset` が `null` になるまで追い**、`<UTC 時刻>-page<何枚目>-<部>.json` に落とします (T.15.0 (11)。
1 枚には `/recent` は 2,000 件中 615 件、`/hosts` は 1,000 件中 639 件、`/profile?res=5` は 720 標本中 456 しか
入りません。T.15.0 の行の形での実測で、T.16.0 の欄のぶん少し減ります)。**`recent` と `hosts` は T.22.5 から `--full` が無くても追って雪像に繋ぐ**ので
(上の段落)、`--full` で別ファイルに落ちるのは `profile` と、`MAX_PAGES` か `FOLLOW_SECS` で止めた残りだけです (続きは繋いだ部の `next_offset` から)。
`--full --no-follow` で前の `--full` (3 つとも別ファイル) になります。続きはどれも重い口で、重い口は同時 1 本 (上の「重い口は同時に 1 本だけ」) なので
順に引きます。1 つの部で追う枚数の上限は `MAX_PAGES` (既定 8)。
`/recent?since=<前の雪像の取得時刻>` で取らないのはわざとです: `since` は**開いた**時刻で絞るので、前の雪像より前に開いて後に閉じた長いトンネルがどちらの雪像にも入りません (それにリングは 2,000 件しか持たないので、`since` で窓は広がりません)。頁の間に閉じた接続で並びがずれて 2 枚に出た行は `id` (`hosts` は `host`) で落とします。`hosts` は要求数の順なので、頁の間に順位が入れ替わったホストが 1 つ出ないことはあります。
`snapshot-summary.py` に渡す雪像の形は変わりません (落とすのは別ファイルです)。
**名前が `page<何枚目>-<部>` の順なのはわざと**です: `<部>-<何枚目>` にすると
`snapshot-diff.py --from-files` と `anonymize-snapshot.py` が「`<UTC 時刻>-` の後ろが部の名前で始まれば
その部を 1 本ずつ取ったファイル」と見分けるので、**続きの 1 枚を部そのものと取り違えます**
(`page2-hosts.json` はどちらの道具も読み飛ばすので、`--full` を回した置き場に `--from-files` を掛けても
雪像 1 枚ぶんが混ざりません)。`--full` は `--from-server` とは併用できません (あちらは保存済みの雪像を
そのまま取り寄せるだけなので、続きを引く相手が居ません。両方付けると 1 行断って `--from-server` だけを行います)。
7 日ぶん溜まったら `scripts/weekly-report.py` が週次の 1 枚になります。

**読むときの注意** (2026-09-18 に実際に判定して分かったこと):

- **判定表の「平常時」は本数で切っています** (1 標本 300 本未満)。同時接続の山ではなく本数で切るので、
  同時接続 32 の山が来た 1 時間を**残し**、同時接続 6〜8 で本数だけ多い時間を**捨てる**ことがあります。
  実測では切り方を 9 通り変えると名前解決のミス率が **0.094〜0.171** に振れました。決まりどおりの 0.144 は
  いちばん悪い値ではなく、本数の閾を 200 本 / 500 本にすると **0.156 / 0.171** で基準 0.15 を越えます。
  切らずに全時間で読むと 0.147 (基準に対する余裕 2%、0.144 なら 4%)、同時接続で切ると **0.094** まで下がります。
  **判定がぎりぎりのときは切り方を変えて幅も見てください** (内訳は `TODO.md` §2 の「物差しの但し書き」)。
- **雪像の `dropped` が `[]` でも欠けていないとは限りません**。`dropped` は「部ごと落とした」の意味で、
  部の中の打ち切り (1 部 256 KiB) は**各部の `truncated` / `count` / `shown`** に出ます。2026-09-18 の雪像は
  `/recent` 615/2,000・`/hosts` 639/1,000・`/profile?res=60` 362/1,440 が切れているのに `dropped` は `[]` でした。
- **`/recent` に残るのは「最後に閉じた N 本」**です。`at` (開始時刻) の最小で窓を切ると手前が抜けるので
  (2026-09-18 の雪像では 2 時間ぶん 85 本)、窓の左端は **`min(at + secs)`** で取ってください。
- **T.15.0 で足した `/history` の 8 列は、再デプロイより前の標本では 0 です**
  (`waits` / `wait_ms_sum` / `wait_ms_max` / `wait_buckets` / `dns_warm` / `requests_delta` /
  `bytes_delta` / `active_peak`)。版を上げずにレコードの余白へ足したので、前の版が書いた `.rrd` の
  レコードはそこがゼロ埋めで読み戻ります。再デプロイ直後の `/history` にはその 0 が
  `?res=5` で最大 1 時間・`?res=60` で最大 1 日・**`?res=3600` で最大 30 日**ぶん並ぶので、
  **`t` が最後の再起動 (`/status` の `uptime_secs` から出せます) より前の標本ではこの 8 列を読まないでください**
  (`active_peak >= active_max` もそこでは成り立ちません)。`wait` の p50 を次の完了の定義の基準線にするときも、
  見るのは再起動より後の標本だけです。`scripts/snapshot-diff.py` は**再起動の時刻で切った「後」の期間**しか
  判定に使わないので、この穴を踏みません (`--criteria phase15`)。

### 5. 何かおかしいとき

```bash
curl "http://127.0.0.1:8080/errors?n=100"        # 直近のエラー (誰が・いつ・なぜ。403 も入る)
curl "http://127.0.0.1:8080/log?n=200"           # 直近の warn 以上のログ
curl "http://127.0.0.1:8080/events?n=200"        # 起動・再読込・IPv6 の切替・異常の検知
curl "http://127.0.0.1:8080/recent?n=200&sort=slow"   # 遅かった接続から順に、段階と閉じた理由
curl "http://127.0.0.1:8080/explain?host=<name>"      # 1 つの相手を 1 枚で (?client=<ip> も)
```

`/events` の `anomaly` は自動で立った異常 (名前解決が遅い・**ミスの率が高い**・エラーが増えた・山が来た・
**CPU が絞られ続けている**・**回りっ放しのトンネルがある**・新しい接続元) で、
`reload` には `.env` で変えた名前と前後の値が入ります。

### 6. 止めるとき

`SIGTERM` を送ったあと、**ログに `statistics saved to ... (N individual records appended)` が出るまで待ってから**
次の版を起動してください。待たずに起動すると最後の書き出しが間に合わず、
ホスト別・接続元別の通算が読み戻せません (次の起動のログが `0 hosts, 0 clients restored` になります)。

### 7. そのあと

再デプロイ後 24 時間の数字で `TODO.md` の §2 と §0 を書き直すのが、そのタスクの `Tn.99` (タスク 14 は `T.14.99`、タスク 17 は `T.17.99`、タスク 18 は `T.18.99`) です。
