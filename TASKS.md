# TASKS.md — 未着手のタスク

`rust-http-proxy` の**まだ着手していないタスクだけ**をここに置く (2026-10-22、T.24.13)。入り口は [`TODO.md`](TODO.md)。

## 決まり

- **済んだタスクはここに残さない。** タスクが終わったら、そのタスクの見出しごと [`tasks/TNN.md`](tasks/README.md) へ本文を 1 文字も変えずに移し、`TODO.md` の §5 には見出しと索引の 1 行だけ残す。
- **番号は使い回さない。** `T.n.m` は付けた順で、並び替えても振り直さない (コミットメッセージから引けるようにする)。**ただし、まだ一度も使っていない空きの番号 (タスク 19 がそうだった) は、新しい番号を増やさずに割り当ててよい** (2026-10-23、はかせの指示)。**サブタスクごとにファイルは作らない** — 1 つのタスクのサブタスクは、済んだら `tasks/TNN.md` の 1 本にまとめる。各タスクの `Tn.99` は「デプロイ後の様子見と修正、締めの文書」に予約する。**締めの番号は、そのデプロイに入る一番大きいタスクの番号に合わせる** (版と同じ。2026-10-24 の 0.27.8 はタスク 27 が最大なので **T.27.99**) (2026-10-23、はかせの指示)。
- **番号の書き方は `T.19.10`** (T のあとに点。`T19.10` とは書かない)。3 段目は必要なときだけ `T.19.10.1` のように足す。ファイル名は今までどおり `tasks/T19.md` ([`tasks/README.md`](tasks/README.md))。
- **1 タスク = 1 worktree = 1 コミット。** 手順の型は `TODO.md` の「タスク 15 の渡し方」と「実装を Opus に渡すときの決まり」。
- **クレートの版は「そのクレートの中身を最後に変えたタスクの番号」** `0.<タスク>.<番号>` (T.19.1 → `0.19.1`。2026-10-23、はかせの指示で T.26.3 の「デプロイの番号」を撤回して元に戻した)。タスクの中で `scripts/crate-versions.py --task T.19.1` を回すと、分かれ目から中身が変わったクレートが `0.19.1` になり、**本体と `proxy-server` はそのビルドの最大**になる。
- **デプロイまでの新しい仕事は、新しいタスクを起こさずに、いま開いている親タスクの小タスクとして足す** (2026-10-23 にはかせの指示で T.19.8・T.19.9 をそうした。2026-10-24 にも念を押された)。次のデプロイの前に見つけた仕事は、どんなに毛色が違っても `T.<親>.<次の番号>` にして同じ親タスクに入れ、そのデプロイの締め (`T.<親>.99`) までを 1 つのタスクで扱う。**新しいタスク (`タスク N+1`) を起こすのは、締め (`T.<親>.99`) が済んでから** (デプロイのあとに見つけた仕事)。番号を足したら `T.<親>.99` の前提の「版」と「入る小タスク」も書き直す。
- **タグは Claude がセッションの中から打つ。利用者の手元には回さない** (2026-10-24、はかせの指示「そっちでできるじゃん」)。まず `git push origin vX.Y.Z` を試し、通らなければ (クラウドのセッションはタグの git push も GitHub API の書き込みも通らない) **GitHub のツールで `release.yml` を `workflow_dispatch` (入力 `version` = `X.Y.Z`、`ref` = 既定の `main`) で起動して打つ** (workflow の中で本体の版と `crate-versions.py --check` を確かめ、注釈付きのタグを push してからビルドする)。打てたかは `git ls-remote origin refs/tags/vX.Y.Z` と release の実行で確かめ、締め (`T.<親>.99`) の前提に書く。できない理由があれば、手順を利用者に渡す前に、別の口 (workflow・GitHub のツール) を探してから言う。
- **デプロイは複数のタスクをまとめてよい。** そのときの版 (= タグ) は**まとめた中で一番大きいタスクの番号**。番号はタスクの番号なので、**前に出したタグより小さくなることもある** (T.19 は空いていた番号なので `v0.19.x` は `v0.23.3` より小さい)。タグはデプロイの合図の直前に打つ。
- このファイルと `tasks/` の 1 対 1 は `./scripts/check-docs.sh` の (g) が見る (`scripts/check-ledger.py`)。**未着手のタスクに `- [x]` が付いていたら NG** (移し忘れ)。

## 呼び方

**「Phase」という呼称は 2026-10-23 に廃止して、すべて「タスク」に統一した** (はかせの指示)。台帳は **`T.19.10` の形の番号** (2026-10-23 に `T19.10` から変更。はかせの指示) と `tasks/T19.md` のファイル名で引き、まとまりの単位も「タスク」と呼ぶ。

例外は**判定プロファイルの名前** `--criteria phase24` (`scripts/snapshot_criteria.py` の `RULES["phase24"]`) で、これは**過去の出力を 1 バイトも変えない**ために歴史的な綴りのままにしてある。台帳の呼称とは別物。

## いま開いているタスク

### タスク 28 (2026-10-25 に起こした: 6 方向の洗い出し (プロトコルとキャッシュ・性能・実データ・頑丈さ・機能と道具・過去の判断の見直し) で見つけた穴)

