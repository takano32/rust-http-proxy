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
- 振る舞いの決まり (仕様 S1〜S23): [`SPECS.md`](SPECS.md)・性能の測り方とデプロイ先の数字: [`docs/perf.md`](docs/perf.md)

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
(`--lite` でも `/status` `/hosts` `/clients` `/errors` の集計はメモリに積みます。仕様 S8)。

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
比べるときは必ず同じ固定の仕方で測ってください。数字は 2026-09-08 の実測で、[`docs/status.md`](docs/status.md) の表と同じものです
(出典: T10.11 / T11.4)。括弧の **(NN%)** は**プロキシが使った 1 コアぶんの割合** (割り当ては 4 コア = 400%)。
これが 100% に遠い行は**プロキシが律速していない** = その req/s はプロキシの上限ではありません。
**命令数 (callgrind の Ir/要求) で前後を比べる**のは `scripts/callgrind-per-request.py --bin 前 --bin 後` です (forward / HIT / CONNECT を N=400 と 2000 の傾きで出し、最初のバイナリを基準に増減 % を添えます。T34.7。既定は 3 回の中央値で、中央値から 1% 以上離れた回に `*` を付けます。T36.6)。

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
| 1 要求あたりのシステムコール | **5.02** (`recvfrom` 3 + `sendto` 2。T26.0 で GET / HEAD の覗きを省いて 4.03、T29.2 で全要求に戻したので今は 5 前後 (測り直していない)) | 5.02 |
| 同時 5,000 トンネル (アイドル) | **68 スレッド (山 260) / RSS 29.9 MB** | 68 (260) / 28.7 MB |
| 暇な keep-alive 接続 2,000 本 | **25 スレッド / RSS 25.2 MB** | 28 スレッド / 25.4 MB |
| 起動直後のスレッド | `--lite` で 4 本、既定で 7 本 (2026-09-08。今は 9 本 → 「起動直後のスレッド数」) | 同左 |
| バイナリ | 1,381,872 B (2026-09-08) | **1,054,184 B** (2026-09-08。いまの配布物 v0.30.4 は x86_64 2,158,664 B・aarch64 1,962,008 B) |

`release` は `cargo build --release` で作るもの、`dist` は配布用 (fat LTO、`cargo build --profile dist`) です。
違いは仕様 S23 を見てください。

**ベンチ自身の上限** (`--only direct` = プロキシを通さないオリジン直結) は LITTLE に固定して **55,112 req/s**、
固定しなければ 263,821 req/s です。forward・HIT・64 並列・1 接続 1 要求はいずれもベンチが先に頭打ちになるので、
表の req/s はプロキシの上限ではありません。**CONNECT 確立はカーネルの TIME_WAIT が律速**で
(`tcp_max_tw_buckets = 32768` を 10 秒で使い切る)、同じ設定でも tunnels/s は ±25% 暴れます
(比べてよいのは CPU/本。こちらは ±4%)。

着手前 (2026-09-07 の Python ベンチ) は forward 8 並列 188 req/s / p50 42.0 ms、CONNECT 確立 3,506 tunnels/s、
バイナリ 857 KB でした (forward は **205 倍**)。当時の値と、コアを固定せずに測っていた頃の値は
[`docs/status.md`](docs/status.md) と [`docs/measurements.md`](docs/measurements.md) に残してあります。

**置いた先での測り方** (`PROXY_SELF_BENCH=on`)、**デプロイ先 (tokyo) の数字と判定表 (`--criteria phaseNN`) の定義**、
手元での再現 (`scripts/deployed-like.sh`・`bench --only replay`)、トンネルの MiB/s の注は
[`docs/perf.md`](docs/perf.md) に分けました (T29.32)。

## 特徴

- **認証不要（No Auth）**: 事前設定なしで誰でも即座に利用可能 (S1)
- **依存クレートゼロ**: 外部クレート依存がないため、ビルド負荷が最小限で高速にビルド可能 (S1)
- **超軽量バイナリ**: `cargo build --release` で約 3.3 MB、配布用の `dist` (fat LTO) で約 2.1 MB (x86_64、v0.29.35) (S1)
- **HTTP / HTTPS (CONNECT トンネリング) 対応** (S1)
- **同時ミスの合流 (collapsed forwarding)**: 同じ URL を同時に頼まれても、オリジンへ行くのは最初の 1 本だけ (S5)
- **keep-alive と接続プール**: クライアント側は HTTP/1.1 の持続接続、オリジン側はつないだ接続を使い回す (S2)
- **Range / HEAD**: キャッシュ済みの完全な表現から `206` (単一範囲・`If-Range`・範囲外は `416`) と `HEAD` を切り出す (S5)
- **HTTPS のオリジンもキャッシュ (CA 不要)**: `GET /https/example.com/path` か `GET https://example.com/path` で頼めばプロキシが TLS を張って保存する (S5)
- **IPv4 / IPv6 デュアルスタック**: `[::]` と `0.0.0.0` で待ち受け、オリジンへは IPv6 優先の Happy Eyeballs (IPv6 が通らない所では IPv4 first に切り替える) (S2)
- **RFC 7230 / RFC 9110 準拠**: hop-by-hop の除去・`Via`・ループの検知 (`508`)・枠組みの検査・`Max-Forwards` など (S1)
- **ACL / ホストフィルタリング**: 接続元の許可・拒否、宛先のホストとポートの許可・拒否、私設アドレス宛ては既定で断る (S4)
- **2 段キャッシュ (メモリ + ディスク) — 固まらない限界まで使う**: メモリとディスクの空きを測り、余白を残して残りをすべてキャッシュに充てる (S5)
- **詳細なアクセスログ**: 1 要求 1 行 (キャッシュの判定・時間・バイト数)、同じ失敗の WARN は理由ごとに 1 分 1 行 (S9)
- **`.env` の自動再読込**: `$HOME/.env` の保存を inotify で検知し、ACL・タイムアウト・ログレベルを再起動なしで反映 (S7)
- **DNS キャッシュ**: 名前解決の結果を 60 秒保持し、CONNECT ごとの解決をなくす (S6)
- **入場制御 & ネガティブキャッシュ**: 層が埋まったら 2 回目に見た URL だけ保存 (S5)
- **ドメインのブロックリスト**: hosts 形式のファイルや URL (1 日 1 回自動更新) から読み、広告・トラッカーを CONNECT の段階で 403 にする (S4)
- **統計と履歴の永続化**: 固定サイズ (8 MiB) の状態ファイルに、履歴 3 解像度 (5 秒・1 分・1 時間) とホスト別・接続元別の統計を残す (S10)
- **日次の snapshot**: 1 日 1 回 (UTC 0 時)、その時点の `/snapshot` をまるごと残す (既定 30 日ぶん、起動と停止の 1 枚も) (S10)
- **個票の永続化**: 固定サイズ (4 MiB) の個票のファイルに `/recent` (閉じた接続) を残し、再起動しても読める (S10)
- **状態ファイルの版 (`.rrd`)**: 先頭 8 バイトが版の印で、いまは `SHPRRD03` (版 3) (S10)
- **接続元別の統計**: 接続元 IP ごとの要求数・転送量・拒否数・応答時間を `/status` `/metrics` とダッシュボードに出す (S14)
- **`/proxy.pac`**: ブラウザの自動設定スクリプト (S4)
- **ヘルスチェック & メトリクス & 操作**: `/healthz`・`/status`・`/metrics` (Prometheus)・ダッシュボード・`/purge`・`/blocklist` など (一覧は「動作確認 (curl)」) (S11)
- **タイムアウト制御**: 確立 (CONNECT は既定 3 秒)・ヘッダーの締め切り (408)・無通信の締め切り・転送中の最小の速さ (T29.34) (S2)

