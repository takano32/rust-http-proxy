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
8. 実装を Opus (サブエージェント) に渡すときは §5 冒頭の「実装を Opus に渡すときの決まり」を指示文に貼る。

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
| forward, 8 並列 (`--lite`) | 188 req/s, p50 42.0 ms | **39,389 req/s, p50 0.176 ms** (210 倍) |
| forward, 8 並列 (キャッシュ HIT) | — | **75,563 req/s, p50 0.070 ms** |
| forward, 64 並列 | 1,517 req/s, p50 41.9 ms | 25,912 req/s, p50 1.6 ms |
| CONNECT トンネル 1 本 | 1,044 MiB/s | **2,681 MiB/s** |
| CONNECT 確立, 8 並列 | 3,506 tunnels/s | **8,283 tunnels/s** |
| トンネル 1 本あたりのスレッド | 3 | **1 (中継中) / 0 (暇なとき)** |
| 暇な CONNECT トンネル 5,000 本 | 5,005 スレッド / RSS 93.9 MB | **68 スレッド / 71.5 MB** |
| 暇な keep-alive 接続 2,000 本 | 2,003 スレッド / RSS 72.3 MB | **18 スレッド / 25.7 MB** |
| 1 要求あたりの確保回数 | 98.7 | **10.0** |
| 1 要求あたりのシステムコール | 10.06 | **5.02** |
| ビルドが通る最小のメモリ | 347 MB でも通らない | **110 MB** (CI) / 100 MB (手元) — 上限 200 MB に対し 90 MB の余裕 |
| バイナリ | 857 KB | 1,382 KB (release) / **988 KB** (dist) |
| CPU/要求 (forward / HIT / CONNECT 確立) | — | **42.7 / 38.7 / 170.3 us** |
| テスト | 150 単体 + 21 結合 | **189 単体 + 48 結合** |

### 完了の定義 (§0 のゴールに対して)

- [x] Phase 0〜1 と T2.1・T2.2・T3.1・T3.2・T3.4 が完了、README の性能節に Rust ベンチの値がある
- [x] forward 8 並列 p50 が 1 ms 台 (loopback) — 実際は **0.20 ms**
- [x] トンネルが 1.5 GiB/s 以上 — **2.6 GiB/s**
- [x] 既定設定で `cargo test --workspace` 全通過 (180 単体 + 42 結合)
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
| 冷たい 9 クレートを `opt-level = "s"` に (T9.2) | バイナリ **1,316,336 B のまま** (1 バイトも変わらない。`.text` は -17,952 B だが例外表が +6,660 B、セクション合計 -10,940 B は 64 KiB のセグメント境界の詰め物に吸われる)、`dist` も 988,648 B のまま、通る最小のメモリ 100 MB のまま、クリーンビルド 37.6 / 36.3 → 36.0 / 36.1 秒、CPU/要求 forward +2.3% / connect -4.4% / HIT -0.2% | **払うものは無いが得るものも無い**。冷たい層は合計しても `.text` の 2% しかなく、減った分はページ境界の詰め物に消える。9 行の override を Cargo.toml に置く (クレートが増えるたびにどちらの層か決める) 分だけ損 (T9.2) |
| accept したスレッドがそのまま接続を処理する (leader/follower、T9.4) | `futex` 2.02 → **0.02 回/接続**、システムコール 11.05 → 9.04 回/接続。だが 1 接続 1 要求の CPU/接続 114.99 → 113.04 us (**-1.7%**、目標 -5%)、p50 0.321 → 0.319 ms で**動かない**。keep-alive 48.56 → 50.14 us (**+3.3%**)、CONNECT 確立 176.25 → 187.53 us (**+6.4%**)、忙しいときのスレッド 12 → 18 / 68 → 131 | **受け渡しの `futex` は遅さの原因ではなかった**。この機械では 1 回 1 us なので 2 回消しても上限 -2%。しかも長生きする仕事 (keep-alive の接続・トンネル) を accept したスレッドが抱えるので群れが上限 64 まで育ち、そこから先は結局ワーカーへ渡す = 両方の費用を払う (実装 `f7eb35e`、差し戻し `42889d8`) |
| クライアント書き込みバッファ (64 KiB) を `CopyBuf` と同じくプールする (T9.5) | user CPU 12.15 → 11.65 us (**-4.1%**、交互 3 組とも同じ向き)、全体 43.57 → 42.72 us (-2.0%)、確保 10.0 → 9.0 回/要求 | **5% (user で 0.8 us) に届かない**。`BufWriter` を手書きの版で置き換える (flush・`Drop`・容量超えの直書きの分岐) 保守と誤りのリスクに見合わない。**tcache に載らない唯一の確保なので、小さい確保と違って効きはする** (段 3 と対照的)。やり方は「`crates/http` に `ClientOut` を足し、`let client = &mut BufWriter::with_capacity(...)` を差し替える」だけ。次のラウンドで採るなら判断材料はこれで足りる |

## 5. これから (未着手)

上と同じ形式。**着手する前に必ず「今どうなっているか」を測る**こと。

### 実装を Opus に渡すときの決まり

ここから下のタスクは、1 つずつ Opus (Claude Code のサブエージェント) に渡して実装する前提で書いてある。
渡すときの指示はこの節と各タスクの本文だけで足りるようにしてある (別のメモは要らない)。

**渡し方**: 1 タスク = 1 エージェント = 1 コミット。直列に回す (ベンチが重なると計測が汚れる。`TASKS.md` の取り合いにもなる)。
指示文には「最初に読むもの」として §0・§1・§4 とそのタスクの本文、触るクレートのソース全体を挙げ、
下の「エージェントが守ること」を貼る。おすすめの順は **T9.3 → T9.4 → T8.1 → T9.5 → T8.5 → T8.6**
(効きの大きさと、後のタスクが前の結果を使う順。T9.4 は T9.3 の `SO_RCVTIMEO` を、T8.5 は T8.1 の結果を使う)。

**エージェントが守ること** (指示文にそのまま貼る):

1. 外部クレートは 1 つも足さない。システムコールは `crates/sys/src/sys.rs` と同じ `unsafe extern "C"` + `#[cfg(target_os = "linux")]`、
   他 OS には従来コードを残す。認証は入れない。既存機能を壊さない。
2. `cargo fmt` → `cargo clippy --workspace --all-targets -- -D warnings` → `cargo test --workspace` → `cargo build --release` を通してからコミット。
   `.cargo/config.toml` の `jobs = 1` はそのまま。
3. 性能に関わる変更は、**変更前のバイナリを退避して、変更後と交互に 3 回ずつ 10 秒** (`scripts/cpu-per-request.sh`、`BIN=` で切り替え) 測り、
   中央値で比べる。差が 5% 未満なら「ぶれの中」と正直に書く。システムコールは strace の下でプロキシを起動して数える (§1)。
   計測中は他のビルド・ベンチを走らせない。終わったらプロキシとベンチのプロセスを残さない (`pgrep -x rust-http-proxy` が空)。
4. 効かなかったら**戻す**。戻した案は §4 の表に数字つきで 1 行足す (これも 1 コミット)。
5. コミットメッセージは既存の形式 (`perf:` / `feat:` / `fix:` / `docs:` / `chore:` / `refactor:` / `test:` + 日本語 1 行 + 本文に前後の表)。
   `Co-Authored-By:` にモデル名を付ける。**`git push` はしない** (main にもブランチにも。CI で確かめたいときは人が push する)。
6. 終わったらそのタスクを `[x]` にして `結果:` に実測値とコミットを書く。§2 の表に効く数字が変わったら更新する。
   環境変数や挙動を足したら README の表と説明も更新する。
7. 最後の報告に、コミットハッシュ・前後の表・テストの本数・やり残しと気づいた問題を入れる。

**この機械で分かっている計測の癖** (エージェントに伝える): `strace -p` は `ptrace` が拒まれる (strace の下で起動する)。
`perf` は `perf_event_paranoid = 2` でユーザー空間しか数えず、既定の `cycles` はサンプルがほぼ取れないので `-e cpu-clock` を使う。
CPU/要求 は 5 秒の計測で ±8% ぶれる。手元の cgroup の最小メモリは CI より 10 MB 低く出る。

### Phase 8 — 前のラウンドからの持ち越し