0.27.8 (T.27.99 で満たした 11) のあと、調査員 6 人 (Fable、effort max) が手元の release を叩き、tokyo の雪像 (`2026-10-25T203134Z`・日次) を読み、過去の「採らない」も考え直して挙げた候補を、値打ちの順に並べた (2026-10-25)。**証拠は実機で再現したもの**で、作業ファイルは当日のコンテナの scratchpad (`t28/`) にあった (コンテナと一緒に消えうるので、要る数字は各小タスクの本文に写した)。本体を変えるものは 28a・28b、計器と物差しは 28c、台帳と文書は 28d。**利用者の判断が要るもの**は番号を振らず、節の最後の表に置いた (決まったら番号を足す)。

**全体の見立て (実データ)**: 利用者 (生きた 443 宛て) の待ちは実質 1 RTT = 距離で、コードで削れる所はほぼ無い (T.14.99 の結論は今も有効)。待ちの尾・失敗・`dns_slow` のほとんどはメール宛の総当たり (要求の 4.2% が `/errors` の 95%)。値打ちがあるのは**頑丈さ**・**RFC の残り**・**forward の user 時間**・**計器の穴**。

**28a 頑丈さと HTTP の残り (本体を変える)**

- [ ] **T.28.0 ヘッダー受信に総時間の締め切りを入れる (沈黙・slowloris でワーカーと接続枠が尽き、`/status` も応答しなくなる)**
  - 目的: 接続を開けて何も送らない、またはヘッダーを 1 バイトずつ送るだけで、1 本につきワーカー 1 本と接続枠 1 つを握れる。既定 `PROXY_MAX_CONNS_PER_CLIENT=0` なので 1 つの接続元が 240 本全部を握れる (tokyo は接続元が NAT の 1 つなので、接続元ごとの上限は効かない)。実測 (`PROXY_MAX_CONNS=8`): 沈黙 8 本で `/status` は **25 秒待って応答なし**、代理の GET は 4.8 秒後に 503 (オリジンに 1 本も届かない)。沈黙は `PROXY_TIMEOUT_SECS` (30 秒) で初めて放される。`PROXY_TIMEOUT_SECS=3` で 1 バイト / 2 秒を送ると **12 秒 (締め切りの 4 倍) たっても切れない** (`SO_RCVTIMEO` は「無通信の締め切り」で、読めるたびに待ち直す)。T.13.2 の「上限に当たっても監視は取れる」も崩れている (上限の外の 4 枠は accept だけで、走るのは同じワーカーの待ち行列)。
  - 変更箇所: `crates/server/src/lib.rs` (`serve_one` の 1 要求目の待ち 1343-1359、`read_line_or_idle` / `read_limited_line` 558-602、上限の外の枠 `OVERFLOW_SLOTS` 111・360)、`crates/workers/src/workers.rs` (122-139)、`crates/config`、README の環境変数の表。
  - やること: accept (か 1 バイト目) からの絶対の期限を `Instant` で持ち、ヘッダーを読む輪に通して、越えたら `408 Request Timeout` で閉じる (`PROXY_HEADER_TIMEOUT_SECS`。既定は 10 秒前後を段 1 で決める。`PROXY_CONNECT_TIMEOUT_SECS` を分けた T.15.6 と同じ型)。`PROXY_TIMEOUT_SECS` (本文の転送) は変えない。上限の外で受けた接続の `/status` を確実に返す予備 (ワーカーの外の少数) も段 2 で考える。
  - 受け入れ基準: 結合テストで (a) 沈黙の接続が期限 + 1 秒以内に 408 で閉じる、(b) 1 バイト / 1 秒の trickle も同じ、(c) `PROXY_MAX_CONNS` ぶんの沈黙を張っても `/status` が 3 秒以内に 200、(d) 10 MiB の本文の転送は今までどおり通る。`--lite` でも効く。

- [ ] **T.28.1 応答の `Transfer-Encoding` が chunked で終わらないときは `Content-Length` を捨てて閉じるまでを本文にする (RFC 9112 §6.3)**
  - 目的: T.27.6 で直した要求側の裏返し。オリジンが `Transfer-Encoding: gzip` (や `xchunked`・`cheese`) と `Content-Length: 5` を一緒に返すと、CL で区切り、TE の行は黙って落とし、**オリジンの接続をプールに戻して使い回し、キャッシュにも保存**した (keep-alive で 3 要求がオリジンへの接続 1 本、3 回目は `X-Cache: HIT`)。CL の後ろに続けたバイトが次の要求の応答として読まれうる (応答の密輸・キャッシュ汚染。`pool.put` は読み残しを捨てるので、続きが put の後に届く競合のときだけ成立)。
  - 変更箇所: `crates/msg/src/body.rs` (`of_response_head_checked` 76-106、`of_response_head` 46-64、`of_headers` 113-130)、使い場所 `crates/http/src/http/mod.rs` 939・956-958。
  - やること: 応答に TE があって最後の符号化が chunked でなければ `Framing::Close` (CL は無視、接続は使い回さない、保存もしない)。chunked と CL が両方なら今までどおり chunked。
  - 受け入れ基準: 結合テストで、TE + CL の応答が閉じるまでの本文で返り、オリジンへの接続が 3 要求で 3 本、キャッシュに入らない。chunked + CL は変わらない。`scripts/framing-probe.py` に応答の 2 形。

