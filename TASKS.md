# TASKS.md — 認証なし・手軽・最速の HTTP Proxy にするための作業一覧

**この文書の形式**: 済んだ作業もこれからの作業も、同じ 1 つの形で書く。

```
- [x] **Tn.n 題**
  - 目的: なぜやるか
  - 変更箇所: どのファイル (済んだものは、いまコードがある場所)
  - やること: 何をするか
  - 受け入れ基準: どうなったら終わりか
  - 結果: 実測値と結論 (`コミット`)   ← 終わったものだけ
```

1 タスク = 1 コミット。性能に関わるものは **変更前後を必ず測って** コミットメッセージ本文に貼る。
測って「やらない」と決めたものは消さずに §4 に残す (同じ道を二度調べないため)。
番号は付けた順で、並び替えても振り直さない (コミットメッセージから引けるように)。

| 節 | 何が書いてあるか |
|---|---|
| §0 | ゴールと、守ること |
| §1 | 計測の作法 (道具と手順) |
| §2 | 現在地 (数字の一覧) |
| §3 | やったこと (Phase 0〜7) |
| §4 | 測って採らなかったもの |
| §5 | これから (Phase 8〜9) |
| 付録 A | 計測の記録 (時系列) |

## 0. ゴールと前提

**ゴール**: 「バイナリを 1 つ置いて起動するだけで使える、認証なしの HTTP/HTTPS(CONNECT) プロキシ」を、
同じ条件でこれ以上速くならないところまで速くする。**速さは必ず計測して示す** (印象で判断しない)。

**守ること (非目標も含む)**:

- **認証は入れない**。`Proxy-Authorization` を要求するコードは書かない (履歴上 `dad8984 refactor: remove authentication` で意図的に外した)。
- **外部クレートは追加しない**。依存してよいのはこのリポジトリのコードだけ (`crates/` のワークスペースメンバー)。
  crates.io のクレートは 1 つも入れない (`libc` も入れない)。
  システムコールが必要なら `crates/sys/src/sys.rs` (`poll` / `epoll` / `splice` / `recv`)、`crates/sys/src/signal.rs`、
  `crates/sysinfo/src/sysinfo/inotify.rs`、`crates/tls/src/tls.rs` (`dlopen`) と同じく `unsafe extern "C"` で直接宣言し、
  `#[cfg(target_os = "linux")]` で囲んで他 OS には従来コードへのフォールバックを残す。
- **既存機能を壊さない**。`cargo test --workspace` は常に全通過。キャッシュ・ダッシュボード等は
  「使わないときに一切コストがかからない」ようにするのが方針で、削除はしない。
- Rust は `edition = "2024"`、`rustc 1.96` で動くこと。`cargo clippy --workspace --all-targets` の警告を増やさない。
- **`cargo build --release` はメモリ 200 MB で通ること** (動作環境のコンテナが小さい)。
  `scripts/build-memory.sh` が CI で見張っている (実際に 200 MB の cgroup に入れてビルドする)。
  効くのは 2 つだけ: **クレートを小さく割ること** (`rustc` はクレート単位で全部を抱える。
  **ファイルを割っても下がらない**) と、**`jobs = 1`** (`.cargo/config.toml`。並列に走る rustc の
  合計が上限を超えるため)。
- **1 ファイルは 2,000 行を目安**にする。超えたら責務で割る。
- コメントとログの文体は既存に合わせる (コメントは日本語、ログ文字列は英語)。
- 動作環境は Linux (Pterodactyl コンテナ) が主。`SERVER_PORT` / `SERVER_MEMORY` / `$HOME/.env` の扱いは変えない。

**作業の進め方 (毎タスク共通)**:

1. 着手前に対象ファイルを読み、README の該当箇所を確認する。**着手前に「今どうなっているか」を測る。**
2. 実装 → `cargo fmt` → `cargo clippy --workspace --all-targets -- -D warnings` → `cargo test --workspace` → `cargo build --release`。
3. 性能に関わるタスクは **変更前後の計測値** を取り、コミットメッセージ本文に貼る (§1)。
4. 環境変数や挙動を足したら README の表と説明を更新する。
5. コミットメッセージは既存に合わせて `feat:` / `fix:` / `perf:` / `docs:` / `chore:` / `refactor:` / `test:` の接頭辞。
6. 判断に迷ったら「速い方・単純な方・既定で安全な方」を選び、理由をコメントに残す。
7. 終わったらこの文書の該当タスクを `[x]` にし、`結果:` に実測値とコミットを書く。§2 の表に効く数字が変わったら更新する。

## 1. 計測の作法

**この機械は big.LITTLE** (cpu0-3 = Cortex-A55 / cpu4-7 = Cortex-A78、システムコールのコストが 2.2 倍違う)。
**必ず `taskset` でプロキシを 4-7、ベンチを 0-3 に固定する。** 固定しないとぶれが ±5-10% になり、
数 % の差は見えない。

主指標は **CPU/要求** (`/proc/<pid>/stat` の utime+stime ÷ 要求数)。スループットは律速がベンチ側に
移るので補助でしかない。負荷に依らない指標として **1 要求あたりの確保回数** (数えるアロケータ) と
**システムコール回数** (`strace -f -c`) も使う。

メモリは 2 つを見る: **暇な接続 1 本あたりの RSS** (実行時) と、**ビルドが通る最小の cgroup 上限** (ビルド時、上限 200 MB)。

### 速さの測り方

`scripts/cpu-per-request.sh` が「固定して起動 → ベンチ → utime+stime の差 ÷ 操作数 → ピーク RSS とスレッド数」までやる。
ベンチ (`crates/bench`) は既定のビルド対象から外してあるので `cargo build --release -p proxy-bench` で作る。

```bash
scripts/cpu-per-request.sh                                   # keep-alive の forward (既定 --conc 8 --seconds 10、--lite)
scripts/cpu-per-request.sh --no-keepalive                    # 1 接続 1 要求 (接続あたりの固定費)
scripts/cpu-per-request.sh --only connect                    # CONNECT の確立
scripts/cpu-per-request.sh --only tunnel --conc 1            # トンネル 1 本 (CPU/MiB)
PROXY_ARGS="" PROXY_MEM_CACHE_MB=64 PROXY_DISK_CACHE_MB=64 PROXY_CACHE_RESERVE=off \
  PROXY_CACHE_DIR=/tmp/proxy-bench/cache scripts/cpu-per-request.sh --cacheable   # キャッシュ HIT
```

手で回すなら、中身はこれと同じ:

```bash
taskset -c 4-7 ./target/release/rust-http-proxy --lite -p 18080 --bind 127.0.0.1 &
taskset -c 0-3 ./target/release/bench --proxy 127.0.0.1:18080 --conc 8 --seconds 10
```

**ぶれの扱い**: 同じ設定でも 5 秒の計測で ±8% ぶれることがある (2026-09-07 深夜の実測: 50.9 / 55.3 / 58.9 us)。
数 % の差を見るときは **変更前と変更後のバイナリを交互に 3 回ずつ** 10 秒で回し、中央値で比べる。
片方を 3 回続けて測ると、機械の状態の変化 (熱・他プロセス) が差に化ける。

**システムコールの数え方**: `strace -p` はこの環境では `ptrace` が拒まれるので、プロキシを **strace の下で起動する**
(`strace -f -c -o out.txt taskset -c 4-7 target/release/rust-http-proxy --lite ...`)。止めるときは strace ではなく
プロキシ (strace の子プロセス) に SIGINT を送る。回数 ÷ 操作数 が 1 要求 (または 1 接続) あたりの値。

**プロファイル**: `CARGO_PROFILE_RELEASE_STRIP=none CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --release` でシンボルを残し、
`perf record -g -p <pid>` → `perf report`。この環境の `perf` は**ユーザー空間しか数えない** (カーネル側は utime/stime の差で見る)。

### ビルドのメモリの測り方

```bash
cargo clean && ./scripts/build-memory.sh 200                 # 200 MB の cgroup で通るか
./scripts/build-memory.sh --find 100 110 120 130 140 150      # 通る最小の上限を探す
```

`.cargo/config.toml` で `jobs = 1` にしてある。既定の並列数だと複数の `rustc` が同時に走り、その合計が上限を超えて
OOM killer に落とされる (実測: 200 MB の cgroup で、並列だと落ち、`-j 1` なら通る)。代償はビルド時間で 8 コアの機械で 14.9 → 32.3 秒。

**RSS を測るだけでは機械をまたいだ判定にならない。** メモリ圧がかかっていないとアロケータが解放済みのページを持ち続けるので、
同じビルドでも余裕のある機械ほど大きく出る (実測: 手元の aarch64 で 194 MB、GitHub の runner では 334 MB)。
そこでこのスクリプトは **実際にその上限の cgroup の中でビルドして通るか** を見る。CI は毎回 200 MB で回す。

**手元でも判定できる** (T9.0)。この機械は PID 1 が systemd ではないので `sudo systemd-run --scope` は使えないが、
**ユーザーの systemd** (`systemd-run --user --scope -p MemoryMax=…`) は動いていて上限も効く。スクリプトは
「システムの systemd → ユーザーの systemd → 参考の RSS だけ」の順に落ちる。
いま通る最小は **CI で 110 MB、手元で 100 MB** (上限 200 MB に対し 90 MB の余裕)。手元の方が 10 MB 低く出るので、
手元の数字で判断するときは **+10 MB 見ておく**。

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
| ビルドが通る最小のメモリ | 347 MB でも通らない | **110 MB** (CI) / 100 MB (手元) — 上限 200 MB に対し 90 MB の余裕 |
| バイナリ | 857 KB | 1,316 KB (release) / **988 KB** (dist) |
| CPU/要求 (forward / HIT / CONNECT 確立) | — | **50.2 / 39.1 / 179.3 us** |
| テスト | 150 単体 + 21 結合 | **177 単体 + 42 結合** |

