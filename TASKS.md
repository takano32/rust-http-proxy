# TASKS.md — 認証なし・手軽・最速の HTTP Proxy にするための作業一覧

**この文書の形式**: 済んだ作業もこれからの作業も、同じ 1 つの形で書く。

```
- [x] **Tn.n 題**
  - 目的: なぜやるか
  - 変更箇所: どのファイル
  - やること: 何をするか
  - 受け入れ基準: どうなったら終わりか
  - 結果: 実測値と結論 (`コミット`)   ← 終わったものだけ
```

1 タスク = 1 コミット。性能に関わるものは **変更前後を必ず測って** コミットメッセージ本文に貼る。
測って「やらない」と決めたものは消さずに §4 に残す (同じ道を二度調べないため)。

## 0. ゴールと前提

**ゴール**: 「バイナリを 1 つ置いて起動するだけで使える、認証なしの HTTP/HTTPS(CONNECT) プロキシ」を、
同じ条件でこれ以上速くならないところまで速くする。**速さは必ず計測して示す** (印象で判断しない)。

**守ること (非目標も含む)**:

- **認証は入れない**。`Proxy-Authorization` を要求するコードは書かない (履歴上 `refactor: remove authentication` で意図的に外した)。
- **外部クレートは追加しない**。依存してよいのはこのリポジトリのコードだけ (`crates/` のワークスペースメンバー)。
  crates.io のクレートは 1 つも入れない (`libc` も入れない)。
  システムコールが必要なら `crates/base/src/sysinfo/inotify.rs` / `signal.rs` / `crates/net/src/tls.rs` と同じく
  `unsafe extern "C"` で直接宣言し、`#[cfg(target_os = "linux")]` で囲んで他 OS には従来コードへのフォールバックを残す。
- **既存機能を壊さない**。`cargo test --workspace` は常に全通過。キャッシュ・ダッシュボード等は「使わないときに一切コストがかからない」ようにするのが方針で、削除はしない。
- Rust は `edition = "2024"`、`rustc 1.96` で動くこと。`cargo clippy --workspace` の警告を増やさない。
- **`cargo build --release` はメモリ 200 MB で通ること** (動作環境のコンテナが小さい)。
  `scripts/build-memory.sh` が CI で見張っている。1 クレートが大きいと `rustc` がまとめて抱えるので、
  超えたら層を増やして割る。**ファイルを割っても下がらない** (同じクレートなので)。
- **1 ファイルは 2,000 行を目安**にする。超えたら責務で割る。
- コメントとログの文体は既存に合わせる (コメントは日本語、ログ文字列は英語)。
- 動作環境は Linux (Pterodactyl コンテナ) が主。`SERVER_PORT` / `SERVER_MEMORY` / `$HOME/.env` の扱いは変えない。

**作業の進め方 (毎タスク共通)**:

1. 着手前に対象ファイルを読み、README の該当箇所を確認する。
2. 実装 → `cargo fmt` → `cargo clippy --all-targets` → `cargo test` → `cargo build --release`。
3. 性能に関わるタスクは **変更前後の計測値** を取り、コミットメッセージ本文に貼る (下の「計測手順」)。
4. 環境変数や挙動を足したら README の表と説明を更新する。
5. コミットメッセージは既存に合わせて `feat:` / `fix:` / `perf:` / `docs:` / `chore:` の接頭辞。
6. 判断に迷ったら「速い方・単純な方・既定で安全な方」を選び、理由をコメントに残す。

## 1. 計測の作法

**この機械は big.LITTLE** (cpu0-3 = Cortex-A55 / cpu4-7 = Cortex-A78、システムコールのコストが 2.2 倍違う)。
**必ず `taskset` でプロキシを 4-7、ベンチを 0-3 に固定する。** 固定しないとぶれが ±5-10% になり、
数 % の差は見えない (固定すれば ±1%)。

主指標は **CPU/要求** (`/proc/<pid>/stat` の utime+stime ÷ 要求数)。スループットは律速がベンチ側に
移るので補助でしかない。負荷に依らない指標として **1 要求あたりの確保回数** (数えるアロケータ) と
**システムコール回数** (`strace -f -c`) も使う。

メモリは 2 つを見る: **暇な接続 1 本あたりの RSS** (実行時) と、**`rustc` の最大 RSS** (ビルド時、上限 200 MB)。

### 速さの測り方

```bash
# 端末 1: 最速設定でプロキシを起動 (ポート 18080、ループバックのみ)
HOME=/tmp/proxy-bench SERVER_PORT=18080 PROXY_BIND=127.0.0.1 PROXY_ALLOW_LOCAL=on \
PROXY_CACHE_ENABLED=off PROXY_STATS_PERSIST=off PROXY_LOG_LEVEL=warn \
./target/release/rust-http-proxy

# 端末 2: ベンチ (オリジンはベンチ自身が起動する)
./target/release/bench --proxy 127.0.0.1:18080 --conc 8 --seconds 5
```

**big.LITTLE なので、実際にはこう固定して測る**:

```bash
taskset -c 4-7 ./target/release/rust-http-proxy --lite -p 18080 --bind 127.0.0.1 &
taskset -c 0-3 ./target/release/bench --proxy 127.0.0.1:18080 --conc 8 --seconds 10
```

CPU/要求 は `/proc/<pid>/stat` の 14・15 列目 (utime, stime) の差 ÷ 要求数。
同じ内容をキャッシュ有効 (`PROXY_MEM_CACHE_MB=64 PROXY_DISK_CACHE_MB=64 PROXY_CACHE_RESERVE=off PROXY_CACHE_DIR=/tmp/proxy-bench/cache`)
でも 1 回取り、キャッシュを入れても遅くならないことを確認する。

### ビルドのメモリの測り方

```bash
cargo clean && ./scripts/build-memory.sh 200
```

**RSS を測るだけでは機械をまたいだ判定にならない。** メモリ圧がかかっていないとアロケータが
解放済みのページを持ち続けるので、同じビルドでも余裕のある機械ほど大きく出る
(実測: 手元の aarch64 で 194 MB、GitHub の runner では 334 MB)。
そこでこのスクリプトは、cgroup を作れる環境では **実際にその上限の中でビルドして通るか** を見る。
作れない環境 (手元) では参考の RSS を出すだけで判定しない。CI が同じものを回している。

