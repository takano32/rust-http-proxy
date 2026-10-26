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


**28d 台帳・文書・道具**

- [ ] **T.28.27 2,000 行を超える 3 本を割る**
  - 目的: `crates/metrics-core/src/metrics.rs` 3,149 (T.24.8 の時点の 2,834 から増えた)・`crates/metrics-window/src/history.rs` 2,375・`crates/server/src/lib.rs` 2,066。目安は 1 ファイル 2,000 行 (TODO.md 93)。T.24.8 は `recent.rs` だけを見た。
  - やること: T.24.8 / T.25.2 の型 (段 1 で割り方、段 2 で中身を変えずに割る 1 コミット)。T.28.0・T.28.15 と同じ所を触るので、その後に回す。
  - 受け入れ基準: 3 本とも 2,000 行以下、`cargo test --workspace` の本数が前後で同じ。




- [ ] **T.28.37 揺れる試験 2 本を直す (T.28.9・T.28.20 で見つけた)**
  - 目的: `proxy-selfbench` の `cpu_of_the_process_and_of_this_thread_is_readable` はプロセスの CPU を先に、スレッドの CPU を後に読むので `111 < 113` で落ちることがある。`expect_continue_test::test_integration_expect_100_continue_gets_100_before_the_body` は全体を回して負荷が高いときに 1 回落ちた (エコーするオリジンが本文の途中で返す)。
  - やること: 読む順を入れ替えるか比べ方を直す。オリジンが本文を読み切ってから返すようにする。判定は弱めない。
  - 受け入れ基準: 2 本を 4 並列の負荷の下で 20 回ずつ回して 0 回落ちる。

- [ ] **T.28.39 不正な要求行の WARN に載せる要求行を短く切る (T.28.9 で見つけた)**
  - 目的: `malformed request line: {:?}` は要求行を最大 64 KiB までそのまま出す。T.28.9 で 1 分 1 行になったが、1 行の長さは攻撃者が決められる。
  - やること: ログに載せるのは先頭 N バイト (例 256) と全体の長さにする (ほかの WARN で要求の一部を載せる所も同じ)。
  - 受け入れ基準: 60 KiB の壊れた要求行で WARN 1 行が 512 B 以下、長さが分かる。

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
| `/slo` の `dns_miss_per_connect` の既定か定義 (T.28.16 段 1) | 確立を「接続ごとの (確立 − RTT)」+ error_rate を名前解決に成功した試行あたりにしても 10-25 は 0.813 で 0.9 に届かない。届かない分は `dns_miss_per_connect` が 96 / 573 標本で外れるため (外すと 0.953) | 既定を緩める / 定義を変える (例: 名前解決の失敗を除く) / 今のまま (`/slo` は目安として読む) |
| 日次の `/snapshot` の `/recent` を多く持たせる (T.28.18 段 1) | 日次は部ごとに 256 KiB で切り、`/recent` は約 580 行 (1,000 本を割る)。締めで日次を「前」にすると接続のエラーが「判定できず」になる | 1 MiB (約 2,300 行。日次 2.9 → 約 3.6 MB) / 今のまま (締めは collect の雪像を「前」にする) |

**見直したが元の判断のままでよいもの** (2026-10-25、理由と数字は見直しの調査): 繋がらない宛先を覚える (T.22.3。今 1.18 分/日)、負のキャッシュの延長 (T.27.3。今 0.50 分/日、総当たりは毎回違う名前)、keep-warm 停止 (T.22.4。`stale_retries` 0)、serve-stale 21,600 秒 (T.21.3)、T.24.1 段 2 (構造上 0%)、warm の枠・窓の格子 (T.17.5・T.20・T.22.4。keep-warm 0 で前提が消えた)、IPv6 だけの宛先をすぐ断る (T.18.99)、先張り (T.14.1。今の数字で約 1.5 分/日)、一律 TTL・名前ごとの TTL (T.15.4)、§4 の loopback の micro-opt 一式と システムコールの床 4 回 (T.27.4。今も 4.03 回/要求)、LTO を `release` で (120 MB の関門の中では無理。見直すなら配布物の側)、musl、HTTP/2、ws の `Upgrade`、`/connections` の `offset=` (240 本でも 54 KB)、`PROXY_MAX_CONNS` auto 240 (山 49)、T.27.5〜T.27.8 の 400 / 505 (23.7 時間で断った要求 0 件)。

**調べたが問題が無かったもの** (重ねて調べないため): キャッシュの基本 (HIT/MISS・`Age`・`Vary: Accept-Encoding` の正規化・`Vary: *`/`Cookie`・`Set-Cookie`/`private`/`no-store` を保存しない・`Authorization` 付きは参照も保存もしない・`no-cache`+ETag・`s-maxage`・`Expires: 0`・HEAD・Range/206/416・`If-Range`・条件付きの 304・unsafe メソッドの無効化・404 の短期保存・stale-while-revalidate)、応答の枠組み (chunked+CL・`chunked,gzip`・HEAD/204/304 の本文・close 区切りの再 chunk・HTTP/1.0 オリジン)、パイプライン、`Connection: close` と HTTP/1.0 keep-alive、CONNECT の大文字のホスト、`PROXY_ENDPOINTS_READONLY`、内部エンドポイントのクエリの壊れた値 17 種、SNI の読み手、chunked の読み手、rrd の CRC と版の変換、ワーカーのパニックの後始末、メモリの上限 (ヘッダーの置き場・中継のプール)、`.env` の読み手、メモリ漏れ (日次 16 枚で 17.4〜20.8 MB)、CPU の絞り (この起動 0)、キャッシュがほぼ当たらない (通信の大半が CONNECT = 使われ方)、`first_relay` 4.2 秒 (クライアントが TLS を始めるまで = 使われ方)、`sni_mismatch` (ほぼ全部が Microsoft の IP リテラル宛)、`/proxy.pac`、`--check` の 7 項目、起動 17〜19 ms、ビルドのメモリ (80 MiB でも通る)。