- [x] **T8.1 CONNECT トンネルも暇なときは監視スレッドに預ける**
  - 目的: トンネルは今も 1 本 1 スレッド。実測で同時 5,000 本のとき RSS 140〜198 MiB (T2.3)。HTTP 側の keep-alive 接続は
    T6 で監視スレッド (epoll) に預けられるようになったので、両方向が暇なトンネルも同じ仕組みに預ければスレッドが要らない。
  - 変更箇所: `crates/tunnel/src/tunnel.rs` (`handle_connect` / `relay::run` / `Dir` / `Relay`)、`src/idle.rs` (`IdleWatch`)、
    `src/lib.rs` (`pump` の `Step::Connect`)、`crates/bench/src/main.rs` (計測モード)、`tests/tunnel_test.rs`。
  - 読む場所: `relay::run` は poll + splice の 1 スレッドループで、`Dir` (方向ごとの状態) と `Relay` (パイプ / バッファ、
    最初にデータが動くまで `Unset`) を持つ。`IdleWatch` は `HashMap<RawFd, Parked { conn: Box<Conn>, deadline }>` と
    `BTreeSet<(Instant, RawFd)>` を持ち、`park` はロックを握ったまま epoll に足し、`take` / `expire` で外す。
    `pump` は `Step::Connect` で `Conn` から `client` だけ部分ムーブし、残り (`_open` / `_active` のガード) は式の終わりまで生きる。
  - やること:
    1. `relay::run` を「暇になるまで回して戻る」形に割る: `pub struct Idle` (2 つの `TcpStream`、`Dir` ×2、conn_id、宛先の文字列、
       開始時刻、`connect_took`、`Arc<Metrics>`、接続元 IP、`idle` の長さ) と `fn run_until_idle(&mut self) -> Outcome { Done(u64) | Idle }`。
       終わったときのアクセスログと統計 (`record_host_timed` / `record_client` / `access`) は `Idle` を落とす側で 1 回だけ出す
       (どのワーカーで終わっても同じ関数を通す)。
    2. **預けられる条件**: 両方向とも `pending == 0` かつ `src_eof == false` かつ `done == false` で、直前の `poll` が両方空振り。
       片方向だけ EOF (half-close) のトンネルは預けない (単純さを優先。理由をコメントに)。
    3. 預ける前に `Relay::Pipe` を落として `Relay::Unset` に戻す (方向あたり fd 2 本を持たない。戻ったら遅延生成のまま作り直す)。
    4. `IdleWatch` の預かりものを `enum Parked { Http(Box<Conn>), Tunnel(Box<tunnel::Idle>) }` にする。依存の向きは 本体 → `proxy-tunnel`
       なので、起きたら `tunnel::resume(idle)` のようにトンネル側の関数へ渡せばよい。**トンネルは fd が 2 本**: 両方を
       `EPOLLIN | EPOLLRDHUP` で登録し、fd → 同じエントリ を引ける表を持つ (例: `HashMap<RawFd, Key>` + `HashMap<Key, Parked>`)。
       どちらかが起きたら**両方を epoll から外し、両方の鍵を消してから**ワーカーへ渡す。期限切れも同じ。
       1 回の `epoll_wait` で同じトンネルの 2 事象が来たら 2 つ目は `take` が `None` を返して無視される (確かめること)。
    5. 起きた事象が `EPOLLIN` を含まず HUP / ERR だけのときも、HTTP 側と違って**ワーカーへ戻す** (トンネルは終わるときに
       shutdown・アクセスログ・統計が要る)。
    6. 期限は `config.tunnel_idle` (既定 300 s)。`0` (無期限) のときは `Instant::now().checked_add(Duration::from_secs(365 * 86400))`
       のような遠い期限を使い、`None` なら預けない。期限切れで引き上げたトンネルはワーカーで「idle timeout」として閉じる
       (今 `poll` が 0 を返したときと同じログと統計)。
    7. `Conn` の `_open` / `_active` (同時接続数と `active_connections` の持ち分) は `Step::Connect` のあとも生きている。
       預けるならこの 2 つのガードもトンネル側の状態に移して一緒に運ぶ (そうしないと預けた瞬間に数が減り、`PROXY_MAX_CONNS` の意味が壊れる)。
    8. `metrics.parked_connections` は HTTP と合算でよい。`/status` に `parked_tunnels` を別に出すと計測が楽 (任意)。Linux 以外は一切変えない。
    9. 計測のために `proxy-bench` に `--only idle-tunnels` を足す: `--conc N` 本の CONNECT を「accept して持ち続けるだけのリスナー」に張り、
       `200 Connection Established` を読んだら `--seconds` 秒そのまま握る。5,000 本張るので、ベンチ側は接続を `Vec` に持って
       1 スレッドで握る (5,000 スレッドを作らない)。プロキシ側は `PROXY_MAX_CONNS` (既定 4096) に当たるので計測時は `PROXY_MAX_CONNS=0` か 8192。
       `scripts/cpu-per-request.sh` に「ベンチ実行中の最大スレッド数」のサンプリングを足す (今は開始と終了の値しか出ない)。
  - 受け入れ基準: 同時 5,000 本のアイドルトンネルでスレッド数が 5,000 → 数十、RSS が下がること (前後の値を表に)。
    `--only tunnel --conc 1` のスループットと `--only connect` の CPU/本・p99 が交互 3 回で悪化しないこと。forward keep-alive も 1 回。
    結合テストを足す: 預けられたトンネル (少し待ってから送る) がその後もデータを通す、`tunnel_idle` で閉じる、片側が閉じたら相手も閉じる。
    既存の `tests/tunnel_test.rs` 3 本と keep-alive の park 系テストが通ること。
  - 結果: 暇なトンネル 5,000 本で **5,005 スレッド・RSS 93.9 MB → 68 スレッド・71.5 MB**。
    両方向とも動きの無いトンネルは、記述子 2 本 (クライアントとオリジン) を同じ鍵で `idle-watch` の epoll に預け、
    ワーカースレッドと中継パイプを手放す。

    | 同時 5,000 本のアイドルトンネル | 前 | 後 |
    |---|---|---|
    | スレッド (握っている間の中央値) | 5,005 | **68** |
    | ピーク RSS | 93.9 MB | **71.5 MB** |
    | 預けている本数 (`/status` の `parked_tunnels`) | — | 5,000 |
    | 一斉 close の瞬間のスレッド最大 | 5,005 | 4,621 (一時的) |

    悪化していないことの確認 (前後交互 3 回の中央値、10 秒):

    | 指標 | 前 | 後 |
    |---|---|---|
    | tunnel 1 本 | 1,280.0 MiB/s | 1,252.1 MiB/s (-2.2%、ぶれの中) |
    | connect CPU/本 | 182.29 us | 176.23 us (-3.3%) |
    | connect p99 | 1.907 ms | 1.960 ms (+2.8%) |
    | forward CPU/要求 | 49.28 us | 49.61 us (+0.7%) |

    トンネルのスループットは別に 5 回ずつも取った (前 1,274.8 / 後 park=on 1,273.2 / 後 park=off 1,299.5 MiB/s の中央値)。
    差はぶれの中 (この機械の tunnel は 1 回ごとに ±15% 動く)。

    **猶予はトンネルだけ下限 100 ms** にした。`PROXY_PARK_GRACE_MS` の既定 3 ms のままだと、短命トンネルが
    「閉じられる直前に預けて、すぐ起こされる」往復を丸ごと払い、connect の CPU/本 175.19 → 190.24 us (**+8.6%**)、
    p99 1.709 → 2.263 ms (**+32%**) と受け入れ基準を割った。トンネルの往復は記述子 2 本の epoll 出し入れ +
    中継パイプの作り直しで HTTP より高い一方、預けたいのは秒〜分の単位で暇なトンネルなので 100 ms 待って損はない。
    HTTP 側の `PROXY_PARK_GRACE_MS` の意味は変えていない (README に明記)。

    アクセスログと統計は `tunnel::Idle` の `Drop` に置き、**どのワーカーで終わっても、預かり所が期限切れで落としても 1 回だけ**通る。
    片方向だけ EOF (half-close) のトンネルは預けない (単純さを優先)。`Conn` の `_open` / `_active` は `Box<dyn Send>` にして
    トンネルへ運ぶので、**預けても `PROXY_MAX_CONNS` に数える**。起きた事象が HUP / ERR だけのときもワーカーへ戻す
    (トンネルは終わるときに shutdown・アクセスログ・統計が要る)。**Linux 以外は一切変えていない。**
    結合テスト 4 本追加 (預けたあとも通る / 上限に数える / 片側が閉じたら相手も閉じる / 両側同時 close)。
    **1 回の `epoll_wait` に同じトンネルの事象が 2 つ来て 2 つ目が `take` で `None` になり無視されること**は、
    一時プローブを仕込んで 30 本を両端同時に閉じ、その枝を 9 回踏むことで確認した (プローブはコミットしていない)
    (`9dc1700`、ベンチの `--only idle-tunnels` とスクリプトのスレッド数サンプリングは `9b71c50`)
    - **一斉 close のスレッドの山**: 5,000 本が同時に切れると 1 本ごとにワーカーへ渡すので一時的に最大 4,621 スレッドまで増える
      (中央値は 68、RSS のピークもこれ込みで 71.5 MB)。閉じるのに shutdown・アクセスログ・統計が要るため監視スレッドでは落とせない。
      同じ性質は HTTP の預かり接続が一斉に要求を送ったときにもある (T6.4 から)。`Workers` に「生きているスレッドの上限」を
      入れる話になるので T8.1 の範囲外とした。
    - `--only idle-tunnels` の CPU/本 は指標として弱い (確立 + 保持 + 一斉 close の合計 ÷ 本数で、前 330〜556 us、
      後 298〜464 us と実行ごとに揺れる)。受け入れ基準はスレッド数と RSS で見た。

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