## 2. 現在地

| 項目 | 着手前 | 現在 |
|---|---|---|
| forward, 8 並列 (`--lite`) | 188 req/s, p50 42.0 ms | **32,419 req/s, p50 0.20 ms** (172 倍) |
| forward, 8 並列 (キャッシュ HIT) | — | **75,563 req/s, p50 0.070 ms** |
| forward, 64 並列 | 1,517 req/s, p50 41.9 ms | 25,912 req/s, p50 1.6 ms |
| CONNECT トンネル 1 本 | 1,044 MiB/s | **2,681 MiB/s** |
| CONNECT 確立, 8 並列 | 3,506 tunnels/s | **8,283 tunnels/s** |
| トンネル 1 本あたりのスレッド | 3 | **1** |
| 暇な keep-alive 接続 2,000 本 | 2,003 スレッド / RSS 72.3 MB | **18 スレッド / 25.7 MB** |
| 1 要求あたりの確保回数 | 98.7 | **41.1** |
| 1 要求あたりのシステムコール | 10.06 | **5.78** |
| ビルドの最大 RSS (この機械) | 347 MB | **194 MB** |
| バイナリ | 857 KB | 1,251 KB (release) / **988 KB** (dist) |
| テスト | 150 単体 + 21 結合 | **177 単体 + 41 結合** |

### 完了の定義 (§0 のゴールに対して)

- [x] Phase 0〜1 と T2.1・T2.2・T3.1・T3.2・T3.4 が完了、README の性能節に Rust ベンチの値がある
- [x] forward 8 並列 p50 が 1 ms 台 (loopback) — 実際は **0.20 ms**
- [x] トンネルが 1.5 GiB/s 以上 — **2.6 GiB/s**
- [x] 既定設定で `cargo test --workspace` 全通過 (177 単体 + 41 結合)
- [x] `rust-http-proxy --lite -p 8080` の 1 行で「認証なし・手軽・最速」

## 3. やったこと

### Phase 0 — 計測できるようにする

- [x] **T0.1 std だけで書いたベンチ (`src/bin/bench.rs`)**
  - 目的: Python の上限 (約 600 req/s) を外し、プロキシ自身の限界を測る。
  - やること: `scripts/bench.py` と同じ 3 種 (forward / tunnel / connect) を Rust で。オリジンもベンチ内に持つ
    (固定応答を 1 回の `write_all` で返す TCP サーバー。`TCP_NODELAY` を立てる)。引数は `--proxy`, `--conc`, `--seconds`, `--body-bytes`。
    出力は req/s, MiB/s, p50/p99 (ms) の 1 行ずつ。スレッドは並列数ぶん `std::thread::spawn` で十分。
  - 受け入れ基準: プロキシを通さずオリジン直結で **50,000 req/s 以上** 出る (ベンチが律速しない)。
    `cargo run --release --bin bench -- --proxy 127.0.0.1:18080` で動く。README の「ビルド・テスト」に使い方を 3 行で追記。
    ベースライン表 (このファイルの §1) に Rust ベンチの値を追記する。

- [x] **T0.2 プロファイル取得の手順を書く**
  - やること: `perf record -g` (無ければ `strace -c -f`) でホットパスを見る手順を README の開発者向け節に 5 行で。
    `[profile.release] debug = 1` は入れない (バイナリが太る)。必要なら `CARGO_PROFILE_RELEASE_DEBUG=1` を環境変数で。
  - 受け入れ基準: 手順どおりに `perf report` か `strace -c` の出力が得られる。

### Phase 1 — 少ない変更で確実に速くする (最優先)

- [x] **T1.1 `TCP_NODELAY` を全ソケットに立てる** ← 最重要、実測済み
  - 変更箇所: `src/lib.rs` `handle_client` (クライアント側、`set_write_timeout` の直後)、`src/origin.rs` `connect` (オリジン側)、
    `src/net.rs` `connect_resolved` (Happy Eyeballs で勝った接続。ここに入れればトンネルとオリジンの両方に効く)。
  - やること: `set_nodelay(true)` を呼ぶ (失敗は無視して良い)。プールから取り出した接続は設定が残るので追加処理不要。
  - 受け入れ基準: `scripts/bench.py` の forward 8 並列 p50 が **44 ms → 10 ms 未満**。
    結合テストを 1 つ追加: keep-alive 接続で 5 要求を連続して送り、合計時間が 100 ms 未満であること (Nagle があれば 200 ms 以上かかる)。
    ループバックなので閾値は余裕を持たせ、CI でフレークしないようにする。

- [x] **T1.2 応答ヘッダーと本文を 1 回の書き込みにまとめる**
  - 変更箇所: `src/http/mod.rs` の配信部 (`client.write_all(&client_head)` からループまで)、`src/http/serve.rs` の `write_cached_response`、
    `src/endpoints/mod.rs` の自前エンドポイント応答。
  - やること: クライアントへの書き込みを `BufWriter::with_capacity(64 * 1024, client)` 経由にし、ヘッダー + 最初の本文チャンクが 1 セグメントで出るようにする。
    chunked の枠 (`body::write_chunk`) も同じ writer に書く。応答の最後で必ず `flush()`。
    CONNECT へは影響させない (トンネルは生の `TcpStream` を使い続ける)。keep-alive の次要求の前にバッファが空であることを確認する。
  - 受け入れ基準: `strace -f -e trace=write,sendto -c` で 1 要求あたりのクライアント向け `write` 回数が 2 回以上 → 1 回。全テスト通過。
    キャッシュ HIT の p50 が悪化しない。

