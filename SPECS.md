# SPECS.md — 仕様

`rust-http-proxy` が**どう振る舞うか**の決まりを、仕様 `S1`・`S2`… として番号で並べる (2026-11-13、利用者の指示)。
やること・やったことは [`TASKS.md`](TASKS.md) のタスク、使い方の説明は [`README.md`](README.md) と `docs/` の文書に置き、
振る舞いの決まりはここに置く。文書とタスクは、決まりを書き写さずに `Sn` を指す。

## 決まり

- **番号は `S1` から、付けた順に振る。使い回さない。並び替えても振り直さない** (タスクの番号と同じ)。
- **1 つの仕様は 1 つのまとまった振る舞い** (例: CONNECT のトンネル、キャッシュの鮮度、環境変数の読み方)。
- **仕様は生きている文書**。振る舞いを変えたタスクは、同じコミットで仕様を書き直し、どのタスクで変えたかを仕様の中に書く (`T36.8 で足した` のように)。済んだタスクの記録 (`tasks/TNN.md`) と違い、書き直してよい。
- **仕様はいまの振る舞いの決まりだけを現在形で書く** (T38.4)。前の版の振る舞い・変えた理由・そのとき測った数字は、決めたタスクの番号で指すだけにして、運用や読み方に要るものは [`specs/HISTORY.md`](specs/HISTORY.md) の同じ仕様の見出しの下に置く。
- **大きくなった仕様は `specs/Sn.md` に分ける**。分けたら、この文書には番号・題・1 行の要約と `specs/Sn.md` への参照だけを残す。
- **指すときは `Sn`・タスク `Tn.m`・文書の名前だけを使う**。`§`・`(a)` のような記号や章の番号は使わない ([`AGENTS.md`](AGENTS.md) の「指し方」)。

## 仕様の一覧

S1〜S23 は T37.4 で、S24・S25 は T37.5 で起こし、T38.4 でいまの振る舞いの決まりだけを現在形で書いた文にした。80 行を越える仕様は `specs/Sn.md` に本文を置き、ここには題と 1 行の要約だけを置く。
決まりを入れた・変えたタスクは仕様の中に番号で書き、そのときの経緯 (前の版の振る舞い・理由・測った数字) は [`specs/HISTORY.md`](specs/HISTORY.md) に仕様ごとに置く。

| 番号 | 題 | 本文 |
|---|---|---|
| S1 | 受け付ける要求と断るもの | この文書 |
| S2 | 接続・keep-alive・接続プール・IPv4 / IPv6・タイムアウト | この文書 |
| S3 | スレッドと暇な接続の預かり | この文書 |
| S4 | あて先と接続元の許可・拒否、ブロックリスト、`/proxy.pac`、外から使える口の守り | この文書 |
| S5 | キャッシュ | [`specs/S5.md`](specs/S5.md) |
| S6 | 名前解決 | この文書 |
| S7 | 環境変数と `.env` の読み直し | [`specs/S7.md`](specs/S7.md) |
| S8 | コマンドライン引数と `--lite` | [`specs/S8.md`](specs/S8.md) |
| S9 | ログ | この文書 |
| S10 | 状態ファイルと再起動をまたぐもの | この文書 |
| S11 | 内部エンドポイントの共通の形、`/status`、`/metrics` | [`specs/S11.md`](specs/S11.md) |
| S12 | 画面 (`/dashboard`・`/inspect`・`/probe.html`) | [`specs/S12.md`](specs/S12.md) |
| S13 | `/healthz` と `capabilities` | この文書 |
| S14 | 個票 | [`specs/S14.md`](specs/S14.md) |
| S15 | 出来事と異常の自動検知 (`/events`) | この文書 |
| S16 | `/snapshot` と日次のスナップショット | この文書 |
| S17 | canary | この文書 |
| S18 | カーネルと cgroup の窓 | この文書 |
| S19 | ホスト別の時系列と 1 相手の説明 (`/hosts/series`・`/explain`) | この文書 |
| S20 | 日次の要約と SLO (`/daily`・`/slo`) | [`specs/S20.md`](specs/S20.md) |
| S21 | `/history` | [`specs/S21.md`](specs/S21.md) |
| S22 | `/profile` | この文書 |
| S23 | 配布物とビルドの設定 | この文書 |
| S24 | 完了の定義 (速さの約束) | この文書 |
| S25 | 守ること (認証なし・外部クレートなし・ビルドのメモリ・動作環境) | この文書 |

## S1 受け付ける要求と断るもの

認証なしで受ける HTTP / CONNECT の要求、RFC に沿った枠組みの検査、自分宛ての判定と `Via` によるループの検知、読めない要求を断る理由。

経緯は [`specs/HISTORY.md`](specs/HISTORY.md) の「S1 受け付ける要求と断るもの」。

- **認証は無い**: 事前の設定なしで、届いた人は誰でも使える (S25)。
- **外部クレートに依存しない** (S25)。バイナリは小さく、配布物は `dist` プロファイル (fat LTO) で作る (S23。測った大きさは `specs/HISTORY.md`)。
- HTTP の要求と HTTPS (CONNECT トンネル) を中継する。
- RFC 7230 / RFC 9110 に沿う:
  - Hop-by-hop ヘッダーを除く
  - `Via` と `X-Forwarded-For` を付け、受けたものは伝える。`Via` の印は
    `1.1 rust-http-proxy/<起動ごとの 8 桁 16 進>` で、**自分の印が付いた要求を受けたら `508 Loop Detected`** で閉じる
    (印が起動ごとなので、rust-http-proxy を 2 段に並べた正当な構成は誤検出しない)。
    **クライアントへ返す応答にも同じ印の `Via` を 1 行足す** (RFC 9110 §7.6.3 は要求と応答の両方で求める。T28.10)。
    オリジンの `Via` の行は残し、その後ろに別の行として足す。キャッシュから返す応答 (HIT・REVALIDATED・STALE・304) にも付け、
    保存する先頭には入れない (配るたびに足すので、再起動で印が変わっても古い印は残らない)。1xx の中間応答と、
    プロキシ自身が作るエラー応答・内部エンドポイントには付けない
  - `Via` の受けた版 (RFC 9110 §7.6.3) は常に `1.1` (HTTP/1.0 のクライアントは少なく、HIT の応答は
    オリジンの版を持っていないので、版を写すと要求と応答で食い違うため)

### 自分宛ての判定

**自分宛てかどうかはポートだけで決める**: 絶対形式 (`GET http://host:PORT/status`) は URL の、
**オリジン形式 (`GET /status` + `Host:`) は `Host` のポート (無ければ 80) が自分の待ち受けポートと同じときだけ**
自分宛て。自分宛てで知らないパスは 404、`/` は 200 でエンドポイントの一覧を返す
(ブラウザでプロキシの URL を直接開いた人への案内。`--lite` でも出すが、持っていない `/dashboard` は載せない)。
**自分宛ての `HEAD` は `GET` と同じヘッダー (`Content-Length` も GET の本文の長さ) で本文だけ返さない**
(外の生死監視やロードバランサが `HEAD /healthz` を投げても 200 になる。RFC 9110 §9.3.2)。
`GET` / `HEAD` / `PURGE` 以外のメソッド (`POST` など) は `405` + `Allow: GET, HEAD`、書き換える口
(`/purge` / `/blocklist?action=`) への `HEAD` は何もせず `405` + `Allow: GET`、`/favicon.ico` は本文なしの `204` (T28.7)。
`Host` のポートが違うオリジン形式は `Host` 宛てに転送する (透過プロキシの使い方を壊さないため)。
`Host` の無い HTTP/1.0 のオリジン形式は 400。
転送する要求には `Via: 1.1 rust-http-proxy/<起動ごとの 8 桁 16 進>` を付け、自分の印が付いた要求を受けたら
`508 Loop Detected` で閉じるので、`Host` が自分自身を指す要求も自分へ繋ぎ続けない。

### 読めない要求を数える

`/status` の **`rejected_requests`** は、**要求を読めずに断った数**を理由別に並べた欄 (T14.28)。
`{"request_line":0,"header_too_large":0,"method":0,"no_host":0,"bad_uri":0,"body_framing":0,"obs_fold":0,"dup_host":0,"bad_header":0,"header_timeout":0,"total":0}` の形で、
理由は `/errors` (S14) の `bad_request:<reason>` と同じ 10 種 (`header_timeout` = 408 は T29.10 で足した)、`total` はその合計。公開ポートには走査 (scanner) の
要求が来るので、**何が来たか**をこの 1 行で読むためのもの。エラー (5xx) ではないので `errors` /
`errors_by_cause` には混ぜない。**数えるのは断る経路だけ**で、通した要求は原子を 1 つも触らない。
誰がいつ送ってきたかは `/errors` の個票 (`cause` が `bad_request:<reason>`) で読める。
1 バイトも送らないまま締め切りを過ぎた先張りの接続は**ここには入れず**、`/status` の末尾の **`silent_closes`** に数える
(408 を返さずに黙って閉じる。`/errors` にも積まない。T33.0)。
ログの WARN は理由ごとに 1 分に 1 行 (T28.9。S9)。
### 枠組みの検査で断るもの

- **壊れた要求行**には `400 Bad Request` を返してから閉じる (返した状態コードを個票に正しく書くため。T14.28)
- **`Content-Length` と `Transfer-Encoding: chunked` が両方ある要求**は `400` で断る (T14.28。RFC 9112 §6.1 は中継してはならないと書く。
  前段と後段で本文の切れ目が食い違う「要求の密輸」を通さないため)。判定はどちらも既にある分岐の中なので、通る要求の費用は増えない
- **`Host` が 2 本以上ある要求**は `400` (T19.2。RFC 9110 §7.2。前段と後段で「どの `Host` を信じるか」が食い違う形を通さない)。
  印は 2 本目を見たときだけ立つので、通る要求の費用は増えない
- **不正な `Host` と authority は `400`** (T32.12。RFC 9112 §3.2。オリジン形式の `Host` は `no_host`、絶対 URI は
  `bad_uri` に数える)。ホストは reg-name の字 (英数字と `-._~`、`%XX`) か `[` IP リテラル `]`、ポートは 1〜5 桁の
  数字で 65535 以下 (空のポート `h:` は既定のポート。`h:080` は 80)。`,`・`+`・`;` などの sub-delims は RFC の文法では
  reg-name に入るが、DNS の名前にはなりえないので断る。`Host` は絶対 URI の要求でも確かめる (宛先は要求の対象で決める)
- **枠組みの小物** (T32.15): 版の `HTTP` は大小を区別する (`http/1.1` は `400` の `request_line`。RFC 9112 §2.3)。
  `connect` (小文字) は CONNECT として扱わず、未知のメソッドとして転送する (T32.8 の `get` と同じ)。要求の chunked は
  データの後ろが空行でなければ (`2\r\nokXX\r\n`)、大きさに `+` などの 16 進でない字があれば (`+2`) `400` の `body_framing`
  (LF だけの行末は受ける)。LF だけの行末で届いた要求のヘッダーは、オリジンへは CRLF にそろえて送る。
  オリジンの応答の名前が token でない行 (`X A: b`・`: v`) は落としてから返す。CONNECT の確立に失敗した `502` にも
  `Date` と `Content-Length: 0` を付ける
- 要求行のメソッドが token でない要求 (`GE@T`・NBSP の混ざった `GET`。RFC 9112 §3) は `400` の `method` (T34.142)。
  要求行の区切りは ASCII の空白だけ

## S2 接続・keep-alive・接続プール・IPv4 / IPv6・タイムアウト

クライアント側の持続接続、オリジン側の接続プールと生存確認、デュアルスタックと Happy Eyeballs、確立・ヘッダー・無通信・転送の速さの締め切り。

経緯は [`specs/HISTORY.md`](specs/HISTORY.md) の「S2 接続・keep-alive・接続プール・IPv4 / IPv6・タイムアウト」。