- [ ] **T.28.2 304 で届いた新しいヘッダーで保存済みの応答を書き直す (RFC 9111 §3.2・§4.3.4)**
  - 目的: 再検証の 304 が `Cache-Control: max-age=600` と `X-Ver: 2` を返しても、内部の期限 (TTL) だけ延ばし、配る先頭は**古いまま** (`max-age=1`・`X-Ver: 1`)。下流のキャッシュとクライアントの鮮度の計算を誤らせる。
  - 変更箇所: `crates/http/src/http/mod.rs` 961-972 (304 の経路)、`crates/http/src/http/refresh.rs` 174-184 (裏の再検証)、`crates/cache/src/cache/ops.rs` 315-333 (`refresh`)、`crates/freshness/src/freshness.rs` 121。
  - やること: 304 を受けたら、保存済みの先頭の行を 304 の `Cache-Control`・`Expires`・`Date`・`ETag`・`Last-Modified`・`Vary` などで差し替えて置き直す (メモリと disk の両方)。枠組みのヘッダー (CL・TE) と hop-by-hop は差し替えない。
  - 受け入れ基準: 結合テストで、304 の後の HIT が新しい `Cache-Control` と `X-Ver: 2` を返す。本文は 1 バイトも変わらない。

- [ ] **T.28.3 `Expect: 100-continue` の要求に、本文を読む前に `100 Continue` を返す (RFC 9110 §10.1.1)**
  - 目的: いまは 100 を自分でも返さず、オリジンの 100 も中継しない (`crates/msg/src/response.rs` 42-50) ので、100 を厳密に待つクライアントは締め切りまで本文を送らず、こちらは本文を待って止まる。
  - 変更箇所: `crates/http/src/http/mod.rs` (`forward_request_body` の前)。
  - やること: `Expect: 100-continue` (大小無視) があり本文がある要求では、本文を読む前に `HTTP/1.1 100 Continue\r\n\r\n` を 1 回書く。`Expect` の行はオリジンへ転送しない (オリジンの 100 は今までどおり読み飛ばす)。100-continue 以外の `Expect` は 417。
  - 受け入れ基準: 結合テストで、ヘッダーだけ送った時点で `100 Continue` が届き、本文を送ると 200。`Expect` の無い要求は 1 バイトも変わらない。

- [ ] **T.28.4 失敗した CONNECT を WARN 1 行だけにする (今は WARN と ERROR の 2 行)**
  - 目的: tokyo の `/log` 1,000 行は error 500・warn 500 で、**500 接続のうち 499 が同じ失敗の 2 行** (`warn: 502 Bad Gateway: connect imap.…:993 failed: …` と、宛先の無い `error: failed to lookup address information …`)。環の実際の窓が約 8.7 時間に半減し、ERROR で警報を組むと総当たりで毎時数十回鳴る。forward の失敗は WARN 1 行。
  - 変更箇所: `crates/tunnel/src/tunnel.rs` 65-103 (502 を書いて WARN を出して `Err`)、`crates/server/src/lib.rs` 1284-1290 (`pump` の呼び元が EOF 系以外を `log_error!` で出し直す)。
  - やること: tunnel が 502 を書いたあとは「報告済み」と分かる戻り値にし、呼び元は出し直さない。
  - 受け入れ基準: 結合テストで、繋がらない宛先への CONNECT 1 本で `/log` に warn 1 行・error 0 行、`/errors` の個票は今までどおり。次の締めの雪像で `/log` の error が 0〜数件。

- [ ] **T.28.5 オリジンの応答の読みの締め切りを `timeout` に数え、文面も直す (今は EAGAIN で `other`)**
  - 目的: オリジンの読みはブロッキング + `SO_RCVTIMEO` (`crates/origin/src/origin.rs` 56) で、Linux では締め切りが切れると `WouldBlock` (EAGAIN = os error 11) が返る。`crates/http/src/http/mod.rs` 905 はそれを `failed to read origin response: Resource temporarily unavailable (os error 11)` と出し、`ErrCause::from_io` (`crates/metrics-types/src/metrics.rs` 398) は `TimedOut` しか見ないので **`other` に数える**。日次 10-10〜10-16 に 21 行 (いまの量は小さい)。
  - やること: `from_io` に `WouldBlock => Timeout` (終端のエラーでしか通らない)。ログは「origin response timed out after N s」に言い換える。
  - 受け入れ基準: 単体で `WouldBlock` が `Timeout` になる、結合で 2 秒の締め切りに対し 3 秒黙るオリジンが `errors_by_cause` の `timeout` に入り、ログに "timed out" が出る。

- [ ] **T.28.6 `.env` の打ち間違いと読めない値を警告し、`--check` と `/config` に出す**
  - 目的: `PROXY_TIMOUT_SECS=20` (綴り違い)・`PROXY_KEEPALIVE_SECS=abc`・`PROXY_TIMEOUT_SECS=xyz`・`PROXY_FOO_BAR=1` を書いても WARN は 0 行 (範囲外の `PROXY_HE_STAGGER_MS` だけ出る)、再読込でも `settings.error` は `null`、`--check` は「9 variables」と言いつつ読めなかった 5 行を出さない。`crates/config/src/config.rs` は `parse().ok()` で黙って既定へ落とす所が 21 か所。**README 1701-1702「解釈できない値は前の設定を維持し `settings.error` に入る」は実装と違う**。「手軽」の入口で、Pterodactyl のファイルマネージャで `.env` を書く人が気づけない。
  - 変更箇所: `crates/config/src/config.rs`、`crates/reload/src/reload.rs` 95-106、`crates/base/src/envfile.rs`、README。
  - やること: (a) `PROXY_*` / `SERVER_*` の知らない鍵を WARN 1 行 (編集距離 2 以内の候補を添える)、(b) 21 か所を「読めなければ WARN + 既定」の 1 つの関数に寄せる、(c) `--check` に「読めなかった・知らない」の行、`/config` と `/status.settings` に `invalid` / `unknown` の欄 (足すだけ)、(d) README の文を実装に合わせる。
  - 受け入れ基準: 上の 4 種で起動ログに WARN 4 行、`--check` と再読込後の `/status.settings` に名前が出る。正しい `.env` では 1 行も増えない。通る要求の経路は触らない。