- [x] **T1.3 CONNECT トンネルを 1 スレッド + `poll(2)` に、Linux では `splice(2)` でゼロコピー**
  - 変更箇所: `src/tunnel.rs` `tunnel()` (現状: `try_clone` して 2 スレッドで `io::copy`、接続ごとに合計 3 スレッド)。
  - やること:
    1. 両ソケットを non-blocking にし、`poll` で読める側を待って `read`/`write` する 1 ループにする。片側 EOF は `shutdown(Write)` で相手に伝え、両方向が閉じたら終了。
       `poll` の宣言は `src/sysinfo/inotify.rs` に既にある (`PollFd`, `POLLIN`)。共通化して `src/sys.rs` (新規, Linux/Unix 用の薄い syscall 層) に移すと良い。
    2. Linux では `pipe2` + `splice(SPLICE_F_MOVE | SPLICE_F_NONBLOCK)` でカーネル内コピーにする (パイプ容量は 1 MiB に `fcntl(F_SETPIPE_SZ)`、失敗しても続行)。
       `splice` が `EINVAL` などで使えない場合は 1. の read/write に自動フォールバック。
    3. `handle_connect` にアイドルタイムアウトを入れる (T2.2 と同じ値)。現状は `set_read_timeout(None)` で **無期限** なので、死んだトンネルのスレッドが残る。
  - 受け入れ基準: トンネル 1 本のスループットが **881 MiB/s → 1.5 GiB/s 以上** (Rust ベンチで)。`ps -o nlwp` でトンネル 1 本あたりのスレッドが 3 → 1。
    既存の結合テスト (CONNECT を使うもの) と、`prefix` (先読みした ClientHello 相当のバイト列) が最初に送られることのテストが通る。

- [x] **T1.4 キャッシュ無効時・統計無効時に一切の余計な仕事をしない**
  - 変更箇所: `src/http/mod.rs` (`cache_key_variant`, `freshness::*`, `now_epoch`, `Ctx` の `String` 生成)、`src/main.rs` (`history::spawn`, `Cache::spawn_probe`, `blocklist::spawn`)。
  - やること: `cache.enabled()` が false なら鍵の生成・鮮度判定・合流 (`inflight`) を通らない早期分岐を置く。
    キャッシュ無効なら probe スレッドを起動しない、`PROXY_STATS_PERSIST=off` なら persist/history のスレッドを起動しない、ブロックリスト未設定なら fetch スレッドを起動しない (既にそうなっている箇所は確認だけ)。
    `metrics.record_host` / `record_client` の `format!` は info 未満のログレベルでも走るので、ホスト別統計の上位表更新はロック 1 回 + 文字列生成 1 回に抑える。
  - 受け入れ基準: `PROXY_CACHE_ENABLED=off` で `perf`/`strace -c` 上に cache/freshness 由来の関数・`stat`/`open` が出ない。起動直後のスレッド数を README に書く (期待: 待ち受け + 数本)。

- [x] **T1.5 要求解析のアロケーションを減らす (計測してから)**
  - 変更箇所: `src/lib.rs` `handle_client` (`raw_headers: Vec<String>`、`method`/`target` の `to_string`)、`src/http/request.rs` `parse_request_headers`。
  - やること: まず T0.1 のベンチ + `perf` で `alloc` の割合を見る。5% 未満なら **このタスクは飛ばす** (理由をコミットに書く)。
    やるなら 1 要求ぶんのヘッダーを 1 つの `Vec<u8>` に読み、`(start, end)` のスライスで参照する。`MAX_LINE` / `MAX_HEADER_LINES` の制限は維持。
  - 受け入れ基準: forward 64 並列の req/s が 5% 以上向上、または「計測の結果不要」と記録。

- [x] **T1.6 オリジン接続プールのヒット率を上げる**
  - 変更箇所: `src/pool.rs`, `src/main.rs` (`ORIGIN_IDLE` = 30 s)。
  - やること: `is_alive` の `set_nonblocking(true)` → `peek` → `set_nonblocking(false)` は 3 syscall。`MSG_PEEK | MSG_DONTWAIT` の `recv` 1 回にする (Linux、`extern "C"` の `recv`)。
    アイドル保持を 30 s → 60 s。`/status` にプールのヒット/ミス数を出す。
  - 受け入れ基準: forward ベンチで `pool_hits / (hits+misses)` が 95% 以上。テスト `reuses_live_connections_and_drops_dead_ones` などが通る。

### Phase 2 — 認証なし (= 誰でも使える) でも落ちない

- [x] **T2.1 同時接続数の上限 (`PROXY_MAX_CONNS`、既定 4096)**
  - 変更箇所: `src/main.rs` `serve()` (無制限に `thread::spawn`)、`src/config.rs`、`src/reload.rs` (即時反映に含める)。
  - やること: `AtomicUsize` の接続カウンタが上限なら `503 Service Unavailable` + `Retry-After: 1` を書いて閉じる (スレッドは起こさない)。
    上限到達は `warn` で 1 分に 1 回だけログ。`/status` と `/metrics` に `active_connections` と `rejected_overload` を出す (前者は既にある)。
  - 受け入れ基準: 上限 8 で起動し、9 本目が 503 になる結合テスト。ベンチの数値が悪化しない。

- [x] **T2.2 トンネルと keep-alive のアイドルタイムアウト**
  - 変更箇所: `src/tunnel.rs` (T1.3 の `poll` タイムアウト)、`src/config.rs`。
  - やること: `PROXY_TUNNEL_IDLE_SECS` (既定 300、`0` で無期限)。双方向とも `idle` 秒データが無ければ両側を閉じる。
  - 受け入れ基準: 1 秒設定で無通信トンネルが約 1 秒で閉じる結合テスト。README の表に追加。

- [x] **T2.3 (任意・大きい) 1 スレッドで多数のトンネルをさばく `epoll` リアクター** ← 計測の結果、着手しない
  - 前提: T1.3 完了後、Rust ベンチで **同時 5,000 トンネル** を張ったときの RSS とレイテンシを測り、スレッドあたり約 8 MiB のスタック予約が問題になる場合だけ着手。
    先に `thread::Builder::new().stack_size(256 * 1024)` で接続スレッドのスタックを小さくする (これは安価で必ずやる)。
  - やること: 確立済みトンネルの fd を `epoll` (`extern "C"`) で待ち、`splice` で中継する専用スレッドを N (= コア数) 本。HTTP 転送は従来どおりスレッド。
  - 受け入れ基準: 5,000 本のアイドルトンネルで RSS 200 MiB 未満、新規 CONNECT の p99 が 10 ms 未満。

- [x] **T2.4 開放プロキシとしての最小限の安全策 (速度に影響しないもの)**
  - やること: `CONNECT` の宛先ポートが `PROXY_CONNECT_PORTS` (既定 `443,80,8080-8099`… は絞りすぎなので **既定は制限なし**、書式だけ用意) に無ければ 403。
    ループバック・リンクローカル (`169.254.0.0/16`, `fe80::/10`) 宛てのオリジンは `PROXY_ALLOW_LOCAL=on` が無い限り 403 (クラウドメタデータ経由の SSRF 防止。テストは `127.0.0.1` を使うので `PROXY_ALLOW_LOCAL=on` を結合テストの Config に入れる)。
  - 受け入れ基準: 既定設定でメタデータアドレスへの `GET` が 403 になる単体テスト。既存テストは設定追加だけで通る。