### 完了の定義 (§0 のゴールに対して)

- [x] Phase 0〜1 と T2.1・T2.2・T3.1・T3.2・T3.4 が完了、README の性能節に Rust ベンチの値がある
- [x] forward 8 並列 p50 が 1 ms 台 (loopback) — 実際は **0.20 ms**
- [x] トンネルが 1.5 GiB/s 以上 — **2.6 GiB/s**
- [x] 既定設定で `cargo test --workspace` 全通過 (177 単体 + 42 結合)
- [x] `rust-http-proxy --lite -p 8080` の 1 行で「認証なし・手軽・最速」
## 3. やったこと

### Phase 0 — 計測できるようにする

- [x] **T0.1 std だけで書いたベンチ**
  - 目的: Python のベンチは約 600 req/s が上限で、プロキシ自身の限界が見えなかった。
  - 変更箇所: `crates/bench/src/main.rs` (当時は `src/bin/bench.rs`)。
  - やること: direct / forward / tunnel / connect の 4 種。オリジンもベンチ内に持つ (固定応答を 1 回の `write_all` で返す
    keep-alive サーバー、`TCP_NODELAY`)。引数は `--proxy` `--conc` `--seconds` `--body-bytes` `--cacheable` `--only` `--no-keepalive`。
  - 受け入れ基準: オリジン直結で 50,000 req/s 以上 (ベンチが律速しない)。
  - 結果: direct 8 並列 **300,805 req/s、p50 0.013 ms**。以後の数字はすべてこのベンチ (`879cb75`)

- [x] **T0.2 プロファイル取得の手順を書く**
  - やること: `perf record -g` (無ければ `strace -c -f`) の手順を README の開発者向け節に。`[profile.release] debug = 1` は入れない (バイナリが太る)。
  - 結果: `CARGO_PROFILE_RELEASE_DEBUG=1` だけでは `strip = true` に消されるので `CARGO_PROFILE_RELEASE_STRIP=none` も要る (README に明記)。
    `perf` はこの環境ではユーザー空間だけ数える (`a5853c3`)

### Phase 1 — 少ない変更で確実に速くする

- [x] **T1.1 `TCP_NODELAY` を全ソケットに立てる**
  - 目的: 応答ヘッダーと本文を別々に `write` すると Nagle + delayed ACK で **1 要求あたり約 40 ms 止まる**。着手時の最大のボトルネック。
  - 変更箇所: クライアント側 (`src/lib.rs` `Conn::new`)、オリジン側と Happy Eyeballs で勝った接続 (`crates/net/src/net.rs`)。
  - やること: `set_nodelay(true)` (失敗は無視)。プールから出した接続は設定が残る。keep-alive で 5 要求を連続して送り
    100 ms 未満で終わる結合テスト (Nagle があれば 200 ms 以上かかる)。
  - 受け入れ基準: forward 8 並列 p50 44 ms → 10 ms 未満。
  - 結果: Rust ベンチで 188 → **29,107 req/s、p50 42.0 → 0.222 ms** (155 倍)。Python ベンチでは 176 → 674 req/s で直結と同じ値 (`4717a3d`)

- [x] **T1.2 応答ヘッダーと本文を 1 回の書き込みにまとめる**
  - 変更箇所: `crates/http/src/http/mod.rs` (配信部)、`serve.rs` (`write_cached_response`)、`crates/endpoints`。
  - やること: クライアントへの書き込みを `BufWriter::with_capacity(64 KiB)` 経由にし、ヘッダー + 最初の本文チャンクを 1 セグメントで出す。
    chunked の枠も同じ writer に書く。応答の最後で必ず `flush()`。CONNECT には影響させない。
  - 受け入れ基準: `strace` で 1 要求あたりのクライアント向け `sendto` が 2 → 1 回。
  - 結果: `sendto(7, "HTTP/1.1 200 OK...", 90)` + `sendto(7, body, 1000)` → **`sendto(7, ..., 1090)` の 1 回**。
    forward 8 並列 30,231 req/s、キャッシュ HIT 60,813 → **75,563 req/s** (`8177604`)

- [x] **T1.3 CONNECT トンネルを 1 スレッド + `poll(2)` に、Linux では `splice(2)` でゼロコピー**
  - 変更箇所: `crates/tunnel/src/tunnel.rs` (`relay::run`)、`crates/sys/src/sys.rs` (`poll` / `pipe2` / `splice` の束縛)。
  - やること: 両ソケットを non-blocking にし `poll` で待つ 1 ループ。片側 EOF は `shutdown(Write)` で相手に伝える。
    Linux では `pipe2` + `splice(SPLICE_F_MOVE | SPLICE_F_NONBLOCK)` (パイプ容量 1 MiB)、`EINVAL` などは read/write に自動フォールバック。
    アイドルタイムアウトは T2.2。
  - 受け入れ基準: トンネル 1 本 881 MiB/s → 1.5 GiB/s 以上、トンネルあたりのスレッド 3 → 1。
  - 結果: 1,114 → **2,800 MiB/s**、CONNECT 確立 3,704 → **7,375 tunnels/s** (p50 1.77 → 0.68 ms)、スレッド 3 → **1** (`dde5b46`)

- [x] **T1.4 キャッシュ無効時・統計無効時に一切の余計な仕事をしない**
  - 変更箇所: `crates/http/src/http/mod.rs` (鍵の生成・鮮度判定・合流の早期分岐)、`src/main.rs` (スレッドの起動条件)、`crates/metrics`。
  - やること: `cache.enabled()` が偽なら鍵・鮮度・`inflight` を通らない。キャッシュ無効なら probe スレッドを起こさない、
    `PROXY_STATS_PERSIST=off` なら persist / history を起こさない、ブロックリスト未設定なら fetch を起こさない。
    ホスト別統計はロック 1 回 + 文字列生成 1 回に抑える。
  - 受け入れ基準: `PROXY_CACHE_ENABLED=off` で `perf` / `strace -c` に cache / freshness 由来のものが出ない。
  - 結果: 起動直後のスレッド数 5 → **3** (既定は 6)。`perf report` に `cache::` / `freshness::` は出ず、
    ファイル系のシステムコールは起動時の 22 回だけ (要求あたり 0) (`c3f1d83`)

- [x] **T1.5 要求解析のアロケーションを減らす (計測してから)**
  - やること: まず 1 要求の内訳を測り、解析が 5% 未満なら飛ばす。
  - 結果: 要求解析は 1 要求 128 us のうち 4.9 us (**3.8%**) だったので**ヘッダーのスライス化は見送り**。代わりに内訳で見つかった
    「毎要求 64 KiB を確保・ゼロ埋め」する中継バッファをスレッドで使い回した (配信 24.2 → 17.9 us)。
    forward 1 並列 6,697 → **8,286 req/s**、64 並列 22,392 → **25,912 (+15.7%)** (`6f4679a`。内訳の表は付録 A)

- [x] **T1.6 オリジン接続プールのヒット率を上げる**
  - 変更箇所: `crates/origin/src/pool.rs`、`src/main.rs` (`ORIGIN_IDLE`)。
  - やること: `is_alive` の `set_nonblocking` → `peek` → `set_nonblocking` (3 syscall) を `recv(MSG_PEEK | MSG_DONTWAIT)` 1 回に。
    アイドル保持 30 → 60 s。`/status` にヒット / ミスを出す。
  - 受け入れ基準: forward ベンチで `pool_hits / (hits+misses)` が 95% 以上。
  - 結果: `pool_hit_ratio` **0.9712**。forward 8 並列 32,074 req/s、64 並列 26,142 req/s (`db8f17d`)

### Phase 2 — 認証なし (= 誰でも使える) でも落ちない

- [x] **T2.1 同時接続数の上限 (`PROXY_MAX_CONNS`、既定 4096)**
  - 変更箇所: `src/lib.rs` (`serve` / `Limiter`)、`crates/config`、`crates/reload` (即時反映)。
  - やること: 上限なら `503 Service Unavailable` + `Retry-After: 1` を書いて閉じる (スレッドは起こさない)。到達は 1 分に 1 回だけ `warn`。
    `/status` と `/metrics` に `active_connections` と `rejected_overload`。
  - 受け入れ基準: 上限 8 で 9 本目が 503 になる結合テスト。
  - 結果: `test_integration_connection_limit_returns_503`。ベンチの数値は不変 (`7d8644d`)

- [x] **T2.2 トンネルのアイドルタイムアウト (`PROXY_TUNNEL_IDLE_SECS`、既定 300、`0` で無期限)**
  - 変更箇所: `crates/tunnel/src/tunnel.rs` (`poll` のタイムアウト)、`crates/config`。
  - 受け入れ基準: 1 秒設定で無通信トンネルが約 1 秒で閉じる結合テスト。
  - 結果: `test_integration_idle_tunnel_is_closed_after_the_idle_timeout` (`9b9a780`)