### keep-alive と接続プール

- クライアント接続は HTTP/1.1 の持続接続 (アイドル 15 秒。`PROXY_KEEPALIVE_SECS`)。
  **1 本の接続で 1,000 要求を捌いたら閉じる** (`crates/config` の `DEFAULT_MAX_REQUESTS_PER_CONN`。
  環境変数では変えられない)。**1,000 要求目の応答には `Connection: close` を付ける**ので、
  クライアントは「この応答で終わり」と分かってから閉じられる (入れ違いで次の要求を送って取りこぼさない)。
  `.env` の読み直しは「次の接続から効く」ので、ずっと繋ぎっぱなしのクライアントでも
  遅くとも 1,000 要求後には新しい設定に入れ替わる
- オリジンへの接続はホストごとにプールして再利用する (既定 64 本・全体 256 本まで、アイドル 60 秒まで保持。
  `PROXY_ORIGIN_POOL` / `PROXY_ORIGIN_POOL_TOTAL`)。再利用の前に 1 回の `recv(MSG_PEEK|MSG_DONTWAIT)` で生存確認する。
  **生存確認はどの要求でも行う** (T29.2): 覗いて読み残しがあれば、その接続は捨てて張り直す。
  **覗いてから読むまでの窓は塞げない**: 覗いた直後に届いたバイトは、状態行の形の検査
  (`HTTP/1.x` と 3 桁) だけが頼り。
  再利用できた割合は `/status` の `origin_connections.pool_hit_ratio` に出る
- 再利用したプールの接続が応答を返す前に閉じていたとき、**新しい接続で 1 回だけ送り直すのは本文の無い GET / HEAD だけ**
  (1xx をもう中継していれば送り直さない)。本文付きの要求 (POST・PUT など) は送り直さずに 502 を返す。
  冪等でない要求を送り直さないのは RFC 9110 §9.2.2 (プロキシは冪等でない要求を自動で送り直してはならない) のとおりで、
  冪等でも本文のある要求は本文をもう読み出していて送り直せないため (T38.3 で書いた。振る舞いは T18.4 から同じ)
- 本文は必ず解読してから自前で枠付けし直す (Content-Length / 再 chunk / close) ので、
  HTTP/1.0 のオリジンやクライアントが混ざっても正しく持続する

### IPv4 / IPv6 デュアルスタック

- `[::]` と `0.0.0.0` の両方で待ち受ける (IPv6 が無ければ IPv4 のみ)。
  オリジンへは A / AAAA を引いて IPv6 優先の Happy Eyeballs (RFC 8305) で速い方に接続する。
  `http://[2001:db8::1]:8080/` などの IPv6 リテラルも受ける。`PROXY_IPV6=off` で IPv4 のみにできる
- **IPv6 が黙って落ちる環境 (経路はあるのに繋がらない) では自動で IPv4 を先にする**: ホストごとに
  最後に接続できた族を覚え、IPv6 が起動から 1 度も勝たずに 3 回続けて負けたら、初めて見るホストも
  IPv4 から試す (RFC 8305 §8)。IPv6 が生き返ったかは **canary の IPv6 側の 1 本** (1 分に 1 回、利用者を待たせない) が見ていて、
  繋がれば自動で戻る (S17)。canary が IPv6 を見ていないとき (`PROXY_CANARY=off`、`PROXY_CANARY_IPV6=off`、`--lite` と `PROXY_STATS_PERSIST=off`、
  canary の宛先に AAAA が無い) は、600 秒に 1 回だけ利用者の要求で IPv6 を先頭に戻して試す (T17.7)。
  試行そのものはやめないので、IPv4 が死んでいるホストは IPv6 で拾える。**確実に IPv6 を避けたいなら `off`**
- 勝敗は `/status` の `ipv6` と `/metrics` の `sorahost_ipv6_*` に出る。`/status` の `ipv6.probe_by` は
  いま IPv6 を探っているのが誰か (`"canary"` = canary が直近 600 秒に IPv6 を 1 本試した、`"request"` = 利用者の要求で探る)。
  `probe_by` は「いま」だけなので、**利用者の要求で実際に探った回数は `ipv6.request_probes`** (IPv4 優先の間に IPv6 を先頭に戻した回数。通算)、
  最後の時刻は `ipv6.request_probe_at` (epoch 秒、無ければ 0) で読む (T18.1)。`attempts` は「IPv6 の候補を起動した回数」で、
  IPv4 を先頭に置いた接続が Happy Eyeballs の間隔 (既定 250 ms。`PROXY_HE_STAGGER_MS`) を過ぎて IPv6 の候補を追い掛けで出した回でも増えるので、探りの数ではない
- 待ち受けの**受け入れ待ち行列は既定 `min(1024, somaxconn)`** (`PROXY_LISTEN_BACKLOG`)。Rust の既定の 128 だと、
  ブラウザが 1 ページで開く数十本の CONNECT で溢れて SYN が捨てられ、クライアントの再送で 1 秒待たされるため

### タイムアウト

- `PROXY_TIMEOUT_SECS` が接続と読み書きの締め切り (無通信の締め切り。S7)
- `PROXY_CONNECT_TIMEOUT_SECS` が **オリジンへの TCP の確立だけ** (`CONNECT` と、T28.8 から forward も) の締め切り
  (**既定 3 秒**。未設定なら 3 秒と `PROXY_TIMEOUT_SECS` の小さい方)
- `PROXY_CONNECT_NEGATIVE_SECS` は、**確立が 2 回続けて時間切れになった宛先 (`host:port`) を覚え**、
  その秒数のあいだは繋ぎに行かずに**すぐ `504 Gateway Timeout`** を返す (**既定 30 秒**、`0` で覚えない。`CONNECT` と forward。T36.8)。
  期限が過ぎたら 1 本だけ通して試し、繋がれば忘れ、また時間切れなら同じ秒数だけ断り直す。
  死んだ宛先へ毎回締め切りまで待たせないためで、生き返った宛先は最長でその秒数だけ `504` が続く。
  表の様子は `/status` の `connect_negative` と `/metrics` の `sorahost_connect_negative_*` に出る
- `PROXY_HEADER_TIMEOUT_SECS` が**要求行とヘッダーを読み終えるまでの総時間**の締め切り
  (**既定 10 秒**。越えたら `408 Request Timeout` で閉じる。黙ったまま・1 バイトずつ送る接続 (slowloris) 対策。
  1 バイトも来ない先張りの接続は 408 を返さずに黙って閉じ、`/status` の `silent_closes` に数える。T33.0)
- 転送中の最小の速さ (`PROXY_MIN_TRANSFER_BPS` / `PROXY_MIN_TRANSFER_WINDOW_SECS`) と、名前解決の締め切り (`PROXY_DNS_TIMEOUT_SECS`) は S7

## S3 スレッドと暇な接続の預かり

起動直後のスレッドの本数、接続スレッドの使い回しと上限、暇な keep-alive 接続とトンネルを監視スレッドに預ける仕組み。

経緯と測った数字は [`specs/HISTORY.md`](specs/HISTORY.md) の「S3 スレッドと暇な接続の預かり」。

### 起動直後のスレッド

「使わない機能にはコストを払わない」方針なので、無効にした機能のスレッドは起動しない。

| 設定 | スレッド | 内訳 |
|---|---|---|
| 既定 (キャッシュ・統計あり) | 9 | 待ち受け + `env-reload` + `cache-probe` + `persist` + `history` + `shutdown` + `idle-watch` + `canary` + `profile-sample` (0.29.x の x86_64 で数えた) |
| `PROXY_CACHE_ENABLED=off PROXY_STATS_PERSIST=off` (= `--lite`) | 4 | 待ち受け + `env-reload` + `shutdown` + `idle-watch` |
| `--lite` + `PROXY_PARK_IDLE=off` | 3 | `idle-watch` が減る |

ブロックリストの取得スレッドは `PROXY_BLOCKLIST_FILE` / `PROXY_BLOCKLIST_URL` が設定されたときだけ動く
(`$HOME/.env` の再読込で後から設定された場合もその時点で起動する)。

接続 1 本ごとのスレッドはこれとは別 (CONNECT トンネルは Linux では中継中 1 本あたり 1 スレッド、
暇なあいだは 0 本)。

### 接続スレッドの使い回しと上限

- **接続スレッドは使い回す**: 仕事を終えたスレッドは空き置き場に戻り、30 秒使われなければ自分で
  終わる (空きは最大 64 本まで)。生成と破棄のシステムコールを接続ごとに払わない
- **同時に生きているスレッドには上限がある** (`PROXY_MAX_THREADS`、既定 `auto`。S7)。
  「空いている数」(最大 64 本) と「生きている数」は別のもので、後者がこの上限。
  上限に達したら新しいスレッドを起こさず、その仕事を待たせる (**捨てない**。空いたスレッドが
  順に引き取る)。預けた接続が一斉に切れても、スレッドの数が跳ねない
- 上限は `.env` の再読込で変えられる。当て直すのは接続を受けたときなので、書き換えてから
  **次の接続 1 本**で効く。**上げた場合**は待たせていた仕事をその場で起こし、**下げた場合は
  走っているスレッドを殺さない** (代理の途中で切らないため)。新しいスレッドを起こさなくなり、
  仕事を終えたスレッドから順に終わるので、しばらくは `/status` の `live_threads` が
  `max_threads` を上回ったままになる
- **裏側の再検証 (stale-while-revalidate) もこの置き場で走る** (上限の内側)。ただし待ち方は接続と逆で、**空きが無ければ
  待ち行列に積まずに捨てる**。再検証は「後でやればいい仕事」で、捨てても次の要求で
  普通に取り直されるだけなのに対し、積むと新しい接続の処理がその後ろに並び、
  順番が来るまで「このキーは再検証中」の印を握り続けるため。捨てた要求はそのまま
  同期の再検証に回るので、クライアントには新しい表現が返る
- 代償は「暇なトンネルを預かる速さ」。トンネルは 1 本ごとに猶予 100 ms のあいだワーカーを
  握るので、**次々に張られる CONNECT を受けられる速さの天井が「上限 ÷ 100 ms」**になる
  (上限 256 で毎秒 2,560 本)。それより速く暇なトンネルが増える環境では `PROXY_MAX_THREADS` を上げる。
  短命な CONNECT (張ってすぐ閉じる) と HTTP の代理はワーカーをすぐ手放すので、この天井には当たらない

### 暇な接続の預かり

- **要求を処理していない keep-alive 接続はスレッドを握らない** (`PROXY_PARK_IDLE`、既定 on、
  Linux のみ)。次の要求が猶予 (既定 3 ms。`PROXY_PARK_GRACE_MS`) のあいだ来なければ、その接続は `idle-watch` スレッドの
  epoll に預けられ、ワーカースレッドは解放される。読めるようになったら空いているワーカーに戻す。
  忙しいときの CPU/要求とシステムコール数は変わらない。`off` にすると
  「1 接続 = 1 スレッドが専任する」動きになる
- **預かりが 0 の間、`idle-watch` は眠ったまま起きない** (T29.23)。預かっているものがあれば先頭の期限で起き、
  眠っている所へ預けたとき・先頭より早い期限で預けたとき (`.env` で keepalive を縮めた直後) は、預ける側が
  eventfd の呼び鈴で起こして待ち時間を計り直させる
- **暇な CONNECT トンネルも同じ預かり所に預ける** (同じ `PROXY_PARK_IDLE`、Linux のみ)。
  両方向とも 100 ms 動きが無ければ、クライアント側とサーバー側の記述子 2 本を epoll に預けて
  スレッドを手放す (中継に使うパイプもこのとき手放す)。どちらかが動いたら空いている
  ワーカーへ戻し、`PROXY_TUNNEL_IDLE_SECS` を過ぎたものは預かり所が引き上げて閉じる。
  預けているあいだも同時接続数には数える (`PROXY_MAX_CONNS` の意味は変わらない)。数えているので、
  上限に当たった接続はこの中の**最古の 1 本を閉じて**席を作る (S7 の `PROXY_MAX_CONNS`)。
  本数は `/status` の `parked_tunnels` に出る。片方向だけ閉じたトンネルは預けない