### Phase 3 — 手軽にする

- [x] **T3.1 コマンドライン引数 (`--help`, `--version`, `-p/--port`, `--bind`, `--no-cache`, `--quiet`, `--lite`)**
  - 変更箇所: `src/main.rs` 先頭、`src/config.rs`。`std::env::args` を手で解析 (クレート禁止)。
  - やること: 引数は対応する環境変数を `std::env::set_var` する前段として扱う (優先順位: 引数 > `$HOME/.env` > 環境変数。README の説明を更新)。
    `--help` は環境変数の表を短く出す。`--version` は `env!("CARGO_PKG_VERSION")`。
  - 受け入れ基準: `rust-http-proxy -p 3128 --lite` で起動する。`--help` の終了コード 0。不明な引数は使い方を出して終了コード 2。

- [x] **T3.2 `--lite` / `PROXY_PROFILE=lite` = 最速の素通しプロファイル**
  - やること: キャッシュ off、統計永続化 off、ブロックリスト取得 off、ログ `warn`、プローブ無し、`/dashboard` は 404 ではなく「lite mode」の 1 行を返す。
    起動ログ 1 行目に `profile: lite` と出す。
  - 受け入れ基準: `--lite` で起動直後のスレッド数が待ち受け + 2 以下。ベンチの数値が既定プロファイルと同等以上。

- [x] **T3.3 配布: 静的バイナリ・Dockerfile・GitHub Release**
  - やること: `scripts/build-static.sh` (`rustup target add x86_64-unknown-linux-musl` → `cargo build --release --target ...`)。
    musl 静的リンクでは `dlopen(libssl)` が使えないので、起動ログに「TLS: unavailable in static build」と出ることを確認して README に明記
    (HTTPS オリジンのキャッシュだけが無効、CONNECT は影響なし)。
    `Dockerfile` は 2 段 (rust:1.96 でビルド → `scratch` にバイナリのみ、`EXPOSE 8080`, `ENV SERVER_PORT=8080`)。
    `.github/workflows/release.yml`: タグ `v*` で x86_64 / aarch64 (musl) をビルドして Release に添付。
  - 受け入れ基準: `docker build . && docker run -p 8080:8080 <image>` で `curl -x localhost:8080 http://example.com/` が通る (Docker が無い環境では手順とワークフローのみで可、その旨をコミットに書く)。

- [x] **T3.4 README を「30 秒で使える」構成に直す**
  - やること: 先頭を **クイックスタート** (ビルド 1 行、起動 1 行、`curl -x` 1 行、ブラウザ設定 = `/proxy.pac` の URL) にし、機能一覧と環境変数の表はその下へ。
    バイナリサイズの記述を実測 (T1〜T3 完了後の値) に直す。`cargo install --git <このリポジトリ>` を書く。
    §1 の計測値を「性能」節として README に転記する (Rust ベンチの値)。
  - 受け入れ基準: README の先頭 20 行だけで起動と動作確認ができる。

- [x] **T3.5 CI (`.github/workflows/ci.yml`)**
  - やること: push / PR で `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build --release`。
    ベンチは `bench --seconds 2` を実行して出力を貼るだけ (閾値では落とさない)。
  - 受け入れ基準: ワークフローが緑。

### Phase 4 — 品質の下支え (余力があれば)

- [x] **T4.1 要求解析の堅牢性テスト**: 不正な要求行、巨大ヘッダー、`\r` 無しの行、CONNECT の宛先にパスが付く、IPv6 リテラルなどを流し、
  必ず 400/414/431 のいずれかを返して接続が閉じる (パニックしない) ことを結合テストで確認する。
- [x] **T4.2 `opt-level = "s"` と `"3"` を比較**: ベンチで 5% 以上速いなら `3` に変更 (サイズ増は 1 MiB まで許容)。結果を README に 1 行。
- [x] **T4.3 `SO_REUSEPORT` で待ち受けスレッドをコア数ぶん**: 計測の結果 **不要**。
  `connect` ベンチ中の accept スレッドの CPU は 8 並列 8,283 tunnels/s のときで 2,250 ms / 5,000 ms
  (1 コアの 45%)、32・64 並列ではむしろ下がる (プロセス全体は 3.5 コアぶん使っており、機械側が飽和している)。
  accept は律速していないので `SO_REUSEPORT` は入れない。accept スレッドの 1 接続あたり 54 us の大半は
  スレッド生成で、これを削るならスレッドプール化が要るが、それは TASKS の範囲外。

### Phase 5 — 「前提を疑う」ラウンド

Phase 0〜4 の後、設計上の前提を 8 つの観点から疑い直した。実測で裏付けの取れたものだけ入れた。

- [x] **T5.1 素通しできる本文を `splice(2)` で運ぶ**
  - 目的: 中継バッファを経由せず、カーネル内でコピーする。
  - 変更箇所: `crates/http/src/http/mod.rs` の `passthrough`。
  - 受け入れ基準: 大きい本文の CPU/要求が下がり、小さい本文が退行しないこと。
  - 結果: 本文 1 MiB の CPU/要求 771.6 → **302.9 us** (-60%)。しきい値 128 KiB 未満は従来経路 (`5eac139`)

- [x] **T5.2 オリジンプールの既定を 8 → 64 本、全体上限 256**
  - 目的: 高並列で張り直しが増えていた。
  - 結果: conc=64 の p99 37.3 → **12.1 ms** (`0980886`)

- [x] **T5.3 中継バッファを 64 → 32 KiB**
  - 結果: 本文 64 KiB で -8%、1 MiB で -11%、1 KiB は退行なし (`093c68e`)

- [x] **T5.4 ヘッダー組み立ての中間 `Vec<String>` を外す**
  - 結果: 確保 76.1 → **41.1 回/要求** (-46%)、CPU -8.9% (`9f552fc`)

- [x] **T5.5 キャッシュ HIT の本文直接書き・ヘッダー再解析の省略**
  - 結果: 256 KiB の HIT で user CPU 50.3 → **24.3 us** (`cbb2c00`)