- [x] **T2.3 1 スレッドで多数のトンネルをさばく `epoll` リアクター** ← 計測の結果、着手しない
  - 前提: スレッドあたり約 8 MiB のスタック予約が問題になる場合だけ着手する。同時 5,000 本のアイドルトンネルで
    RSS 200 MiB 未満・新規 CONNECT p99 10 ms 未満なら作らない。先に接続スレッドのスタックを 256 KiB にする (安価で必ずやる)。
  - 結果: スタック 256 KiB + パイプの遅延生成 + CONNECT 中の `BufReader` 解放で **RSS 198 MiB、fd 2.0/本、p99 9.13 ms**。
    基準を満たすので作らない。RSS の大半はスレッドスタックの実使用ぶん (約 36 KiB/本) (`82cc038`)。
    → スレッドそのものを手放す案は T8.1

- [x] **T2.4 開放プロキシとしての最小限の安全策**
  - やること: `PROXY_CONNECT_PORTS` (既定は制限なし、書式だけ用意)。ループバック・リンクローカル (`169.254.0.0/16`、`fe80::/10`) 宛ての
    オリジンは `PROXY_ALLOW_LOCAL=on` が無い限り 403 (クラウドのメタデータ経由の SSRF 防止。結合テストは `127.0.0.1` を使うので on)。
  - 受け入れ基準: 既定設定でメタデータアドレスへの GET が 403 になるテスト。
  - 結果: `test_integration_metadata_address_is_forbidden_by_default`、`test_integration_connect_port_restriction` (`9318792`)

### Phase 3 — 手軽にする

- [x] **T3.1 コマンドライン引数 (`--help` `--version` `-p/--port` `--bind` `--no-cache` `--quiet` `--lite`)**
  - 変更箇所: `crates/base/src/cli.rs`、`src/main.rs`。`std::env::args` を手で解析 (クレート禁止)。
  - やること: 引数は対応する環境変数の上書きとして扱う (優先順位: 引数 > `$HOME/.env` > 環境変数)。`--help` は環境変数の表を短く出す。
    不明な引数は使い方を出して終了コード 2。
  - 結果: `rust-http-proxy -p 3128 --lite` で起動 (`b0a0533`)

- [x] **T3.2 `--lite` / `PROXY_PROFILE=lite` = 最速の素通しプロファイル**
  - やること: キャッシュ off、統計永続化 off、ブロックリスト取得 off、ログ `warn`、プローブ無し、`/dashboard` は「lite mode」の 1 行。
    起動ログ 1 行目に `profile: lite`。
  - 受け入れ基準: 起動直後のスレッド数が待ち受け + 2 以下、ベンチが既定プロファイル以上。
  - 結果: 既定 19,475 req/s (p50 0.295 ms)・スレッド 6 に対し、lite **33,125 req/s (p50 0.199 ms)・スレッド 3** (`f35fdf3`)

- [x] **T3.3 配布: Dockerfile・GitHub Release**
  - やること: `Dockerfile` は 2 段 (rust:1.96 でビルド → 最小イメージ)、`.github/workflows/release.yml` はタグ `v*` で
    x86_64 / aarch64 をビルドして Release に添付。
  - 結果: musl 静的リンクも作ったが、`dlopen(libssl)` が使えず HTTPS オリジンのキャッシュが死ぬうえ、ビルドが 200 MB に収まらないので
    **既定は glibc に戻した** (`c254783` → `858eee1` → `f580716`)

- [x] **T3.4 README を「30 秒で使える」構成に直す**
  - やること: 先頭をクイックスタート (ビルド 1 行、起動 1 行、`curl -x` 1 行、`/proxy.pac` の URL) にし、機能一覧と環境変数の表はその下へ。
    性能節に Rust ベンチの値。`cargo install --git`。
  - 結果: `2c7d89f`

- [x] **T3.5 CI (`.github/workflows/ci.yml`)**
  - やること: push / PR で `cargo fmt --check`、`clippy -D warnings`、`cargo test --workspace`、`cargo build --release`、ベンチ 2 秒 (数字を残すだけ)。
  - 結果: `7b48ed1`。ビルドメモリの見張りは T7.5

### Phase 4 — 品質の下支えと、計測して分かったこと

- [x] **T4.1 要求解析の堅牢性テスト**
  - やること: 不正な要求行、巨大ヘッダー、`\r` 無しの行、CONNECT の宛先にパス、IPv6 リテラルなどを流し、必ず 400 / 414 / 431 で閉じる (パニックしない)。
  - 結果: `test_integration_malformed_requests_do_not_panic` (`b3bde1d`)

- [x] **T4.2 `opt-level = "s"` と `3` を比較**
  - 結果: forward 32,419 → 33,121 req/s (**+2.2%**)、バイナリ +131 KB。5% に届かないので当時は `"s"` のまま (`8b207ef`)。
    T7.1 で LTO を切ったときに `3` を採用 (41.4 → 39.7 us)

- [x] **T4.3 `SO_REUSEPORT` で待ち受けスレッドをコア数ぶん** ← 計測の結果、不要
  - 結果: `connect` ベンチ 8 並列 8,283 tunnels/s のとき accept スレッドの CPU は 2,250 / 5,000 ms (1 コアの 45%)。
    32・64 並列ではむしろ下がる (プロセス全体で 3.5 コアぶん使っていて機械側が飽和)。accept は律速していない (`9be6d7c`)。
    当時 1 接続あたり 54 us の大半だったスレッド生成は T4.5 で消した

- [x] **T4.4 1 要求あたりのシステムコールを 10.06 → 5.06 回にする**
  - 目的: `perf` で `malloc` 6.6% + `cfree` 5.1% が見えて確保を減らした (90.2 → 76.3 回/要求) が、速度は誤差の中だった。
    プロキシの CPU は **63% がカーネル側** (要求あたり user 33 / kernel 56 us) で、ユーザー空間の確保・解放 12% は全体の約 4 us でしかない。
    そこで `strace -f -c` で数えたら 10.06 回/要求のうち `setsockopt` 4.00、`getsockname` 1.00 と半分が間接費だった。
  - 変更箇所: `src/lib.rs` (`serve_one` の `read_timeout`)、`crates/origin/src/pool.rs`、`crates/http/src/http/mod.rs`。
  - やること: `local_addr()` は接続ごとに 1 回だけ、読み取りタイムアウトは値が変わるときだけ設定、プールから出した接続は
    タイムアウトが同じなら `setsockopt` を呼ばない。本文転送を 64 KiB 読みにし、接続ごとの `getpeername` / `getsockname` / `try_clone` を消す。
  - 結果: **10.06 → 5.06 回/要求** (`recvfrom` 3 + `sendto` 2 だけ)、CPU/要求 89.9 → **85.0 us** (-5.4%、3 回とも改善)、
    forward 25,321 → 26,685 req/s (`d853973`、`739aad1`、`4a1c48f`)

- [x] **T4.5 接続スレッドを使い回す (`crates/workers`)**
  - 目的: 1 接続あたり約 26 システムコールのうち約 16 がスレッドの生成・破棄 (`clone3` / `mmap` / `mprotect` / `sigaltstack` ×3 …)。
    keep-alive が効かないクライアントでは全額かかる。
  - やること: 空いたスレッドを後入れ先出しで積み、30 秒使われなければ自分で終わる (上限 64)。スタック 256 KiB。
    仕事がパニックしてもプールは使えるまま。
  - 結果: 短命接続で **CPU/要求 -33%、システムコール -52%** (`a011c24`、テスト `fd2652a`)

### Phase 5 — 「前提を疑う」ラウンド

Phase 0〜4 の後、設計上の前提を 8 つの観点から疑い直した。実測で裏付けの取れたものだけ入れた。

- [x] **T5.1 素通しできる本文を `splice(2)` で運ぶ**
  - 目的: 中継バッファを経由せず、カーネル内でコピーする。
  - 変更箇所: `crates/http/src/http/mod.rs` の `passthrough` (`sys::splice_block`)。
  - 受け入れ基準: 大きい本文の CPU/要求が下がり、小さい本文が退行しないこと。
  - 結果: 本文 1 MiB の CPU/要求 771.6 → **302.9 us** (-60%)。しきい値 128 KiB 未満は従来経路 (`5eac139`)

- [x] **T5.2 オリジンプールの既定を 8 → 64 本、全体上限 256**
  - 目的: 高並列で張り直しが増えていた。`Pool::sweep()` に呼び出し元が無く、負荷が止まってもアイドル接続を解放していなかったのも直した
    (100 秒放置後も fd 54 のまま → 7)。
  - 結果: conc=64 の p99 37.3 → **12.1 ms** (`0980886`、テスト `af9e0f1`、README `d7662dc`)

- [x] **T5.3 中継バッファを 64 → 32 KiB**
  - 結果: 本文 64 KiB で -8%、1 MiB で -11%、1 KiB は退行なし (`093c68e`)

- [x] **T5.4 ヘッダー組み立ての中間 `Vec<String>` を外す**
  - 前段として URL・プールキー・接続先を 1 本の `String` から借りるようにした (76.3 → 64.1 回/要求、`c8ff795`)。
  - 結果: 確保 64.1 → **41.1 回/要求** (T4.4 前の 90.2 から -54%)、CPU -8.9% (`9f552fc`)

- [x] **T5.5 キャッシュ HIT の本文直接書き・ヘッダー再解析の省略**
  - 結果: 256 KiB の HIT で user CPU 50.3 → **24.3 us** (`cbb2c00`)