- 中継に使う `splice(2)` のパイプは、**空になったものをワーカースレッドが 2 本まで持ち越して
  次のトンネルで使い回す**。中身が残ったままのパイプ (送り先が途中で消えたとき) は使い回さずに閉じる

## S4 あて先と接続元の許可・拒否、ブロックリスト、`/proxy.pac`、外から使える口の守り

ACL・接続元ごとの上限・ブロックリスト・自動設定スクリプト、書き換える口を誰でも使えることと `PROXY_ENDPOINTS_READONLY`、重い口を同時に 1 本だけ組むこと。

経緯は [`specs/HISTORY.md`](specs/HISTORY.md) の「S4 あて先と接続元の許可・拒否、ブロックリスト、`/proxy.pac`、外から使える口の守り」。

### 許可と拒否

- `PROXY_ALLOW_HOSTS` / `PROXY_DENY_HOSTS` があて先ホストの許可・拒否リスト (ワイルドカード可)。外れた要求は 403 Forbidden
- `PROXY_ALLOW_CLIENTS` が**接続元**の許可リスト (`10.0.0.0/8` のような CIDR 可)。
  一覧に無い相手は accept 直後に閉じる (内部エンドポイントも含めて。既定は全許可)
- `PROXY_MAX_CONNS_PER_CLIENT` が**接続元ごとの同時接続の上限** (既定 `0` = 無効)。
  1 人が `PROXY_MAX_CONNS` を使い切るのを防ぐ公平さの上限で、超えた接続は 503。
  自分宛て (`/status` など) は上限の外で受けるので、上限に当たっている相手からでも監視は取れる
- **ドメインのブロックリスト**: hosts 形式のファイルや URL (1 日 1 回自動更新) から読み、広告・トラッカーを CONNECT の段階で 403 にする。
  `/blocklist?host=<h>` でブロックリストの判定、
  `&action=block|allow|clear[&ttl_secs=N]` で一時的な上書き (既定 24 時間、`0` で無期限。状態ファイルに 256 件まで残る)
- **`/proxy.pac`**: ブラウザの自動設定スクリプト。自分自身・ローカル・`PROXY_PAC_DIRECT` のホストは DIRECT、
  それ以外はこのプロキシ経由 (落ちていれば DIRECT)。ブラウザに `http://<host>:<port>/proxy.pac` を設定するだけで使える

### 書き換える口

**書き換える口 (`/purge?url=` / `/purge?all=1` / `/blocklist?...&action=`) も、読む口と同じくポートだけで自分宛てにする**
(T29.35、利用者の決定「すべて外から使える」)。公開の名前 (例 `tokyo.sorahost.net`) で開いたダッシュボードの「消す」と
ブロックリストの上書きのボタンも効く。`PROXY_SELF_NAMES` は読まない。
そのため、ポート 80 で動かすと外のサイトが `http://任意の名前/purge?all=1` を
踏ませるだけで全消去やブロックリストの改変ができる。止めたいときは `PROXY_ENDPOINTS_READONLY=on`。

これらのパスはプロキシ自身が応答し、同じポート宛てのオリジン形式の要求より優先する。認証は無いので、
到達できる人は誰でも purge できる (公開ポートで動かすなら到達制御を)。
**`PROXY_ENDPOINTS_READONLY=on`** にすると、書き換える口 (`/purge` / `PURGE` / `/blocklist?action=`) だけを
405 で断る (読む口はそのまま)。**公開ポートで見知らぬ接続元が増えたら `PROXY_ALLOW_CLIENTS` で絞れる**
(一覧に無い相手は accept 直後に閉じるので、内部エンドポイントにも届かない)。
**`PROXY_MAX_CONNS_PER_CLIENT=N`** は、1 つの接続元が同時に開ける本数を N 本に抑える (超えた接続は 503。
自分宛ては上限の外で受けるので監視は取れる)。どれも**認証ではない** — このプロキシに `Proxy-Authorization` は
無く、入れる予定もない (S25)。経路を絞る (`PROXY_ALLOW_CLIENTS`) か、消せる口を閉じる (`PROXY_ENDPOINTS_READONLY`) か、
1 人の取り分を抑える (`PROXY_MAX_CONNS_PER_CLIENT`) かの 3 つだけ。

### 重い口

**重い口は同時に 1 本だけ組む** (つまみは無い。いつもこの動作)。`/snapshot` は 17 本ぶんの口を
1 つにまとめた**最大 4 MiB** の JSON で、認証が無いので誰でも好きなだけ叩ける。1 秒に 10 回引かれると、それだけで
CPU と統計の鍵の時間を食う。そこで**大きい応答を組む口は同時に 1 本**とし、組んでいる最中に来た 2 本目からは
**`503 Service Unavailable` + `Retry-After: 1`** (本文は `{"schema":1,"error":"busy"}`) で断る。重い口はこの 6 つ:

| 口 | 重いと見なす条件 |
|---|---|
| `/snapshot` | いつも (17 部・最大 4 MiB を組む) |
| `/profile` | `?summary=1` **以外**はいつも (段階・スレッドの CPU と状態・ロックの取り合いの窓。要約は標本を 1 本も組まないので軽い口。T15.0) |
| `/explain` | いつも (ほかの口を横断して 1 枚に組む。S19) |
| `/hosts` | `limit` が **200 より大きい**とき (`/hosts?limit=1000` など) |
| `/recent` | `n` が **500 より大きい**とき (`/recent?n=2000` など) |
| `/history` | `n` が **720 より大きい**とき (`/history?res=5&n=4320` は 1.9 MB) |

既定の `limit` / `n` のまま引くぶんには重い口にならない。`/status` `/healthz` `/metrics` のような軽い口は
**この判定にも旗にも触らない** (監視が 5 秒ごとに叩く口を断ったら、断ったこと自体が事故になるため)。
断った累計は `/status` の末尾の **`heavy_rejected`**。**走査に対する最小限の保護で、認証ではない** —
順に引けば誰でも全部 200 で取れ、`Retry-After: 1` のとおり 1 秒後に引き直せばまず通る。
旗は**組み終わったところ**で戻すので、読むのが遅い相手 1 人が全員に 503 を返させることはない。
費用は重い口に来たときの原子 1 回だけで、プロキシとしての転送 (forward / CONNECT) には 1 命令も足さない。

## S5 キャッシュ

2 段キャッシュの保存の条件、HTTPS のオリジン、鮮度と再検証 (RFC 9111)、自動モードの予算と動的マージン、先行確保 (バラスト)、Pterodactyl での割当、キャッシュの操作。 本文は [`specs/S5.md`](specs/S5.md)。

## S6 名前解決

DNS キャッシュの TTL、期限前の引き直し、keep-warm、serve-stale、失敗の記憶、1 要求 1 回の解決、答えが変わった回数。

経緯と見積もりの道具の読み方は [`specs/HISTORY.md`](specs/HISTORY.md) の「S6 名前解決」。

- **DNS キャッシュ**: 名前解決の結果を 60 秒保持し (`PROXY_DNS_TTL_SECS`)、CONNECT ごとの解決をなくす。解決に失敗したときは古い結果で凌ぐ。
  **直近 60 秒に使われた名前は期限の 3/4 (45 秒) で裏で引き直す**ので、使い続けているホストは
  期限切れのミス (`getaddrinfo` の待ち) を払わない。引き直しは専用のスレッド 1 本 (`dns-refresh`、
  最初の引き直しで起きる) が順にやり、要求の経路は待たない。そのぶん**古い答えで繋ぐ窓が
  最長 TTL の 1/4 (15 秒)** できるが、その間に IP が変わっていても次の候補へ行くだけ
  (Happy Eyeballs)。解決に失敗した名前は 60 秒覚えて引き直さない (`PROXY_DNS_NEGATIVE_SECS`)
- **keep-warm** (**既定は止めてある**。T22.4。`PROXY_DNS_WARM_SECS=3600` で動く):
  **直近 3,600 秒 (`PROXY_DNS_WARM_SECS`) に 2 回以上使われた名前 (warm) は、
  使われていなくても 45 秒 (3/4 TTL) ごとに裏で引き直し続ける**ので、TTL より長い間隔で来る
  ホスト (数分おきの push 通知など) もミスにならない。同時に warm でいられるのは 32 件まで
  (最後の使用が古いものから外れ、最後の使用から 3,600 秒過ぎたら止まる)
- **serve-stale** (RFC 8767。T21.3): **期限が切れていても、引いてから 21,600 秒
  (`PROXY_DNS_STALE_SECS`、0 で止める) 未満の答えは待たずにそのまま使い、裏で 1 回だけ引き直す**。
  間遠な名前 (1 時間以上あけて使う名前) の期限切れのミスを利用者に待たせないため。
  **古い答えで繋げなかったとき** (`refused` / `unreachable` / `timeout`) は、その場で 1 回だけ引き直し、
  番地が変わっていれば 1 回だけ繋ぎ直す (CONNECT と forward の両方。番地が同じなら繋ぎ直さず元のエラー)。
  `/status` の `dns.stale_hits` が古い答えで繋ぎに行った回数 (**ミスには数えない**)、`dns.stale_retries` が
  古い答えで繋げずに引き直した回数 (そのとき同期で引いた回は `misses_by_kind.expired` にも入る)。
  `stale_served` は別物で、引き直しが**失敗**したときに古い答え (1 時間以内) で凌いだ回数
- この規則を変えたらミスと引き直しがどう動くかは **`scripts/dns-replay.py SNAP.json`** が、
  引き直しが「使われていない名前」で起きていないかは **`scripts/dns-idle.py SNAP.json …`** が、
  スナップショットから見積もる
- **名前解決は 1 要求 1 回**: ローカル宛て (SSRF) の判定で引いた答えをそのまま接続に使うので、
  キャッシュを切っていても判定と接続が別の答えを見ることはない
- **答えが変わった回数 (`changes`)**: 引き直し (keep-warm と期限切れの再解決) は
  1 つ前の答えを手元に持っているので、**変わったかどうかを数えるだけ**で「その名前の
  アドレスがどれくらいの頻度で動くか」(CDN のローテーション頻度) が分かる。名前ごとは
  `/dns` の `changes`、合計は `/status` の `dns.changes`。**読み方は
  `changes ÷ (misses + refreshes)`** = 引き直し 1 回あたりに答えが変わる割合で、
  TTL 60 秒が長すぎるか短すぎるかの材料になる。0 に近ければ答えは固定 = TTL を延ばして
  引き直しを減らせる (`getaddrinfo` の回数が減る)、1 に近ければ毎回違う = TTL を延ばすと
  古いアドレスへ繋ぐ窓がそのぶん延びる、と読む。**順番が入れ替わっただけは数えない**
  (`getaddrinfo` は同じ答えを違う順で返すことがあるため)
- 要求が名前解決を待つ締め切り (`PROXY_DNS_TIMEOUT_SECS`) は S7

## S7 環境変数と `.env` の読み直し

全 `PROXY_*` / `SERVER_*` の既定と意味、`$HOME/.env` の優先と再読込、読めない値・知らない名前の扱い、`/config`、キャッシュの置き場の選び方。 本文は [`specs/S7.md`](specs/S7.md)。

## S8 コマンドライン引数と `--lite`

引数と環境変数の優先、`--check` の出すものと終了コード、`-V` の版とビルドの種類、`--lite` で止めるものと残るもの。 本文は [`specs/S8.md`](specs/S8.md)。