- [ ] **T.28.7 内部エンドポイントに HEAD を通し、GET / HEAD 以外は 405 にする (今は 404 + 2,864 B の案内)**
  - 目的: `curl -I` で `/healthz` `/status` `/metrics` `/proxy.pac` `/dashboard` が全部 404 で、本文は案内文 2,864 B。外の生死監視 (既定は HEAD のものが多い) やロードバランサが「死んでいる」と読む。RFC 9110 §9.3.2 は HEAD に GET と同じヘッダーを求める。`/favicon.ico` も 404 で 2.8 KB (ダッシュボードを開くたび)。
  - 変更箇所: `crates/endpoints/src/endpoints/mod.rs` 290 (`is_get`)・354 以下。
  - やること: HEAD は GET として組んで本文を書かない、GET / HEAD 以外の自分宛ては `405` + `Allow: GET, HEAD` (PURGE と `PROXY_ENDPOINTS_READONLY` の 405 は今のまま)、`/favicon.ico` は 204。
  - 受け入れ基準: 結合テストで `HEAD /healthz` が 200 で GET と同じ `Content-Length`・本文 0 B、`POST /status` が 405、`check-docs.sh` の (a) は変わらない。

- [ ] **T.28.8 forward のオリジン接続にも短い接続の締め切りを当てる (今は `PROXY_TIMEOUT_SECS` の 30 秒)**
  - 目的: CONNECT の確立は T.23.3 で 5 秒にしたが、forward のオリジン接続は 30 秒のまま (`crates/http/src/http/mod.rs` 761-765)。繋がらない宛先への forward 1 本がワーカーを 30 秒握る (T.28.0 と同じ枯渇の増幅)。**見直し**: README の「forward は `PROXY_TIMEOUT_SECS` のまま」の判断。
  - やること: forward の接続の確立にも `PROXY_CONNECT_TIMEOUT_SECS` を当てる (応答の読みは 30 秒のまま)。段 1 で tokyo の forward の確立の分布を見て、落とす成功が 0 であることを確かめる。
  - 受け入れ基準: 繋がらない宛先への forward が 5 秒で 502、forward の確立の最大 (実データ) が締め切りより十分小さい。

- [ ] **T.28.9 ログの量を攻撃者が決められないようにする (400 系の WARN と自分宛ての INFO)**
  - 目的: 不正な要求は 1 本につき WARN 1 行 (実測 20 本 → 20 行) で上限が無い。正常な要求も既定 INFO で ACCESS 1 行。ダッシュボードを開いたままにすると自分宛ての INFO が約 1.2 行/秒で Pterodactyl のコンソールを埋める (`crates/endpoints/src/endpoints/mod.rs` 540)。accept のエラー (1 時間に 1 回) と上限の WARN (1 分に 1 回) は既に絞っている。
  - やること: 400 系の WARN を理由ごとに 1 分 1 回 + 件数 (数は `rejected_requests` が持つ)、自分宛ての INFO は debug へ。
  - 受け入れ基準: 不正な要求 100 本で WARN が理由ごとに 1〜2 行、件数は `/status` で読める。自分宛ての要求は info で 0 行。

- [ ] **T.28.10 キャッシュの小物 (ホスト名の大小・応答の `Via`・要求の `Cache-Control`・壊れた `max-age`)**
  - 目的: (a) `http://LOCALHOST:P/x` は `localhost` のエントリに当たらない (キーが URL そのまま、`crates/cachekey/src/key.rs` 44)。(b) 応答に `Via` を付けていない (RFC 9110 §7.6.3 は要求と応答の両方で MUST。`crates/msg/src/headers.rs` の `write_response_head`・`sanitize_response_head`)。(c) 要求の `max-age=5` (>0)・`min-fresh` で再検証せず HIT、`only-if-cached` で無いときはオリジンへ行って 404 (RFC 9111 §5.2.1.7 は 504)。(d) 応答の `max-age=abc` を既定 300 秒で新鮮扱い。
  - 変更箇所: `crates/cachekey/src/key.rs`、`crates/msg/src/headers.rs`、`crates/http/src/http/mod.rs` 630-641、`crates/freshness/src/freshness.rs` 194。
  - 受け入れ基準: 4 つそれぞれに結合テスト 1 本。(b) は HIT の応答にも付く。

- [ ] **T.28.11 `--check` の `cgroup_cpu` を読む側とそろえる (cgroup v1 で [ok] なのに値は `null`)**
  - 目的: 判定 (`crates/sysinfo/src/sysinfo/capabilities.rs` 184) は全階層で `cpu.stat` を探すので v1 にも当たるが、読む側 (`cgroup.rs` 149-151) は v2 前提。README 1556「`null` の理由の切り分けに使える」が v1 では果たせない (tokyo は v2 で今は無害)。
  - 受け入れ基準: v1 だけの `/proc/self/cgroup` の見本で `cgroup_cpu` が false になる単体テスト。

**28b 性能 (forward の user 時間と、既定プロファイルの上乗せ)**