- [x] **T5.6 malloc のアリーナ数に上限を掛ける (`PROXY_MALLOC_ARENAS`、既定 8)**
  - 目的: glibc の既定は「コア数 × 8」で、接続ごとにスレッドが増えると使われないアリーナが RSS に居座る。
  - 結果: アイドル接続あたり 80.5 → **26.8 kB** (-67%)。代償は conc=64 の CPU +4% (conc=8 は差なし) (`05e6c43`)

- [x] **T5.7 見つかった不具合を直す**
  - **要求スマグリング** (`6edc748`): `Connection: Content-Length` と指名されると、転送するヘッダーからは
    落ちるのに本文は送られ、オリジンがそれを次の要求の先頭として読む。認証なしの開放プロキシなので誰でも踏めた。
    枠組みのヘッダーは指名されても落とさないようにした。
  - **要求ヘッダー全体に上限が無い** (`e768ece`): 1 行 64 KiB × 256 行 = 16 MiB を正常な形で送れ、
    行バッファの使い回しでそれが接続の間ずっと居座る。同時 8 接続で RSS 15 → 272 MiB。
    合計 128 KiB の上限を入れて 272 → **17 MiB**。
  - **`Pool::sweep()` に呼び出し元が無い** (`0980886`): 負荷が止まってもアイドル接続を解放しない
    (100 秒放置後も fd 54 のまま → 修正後は 7)。
  - **URL 正規化が 2 実装に分かれていた** (`5ffaaf5`): 乖離を捕まえるテストつきで 1 実装にまとめた。

### Phase 6 — アイドル接続をスレッドから外す (方針 C)

「1 接続 = 1 スレッドが専任」という前提を外す。要求を処理していない keep-alive 接続はスレッドを手放し、
1 本の監視スレッド (epoll) に預ける。挙動が変わらないことを確かめながら 5 段階に割って進めた。

- [x] **T6.1 クライアントの読み取りバッファとストリームを分ける**
  - 目的: 預けるには、バッファを接続から切り離せる必要がある。
  - 変更箇所: `crates/net/src/clientio.rs` (`ClientBuf` / `ClientReader`)。
  - 結果: 挙動不変。CPU/要求に差なし (`c688372`)

- [x] **T6.2 接続の状態を `struct Conn` にまとめる**
  - 目的: 270 行 1 枚の `handle_client` が状態を全部ローカル変数で抱えていて、持ち運べなかった。
  - やること: `struct Conn` / `enum Step` / `serve_one` / `pump` に割る。行の置き場は接続あたり 1 回だけ借りる。
  - 結果: 38.0 → 38.3 us/要求 (ぶれ ±1% の中)。要求ごとに借りて返す形は +1.5% だったのでやめた (`931a276`)

- [x] **T6.3 `sys` に epoll の束縛を足す**
  - 注意: `struct epoll_event` は x86_64 (と x32) だけ packed で 12 バイト、それ以外は 16 バイト。
    食い違うと `epoll_wait` が書き戻す配列の刻みがずれて token が化ける。arch ごとに `repr` を変えて `size_of` を const で突き合わせる。
  - 結果: テスト 4 本。動作環境は aarch64 (16 バイト側) (`19b4c3d`)

- [x] **T6.4 `IdleWatch` を足して配線する (既定 off)**
  - 変更箇所: `src/idle.rs` (新規)、`src/lib.rs`、`crates/stats/src/config.rs`。
  - 受け入れ基準: 暇な接続を握ったときのスレッド数と RSS が下がり、既存テストが通ること。
  - 結果: 暇な接続 1,000 本で 1,003 スレッド 44.4 MB → **12 スレッド 25.6 MB** (`8a4fbc4`)

- [x] **T6.5 猶予を読み取りタイムアウトに畳んで既定 on にする**
  - 目的: 猶予を `poll(2)` で待つと、忙しい接続に要求ごとの `ppoll` が 1 回増えて CPU +5.5% だった。
  - やること: 2 回目以降の読み取りタイムアウトを猶予の長さにして、空振りを「暇だ」と解釈する。
    要求の途中で猶予を過ぎたら本来のアイドル時間まで待ち直す。
  - 結果: システムコール 6.809 → **5.780 回/要求** (park=off と同じ)、CPU 40.30 → **38.69 us** (`ba8a240`, `bca3faf`)

- [x] **T6.6 途中で見つけた不具合を直す**
  - **要求を処理したスレッドが終わるとプロセスごと abort** (`f683da4`): T6.2 で入れた不具合。
    スレッドローカルに `Drop` を持つ型を置いてしまい、スレッド終了時の破棄でその置き場自身を触って
    `thread local panicked on drop` になる。ワーカースレッドは 30 秒で自分から終わるので
    **1 要求通してから 30 秒後に必ず落ちていた**。回帰テストつき。
  - **期限切れの一斉 close で監視スレッドが止まる** (`196eb06`): 1 周 16 本で切る。
    実測では差が出なかったが、越えたときの被害 (数百 ms 全接続が止まる) と釣り合わないので入れた。
  - **再検証後の HIT 判定が負荷次第で落ちる** (`69b384f`): テスト側の競合。

### Phase 7 — ビルドと構造

- [x] **T7.1 1 クレートを 7 つの層に割る**
  - 目的: `cargo run --release` が動作環境で SIGKILL (OOM) される。ビルドの最大 RSS が 347 MB あった
    (**この超過は前からで、`05e6c43` の時点で既に 339.7 MB**)。
  - 変更箇所: `crates/{base,net,store,cache,stats,http}` を新設し、`src/` は受け付けと keep-alive だけにする。
  - やること: `rustc` はクレート単位で全部を一度に抱えるので、行数がそのまま最大 RSS になる
    (実測: 空クレート 37.8 MB、1,046 行 148.5 MB、18,276 行 330 MB)。層ごとに別クレートへ割る。
    各クレートは下の層を同じ名前で再エクスポートし、`crate::sync` のような書き方を移設前のまま通す。
  - 受け入れ基準: どのクレートの `rustc` も最大 RSS 200 MB 未満。テストの本数が変わらないこと。
  - 結果: 347 → **194 MB**。分割そのものの代償は CPU +1%。LTO を切ったぶんが +3.9% で、
    配布バイナリは LTO ありの `dist` プロファイルで作る。`scripts/build-memory.sh` を CI に入れた (`1a9e11a`)