## S9 ログ

標準出力への 1 要求 1 行のアクセスログ、レベルごとの中身、`/log` に残す行、WARN を理由ごとに 1 分 1 行に抑えること、`cache=` の値。

経緯は [`specs/HISTORY.md`](specs/HISTORY.md) の「S9 ログ」。

- 既定 (INFO) で 1 リクエスト 1 行のアクセスログ (キャッシュ HIT/MISS/REVALIDATED/STALE 付き) を標準出力へ出す
- `PROXY_LOG_LEVEL` で `error` / `warn` / `info` / `debug` / `trace` を切り替える

すべてのログはレベルによらず **標準出力 (stdout)** へ出る。既定の `info` レベルで
1 リクエストにつき 1 行のアクセスログが出る。

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
| `warn` | 400 系 (400 / 403 / 414 / 417 / 431 / 505 / 508。**理由ごとに 1 分に 1 行**、下を参照)、502 とオリジンの時間切れ (**原因ごとに 1 分に 1 行**。T29.15)、キャッシュ I/O 失敗、資源が計測できない環境、設定の警告 |
| `info` | 起動時の設定サマリ、予算の大きな変化、まとまった先行確保、圧迫 / ENOSPC の検知、アクセスログ |
| `debug` | 接続の受付・切断、自分宛ての要求 (`/status` などの 1 行)、キャッシュ HIT/MISS/STORE/EVICT/REFRESH の詳細、予算の小さな変化 |
| `trace` | リクエスト / 転送 / レスポンスの全ヘッダー、トンネル転送量、L2 書き込み |

**`warn` 以上の行は直近 1,000 行だけプロセスのメモリにも残り、`/log?n=200` で読める** (1 行 256 B まで、新しい順)。コンソールが流れて消える環境 (Pterodactyl) で「さっき何を警告したか」を後から見るための口。`info` のアクセスログは写さない (1 行 7.2 us/要求 の熱い経路を重くしないため)。

**ログの量は相手に決めさせない** (T28.9)。不正な要求 (400 / 414 / 431 / 505) の WARN は
**`/status` の `rejected_requests` の理由ごとに 1 分に 1 行**だけ出し、その 1 分に抑えた本数は次の行の終わりに
` (+N more since the previous line, not logged)` と添える (本数そのものは `rejected_requests` と `/errors` が全部持っている)。
403 (ACL・ブロックリスト・CONNECT のポート・ローカル宛て) は拒否の理由ごとに、417 と 508 はそれぞれ 1 分に 1 行
(接続数の上限の WARN と同じ間隔)。**自分宛ての要求** (`/status`・`/healthz`・ダッシュボードが引く口) の
`GET /status -> 200 (internal endpoint)` の行は `debug` (ダッシュボードを開いたままにすると約 1.2 行/秒でコンソールを埋めるため。
`PROXY_LOG_LEVEL=debug` で見える)。
**オリジン側の失敗** (CONNECT と転送の `502 Bad Gateway: connect … failed`、`origin response timed out`・
`failed to read origin response`、壊れた `Content-Length`・本文の途中で切れた応答・HTTP/1.0 へ渡せない符号化) の WARN も
**原因ごとに 1 分に 1 行** (T29.15。原因は `/status` の `errors_by_cause` と同じ `dns`〜`other` の 8 つと名前解決の内訳
`nxdomain` / `dns_temp`。抑えた本数は同じく次の行の終わりに添える)。本数は `/errors` (`repeats`)・`/hosts`・`errors_by_cause` が
全部持っている。

アクセスログの `cache=` の値: `HIT(層)` 新鮮なヒット / `HIT(層,304)` クライアントの条件付き要求に 304 で応答 /
`REVALIDATED(層)` オリジンに再検証して 304 を受け延命 / `REFRESHING(層)` 期限切れ直後の表現を返し裏で再検証 /
`COALESCED(層)` 同時に進行中だった取得の完了を待ってキャッシュから配信 /
`STALE(層)` オリジン障害・待ち切れ時に期限切れを配信 /
`MISS stored` 取得して保存 / `MISS` 取得したが保存対象外 / `BYPASS` キャッシュ対象外の要求 (S5)。

## S10 状態ファイルと再起動をまたぐもの

統計と履歴の状態ファイル (`.rrd`) の大きさと版、日次の要約、日次のスナップショット、個票のファイル。

経緯と測った数字は [`specs/HISTORY.md`](specs/HISTORY.md) の「S10 状態ファイルと再起動をまたぐもの」。

- **統計と履歴の永続化**: 固定サイズ (**8 MiB**) の状態ファイルに、履歴 3 解像度 (5 秒 × 1 時間、1 分 × 1 日、
  1 時間 × 30 日) を環状に、ホスト別・接続元別の上位 1000 を固定スロットに書く。ファイルは伸びず、再起動後も表とグラフが残る。
  **形式に版があり、いまは版 3** (`SHPRRD03`)。**版 2 のファイルは起動時に 1 回だけ版 3 の形へ詰め直す**ので、
  版を上げても通算の統計 (要求数・ホスト・履歴) は消えない (この仕様の「状態ファイルの版 (`.rrd`)」)。
  停止シグナル (SIGTERM / SIGINT) では表を書き出してから終了する (3 秒以内、2 回目のシグナルで即終了)。
  あわせて **1 日 1 行の要約**を `$HOME/.rust-http-proxy.daily.jsonl` に**永久に**残す (S20 の `/daily`)
- **日次のスナップショット**: **1 日 1 回 (UTC 0 時)**、その時点の `/snapshot` をまるごと
  `$HOME/.rust-http-proxy/snapshots/<YYYY-MM-DD>.json` に残す (既定 30 日ぶん、31 個目で最古を消す。
  1 ファイル 4 MiB まで)。**外から取りに行かなくても、見に行ったときに 30 日ぶんの個票がある**。
  **停止シグナルで状態ファイルを書く前と起動から 60 秒にも 1 枚ずつ** (`<UTC 時刻>-shutdown.json` / `-start.json`。
  締めの「前」と「0 時間」。T28.20) 残す。
  読む口は `/snapshots` (一覧) と `/snapshots/<date>` / `/snapshots/<名前>` (S16 と、S7 の `PROXY_SNAPSHOT_DAYS`)
- **個票の永続化**: 統計の状態ファイルの隣にもう 1 つ、**固定サイズ (4 MiB) の個票のファイル**
  (`$HOME/.rust-http-proxy.recent`) を置き、`/recent` (閉じた接続) ・`/errors` ・`/bursts` (山の写真) ・
  `/events` (出来事) ・`/log` を再起動をまたいで残す。書くのは**履歴スレッドの 5 秒の周期だけ** (接続を受ける経路はメモリのリングに
  書くだけで、1 命令も増えない) と、停止シグナルの最後の 1 回。これも伸びず、版が違うファイルは読み捨てて作り直す。
  統計の `.rrd` とファイルを分けてあるのは、**版を別々に上げられるようにする**ため (個票の形を変えても統計は触らない)

### 状態ファイルの版 (`.rrd`)

- 先頭 8 バイトが版の印で、いまは `SHPRRD03` (**版 3**)。大きさは
  **8,388,608 B 固定** (領域の合計は 4,274,208 B、残りは次に項目が増えたときの余白)。1 レコードは
  履歴の標本が 1,024 B (いま使っているのは 976 B = 122 項目。足したタスクは `specs/HISTORY.md`)、ホスト別・接続元別が 640 B (同 568 B) で、
  項目は**「固定の順 + 予備」**。**末尾に足す限り版は上がらない** (古いファイルはその位置が
  ゼロ埋めなので 0 として読み戻る)。`/status` の `state_file` に `version` と `converted_from` が出る
- **領域を 1 つ足しても版 3 のまま** (T24.4。並びの末尾に 32 B の `baseline` を 1 本。
  前の 6 領域のオフセットは 1 バイトも動かないので、それ以前の版 3 のファイルもそのまま読める)
- **版 2 (`SHPRRD02`) のファイルは、起動時に 1 回だけ版 3 の形へ詰め直す**
  (領域ごとに読み、レコードの末尾をゼロで伸ばして書き戻すだけ。**通算の統計は 1 件も消えない**)。
  そのとき `state_file.converted_from` が `2` になり (詰め直していなければ `null`)、起動のログに所要が出る
  (要求を受ける経路の費用は 0)
- 退避 (`.rrd.v2` など) は残さず**その場で書き換える**。**版の印は最後に書く**ので、詰め直しの途中で
  落ちたファイルは次の起動で「壊れている」と見なして作り直す (中途半端に混ざった版は残らない)
- 版 1 以前・短いファイル・先頭が壊れたファイルは、読み捨てて作り直す
  (統計は運用の参考値なので、そこまで面倒は見ない)

## S11 内部エンドポイントの共通の形、`/status`、`/metrics`

`/` の案内、応答の形の版 `schema`、`/status` の窓と欄 (上限・ホスト別・接続元別・名前解決・RSS の内訳・直近の分位点・自己ベンチ・転送速度)、`/metrics` の系列。 本文は [`specs/S11.md`](specs/S11.md)。

## S12 画面 (`/dashboard`・`/inspect`・`/probe.html`)

ブラウザで開く 3 枚のページが何を何秒ごとに引いて何を描くか、大きさの上限、欄を持たない版での振る舞い。 本文は [`specs/S12.md`](specs/S12.md)。

## S13 `/healthz` と `capabilities`

健康診断の検査と状態コード、この環境で何が読めるかの項目。

経緯は [`specs/HISTORY.md`](specs/HISTORY.md) の「S13 `/healthz` と `capabilities`」。

### `/healthz`

- `{"ok":bool,"checks":{...}}` の**軽い JSON** (1 KiB 弱) で、
  **`fatal` な検査**が 1 つでも偽なら **`503 Service Unavailable`** を返す (Pterodactyl やモニタが
  200 / 503 で判断できるように)。問い合わせ (`?sort=` など) は読まない
- 検査は 7 つ:
  - `listening` (待ち受けが生きている = この応答が届いている)
  - `fds` (開いている記述子が `max_fds` の 90% 未満)
  - `connections` (いまの接続数が `PROXY_MAX_CONNS` 未満。**この `/healthz` 自身の 1 本は除く** —
    上限に当たっている最中でも T13.2 の「上限 + 4 本」の枠でこの応答は届くため)
  - `state_file` (状態ファイルの書込エラーが直近 5 分で増えていない)
  - `listen_overflows` (受け入れ待ち行列が直近 5 分で溢れていない。**ネットワーク名前空間ごとの数**なので、
    同じ名前空間に他の待ち受けがあるとそちらの溢れも数える。コンテナなら実質このプロキシのぶん)
  - `resolver` (名前解決の最後のミスが 2 秒未満 = リゾルバが死んでいない)
  - `cpu` (cgroup の CPU の上限で**直近 5 分に絞られた期間が 50% 未満**。`"throttled_5m"` /
    `"periods_5m"` / `"percent"` も出る。T15.0。**起動から 5 分に満たない間は `null`** —
    5 分の窓は「いちばん新しい標本から 300 秒ぶん」を足すだけなので、溜まる前は数秒ぶんの
    割合しか出ない。異常の規則と同じ物差しで黙る)
- **`cpu` だけは `fatal` ではない** (`"fatal":false` が出る): 偽になると本文の `ok` は
  偽になるが、**状態は 200 のまま**。絞られているのは「遅いが動いている」状態で、
  パネルが 503 で再起動をかける作りだと再起動の輪に入るため (503 にするかは運用の決めごと)。
  `"fatal"` は**偽のときだけ**出る (無い = `fatal`)
- **この環境で調べられないものは `null`** で、`ok` の判定に入れない
  (Linux 以外・`/proc/net` の無いコンテナ・状態ファイル無し・まだ名前解決をしていない・
  履歴スレッドが動いていない `--lite` / `PROXY_STATS_PERSIST=off`)