それぞれの決まり (数字・既定・試した経緯) は [`SPECS.md`](SPECS.md) の仕様にあります (T37.4 で `docs/features.md` から移した)。

## コマンドライン引数

環境変数を書かなくても、よく使う設定は引数で渡せます (優先順位は **引数 > `$HOME/.env` > 環境変数**)。

```
  -p, --port <PORT>    待ち受けポート        (SERVER_PORT)
      --bind <ADDRS>   待ち受けアドレス      (PROXY_BIND、カンマ区切り)
      --no-cache       キャッシュを止める    (PROXY_CACHE_ENABLED=off)
      --quiet          警告以上だけ出す      (PROXY_LOG_LEVEL=warn)
      --lite           最速の素通しプロファイル (PROXY_PROFILE=lite。止めるものと残るものは S8)
      --check          起動せずに環境と効く設定を出して終了 (S8)
  -h, --help           使い方を出して終了 (終了コード 0)
  -V, --version        版を出して終了
```

`--port=3128` の形も使えます。知らない引数、`--check` の出すものと終了コード、`-V` の版とビルドの種類の決まりは仕様 S8 ([`specs/S8.md`](specs/S8.md)) にあります。


## プロファイル

`--lite` (= `PROXY_PROFILE=lite`) は「認証なし・手軽・最速」の素通し設定です。止めるものと残るものは仕様 S8 にあります。


```bash
./rust-http-proxy --lite -p 8080
```

## 環境変数

すべての環境変数の既定と意味、`$HOME/.env` の読み直し、読めない値と知らない名前の扱い、`/config` の形、キャッシュの置き場の選び方は
仕様 S7 ([`specs/S7.md`](specs/S7.md)) にあります (表の並びは `/config` と `--check` が出す順)。

環境変数を設定できない環境 (Pterodactyl で egg 変数を追加する権限が無い等) では、`$HOME/.env` に `KEY=VALUE` を
1 行ずつ書きます。Pterodactyl ならファイルマネージャで `/home/container/.env` を置くだけです。
いま何が効いているかは `curl http://127.0.0.1:8080/config` で 1 枚に出ます。

## ログ

ログの行の形、レベルごとの中身、`/log` に残す行、WARN の抑え方、`cache=` の値は仕様 S9 にあります。

## キャッシュ

保存の条件、HTTPS のオリジン (`curl http://127.0.0.1:8080/https/example.com/file.zip -o file.zip` の形)、鮮度と再検証、
自動モードの予算と動的マージン、先行確保 (バラスト)、Pterodactyl での割当は仕様 S5 ([`specs/S5.md`](specs/S5.md)) にあります。


## クレート構成

**外部クレートは 1 つも使っていません** (すべて `std` のみ)。`crates/` にあるのは全部このリポジトリのコードで、
責務ごとの層に分けてあります (41 + 本体)。分けている理由は 2 つで、責務を 1 つに保つことと、`rustc` がクレート単位で
全部を一度に抱えるためビルドのメモリがそのまま行数に比例すること (動作環境の `SERVER_MEMORY` は 256 MiB)。

**クレートの版の決まり** (2026-10-23 に T26.3 の「デプロイの番号」を撤回して、T21.6 の形に戻しました): 各クレートの版は
**そのクレートの中身を最後に変えたタスクの番号** `0.<タスク>.<番号>` です (T19.1 → `0.19.1`、T25.3 → `0.25.3`)。
本体 (`rust-http-proxy`) と `proxy-server` は**そのビルドに入っている最後のタスク** = 全クレートの版と `src/` を最後に変えたタスクのうち最大です
(`-V` の版は `proxy-server` の `build.rs` が `CARGO_PKG_VERSION` から作ります)。**タグは本体の版**です (`v0.25.3` ↔ `0.25.3`)。
デプロイは複数のタスクをまとめてよく、そのときのタグは**まとめた中で一番大きいタスクの番号**になります (番号はタスクの番号なので、前に出したタグより小さくなることもあります)。
タスクの中で上げるときは `scripts/crate-versions.py --task T19.1` (分かれ目から中身が変わったクレートを `0.19.1` にし、本体と `proxy-server` を最大にします)。
「中身が変わった」は `crates/<名前>/` の差分から `Cargo.toml` の `version` の 1 行を除いたもの、本体は `src/` の下と**ルートの `Cargo.toml`** (`[profile.*]` など。版の行と `members` は除く。T29.28) です。
`--check` は `Cargo.lock` の各メンバーの版が `Cargo.toml` と同じことも見ます (手で上げて `Cargo.lock` を直し忘れても、CI は `--locked` なしでビルドが通るため。T29.28)。

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
> LTO を既定で切っているのと、全クレートを `codegen-units = 4` にしてあるのもこのためです (仕様 S23)。
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
# テスト実行 (ワークスペースなので --workspace。無いと根の `default-members = ["."]` で 1,069 本のうち 337 本しか回らない。T32.28)
cargo test --workspace

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
# connect-multi と replay は scripts/deployed-like.sh の中で回す (docs/perf.md の「デプロイ先に似せた条件で手元で測る」)。
#   replay は /recent の個票を同じ間隔・同じ本数で張り直す (--replay-file / --speed / --rtt-file)