- [x] **T5.6 malloc のアリーナ数に上限を掛ける (`PROXY_MALLOC_ARENAS`、既定 8)**
  - 目的: glibc の既定は「コア数 × 8」で、接続ごとにスレッドが増えると使われないアリーナが RSS に居座る。
  - 結果: アイドル接続あたり 80.5 → **26.8 kB** (-67%)。代償は conc=64 の CPU +4% (conc=8 は差なし) (`05e6c43`)

- [x] **T5.7 見つかった不具合を直す**
  - **要求スマグリング** (`6edc748`): `Connection: Content-Length` と指名されると、転送するヘッダーからは落ちるのに本文は送られ、
    オリジンがそれを次の要求の先頭として読む。認証なしの開放プロキシなので誰でも踏めた。枠組みのヘッダーは指名されても落とさない。
  - **要求ヘッダー全体に上限が無い** (`e768ece`): 1 行 64 KiB × 256 行 = 16 MiB を正常な形で送れ、行バッファの使い回しで
    それが接続の間ずっと居座る (同時 8 接続で RSS 15 → 272 MiB)。合計 128 KiB の上限で 272 → **17 MiB**。
  - **URL 正規化が 2 実装に分かれていた** (`5ffaaf5`): 乖離を捕まえるテストつきで 1 実装に。
  - `cargo run --release` が bin を決められない (`9e205ec`、`default-run`)。CI だけで落ちるテスト 2 本 (`0bb4138`、`ec38a6d`)。

### Phase 6 — アイドル接続をスレッドから外す

「1 接続 = 1 スレッドが専任」という前提を外す。要求を処理していない keep-alive 接続はスレッドを手放し、
1 本の監視スレッド (epoll) に預ける。挙動が変わらないことを確かめながら 5 段階に割って進めた。

- [x] **T6.1 クライアントの読み取りバッファとストリームを分ける** (`crates/msg/src/clientio.rs`: `ClientBuf` / `ClientReader`)
  - 目的: 預けるには、バッファを接続から切り離せる必要がある。
  - 結果: 挙動不変。CPU/要求に差なし (`c688372`)

- [x] **T6.2 接続の状態を `struct Conn` にまとめる** (`src/lib.rs`)
  - 目的: 270 行 1 枚の `handle_client` が状態を全部ローカル変数で抱えていて、持ち運べなかった。
  - やること: `struct Conn` / `enum Step` / `serve_one` / `pump` に割る。行の置き場は接続あたり 1 回だけ借りる。
  - 結果: 38.0 → 38.3 us/要求 (ぶれの中)。要求ごとに借りて返す形は +1.5% だったのでやめた (`931a276`)

- [x] **T6.3 `sys` に epoll の束縛を足す** (`crates/sys/src/sys.rs`)
  - 注意: `struct epoll_event` は x86_64 (と x32) だけ packed で 12 バイト、それ以外は 16 バイト。食い違うと `epoll_wait` が
    書き戻す配列の刻みがずれて token が化ける。arch ごとに `repr` を変えて `size_of` を const で突き合わせる。
  - 結果: テスト 4 本。動作環境は aarch64 (16 バイト側) (`19b4c3d`)

- [x] **T6.4 `IdleWatch` を足して配線する (既定 off)** (`src/idle.rs`)
  - 結果: 暇な接続 1,000 本で 1,003 スレッド 44.4 MB → **12 スレッド 25.6 MB** (`8a4fbc4`。設計レビューの指摘の修正 `db634b9`)

- [x] **T6.5 猶予を読み取りタイムアウトに畳んで既定 on にする**
  - 目的: 猶予を `poll(2)` で待つと、忙しい接続に要求ごとの `ppoll` が 1 回増えて CPU +5.5% だった。
  - やること: 2 回目以降の読み取りタイムアウトを猶予の長さにして、空振りを「暇だ」と解釈する。
    要求の途中で猶予を過ぎたら本来のアイドル時間まで待ち直す。
  - 結果: システムコール 6.809 → **5.780 回/要求** (park=off と同じ)、CPU 40.30 → **38.69 us** (`ba8a240`、`bca3faf`、README `02d8bc9`)

- [x] **T6.6 途中で見つけた不具合を直す**
  - **要求を処理したスレッドが終わるとプロセスごと abort** (`f683da4`): T6.2 で入れた不具合。スレッドローカルに `Drop` を持つ型を
    置いてしまい、スレッド終了時の破棄でその置き場自身を触って `thread local panicked on drop` になる。ワーカースレッドは 30 秒で
    自分から終わるので **1 要求通してから 30 秒後に必ず落ちていた**。回帰テストつき。
  - **期限切れの一斉 close で監視スレッドが止まる** (`196eb06`): 1 周 16 本で切る。実測では差が出なかったが、
    越えたときの被害 (数百 ms 全接続が止まる) と釣り合わないので入れた。
  - **再検証後の HIT 判定が負荷次第で落ちる** (`69b384f`): テスト側の競合。

### Phase 7 — ビルドと構造

- [x] **T7.1 1 クレートを層ごとのクレートに割る**
  - 目的: `cargo run --release` が動作環境で SIGKILL (OOM) される。ビルドの最大 RSS が 347 MB あった
    (**この超過は前からで、`05e6c43` の時点で既に 339.7 MB**)。
  - やること: `rustc` はクレート単位で全部を一度に抱えるので、行数がそのまま最大 RSS になる
    (実測: 空クレート 37.8 MB、1,046 行 148.5 MB、18,276 行 330 MB)。層ごとに別クレートへ割り、
    下の層は同じ名前で再エクスポートして `crate::sync` のような書き方を移設前のまま通す。
  - 結果: 7 クレートで 347 → **194 MB**。分割そのものの代償は CPU +1%。LTO を切ったぶんが +3.9% で、
    配布バイナリは LTO ありの `dist` プロファイルで作ることにした (`1a9e11a`)

- [x] **T7.2 2,214 行の結合テストを責務ごとに 4 本へ分ける**
  - 結果: `common` 586 / `proxy_test` 728 / `cache_test` 604 / `keepalive_test` 263 / `tunnel_test` 92 行。中身は動かしていない (`4ca9465`)

- [x] **T7.3 ビルドを直列にする (`.cargo/config.toml` の `jobs = 1`)**
  - 目的: クレートを割っても、並列に走る `rustc` の合計が上限を超えれば同じこと。
  - 結果: 200 MB の cgroup で、並列だと `proxy-net` が SIGKILL、`-j 1` なら通る。代償はビルド時間で 8 コアの機械で 14.9 → 32.3 秒 (`0d3cb8b`)

- [x] **T7.4 責務ごとの層まで割り直す (7 → 26 + 本体)**
  - 目的: T7.1 は「200 MB に収まる」ことだけを見て切ったので、1 クレートに複数の責務が同居していた。
  - やること: 依存の向きを実際に測ってから、循環しない範囲で意味のある単位に割る。
  - 結果: CPU/要求 41.42 → 41.46 us (ぶれの中)、ビルド時間 34.5 → 42.0 秒、テスト 218 本すべて通過。
    ビルドが通る最小の上限は 23 クレートで 130 MB、26 クレートで **110 MB**。`bench` は既定のビルド対象から外した。
    **割らなかったところ**: `proxy-metrics` の 3 モジュール (metrics / history / persist) は互いを参照している
    (履歴は指標の一部で、状態ファイルはその両方を載せる)。`proxy-msg` の 4 つも「HTTP メッセージの表現」で 1 つの責務
    (`8c61181`、`5d4eab4`、`5021087`、`cff3fc8`)

- [x] **T7.5 ビルドメモリの見張りを RSS ではなく cgroup での実測にする** (`scripts/build-memory.sh`)
  - 目的: RSS を測るだけでは機械をまたいだ判定にならない。メモリ圧がかかっていないとアロケータが解放済みのページを持ち続けるので、
    同じビルドでも余裕のある機械ほど大きく出る (手元の aarch64 で 194 MB、GitHub の runner では 334 MB)。
  - やること: cgroup を作れる環境では実際にその上限の中でビルドして通るかを見る (`systemd-run --scope -p MemoryMax=`)。
    `--find` で通る最小の上限を探す。
  - 結果: CI は毎回 200 MB の cgroup でビルドする (`21f4c48`、`cb44d95`、`b0b9f74`、`3e5a8e2`)。手元でも判定できるようにしたのは T9.0

## 4. 測って採らなかったもの

同じ道を二度調べないための記録。**すべて実装して測ったうえで戻した / 入れなかった**もの。