### `capabilities` (この環境で何が読めるか)

- `/status` と `/config` の `capabilities` に
  `{"proc_syscall":true,"tcp_info":true,"cgroup_cpu":true,"cgroup_pressure":true,"ipv6_route":true,"resolver_ms":9,"home_writable":true,"checked_at":1758...,"proc_schedstat":true}` が出る。
  統計の `null` が「無かった」のか「読めなかった」のかを先に答えるためのもの
- `proc_syscall` は `/proc/self/task/<tid>/syscall` (スレッドの状態)
- `tcp_info` は待ち受けソケットへの `getsockopt(SOL_TCP, TCP_INFO)` (カーネルの RTT と再送。**読む側と同じ口**で RTT の欄まで書かれるときだけ真。T35.6)
- `cgroup_cpu` / `cgroup_pressure` は自分の cgroup の `cpu.stat` / `cpu.pressure` (CPU の絞りと PSI)。**`cgroup_cpu` は `kernel.cgroup_cpu` を読む側と同じ道** =
  cgroup v2 の階層を根まで遡って絞りの欄のある `cpu.stat` が見つかるときだけ真で、cgroup v1 だけの環境では偽 (T28.11)。`cgroup_pressure` も読む側と同じで、v2 の階層に `cpu.pressure` が無ければ機械全体の `/proc/pressure/cpu` が読めるときに真 (T28.32)
- `ipv6_route` は `/proc/net/ipv6_route` の既定経路
- `resolver_ms` は `example.com` を 1 回引くのにかかった ms (締め切り 2 秒、失敗は `null`)
- `home_writable` は `$HOME` に書けるか (状態ファイルの置き場)
- `proc_schedstat` は `/proc/self/task/<tid>/schedstat` (走れるのに走れなかった時間。`/profile` の
  `run_delay_us`)。`schedstat` は `CONFIG_SCHEDSTATS` を入れていないカーネルでは
  **ファイルごと無い**ので、そこでは `false` = `/profile` の `run_delay_us` が `null` になる
- **判定は起動時 1 回と 1 時間ごと** (`.env` の監視スレッドのついで。要求の経路では何もしない)、
  `checked_at` がその時刻。Linux 以外では `/proc` も cgroup も無いので `false` になる

## S14 個票

`/errors`・`/clients`・`/readers`・`/recent`・RTT と再送・SNI・SYN の再送・中継の詰まりの向き・`/bursts`・`/trace`、再起動をまたぐか、応答の大きさの上限、`PROXY_RECORDS`。 本文は [`specs/S14.md`](specs/S14.md)。

## S15 出来事と異常の自動検知 (`/events`)

`/events` に積む出来事の種類、異常の自動検知の規則、初めて見た接続元。

経緯と測った数字は [`specs/HISTORY.md`](specs/HISTORY.md) の「S15 出来事と異常の自動検知 (`/events`)」。

### 起きたことの時系列

- `/events?n=200&since=<epoch>` は、**プロキシに起きた出来事**を
  1 本の時系列にしたもの (T14.11。新しい順、既定 200 件、512 件の環状)。数字が動いたときに
  「**そのとき何を変えたか**」を読むための口で、`/status` の `settings` は最後の 1 回しか残さず、
  `/log` は warn 以上なので info の出来事 (再読込・ブロックリストの取得・バラストの増減) が入らない
- 1 件 = 時刻 (`at`、epoch 秒)・種類 (`kind`)・短い説明 (`text`、128 バイトまで)
- 種類は `start` (起動。版と設定の要約) / `reload` (`.env` の再読込。**変わった名前と前後の値**、
  再起動が要る項目はその旨) / `blocklist` (一覧を組み直した。件数と取得の成否) / `ipv6`
  (IPv4 優先への切替と解除) / `pressure` (メモリの圧迫の検知と解消) / `ballast` (先行確保が
  ±64 MiB 以上動いた) / `state_file` (状態ファイルの書込エラー。**最初の 1 回だけ**) / `evict`
  (上限に当たって暇なトンネルを閉じた。**1 時間に初めて起きたときだけ**) / `emfile`
  (accept の失敗。同じく 1 時間に 1 回) / `shutdown` (停止シグナル) / `anomaly`
  (この仕様の「異常の自動検知」) / `new_client` (**初めて見た接続元**。この仕様の「初めて見た接続元」) の **12 種で固定**
  (`kinds` にも並ぶ)。**書くのは稀な経路だけ**で、要求ごとの経路には 1 命令も足さない
- `ipv6` / `pressure` / `ballast` / `anomaly` / `new_client` の 5 つは履歴スレッドの周期 (5 秒) で状態の変わり目を拾うので、
  時刻は最大 5 秒遅れ、`--lite` と `PROXY_STATS_PERSIST=off` (履歴スレッドを起こさない設定) では
  残らない。残りの 7 種は `--lite` でも残る。**メモリだけ**なので再起動で消える
  (512 件で 87 KiB、応答は 256 KiB 以下)。再起動をまたいで残すのは個票のファイル (S10)

### 異常の自動検知

- 24 時間の図を人が全部読むのは手間なので、**「いつから」を機械に印させる** (T14.23)。履歴スレッドが 5 秒ごとに取る標本を**直近 5 分の窓**に畳み、**直近 1 時間の基準値**と
  比べて外れていたら、`/events` に `anomaly` の 1 件を書く (専用の口は無い。
  `/events?n=200` で読む。畳むのは `/history?summary=1` と同じ関数・同じ切り方)
- 判定は **8 種 + 規則 6** で固定で、閾は次のとおり。

  | 種類 (説明の頭に出る) | 立つ条件 |
  |---|---|
  | `connect_p95` | CONNECT 1 本ごとの**「確立 − オリジン側の RTT」(名前解決を除く。`/history` の `conn_excess_*`)** の p95 (直近 5 分) が**直近 1 時間の p95 の 3 倍以上**、かつ 50 ms 以上、かつ**その窓に 20 本以上**あるとき (T15.0。T29.11 で「確立 − RTT」にした。名前解決の遅れは `dns_slow` が見る)。**直近 1 時間の p95 が 1 ms 以下 (分位点の細かさの下限 = 底) なら、倍率ではなく基準値を 500 ms 以上上回ったとき** (T39.3)。**起動から 10 分 (直近 5 分の窓 2 つぶん) は判定しない** (T39.3。`/history` は状態ファイルから読み戻すので、基準値の窓は起動直後でも前のプロセスの標本で埋まっている) |
  | `dns_slow` | **443・80 宛ての接続のための、名前解決に成功した**ミス (`/history` の `web_dns_misses` 回・`web_dns_ms_sum` ms。T32.16・T33.3。外したぶんがあれば説明に `, 10 not to 443/80, 3 failed not counted` のように入る。`/slo` の `error_rate` と同じく 443・80 だけを見るのは利用者の決定) 1 回の平均 (直近 5 分) が **100 ms と canary の名前解決の中央値 (直近 1 時間) の 3 倍の大きい方以上**、かつ**その窓にミスが 3 回以上**、かつ**その窓のミスの合計が 1,000 ms 以上**あるとき (canary が `off` なら 100 ms だけ。5 分に 1 回の重いミスだけでも、3〜5 回で合計の小さい初めての名前の引きだけでも立たない。3 回でも 1 回 334 ms 以上なら立つ。T17.1 / T22.2) |
  | `errors` | エラーが **5 分で 5 件以上** (原因の内訳も説明に入る)。**`/slo` の `error_rate` と同じ数え方で、名前解決の失敗 (原因 `dns` の全部。`nxdomain` も `dns_temp` も) と 443・80 以外の宛先のエラー (`/history` の `other_port_errors`。メール宛 `:993` の確立の時間切れなど) は数えない** — 外した数があれば `(threshold 5; 9 dns not counted; 3 not to 443/80)` のように説明に入る (内訳には外したものも入ったまま。T28.17 で `nxdomain` を外し、T32.36 で `/slo` に揃えた) |
  | `active_high` | 同時接続の山が **`PROXY_MAX_CONNS` の 50% 以上** (`/bursts` の写真と同じ閾。同じ周期で写真が撮れていればその `seq`) |
  | `rejected` | `rejected_overload` / `evicted_idle` / `rejected_client_acl` が**増えた** |
  | `cpu_throttled` | cgroup の CPU の上限で**直近 5 分に絞られた期間が 50% 以上** (解除は 25% 未満。説明に `quota_cores` と使ったコア数と `/profile` の上位スレッドの tid が入る。T15.0) |
  | `tunnel_spin` | 「起こされたのに 1 バイトも進まない」が **1 周期 1,000 回以上のトンネルが 1 分続いた** (説明に接続の `id` と宛先と齢と `half_closed`。T15.0) |
  | `dns_miss_rate` | 名前解決に**成功した**ミス (`dns_misses − dns_fail_misses`。`/slo` の `dns_miss_per_connect` と同じ数え方。T32.36) が**直近 1 時間で 0.20 回/接続 以上** (失敗したぶんを外したら説明に `, 387 failed not counted` のように入る。解除は 0.10 未満。**起動から 6 時間は判定しない** — 再起動の直後はどの名前も warm でないため。1 時間に 30 本以上の確立があるときだけ。T15.0) |
  | `new_client` (規則 6) | `/clients` の `first_seen` が**この 5 秒の窓の中**にある接続元 (T14.54。この仕様の「初めて見た接続元」) |

- `dns_slow` の閾を変えたら件数がどう動くかは、**`scripts/dns-slow-replay.py SNAP.json …`** がスナップショットの
  `/history?res=60` から 5 分の窓を作り直して案ごとに並べる (T22.2。1 分の行で数えるので、5 秒の境目に
  またがる窓は見えず、実機より少なめに出る)。表の `sum1000` がいまの条件 (T22.2) で、`current` はその前の条件
- **同じ種類は収まるまで 1 回だけ**書く。条件を外れたまま 5 分続いたら
  `cleared: <種類>` で始まる解除の 1 件を書き、また立てるようになる。説明には必ず数字
  (何が・いくつ・基準値) が入る (例: `connect_p95: connect-rtt p95 136 ms over 5m is 13.9x
  the 1h baseline 9.8 ms (122 of 1442 connects)`、`cleared: errors after 11m (0 errors in 5m)`)。
  `connect_p95` の基準値が底のときは倍率の代わりに上回った ms を書く (例: `connect_p95: connect-rtt
  p95 1038 ms over 5m is 1037 ms above the 1h baseline 1.0 ms (37 of 516 connects)`。T39.3)
- `connect_p95` だけは**立っている間は「立った瞬間の基準値」と比べる** (基準値の窓は直近の
  5 分を含むので、山が 1 時間続くと基準値そのものが山になって倍率が 1 に戻り、**山の最中に
  解除を書いてしまう**ため)
- **費用は 0** で、判定するのは履歴スレッドだけ
  (要求ごとの経路には 1 命令も無い。`--lite` と `PROXY_STATS_PERSIST=off` では
  履歴スレッドが無いので判定もしない)

### 初めて見た接続元

- 認証なしの公開プロキシなので、**見知らぬ接続元は「現れた瞬間」に気づきたい** (T14.54。この仕様の「異常の自動検知」の規則 6)。
  異常の自動検知と同じ 5 秒の周期で `/clients` を見て、**`first_seen` がその窓の中**にある接続元を
  `/events` に **`new_client`** で 1 件残す (種類が `anomaly` ではなく `new_client` なのは、
  異常ではなく「初めて」だから)
- 説明には接続元・要求数・**最初の宛先の種類**
  (ポートと、名前宛てか IP リテラル宛てか)・`User-Agent` が入る
  (例: `new_client: 198.51.100.7 first seen (1 req, first target port 443 (name), agent "curl/8.5.0")`)