# この機械での sendto / recvfrom 1 回の実費 (プロキシは使わない。CPU の固定が要る)
taskset -c 4-7 cargo run --release --bin bench -- --only syscall-cost --seconds 3
```

### CI (GitHub Actions)

push と Pull Request で `.github/workflows/ci.yml` が回ります。`check` は `cargo fmt --check` →
`clippy -D warnings` → **doc コメントの切れたリンク** (`RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" cargo doc --workspace --no-deps --document-private-items`。T36.20) → **文書とコードの整合** (`scripts/check-docs.sh`) → `scripts/` の単体テスト →
**クレートの版の決まり** (`scripts/crate-versions.py --check`) → **`shellcheck scripts/*.sh scripts/mx`** (切った指摘とその理由は `scripts/.shellcheckrc`) → `cargo test --workspace` →
リリースビルド → 短いベンチ、の順です。**120 MB の cgroup でビルドが通るか** (`scripts/build-memory.sh 120`) は
別のジョブ `build-memory` で並べて回します。**Linux 以外の版が組めるか**は `check-other-target` が `cargo check --workspace --all-targets --target aarch64-apple-darwin` で見ます (T36.21。警告では落としません)。それと並べてもう 1 つ、`deployed-like-snapshot` が
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

**コミットの前に手元で全部を確かめるのは `scripts/check-all.sh`** (T29.29)。fmt・clippy・`check-docs.sh`・
`check-dashboard.js`・`scripts/` の unittest・`crate-versions.py --check`・shellcheck を**1 つ落ちても止めずに全部回し**、
最後に落ちたものの名前を並べて exit 1 にします (`&&` でつなぐと最初に落ちた所で止まって残りが見えず、T28.48 では
fmt に落ちるコミットを push しました)。`--only fmt,versions` / `--skip clippy` で絞れ、入っていない命令 (node・shellcheck) は
「飛ばした」と出します。`cargo test` は入れていないので別に回してください。

**shellcheck を手元で回すのも同じ 1 行です**: `shellcheck scripts/*.sh scripts/mx`
(切った指摘とその理由は `scripts/.shellcheckrc` にあり、shellcheck が自動で読みます。入っていなければ
`apt-get install shellcheck` / `brew install shellcheck`。CI の `check` ジョブも同じ命令を回すので、
引数は CI と揃えてください)。

**文書とコードが同じ集合かを見るのは `scripts/check-docs.sh`** (T14.56)。エンドポイントも環境変数も
別々の作業で増えていくので、「**コードにあるのに文書に無い / 文書にあるのにコードに無い**」を機械で見ます。
突き合わせるのは 11 で、検査は何を見るかの名前で呼びます (出力の行の頭と NG の行に `[エンドポイント]` のように出ます。T37.7)。
**[エンドポイント]** (`crates/endpoints/src/endpoints/dispatch.rs` の経路表と
下の「動作確認 (curl)」の一覧と `/` の案内 (`endpoint_list`))、**[環境変数]** (コードの `"PROXY_…"` / `"SERVER_…"` の
文字列と仕様 S7 ([`specs/S7.md`](specs/S7.md)) の環境変数の表の鍵の欄)、**[スナップショットの部]** (`/snapshot` の `parts` と個票のエンドポイント)、
**[画面の関数]** (`scripts/check-dashboard.js` が画面の HTML から切り出す関数名と、そこで実際に呼んでいる名前)、
**[クレート]** (上の「クレート構成」の表と `crates/*/Cargo.toml` の `[package] name`。表の前の「(41 + 本体)」の数も見ます)、
**[道具の実行ビット]** (この README が `scripts/<名前>.py` と書いている道具に実行ビットがあるか。そのまま叩ける道具か。
README にあるのにファイルが無いものも NG。共通部品の `proxydata.py`、判定の部の `snapshot_criteria.py`、
unittest が名前で読む `test_*.py` は、叩かないものとして除外表に理由つきで入れてあります。T24.7)、
**[台帳]** (`TASKS.md` の**未着手**、`docs/task-index.md` (タスクの見出しと索引だけ)、`tasks/TNN.md` の
3 つ。**移し忘れ** (`TASKS.md` に `- [x]` が無いこと。済んだタスクは `tasks/` へ移します)、
**リンク** (`tasks/TNN.md` と `docs/task-index.md` の `## タスク NN (…)` の見出しが両方向で
1 対 1)、**番号** (`T24.10` が台帳全体で 1 回だけ。番号のタスクが置き場所と合うこと)、
**1 行目** (`# タスク NN — …`) などを見ます。中身は `scripts/check-ledger.py` で、台帳だけを回すなら
これ 1 本です。T24.10 / T24.13)、
**[判定表の既定]** (`collect-deployed.sh` の `CRITERIA` の既定と、文書の「既定は phaseNN」。T28.23)、
**[-h の既定]** (`crates/base/src/cli.rs` の `-h` が括弧で書く既定と、仕様 S7 の環境変数の表の既定。T28.23)、
**[上の・下の]** (README・`docs/*.md`・`SPECS.md`・`specs/*.md` で「上の」「下の」のあとにかぎ括弧で書いた名前が、同じファイルの見出し・太字・段落の頭にあって向きが合うか。
別のファイルを指すときはリンクで書きます。T32.28)、**[いまの数字の版]** (`docs/perf.md` の「いまの数字」の版が `docs/status.md` の
「デプロイ先の現在地」の版と同じか。T32.28) です。
[道具の実行ビット] と [判定表の既定] は README のほかに `docs/*.md` (性能の本文。T29.32) と仕様 (`SPECS.md`・`specs/*.md`。T37.4) も読みます。
**意図的に外してあるものはスクリプトの中の除外表に理由つきで持ちます** (`/snapshot` に入れない 18 の口、
ビルド時にしか無い `PROXY_VERSION`、`tasks/README.md` = `tasks/` の索引 など)。差分が 1 件でもあれば
その名前を印字して終了コード 1 です。
bash と grep / sed / awk / python3 の標準ライブラリだけで動くので、`cargo` も Node も要りません
(`scripts/check-docs.sh` を引数なしで回すだけ。CI の `check` が毎回回します)。