調査の機械 (4 コア KVM、T.27.4 より遅い) の forward 8 並列は CPU/要求 53.3 us (user 10.2 / kernel 43.1)、システムコール 4.03 回/要求 (床)、確保 10 回/要求。callgrind で user 27,379 Ir/要求の **78% がオリジン応答の先頭 (92 B・3 行) の扱い**だった。

- [ ] **T.28.12 オリジンの応答の先頭を 1 回の走査で扱う (段 1 は済み: −5.6%)**
  - 目的: `write_response_head` 6,280 Ir、`response_lines` の反復子 4,046 (`of_response_head_checked` の 2 走査 + `response_names` + **trace が切れていても回る `for … in response_lines(&head) { log_trace! }`**)、`of_response_head_checked` 4,025、`read_one_response_head` + `read_head_forwarding` 5,688 (行ごとの `String`・UTF-8 検証・写し)、`response_names` 1,500。HIT でも `write_response_head` 5,051 と `body::drain` が本文なしでも 8 KiB を 2 回ゼロ埋め (16.5k Ir)。
  - 段 1 (2026-10-25、写しで試作・A/B): p1 = trace の輪を `log::enabled(Trace)` で囲む + 要求行と状態行の `split_whitespace` → `split_ascii_whitespace` + `drain` を `Framing::None` で即 0。p2 = p1 + 枠組み・CL 検査・`Connection: close` を 1 走査 (`of_response_head_checked_with_close`)、`read_one_response_head` を `read_until(b'\n')` で `Vec<u8>` へ直接、`write_response_head` を 1 走査 (行の範囲を固定配列 48 本に控え、元の行がちょうど `Name: value` なら 1 回で写す)。+142/−40 行・6 ファイル。**p1 → p2 (forward 8 並列 10 秒・3 組の中央値): CPU/要求 55.38 → 52.30 us (−5.6%)、user 10.30 → 8.75 (−15.0%)、op/s +4.6%、p50 0.220 → 0.210 ms** (3 組とも B が下)。HIT の base → p1: user −8.4%・合計 −0.5%。写しで `proxy-msg`・`proxy-http` の試験と結合 12 本が通った。
  - 変わる振る舞い 1 つ: 不正な UTF-8 を含む応答のヘッダー行が、今は 502 なのが lossy 変換 (U+FFFD) で通る (obs-text としてはこちらが RFC に近い。採るなら README に書く)。
  - 変更箇所: `crates/msg/src/headers.rs` 192-337、`crates/msg/src/body.rs` 76-104・272-275、`crates/msg/src/response.rs` 63-106、`crates/http/src/http/mod.rs` 931-958、`crates/server/src/lib.rs` と `crates/http` の要求行の `split_whitespace`。
  - 受け入れ基準: forward 8 並列の CPU/要求 −5% 以上 (前後交互 3 組)、HIT と既定プロファイルの A/B も出す、応答のバイト列は 1 バイトも変わらない (既存の byte-for-byte の試験と `framing-probe.py`)、`cargo test --workspace` 全通過、lite のシステムコール 4.03・確保 ≤10 回/要求が増えない。

- [ ] **T.28.13 既定プロファイルのキャッシュの機構が no-store の経路で乗せている user 3.6 us (段 1)**
  - 目的: 既定 vs `PROXY_CACHE_ENABLED=off` で **58.53 → 54.03 us (−7.7%、user 14.06 → 10.43)** (3 組)。`PROXY_RECORDS=off` は −2.4% で、上乗せの主役は接続元の表ではなくキャッシュ。callgrind: `response_pairs` 3,189 + `response_policy` 1,129、`cache_key_variant` 735、`accept_encoding_variant` (`Vec<String>` + join)、`admit`/`may_coalesce`/`begin_fetch`/`remember_not_stored`、確保 19 vs 10 回/要求。**見直し**: T.11.2 (保存の判定に `response_pairs` を渡さない) は −0.4% で採らなかったが、今の数字では機構全体が 7.7%。
  - やること: 段 1 で (a) `response_policy` を借用の `response_lines` で判断、(b) `accept_encoding_variant` を Accept-Encoding なし / 1 トークンで確保なし、(c) `remember_not_stored` に居る鍵は参照の前に見て入場と合流を飛ばす、をそれぞれ A/B。
  - 受け入れ基準: 既定プロファイルの forward 8 並列 CPU/要求 −3% 以上 (3 組)、キャッシュの判定と保存の結果が変わらない (freshness / cache の試験)、確保 19 → 13 回/要求以下。

- [ ] **T.28.14 測るだけ: T.19.5 の代償と `codegen-units` 1 対 4**
  - 目的: (1) T.19.5 (応答の CL 検査。32 KiB 以下の本文はヘッダーを出す前に読み切る) は「初バイトが本文の後ろになる」と書いたまま前後の数字が無い。入れたコミットは `0e576a3`、親は `058afa9`。(2) `codegen-units = 4` は「桁で悪くなっていないか」しか見ていない (Cargo.toml 93-118、A/B なし)。1 にするとバイナリ −6.4% (2,897,240 B)。ビルドのメモリは今 80 MiB でも通る (cgroup v1、関門 120 に 40 MB 以上の余裕。2026-10-25 の調査)。
  - やること: どちらも forward 8 並列 10 秒・前後交互 3 組・中央値 (±4% はぶれの中)。(2) は 1 のときの通る最小も。
  - 受け入れ基準: 表 2 つ。(2) は差が ±4% の中なら 4 のまま、外なら段 2 を起こす。