- **同じ接続元は 1 回だけ**で、2 回目の要求では増えない。走査を受けて 1 周期に何百も現れたときは
  **8 件まで**書き、残りは `new_client: N more clients first seen in the same 5s window (wrote 8)` の
  1 行にまとめる (出来事のリング 512 件を 1 回で流さないため)。状態ファイルから読み戻した接続元
  (`first_seen` が `0` = この起動より前から居た) は「新しい」ではない
- **費用は 0** で、5 秒に 1 回、接続元の表の鍵を取って `first_seen` を比べるだけ
  (`/clients` を丸ごと写さない。要求ごとの経路には 1 命令も無い)

## S16 `/snapshot` と日次のスナップショット

1 要求で全部を取る `/snapshot` の形と上限、日次・起動・停止のスナップショットを残す規則。

経緯と測った数字は [`specs/HISTORY.md`](specs/HISTORY.md) の「S16 `/snapshot` と日次のスナップショット」。

### 1 要求で全部取る (`/snapshot`)

- `/snapshot` はほかの口を **1 つの JSON** にまとめて返す (T14.4)
  (`status` / `status_errors` / `status_dns` / `history` (`5` / `60` / `3600`) / `dns` / `errors` /
  `connections` / `recent` / `hosts` / `hosts_series` / `clients` / `bursts` / `profile` / `events` / `log` の 17 部。何が入っているかは `parts` に並ぶ)。
  デプロイ先の様子を見るのに 17 本の URL を手で叩かずに 1 回で済ませるための口
- **組み立ては同じプロセス内の関数呼び出し** (自分へ HTTP で繋ぎ直さないので、接続を 17 本増やさず、
  上限に当たっている最中でも取れる)
- 上限は **4 MiB** で、越えたら `recent` → `log` → `history.5` の順に
  `null` へ落として `dropped` に名前を出す。保存して読むのは `scripts/collect-deployed.sh`
- 重い口なので同時に 1 本だけ組む (S4 の「重い口」)

### 日次・起動・停止のスナップショット

- `/snapshots` は `$HOME/.rust-http-proxy/snapshots/` に残してある日付と大きさ (`files[]` の `date` / `bytes` /
  `t` (書いた時刻)、**古い順**) と、置き場所 (`dir`)・残す日数 (`days`)・上限
  (`max_bytes` = 4 MiB は `/snapshot` の上限。**ディスクに書く 1 枚 (日次・起動・停止) の上限は末尾の `daily_max_bytes` = 6 MiB**。
  T29.16 で日次、T38.7 で起動と停止も。鍵の名前は日次だけだった頃のまま)
  を返す (T14.34。既定 30 日ぶん)。`/snapshots/<YYYY-MM-DD>` はその日のファイルを
  **そのまま** (`/snapshot` の応答そのもの) 返すので、`scripts/snapshot-diff.py` や
  `scripts/status-diff.py`、1 週間ぶんを 1 枚にする `scripts/weekly-report.py` に
  そのまま渡せる (置いていない日と日付として読めない名前は 404)
- **日次の 1 枚を書くのは履歴スレッドが UTC の日付をまたいだ瞬間の 1 回だけ**で (要求の経路の費用は 0)、
  名前は**終わった日** (前日) の日付。`PROXY_SNAPSHOT_DAYS` で日数を変えられ、`0` で止まる。
  `PROXY_STATS_PERSIST=off` (と `--lite`) では 1 ファイルも書かない (`dir` が `null`)。
  ディスクの空きが予算 (`PROXY_DISK_KEEP_FREE_MB` のマージン) を割り込むときは**書かずに**
  `/events` に 1 件 (`state_file`) 残す
- **ディスクに書く 1 枚 (日次・起動・停止) は、部を部ごとの 256 KiB で切らずに入れる** (T38.7)。
  `dns`・`hosts`・`profile` は全体の上限まで全部入れ、`recent` は日次の 1 枚が 1 MiB まで (T28.46。環の 2,000 件が入る)、
  起動と停止の 1 枚は 2,000 件全部 (T28.20・T38.7)。全体の上限は 3 つとも 6 MiB (T29.16・T38.7。スナップショットの
  `limit_bytes` に効いた上限が出る)。いつもの `/snapshot` と `/dns`・`/hosts`・`/profile`・`/recent` の口は部ごとの 256 KiB で切る
- **ディスクに書く 1 枚が全体の上限を越えるときは、全部入れた部を `profile` → `hosts` → `dns` → `recent` の順に 1 つずつ
  いつもの 256 KiB で切った形に戻し**、収まったらそこで止める。4 つとも戻しても越えるときに、いつもの順
  (`recent` → `log` → `history.5`) に `null` へ落とす (全部入れたせいで部ごと `null` にはしない。T38.7)
- `collect-deployed.sh --from-server` は取り寄せた 1 枚ごとに大きさ・上限に対する割合・部の `truncated` を並べ、
  **90% 以上なら警告**する (`scripts/snapshot-summary.py --size-check FILE…` と同じ 1 行。要約の頭にも同じ行が出る)
- **起動と停止の 1 枚** (T28.20) は同じ置き場所の `<YYYY-MM-DDTHHMMSSZ>-shutdown.json` (**停止シグナルで状態ファイルを書く前**) と
  `-start.json` (**起動から 60 秒**、`memory.baseline` を書くのと同じ時) で、`/snapshots` の **`moments[]`**
  (`name` / `kind` = `start`・`shutdown` / `bytes` / `t` (撮った時刻)、古い順。数は `moments_count`・`moments_bytes`) に並び、
  `/snapshots/<name>` で**そのまま**返る (日次の `files[]` とは別の並びなので、日付で取りに行く道具はそのまま使える)。
  締めの判定の「前」と「0 時間」(`collect-deployed.sh` の `BEFORE` / `ZERO`) をプロキシ自身が残すためのもので、
  `collect-deployed.sh --from-server` が取り寄せる
- 起動と停止の 1 枚が部を切らずに入れるのは、取り寄せた 1 枚は `offset=` で続きを追えず (取り寄せたときにはもうプロセスが居ない)、
  締めの判定の分母 (接続のエラーの行は `/recent` の試行、名前解決の行は `/dns` の答えのある名前) が欠けるため (T28.20・T38.7)。
  停止の 1 枚を組んで書くのは停止の後始末の頭で、手元で数 ms (後始末の上限は 3 秒)
- 起動と停止の 1 枚を残すのは種類ごとに `PROXY_SNAPSHOT_DAYS` 日以内かつ新しい方から
  `PROXY_SNAPSHOT_DAYS` 枚まで (再起動を繰り返しても置き場所を埋めない)

## S17 canary

利用者の要求が無い時間帯も待ちを測る canary の宛先・周期・IPv6 側。

経緯は [`specs/HISTORY.md`](specs/HISTORY.md) の「S17 canary」。

### canary

- プロキシ自身が **60 秒に 1 回**
  (`PROXY_CANARY_SECS`)、`PROXY_CANARY` で決めた宛先へ **名前解決 → TCP 接続 → 即 `close`** だけを行い、
  その時間を残す (T14.10)。**TLS も HTTP も送らない** (相手に届くのは 1 分に 1 回の名前解決と SYN / FIN だけ。
  利用者と同じ Happy Eyeballs を通るので、A と AAAA の両方を持つ相手には 250 ms ずらして
  もう 1 本 SYN が出ることがある)
- 利用者の要求が無い時間帯 (深夜や利用者が居ない日) も待ちを測り、
  「遅かったのはプロキシか、回線か、利用者の端末か」を切り分けるためのもの。canary の値が利用者の値と
  合っていれば回線 (またはリゾルバ)、合っていなければ利用者側、と読む
- 最後の 1 回は `/status` の `canary` (`mode` / `secs` / `runs` / `failures` / `at` / `host` / `dns_ms` /
  `connect_ms` / **`ipv6_connect_ms`** / `error` / `ipv6_runs` / `ipv6_skipped`)、`/metrics` の
  `sorahost_canary_seconds{stage="dns"|"connect"|"ipv6_connect"}` (最後の値。
  1 回も回っていなければ 1 行も出さない。**失敗した回は届かなかった段階が 0 になる**ので、
  成否は `/status` の `canary.error` と `canary.failures`、`/errors` で見る)
- 時系列は **`/history?res=5|60` の `canary`** で、
  `{"keys":["t","canary_dns_ms","canary_connect_ms","canary_host","canary_ipv6_connect_ms"],"samples":[[…]]}` という
  **別の配列** (`/history` の `keys` / `samples` とは別。`.rrd` の標本には書かないので
  再起動で消える。窓は 5 秒 × 720 と 60 秒 × 1,440)。失敗は `/errors` に `kind: "canary"` で 1 件だけ残り、
  利用者に返したエラーの集計 (`errors_by_cause`) には混ざらない
- 名前解決は**名前解決の表を通さず** OS に直接聞くので (リゾルバの実力を測るため)、`/status` の
  `dns.hits` / `dns.misses` にも keep-warm にも影響しない。TCP 接続は利用者と同じ経路
  (Happy Eyeballs と IPv4 優先の学習) を通る。回すのは **`canary` スレッド 1 本**だけで、
  利用者の要求の経路には 1 命令も足さない。**履歴スレッドが動いているときだけ回る**
  (`--lite` と `PROXY_STATS_PERSIST=off` では履歴スレッドごと止まるので canary も回らない)

### canary の IPv6 側

- `PROXY_CANARY_IPV6` (既定 on。T14.37): 同じ周期に、canary の名前の
  **AAAA へ 1 本だけ**繋いでみて `canary.ipv6_connect_ms` に残す (繋がれば ms、
  **繋がらなければ `null`**: AAAA が無い名前、`PROXY_IPV6=off`、経路が黒穴、`PROXY_CANARY_IPV6=off`)
- コンテナの IPv6 が黙って落ちる環境では、プロキシは 3 回続けて負けると初めて見るホストを
  IPv4 優先に切り替える (`/status` の `ipv6.v4_first`。S2)。この 1 本があれば「IPv6 が生き返ったか」を
  1 分の粒度で (利用者を待たせずに) 読める。**元に戻すかどうかもこの 1 本が決める** (T17.7):
  この 1 本が試している間 (直近 600 秒に 1 本、`ipv6.probe_by` が `"canary"`) は利用者の要求で
  600 秒に 1 回 IPv6 を先頭に戻す探りをやめ (探りの 1 本は利用者を Happy Eyeballs の間隔ぶん待たせるため)、
  **繋がったら `v4_first` を解く**。Happy Eyeballs も勝敗の記録 (`ipv6.attempts` / `wins` / `losses`) も
  ホストごとの族の記憶も動かさない
- 失敗は `/errors` にも残さない (黒穴のままだと 1 分に 1 件ずつ個票が埋まってしまうため。
  生死は `ipv6_connect_ms` が `null` かどうかで読む)
- `null` は「試して繋がらなかった」と「試さなかった」の両方なので、**試したかどうかは
  `canary.ipv6_runs` (IPv6 側を実際に試した回数) と `canary.ipv6_skipped` (試さなかった回数) で読む** (T18.1。通算で、
  canary の 1 回につきどちらかが 1 増える = `runs` と同じ歩み)。試さないのは、名前が引けなかった回・引けた答えに AAAA が無い回・
  `PROXY_IPV6=off`・`PROXY_CANARY_IPV6=off` で、試さなかった回は「探っている」印を更新しない
  (600 秒ぶん続くと探りは利用者の要求に戻り、`ipv6.request_probes` が増える)。宛先が 1 つも無い周
  (`PROXY_CANARY=off`、`auto` で直近 1 時間に CONNECT が無い) は `runs` ごと増えない

## S18 カーネルと cgroup の窓

バーストのときカーネル側で何が起きていたかを 5 秒ごとに残す窓。