- [x] **T8.4 1 要求あたりの確保 41.1 回の内訳を出す** → **T9.5 で取った** (呼び出し元別・大きさ別の表は T9.5 の `結果:`)。

- [x] **T8.5 `PROXY_MAX_CONNS` の既定 4096 を見直す**
  - 目的: 上限の意味が変わった。T2.1 のときは「同時に立つスレッド数」の歯止めだったが、アイドル接続を預けるようになった今は
    実質「開いている記述子の数」の歯止め。T8.1 が入るとトンネルも同じになる。
  - 変更箇所: `crates/config`、`src/lib.rs` (`serve` の判定)、必要なら `crates/sys/src/sys.rs` (`getrlimit`)、README の環境変数の表。
  - やること: 動作環境 (Pterodactyl のコンテナ) の `ulimit -n` の既定を確かめる (Docker の既定は 1048576 だが Wings の設定で変わる。
    分からなければ「1024 の環境でも 4096 の環境でも記述子切れで accept が失敗する前に 503 で断れる値」を選ぶ)。
    1 接続あたりの fd は クライアント 1 + オリジン 1 + (素通し中の) パイプ 2。上限に当たったときの挙動 (503 + `Retry-After: 1`) は既にある。
    案: `PROXY_MAX_CONNS=auto` (既定) を足し、起動時に `getrlimit(RLIMIT_NOFILE)` の soft limit から `(soft - 予備 64) / 4` のように決める
    (Linux 以外は固定値 4096)。**挙動を変えるなら実測とテスト**、変えないなら README に根拠を 1 行、で十分。
  - 受け入れ基準: 既定値の根拠が 1 行で説明できること。`auto` を入れるなら、`ulimit -n 256` で起動したときに記述子切れではなく 503 で断る結合テスト。
  - 結果: **既定を `auto` にした。`auto = min(4096, (RLIMIT_NOFILE の soft − 予備 64) ÷ 4)`。**
    1 接続が最悪で使う記述子は クライアント 1 + オリジン 1 + 素通しのパイプ 2 = **4 本**。

    | `ulimit -n` | 決まる上限 | 満杯のとき使う記述子 |
    |---|---|---|
    | 256 | 48 | 192 |
    | 1024 | 240 | 960 |
    | 4096 | 1008 | 4032 |
    | 524288 (開発機) | 4096 (頭打ち) | 16384 |
    | 1048576 (Docker の既定) | 4096 (頭打ち) | 16384 |

    4096 で頭打ちにするのは、上限が fd だけの歯止めではなく **fd 以外の資源 (スレッド・RSS) の歯止め**でもあるため
    (T2.3 の実測: 同時 5,000 本で RSS 198 MiB、動作環境のコンテナは小さい)。**soft が 16,448 以上なら従来と同じ 4096** なので、
    開発機でも Docker 既定でも挙動は変わらず、**記述子の少ない環境だけが下がる**。数値指定と `0` (無制限) は従来どおりで、
    下限は 1 (極端に低い `ulimit` でも `0` = 無制限には化けない)。読めない値は既定 (auto) に落ちる。
    `.env` 再読込は `Config::from_env` を通るので `auto` でも壊れない (`crates/reload` は数値を比べるだけなので無改修)。
    決まった値は起動ログに根拠ごと出る: `max connections: 48 (open file limit 256, 4 descriptors per connection)`。
    `getrlimit` / `setrlimit` の束縛は `crates/sys/src/sys.rs` に足したが、**番号 (`RLIMIT_NOFILE` = 7) も `rlim_t` の幅も
    arch/libc で違う** (32 ビット glibc は 32 ビット、32 ビット musl は 64 ビット。食い違うと呼び出し側のスタックを壊す) ので、
    T9.3 の `inherit_socket_options` と同じく **aarch64 / x86_64 だけ**で有効にし、他は固定 4096 に落とした。
    テスト 222 → **225 本**。`tests/maxconns_test.rs` は式の検算ではなく**実際に `setrlimit` して `ulimit -n 256` 相当にしてから起動**し、
    48 本握った時点で `/proc/self/fd` が 256 未満 (= 記述子はまだ余っている) であること、49 本目が 503 + `Retry-After: 1` で
    断られること、握りを返せばまた通ることを見る。`setrlimit` はプロセス全体に効くので**このテスト専用のテストバイナリ**にした
    (`tests/*.rs` は 1 ファイル 1 プロセス)。0.28 秒。性能ベンチは不要 (accept の判定は増えていない。式は起動時と `.env` 再読込時だけ) (`baaff2a`)
    - **`/status` には出していない** (起動ログのみ)。`/status` の JSON は `proxy-metrics` が組み立てていて `Config` を持たないので、
      出すには `endpoints::Endpoint` に `max_conns` を渡す配線が要る。「起動ログか `/status`」なので前者を採った。
    - **動作環境 (Pterodactyl / Wings) の `ulimit -n` は未確認** (開発機からは分からない)。16,448 未満なら auto が効いて上限が下がるので、
      本番で起動ログの `max connections:` を 1 度見ること。