| 案 | 測った結果 | 判断 |
|---|---|---|
| `epoll` リアクターでトンネルを多重化 (T2.3) | 5,000 本のアイドルトンネルで RSS 198 MiB、新規 CONNECT p99 9.1 ms | 受け入れ基準 (200 MiB / 10 ms) を満たすので作らない |
| `SO_REUSEPORT` で待ち受けを複数に (T4.3) | accept スレッドの CPU は 1 コアの 45%、並列を上げるとむしろ下がる | accept は律速していない |
| `opt-level = 3` (T4.2、当時) | +2.2%、バイナリ +131 KB | 5% に届かないので見送り (T7.1 で LTO を切ったときに採用) |
| musl 静的リンク (T3.3) | ビルドがメモリ 200 MB に収まらない。`dlopen(libssl)` も使えず HTTPS オリジンのキャッシュが死ぬ | 採らない |
| 統計 `Mutex` の分割 | 統計を丸ごと消したビルドでも `futex` 0.010 vs 0.011 回/要求、p99・CPU とも差なし | 期待効果 0〜0.25% |
| `recvfrom` を 3 → 2 回に (プールの生存確認の省略) | — | プールはクライアント間で共有なので、desync すると**他人の応答**が渡り、誤った URL のキャッシュとして固定化する。期待値 1% のために踏む橋ではない |
| クライアント書き込みバッファのプール化 | アイドル接続あたり 30.3 → 30.9 kB、CPU 43.3 → 43.9 us | 改善なし。アイドル接続の資源はスレッドスタックが主因だった |
| 猶予を `poll(2)` で待つ (T6.4) | `ppoll` が 1 回/要求 増えて CPU +5.5% | 読み取りタイムアウトに畳めば 0 回 (T6.5) |
| `PROXY_PARK_MAX_GRACE` (猶予待ちスレッドの上限) | — | T6.5 で猶予が「普通に次の要求を読んでいる状態」になり、数えて止める意味が消えた |
| 期限切れの close をワーカーに投げる | — | 受け渡し (Mutex + channel + スレッド起床) が隠したい `close` と同じ桁。空きスレッド上限 64 を越えると新規スレッドを起こす |
| 要求ごとに行の置き場を借りて返す (T6.2) | +1.5% | 接続あたり 1 回だけ借りる |
| クレート分割後に `#[inline]` を当てる (T7.1) | 41.7 us (当てる前 41.4 us) | 効かない。LTO を切った損は個々の小さい関数ではなかった。`opt-level = 3` の方が効いた (41.4 → 39.7 us) |
| クレート分割で `lto = "thin"` | ビルド最大 334 MB、CPU 39.4 us | 200 MB に届かない。`lto = false` (194 MB / 39.7 us) を採る (7 クレート・並列ビルドのときの値。26 クレート・`jobs = 1` で測り直したのが下の 2 行) |
| `lto = "thin"` (26 クレート、`jobs = 1`) | 通る最小 **170 MB** (`false` は 100 MB)、ビルド 38.7 → 55.4 秒、バイナリ 1,316,336 → 1,316,328 B (**8 バイト**)、CPU/要求 forward -0.2% / connect +0.7% / HIT -2.5% | **払うものだけあって得るものが無い**。速さはすべてぶれ (±8%) の中で、バイナリも実質同じ。CI では +10 MB (≈180 MB) になり上限 200 MB の余裕が 20 MB しか残らない (T9.1) |
| `lto = "fat"` (26 クレート、`jobs = 1`) | 通る最小 **350 MB** (300 MB では通らない)、ビルド 48.8 秒、バイナリ -128 KB、CPU/要求 forward -4.5% / connect -2.6% / HIT -9.3% | 効くが上限 200 MB の **1.75 倍**。LTO の重さは最終リンク 1 回にかかるので、クレートを 26 に割っても下がらなかった。配布用の `dist` プロファイルだけで使う (T9.1) |

## 5. これから (未着手)

上と同じ形式。**着手する前に必ず「今どうなっているか」を測る**こと。

### Phase 8 — 前のラウンドからの持ち越し

- [ ] **T8.1 CONNECT トンネルも暇なときは監視スレッドに預ける**
  - 目的: トンネルは今も 1 本 1 スレッド。実測で同時 5,000 本のとき RSS 140 MiB。
    HTTP 側と同じ `IdleWatch` がもうあるので、両方向が暇なトンネルは預けられるはず。
  - 変更箇所: `crates/tunnel/src/tunnel.rs`、`src/idle.rs`、`crates/bench/src/main.rs` (計測モード)。
  - やること: `splice` のループが両方向とも `WouldBlock` になったら、2 つの記述子を epoll に預けて
    スレッドを解放する。どちらかが読めるようになったらワーカーへ戻す。
    - **今の `IdleWatch` は記述子 1 本と `Box<Conn>` の対応しか持てない。トンネルは 2 本要る。**
      預かるものを enum (`Http(Box<Conn>)` / `Tunnel(Box<…>)`) にし、2 つの fd から同じトンネルを引けるようにする
      (両方を epoll に入れ、どちらかが起きたら両方外す。期限切れも同じ)。
    - `relay::run` を「暇になるまで回して戻る」形 (状態を struct に持つ) に割り、預けている間に要る
      情報 (conn_id、宛先、開始時刻、`metrics`、接続元 IP) も一緒に運ぶ。終わったときのアクセスログと
      統計は、どのワーカーで終わっても 1 回だけ出す。
    - 預けられる条件は「両方向とも未送信 0・EOF でない・直近の poll が両方空振り」。預ける前にパイプ
      (`Relay`) を手放す (方向あたり fd 2 本。戻ったら遅延生成のまま作り直す)。
    - 期限は `tunnel_idle` (既定 300 s)。`0` (無期限) のときの扱いは決めて記録する
      (`Instant` の足し算が溢れない範囲で遠い期限を使うか、預けないか)。
    - Linux 以外は従来どおり (2 スレッドの `io::copy`)。
    - **計測のために** `proxy-bench` に `--only idle-tunnels` を足す (`--conc` 本の CONNECT を張って `--seconds` 秒握る。
      プロキシ側のスレッド数と RSS は `scripts/cpu-per-request.sh` が出す)。
  - 受け入れ基準: 同時 5,000 本のアイドルトンネルでスレッド数が 5,000 → 数十、RSS が下がること。
    スループット (`--only tunnel`) と新規 CONNECT の CPU/接続・p99 が悪化しないこと。
    結合テストを足す: 預けられたトンネルがその後もデータを通すこと、`tunnel_idle` で閉じること。

- [x] **T8.2 1 接続 1 要求のときの内訳を取る** → 内訳は取った (2026-09-07)。**対策は T9.3 / T9.4 に分けた。**
  - 結果: 1 接続 1 要求 (`--no-keepalive`、8 並列) は **123.3 us/接続** (user 29.1 / kernel 94.2)。keep-alive の
    50.9 us (user 16.2 / kernel 34.7) との差 72 us が接続 1 本の固定費。`strace -f -c` の内訳は **14 システムコール/接続**
    (keep-alive は 5.03/要求):

    | システムコール | 回/接続 | 何か |
    |---|---|---|
    | `setsockopt` | **4.00** | `SO_SNDTIMEO`、`TCP_NODELAY`、`SO_RCVTIMEO` (1 要求目の timeout)、`SO_RCVTIMEO` (2 要求目の猶予) |
    | `recvfrom` | 4.00 | 要求 1、オリジン応答 1、プールの生存確認 (`MSG_PEEK`) 1、**次の要求待ちで EOF** 1 |
    | `futex` | **2.00** | accept スレッド → ワーカーの受け渡し (起こす 1 + 待つ 1) |
    | `sendto` | 2.00 | オリジンへ 1、クライアントへ 1 |
    | `accept4` / `close` | 1.00 / 1.00 | 本質 |

    `recvfrom` の 4 本目は、ベンチ (と curl のような既定のクライアント) が `Connection: close` を付けずに
    自分から閉じるので、次の要求を待って EOF を読むぶん。クライアント側の都合なので手を付けない。
    残りで削れるのは **`setsockopt` 4 回** (T9.3) と **受け渡しの `futex` 2 回 + コンテキストスイッチ** (T9.4)。

- [x] **T8.3 LTO を切って失った 4.9% を埋める** → **T9.1 で答えが出た**: 埋まらない。LTO は 26 クレートでも最終リンクに 350 MB (fat) 要るので入らず、`thin` は効果が無い。差はユーザー空間 1.31 us/要求 を層じゅうに薄く散らしたもので、名指しできる関数が無い (§5 T9.1 の内訳)。

- [ ] **T8.4 1 要求あたりの確保 41.1 回の内訳を出す** → **T9.5 に統合** (perf の内訳と一緒に取る)。

- [ ] **T8.5 `PROXY_MAX_CONNS` の既定 4096 を見直す**
  - 目的: 上限の意味が変わった。以前は「同時に立つスレッド数」の歯止めだったが、
    アイドル接続を預けるようになったので、いまは実質「記述子の数」の歯止め。T8.1 が入るとトンネルも同じになる。
  - やること: `ulimit -n` との関係を確かめ、既定値の根拠を決め直して README に書く。
  - 受け入れ基準: 既定値の根拠が 1 行で説明できること。

- [ ] **T8.6 `proxy-cache` と `proxy-http` をさらに割れるか調べる**
  - 目的: いちばん大きいのが `proxy-cache` (2,257 行、うち 936 行はテスト) と
    `proxy-http` (1,728 行)。どちらも「1 つの責務」に見えるが、内訳は見ていない。
  - やること: モジュール間の依存を実際に測ってから決める (`crate::` の参照を数える)。
    切れ目が無ければ「無い」と記録する。
  - 受け入れ基準: 割るか割らないかの判断が、依存の実測にもとづいていること。

- [ ] **T8.7 結合テストのオリジンが要求本文を読まず、機械が混むと落ちる**
  - 目的: `test_integration_request_body_on_a_reused_connection` が `taskset -c 0` で 4 回に 1 回落ちる (T9.1 の作業中に判明)。
    テスト側のオリジン (`tests/common/mod.rs` の `start_origin`) が `\r\n\r\n` までしか読まず、ヘッダーと本文が別セグメントで
    届くとエコーが空になる。プロキシの不具合ではない。
  - やること: ヘルパーが `Content-Length` ぶん本文を読み切ってから応答するように直す。
  - 受け入れ基準: `taskset -c 0 cargo test --test proxy_test request_body` を 10 回回して落ちないこと。