- バーストのときカーネル側で何が起きていたかを読むため、プロキシの統計には残らない事象を、**5 秒の標本のときだけ** `/proc` と `/sys/fs/cgroup` を読んで
  メモリ上の窓 (5 秒 × 720 = 1 時間 と 60 秒 × 1,440 = 1 日。1 標本 200 B なので約 420 KiB、
  環状バッファの伸び方しだいで最大 600 KiB) に残す (T14.12)。**要求ごとには 1 回も読まない**。
  - **受け入れ待ち行列の溢れ** (`listen_overflows` / `listen_drops`)。溢れるとクライアントは SYN を
    1〜3 秒後に再送するので、**プロキシから見ると「遅い接続」としてすら残らない**
    (`/proc/net` の数はどれも**ネットワーク名前空間ごと**で、このプロキシの待ち受けのぶんだけではない)
  - **再送** (`retrans_segs` / `syn_retrans` / `tcp_timeouts` / `abort_on_timeout`)
  - **TIME_WAIT の本数** (`time_wait`) と TCP ソケットの数 (`sockets_inuse` / `sockets_alloc` / `curr_estab`)。
    手元で CONNECT のベンチを回すと `tcp_max_tw_buckets` (この機械は 32,768) に張り付く
  - **cgroup の CPU の絞り** (`cpu_nr_periods` / `cpu_nr_throttled` / `cpu_throttled_usec` と
    `cpu.max` の `quota_cores`)。CPU 上限を持つコンテナで「自分が遅い」のか「絞られて待たされた」のかが
    分かれる。**読むのは割合** (`cpu_nr_throttled` ÷ `cpu_nr_periods`) で、`cpu_nr_periods` がその分母
    (T15.0。「41 回絞られた」だけでは 41/8,123 = 0.5% なのか 41/41 = 100% なのか決まらない)。
    `/status` の `kernel.cgroup_cpu` には累計のほかに **`path`** (階層のどこの `cpu.stat` を読んでいるか。
    自分の cgroup に cpu コントローラが無いと**親の値**を読む。`--check` にも同じ道が出る) と
    **`since_start`** (`nr_periods` / `nr_throttled` / `throttled_usec` の**このプロセスが始まってからの
    増分**。累計はプロセスより長生きなので、起動直後のスナップショットにも `nr_throttled` が乗る) が出る。
    **`cgroup_cpu` と `since_start` の末尾の `usage_usec` / `user_usec` / `system_usec`** (T16.0) は同じ `cpu.stat` の
    CPU の使用量 (累計 us / 起動からの増分) で、**ユーザー空間とカーネル側**に分かれる (コンテナ全体。
    プロセスだけなら `/profile` の `cpu_user_us`)。行の無い `cpu.stat` では 3 つとも `null`。`/history` の `kernel` の
    末尾には同じものの増分が `cpu_user_usec` / `cpu_system_usec` で入る (1 分へは足し合わせ)
  - **PSI** (`psi_cpu_some_avg10` など。直近 10 秒のうち、その資源を待って進めなかった時間の割合 %)。
    隣のコンテナに CPU を取られている時間が読める
  - 一緒に取るもの: 名前解決のミスの回数とミス 1 回の ms、状態ファイルの書込エラー
- 累計のものは**増分** (その 5 秒に何回起きたか)、値のものは値で入る (1 分へ畳むときは
  増分は足し合わせ、値と PSI は**最大**)。最新の値と累計は `/status` の `kernel`
  (`tcp` / `cgroup_cpu` / `psi` / 直近 5 分の `last_5m`)、時系列は `/history` の `kernel`、
  Prometheus では `sorahost_kernel_listen_overflows_total` などの累計と
  `sorahost_kernel_time_wait` / `sorahost_cgroup_cpu_throttled_seconds_total` /
  `sorahost_cgroup_cpu_throttled_periods_total` ÷ `sorahost_cgroup_cpu_periods_total` (絞られた割合) /
  `sorahost_psi_some_avg10{resource="cpu"|"memory"|"io"}`
- **読めない源は `null`** (Linux 以外、`/proc/net` の無いコンテナ、cgroup v1、PSI 無しのカーネル)
- `.rrd` (状態ファイル) には書かないので**再起動で消える** (版 3 で標本 1 本の余白は 516 B = 64 項目に
  広がったので、入れるなら 26 列は収まる。移すかどうかは項目ごとに決める)
- 履歴の収集スレッドが動いていない `--lite` / `PROXY_STATS_PERSIST=off` では窓は空 (`kernel` は `null`)

## S19 ホスト別の時系列と 1 相手の説明 (`/hosts/series`・`/explain`)

上位ホストの 5 分 × 24 時間の時系列と、1 宛先・1 接続元を 1 枚に組む口。

経緯は [`specs/HISTORY.md`](specs/HISTORY.md) の「S19 ホスト別の時系列と 1 相手の説明 (`/hosts/series`・`/explain`)」。

### ホスト別の時系列 (`/hosts/series`)

- `/hosts/series?top=16` と `/hosts/series?host=<name>` は、
  **直近 1 時間の要求数で選んだ上位 16 ホスト**について、**5 分の窓 × 288 標本 (24 時間)** を返す (T14.22)。
  `/hosts` は**通算** (`avg_ms` は 3 日の平均)、`/history` は**全体**しか無いので、その間を埋める口
  (「datadog が 15 時に遅かった」「mtalk だけ夜に再送が増えた」を読む)
- 1 標本 = `count` (件数)・`ms_sum` / `ms_max` (CONNECT は**確立まで**、forward は**初バイトまで**の ms)・
  `dns_ms` (名前解決の合計 ms)・`errors` の 5 項目で、`keys` がその並び。標本は**古い順**で、
  `i` 番目の時刻は `t0 + i * window_secs` (`t` を 16 × 288 個並べると応答が 50 KB 太るので置かない)。
  ほかに `hour_requests` (直近 1 時間の要求数 = 順位そのもの)、`total` (`keys` と同じ順の合計。`ms_max` だけは最大)、
  `tracked` (いま系列を持っているホストの数)、`rotations` (窓を進めた回数) が出る
- **上位の入れ替えは履歴スレッドが 5 分ごと**に行い、要求の経路は「上位に居るか」の旗を見て
  配列 1 要素に 5 回足すだけ (鍵はホスト別統計が既に取っているもの。原子操作もシステムコールも増えない)。
  **上位から落ちたホストの標本は捨てる** (再び上がれば 0 から)。ただし**枠が余っている間はいまの住人を残す**ので、
  夜だけ動くホストの 24 時間が 1 時間の無音で消えることはない
- メモリは 16 ホスト × 288 標本 × 5 項目 × 8 B = **180 KiB 固定** (上位が 1 つ決まったときに 1 回だけ確保)。
  `.rrd` (状態ファイル) には**書かない**ので再起動で消える。履歴スレッドが動いていない
  `--lite` / `PROXY_STATS_PERSIST=off` では系列を 1 本も持たない (`series` は空。費用も 0)
- 応答はデプロイ先並みの値なら 16 本で約 96 KiB、他の個票と同じく 256 KiB 以下 (切ったら `truncated`)

### 1 相手の説明 (`/explain`)

- `/explain?host=<name[:port]>` と `/explain?client=<ip>` は、
  **1 つの相手についてほかの口を全部横断して 1 枚に組んだもの** (T14.36)。「この宛先はなぜ遅いか」を
  調べるのに `/hosts` `/dns` `/recent` `/clients` `/errors` を手で突き合わせる作業を、サーバー側で 1 要求にする
- `?host=` に入るのは、ホスト別の統計 (`/hosts` と同じ形。**ポートを書かなければ
  `connect://` と `http://`、ポート違いを全部まとめた合計**で、混ぜた鍵は `keys` に並ぶ。
  ポートを書けばそのポートの鍵だけ)、原因別のエラー件数 (`errors_by_cause`、名前つき)、
  **`/dns` の行そのもの** (warm か・残り TTL・答え・OS への問い合わせ回数)、
  そのホスト宛ての**閉じた接続の個票 10 本** (`/recent` と同じ 1 件。段階の ms と閉じた理由つき)、
  **エラーの個票 5 件** (`/errors` と同じ)、上位 16 ホストに居れば**直近 1 時間の時系列 12 窓**
  (`/hosts/series` の末尾。居なければ `null`)、**直近の異常 3 件** (`/events` の `anomaly`。
  これはプロキシ全体の判定で、相手ごとではない)
- `?client=` は `/clients` の行 + その接続元の個票 10 本 + エラー 5 件 + RTT (利用者 → プロキシ)
- **末尾に 1 行の判定 `summary`** が付く — 日本語で
  「61 要求 (CONNECT 17 / forward 44)。所要の平均 1.0 ms = 名前解決 0.2 + 接続 0.3 + プロキシ側 0.5 ms。
  p50 0.6 / p95 8.9 ms。カーネルの RTT 0.4 ms (標本 17、再送 0)。…」のように**段階の数字を並べるだけ**で、
  「だから RTT だ」の**推定は書かない** (読む人がする)。同じ値は機械が読める `numbers` にも入る
  (`avg_ms` = `dns_ms` + `connect_ms` + `proxy_ms`。平均は時間を測れた要求 `timed` で割った値。
  引き算で出る `proxy_ms` は、CONNECT だけの相手なら**プロキシ側の待ち**だが、forward が混ざる相手では
  **オリジンが考えていた時間と本文を流した時間も入る**ので、文章の方もそう呼ぶ)
- **知らない相手でも 200 のまま `"known":false`** を返す (「どこにも記録が無い」こと自体が答えなので)。
  `?host=` も `?client=` も書かなければ 400。応答は **64 KiB 以下** (個票 10 本・エラー 5 件の上限つき)
- **新しい記録は 1 つも増やさない** (この口に来たときだけ既にある表とリングを読んで組み立てるので、
  要求の経路の費用は 0)。重い口なので同時に 1 本だけ組む (S4 の「重い口」)

## S20 日次の要約と SLO (`/daily`・`/slo`)

1 日 1 行の要約を永久に残す規則と、SLO の閾の達成率の出し方。 本文は [`specs/S20.md`](specs/S20.md)。

## S21 `/history`

3 解像度の履歴の列、末尾に足した列、期間を畳んだ 1 行 (`summary=1`)。 本文は [`specs/S21.md`](specs/S21.md)。

## S22 `/profile`

1 要求の時間がどの段階に消えたか、CPU がどの役割のスレッドに使われたか、ロックの取り合い。

測った数字は [`specs/HISTORY.md`](specs/HISTORY.md) の「S22 `/profile`」。