- [x] **T7.2 2,214 行の結合テストを責務ごとに 4 本へ分ける**
  - 目的: 1 ファイル 2,000 行の目安を超えていた。
  - 結果: `common` 586 / `proxy_test` 728 / `cache_test` 604 / `keepalive_test` 263 / `tunnel_test` 92 行。
    中身は動かしていない (218 本すべて通過) (`4ca9465`)

## 4. 測って採らなかったもの

同じ道を二度調べないための記録。**すべて実装して測ったうえで戻した / 入れなかった**もの。

| 案 | 測った結果 | 判断 |
|---|---|---|
| `epoll` リアクターでトンネルを多重化 (T2.3) | 5,000 本のアイドルトンネルで RSS 198 MiB、新規 CONNECT p99 9.1 ms | 受け入れ基準 (200 MiB / 10 ms) を満たすので作らない |
| `SO_REUSEPORT` で待ち受けを複数に (T4.3) | accept スレッドの CPU は 1 コアの 45%、並列を上げるとむしろ下がる | accept は律速していない |
| `opt-level = 3` (T4.2、当時) | +2.2%、バイナリ +131 KB | 5% に届かないので見送り (T7.1 で LTO を切ったときに採用) |
| musl 静的リンク | ビルドがメモリ 200 MB に収まらない。`dlopen(libssl)` も使えず HTTPS オリジンのキャッシュが死ぬ | 採らない |
| 統計 `Mutex` の分割 | 統計を丸ごと消したビルドでも `futex` 0.010 vs 0.011 回/要求、p99・CPU とも差なし | 期待効果 0〜0.25% |
| `recvfrom` を 3 → 2 回に (プールの生存確認の省略) | — | プールはクライアント間で共有なので、desync すると**他人の応答**が渡り、誤った URL のキャッシュとして固定化する。期待値 1% のために踏む橋ではない |
| クライアント書き込みバッファのプール化 | アイドル接続あたり 30.3 → 30.9 kB、CPU 43.3 → 43.9 us | 改善なし。アイドル接続の資源はスレッドスタックが主因だった |
| 猶予を `poll(2)` で待つ | `ppoll` が 1 回/要求 増えて CPU +5.5% | 読み取りタイムアウトに畳めば 0 回 (T6.5) |
| `PROXY_PARK_MAX_GRACE` (猶予待ちスレッドの上限) | — | T6.5 で猶予が「普通に次の要求を読んでいる状態」になり、数えて止める意味が消えた |
| 期限切れの close をワーカーに投げる | — | 受け渡し (Mutex + channel + スレッド起床) が隠したい `close` と同じ桁。空きスレッド上限 64 を越えると新規スレッドを起こす |
| クレート分割で `lto = "thin"` | ビルド最大 334 MB、CPU 39.4 us | 200 MB に届かない。`lto = false` (194 MB / 39.7 us) を採る |
| クレート分割後に `#[inline]` を当てる | 41.7 us (当てる前 41.4 us) | 効かない。LTO を切った損は個々の小さい関数ではなかった。`opt-level = 3` の方が効いた (41.4 → 39.7 us) |

## 5. これから (未着手)

上と同じ形式。**着手する前に必ず「今どうなっているか」を測る**こと。

- [ ] **T8.1 CONNECT トンネルも暇なときは監視スレッドに預ける**
  - 目的: トンネルは今も 1 本 1 スレッド。実測で同時 5,000 本のとき RSS 140 MiB。
    HTTP 側と同じ `IdleWatch` がもうあるので、両方向が暇なトンネルは預けられるはず。
  - 変更箇所: `crates/http/src/tunnel.rs`、`src/idle.rs`。
  - やること: `splice` のループが両方向とも `WouldBlock` になったら、2 つの記述子を epoll に預けて
    スレッドを解放する。どちらかが読めるようになったらワーカーへ戻す。
    **注意**: 今の `IdleWatch` は記述子 1 本と `Box<Conn>` の対応しか持てない。トンネルは 2 本要る。
  - 受け入れ基準: 同時 5,000 本のアイドルトンネルでスレッド数が 5,000 → 数十、RSS が下がること。
    スループット (2.6 GiB/s) と新規 CONNECT の p99 (9.1 ms) が悪化しないこと。

- [ ] **T8.2 1 接続 1 要求のときの CPU 98 us/要求 を下げる**
  - 目的: keep-alive が効かないクライアント (curl の既定など) では、いまも 1 要求あたり 98 us かかる。
    表の中でいちばん悪い数字。
  - やること: まず `strace -f -c` と CPU/要求 の内訳を取る (accept、スレッドの受け渡し、ソケットの後始末の比)。
    測ってから決める。スレッド置き場は既にあるので、残っているのは accept まわりのはず。
  - 受け入れ基準: 内訳を出したうえで、5% 以上下がるか「効かない」と記録する。

- [ ] **T8.3 LTO を切って失った 4.9% を埋める**
  - 目的: `release` (LTO なし) 39.7 us と `dist` (LTO あり) 38.1 us の差。動作環境で動くのは前者。
  - やること: 両者の `perf` を突き合わせて、差が出ている関数を特定する。`#[inline]` の総当たりは
    効かないことが分かっている (§4) ので、**どこで差が出ているかを先に測る**。
  - 受け入れ基準: 差を半分以下にするか、「LTO でしか埋まらない」と根拠つきで記録する。

- [ ] **T8.4 1 要求あたりの確保 41.1 回の内訳を出す**
  - 目的: 98.7 → 41.1 まで来たが、残りが何なのかは数えていない。
  - やること: 数えるアロケータに呼び出し元を記録させて上位を出す。5% 以上減らせるものだけ潰す。
  - 受け入れ基準: 内訳の表を TASKS に残す。

- [ ] **T8.5 `PROXY_MAX_CONNS` の既定 4096 を見直す**
  - 目的: 上限の意味が変わった。以前は「同時に立つスレッド数」の歯止めだったが、
    アイドル接続を預けるようになったので、いまは実質「記述子の数」の歯止め。
  - やること: `ulimit -n` との関係を確かめ、既定値の根拠を決め直して README に書く。
  - 受け入れ基準: 既定値の根拠が 1 行で説明できること。