- [x] **T8.6 `proxy-cache` と `proxy-http` をさらに割れるか調べる**
  - 目的: いちばん大きいのが `proxy-cache` (2,257 行、うち 936 行はテスト) と `proxy-http` (1,728 行)。どちらも「1 つの責務」に見えるが、内訳は見ていない。
    ビルドのメモリは本体クレート (`src/lib.rs` 903 行 + 依存の単相化) が最大を決めているので (T9.2 の知見)、割っても上限は下がらないかもしれない。
    その場合は「責務が分かれるか」だけで判断する。
  - やること: `crates/cache/src/cache/{mod,ops,probe,sink,status}.rs` と `crates/http/src/http/{mod,refresh,serve}.rs` について、
    モジュール間の `crate::` / `super::` 参照を数え (grep で十分)、参照の向きが一方通行の切れ目があるかを見る。
    候補は `probe.rs` (ディスクの実測の駆動、354 行) と `refresh.rs` (裏の再検証、175 行)。切れ目が無ければ「無い」と記録する。
    割るなら T7.4 と同じく下の層を同じ名前で再エクスポートして、呼び出し側の書き方を変えない。
  - 受け入れ基準: 割るか割らないかの判断が、依存の実測 (参照の数と向きの表) にもとづいていること。割ったらテストの本数と CPU/要求 が変わらないこと。
  - 結果: **割らない。切れ目が無い。** 両クレートのモジュール間参照を数えたところ、**一方通行 (逆参照ゼロ) の切れ目は 1 つも無かった**。

    | クレート / モジュール | 下 → 上 (子 → `mod.rs`) | 上 → 下 (逆参照) | 判定 |
    |---|---|---|---|
    | cache / `ops.rs` (338 行) | 型 11 + 非公開フィールド 35 + 非公開メソッド 7 | `pub use ops::PeekInfo` 1 | 相互 |
    | cache / `probe.rs` (354 行) | 型 9 + **非公開フィールド 14 種に 57 回** + `count_evictions()` | `probe::spawn` / `refresh_budget()` / `refresh_other_disk_usage()` の **3** | 相互 |
    | cache / `sink.rs` (119 行) | 型 2 + 非公開フィールド 6 + 非公開メソッド 4 | `pub use sink::{StoreOutcome, StoreSink}` 2 | 相互 |
    | cache / `status.rs` (119 行) | 型 2 + 非公開フィールド 6 | 0 | `impl Cache` なので動かせない |
    | http / `serve.rs` (235 行) | **非公開 struct `Ctx` の非公開フィールドに 20 回** | `serve_cached` 7 + `can_serve_stale` 3 + `pub use` 2 = **12** | 相互 |
    | http / `refresh.rs` (175 行) | `Shared` / `acquire_origin` / `request_head` / `CopyBuf` の 5 名前・16 回 | `refresh::spawn` **1** (`mod.rs:395`) | 相互 (逆参照は 1 つだけ) |

    **決め手は 2 つ。** ひとつは `proxy-cache` の 4 モジュールも `serve.rs` も、**`Cache` / `Ctx` の固有 impl か、その非公開フィールドを
    直接触るコード**だということ。Rust は固有 impl を型の定義クレートにしか置けないので、`Cache` を動かさずに `probe` だけ
    別クレートへ出す道は無い。自由関数に書き換えるなら `cfg` `quota` `mem` `disk` `margins` `disk_probe` ほか
    **14 個の非公開フィールドを `pub` に開ける**ことになり、隠蔽が壊れるだけで責務は分かれない。
    もうひとつは `probe.rs` が `Cache::new` から呼ばれていること (`mod.rs:223-224` の `refresh_other_disk_usage()` / `refresh_budget()`)。
    **予算の決定は「裏の仕事」ではなく `Cache` の構築の一部**で、責務としても切れていない。

    唯一 T7.4 と同じ形に持ち込めるのは `refresh.rs` (逆参照が `mod.rs:395` の `refresh::spawn` 1 つだけ) だが、切るには
    `Shared` / `CopyBuf` / `request_head` / `acquire_origin` (計 124 行) を新クレート `proxy-httpcore` に落として
    `proxy-revalidate` (175 行) と `proxy-http` (1,038 行) の 3 段にする必要がある。落とす 4 つは**素通し経路でも使う**ので
    熱い経路がクレート境界をまたぎ (LTO 無しの境界の値段は T8.3 の 1.31 us/要求 = ユーザー空間の 8.2%)、`pub(super)` を 3 つ
    `pub` に格上げする一方、増えた 2 クレートの依存は元と同じ広さのまま。**払うものだけあって得るものが無い** (T9.1 の thin LTO と同じ結論)。

    **ビルドメモリは下がらない見込み** (実測はしていない)。`jobs = 1` なので通る最小 = 最大の rustc 1 つで、T9.2 のとおり
    最大を決めているのは本体クレート。`cargo build --release` に食わせる行数は **`proxy-cache` 1,321 行 / `proxy-http` 1,358 行**
    (テストは両クレートとも別ファイルなので `#[cfg(test)]` で丸ごと落ちる) で、どちらも本体クレート
    (`src/lib.rs` 963 + `main.rs` 294 + `idle.rs` 291 = 1,548 行 + 26 クレートぶんの単相化) より小さく、**既に peak ではない**。
    2,257 行 / 1,728 行という見た目の大きさの 41% / 21% はテストで、release ビルドは最初から見ていない。

    **ついでに見つけたもの**: `crates/http/src/http/mod.rs:15` の `pub use serve::{Serve, write_cached_response};` は
    クレートの外に利用者がいない (リポジトリ全体を grep して 0 件)。`pub(super)` に落とせるが、公開 API を狭めるだけなので別扱い。

- [x] **T8.7 結合テストのオリジンが要求本文を読まず、機械が混むと落ちる**
  - 目的: `test_integration_request_body_on_a_reused_connection` が `taskset -c 0` で 4 回に 1 回落ちる (T9.1 の作業中に判明)。
    テスト側のオリジン (`tests/common/mod.rs` の `start_origin`) が `\r\n\r\n` までしか読まず、ヘッダーと本文が別セグメントで
    届くとエコーが空になる。プロキシの不具合ではない。
  - やること: ヘルパーが `Content-Length` ぶん本文を読み切ってから応答するように直す。
  - 受け入れ基準: `taskset -c 0 cargo test --test proxy_test request_body` を 10 回回して落ちないこと。
  - 結果: `start_origin` がヘッダー終端を見つけたあと、`Content-Length` ぶんに届くまで読み足すようにした。
    ハンドラに渡すもの (要求全文と通し番号) は変えていない。`taskset -c 0 … --test-threads=1` を 10 回:
    **直す前 3/10 が落ち** (`本文が転送される left: "" right: "name=value&x=1"`)、**直したあと 0/10**。
    `Content-Length` の取り出しは `start_body_echo_origin` と同じものだったので `content_length()` に括り出した
    (名前の大小を無視する)。**chunked は扱わない**: このリポジトリのテストに chunked の**要求**本文を送るものは無い
    (`Transfer-Encoding` が出てくるのは chunked の**応答** (`cache_test`) と、`Connection: Transfer-Encoding` の
    スマグリング試験 (自前のオリジンを立てている) だけ)。同じ読み方をしている他のヘルパー
    (`start_keepalive_origin` `start_408_on_second_request_origin` `start_sized_origin`) は本文付きの要求を
    受け取らないので触っていない。`cargo test --workspace` 219 通過

### Phase 9 — クレートを割ったことで手が届くようになった最適化

T7.1 / T7.4 で 1 クレートを 26 に割った結果、**手が届くようになったこと**が 3 つある。

1. **ビルドのメモリに 90 MB の余裕ができた** (347 MB → 110 MB、上限 200 MB)。ビルドを重くする最適化 (LTO、
   最適化レベル) は「200 MB に入らない」の一言で切っていたが、その数字は **7 クレート・並列ビルドのとき** のものだった。
   → 測り直した結果、**どちらも効かなかった** (T9.1: LTO の重さは最終リンク 1 回にかかるので割っても入らない。
   T9.2: 冷たい層の `opt-level = "s"` はバイナリが 1 バイトも変わらない)。コード生成側の調整はここで打ち止め。
2. **層ごとに別クレートになった**ので、ホットパスにコードを足しても (T8.1、T9.4) そのクレートが小さく、上限を脅かさない。
3. **システムコールの層 (`proxy-sys`) と計測の道具 (`proxy-bench`) が独立した**ので、束縛や計測モードを足しても
   プロキシ本体のビルドには効かない。

残っているのは **システムコールを減らす** (T9.3 / T9.4)、**スレッドを手放す** (T8.1)、**ユーザー空間の内訳を出す** (T9.5) の 3 系統。
CPU/要求 50.2 us の 68% はカーネル側なので、効く順もこの順。

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