### Phase 9 — クレートを割ったことで手が届くようになった最適化

T7.1 / T7.4 で 1 クレートを 26 に割った結果、**手が届くようになったこと**が 3 つある。

1. **ビルドのメモリに 90 MB の余裕ができた** (347 MB → 110 MB、上限 200 MB)。ビルドを重くする最適化 (LTO、
   最適化レベル) は「200 MB に入らない」の一言で切っていたが、その数字は **7 クレート・並列ビルドのとき** のもの
   (`lto = "thin"` 334 MB は `jobs = 1` より前の計測)。前提が変わったので測り直せる。
2. **層ごとに別クレートになった**ので、クレート単位のプロファイル (`[profile.release.package.*]`) で
   ホットパスと関係ない層だけ最適化を軽くできる。ホットパスにコードを足しても (T8.1、T9.4)、
   そのクレートが小さいので上限を脅かさない。
3. **システムコールの層 (`proxy-sys`) と計測の道具 (`proxy-bench`) が独立した**ので、束縛や計測モードを足しても
   プロキシ本体のビルドには効かない。

- [x] **T9.0 手元でも cgroup の中でビルドを試せるようにする**
  - 目的: 「200 MB で通るか」は CI に投げないと分からなかった (手元は `sudo systemd-run` が使えない)。
    LTO の判断 (T9.1) を手元で回すには、手元で cgroup を作れる必要がある。
  - 発見: この機械では **`systemd-run --user --scope -p MemoryMax=64M`** が通り、上限も効く
    (64M の scope で 40 MB を確保した python が SIGKILL された。cgroup の名前空間の都合で
    `/sys/fs/cgroup` からは見えないが、殺されることは確認済み)。
  - 変更箇所: `scripts/build-memory.sh`、`TASKS.md` §1。
  - やること: `run_in_cgroup` で、システムの systemd (`sudo systemd-run --scope`) が使えなければ
    **ユーザーの systemd (`systemd-run --user --scope`)** を試す。どちらも使えないときだけ RSS の参考値に落ちる。
    `--find` を手元で回し、CI の「110 MB」と比べて差を記録する (機械が違うので同じにはならない。
    差が分かっていれば手元の数字で判断できる)。
  - 受け入れ基準: `scripts/build-memory.sh --find 100 110 120 130 140 150` が手元で数字を返すこと。
    CI 側の動きは変えない。
  - 結果: `run_in_cgroup` を「システムの systemd (`sudo systemd-run --scope`) → ユーザーの systemd
    (`systemd-run --user --scope`) → 参考の RSS だけ」の 3 段に落とした (`detect_scope` が小さい scope を
    実際に 1 つ作って判定する)。ユーザーの scope は自分自身として走るので `--uid/--gid` は付けない。
    **手元で判定できるようになった**: `--find 90 100 110 …` は 90 MB で落ち (exit 143)、**100 MB で通る**。
    **CI は 110 MB** なので手元の方が 10 MB 低く出る (機械もアロケータの挙動も違う)。
    手元の数字で判断するときは +10 MB 見ておけばよい。

- [x] **T9.1 LTO を `jobs = 1`・26 クレートの条件で測り直す** (T8.3 の答え)
  - 目的: `release` (LTO なし) と `dist` (LTO あり) の差 4.9% は、LTO が 200 MB に入らないから諦めていた。
    その根拠の数字 (thin 334 MB / fat 347 MB) は 7 クレート・並列ビルドの値で、いまの条件では測っていない。
    LTO の重さは **最終リンク 1 回だけ**にかかるので、クレートを小さく割っても最終リンクは同じ大きさ、
    という可能性もある。どちらかは測れば分かる。
  - 変更箇所: `Cargo.toml` (`[profile.release]`)、README の「ビルド・テスト」「配布」節、`TASKS.md` §2・§4。
  - やること: `lto = false` (現状) / `"thin"` / `"fat"` の 3 つで、(a) T9.0 の cgroup で **通る最小の上限**、
    (b) CPU/要求 (forward・HIT・connect を交互に 3 回)、(c) バイナリサイズ、(d) クリーンビルドの時間 を取る。
    **採用の条件**: cgroup で 170 MB 以下 (上限に 30 MB の余裕) で通り、CPU/要求 が下がるかバイナリが小さくなること。
    thin と fat の両方が通るなら速い方。採用したら `dist` プロファイルとの差を取り直し、
    README の古い記述 (**「ビルドにピーク約 450 MiB」「`lto = true`」「`opt-level = "s"` のまま」は今の設定と合っていない**) を直す。
    入らなければ、`release` と `dist` の `perf report` を突き合わせて差の出ている関数を表にし、
    「LTO でしか埋まらない」か「`#[inline]` を当てるべき関数がある」かを記録する (総当たりの `#[inline]` は効かないことが §4 で分かっている)。
    `panic = "abort"` は測らない: ワーカーの仕事がパニックしてもプロセスが生き残る (`survives_a_panicking_job`) のは設計上の要件。
  - 受け入れ基準: 3 通りの表が残り、採用 / 不採用が根拠つきで書かれていること。採用したら CI の `Build memory` (200 MB) が通ること。
  - 結果: **不採用。`lto = false` のまま。** LTO の重さは最終リンク 1 回にかかるので、
    **クレートを 26 に割っても最終リンクは小さくならなかった** (これが「測れば分かる」の答え)。
    3 通りとも `cargo clean` から、手元の cgroup (T9.0) と交互 3 回の中央値:

    | `[profile.release]` | 通る最小のメモリ | クリーンビルド (`jobs = 1`) | バイナリ (strip 済) | CPU/要求 forward | CONNECT 確立 | キャッシュ HIT |
    |---|---|---|---|---|---|---|
    | `lto = false` (現状) | **100 MB** | 38.7 秒 | 1,316,336 B | 50.19 us | 179.25 us | 39.09 us |
    | `lto = "thin"` | 170 MB | 55.4 秒 | 1,316,328 B (**-8 B**) | 50.09 us (-0.2%) | 180.59 us (+0.7%) | 38.11 us (-2.5%) |
    | `lto = "fat"` | **350 MB** | 48.8 秒 | 1,185,248 B (-128 KB) | 47.95 us (**-4.5%**) | 174.67 us (-2.6%) | 35.44 us (**-9.3%**) |

    - `fat` は効くが **350 MB** (300 MB では通らない)。上限 200 MB の 1.75 倍で論外。
    - `thin` は 170 MB。CI は手元 +10 MB なので実質 180 MB、余裕が 20 MB しか残らない。
      そのうえ**効果が無い**: 速さは 3 つともぶれ (±8%) の中、ユーザー空間の CPU はむしろ +1.6%
      (15.84 → 16.10 us)、バイナリは 8 バイトしか変わらず、ビルドは +43% (38.7 → 55.4 秒)。
      「CPU/要求 が下がるかバイナリが小さくなること」を満たさない。
    - `panic = "abort"` は測っていない (`survives_a_panicking_job` が設計上の要件)。

    **LTO が何をしているか。** 3 つのビルド — `release` (現状)、`release` + fat LTO (LTO だけを変えたもの)、
    `dist` (`opt-level = "s"` + fat LTO) — をシンボル付き (`CARGO_PROFILE_*_STRIP=none` / `_DEBUG=1`) で作り、
    forward を回しながら `perf record -e cpu-clock -g` → `perf report --no-children --sort symbol`。
    この環境は `perf_event_paranoid = 2` なので**ユーザー空間だけ**が数えられる
    (既定の `cycles` イベントは 3 サンプルしか取れなかったので `cpu-clock` を使う)。
    % を user CPU/要求 の中央値に掛けて us/要求 に直したもの:

    | 分類 | `release` | +fat LTO | `dist` | 差 (fat - release) |
    |---|---|---|---|---|
    | 自分のコード (`proxy_*` + 本体) | 4.72 us | 3.96 us | 4.22 us | **-0.76** |
    | `core::` / `std::` の中 | 4.13 us | 3.90 us | 3.81 us | -0.23 |
    | `memcpy` / `memchr` / `memcmp` | 1.02 us | 0.64 us | 1.04 us | -0.38 |
    | アロケータ (`malloc` + `cfree`) | 1.54 us | 1.57 us | 1.51 us | +0.04 (**変わらない**) |
    | 原子操作 (`__aarch64_*`) | 1.08 us | 1.00 us | 1.00 us | -0.08 |
    | **user CPU/要求 の合計** | **15.88 us** | **14.57 us** | **14.68 us** | **-1.31 (-8.2%)** |

    **結論: 「LTO でしか埋まらない」。`#[inline]` を当てるべき関数は見つからなかった。**
    個々の関数で大きく動いたのは、`write_request_headers` (0.27 us) と `Pool::get` (0.22 us) と
    `memchr_aligned` (0.45 us) が**上位から消える**一方で `handle_http_with_headers` が +0.31 us 太る、という
    **帰属の移動** (呼び先がクレートをまたいで呼び元に取り込まれただけ)。差し引きで実際に浮くのは
    **1.31 us を `proxy_*` の層じゅうに薄く散らした**もので、名指しできる関数が無い
    (§4 の「総当たりの `#[inline]` は効かない」と同じ結論を、内訳から裏づけた形)。
    そもそも CPU/要求 50.2 us の **68% はカーネル側** (34.4 us) なので、ユーザー空間を 8.2% 削っても
    全体では 2〜3% にしかならない。**次に効くのはシステムコールを減らすこと** (T9.3 / T9.4) で、
    LTO ではない。