- [ ] **T8.6 ビルドのメモリに余裕を作る**
  - 目的: いま最大 194 MB で上限 200 MB。行が増えると超える (CI が落ちる)。
  - やること: 超えたクレート (いまは `proxy-stats` と `proxy-cache`) をさらに割る。
    割る前に `scripts/build-memory.sh` で今の値を出すこと。
  - 受け入れ基準: どのクレートも 180 MB 未満。

## 付録 A. 計測の記録

着手時からの数字の履歴。**現在地は §2**。

**着手時に判明していた最大のボトルネック** (T1.1 で解消): プロキシが `TCP_NODELAY` を立てていないため、
応答ヘッダーと本文を別々に `write` した際に Nagle + delayed ACK で **1 要求あたり約 40 ms 止まっていた**。
両側に `set_nodelay(true)` を入れると 8 並列で **176 → 674 req/s、p50 44 → 9.6 ms**
(直結と同等、つまり当時の Python ベンチの上限に到達)。

### 着手前 (2026-09-07 計測)

環境: 8 コア / 6.6 GiB RAM、loopback、`target/release` (opt-level=s, lto)、`PROXY_CACHE_ENABLED=off PROXY_STATS_PERSIST=off PROXY_LOG_LEVEL=warn`。
計測ツールは `scripts/bench.py` (Python 標準ライブラリのみ)。**Python 側が上限になる** ので絶対値は低めで、前後比較専用。

| 項目 | 値 | 備考 |
|---|---|---|
| forward, 8 並列 keep-alive, 1 KiB 応答 | **176 req/s, p50 44 ms** | プロキシ無しの直結は 621 req/s, p50 9.6 ms |
| forward, 64 並列 | 371 req/s, p50 66 ms, p99 1.5 s | p99 の悪さは Python オリジン由来の可能性あり (T0.1 で確定させる) |
| CONNECT トンネル 1 本のスループット | 881 MiB/s | 256 MiB を Python から送出 |
| CONNECT 確立/秒, 64 並列 | 398 tunnels/s | 短命トンネル |
| リリースビルド時間 | 25〜28 s | クリーンビルド |
| バイナリサイズ | 857 KB | README の「約 500KB」は古い。T3.4 で直す |
| テスト | 150 単体 + 21 結合、約 3 s | |

#### Rust ベンチ (`cargo run --release --bin bench`、T0.1 で追加。以後はこちらを正とする)

同じ環境、`--body-bytes 1024`。`direct` はプロキシを通さないオリジン直結 (ベンチ自身の上限)。

| 項目 | T0.1 時点 | T1.1 (TCP_NODELAY) 後 | 備考 |
|---|---|---|---|
| direct, 8 並列 | **300,805 req/s, p50 0.013 ms** | 300,542 req/s, p50 0.014 ms | ベンチは律速しない (基準 50,000 req/s) |
| forward, 8 並列 | 188 req/s, p50 42.0 ms, p99 54.1 ms | **29,107 req/s, p50 0.222 ms, p99 1.33 ms** | 155 倍 |
| forward, 64 並列 | 1,517 req/s, p50 41.9 ms, p99 51.3 ms | 22,027 req/s, p50 1.97 ms, p99 15.8 ms | |
| tunnel 1 本 | 1,044 MiB/s | 1,114 MiB/s | 256 MiB。T1.3 で 1.5 GiB/s 以上へ |
| connect, 8 並列 | 3,506 tunnels/s, p50 1.85 ms | 3,938 tunnels/s, p50 1.70 ms | |
| connect, 64 並列 | 3,648 tunnels/s, p50 17.3 ms | 3,288 tunnels/s, p50 18.1 ms | |

T1.2 (BufWriter) 後: forward 8 並列 30,231 req/s, p50 0.216 ms。
キャッシュ HIT (`--cacheable`, メモリ 64 MiB) は 60,813 → **75,563 req/s**、p50 0.091 → 0.070 ms。

T1.3 (poll + splice) 後: トンネル 1 本 1,114 → **2,800 MiB/s**、CONNECT 確立 3,704 → **7,375 tunnels/s**
(p50 1.77 → 0.68 ms)、トンネル 1 本あたりのスレッド 3 → **1**、待ち受けのみのスレッド数 5。

T1.4 後: `PROXY_CACHE_ENABLED=off PROXY_STATS_PERSIST=off` の起動直後スレッド数 5 → **3**
(既定は 6)。forward 8 並列 30,699 req/s、CONNECT 確立 8,293 tunnels/s。

T1.5 の計測 (conn 1 本、1 要求あたりの内訳、`Instant` を一時的に仕込んで測定):

| 区間 | 時間 | |
|---|---|---|
| 要求解析 (parse_request_headers + parse_origin) | 4.9 us | 全体 128 us の **3.8%** |
| キャッシュ判定 | 2.2 us | |
| 要求ヘッダー組み立て | 1.6 us | |
| オリジン接続の取得 (プール) | 5.2 us | |
| オリジン往復 | 61.2 us | 大半は待ち時間 |
| 応答ヘッダー整形 | 5.6 us | |
| 本文の配信 | 24.2 us → 17.9 us | 毎要求 64 KiB を確保・ゼロ埋めしていた |
| 統計・アクセスログ | 3.3 us | |

確保回数は 1 要求あたり 98.7 回 (一時的に数えるアロケータを入れて測定)。解析の割合が 5% 未満なので
**ヘッダーのスライス化は見送り**、代わりに計測で見つかった中継バッファの確保を潰した。

T1.5 後: forward 1 並列 6,697 → **8,286 req/s**、8 並列 30,699 → **32,842**、64 並列 22,392 → **25,912 (+15.7%)**。

T1.6 後: `pool_hit_ratio` = **0.9712** (基準 0.95)、forward 8 並列 32,074 req/s、64 並列 26,142 req/s。

T2.3 の判断のための計測 (同時 5,000 本のアイドルトンネル):

| | RSS | fd/本 | 新規 CONNECT p50 / p99 |
|---|---|---|---|
| 接続スレッドのスタック 256 KiB のみ | 199 MiB | 7.0 | 1.18 / 9.07 ms |
| + パイプの遅延生成・CONNECT 中の BufReader 解放 | **198 MiB** | **2.0** | 1.59 / 9.13 ms |