- [ ] **T.28.15 要求ヘッダーの 2 回の走査を 1 回にする (`serve_one` と `parse_request_headers`)**
  - 目的: forward は `serve_one` が host / CL / TE / Via / User-Agent を手で走査し (`crates/server/src/lib.rs` 1539-1552)、続いて `http::handle_http_with_headers` が `parse_request_headers` で全ヘッダーを走査し直す (`crates/http/src/http/mod.rs` 426 → `crates/origin/src/request.rs`)。CONNECT は `serve_one` で別に `parse_request_headers` を呼ぶ (1693)。host / CL / TE の判定が 2 か所にあり、T.27.5〜T.27.6 でもずれないよう両方に手を入れた。
  - やること: ヘッダーを読んだ後に `parse_request_headers` を 1 回だけ呼び、host / agent / via もそこから取る。
  - 受け入れ基準: forward 8 並列の CPU/要求が下がるか同じ (3 組)、`framing-probe.py` と `cargo test --workspace` 全通過。

**28c 計器と物差し**

- [ ] **T.28.16 `/slo` の閾を距離で外れない形にする (25 時間の達成率 0.15。段 1 から)**
  - 目的: `2026-10-25T203134Z-slo.json` は 25 時間で判定 11,482 標本・満たした 1,718 = **達成率 0.15**。外した内訳は `connect_p50_ms` 9,002 (78%)・`connect_p95_ms` 4,182・`error_rate` 840・`dns_miss_per_connect` 808。既定 `PROXY_SLO=connect_p50_ms=10,…` は T.14.50 の nagoya (p50 8 ms) 前提で、tokyo の確立 p50 18 ms は 1 RTT = 距離 (T.14.99 で「物理だから追わない」とした種類)。判定表は T.24.3 で「RTT + 5 ms」に直したが `/slo` とダッシュボードの「今日の SLO」は固定値のまま。`error_rate` はメール宛の NXDOMAIN を数える。`collect-deployed.sh` は毎回 `-slo.json` を撮るが、要約も判定表も読まない。`PROXY_SLO` は再起動でしか反映されない。
  - やること: 段 1 で `history["5"]` を再生する模型を作り、(1) 確立を「そのときの RTT (個票の `rtt_ms.origin`・canary) + N ms」で判定、(2) 既定の閾を tokyo 向きに変える、(3) `error_rate` を「名前解決に成功した試行あたり」(T.23.0) にする、を並べる。段 2 は採った形を本体へ (既存の鍵は足すだけ)。`collect-deployed.sh` の要約に達成率と内訳を 1 行。
  - 受け入れ基準: 10-25 の雪像で新しい判定の達成率が 0.9 以上、10-17〜10-20 の総当たりの時間帯で `error_rate` 以外が外れない。既定値を変えるなら利用者に確かめる (T.15.6 の「特殊な設定は入れたくないので既定を変える」の先例あり)。

- [ ] **T.28.17 異常検知 (`errors`・`dns_slow`) をメール宛の総当たりで鳴らさない (`/events` の環が 5 日で回る)**
  - 目的: 起動から 23.7 時間で `errors` 42・`dns_slow` 13・`cleared` 53 = 109.5 件/日。環 512 件は約 4.7 日で回る (推定) ので、T.23.2 の「`/events` は 10 日以上残る」(無言の再起動の計器の前提) が崩れる。中身は全部総当たり: 名前解決 ≥500 ms の 25 本は 25 本ともメール系のポート、443 宛ての名前解決は 2.81 時間で 8 回・最大 256 ms。`/errors` 649 件のうちメール系 615 件 (95%)。
  - やること: `errors` から `nxdomain` (客が無い名前を頼んだ = `blocked` と同じ客の都合) を外す、`dns_slow` は解決に成功したミスか `cold` 以外で判定 (`dns_temp` は残す)。T.23.2 の前提を数字で書き直す。
  - 受け入れ基準: `scripts/dns-slow-replay.py` を 10-25・tokyo 10-17・nagoya 10-08 に当て、総当たりだけの窓での発火 0、T.22.2 の「本当に遅い一続き」4 件を落とさない (総当たりかを先に確かめる)。`errors` も同じ 3 枚で件/時を前後で。

- [ ] **T.28.18 `/recent` の窓の不足を数える (段 1。2,000 件で 2.8 時間、量の多い日は 1 時間)**
  - 目的: 10-25 の個票の窓は 2.81 時間、日次 10-19 は 1.0 時間。phase25 の接続のエラーの行は前後とも試行 1,000 本以上を求める (T.27.0) ので、量が増えると判定できなくなる。1 回の GET は 256 KiB で約 860〜1,000 本で切れる (道具は `offset=` で繋ぐが、ダッシュボードや素の curl は切れた窓しか見ない)。T.22.5 の気づき「広げるなら本体の話」が宙に浮いている。
  - やること: 段 1 で日次 16 枚と 10-24〜25 の雪像から「窓が 1,000 本を割った日」を数え、環を 2,000 → 8,000 にしたときのメモリ (1 件 ≤256 B で +1.5 MB) と、状態ファイル・`/snapshot` 4 MiB の上限との兼ね合いを見積もる。判定表の行を「同じ時刻帯の窓」で比べる案 (道具だけ) と並べる。
  - 受け入れ基準: 表と、段 2 に進むかの判断。