- [x] **T9.2 ホットパスと関係ないクレートは `opt-level = "s"` に落とす**
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
  - 結果: **不採用。`opt-level = 3` のまま。** 候補 9 つ (`proxy-rrd` `proxy-sysinfo` `proxy-capacity`
    `proxy-diskprobe` `proxy-cachecfg` `proxy-config` `proxy-reload` `proxy-prom` `proxy-endpoints`) に
    `[profile.release.package."proxy-…"] opt-level = "s"` を当てて測った (`rustc` の引数が `opt-level=s` に
    変わっていることは `cargo build --release -v` で確認済み)。**受け入れ基準の「バイナリが小さくなり」を満たさない**:

    | 見るもの | 変更前 | 変更後 |
    |---|---|---|
    | バイナリ (release、strip 済) | 1,316,336 B | **1,316,336 B (0 バイト)** |
    | バイナリ (`--profile dist`) | 988,648 B | 988,648 B (0 バイト) |
    | 通る最小のメモリ (手元の cgroup) | 100 MB | 100 MB |
    | クリーンビルド (`jobs = 1`、2 回) | 37.6 / 36.3 秒 | 36.0 / 36.1 秒 |
    | CPU/要求 forward (交互 3 回の中央値) | 46.82 us | 47.88 us (+2.3%) |
    | CONNECT 確立 | 172.75 us | 165.20 us (-4.4%) |
    | キャッシュ HIT | 39.27 us | 39.21 us (-0.2%) |

    **なぜファイルサイズが 1 バイトも動かないか。** 中身は動いている: `.text` が 953,732 → 935,780 B (**-17,952 B**、-1.9%)。
    ところが `"s"` はインライン展開を控えるので関数が増え、例外表が太る (`.gcc_except_table` +3,988 B、
    `.eh_frame` +2,672 B、`.eh_frame_hdr` +672 B)。差し引きのセクション合計は -10,940 B (-0.8%) で、
    これが **64 KiB のセグメント境界 (aarch64 の `p_align = 0x10000`) の詰め物にそのまま吸われる**。
    ビルドのメモリが変わらないのも道理で、最大を決めているのは `opt-level = 3` のままの本体クレート。
    ビルド時間の差 (-0.3〜-1.5 秒) は 2 回の中でも重なる幅で、ぶれの中。
    CPU/要求 は符号がばらばら (+2.3% / -4.4% / -0.2%) でぶれ (±8%) の中。
  - 分かったこと: **冷たい層は小さすぎて効かない。** 9 クレートを全部 `"s"` にしても `.text` の 1.9% しか動かない
    (`endpoints::handle` は `impl Write` を取るジェネリック関数なので、単相化されるのは呼び出し側 = 本体で、
    そもそも `"s"` にならない。冷たいクレートに残る非ジェネリック関数は起動時か人が見に来たときにしか走らない)。
    `proxy-cachedisk` は別に測る予定だったが、9 クレートで何も動かない以上、1 つ足しても同じなので測っていない。
    9 行の override は「クレートを足すたびにどちらの層か決める」手間だけを増やすので、置かずに戻した。

- [x] **T9.3 accept した接続への `setsockopt` 4 回を、待ち受けソケットからの継承に置き換える**
  - 目的: 1 接続 1 要求では 14 システムコール/接続のうち 4 が `setsockopt` (T8.2 の内訳)。Linux では accept した
    ソケットが待ち受けソケットの `TCP_NODELAY` / `SO_RCVTIMEO` / `SO_SNDTIMEO` を引き継ぐ (`sk_clone_lock` が
    `struct sock` ごと複製する) ので、待ち受けに 1 回設定すれば接続ごとには要らない。
  - 変更箇所: `crates/sys/src/sys.rs` (`setsockopt` の束縛。`TcpListener` には `set_nodelay` が無い)、
    `src/lib.rs` (`serve` / `Conn::new` / `serve_one` の `read_timeout` の初期値)、`src/main.rs` (bind 直後)、`crates/net/src/net.rs` (`bind_all`)。
    → 実際に触ったのは `crates/sys/src/sys.rs` と `src/lib.rs` の 2 つだけ。`.env` の再読込に追従するには
    設定を引いている `serve` で当てるのが素直で、`bind_all` (`net`) や `main` は設定を持っていないため。
  - 今の経路: `Conn::new` が `set_write_timeout(timeout)` と `set_nodelay(true)` (2 回)。`serve_one` は `read_timeout` が `None` から
    始まるので 1 要求目で `set_read_timeout(timeout)` (3 回目)、2 要求目は猶予 `park_grace` に変えるので 4 回目。
    **4 回目は残す** (1 要求目 = timeout、2 要求目以降 = 猶予、という切り替えは要る)。503 の経路 (`OVERLOAD_RESPONSE`) でも `set_write_timeout` を呼んでいる。
  - やること:
    1. `sys.rs` に `setsockopt` の束縛 (Linux のみ)。定数は `SOL_SOCKET = 1`、`SO_RCVTIMEO = 20`、`SO_SNDTIMEO = 21`、
       `IPPROTO_TCP = 6`、`TCP_NODELAY = 1` (aarch64 / x86_64 とも同じ値。他の arch は `#[cfg]` で外して従来経路に)。
       `struct timeval { tv_sec: i64, tv_usec: i64 }` (64 bit Linux)。`pub fn inherit_socket_options(listener_fd: RawFd, timeout: Duration) -> io::Result<()>`
       の 1 関数にまとめ、失敗したら呼び出し側は従来どおり接続ごとに設定する (**フォールバックを必ず残す**)。
    2. bind 直後に待ち受けへ 3 つを設定し、`Accepted` (か `Conn::new` の引数) に「継承済みの timeout」を持たせる。継承済みなら
       `set_write_timeout` / `set_nodelay` を飛ばし、`Conn.read_timeout = Some(timeout)` から始める。Linux 以外は従来どおり。
    3. **`SO_RCVTIMEO` は `accept()` にも効く**: timeout 秒 (`PROXY_TIMEOUT`) ごとに `EAGAIN` で戻る。`serve` の accept ループで
       `WouldBlock` / `TimedOut` はログも待ちも無しに `continue` する (今は「その他のエラー」として `log_error!` + 100 ms sleep に落ちるので、必ず先に拾う)。
    4. `.env` の再読込で `timeout` が変わったら待ち受けに当て直す (`serve` は接続ごとに `config_of()` を引いているので、前回当てた値と違うときだけ `setsockopt`)。
    5. 503 の経路の `set_write_timeout` も継承で要らなくなる。
    6. 単体テスト (Linux のみ): 待ち受けに 3 つを設定 → `TcpStream::connect` → `accept` → accept したソケットの `nodelay()? == true`、
       `read_timeout()? == Some(..)`、`write_timeout()? == Some(..)` を確かめる (カーネルの挙動を固定するテスト。継承されない環境が出たら
       このテストが落ちて接続ごとの設定に戻せる)。`accept()` が `SO_RCVTIMEO` で `WouldBlock` になることも 1 本。
    7. 結合テストは既存のもの (`test_integration_keepalive_requests_are_not_delayed_by_nagle`、`test_integration_connection_limit_returns_503`、
       keepalive_test.rs の park 系) が通れば十分。
  - 計測: 変更前のバイナリを退避し、`scripts/cpu-per-request.sh --no-keepalive` を前後交互に 3 回ずつ。keep-alive・`--only connect`・
    キャッシュ HIT も前後 1〜2 回ずつ (退行が無いこと)。`strace -f -c` で `setsockopt` /接続 を前後で数える。p50 も表に。
  - 受け入れ基準: `setsockopt` が 4.00 → 1.00 回/接続。1 接続 1 要求の CPU/接続 が下がること (中央値)。keep-alive・connect・HIT が退行しないこと。全テスト通過。
  - 結果: **`setsockopt` は 4.00 → 1.00 回/接続** (受け入れ基準どおり。`strace -f -c`、1 接続 1 要求で
    111,928/27,973 → 26,499/26,463)。残る 1 回は「2 要求目を猶予 `park_grace` で待つ」ぶんで、これは残す仕様。
    CPU/接続 は前後交互 5 回ずつの中央値で **117.27 → 113.99 us (-2.8%)**。5 組すべてで後が低いので向きは確かだが、
    ぶれ (±8%) より小さい。3 回のシステムコールは 1 回 0.4 us 程度なので、この幅で妥当。

    | 1 接続 1 要求 (`--no-keepalive`、5 回の中央値) | 前 | 後 |
    |---|---|---|
    | CPU/接続 | 117.27 us | **113.99 us** (-2.8%) |
    | req/s | 13,047 | 13,205 |
    | p50 | 0.317 ms | 0.320 ms |
    | `setsockopt`/接続 | 4.00 | **1.00** |

    退行が無いことの確認 (前後交互、keep-alive と HIT は 2 回・connect は 5 回の中央値):
    keep-alive forward 48.11〜49.72 → 49.13〜50.75 us (ぶれの中)、`--only connect` 187.58 → 180.26 us (-3.9%)、
    キャッシュ HIT 40.00〜41.07 → 39.62〜40.28 us。keep-alive の 1 要求あたりのシステムコールは 5.04 のまま
    (`setsockopt` は 0.0048 → 0.0016 回/要求。keep-alive では元々 1 接続ぶんが多数の要求に薄まっている)。
    `--only connect` の tunnels/s だけ中央値 7,402 → 6,187 と出たが、この計測は 4,156〜7,862 と倍近く動くので
    ぶれ (CPU/op と p50 はどちらも改善している)。

    実装は `crates/sys/src/sys.rs` の `inherit_socket_options`
    (`SOL_SOCKET`/`SO_RCVTIMEO`/`SO_SNDTIMEO`/`IPPROTO_TCP`/`TCP_NODELAY`、`struct timeval`、aarch64 と x86_64 のみ)。
    `serve` が待ち受けに 1 回当て、`Conn::new` は継承済みなら `set_write_timeout` / `set_nodelay` を飛ばし
    `read_timeout` を継承値から始める。**失敗したら従来どおり接続ごとに設定する**フォールバックは残してある
    (`inherit_on_listener` が `None` を返す経路。Linux 以外・aarch64/x86_64 以外・`timeout` が 0 のときもここに落ちる)。
    `SO_RCVTIMEO` は `accept()` にも効くので、accept ループは `WouldBlock`/`TimedOut` をログも sleep も無しに `continue` する。
    `.env` で `timeout` が変わったら待ち受けに当て直し、その 1 本だけは接続ごとの設定に落とす。
    カーネルの挙動を固定する単体テストを 3 本足した (継承の確認・`accept()` が `SO_RCVTIMEO` で `WouldBlock`・`0` を断る)。
    テストは 180 単体 + 42 結合 (`ab7a5d7`)