- [ ] **T9.2 ホットパスと関係ないクレートは `opt-level = "s"` に落とす**
  - 目的: クレートを割ったので、層ごとに最適化レベルを選べる。`/dashboard` の HTML、Prometheus 出力、設定の読み取り、
    ディスクの実測、`.env` の監視などは 1 要求ごとには走らない。そこを `"s"` にすればバイナリとビルドの時間・メモリが減り、
    速さは変わらないはず。
  - 変更箇所: `Cargo.toml` (`[profile.release.package."proxy-…"]`)。
  - やること: 候補は `proxy-rrd` `proxy-sysinfo` `proxy-capacity` `proxy-diskprobe` `proxy-cachecfg` `proxy-config`
    `proxy-reload` `proxy-prom` `proxy-endpoints` (`endpoints::handle` は毎要求呼ばれるが、先頭の判定だけで抜ける)。
    `proxy-cachedisk` は書き出し経路がそこそこ熱いので、入れる場合は別に測る。
    **ホットパス (`base` `sys` `msg` `net` `origin` `http` `freshness` `cache` `cachemem` `cachekey` `tunnel` `workers`
    `metrics` `blocklist` `tls` と本体) は触らない** (`metrics.record_host` と `blocklist::is_blocked` は毎要求走る)。
  - 受け入れ基準: CPU/要求 (forward・HIT・connect) がぶれの中、バイナリが小さくなり、ビルドのメモリが増えないこと。
    差が出なければ「効かない」と §4 に書いて戻す。

- [ ] **T9.3 accept した接続への `setsockopt` 4 回を、待ち受けソケットからの継承に置き換える**
  - 目的: 1 接続 1 要求では 14 システムコール/接続のうち 4 が `setsockopt` (T8.2 の内訳)。Linux では accept した
    ソケットが待ち受けソケットの `TCP_NODELAY` / `SO_RCVTIMEO` / `SO_SNDTIMEO` を引き継ぐ (`sk_clone_lock` が
    `struct sock` ごと複製する) ので、待ち受けに 1 回設定すれば接続ごとには要らない。
  - 変更箇所: `crates/sys/src/sys.rs` (`setsockopt` の束縛。`TcpListener` には `set_nodelay` が無い)、
    `src/lib.rs` (`serve` / `Conn::new` / `serve_one` の `read_timeout` の初期値)、`crates/net/src/net.rs` (`bind_all`)。
  - やること:
    1. Linux では bind 直後に待ち受けへ `TCP_NODELAY`、`SO_SNDTIMEO = timeout`、`SO_RCVTIMEO = timeout` を設定し、
       `Conn::new` の `set_write_timeout` / `set_nodelay` と 1 要求目の `set_read_timeout` を省く
       (`Conn.read_timeout` の初期値を継承した `Some(timeout)` にする)。Linux 以外は従来どおり接続ごとに設定。
    2. `SO_RCVTIMEO` は `accept()` にも効く (timeout 秒ごとに `EAGAIN` で戻る)。`serve` の accept ループで
       `WouldBlock` / `TimedOut` はログも待ちもせず `continue` する。
    3. `.env` の再読込で `timeout` が変わったら待ち受けの値も更新する (`serve` は接続ごとに `config_of()` を
       引いているので、前回当てた値と違うときだけ `setsockopt` し直す)。
    4. 継承を確かめる単体テスト: 待ち受けに 3 つを設定 → connect → accept したソケットの `nodelay()` /
       `read_timeout()` / `write_timeout()` が待ち受けの値になっていること (カーネルの挙動を固定するテスト。
       もし継承されない環境が出たら、このテストが落ちて接続ごとの設定に戻せる)。
    5. 503 (上限超過) の経路の `set_write_timeout` も継承で要らなくなる。
  - 受け入れ基準: `strace -f -c` で `setsockopt` が 4.00 → 1.00 回/接続 (残る 1 回は 2 要求目の猶予)。
    1 接続 1 要求の CPU/接続 が下がること (交互 3 回の中央値)。keep-alive・connect・HIT が退行しないこと。全テスト通過。

- [ ] **T9.4 accept したスレッドがそのまま接続を処理する (受け渡しの `futex` をなくす)**
  - 目的: いまは accept 専用スレッドが接続を受け、チャネルでワーカーへ渡す。渡すたびに `futex` 2 回
    (起こす + 待つ) と別コアでの起床が要る (T8.2 の内訳)。1 接続 1 要求の p50 が 0.33 ms と keep-alive の
    0.18 ms より悪いのはここ。
  - 変更箇所: `src/lib.rs` (`serve`)、`crates/workers/src/workers.rs`。
  - やること: leader / follower にする。待ち受けで `accept()` を待つスレッドを複数持ち、accept した
    スレッドが**自分で**その接続を処理する (処理が終わるか、預けたら待ち受けに戻る)。
    accept したとき「待ち受けに誰も残っていない」なら 1 本だけ起こす (このときだけ `futex`)。
    起こした数と待っている数を `AtomicUsize` で数え、待ち受けのスレッド数には上限を置く (設定は増やさない。
    `MAX_IDLE` と同じ 64 で十分か、実測で決める)。監視スレッドから戻る接続は従来どおりワーカー経由で良い。
    同時接続数の 503 (`PROXY_MAX_CONNS`) の判定と `rejected_overload` はそのまま動くこと。
    まず実装して測り、効かなければ戻して §4 に書く (この計画の他の案と同じ)。
  - 受け入れ基準: `strace -f -c` で `futex` が 2.00 → 0.1 回/接続 以下、1 接続 1 要求の CPU/接続 が 5% 以上下がること。
    keep-alive (預ける経路を含む)・connect・上限 8 で 9 本目が 503 になるテストが通ること。

- [ ] **T9.5 ユーザー空間の 16 us/要求 の内訳を出し、上位を潰す** (T8.4 を含む)
  - 目的: keep-alive の経路はシステムコール 5 回で床に着いた (`recvfrom` 3 + `sendto` 2、うち 1 はプールの生存確認で
    §4 のとおり残す)。残りはユーザー空間 16 us (全体の 1/3) とカーネル 35 us で、ユーザー空間の内訳は
    T1.5 (128 us 時代の区間計測) 以来取っていない。当時 3.8% だった要求解析は、いまなら 1 割になっている計算。
    確保 41.1 回/要求 の内訳 (T8.4) も同じ道具で出る。
  - 変更箇所: 内訳しだい (`crates/msg`・`crates/http`・`crates/origin`・`src/lib.rs` のどれか)。
  - やること: `CARGO_PROFILE_RELEASE_STRIP=none CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --release` で
    シンボルを残し、forward 8 並列を `perf record -g` (この環境はユーザー空間だけ数える) で取って上位 10 関数を表にする。
    確保は数えるアロケータに `std::backtrace` で呼び出し元を記録させ (計測時だけ。コミットには入れない)、
    上位を表にする。**5% 以上取れるものだけ** 潰し、1 つずつ測る。
  - 受け入れ基準: 関数と確保の内訳の表が TASKS に残り、ユーザー空間の CPU/要求 が 5% 以上下がるか
    「これ以上は効かない」と根拠つきで記録されていること。

## 付録 A. 計測の記録 (時系列)

着手時からの数字の履歴。**現在地は §2**。Phase 5 以降の数字は各タスクの `結果:` にある (ここには重複して書かない)。

### 着手前 (2026-09-07、Python ベンチ)

環境: 8 コア / 6.6 GiB RAM、loopback、`target/release` (当時 opt-level=s, lto)、`PROXY_CACHE_ENABLED=off PROXY_STATS_PERSIST=off PROXY_LOG_LEVEL=warn`。
計測ツールは `scripts/bench.py` (Python 標準ライブラリのみ)。**Python 側が上限になる** ので絶対値は低めで、前後比較専用。

| 項目 | 値 | 備考 |
|---|---|---|
| forward, 8 並列 keep-alive, 1 KiB 応答 | **176 req/s, p50 44 ms** | プロキシ無しの直結は 621 req/s, p50 9.6 ms |
| forward, 64 並列 | 371 req/s, p50 66 ms, p99 1.5 s | |
| CONNECT トンネル 1 本のスループット | 881 MiB/s | 256 MiB を Python から送出 |
| CONNECT 確立/秒, 64 並列 | 398 tunnels/s | 短命トンネル |
| リリースビルド時間 | 25〜28 s | クリーンビルド |
| バイナリサイズ | 857 KB | |
| テスト | 150 単体 + 21 結合、約 3 s | |

**着手時に判明していた最大のボトルネック** (T1.1 で解消): `TCP_NODELAY` を立てていないため、応答ヘッダーと本文を別々に `write` した際に
Nagle + delayed ACK で **1 要求あたり約 40 ms 止まっていた**。両側に `set_nodelay(true)` を入れると 8 並列で 176 → 674 req/s、
p50 44 → 9.6 ms (直結と同等 = Python ベンチの上限)。

### Rust ベンチでの推移 (Phase 0〜1)

同じ環境、`--body-bytes 1024`、8 並列。`direct` はプロキシを通さないオリジン直結 (ベンチ自身の上限)。