受け入れ基準 (RSS 200 MiB 未満、p99 10 ms 未満) を満たすので **epoll リアクターは作らない**。
RSS の大半はスレッドスタックの実使用ぶん (1 本あたり約 36 KiB) で、fd とヒープはもう削ってある。

T3.2 (lite プロファイル) の比較 (同じ機械で連続して測定、8 並列):

| プロファイル | forward | connect | 起動直後のスレッド |
|---|---|---|---|
| 既定 (キャッシュ + 統計あり) | 19,475 req/s, p50 0.295 ms | 7,440 tunnels/s | 6 |
| `--lite` | **33,125 req/s, p50 0.199 ms** | **8,350 tunnels/s** | **3** |

T4.2 `opt-level` の比較 (`--lite`、8 並列、forward を 3 回測った平均):

| | forward | tunnel | connect | バイナリ |
|---|---|---|---|---|
| `"s"` (現状) | 32,419 req/s | 2,681 MiB/s | 8,177/s | 923,040 B |
| `3` | 33,121 req/s (+2.2%) | 2,577 MiB/s | 8,231/s | 1,054,112 B |

5% に届かないので `"s"` のままにする。

### 追加: 確保回数とシステムコールを減らす (perf が入ってから)

`perf` の内訳で `malloc` 6.6% + `cfree` 5.1% が見えたので手を入れた。

1. **確保を減らす**: 要求行・ヘッダー行のバッファをスレッドで持ち続ける (解放しない)、
   要求行の分解を借用にする、`split_host_port` に文字列を作らない版を足す、
   応答ヘッダーの読み取りバッファを 1 本使い回す。
   → **1 要求あたりの確保が 90.2 → 76.3 回** (数えるアロケータで決定的に測定)。
   ただし**速度は誤差の中** (CPU/要求 91.8 → 93.3 us、この機械のノイズは ±5%)。

2. なぜ効かないかを調べた: プロキシの CPU は **63% がカーネル側** (要求あたり ユーザー 33 us /
   カーネル 56 us)。`perf` はこの環境ではユーザー空間しか数えないので、ユーザー空間の
   確保・解放 12% = 全体の約 4 us。14 回減らしても 0.6 us で、測れなくて当然だった。

3. **そこでシステムコールを数えた** (`strace -f -c`) — 1 要求あたり **10.06 回**、うち
   `setsockopt` が 4.00 回、`getsockname` が 1.00 回で、半分が間接費だった。潰した:
   - `local_addr()` は接続ごとに 1 回だけ (要求ごとの `getsockname` を削除)
   - クライアントの読み取りタイムアウトは値が変わるときだけ設定し、本文付きの要求のときだけ戻す
     (要求行とヘッダーは keep-alive のアイドル時間で待つ = 遅い相手にはむしろ厳しくなる)
   - プールから出した接続は、設定済みのタイムアウトが同じなら `setsockopt` を呼ばない

   | | システムコール/要求 | CPU/要求 | forward |
   |---|---|---|---|
   | 前 | 10.06 (setsockopt 4.00, getsockname 1.00) | 89.9 us (user 34.6 / kernel 55.3) | 25,321 req/s |
   | 後 | **5.06** (recvfrom 3.00 + sendto 2.00 だけ) | **85.0 us** (user 32.3 / kernel 52.8) | **26,685 req/s** |

   3 回測って 3 回とも改善 (**-5.4% CPU / +5.4% スループット**)。残る 5 回は本質的な入出力。

### 最終値 (Phase 0〜4 完了時点)

| 項目 | 着手前 | 現在 |
|---|---|---|
| forward, 8 並列 (`--lite`) | 188 req/s, p50 42.0 ms | **32,419 req/s, p50 0.20 ms** (172 倍) |
| forward, 8 並列 (キャッシュ HIT) | — | **75,563 req/s, p50 0.070 ms** |
| forward, 64 並列 | 1,517 req/s, p50 41.9 ms | 25,912 req/s, p50 1.6 ms |
| CONNECT トンネル 1 本 | 1,044 MiB/s | **2,681 MiB/s** (2.6 GiB/s) |
| CONNECT 確立, 8 並列 | 3,506 tunnels/s | **8,283 tunnels/s** |
| トンネル 1 本あたりのスレッド | 3 | **1** |
| 起動直後のスレッド | 6 | 6 (既定) / **3** (`--lite`) |
| 同時 5,000 トンネル | 未計測 | RSS 198 MiB、新規 CONNECT p99 9.1 ms |
| バイナリ | 857 KB | 923 KB |
| テスト | 150 単体 + 21 結合 | **158 単体 + 28 結合** |

### perf / strace での確認 (後から両方が入ったので取り直した)

- **T0.2**: `CARGO_PROFILE_RELEASE_DEBUG=1` だけでは `profile.release` の `strip = true` に消されるので、
  `CARGO_PROFILE_RELEASE_STRIP=none` も要ることが分かった (README を修正)。付ければ
  `perf record -g -p <pid>` → `perf report` でシンボルが出る。
- **T1.2**: `strace -f -e trace=write,sendto` で 1 要求あたりのクライアント向け呼び出しを直接確認した
  (Rust の `TcpStream` は `write` ではなく `sendto` を使う)。

  ```
  変更前 (4717a3d): sendto(7, "HTTP/1.1 200 OK...", 90)   ← ヘッダー
                    sendto(7, "xxxxxxxx...",        1000) ← 本文
  変更後 (現在):    sendto(7, "HTTP/1.1 200 OK...", 1090) ← 1 回
  ```

- **T1.4**: `PROXY_CACHE_ENABLED=off` のとき、`perf report` に `cache::` / `freshness::` のシンボルは
  出ない (唯一出るのは起動時の `Ballast` の drop が 0.01%)。`strace -c` のファイル系システムコールも
  28,644 要求で `openat` 18 回・`statx` 2 回・`mkdirat` 2 回 (すべて起動時) で、要求あたり 0。
  キャッシュ有効時は 58,078 要求で `openat` 283 回・`mkdirat` 257 回。

参考: 旧 `scripts/bench.py` でも forward 8 並列 p50 44 ms → **11.0 ms** になった (直結が p50 9.6 ms
なので Python 側の下限に張り付いている。実際の値は Rust ベンチの 0.20 ms)。