- **`/profile?res=5|60`** は「**1 要求の時間がどの段階に消えたか**」「**CPU がどの役割のスレッドで何をして使われたか**」
  「**ロックの取り合いと待ち行列の待ち**」を 1 枚で返す (5 秒 × 720 = 1 時間 と 60 秒 × 1,440 = 1 日 の窓。
  ダッシュボードの「プロファイル」の節がこれを描く):
  - **段階** — CONNECT は `queue` (accept してからワーカーが動き出すまで) / `client_read` (要求行を読んでから `Host` まで) /
    `dns` / `connect` / `first_relay` (`200 Connection Established` を書いてから最初の中継バイトまで =
    トンネル越しの TLS 握手の往復) / `relay` / `park` の 7 段、転送は `queue` / `client_read` / `origin` /
    `send` / `ttfb` / `body` の 6 段。段階 1 つにつき 件数・合計 ms・最大 ms・12 段の区間 (`/history` と同じ 1〜5,000 ms)
  - **スレッド** — 役割ごとの CPU と「走行中 / どのシステムコールで待っているか / 休眠」の割合 (`PROXY_PROFILE_SAMPLE_MS`)。
    **状態の割合は「どこで待っているか」で、「どこで CPU を使っているか」ではない**:
    `/proc/<tid>/syscall` はスレッドが CPU に乗っている間は中身に関係なく `running` を返すので、
    カーネルの中で回り続ける処理 (`splice` の中継など) は `running` に出る。CPU の行き先は役割ごとの CPU を見る。
    また `conn` 役には**次の仕事を待っているワーカー**も入る (`futex`)
  - **`threads_top` (どのスレッドが回っているか)** — その窓で CPU を多く使ったスレッドを**最大 8 本**、
    `[tid, "comm", roles の添字, cpu_us, その窓に走行中だった標本の数]` で。役割ごとの集計では
    「`conn` 役の `running` がいつも 2.0 本」までしか読めず、**どのスレッド**かが出ないので、
    空回り (回り続けているのに何も進んでいないトンネル) を tid と名前で指すための欄。
    何もしなかったスレッドは並べない (1 本も無い窓は `0` 1 文字)
  - **`run_delay_us` (走れずに待った時間)** — 役割ごとに「走行可能なのに CPU に乗れなかった」時間の
    その窓の増分 (us。`/proc/self/task/<tid>/schedstat` の 2 番目)。CPU の絞り (cgroup の quota) と
    隣のプロセスとの取り合いがどちらもここに出るので、**確立の尾がプロキシのせいか機械のせいか**を
    切り分けられる。`CONFIG_SCHEDSTATS` の無いカーネルでは `null` (`capabilities` の `proc_schedstat` が偽。S13)
  - **`user_us` / `cpu_user_us` (ユーザー空間の CPU。T16.0)** — `keys` の末尾の 2 列。`user_us` は `roles` と同じ長さの配列で
    役割ごとの、`cpu_user_us` はプロセス全体の、その窓の CPU のうち**ユーザー空間** (utime の増分 us)。
    **カーネル側は `threads` の `cpu_us − user_us` / `cpu_us − cpu_user_us`** で出す (sys は持たない)。
    中継の CPU は大半がカーネル (`splice` / `poll`) なので、ビルドの設定のようにユーザー空間にしか効かない変更は
    合計では差が埋もれる。`recent` と `?summary=1` の各段の末尾にも `cpu_user_us` が付く
  - **CPU/要求** — 窓の CPU (utime + stime の増分) ÷ 窓の要求数。`scripts/cpu-per-request.sh` と同じ物差しの値が
    デプロイ先でも読める (`recent.cpu_per_request_us` は直近 5 分)
  - **ロック** — 統計の表・名前解決の表・預かり所・ワーカーの 4 つが「待たされた」回数と、待ち行列で待った件数 / 合計 ms / 最大 ms
  - 記録はプロセスのメモリだけ (約 6.9 MB = 1 標本 3,216 B × (720 + 1,440)。状態ファイルには書かない)。応答は 256 KiB 以下で、
    入り切らないときは**新しい標本を残して**古い方から落とし `"truncated": true` を出す。**`--lite` では `{"profile":"off"}`**
  - **`?n=` と `?offset=` (T15.0)** — 応答の 256 KiB に対して標本 1 本の JSON の行が太いので、
    1 枚に全部の標本は入らない (入る本数の実測は `specs/HISTORY.md`。メモリの見積もりの 3,216 B は JSON の行の長さとは別の数)。
    `n=` が 1 枚に返す標本の数 (既定はその解像度の全部、上は 720 / 1,440)、`offset=` が**新しい方から何本飛ばすか**。
    応答の末尾に `n` / `offset` / `next_offset` が出て、`next_offset` が `null` ならそれで終わり。
    **時刻では引けない** (`since=` / `until=` は無い) — 環は新しい順に詰めるだけの作りで、
    標本の `t` は 1 つ目の欄にあるので、読む側で切る
  - **`?summary=1`** — **標本を 1 本も返さず**、`5m` / `1h` / `all` の 3 段に畳んだ
    `requests` / `cpu_us` / `cpu_per_request_us` (と畳んだ標本の数 `samples`) だけを返す。
    **これだけは重い口ではない** (`/snapshot` を組んでいる最中でも 503 にならない。S4 の「重い口」) ので、
    「いまこの機械が 1 要求に何 us 使っているか」を 5 秒おきに引く監視はこちらを使う
  - 段階の窓は **ms 刻み**なので、loopback のように 1 要求が 1 ms に満たない環境ではほとんどの段階が 0 に潰れる
    (これはデプロイ先の 6〜30 ms の待ちを読むための道具。手元の速さを見るなら `cpu_per_request_us` の方)
  - **Prometheus で長く見るなら** 同じ段階が `/metrics` の
    `sorahost_stage_seconds{kind="connect"|"forward",stage=...}` にも**起動からの累計**で出る
    (`/profile` の窓は 1 時間 / 1 日で古い標本から落ちる)。ロックの取り合いは
    `sorahost_lock_contention_total{lock=...}`

## S23 配布物とビルドの設定

配布に musl 静的リンクを採らない理由と、`release` / `dist` のプロファイルの設定。

決めたときに測った数字は [`specs/HISTORY.md`](specs/HISTORY.md) の「S23 配布物とビルドの設定」。

### musl 静的リンクを採らない

配布物は glibc に動的リンクする。musl の静的リンクは「1 ファイルでどこにでも置ける」が、測って採らないことにした:

1. **遅い**: forward の 8 並列が glibc の約 1/3 になる
2. **TLS が使えない**: `dlopen` が機能しないので `libssl` を実行時に読み込めず、最初の `https://` オリジンのときにログに
   `TLS: libssl not found` が出る。`https://` オリジンの取得とキャッシュが無効になる
   (`CONNECT` トンネル = ブラウザの HTTPS は影響なし)
3. **名前解決が NSS を通らない**: musl は `/etc/resolv.conf` だけを見る (systemd-resolved / mDNS が効かない)
4. **ビルドが 120 MB に収まらない**: 静的リンクにしてもビルドが軽くなるわけではない

### プロファイルの設定

| | `release` (既定) | `dist` (配布用) |
|---|---|---|
| `opt-level` | `3` | `"s"` |
| `lto` | `false` | `true` (fat) |
| `codegen-units` / `strip` | `4` (全クレート) / あり | `1` / あり |
| ビルドが通る最小のメモリ | **約 90 MB** (手元。CI では未測定) | 350 MB 以上 |

- **`release` は LTO を切る**: `cargo build --release` を **メモリ 120 MB のコンテナで通す** ため (S25)。
  LTO の重さは最終リンクの 1 回にかかり、クレートを割ってもそこは小さくならない。`fat` は効くが 120 MB の約 3 倍のメモリが要り、
  `thin` はバイナリも速さもほとんど変わらない。配布バイナリだけ `dist` (fat LTO) で作る
- **`codegen-units` は全クレート `4`** (T15.12): ビルドの山はコード生成 (LLVM) の段で、
  `codegen-units = 1` はクレート全体の LLVM IR を 1 つのモジュールとして一度に抱える。
  **4 より上げても減らない** (コード生成の前の段 = 型検査とメタデータの床が約 80 MB)。
  `codegen-units` を増やすとクレートの中でのインライン化の範囲が狭まるが、手元もデプロイ先も共有の機械で
  CPU/要求 が数 % ぶれるので手元の A/B は取らず、**デプロイ先で「桁で悪くなっていないか」だけ**を見る。
  悪くなっていたら `Cargo.toml` の `[profile.release]` の `codegen-units` を `1` に戻す 1 行で戻る
- `dist` は `codegen-units = 1` を明示する (release の `4` を継ぐと fat LTO でも `.text` が 16.9 KB 太るため)
- **最適化レベル**: `release` は `opt-level = 3`、`dist` は `"s"` (`dist` はサイズを優先)。
  `dist` は `release` に対して CPU 以外はほぼぶれの中だが、バイナリが小さいので配布は `dist` にする
- いまのバイナリの大きさと速さは `README.md` の「性能」

## S24 完了の定義 (速さの約束)

ゴール (「バイナリを 1 つ置いて起動するだけで使える、認証なしの HTTP/HTTPS(CONNECT) プロキシ」を、同じ条件でこれ以上速くならないところまで速くする。`docs/goals.md`) に対して、満たしたことを確かめた約束。数字は手元の loopback で、測り方は `docs/measuring.md`、いまの表は `docs/status.md`。

経緯は [`specs/HISTORY.md`](specs/HISTORY.md) の「S24 完了の定義 (速さの約束)」。

- [x] タスク 0〜1 と T2.1・T2.2・T3.1・T3.2・T3.4 が完了、README の性能節に Rust ベンチの値がある
- [x] forward 8 並列 p50 が 1 ms 台 (loopback) — 実際は **0.18 ms**
- [x] **トンネル 1 本の CPU が 200 us/MiB を切る** — **185.5 us/MiB** (user 11.5 / kernel 174.0。ほぼ全部 `splice`)。
  同じ条件のスループットは **3,960 MiB/s (3.87 GiB/s)** で旧条件「1.5 GiB/s 以上」も満たすが、
  **この MiB/s はベンチ律速**なので完了条件には使わない (プロキシは 1 コアの 73% しか使わず、飽和するのはベンチの blaster)。
  測り方は `scripts/cpu-per-request.sh --only tunnel --conc 1` (プロキシ cpu4-5 / ベンチ cpu6-7、10 秒 × 3 回の中央値、T10.8)
- [x] 既定設定で `cargo test --workspace` 全通過 (T9.6 時点で 189 単体 + 48 結合)
- [x] `rust-http-proxy --lite -p 8080` の 1 行で「認証なし・手軽・最速」

## S25 守ること (認証なし・外部クレートなし・ビルドのメモリ・動作環境)

このプロキシとそのビルドが守る約束 (非目標も含む)。作業の決まり (1 ファイルの行数・文体・`$HOME` に書かない・権限を上げない) は `docs/goals.md` にある。

経緯は [`specs/HISTORY.md`](specs/HISTORY.md) の「S25 守ること (認証なし・外部クレートなし・ビルドのメモリ・動作環境)」。

- **認証は入れない**。`Proxy-Authorization` を要求するコードは書かない (履歴上 `dad8984 refactor: remove authentication` で意図的に外した)。
- **外部クレートは追加しない**。依存してよいのはこのリポジトリのコードだけ (`crates/` のワークスペースメンバー)。
  crates.io のクレートは 1 つも入れない (`libc` も入れない)。
  システムコールが必要なら `crates/sys/src/sys.rs` (`poll` / `epoll` / `splice` / `recv`)、`crates/sys/src/signal.rs`、
  `crates/sysinfo/src/sysinfo/inotify.rs`、`crates/tls/src/tls.rs` (`dlopen`) と同じく `unsafe extern "C"` で直接宣言し、
  `#[cfg(target_os = "linux")]` で囲んで他 OS には従来コードへのフォールバックを残す。
- **既存機能を壊さない**。`cargo test --workspace` は常に全通過。キャッシュ・ダッシュボード等は
  「使わないときに一切コストがかからない」ようにするのが方針で、削除はしない。
- Rust は `edition = "2024"`、stable の `rustc` で動くこと (版は固定しない)。`cargo clippy --workspace --all-targets` の警告を増やさない。
- **`cargo build --release` はメモリ 120 MB で通ること** (動作環境のコンテナが小さい。T17.11 で 120 MB にした。
  手元の通る最小は 90 MB)。`scripts/build-memory.sh` が CI で見張っている (実際にその cgroup に入れてビルドする)。
  効くのは 2 つだけ: **クレートを小さく割ること** (`rustc` はクレート単位で全部を抱える。
  **ファイルを割っても下がらない**) と、**`jobs = 1`** (`.cargo/config.toml`。並列に走る rustc の
  合計が上限を超えるため)。
- 動作環境は Linux (Pterodactyl コンテナ) が主。`SERVER_PORT` / `SERVER_MEMORY` / `$HOME/.env` の扱いは変えない。