| 時点 | forward 8 並列 | forward 64 並列 | tunnel 1 本 | connect 8 並列 | 備考 |
|---|---|---|---|---|---|
| T0.1 | 188 req/s, p50 42.0 ms, p99 54.1 ms | 1,517 req/s, p50 41.9 ms | 1,044 MiB/s | 3,506 /s, p50 1.85 ms | direct **300,805 req/s, p50 0.013 ms** |
| T1.1 `TCP_NODELAY` | **29,107, p50 0.222, p99 1.33** | 22,027, p50 1.97, p99 15.8 | 1,114 | 3,938, p50 1.70 | 155 倍 |
| T1.2 1 回の `sendto` | 30,231, p50 0.216 | | | | HIT 60,813 → **75,563 req/s**, p50 0.070 ms |
| T1.3 `poll` + `splice` | | | **2,800** | **7,375**, p50 0.68 | トンネルのスレッド 3 → 1、待ち受けのみ 5 スレッド |
| T1.4 余計な仕事をしない | 30,699 | | | 8,293 | 起動直後のスレッド 5 → 3 |
| T1.5 中継バッファの使い回し | **32,842** | **25,912** (+15.7%) | | | 1 並列 6,697 → 8,286 |
| T1.6 プール | 32,074 | 26,142 | | | `pool_hit_ratio` 0.9712 |

connect 64 並列は T0.1 → T1.1 で 3,648 → 3,288 tunnels/s (p50 17.3 → 18.1 ms)、forward 64 並列の p99 は 51.3 → 15.8 ms。
direct は T1.1 後も 300,542 req/s (p50 0.014 ms) でベンチは律速しない。

**T1.5 の内訳** (conn 1 本、1 要求あたり、`Instant` を一時的に仕込んで測定。全体 128 us):

| 区間 | 時間 | |
|---|---|---|
| 要求解析 (parse_request_headers + parse_origin) | 4.9 us | **3.8%** |
| キャッシュ判定 | 2.2 us | |
| 要求ヘッダー組み立て | 1.6 us | |
| オリジン接続の取得 (プール) | 5.2 us | |
| オリジン往復 | 61.2 us | 大半は待ち時間 |
| 応答ヘッダー整形 | 5.6 us | |
| 本文の配信 | 24.2 us → 17.9 us | 毎要求 64 KiB を確保・ゼロ埋めしていた |
| 統計・アクセスログ | 3.3 us | |

確保回数は 1 要求あたり 98.7 回 (数えるアロケータで測定)。解析の割合が 5% 未満なので**ヘッダーのスライス化は見送り**。

### T2.3 の判断 (同時 5,000 本のアイドルトンネル)

| | RSS | fd/本 | 新規 CONNECT p50 / p99 |
|---|---|---|---|
| 接続スレッドのスタック 256 KiB のみ | 199 MiB | 7.0 | 1.18 / 9.07 ms |
| + パイプの遅延生成・CONNECT 中の BufReader 解放 | **198 MiB** | **2.0** | 1.59 / 9.13 ms |

受け入れ基準 (RSS 200 MiB 未満、p99 10 ms 未満) を満たすので **epoll リアクターは作らない**。
RSS の大半はスレッドスタックの実使用ぶん (1 本あたり約 36 KiB) で、fd とヒープはもう削ってある。

### T3.2 `--lite` の比較 (同じ機械で連続、8 並列)

| プロファイル | forward | connect | 起動直後のスレッド |
|---|---|---|---|
| 既定 (キャッシュ + 統計あり) | 19,475 req/s, p50 0.295 ms | 7,440 tunnels/s | 6 |
| `--lite` | **33,125 req/s, p50 0.199 ms** | **8,350 tunnels/s** | **3** |

### T4.2 `opt-level` の比較 (`--lite`、8 並列、forward は 3 回の平均)

| | forward | tunnel | connect | バイナリ |
|---|---|---|---|---|
| `"s"` (当時) | 32,419 req/s | 2,681 MiB/s | 8,177/s | 923,040 B |
| `3` | 33,121 req/s (+2.2%) | 2,577 MiB/s | 8,231/s | 1,054,112 B |

### T4.4 システムコールの削減 (`strace -f -c`)

| | システムコール/要求 | CPU/要求 | forward |
|---|---|---|---|
| 前 | 10.06 (setsockopt 4.00, getsockname 1.00) | 89.9 us (user 34.6 / kernel 55.3) | 25,321 req/s |
| 後 | **5.06** (recvfrom 3.00 + sendto 2.00 だけ) | **85.0 us** (user 32.3 / kernel 52.8) | **26,685 req/s** |

3 回測って 3 回とも改善 (**-5.4% CPU / +5.4% スループット**)。残る 5 回は本質的な入出力。
その前段で確保を 90.2 → 76.3 回/要求 に減らしたが速度は誤差の中 (91.8 → 93.3 us、ノイズ ±5%) だった。理由は
プロキシの CPU の 63% がカーネル側で、ユーザー空間の確保・解放 12% = 全体の約 4 us だから (14 回減らしても 0.6 us)。

### Phase 4 完了時点の値

| 項目 | 着手前 | Phase 4 完了時 |
|---|---|---|
| forward, 8 並列 (`--lite`) | 188 req/s, p50 42.0 ms | **32,419 req/s, p50 0.20 ms** (172 倍) |
| forward, 8 並列 (キャッシュ HIT) | — | **75,563 req/s, p50 0.070 ms** |
| forward, 64 並列 | 1,517 req/s, p50 41.9 ms | 25,912 req/s, p50 1.6 ms |
| CONNECT トンネル 1 本 | 1,044 MiB/s | **2,681 MiB/s** |
| CONNECT 確立, 8 並列 | 3,506 tunnels/s | **8,283 tunnels/s** |
| 起動直後のスレッド | 6 | 6 (既定) / **3** (`--lite`) |
| 同時 5,000 トンネル | 未計測 | RSS 198 MiB、新規 CONNECT p99 9.1 ms |
| バイナリ | 857 KB | 923 KB |
| テスト | 150 単体 + 21 結合 | **158 単体 + 28 結合** |

### perf / strace での確認

- **T0.2**: `CARGO_PROFILE_RELEASE_DEBUG=1` だけでは `profile.release` の `strip = true` に消されるので、
  `CARGO_PROFILE_RELEASE_STRIP=none` も要る (README を修正)。付ければ `perf record -g -p <pid>` → `perf report` でシンボルが出る。
- **T1.2**: `strace -f -e trace=write,sendto` で 1 要求あたりのクライアント向け呼び出しを直接確認した
  (Rust の `TcpStream` は `write` ではなく `sendto` を使う)。

  ```
  変更前 (4717a3d): sendto(7, "HTTP/1.1 200 OK...", 90)   ← ヘッダー
                    sendto(7, "xxxxxxxx...",        1000) ← 本文
  変更後:           sendto(7, "HTTP/1.1 200 OK...", 1090) ← 1 回
  ```

- **T1.4**: `PROXY_CACHE_ENABLED=off` のとき、`perf report` に `cache::` / `freshness::` のシンボルは出ない
  (唯一出るのは起動時の `Ballast` の drop が 0.01%)。`strace -c` のファイル系システムコールも 28,644 要求で
  `openat` 18 回・`statx` 2 回・`mkdirat` 2 回 (すべて起動時) で、要求あたり 0。キャッシュ有効時は 58,078 要求で `openat` 283 回・`mkdirat` 257 回。
- 旧 `scripts/bench.py` でも forward 8 並列 p50 44 ms → **11.0 ms** になった (直結が p50 9.6 ms なので Python 側の下限。実際の値は Rust ベンチの 0.20 ms)。

### 2026-09-07 深夜の測り直し (Phase 9 の着手前。この機械の今の状態)

`scripts/cpu-per-request.sh` で 5 秒ずつ (8 並列、`--lite`、本文 1 KiB)。§2 の値より 2 割ほど重く出ているが、
同じ日に同じ道具で測った前後比較にしか使わない (機械の状態が違う。この日は計測中に別の作業も走っていた)。

| 経路 | スループット | p50 | CPU/操作 | 内訳 (user / kernel) |
|---|---|---|---|---|
| forward keep-alive | 30,039 req/s | 0.183 ms | **50.9 us/要求** (3 回で 50.9 / 55.3 / 58.9) | 16.2 / 34.7 |
| forward 1 接続 1 要求 (`--no-keepalive`) | 11,955 req/s | 0.330 ms | **123.3 us/接続** | 29.1 / 94.2 |
| connect (短命トンネル) | 6,068 /s | 0.224 ms | **187.3 us/本** | 26.3 / 161.0 |
| tunnel 1 本 (256 MiB) | 1,301 MiB/s | — | 273 us/MiB | 0 / 273 |
| キャッシュ HIT (メモリ 64 MiB) | 59,461 req/s | 0.091 ms | **30.6 us/要求** | 13.1 / 17.5 |

システムコール (`strace -f -c`、プロキシを strace の下で起動): keep-alive は **5.03 回/要求**
(`recvfrom` 3.00・`sendto` 2.00、`futex` 0.026、`setsockopt` 0.005)。1 接続 1 要求は **14 回/接続** (内訳は T8.2)。
暇なプロキシは 10 秒で `epoll_pwait` 10 回 (監視スレッドの 1 秒周期) だけで、起動後は他に起きない (lite・既定とも)。