- [x] **T9.4 accept したスレッドがそのまま接続を処理する (受け渡しの `futex` をなくす)** ← 測って戻した
  - 目的: いまは待ち受けごとに 1 本の accept スレッドが接続を受け、`Box<dyn FnOnce>` にしてチャネルでワーカーへ渡す
    (`Workers::run`、`crates/workers/src/workers.rs`)。渡すたびに `futex` 2 回 (`send` が起こす + ワーカーの `recv_timeout` が待つ) と
    別コアでの起床が要る (T8.2 の内訳で 2.00 回/接続)。1 接続 1 要求の p50 が 0.33 ms と keep-alive の 0.18 ms より悪いのはここ。
  - 変更箇所: `src/lib.rs` (`serve`)、`crates/workers/src/workers.rs`、`src/main.rs` (配線)。
  - 設計 (leader / follower。測って良い方に変えてよい):
    1. 待ち受け 1 本につき「accept で待つスレッドの群れ」を持つ。各スレッドのループ: `waiting += 1` → `listener.accept()` → `waiting -= 1`。
    2. accept できたら、**自分以外に待っている人がいない (`waiting == 0`) なら**群れから 1 本起こす (居なければ上限まで spawn)。
       このときだけ `futex` が要る。上限に達していて起こせないときは、その接続を従来どおり `Workers::run` に渡して自分は accept に戻る
       (**待ち受けが空にならないことを最優先**。これで「スレッドが足りずに backlog で待たせる」ことは起きず、今の「無制限にスレッドを起こす」意味も保てる)。
    3. 503 判定、`Conn::new`、`run_conn` を**このスレッドで**実行し、終わったら (閉じた・預けた) 1. に戻る。
    4. 群れの空きスレッドは `Workers` と同じく LIFO + `recv_timeout` で待たせ、30 秒で自然に減らす。群れの上限は 64 (`MAX_IDLE` と同じ) から始めて実測で決める。
    5. accept で待つスレッドが多すぎるときに減らす仕組み: T9.3 で待ち受けに `SO_RCVTIMEO` が入っていれば `accept()` が timeout ごとに
       `WouldBlock` で戻るので、そのとき `waiting > 1` なら自分は群れの空き置き場に下がる。
    6. 監視スレッド (`IdleWatch`) からの再開は従来どおり `Workers` で良い (受け渡しは避けられない)。
    7. `Limiter` / `rejected_overload` / `Accepted { peer, local_port }` の扱いは変えない。複数の待ち受け (デュアルスタック) はそれぞれ独立した群れ。
  - 気をつけること: `run_conn` はパニックしうる (`survives_a_panicking_job` の精神)。群れのスレッドが 1 本死んでも accept が続くこと
    (少なくとも 1 本は必ず待ち受けに残る) を保証する (`catch_unwind` か、死んだら補充)。スタックは `Workers` と同じ 256 KiB。
    `/status` の空きスレッド数 (`Workers::idle_count`) を出しているなら、群れの空きも数に入れるか名前を分ける。
  - 計測: 変更前のバイナリを退避し、`scripts/cpu-per-request.sh --no-keepalive` を交互に 3 回ずつ。keep-alive・`--only connect`・
    キャッシュ HIT も退行が無いこと。`strace -f -c` で `futex` /接続 (目標 0.1 以下)。`ps -o nlwp` で暇なときのスレッド数が増えすぎないこと
    (起動直後、負荷後 30 秒)。**まず実装して測り、効かなければ戻して §4 に書く** (この計画の他の案と同じ)。
  - 受け入れ基準: `futex` が 2.00 → 0.1 回/接続 以下、1 接続 1 要求の CPU/接続 が 5% 以上下がること。
    keep-alive (預ける経路を含む)・connect・上限 8 で 9 本目が 503 になるテストが通ること。
  - 結果: **不採用。実装して測ったが戻した** (`f7eb35e` → `42889d8`)。**`futex` は消えたが速くならず、他の経路が退行した。**
    受け入れ基準のうち `futex` 2.00 → **0.02 回/接続** (目標 0.1 以下) は満たしたが、
    **1 接続 1 要求の CPU/接続 は -1.7% で目標の -5% に届かない** (前後交互 6 回ずつの中央値。ぶれの幅 113〜120 us の中)。

    | 1 接続 1 要求 (`--no-keepalive`、交互 6 回の中央値) | 前 | 後 |
    |---|---|---|
    | CPU/接続 | 114.99 us | 113.04 us (-1.7%) |
    | req/s | 13,151 | 13,461 (+2.4%) |
    | p50 | 0.321 ms | **0.319 ms (動かない)** |
    | `futex`/接続 | 2.02 | **0.02** |
    | システムコール/接続 | 11.05 | 9.04 |

    | ほかの経路 (交互 5 回 / 2 回の中央値) | 前 | 後 |
    |---|---|---|
    | keep-alive forward CPU/要求 | 48.56 us | **50.14 us (+3.3%)** (5 組すべて後が高い) |
    | CONNECT 確立 CPU/本 | 176.25 us | **187.53 us (+6.4%)** |
    | キャッシュ HIT CPU/要求 | 39.81 us | 40.26 us |
    | 忙しいときのスレッド (ka / connect) | 12 / 68 | 18 / 131 |

    **見立てが 2 つ外れていた。** 1 つ目は「1 接続 1 要求の p50 0.33 ms は受け渡しのせい」で、p50 は 0.321 → 0.319 ms と動かない。
    差の大半は接続の作り直し (`accept4` + `close` + 三方向握手 + EOF を読む `recvfrom`) で、受け渡しの `futex` 2 回は
    この機械で 1 回 1 us 程度しかない (T9.3 で `setsockopt` 3 回が -2.8% だったのと同じ勘定。2 回なら **-2% が上限**で、実測 -1.7% はその通り)。
    2 つ目は「群れは待ち受けだけを見る」で、実際は **長生きする仕事 (keep-alive の接続・CONNECT トンネル) を accept したスレッドが抱える**ため
    群れが上限 64 本まで育ち、そこから先は結局ワーカーへ渡す。群れの費用と受け渡しの費用を**両方**払うことになり、keep-alive と CONNECT の退行になった。
    暇なときのスレッドの縮小自体は効いていた (`--lite` で 起動直後 4 → 負荷直後 31 → 負荷後 30 秒 27 → 70 秒 4)。

    実装は履歴に残してある (`f7eb35e`: `crates/workers/src/flock.rs` の leader/follower、群れの上限 64、T9.3 の `SO_RCVTIMEO` で
    暇な群れが 30 秒で痩せる、上限に当たった接続は従来どおり `Workers` へ、パニックしても待ち受けが止まらない `catch_unwind`、
    待ち受けに `SO_RCVTIMEO` が載らない環境は従来経路)。テストは 185 単体 + 43 結合まで通っていた。バイナリは 1,316,336 → 1,381,880 B。
    別の条件で試すなら `42889d8` を戻せば取り出せる。
    - **T9.3 の申し送り「先に `Connection: close` を見たら猶予に切り替えない」は取り下げ**。調べたところ、クライアントが
      `Connection: close` を送った場合は `handle_http_with_headers` が `keep = false` を返して `Step::Close` になり、
      2 回目の `serve_one` 自体が走らないので猶予の `setsockopt` は元から出ない。残る 1.00 回/接続 は
      「`Connection: close` を送らないクライアント」のぶんだけなので、この判断を入れても 1 回も減らない。
    - 1 接続 1 要求の残りは `recvfrom` 4.00 / `sendto` 2.00 / `accept4` 1.00 / `close` 1.00 / `setsockopt` 1.00 = 9.04 回/接続で、
      **ここから先はシステムコールの数では削れない**。次に効くとすれば T9.5 (ユーザー空間の内訳) か、カーネル側の 87 us そのもの。