### 起動直後のスレッド数

起動直後のスレッドの本数と内訳、接続スレッドの使い回しと上限、暇な接続の預かりは仕様 S3 にあります。


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
版の文字列を作る `build.rs` は `crates/server/` にあるので `crates` に含まれます (T15.12 段 6')。`.git` は入れないので、
イメージの中の版は `rust-http-proxy 0.29.35+unknown` のように `+unknown` になります。
`.dockerignore` で `target/` と `**/target/` を送らないようにしてあります
(イメージには元から入りませんが、ビルドコンテキストとして daemon へ送ると
`cargo build` 済みの作業ツリーでは 1 GB を超えて遅くなるため。除いたコンテキストは約 2 MB)。

タグ `v*` を push すると (か `release` を `workflow_dispatch` で起動してタグを打たせると。「運用」の 0) `.github/workflows/release.yml` が x86_64 / aarch64 の
バイナリ (glibc 2.35 以上) を作って Release に添付します。

### musl 静的リンクを採らない理由 (実測)

仕様 S23 にあります。

### プロファイルの設定 (実測で決めたもの)

`release` と `dist` のプロファイルの設定と、それを決めた実測は仕様 S23 にあります。


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
curl "http://127.0.0.1:8080/status?hosts=0"             # hosts[] を省いた軽い形 (監視のポーリング用。T25.1)
curl http://127.0.0.1:8080/metrics                      # Prometheus 形式
curl http://127.0.0.1:8080/config                       # 効いている設定とその出どころ (仕様 S7)
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
curl "http://127.0.0.1:8080/history?res=5&n=4320"       # 5 秒刻みで 6 時間ぶん (T14.32)
curl "http://127.0.0.1:8080/history?since=restart&summary=1&normal_hours_only=1"  # 起動からの要約 1 行
curl "http://127.0.0.1:8080/dns?sort=misses"            # 名前解決の表 (age / host / misses で並べ替え)
curl "http://127.0.0.1:8080/dns?limit=4096&offset=600"  # 続きの名前 (next_offset が null になるまで。T27.1)
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

それぞれの口が返すものは仕様にあります。

| 口 | 仕様 |
|---|---|
| 自分宛ての判定、読めない要求を断る理由 | S1 |
| `/blocklist`・`/proxy.pac`・書き換える口と重い口の守り | S4 |
| `PURGE`・`/purge`・`/lookup` | S5 ([`specs/S5.md`](specs/S5.md)) |
| `/config` | S7 ([`specs/S7.md`](specs/S7.md)) |
| `/log` | S9 |
| `/`・応答の形の版 `schema`・`/status`・`/metrics` | S11 ([`specs/S11.md`](specs/S11.md)) |
| `/dashboard`・`/inspect`・`/probe.html` | S12 ([`specs/S12.md`](specs/S12.md)) |
| `/healthz` | S13 |
| `/errors`・`/clients`・`/readers`・`/recent`・`/connections`・`/bursts`・`/trace` | S14 ([`specs/S14.md`](specs/S14.md)) |
| `/events` | S15 |
| `/snapshot`・`/snapshots` | S16 |
| `/hosts/series`・`/explain` | S19 |
| `/daily`・`/slo` | S20 ([`specs/S20.md`](specs/S20.md)) |
| `/history` | S21 ([`specs/S21.md`](specs/S21.md)) |
| `/profile` | S22 |

## 運用 (置いた先での手順と、そのあと見るもの)

新しい版を置いたあと**いつ何を見ればよいか**を 1 か所にまとめたものです。
手順の決まりは [`docs/goals.md`](docs/goals.md) の「作業の進め方」の 9 (再デプロイの直前と直後にスナップショットを撮る) と `TASKS.md` の「決まり」(版・タグ・締め)、数字の出どころと期待値は締め (`tasks/TNN.md` の `Tn.99`) の結果にあります。

### 0. リリース (版をそろえてタグを打つ)

版は**そのクレートの中身を最後に変えたタスクの番号** `0.<タスク>.<番号>` です (T21.6。2026-10-23 に T26.3 の「デプロイの番号」を撤回して戻しました)。
小タスクを済ませるたびに、そのタスクの番号で中身が変わったクレートだけを上げ、本体と `proxy-server` は全クレートの最大にそろえます。
**デプロイの版 (= タグ) はデプロイに入れた中でいちばん大きいタスクの番号**です。

```bash
scripts/crate-versions.py --check            # 0 なら決まりに合う (本体 = 全クレートの最大、タグの上では本体 = タグ、Cargo.lock = Cargo.toml)
scripts/crate-versions.py --task T27.8      # 分かれ目から中身が変わったクレートを 0.27.8 にし、本体を最大に (Cargo.lock も)
scripts/crate-versions.py --task T27.8 --fix  # 取り込んだあとの main の上で: --check の (c) で落ちる (上げ忘れ) ものだけを上げる
scripts/crate-versions.py --list             # クレートごとに 今の版・中身を最後に変えたコミット・版の行を書いたコミット
```

**取り込んだあとの `main` の上で `--task` だけを回さないでください** (T32.27): 既定の基準 `git merge-base HEAD main` が
HEAD そのものになり、前は「上げた: 0」・終了コード 0 で抜けて、あとの `--check` が上げ忘れで落としました。いまはその形
(基準を渡さず、分かれ目 = HEAD で 0 本) を使い方の誤り (終了コード 2) にして止めます。`main` の上では `--fix`
(`--check` の決まり (c) で落ちるものだけを上げる) か、`--since <取り込む前のコミット>` を渡します。

**タグは `release.yml` の `workflow_dispatch` で打ちます** (2026-10-24 から)。GitHub の Actions の画面 (か GitHub の API・ツール) で
`release` を **入力 `version` = 本体の版 (`0.27.8` のように `v` は付けない)** で起動すると、workflow の中で本体の版と
`crate-versions.py --check` を確かめ、注釈付きのタグ `v<版>` を `main` の先頭に打って push し、そのタグで x86_64 と aarch64 を
`dist` プロファイル (fat LTO) でビルドして release に載せます。すでに同じタグがあれば打ち直さずに落ちます。
**入力 `ref` (任意。既定 `main`) でタグを打つコミットを選べます** (T29.30): 短いハッシュ (`ab12cd3`)・完全な SHA・
ブランチ名・`main~1` のどれでもよく、workflow の中で `git rev-parse` が 1 つのコミットに解きます (前は `actions/checkout` に
そのまま渡していたので短いハッシュでは落ちました。T28.99 の 1 回目)。**`main` の上に無いコミットは打たずに落ちます**
(置いた先は `main` の先頭でビルドしていることがあるため)。打ったコミット・`main` の先頭・その差は step summary の表に出ます。
**CI (`ci.yml` の `check`) が通ったコミットにだけ打ちます** (T32.32): そのコミットの最後の `check` が終わっていない・
success でない・無いときは、理由と CI の頁を出して落ちます (v0.28.48 は同じコミットの CI が終わる前に打っていた)。
CI の走らないコミットなど理由があるときだけ、入力 `unchecked` を `true` にして飛ばします。
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

**止める直前に `scripts/collect-deployed.sh` で雪像を 1 枚**撮っておきます (次の版の判定の「前」になります。T17.19)。
撮り忘れても、**プロキシ自身が停止シグナルで `<UTC 時刻>-shutdown.json` を 1 枚残します** (T28.20。`recent` は全部入り)。
再デプロイのあとに `scripts/collect-deployed.sh --from-server <host>:<port>` で取り寄せると、`BEFORE=…` の 1 行が出ます。
**「前」は時刻ではなく版で選びます** (T29.26): いまの版と**系列 (`0.<Phase>`) の違う版の最後の停止**です。同じ系列の
途中のデプロイ (T28.99 で 53 分挟んだ 0.28.39 など) の停止は飛ばし、候補を全部、外した理由つきで並べます。

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

起動から **60 秒たってから** `scripts/collect-deployed.sh` で 1 枚 (0 時間の雪像) 撮り、`/status` の `version` と起動時刻 (UTC)、`boot.id` を `TASKS.md` のそのタスクの `Tn.99` の前提に書きます (T17.19)。**すぐ撮ると `memory.baseline` が `null` のまま**なので、heap の判定が「判定できず」になります (T24.4。baseline は起動 60 秒で 1 回だけ書かれる)。
同じ 60 秒に**プロキシ自身も `<UTC 時刻>-start.json` を 1 枚残す**ので (T28.20)、撮り忘れたら `--from-server` で取り寄せて `ZERO` に渡せます
(`BEFORE=… ZERO=…` の 1 行が出ます。0 時間は**いまの版の最初の起動**の 1 枚。T29.26)。
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
**起動から 60 秒たつと `/status` の `memory.baseline` に `{"at":…,"bytes":…}` が出ます** (T24.4。
`heap_used + mmap − cache_memory`)。これがこの起動の heap の基準で、**後の雪像 1 枚で増えが読めます**
(0 時間の雪像は要りません。起動し直すと 60 秒たつまで `null` です)。
**1 時間に 1 回の `malloc_trim` が返したかは `/status` の `memory.trim` で読みます** (T30.1。
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

見るのは `dns.warm` (keep-warm が掴んでいる名前の数。既定 (`PROXY_DNS_WARM_SECS=0`、T22.4) では常に `0`。窓を設定して `0` なら効いていません) と
`dns.misses ÷ CONNECT の本数`、`ipv6` の `v4_first` (`true` なら IPv6 が落ちている環境と判断済み)、
`evicted_idle` と `rejected_overload` (山が無い時間帯は `0`)、`recent_quantiles`
(直近 1,024 本の**実測**の分位点。区間の補間ではありません)。
`/profile` の `stages` は CONNECT が `queue` / `client_read` / `dns` / `connect` / `first_relay` / `relay` / `park`、
forward が `queue` / `client_read` / `origin` / `send` / `ttfb` / `body` で、**1 本の待ちがどこに消えたか**が読めます。

**T15.0 で足した欄** (次の判断 — 窓を伸ばすか・空回りが直ったか・確立の尾は誰のせいか — に要るもの):

| 口 | 欄 | 読み方 |
|---|---|---|
| `/status` | `recent_quantiles.wait` | **利用者が待つ時間** (`queue + client_read + dns + connect`)。`connect` は要求行を読んだ後からなので、**基準線はこちら** |
| `/status` | `dns.misses_by_kind` | `expired` が主なら窓 (`PROXY_DNS_WARM_SECS`) が短い、`warm_stale` が出ていれば裏の引き直しが間に合っていない。隣の `refresh_failures` / `refresh_late` / `refresh_ms_max` を先に見ます |
| `/status` | `kernel.cgroup_cpu` | **割合で読む** (`nr_throttled ÷ nr_periods`)。`since_start` はこのプロセスが始まってからの増分、`path` は読んでいる階層 |
| `/status` | `boot` | `id` (`/proc/sys/kernel/random/boot_id`) と `pid`。**前の起動と `id` が変わっていればホストの再起動、同じならプロセスだけの再起動** (T24.0。読めない環境では `id` が `null`) |
| `/status` | `memory.rings_used` | `rings` と同じ鍵で「いま埋まっているぶん」。`rings` は 0 時間でも 36.6 時間でも変わりません |
| `/profile` | `threads_top` / `run_delay_us` | どの tid が回っているか (`/connections` の `tid` と同じ番号) と、**走れるのに CPU に乗れなかった**時間 |
| `/connections` | `idle_secs` / `spins` / `revents` / `half_closed` | 「回っているのか・待っているのか」。**既定のままの欄は出さない**ので「無ければ既定値」と読みます |
| `/history` | `key_kinds` | `keys` と同じ長さで、列ごとの読み方 (`cumulative` = 通算 / `delta` = 区間 / `gauge` / `peak` / `buckets`) |
| `/healthz` | `checks.cpu` | 直近 5 分に絞られた期間の割合。**起動から 5 分に満たない間は `null`** |

### 3.5 ふだんの見張り (`watch.yml`)

`.github/workflows/watch.yml` が `scripts/watch-deployed.py tokyo.sorahost.net:60357` を回し、
`/healthz` が ok でない・**前回の済んだ見張りから** (15 分さかのぼって) **無言の再起動** (直前に `shutdown` の無い
`start`) がある・口が読めない、のどれかで job を落とします (GitHub が失敗を知らせます。T28.26)。
**cron は毎時 17 分ですが、GitHub の schedule は実際には 2〜7.5 時間おきにしか回りません** (2026-10-26〜31 の
121.2 時間で 23 回、間隔の中央値 5.5 時間・最大 7.5 時間)。前は直近 75 分しか見ていなかったので約 8 割の期間の
再起動が見えず、いまは前回の済んだ (成功か失敗の) 実行の時刻を `--since` に渡します
(取れなければ直近 480 分。T32.25)。前回の**成功した**実行から取っていたころは、一度再起動を見つけると同じ再起動で落ち続けた (2026-11-03〜13 に 36 回。T36.99) ので、
いまは 1 回の再起動で落ちるのは 1 回 (15 分の重なりに入れば 2 回) だけです。読むのは `/healthz`・`/events?n=200`・`/status?hosts=0` の 3 つだけで、
**実行ログには結論・版・起動からの時間・落ちた検査の名前しか出しません** (リポジトリは public で、Actions のログは誰でも読めるため)。
別の置き先を見るときは Actions の画面から `workflow_dispatch` の `target` に `host:port` を渡します。手元からは
`scripts/watch-deployed.py HOST:PORT` (CONNECT しか通さないプロキシの内側からは届きません)。

### 4. 24 時間後

```bash
# 雪像を 1 枚取り、前回との差分と判定表つきの Markdown を 1 枚にする
scripts/collect-deployed.sh <host>:<port>
# 判定表の相手を再デプロイ直前の雪像にし、0 時間の雪像 (直後の 1 枚) も渡す (上の 1 と 2 で撮った 2 枚)
BEFORE=status/<前>-snapshot.json ZERO=status/<0 時間>-snapshot.json scripts/collect-deployed.sh <host>:<port>
# 締めを 1 コマンドで (T29.26): 取り寄せ → 前と 0 時間を版で選ぶ → 上の 1 回 → 要約の頭に前提の 1 行
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
意味が変わらないので雪像の値のままです。T15.0 (15))。判定表の既定は **`CRITERIA=T37`** (T37.1。T36 の 27 行のうち、`dns_slow` が立った窓の名前解決に成功したミスの平均の行を、前の版との比ではなく**いつも 500 ms 未満**かで読むもの。立った窓は成功したミスが数回しかない小さい窓で、前の版との比はぶれで閾を越えやすかった (0.36.34 の締めで 139 → 225 ms の 1.62 倍)。前の版の値と倍率は表示だけです。T36 は T36.1・T36.2・T36.3 で、T35 の 26 行のうち 5 行を直し、27 行目に表示だけの行を 1 つ足したもの: heap の行を、キャッシュの持ち物に加えて**名前解決の表の持ち物** (`/status` の `dns.entries` × 1 名前あたり 1,070 B。実バイナリの表を埋めて `/status` を前後で読んで測った値) も引いた残りで読むもの。表は上限 4,096 名前で止まる増えで、漏れではありません。基準の時点の名前もまとめて引くので多めに引く側に倒れ、`dns.entries` の無いスナップショットは 0〜上限ぶんの幅で引いて、残りの上下限が閾 5 MB をまたげば「判定できず」です。前の版と比べる 2 行は、前が 0 件でも読めるように下限を置きました: 接続のエラー (443・80) の率は前の版の 1.5 倍未満**か、後が 0.001 未満**なら満たし (前が 0 件だと後が 1 件でも届かずになっていた)、`dns_slow` は前の版に成功したミスで立った窓が無ければ、**後の平均が 500 ms 未満**で読みます (tokyo の立った窓の平均 144〜334 ms の最大の 1.5 倍)。CPU の 2 行 (`conn` 役の CPU ÷ 名前解決に成功した要求、暇なときの費用) は、前と後をそれぞれ**機械の速さの物差し**で割ってから、前の版の 1.1 倍未満かを見ます (T36.2。後の値を前の機械の値段に直して書き、割る前の値は表示だけ)。物差しは `/profile` の `history` 役の CPU ÷ その役の標本の数 (us/秒) で、5 秒に 1 回の決まった仕事の値段です (0.34.165 → 0.35.12 では、コードが同じまま 81.3 → 110.5 us/秒 = 1.36 倍になっていた)。`profile-sample` 役の CPU ÷ 読んだスレッドの標本は、暇な秒に全スレッドを読まない (T32.37) ので要求の量で動き、物差しには使わずに参考で並べます。物差しの前後の比は表示だけの 27 行目に出ます。T35 は T35.9・T35.11 で、phase30 の 26 行のうち 2 行を直したもの: 表示だけの `/events` の anomaly の件/時を**要約 (`snapshot-summary.py`) の表と同じ関数**で数え (件/時は 3 桁)、heap の行を `heap_used + mmap − cache_memory` の起動 60 秒の基準からの増えから、さらに**キャッシュの持ち物** (本文の確保の端数と索引・鍵) を引いた残りで読むもの。キャッシュがメモリを使うのは良いことなので、その分では届かずにしません。`cache_memory` は本文の長さの和で、長さの分からない (chunked の) 本文は 64 KiB から倍々で育った確保のまま置かれるので、`cache.memory.alloc_bytes` の無い雪像 (T35.11 より前の版) は端数を 0〜(`used_bytes` + 件数 × 64 KiB) の幅で引き、残りの上下限が閾 5 MB をまたげば「判定できず」(上下限を書く) です。`alloc_bytes` のある雪像は端数が `alloc_bytes − used_bytes` の 1 つに決まり (T35.11 の後は保存の最後で縮めるので 0)、1 つの値で判定します。phase30 は T33.1・T33.2 で、phase29 の 26 行のうち 2 行を使われ方で動かない形にしたもの: `dns_slow` の行を、件/時ではなく**立った窓の名前解決に成功したミスの平均が前の版の 1.5 倍未満**で読むもの (件/時は初めて引く名前の数で動くので表示だけ。前の版に `dns_fail_ms_sum` が無ければ、失敗の混ざらない 1 分の標本は確か・混ざった 1 分は 0〜その 1 分の合計とした上下限で比べ、どちらとも言えなければ「判定できず」)、1 名前あたりの裏の引き直しの行を、表の名前の数 (`dns.entries`。表の上限と総当たりの濃さで動く) ではなく**答えのある名前 1 つあたり** (`/dns` の一覧の `addrs` のある行) で前の期間の 1.5 倍未満で読むもの (前か後の一覧が切れていれば引き直しの合計 (回/時) で比べる)。phase29 は T32.16・T32.29・T32.30 で、phase28 の 26 行のうち 5 行を、名前解決に失敗した要求 (負のキャッシュで即 502) の割合で動かない形にしたもの: `conn` 役の CPU は名前解決に成功した要求 1 本あたり、暇なときの費用は裏の役割 (`conn`・`accept` 以外) の us/秒 (どちらも前の版の 1.1 倍未満。前か後の `/profile` が 12 時間に満たなければ「判定できず」で、`--closeout` は前の版の最後の 12 時間以上の `-profile_res_60.json` を前の材料に選ぶ)、cgroup の CPU は表示だけ、接続のエラーは起動からの `/history?res=3600` の 443・80 宛て (`other_port_*` を引く)、`dns_slow` は名前解決に成功したミスで立った件/時 (`/history` の `dns_fail_ms_sum` の無い版は「判定できず」)。表示だけの平常時の名前解決のミスの行と要約の §2 には、名前解決の失敗を除いた回/接続 (`dns_misses − dns_fail_misses`) も並べます (T32.17。失敗を数えた回/接続は T31.99 で 0.95、除けば 0.047)。phase28 は T31.0・T31.1 で、phase27 の 26 行のうち、cgroup の CPU の行をコアの合計ではなく起動からの要求 1 本あたりで前の版と比べ (1.1 倍未満)、RSS の返していない分の行を「`malloc_trim` が毎時動いて返している (`memory.trim`)」で読むようにしたもの。phase27 は T30.0・T30.1 で、phase26 の 26 行のうち、接続のエラーの行の下限を分母 (名前解決に成功した試行) の 1,000 本に掛け、RSS の返していない分の行を固定の 10 MB ではなく「前の版より下がる」に読み替えたもの。phase26 は T29.10 で phase25 の 17 行に、408 の件数・`/log` の error・`/errors` の `local`・3 秒で切った確立・`rejected_requests`・RSS の返していない分・cgroup の CPU・裏の役割の CPU の割合・`/log` の 502 の 1 分あたりの行数の 9 行を足したもの。phase25 は T27.0 で phase24 の 17 行のうち 3 行の読み方を直したもの。phase24 は T24.2:phase23 の 16 行をそのまま持ち越し、heap の行を「起動から 60 秒の基準 (`/status` の `memory.baseline`) からの増え」に、無言の再起動の行を「こちらが原因 (同じ `boot` のまま起動し直した回) が 0 回」に読み替え、T24.3 で 17 番目に**確立の尾を同じ窓のカーネル RTT + 5 ms で読む行**を足したもの (尾が 5 ms 未満なら「距離」。`/recent` の connect の行の `ms.connect` と `rtt_ms.origin` を同じ行で対にした差の中央値がプロキシの取り分、p95 が尾)。`memory.baseline` の無い雪像は phase23 の読み方に戻り、`rtt_ms.origin` の無い雪像 (Linux 以外・`--lite`) は確立の尾の行が「判定できず」になります) です。**`CRITERIA=phase23`** (T23.1。phase22 の 14 行のうち接続のエラーの行を「名前解決に成功した試行あたり」にし、heap の「判定できず」に `/events` の `start` の時刻を添え、無言の再起動の行を足した 16 行)・**`CRITERIA=phase22`** (T22.1。phase21 の 10 行 (`/events` の行は表示だけ) に、`dns_slow` が合計 1,000 ms 未満の窓で立っていないこと (T22.2) と keep-warm の既定を戻す条件 2 つ (`stale_retries` の割合、接続のエラーの接続 1 本あたり) と T22.4 の目安の 4 行を足し、平常時を「300 本と、その期間の 1 時間の本数の中央値の 3 倍の大きい方」未満で切るもの) です。**`CRITERIA=phase21`** (T21.2。phase20 と同じ並びの 10 行で、平常時のミスを「表示だけ」(日ごとの幅と `cold` / `expired` の内訳)、`conn` 役の CPU/要求 を前か後の `/profile` が 12 時間に満たなければ「参考」、`ipv6.request_probes` を 0 時間の雪像から 3 時間以内の探りなら「満たした」にしたもの)・**`CRITERIA=phase20`** (T20.1。下の phase18 と同じ並びの 10 行で、`heap_used + mmap` の増えから `cache_memory` を引き、`discord.com` のミス率・`timeout`・`dns_warm` の最大と `warm_evicted` の 3 行を「表示だけ」にしたもの)・**`CRITERIA=phase18`**
(phase17 の 8 行 = phase15 の 4 行 + T17.0a の 4 行 に、T18.2 の 2 行 = `heap_used + mmap` の 0 時間の雪像からの増えが 5 MB 未満・`ipv6.request_probes` が 0 を足した 10 行。`conn` 役の CPU/要求 の行には上で繋いだ `-profile_res_60.json` を `--profile` で、相手の雪像の隣にあればそれを `--profile-before` で渡します)・`CRITERIA=phase17`・`CRITERIA=phase15` (T15.4 / T15.5 / T15.6 の 6 行)・`CRITERIA=phase14` も今までどおり使えます。
**判定表の相手は既定では直前の 1 枚**です。再デプロイのあとはそれが 0 時間の雪像になるので、**`BEFORE=<再デプロイ直前の雪像>`** を渡すと判定表だけ相手がそちらになります (隣の同じ時刻の `-profile_res_60.json` が `--profile-before`。「前回との差分」とホスト別の相手は直前の 1 枚のまま)。**`ZERO=<0 時間の雪像>`** は `snapshot-diff.py --zero` に渡り、無ければ `heap_used + mmap` の行 (phase24 は `memory.baseline` の無い雪像のときだけこれで読み、phase23・phase22 と phase21 では 0 回でない `ipv6.request_probes` の行も) は「判定できず」です。phase24・phase23・phase22・phase21・phase20・phase18 のときは保存先を `--daily-snapshots` にも渡すので、`--from-server` で取り寄せた日次の雪像のうち同じ版のものの表 (`rss`・`heap_used`・`heap_free`・`mmap`・`rings_used.total`・`ipv6.attempts`) が判定表の下に付きます。`BEFORE` と `ZERO` の相対パスはリポジトリの根からです (`DIR` と同じ)。取り忘れた日は
`scripts/collect-deployed.sh --from-server <host>:<port>` でプロキシ側の雪像から埋められます。
**`--closeout` は締めの手作業を 1 回にまとめます** (T29.26): `--from-server` で取り寄せ、いまの版 (`/status` の `version`) で
「前」(系列の違う版の最後の停止) と「0 時間」(いまの版の最初の起動) を選んで `BEFORE` / `ZERO` に渡し、普段の 1 回を回して、
要約の頭に**前提の 1 行** (版・ビルド・起動時刻・`boot.id`・前の版の停止時刻) と候補の一覧を置きます。`BEFORE` / `ZERO` を
自分で渡せばそちらを使います (選んだ方も並べて出します)。24 時間後の予約は今までどおり別です。
**`--full` を付けると、雪像で `truncated` が立った部 (`recent` / `hosts` / `profile`) の続きを `offset=` で
`next_offset` が `null` になるまで追い**、`<UTC 時刻>-page<何枚目>-<部>.json` に落とします (T15.0 (11)。
1 枚には `/recent` は 2,000 件中 615 件、`/hosts` は 1,000 件中 639 件、`/profile?res=5` は 720 標本中 456 しか
入りません。T15.0 の行の形での実測で、T16.0 の欄のぶん少し減ります)。**`recent` と `hosts` は T22.5 から `--full` が無くても追って雪像に繋ぐ**ので
(上の段落)、`--full` で別ファイルに落ちるのは `profile` と、`MAX_PAGES` か `FOLLOW_SECS` で止めた残りだけです (続きは繋いだ部の `next_offset` から)。
`--full --no-follow` で前の `--full` (3 つとも別ファイル) になります。続きはどれも重い口で、重い口は同時 1 本 (仕様 S4) なので
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
  **判定がぎりぎりのときは切り方を変えて幅も見てください** (内訳は [`docs/measurements.md`](docs/measurements.md) の「物差しの但し書き」)。
- **雪像の `dropped` が `[]` でも欠けていないとは限りません**。`dropped` は「部ごと落とした」の意味で、
  部の中の打ち切り (1 部 256 KiB) は**各部の `truncated` / `count` / `shown`** に出ます。2026-09-18 の雪像は
  `/recent` 615/2,000・`/hosts` 639/1,000・`/profile?res=60` 362/1,440 が切れているのに `dropped` は `[]` でした。
- **`/recent` に残るのは「最後に閉じた N 本」**です。`at` (開始時刻) の最小で窓を切ると手前が抜けるので
  (2026-09-18 の雪像では 2 時間ぶん 85 本)、窓の左端は **`min(at + secs)`** で取ってください。
- **`/recent` の `count` は `shown` と同じ値** (この応答に入れた件数) です。絞りに合った件数は `matched`、環に残っている件数は `kept`、起動からの通算は `recorded` を読んでください (T36.18)。
- **T15.0 で足した `/history` の 8 列は、再デプロイより前の標本では 0 です**
  (`waits` / `wait_ms_sum` / `wait_ms_max` / `wait_buckets` / `dns_warm` / `requests_delta` /
  `bytes_delta` / `active_peak`)。版を上げずにレコードの余白へ足したので、前の版が書いた `.rrd` の
  レコードはそこがゼロ埋めで読み戻ります。再デプロイ直後の `/history` にはその 0 が
  `?res=5` で最大 1 時間・`?res=60` で最大 1 日・**`?res=3600` で最大 30 日**ぶん並ぶので、
  **`t` が最後の再起動 (`/status` の `uptime_secs` から出せます) より前の標本ではこの 8 列を読まないでください**
  (`active_peak >= active_max` もそこでは成り立ちません)。`wait` の p50 を次の完了の定義の基準線にするときも、
  見るのは再起動より後の標本だけです。`scripts/snapshot-diff.py` は**再起動の時刻で切った「後」の期間**しか
  判定に使わないので、この穴を踏みません (`--criteria phase15`)。

**締めの結果を台帳に移すのは `scripts/ledger-close.py`** です (T32.26。前は写しのスクリプトを毎回書き換えていました):

```bash
scripts/ledger-close.py T32.5 結果.txt    # TASKS.md の T32.5 を [x] にし、結果を足して tasks/T32.md へ。docs/task-index.md・索引・範囲の印も直す
scripts/ledger-close.py --finish 32 --deployed 0.32.9+abc1234 --hours 48.2   # 全部済んだら前置きを移して節を消す
```

`結果.txt` には `  - 結果 (日付、`ハッシュ`): …` の行を、移した先 (`tasks/`) に置く文として書きます。本文は 1 文字も
変えずに移し、**相対リンクだけは移した先から辿れる形に書き換えます** (T32.38)。`--deployed` は `docs/status.md` の「デプロイ先の現在地」の
見出しの版と日付と参照先を直し、要点の数字は手で書く場所として出します。最後に `scripts/check-ledger.py` を回します。
**続けてクレートの版の決まり (`scripts/crate-versions.py --check`) も回し、合わなければその場で知らせて 1 で抜けます**
(T36.25。小タスクを main に取り込むたびに版がずれたままだと、main の CI が版の確かめで赤いままになり、ほかの失敗が
隠れます)。知らせには直す命令 `scripts/crate-versions.py --task <締めた小タスク> --fix` が付くので、取り込んだ
main の上で回して台帳と一緒にコミットします。`--no-check` はどちらも回しません。

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

再デプロイ後 24 時間の数字で `docs/status.md` と `docs/goals.md` を書き直すのが、そのタスクの `Tn.99` (タスク 14 は `T14.99`、タスク 17 は `T17.99`、タスク 18 は `T18.99`) です。