- [ ] **T.28.19 `/hosts` の `other` 行を分位点の比べから外す (3 つの別物を混ぜた通算)**
  - 目的: `other` (表の 1,000 行から溢れたホストの畳み先、`crates/metrics-core/src/metrics.rs` 534) は生きた CONNECT の溢れ・forward の溢れ・死んだメール宛の総当たりを 1 行に混ぜ、しかも 10-08 からの通算 (版をまたぐ)。p95 10,006 ms・max 10,012 ms は T.23.3 (10 → 5 秒) より前の timeout の残り。`status-diff.py` が forward の行として p95 10 秒を並べる。
  - やること: 道具 (`snapshot-summary.py`・`status-diff.py`・`snapshot-diff.py`) で `other` を分位点の比べから外し「混在・通算」の印を付ける。本体に `folded_hosts` を足すかは段 2。
  - 受け入れ基準: 10-25 の雪像で `other` が比べの表に出ない、印が付く。過去の `--criteria` の出力は変えない。

- [ ] **T.28.20 判定の雪像 (「前」と「0 時間」) をプロキシ自身にも残させる**
  - 目的: 締めの「前」と「0 時間」は撮った機械にしか残らない (T.25.99 は「前」が無く日次で代用、T.27.99 はコンテナに置いた)。`§0` の 9 は人の手で撮る決まりで、撮り忘れは T.16 で既に起きた。いまのサーバー側は日次だけ (`crates/endpoints/src/endpoints/recent.rs` 250-253、名前は `YYYY-MM-DD` 固定 `crates/metrics-slo/src/snapshots.rs` 61-70)。
  - やること: SIGTERM で状態ファイルを書く前に `/snapshot` 1 枚を `snapshots/<時刻>-shutdown.json`、起動 60 秒 (`memory.baseline` を書く所) で `-start.json` を残し、`/snapshots` の一覧と取り寄せの口を広げて、`collect-deployed.sh --from-server` が `BEFORE`/`ZERO` に使えるようにする。保持は `PROXY_SNAPSHOT_DAYS` と同じ。サーバー側の雪像は部ごと 256 KiB で切れる (`recent` 約 590 件) ので、shutdown の 1 枚だけは `recent` を全件書くかを先に決める。
  - 受け入れ基準: 起動 → 60 秒 → SIGTERM で 2 枚が `/snapshots` に並び、取り寄せた 2 枚で `snapshot-diff.py --criteria phase25` が読める。shutdown の所要が延びすぎない (Pterodactyl の停止の猶予の中)。

**28d 台帳・文書・道具**

- [ ] **T.28.27 2,000 行を超える 3 本を割る**
  - 目的: `crates/metrics-core/src/metrics.rs` 3,149 (T.24.8 の時点の 2,834 から増えた)・`crates/metrics-window/src/history.rs` 2,375・`crates/server/src/lib.rs` 2,066。目安は 1 ファイル 2,000 行 (TODO.md 93)。T.24.8 は `recent.rs` だけを見た。
  - やること: T.24.8 / T.25.2 の型 (段 1 で割り方、段 2 で中身を変えずに割る 1 コミット)。T.28.0・T.28.15 と同じ所を触るので、その後に回す。
  - 受け入れ基準: 3 本とも 2,000 行以下、`cargo test --workspace` の本数が前後で同じ。

- [ ] **T.28.29 道具のテストの所要を縮める (unittest 652 本 178 秒の半分が 2 本)**
  - 目的: 手元の `python3 -m unittest discover -s scripts` は 178 秒 (CI では 65 秒)、うち `test_collect_deployed.py` 57 秒 (`collect-deployed.sh` を偽のサーバーで 14 回起こす) と `test_mx.py` 32 秒 (`sleep`)。
  - やること: 偽のサーバーを使い回す・`sleep` を条件待ちにする。判定は弱めない。
  - 受け入れ基準: 手元で 60 秒以下、本数は同じ。

**利用者の判断が要るもの (番号は決まってから振る)**