- [x] **T9.5 ユーザー空間の 16 us/要求 の内訳を出し、上位を潰す** (T8.4 を含む)
  - 目的: keep-alive の経路はシステムコール 5 回で床に着いた (`recvfrom` 3 + `sendto` 2、うち 1 はプールの生存確認で
    §4 のとおり残す)。残りはユーザー空間 16 us (全体の 1/3) とカーネル 35 us で、ユーザー空間の内訳は
    T1.5 (128 us 時代の区間計測) 以来取っていない。当時 3.8% だった要求解析は、いまなら 1 割になっている計算。
    確保 41.1 回/要求 の内訳 (T8.4) も同じ道具で出る。T9.1 の perf (シンボル別の上位: `malloc` 6.5%、`handle_http_with_headers` 5.2%、
    `write_response_head` 3.6%、`memchr` 2.8%、`__aarch64_cas4_acq` 2.8%、`cfree` 2.4%、`clock_gettime` 2.4%) が出発点になる。
  - 変更箇所: 内訳しだい (`crates/msg`・`crates/http`・`crates/origin`・`src/lib.rs` のどれか)。
  - やること:
    1. `CARGO_PROFILE_RELEASE_STRIP=none CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --release` でシンボルを残す (終わったら普通に作り直す)。
    2. `scripts/cpu-per-request.sh` と同じ起動 (taskset 4-7、`--lite`) をし、forward 8 並列 15 秒の最中に
       `perf record -e cpu-clock -F 4999 -g -p <pid> -- sleep 10` (この環境は `perf_event_paranoid = 2` でユーザー空間のみ。
       既定の `cycles` はサンプルが取れない)。`perf report --no-children --percent-limit 1 --stdio --sort symbol` で上位を表にし、
       % × user CPU/要求 で us に直す。
    3. 確保の内訳: `perf report --no-children -S malloc -g caller` で `malloc` の呼び出し元ツリーを出す (呼び出し元別の回数の表にする)。
       足りなければ `#[global_allocator]` に数えるアロケータを**一時的に**入れて (本体 `src/main.rs`)、要求あたりの回数とサイズの分布を出す。
       計測用のコードはコミットに入れない。
    4. 上位から、**5% 以上 (ユーザー空間の CPU で 0.8 us、または確保で 2 回/要求) 取れる見込みのものだけ**直す。1 つ直すごとに交互 3 回ずつ測る。
       直したものは 1 コミットにまとめてよいが、本文に「何をいくつ減らしたか」を項目ごとに書く。効かないものは戻し、見込みがあって効かなかったものは §4 に 1 行。
  - 見当 (先入観にしないこと。perf の数字が優先): `serve_one` の `http::parse_origin` が `host_port: String` を作る (`Cow::Owned`)。
    `metrics.record_host` / `record_client` の `format!` と `to_string` (アクセスログ off でも走る部分)。`request_head` / `build_request_head` の `Vec<u8>` を
    要求ごとに確保していないか。オリジン応答ヘッダーの `Vec<Header>` と `String` (`crates/msg/src/headers.rs`、`response.rs`)。
    `pool.get(host)` の `HashMap<String, ..>` 引きで鍵の `String` を作っていないか。`http::Shared` を要求ごとに組む `Arc::clone` の連打。
    `read_line` の `String` 経由 (UTF-8 検証)。`__kernel_clock_gettime` 2.4% = 要求あたり `Instant::now()` を何回呼んでいるか。
  - 受け入れ基準: 関数と確保の内訳の表が TASKS に残り、ユーザー空間の CPU/要求 が 5% 以上下がるか「これ以上は効かない」と根拠つきで記録されていること。
  - 結果: **ユーザー空間の CPU/要求 を 17.41 → 12.60 us (-27.6%)、確保を 40.0 → 10.0 回/要求 (-75%) にした。**
    forward keep-alive の CPU/要求 は 48.80 → **42.74 us (-12.4%)** (user 15.56 → 11.93 で **-23.3%**)。
    内訳は `CARGO_PROFILE_RELEASE_STRIP=none CARGO_PROFILE_RELEASE_DEBUG=1` のビルドに
    `perf record -e cpu-clock -F 4999 -g` を当て、`% × user CPU/要求` で us に直したもの。確保は
    `#[global_allocator]` に数えるアロケータを一時的に入れ (`std::backtrace` で呼び出し元別に集計)、
    呼び出し元と大きさの分布まで出した (**計測用のコードはコミットに入れていない**)。

    | 経路 (前後交互 3 回ずつ 10 秒の中央値) | 前 | 後 | |
    |---|---|---|---|
    | forward keep-alive | 48.80 us/要求 (user 15.56 / kernel 33.24) | **42.74** (user **11.93** / 30.64) | **-12.4%** |
    | 1 接続 1 要求 | 112.99 us/接続 (user 27.24) | **107.66** (user **23.31**) | -4.7% |
    | キャッシュ HIT | 40.11 us/要求 (user 17.20) | **38.66** (user **15.94**) | -3.6% |
    | CONNECT 確立 | 168.85 us/本 | 170.25 | +0.8% (ぶれの中。この経路は通らない) |
    | トンネル 1 本 | 312.50 us/MiB | 312.50 | 変わらず |
    | 確保/要求 | 40.04 | **10.04** | **-75%** |
    | システムコール/要求 | 5.03 | 5.02 | 変わらず |

    **ユーザー空間の内訳 (us/要求)**:

    | 何に使っているか | 前 | 後 | 差 |
    |---|---|---|---|
    | 自分のコードと std | 9.92 | 7.92 | -1.99 |
    | アロケータ (malloc/free/realloc) | 3.55 | 1.28 | **-2.27** |
    | システムコールの入り口 (libc) | 1.39 | 1.10 | -0.29 |
    | 原子操作 (Mutex / Arc) | 1.00 | 1.08 | +0.08 |
    | 時刻 (`clock_gettime`) | 0.58 | 0.43 | -0.15 |
    | `memchr` / `memcmp` | 0.56 | 0.36 | -0.20 |
    | `memcpy` | 0.40 | 0.42 | +0.02 |
    | **user CPU/要求** | **17.41** | **12.60** | **-4.81** |

    区間別では `write_response_head` -1.32、`handle_http_with_headers` の残り -2.40、`serve_one` の残り -0.73、
    要求ヘッダーの組み立て -0.40、ACL/ブロックリスト -0.36。応答ヘッダーの読み取りだけ +0.77 で、これは
    組 (`Vec<(String,String)>`) を作らずに生の先頭を 2 回なめる形にしたぶん (確保はそのぶん消えている)。

    **確保 40.0 回/要求 の内訳 (T8.4)** と、何をいくつ減らしたか:

    | 直したもの | 減った確保 |
    |---|---|
    | 応答ヘッダーの `Vec<(String,String)>` を作らない (生の先頭を直接見る) | 7 |
    | 保存しないときは `cached_head` を組み立てない (`write_response_head` の 2 回目) | 5 |
    | 接続元 IP の文字列を接続ごとに 1 回にする (統計と X-Forwarded-For) | 4 |
    | `parse_request_headers` の `pairs: Vec<(String,String)>` を捨てる | 3 |
    | 足すヘッダー行の `Vec<String>` + `format!` + `connection_line` の `String` | 3 |
    | ACL 判定のための `parse_origin` を借用の `target_host` にする | 2 |
    | `status.to_string()` と `cache_state: String` | 2 |
    | 要求行の `Vec<&str>` | 1 |
    | `write_response_head` の `Vec<&str>` | 1 |
    | ブロックリストの `to_ascii_lowercase` (一覧が空なら小文字化しない) | 1 |
    | `Origin::locate` の `port.to_string()` | 1 |
    | **合計** | **30** |

    残る 10.0 回は「本当に要る置き場」で、`read_line` の行バッファ 2、`parse_origin` 2、応答の先頭 1 (1,024 B)、
    要求の先頭 1 (279 B)、クライアントへの先頭 1 (188 B)、正規化 URL 1 (29 B)、Host の値 1 (15 B)、
    書き込みバッファ 1 (**65,536 B**)、プールへ戻すとき 0.2。

    そのほか、`Ctx::log` の `elapsed()` 2 回を 1 回に、ブロックリストの上書き参照が要求ごとに引いていた
    壁時計 (`SystemTime::now`) を空リストなら引かないようにした。ヘッダー行の切り分けと空白の除去は
    **ASCII だけを見る形**にした (`str::split_once(char)` の `CharSearcher` と `str::trim` の Unicode 判定をやめる。
    `crates/base/src/ascii.rs` を新設し、標準の `split_once` / `rsplit_once` と同じ結果になることをテストで固定)。

    **「これ以上は効かない」の根拠**: 3 段に分けて 1 段ごとに交互 3 回ずつ測った。

    | 段 | 中身 | CPU/要求 | user | 確保/要求 |
    |---|---|---|---|---|
    | 1 | 使わないものを作らない | 50.12 → 43.99 (-12.2%) | 16.04 → 12.63 (-21.3%) | 40.04 → 18.04 |
    | 2 | ASCII だけを見る切り分け + ブロックリストの早い抜け | 43.73 → 43.02 (-1.6%、ぶれの中) | 12.50 → 11.51 (**-7.9%**) | 18.04 → 17.04 |
    | 3 | 応答ヘッダーの組を作らない | 42.51 → 42.58 (+0.2%) | 11.69 → 11.68 (**-0.1%**) | 17.04 → **10.04** |

    段 3 は **確保を 7 回/要求 減らしても CPU が 1 ミリも動かなかった**。減らしたのは 7〜13 バイトの確保で、
    glibc の tcache から出て戻るだけなので 1 回 20 ns 程度しかない。残る 10 回のうち 9 回も同じ大きさなので、
    **確保の回数はもう効く指標ではない** (T4.4 の「14 回減らしても 0.6 us」を内訳つきで裏づけた形)。
    唯一の例外が 64 KiB の書き込みバッファで、これはプールすると効くが 5% には届かなかった (§4)。
    テスト 225 → **231 本**。`cargo build --release` は 200 MB の cgroup で通り、`dist` のバイナリは 988,648 B で変わらない
    (`release` だけ 1,316,336 → 1,381,872 B で 64 KiB の境界を 1 つまたぐ) (`93fe8b4`)
    - 次に効きそうなのは **原子操作 1.08 us** (要求ごとに Mutex 6 回: 統計 2・プール 2・中継バッファ 2)、
      `write_response_head` 1.06 us、`endpoints::handle` 0.24 us の順。いずれも 5% には届かない見込み。
    - `Pool::get` は行が空になると `HashMap` からエントリを消すので、`put` が 0.2 回/要求 の割合で鍵の `String` を
      作り直している (残せば消せるが 0.2 回/要求 なので手を付けていない)。

