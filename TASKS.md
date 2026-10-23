# TASKS.md — 未着手のタスク

`rust-http-proxy` の**まだ着手していないタスクだけ**をここに置く (2026-10-22、T24.13)。入り口は [`TODO.md`](TODO.md)。

## 決まり

- **済んだタスクはここに残さない。** Phase が終わったら、その Phase の見出しごと [`tasks/TNN.md`](tasks/README.md) へ本文を 1 文字も変えずに移し、`TODO.md` の §5 には見出しと索引の 1 行だけ残す。
- **番号は使い回さない。** `Tn.n` は付けた順で、並び替えても振り直さない (コミットメッセージから引けるようにする)。各 Phase の `Tn.99` は「デプロイ後の様子見と修正、締めの文書」に予約する。
- **1 タスク = 1 worktree = 1 コミット。** 手順の型は `TODO.md` の「Phase 15 の渡し方」と「実装を Opus に渡すときの決まり」。
- このファイルと `tasks/` の 1 対 1 は `./scripts/check-docs.sh` の (g) が見る (`scripts/check-ledger.py`)。**未着手のタスクに `- [x]` が付いていたら NG** (移し忘れ)。

## いま開いている Phase

### Phase 24 (2026-10-22 に決めた: 起動の切り分けと heap の基準、短い TTL の引き直し、物差しの手入れ、既知の小物)

済んだ 12 本 (T24.0 〜 T24.12) の本文は [`tasks/T24.md`](tasks/T24.md) にある。残りは締めの 1 本。

- [ ] **T24.99 締める (0.24.5 を 24 時間走らせたあとに見る・決める・書く)**
  - 前提 (§0 の 9 の形): デプロイ先は `tokyo.sorahost.net:60357`。**版は 0.25.4** (波 24a〜24c の 9 本 + 揺れ 4 本 + 台帳の再編 + 波 25a/25b の 4 本。タグ `v0.25.4`)。再デプロイの**直前**に「前」の雪像、**起動から 60 秒たってから** 0 時間の雪像を撮る (`memory.baseline` が 60 秒で 1 回だけ書かれるため。すぐ撮ると heap が「判定できず」になる)。版と起動時刻 (`/events` の `start`)、`/status` の `boot.id` をここに書く。
  - リリース: タグは **`v0.25.4`** (打って push 済み。`v0.23.3` と `v0.24.5` も打った)。
  - 判定: `--criteria phase24`。見ること: こちらが原因の無言の再起動が 0 回 (外からの再起動は `boot_id` の変化で切り分けて数から外す)、heap の増え < 5 MB が**起動ごとの基準**で読めること、引き直しが前の 0.5 倍以下 (T24.1 の段 2 を入れた場合) かつ `stale_retries` < `stale_hits` の 1%、`timeout` が 1 件 5 秒、`dns_slow` が合計 1,000 ms 未満の窓で 0 件。
  - 次のターンの準備: Phase 25 の完了の定義の下書き (候補: T24.8 の割り方を実行するか、`/errors` の `dns` 1,074 件の内訳 (NXDOMAIN と一時的な失敗)、`cache` が 0 ヒットのままなことの確認)、計器の確認、`T25.0`。

### Phase 25 (2026-10-23 に起こした候補。デプロイを待たずに段 1 だけ進められるものもある)

番号は仮。**Phase 25 の見出しを決めるのは T24.99 の「次のターンの準備」**で、そこで採るかどうかを決める。

### Phase 26 (2026-10-23 に起こした: 「最速」に戻る。システムコール、監視の穴、関門の余裕)

Phase 25 で計器と台帳を片付けたので、次は**性能と監視の実利**に戻る。数字は 0.25.4 の実測 (forward 8 並列 20,652〜20,816 op/s、CPU/要求 40.4 us、1 要求あたりのシステムコール 5.02 = `recvfrom` 3 + `sendto` 2)。

- [ ] **T26.0 forward の 1 要求あたりのシステムコールを 1 回減らせるか (段 1 → 段 2)**
  - 目的: 「認証なし・手軽・**最速**」が目標なのに、forward 1 要求で `recvfrom` を 3 回呼んでいる (README の 5.02)。3 回のうち 1 回は要求ヘッダーを読み切る前の空振りか、本文の読み残しかを**まず数える**。減らせれば CPU/要求 40.4 us のうちカーネル時間 (30.2 us) がそのぶん下がる。
  - 変更箇所: 段 1 は計測だけ (`strace -c -f` と `scripts/cpu-per-request.sh`)。段 2 は `crates/http` / `crates/net-conn` の読み取りの形。
  - やること: 段 1 で `strace -f -e trace=recvfrom,sendto,read,write -c` を forward 8 並列 5 秒で取り、**1 要求あたりの回数とバイト数の分布**を出す (3 回のうち何回が 0 バイトか、ヘッダーの 1 回で本文まで読めているか)。段 2 は「最初の `recvfrom` を大きめのバッファで 1 回にし、ヘッダーと本文をまとめて読む」等を試し、**CPU/要求と p50 を前後交互 3 組**で比べる。
  - 受け入れ基準: 段 1 は内訳の表 (0 バイトの回数・平均バイト数・回数の分布)。段 2 は CPU/要求 −5% (38.4 us 以下) か、下がらないなら「なぜ下がらないか」の数字。**どちらでも、`cargo test --workspace` と bench の数字を出す**。

- [ ] **T26.1 `/metrics` に dns の内訳 (`nxdomain` / `dns_temp`) を出す**
  - 目的: T25.0 で `/status` の `errors_by_cause` は 10 要素になり、T25.3 で `/metrics` に `boot` と heap を足した。しかし **`/metrics` の接続エラーの系列は `ERR_CAUSES` (8) を回しているだけ** (`crates/prom/src/prom.rs:654`) なので、`nxdomain` と `dns_temp` が監視から見えない。実データでは `dns` 447 件のうち `dns_temp` は 8 件だが、**これが増えたときに気づける計器が無い**。
  - 変更箇所: `crates/prom/src/prom.rs`、README の `/metrics` の節、`crates/prom` の試験。
  - やること: 既存の `sorahost_connect_errors_total{cause="…"}` の**後ろに 2 つのラベル値を足す** (系列の名前・HELP・既存ラベルの並びは変えない)。`/status` の 9・10 番目と同じ値になることを実測で確かめる。
  - 受け入れ基準: `/metrics` の出力に `cause="nxdomain"` と `cause="dns_temp"` が出る (0 のときは出さない、既存と同じ約束)。既存の系列の行が 1 行も変わらないこと (`diff` で確認)。`cargo test -p proxy-prom`。

- [ ] **T26.2 CI の `build-memory-find` を回して関門 (120 MB) の余裕を数字で持つ**
  - 目的: 120 MB の関門は毎回通しているが、**余裕がどれだけあるかは測っていない** (`.github/workflows/ci.yml` の `build-memory-find` は `if:` で普段 skip)。T24.8/T25.2 で `metrics-recent` の寄与が約 11 MB と分かったので、関門までの余裕を CI で定期的に数字として残したい。
  - 変更箇所: `.github/workflows/ci.yml` (skip の条件)、README の CI の節。
  - やること: `build-memory-find` を **週 1 回 (cron) か、`workflow_dispatch` で回せる**ようにし、結果 (通った最小の上限) を step summary に出す。毎 push では回さない (runner を長く占有するため)。
  - 受け入れ基準: 手で `gh workflow run` して job が走り、step summary に「通った最小 = N MB」が出る。普段の push では skip のまま。