| 何を | 材料 (2026-10-25) | 選択肢 |
|---|---|---|
| 自分宛ての判定がポートだけ | `Host: example.com:<待ち受けポート>` や `http://任意:<ポート>/purge?all=1` がプロキシの状態を変える口に届く (`crates/endpoints/src/endpoints/mod.rs` 75-98)。tokyo は高いポートなので実害は小さいが、**ポート 80 で動かすと外のサイトの URL を踏ませるだけでキャッシュ全消去やブロックリストの改変ができる**。`PROXY_ENDPOINTS_READONLY=on` は正しく効き、すり抜けは無かった | 状態を変える口だけホスト名も見る (ループバック・自分の名前以外は転送) / 状態を変える口を既定で readonly にして opt-in / README の警告を強めるだけ |
| `PROXY_ALLOW_LOCAL` が私設アドレスを断らない | 断るのはループバック・リンクローカル・未指定だけ (`crates/net/src/acl.rs` 110-118)。私設アドレスを通すと決めた記録は台帳に無い。10-16 の日次に私設アドレス宛の CONNECT 15 本 (`192.168.x.x:27042/27043` = Frida の既定ポート、`10.10.34.35/36:443`・`:5222`)、10-15 に **`10.255.255.1:80` (プロキシ自身の NAT 網の中)**。`PROXY_DENY_HOSTS` は CIDR を書けないので、**今の設定では私設アドレスを断る手が無い** | 既定は変えずに `PROXY_ALLOW_LOCAL=private` (か `PROXY_DENY_PRIVATE=on`) で `10/8`・`172.16/12`・`192.168/16`・`100.64/10`・`fc00::/7` を断れるようにする / 既定で断る側にする (§0 の 6「既定で安全な方」。家の LAN 向けの使い方の既定が変わる) / そのまま |
| 認証なし・制限なし (T.15.10) に照らせる事実 | 接続元は全部 `10.255.0.1` (NAT) なので接続元ごとの上限・許可は tokyo では効かない (効く口は宛先とポートだけ)。メール系のポートは要求の 4.2% で `/errors` の 95%・timeout の 100%・名前解決の待ちの 99%。IP リテラル宛は 20%。User-Agent は 807 種を落とした | そのまま / `PROXY_CONNECT_PORTS` を `.env` に書く (口はある) / 私設アドレスの旗だけ |
| `PROXY_MAX_CONNS_PER_CLIENT` の既定 (0) | T.28.0 の枯渇を 1 接続元でできる。ただし tokyo は NAT の 1 つなので効かない | 0 のまま (T.28.0 で根を断つ) / 控えめな既定を設定で選べるように |
| tokyo に release のバイナリ (`dist` = fat LTO) を置く | tokyo の版 `0.27.8+4b75403` はタグの 2 つ先のコミット = コンテナで `cargo build --release` している (推測)。fat LTO の −4.5% と −27% のバイナリが乗っていない。CPU は 0.0017 コアで体感には出ない | 置き方を release の取り寄せに変える (T.28.22 の手順) / ソースから作るまま。`/status` にビルドのプロファイル名を出す (小) はどちらでも |
| CONNECT の締め切り 5 → 3 秒 | 縮む待ち 4.6〜9.0 分/日、落とす成功 0〜0.23 本/日 (T.23.3 の「1 日 1 本未満」を満たす数字になった)。ただし縮むのは全部メール系と `pravda.me:443` の総当たりの待ちで、利用者の待ちは縮まない | 5 秒のまま (調査員の推奨) / 3 秒 (`.env` 1 行、本体の変更なし) |
| `--lite` でも統計を積むのをやめる | lite の user 27.4k Ir/要求のうち `record`・`record_host_detail`・`record_client`・`Ctx::log` が 3.5k (13%)。`--lite` の説明は「no statistics」だが `/hosts` は lite でも出る | lite の `/hosts`・`/clients` を空にする (振る舞いの変更) / 今のまま |
| info のアクセスログを溜めて書く | 既定 info vs warn で +5.2 us/要求、`futex` 2.6/要求 (stdout の鍵の取り合い)。tokyo (0.14 req/s) には無関係 | 溜めて書く (順序と即時性が変わる) / 今のまま |

**見直したが元の判断のままでよいもの** (2026-10-25、理由と数字は見直しの調査): 繋がらない宛先を覚える (T.22.3。今 1.18 分/日)、負のキャッシュの延長 (T.27.3。今 0.50 分/日、総当たりは毎回違う名前)、keep-warm 停止 (T.22.4。`stale_retries` 0)、serve-stale 21,600 秒 (T.21.3)、T.24.1 段 2 (構造上 0%)、warm の枠・窓の格子 (T.17.5・T.20・T.22.4。keep-warm 0 で前提が消えた)、IPv6 だけの宛先をすぐ断る (T.18.99)、先張り (T.14.1。今の数字で約 1.5 分/日)、一律 TTL・名前ごとの TTL (T.15.4)、§4 の loopback の micro-opt 一式と システムコールの床 4 回 (T.27.4。今も 4.03 回/要求)、LTO を `release` で (120 MB の関門の中では無理。見直すなら配布物の側)、musl、HTTP/2、ws の `Upgrade`、`/connections` の `offset=` (240 本でも 54 KB)、`PROXY_MAX_CONNS` auto 240 (山 49)、T.27.5〜T.27.8 の 400 / 505 (23.7 時間で断った要求 0 件)。

**調べたが問題が無かったもの** (重ねて調べないため): キャッシュの基本 (HIT/MISS・`Age`・`Vary: Accept-Encoding` の正規化・`Vary: *`/`Cookie`・`Set-Cookie`/`private`/`no-store` を保存しない・`Authorization` 付きは参照も保存もしない・`no-cache`+ETag・`s-maxage`・`Expires: 0`・HEAD・Range/206/416・`If-Range`・条件付きの 304・unsafe メソッドの無効化・404 の短期保存・stale-while-revalidate)、応答の枠組み (chunked+CL・`chunked,gzip`・HEAD/204/304 の本文・close 区切りの再 chunk・HTTP/1.0 オリジン)、パイプライン、`Connection: close` と HTTP/1.0 keep-alive、CONNECT の大文字のホスト、`PROXY_ENDPOINTS_READONLY`、内部エンドポイントのクエリの壊れた値 17 種、SNI の読み手、chunked の読み手、rrd の CRC と版の変換、ワーカーのパニックの後始末、メモリの上限 (ヘッダーの置き場・中継のプール)、`.env` の読み手、メモリ漏れ (日次 16 枚で 17.4〜20.8 MB)、CPU の絞り (この起動 0)、キャッシュがほぼ当たらない (通信の大半が CONNECT = 使われ方)、`first_relay` 4.2 秒 (クライアントが TLS を始めるまで = 使われ方)、`sni_mismatch` (ほぼ全部が Microsoft の IP リテラル宛)、`/proxy.pac`、`--check` の 7 項目、起動 17〜19 ms、ビルドのメモリ (80 MiB でも通る)。