- [x] **T9.6 同時接続数の数え漏れ (`Conn::new` が失敗すると `open` が戻らない)**
  - 目的: T9.4 の作業中に見つかった**既存の不具合**。`src/lib.rs` の `serve` は accept 直後に
    `limiter.open.fetch_add(1)` してからワーカーへ渡し、持ち分の返却は `Conn` の `_open: OpenGuard` の `Drop` に任せている。
    ところが `Conn::new` は `OpenGuard` を作る**前**に `client.set_write_timeout(Some(config.timeout))?` を通るので、
    ここで `Err` になると `open` が 1 増えたまま誰も戻さない。積み重なると `PROXY_MAX_CONNS` に達して**恒久的に 503** になる。
  - 変更箇所: `src/lib.rs` (`serve` / `Conn::new` / `OpenGuard`)、`crates/workers/src/workers.rs` (`run` が失敗したとき仕事を落とすかどうかの確認)。
  - 今の経路: `serve` が `fetch_add` → `workers.run(Box::new(move || { Conn::new(...) }))` → `Conn::new` の `?` で早期 return →
    `log_error!` だけして終わり。`started.is_err()` のときだけ `serve` 側で `fetch_sub` している。
    **Linux で継承 (T9.3) が効いていれば `set_write_timeout` を呼ばないので踏まない**。踏むのは継承が当たらない環境
    (Linux 以外、`inherit_socket_options` が失敗した場合) だけ。
  - やること: 持ち分を**数える場所と返す場所を 1 つにする**。`OpenGuard` を `serve` 側 (accept したところ) で作って
    `Conn::new` に渡し、`Conn` はそれを持つだけにする。こうすれば `Conn::new` の途中で失敗しても、`workers.run` が失敗して
    仕事が落とされても、`Drop` が必ず 1 回だけ戻す (`serve` の手動の `fetch_sub` も消える)。
    **ホットパスの原子操作を増やさないこと** (今と同じく接続あたり 1 増 1 減)。
    `workers.run` が失敗したときに `Box<dyn FnOnce>` を捨てるのか呼び出し側へ返すのかを先に確かめる (返すなら受け取って落とす)。
  - 受け入れ基準: `Conn::new` を失敗させたときに `open` が戻ることを見る単体テスト (継承を無効にし、閉じたソケットなどで
    `set_write_timeout` を失敗させる。作れなければ `OpenGuard` を渡す形になったことを型で示すテストでよい)。
    `test_integration_connection_limit_returns_503` が通ること。性能は変わらないはず (原子操作の数が同じ) だが、
    keep-alive を 1 回だけ測って退行がないことを確かめる。
  - 結果: **数えるのも返すのも `OpenGuard` だけにした。** `serve` が `OpenGuard::acquire` で取って仕事へ運び、
    `Conn::new` は `Arc<Limiter>` ではなく **`OpenGuard` を受け取る**。これで (1) `Conn::new` の途中で失敗しても、
    (2) `Workers::run` が仕事を渡せず呼び出し元へ返して落ちても、`Drop` が必ず 1 回だけ返す。
    `serve` の手動の `fetch_sub` は消えた。原子操作の数は接続あたり 1 増 1 減で変わらない。

    **想定より届きやすい経路だった。** 着手前は「Linux で継承 (T9.3) が効いていれば `set_write_timeout` を
    呼ばないので実質踏まない」と書いたが、**`PROXY_TIMEOUT_SECS=0` は `Duration::ZERO` が clamp されずに渡り**、
    T9.3 の `inherit_socket_options` は timeout=0 を断るので `set_write_timeout(Some(ZERO))` が**必ず失敗する**。
    つまり設定ひとつで全接続が持ち分を漏らし、`PROXY_MAX_CONNS` 本ぶん漏れた時点で恒久的に 503 になる。
    回帰テストはこの経路をそのまま通す。

    | 回帰テスト | 何を見るか |
    |---|---|
    | `open_slot_comes_back_when_conn_setup_fails` | timeout 0 で**実際に `Conn::new` を失敗させ**、`open` が 1 → 0 に戻る |
    | `open_slot_comes_back_when_the_job_is_dropped` | 渡せなかった仕事 (`Workers::run` が返す `Box<dyn FnOnce>`) が呼ばれずに落ちても戻る |

    keep-alive の CPU/要求 (前後交互 2 回): 前 42.01 / 41.67 us、後 41.58 / 42.82 us。
    **原子操作の数が同じなので想定どおり差は無い** (ぶれの中)。テスト 235 → **237 本**、
    `cargo fmt` / `clippy -D warnings` / `test --workspace` / `build --release` すべて通過 (`38b8319`)
    - 型で守る形になったので、`serve` 側で数えて `Conn` 側で返す形に戻すと**コンパイルが通らない**
      (`Conn::new` が `OpenGuard` を要求する)。
    - `PROXY_TIMEOUT_SECS=0` が「全接続が失敗する」設定になっていること自体は別の問題として残っている
      (`PROXY_TUNNEL_IDLE_SECS=0` は「無期限」なので、同じ 0 でも意味が違う)。持ち分は漏れなくなったので急がないが、
      直すなら「0 は無期限」か「1 秒未満は 1 秒に切り上げ」のどちらかに寄せる話。

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
