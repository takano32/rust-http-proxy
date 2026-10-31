# 性能の測り方とデプロイ先の数字

> README の「性能」の節の続き (T.29.32 で README から移した。本文は移す前と同じ)。loopback の表と測り方の要点は
> [README の「性能」](../README.md#性能) にあります。ここは置いた先での測り方 (`PROXY_SELF_BENCH`)・デプロイ先の数字と
> 判定表の定義の移り変わり・手元での再現 (`memory-replay.py`・`deployed-like.sh`・`bench --only replay`)・トンネルの MiB/s の注です。
> 文中の「下の」「上の」は、移す前の README の中での位置を指しているものがあります。

## 置いた先の CPU/要求 を同じ物差しで測る (`PROXY_SELF_BENCH=on`)

上の表は**この機械の big コア**の値です。置いた先 (小さなコンテナ、別の CPU) で 1 要求が何 us かは、
実トラフィックからは読めません (0.015 req/s では 5 秒の窓に 0〜1 本しか入らないため)。
`PROXY_SELF_BENCH=on` で起動すると、**待ち受けを開いた直後に loopback だけで**
forward 8 並列と CONNECT 8 並列を打ち、同じ指標 (CPU/要求 と CPU/本) を `/status` の `self_bench` に残します。
**外へは 1 バイトも出しません** (相手はこのプロセスの中に立てた 1 KiB のオリジンと、すぐ閉じる sink)。

```bash
PROXY_SELF_BENCH=on ./rust-http-proxy            # 起動ログに 1 行出て、3 秒で終わる
curl -s localhost:8080/status | grep -o '"self_bench":{[^}]*}'
# {"at":1789558422,"forward_us":42.6,"connect_us":140.0,"cores":4,"requests":20000,
#  "connects":2000,"secs":3,"note":"stopped early at the caps (20000 requests, 2000 CONNECTs)"}
```

手元 (既定プロファイル・ログ `warn`・cpu4-7 に固定) では、この `forward_us` は **37〜43 us** に出ます
(上の表の forward 8 並列 **41.4 us** と同じ桁)。ただし `scripts/cpu-per-request.sh --only forward` を
**同じ日に回した値より 17〜26% 低め**に出ます。低いのは、自己ベンチが自分の要求を統計に載せない
(= ホスト別・接続元別の鍵を取らない) ぶんと、打ち手が同じプロセスの中にいてループバックの往復が
キャッシュに乗るためです。**比べるときは「自己ベンチどうし」で**比べてください: 置いた先の
`forward_us` を手元の `forward_us` で割れば、「そのコンテナの CPU は手元の big コアの何倍遅いか」が
1 つの数字になります (同じコードの同じ経路を同じ測り方で測っているので、この比は読めます)。

読むときの注意:

- **本数に上限があります** (forward **20,000 要求** / CONNECT **2,000 本**)。速い機械では 3 秒より早く、
  先に当たった方で終わります (そのとき `note` に `stopped early at the caps` と出ます)。
  上限があるのは、上限なしだと 1.5 秒で CONNECT を 22,000 本張ってしまい、**カーネルに TIME_WAIT が
  44,000 本残る**ためです。`secs` は**回す秒数の上限**で、実際にかかった時間ではありません。
- **自己ベンチのぶんは統計に 1 件も入りません**: `/status` の `total_requests` / `bytes_forwarded`、
  `/hosts`、`/clients`、`/recent` (閉じた接続の個票)、`/connections`、`/history` のどれにも載せません
  (載せると、静かなプロキシでは起動直後の 3 秒が 1 日ぶんの統計を塗りつぶしてしまいます)。
  残るのは `/status` の `self_bench` と `/events` の 1 行だけです。
- **測る 3 秒だけログ水準を `warn` に下げます** (既定の `info` のままだとアクセスログが数万行出て、
  1 行 7.2 us が CPU/要求 に乗ってしまうため)。終わったら元の水準に戻します。
- `PROXY_ALLOW_LOCAL=off` (既定) のままで構いません。**自己ベンチが使う 2 つのポート宛てだけ**を
  3 秒間だけ判定から外します (穴はそれ以外に開きません)。
- 既定は `off` で、そのときは起動時の分岐 1 つ以外**何も走りません**。置いた先では
  **再デプロイの直後に 1 回だけ** `on` にして数字を取り、`off` に戻す使い方を想定しています。

## デプロイ先 (実際に使っているプロキシ) の数字

上の表は loopback = 「同じ機械の中でどこまで速いか」です。**利用者が実際に待つ時間**は
別に測っています (Pterodactyl のコンテナに置いた 1 台。要求の 99% が CONNECT、2026-10-25 の tokyo は 0.27 req/s)。
**2026-10-08 に本番を `nagoya.sorahost.net:50697` から `tokyo.sorahost.net:60357` へ切り替えました**。下の「いまの数字」は tokyo、そのあとの段落と比較の表は切り替え前の nagoya のものです。
道具は `/status` `/history?res=3600` `/metrics` と `scripts/status-diff.py` / `scripts/probe-deployed.sh` で、
**ホスト別の平均は 2 枚の差分で読みます** (`/status` の `hosts[]` は状態ファイルに残って再起動をまたいで
通算されるので、そのまま読むと直す前の値が何日も混ざります)。
**道具はどれも `scripts/` に置いてあり、この README が `scripts/<名前>.py` と書いているものは実行ビット付き**
なので、そのまま `scripts/snapshot-diff.py a.json b.json` と打てます (`proxydata.py` は共通部品、
`snapshot_criteria.py` は `snapshot-diff.py` の判定の部、`test_*.py` は unittest が名前で読むものなので
実行ビットは付けません)。**この区別は `scripts/check-docs.sh` の (f) が毎回見ます** (T.24.7)。

**要求の枠組み (`Content-Length` / `Host` / 折り返し) を実機で流すなら `scripts/framing-probe.py`** (T.19.4)。
`python3 scripts/framing-probe.py ./target/release/rust-http-proxy` で、オリジンとプロキシを自分で起こして
10 通り (正常・CL の重複同値/不一致・21 桁・カンマ・負・数字でない・`Host` 2 本・obs-fold の空白/タブ) を流し、
状態コードと**オリジンが受け取ったヘッダー**を並べます。1 つでも期待と違えば exit 1 なので、直したことの
確かめと、次の退行の見張りに使えます。
T.27.9 で 2026-10-24 に見つけた形も足しました (要求 19 通り・応答 2 通り・CONNECT 4 通り): 名前とコロンの間の空白と
値の CR / NUL は 400 (T.27.5)、`Transfer-Encoding` の最後がちょうど 1 回の `chunked` でない・HTTP/1.0 の TE は 400
(`gzip, chunked` は通る。T.27.6)、応答の値の CR / NUL は SP に置き換えて返す (T.27.7。値に UTF-8 として読めないバイト (obs-text) のある応答も、0.28.12 からは 502 にせずそのバイトを U+FFFD にして返す。T.28.12)、`Max-Forwards: 0` の
TRACE / OPTIONS は自分で応え 0 でなければ 1 減らす・URI の userinfo は 400・`#fragment` は落とす・`HTTP/1.x` でない版は 505・
CONNECT のポートなし / 0 は 400・読めない chunk の大きさは 400 (T.27.8)。0.27.1 ではこのうち 24 行が外れます
(値の CR で区切りがずれたオリジンの接続がプールに戻り、次の無関係な要求まで無応答になる形も出ます)。
応答の 2 形も足しました (T.28.1): オリジンが**最後が `chunked` でない `Transfer-Encoding`** (`gzip`・`xchunked` など) と
`Content-Length` を一緒に返したら、CL を捨てて**接続が閉じるまでを本文**にし (RFC 9112 §6.3)、オリジンの接続は
使い回さず、キャッシュにも保存しません (0.27.8 までは CL で区切って TE を落とし、接続を使い回して保存もしていました)。
`chunked` と CL が両方なら今までどおり `chunked` です。
**その応答の TE の行はクライアントへ伝えます** (T.28.35。RFC 9112 §6.1): 符号化 (`gzip` など) はほどかずにオリジンの
バイト列のまま渡すので、HTTP/1.1 のクライアントへは**オリジンの `Transfer-Encoding` の並びを 1 行に畳んで残し**
(`chunked` で包み直さず)、`Connection: close` で閉じて区切ります。HTTP/1.0 のクライアントへは TE を送れず、
符号化もほどけないので **`502 Bad Gateway`** にします (`/errors` の原因は `other`)。T.28.1 の時点では TE の行を落として
`chunked` で包み直していた (HTTP/1.0 へは TE なしで閉じて区切っていた) ので、クライアントには符号化済みのバイトが
TE なしの本文に見えていました。framing-probe にこの 2 形を足してあります。
**要求行とヘッダー名の文法も見ます** (T.29.8。RFC 9112 §3・9110 §5.1): 要求行は**ちょうど 3 要素**
(メソッド・対象・版。区切りの空白の数は問わない)、対象は VCHAR (0x21〜0x7E) だけ、版は `HTTP/d.d` の形で、
外れたら `400` (理由は `request_line`)。版の無い行 (HTTP/0.9 の形) も 400 です。形が合っていて `1.x` でない版は今までどおり 505。
ヘッダーの名前は空でない token だけで、名前の中の空白・制御文字・`"` など、空の名前、コロンの無い行は `400`
(理由は `bad_header`。CONNECT の入口も同じ)。0.29.35 までは `GET http://h/a b HTTP/1.1` を 505 (`b` を版と読んだ)、
`… HTTP/1.1 junk` と版の無い行を 1.0 として受け付け、対象の DEL / NUL をそのまま転送し、コロンの無い行は黙って捨てていました。
framing-probe に 8 行足してあります。
小物 3 つ (T.29.9): (1) 空のパスの `OPTIONS` (`OPTIONS http://h:P`) はオリジンへ **`OPTIONS *`** で送り (RFC 9112 §3.2.4。
今までは `OPTIONS /`)、`OPTIONS *` + `Host` は `Host` のオリジンへ `*` のまま送ります (今までは `*` をホスト名として引いて 502)。
`*` は OPTIONS 以外 `400`。(2) **`Date` の無い応答には転送のときに今の時刻の `Date` を付け** (RFC 9110 §6.6.1。
ある応答はオリジンのまま)、自前の応答 (400 / 408 / 414 / 431 / 505 / 417 / 502 / 504 と `Max-Forwards: 0` の 200) にも付けます。
保存から返す応答 (HIT) は保存した先頭のまま、ダッシュボードなど内部の口は対象外です。(3) `Upgrade` を頼んでいない要求に
オリジンが `101 Switching Protocols` を返したら `502` にします (RFC 9110 §15.2.2。今まではそのまま返していました。
`Upgrade` のある要求には今までどおり 101 を返します。プロトコルの切り替えはしません)。
オリジンの 1xx の中間応答 (`103 Early Hints` など。100 と 101 は除く) はクライアントへ中継しますが (T19.6)、
**HTTP/1.0 のクライアントへは送らず読み捨てます** (T.29.3。RFC 9110 §15.2 の MUST NOT。1.0 のクライアントは 103 を
最終応答と読む)。framing-probe の「HTTP/1.0 + 103」の行で見ます (0.29.35 では外れる)。
HEAD の応答も GET と同じ TE の行を返します (T.28.36。最後が `chunked` の並びは `Transfer-Encoding: chunked`、そうでない並びはそのまま。HTTP/1.0 へは落とす)。
要求の `Expect` も 2 形 (T.28.3): 本文つきの HTTP/1.1 の要求の **`Expect: 100-continue` (大小無視) には、本文を読む前に
`HTTP/1.1 100 Continue` を 1 回返し**、`Expect` の行はオリジンへ転送しません (オリジンの 100 は今までどおり読み飛ばします)。
`100-continue` 以外の期待は `417 Expectation Failed` で閉じます。本文の無い要求と HTTP/1.0 の要求には 100 を返しません。
0.27.8 までは 100 を返さなかったので、100 を厳密に待つクライアントは締め切りまで本文を送らずに止まっていました。
**要求の本文を送る途中でオリジンが応答を返して閉じたら、その応答を渡します** (T.29.7。RFC 9112 §9.5):
本文の書き込みが `EPIPE` / `ECONNRESET` で落ちたときだけ、届いている応答 (`413 Payload Too Large` など) を読みに行き、
読めればそのまま返して `Connection: close` で閉じます (クライアントの本文の残りは読んでいないため)。読めなければ
今までどおり `502`。0.29.35 までは 8 MiB の POST に先頭だけ読んで 413 を返すオリジンでも 502 でした。
正常な要求のシステムコールは増えません (失敗したときだけ通る道)。
**応答の先頭と chunk の枠の行にも長さの上限があります** (T.29.1。要求の先頭は前から 1 行 64 KiB・合計 128 KiB で 414 / 431):
応答の先頭 (状態行 + ヘッダー) は合計 **128 KiB** を越えたら `502 Bad Gateway` (原因は `other`、やり直さない)、
chunk の大きさの行 (拡張込み) とデータの後ろの行は **4 KiB**、trailer は 1 行 **8 KiB**・合計 **64 KiB** を越えたら、
クライアントの本文なら `400`、オリジンの本文なら先頭はもう出ているので**終端の chunk を出さずに閉じます**
(WARN は 1 分に 1 行)。0.29.35 までは無制限で、空行を送らずにヘッダーを流し続けるオリジン (宛先はクライアントが選べる)
に 300 MB 流されると RSS が 18 → 907 MB、改行なしの `f` を chunked の POST で 796 MiB 送ると +793 MiB になりました。

**1 回で全部取るなら `scripts/collect-deployed.sh HOST:PORT [DIR]`** (T.14.4)。`/snapshot` を
`DIR/<UTC 時刻>-snapshot.json` (既定はリポジトリの `status/`。`.gitignore` 済み) に保存し、要点
(`scripts/snapshot-summary.py`)・ホスト別 (`status-diff.py`。**前回の雪像があれば差分**)・
ダッシュボードの読み方 (`check-dashboard.js`)・手元から見た待ち (`probe-deployed.sh`) を
続けて回して **Markdown 1 枚**を標準出力に出します (同じものを雪像の隣の `<時刻>-collect.md` にも残し、
雪像に入らない `/daily` `/healthz` `/slo` `/config` `/profile?res=60` も `<時刻>-<口>.json` で隣に置きます。
`/profile?res=60` は 1 枚 (256 KiB) だと 24 時間のうち約 5 時間しか入らないので、`--full` が無くても `next_offset` が `null` になるまで `offset=` で追い、`samples` を 1 つに繋いで `truncated` を `false` にしたものを置きます。枚数の上限は `MAX_PAGES`。
**雪像の `recent` と `hosts` も、256 KiB で切れていれば既定で `offset=` で追い、雪像の同じ部に行を足して置き直します** (T.22.5。T.27.1 から `dns` と `profile` も。`/dns` の `offset=` は 0.27.1 からで、それより前の版では 1 枚ぶんのまま要約にそう書きます。形は `/snapshot` のままなので、下の道具はどれもそのまま全部を読みます。2026-10-17 の tokyo では 1 枚に `recent` 2,000 件中 576 件 (33 分ぶん)・`hosts` 1,001 件中 574 件しか入りませんでした)。上限は部ごとに `MAX_PAGES` 枚と、続きを引く時間の合計 `FOLLOW_SECS` (既定 60 秒) で、越えたら止めて要約にそう書きます (`truncated` と `next_offset` は残りを指したまま)。`FULL=0` か `--no-follow` で前のとおり追いません。
さらに、下の `anonymize-snapshot.py` で名前・IP・UA だけを置き換えた**匿名化した写し** `<時刻>-snapshot.anon.json` と、同じ表で通した `<時刻>-daily.anon.json` `<時刻>-profile_res_60.anon.json` も隣に置きます (T.17.16。`ANON=0` で置きません)。テストへ持ち込むときはこの写しを `scripts/testdata/` へ移します)。`status-diff.py` は `/snapshot` の JSON を
そのまま読めるので、保存したファイルを 2 つ渡せばいつでも差分が取れます:

```bash
scripts/collect-deployed.sh tokyo.sorahost.net:60357 status/tokyo > today.md   # 1 日 1 回 (保存先はホストごとに分ける)
PROBE=0 scripts/collect-deployed.sh tokyo.sorahost.net:60357 status/tokyo      # 本物の要求を送らずに取る
scripts/collect-deployed.sh --from-server tokyo.sorahost.net:60357 status/tokyo  # 回し忘れた日を取り寄せる
scripts/collect-deployed.sh --closeout tokyo.sorahost.net:60357 status/tokyo     # 締め: 取り寄せ → 前と 0 時間を版で選ぶ → 本番 → 前提 (T.29.26)
scripts/collect-deployed.sh --full tokyo.sorahost.net:60357 status/tokyo       # 切れた部の続きも `offset=` で追う
scripts/collect-deployed.sh --no-follow tokyo.sorahost.net:60357 status/tokyo  # 切れた `recent`・`hosts`・`dns`・`profile` を追わない (T.22.5 より前の既定)
VIA=$HTTPS_PROXY scripts/collect-deployed.sh tokyo.sorahost.net:60357 status/tokyo  # CONNECT しか通さないプロキシの内側 (クラウドのセッション) から。探りは飛ばす (T.27.2)
scripts/status-diff.py status/*-snapshot.json    # 最初と最後で差分
scripts/status-diff.py status/*-snapshot.json --group domain  # eTLD+1 でまとめる
```

**回し忘れても個票は残ります**: プロキシ自身が 1 日 1 回 (UTC 0 時) `/snapshot` を
`$HOME/.rust-http-proxy/snapshots/<日付>.json` に書いているので (既定 30 日ぶん。下記
`PROXY_SNAPSHOT_DAYS` と `/snapshots`)、`--from-server` を付けると **`/snapshots` の一覧のうち
手元に無い日付だけ**を取り寄せて `DIR/<日付>T000000Z-snapshot.json` に置きます
(中身は `/snapshot` そのものなので、`status-diff.py` も `snapshot-diff.py` も
`weekly-report.py` もそのまま読めます)。
**起動と停止の 1 枚** (T.28.20。下記 `/snapshots` の `moments`) も手元に無いものだけ取り寄せて
`DIR/<UTC 時刻>-shutdown-snapshot.json` / `-start-snapshot.json` に置き、**最後の停止とその後の最初の起動**を
`BEFORE=… ZERO=…` の形で 1 行出します (締めの「前」と「0 時間」を撮り忘れても、再デプロイのあとにこれで埋まります)。

要点 (`snapshot-summary.py`) の**頭には「無言の再起動」の 1 行**が付きます (T.23.2): `/events` の `start` の直前が `shutdown` でない起動 (落とされたか、止め方が違う) を時刻・前の起動の長さ・版つきで数え、0 件なら「0 件 (直前が `shutdown` の起動 N 件)」、`/events` の部が無い雪像では「判定できず」と書きます
(2026-10-20 の tokyo は 2026-10-17 22:14:52Z の 1 回で、前の起動は 19.2 時間。10-15 の雪像は 0 件)。同じ判定は `--criteria phase23` の判定表にも 1 行で入ります (行を足すのは T.23.1。判定の関数は要点と同じ `scripts/snapshot_criteria.py` の `silent_restarts()`)。

要点 (`snapshot-summary.py`) の最後には 2 つの表が付きます (T.17.0c): **`/events` の anomaly を種類別に「起動からの件/時」**
(`cleared:` は数えず、状態ファイルから読み戻した起動より前の出来事は外します) と、**接続元の見張り** — 新しく現れた接続元
(`--prev` があれば前の `/clients` に居ないもの、無ければ `first_seen` が起動より後)・IP リテラル宛ての要求・443 / 80 以外への
CONNECT (`/clients` の起動からの通算と、`/recent` の窓の中の数)・`/events` の `new_client`・**`/readers` に居て `/clients` に居ない
接続元** (プロキシを使わず `GET /` などで内部の口だけを引いた走査)。認証なしのプロキシで実際に起きる危険は乱用なので、その手がかりを
1 か所に集めています。IP とホスト名はそのまま出ます。

保存先は**リポジトリの外**にしてください (個票には接続元 IP と宛先ホストが並びます)。

**エラーを「使われ方」から切り離して数え直すには `scripts/error-mix.py SNAP.json …`** (T.23.0)。
雪像の `recent` を 1 行 = 1 試行として、(1) いまの物差し (`history["3600"]` の `errors_by_cause` の合計 ÷ `connects`)、
(2) 名前解決に成功した試行を分母にした率 (`reason == "error:dns"` を分母から外す)、(3) 接続元×宛先の
連続した失敗を 1 件にまとめた率 (間隔の閾 N = 60 / 300 秒。間で成功したら別の一続き)、(4) 宛先ごとの
試行・成功・DNS 失敗・timeout・refused の上位 20 を並べます。**どの表にも窓** (`recent` の `min(at)`〜`max(at)`、
件数、`errors` の `count`/`kept`) を添えます。`recent` は 2,000 件の環状なので窓は 1 日より短いことがあり
(2026-10-20 の雪像では 7.9 時間)、部が無い雪像は「判定できず」と書いて 0 とは書きません。

**2 枚の雪像から「何が変わったか」を全部読むのは `scripts/snapshot-diff.py A B`** (T.14.17)。
再起動をまたいでいるかを `uptime_secs` と `version` で見て、**`/history` をその再起動時刻で切り**、
**バーストの無い時間帯 (1 時間 300 本未満の標本) どうし**で CONNECT 確立の p50 / p95 と
名前解決のミス率を並べます (**`--criteria phase22` のときだけ**、前と後で別々に「300 本と、その期間の
1 時間の本数の中央値の 3 倍の大きい方」以上をバーストとします。T.22.1) (`/status` の通算は再起動で 0 に戻るので、そのまま引き算すると
「いつからの値か」が混ざります)。続けてホスト別・接続元別 (**新しく現れた接続元**)・
名前解決の warm と引き直し・出来事・エラーの原因別・バーストを出し、`--criteria phase14` を
足すと**完了の定義に対する判定表** (満たした / 届かず / 判定できず) が最後に付きます。
出力は Markdown なので `TODO.md` にそのまま貼れます。
**`0.18.0` までの版は、`/history` の確立の区間 (と `/status` の `recent_quantiles.connect`) に、繋がらずに 502 を返した接続の「失敗するまでの時間」も入っています**。黒穴の宛先を繰り返し叩く端末が居ると p95 がその時間になります (2026-10-08 の雪像は p95 2,066 ms)。**T.20.4 からは、エラーで終わった 1 件は確立 / 初バイト / `wait` の区間にも直近の標本にも入りません** (`connects` は繋がった本数。失敗は `errors` / `errors_by_cause` / `/errors` / ホスト別の行に残ります。「API」の `/history` の説明)。前後の雪像が T.20.4 をまたぐと `connects` と確立の分位点の数え方が違うので、その 1 回は下の「成功した接続だけ」の行で比べてください。「平常時の前後」の表には後の雪像の `/recent` から**成功した接続だけ**の確立時間 (本数・p50・p95。同じ雪像で 398 本・7 / 66 ms) も 1 行並べ、後の期間のエラーが接続の 5% を超えるときは表の下で断ります (T.20.1。`--criteria` が phase14 / phase15 / phase17 / phase18 のときは出しません — この 4 つの出力は過去の判定を出し直せるように変えない決まりです)。判定の部は `scripts/snapshot_criteria.py` に割ってあります (入口は `snapshot-diff.py` のまま。そのテストは `scripts/test_snapshot_criteria.py`)。

```bash
scripts/snapshot-diff.py status/2026-09-1{2,6}*-snapshot.json --criteria phase14
scripts/snapshot-diff.py a.json b.json --aaaa aaaa.json --out json      # 機械で読む形
scripts/snapshot-diff.py a.json b.json --group domain                   # eTLD+1 でまとめる
# phase15 の判定。`/daily` は雪像に入っていないので、同時刻に取ったものを渡すと日ごとのミスの幅が並ぶ
scripts/snapshot-diff.py a.json b.json --criteria phase15 --daily b-daily.json
# phase17 の 8 行 (phase15 の 4 行 + conn 役の CPU/要求・`dns_warm_max` と `warm_evicted`・
# `/events` の anomaly の種類別 件/時 (起動から、`cleared:` は数えない)・cgroup の起動からの CPU)。
# `/profile?res=60` も雪像に入らないので渡す (無ければ雪像の `/profile` の部で「参考」)
scripts/snapshot-diff.py a.json b.json --criteria phase17 --daily b-daily.json \
                         --profile b-profile_res_60.json   # 前も比べるなら --profile-before FILE
# phase18 の 10 行 (phase17 の 8 行 + `heap_used + mmap` の 0 時間の雪像からの増えが 5 MB 未満・
# `ipv6.request_probes` が 0)。0 時間の雪像 (再デプロイの直後の 1 枚) は `--zero` で渡す (無ければ「判定できず」)。
# `--daily-snapshots DIR` は、DIR の日次の雪像 (`<日付>T000000Z-snapshot.json`) のうち b と同じ版のものを
# 撮った時刻の順に並べた表 (起動からの時間・`rss`・`heap_used`・`heap_free`・`mmap`・`rings_used.total`・
# `ipv6.attempts`) を判定表の下に付ける
scripts/snapshot-diff.py a.json b.json --criteria phase18 --daily b-daily.json \
                         --profile b-profile_res_60.json --profile-before a-profile_res_60.json \
                         --zero zero.json --daily-snapshots status
# phase20 (T.20.1) は phase18 と同じ並びの 10 行で、材料も同じ。物差しを 2 つ直してある:
# `heap_used + mmap` の増えは `cache_memory` (メモリのキャッシュ。上限まで使ってよい) を引いてから比べ、
# `discord.com` のミス率・`timeout`・`dns_warm` の最大と `warm_evicted` の 3 行は「表示だけ」(閾を置かず、集計に数えない)
scripts/snapshot-diff.py a.json b.json --criteria phase20 --daily b-daily.json \
                         --profile b-profile_res_60.json --profile-before a-profile_res_60.json \
                         --zero zero.json --daily-snapshots status
# phase21 (T.21.2) は phase20 と同じ並びの 10 行で、材料も同じ。T.20.99 で届かなかった 3 行の読み方を直してある:
# 平常時のミスは「表示だけ」(日ごとの幅と `misses_by_kind` の `cold` / `expired` … の内訳を並べる)、
# conn 役の CPU/要求 は前か後の `/profile` が 12 時間 (`res=60` で 720 標本) に満たなければ「参考」(集計に数えない)、
# `ipv6.request_probes` は最後に探ったのが `--zero` の取得から 3 時間以内なら「満たした」(起動直後は canary が IPv6 をまだ見ていない)
scripts/snapshot-diff.py a.json b.json --criteria phase21 --daily b-daily.json \
                         --profile b-profile_res_60.json --profile-before a-profile_res_60.json \
                         --zero zero.json --daily-snapshots status
# phase22 (T.22.1) は phase21 の 10 行 + 22a の 4 行で、材料も同じ。足した 4 行は
# `dns_slow` が窓のミスの合計 1,000 ms 未満の窓で立った件数 (0 で満たした。新しい閾 = 合計 1,000 ms 以上 (T.22.2) での件/時は表示だけ)、
# keep-warm の既定を戻す 2 つの条件 (`stale_retries` が `stale_hits` の 1% 以上・`refused`/`unreachable`/`timeout` の
# **接続 1 本あたり**が前の 1.5 倍以上で「届かず」。件/時は表示だけ。T.22.4)、T.22.4 の目安 (引き直しの回/時が前の半分以下・
# `expired` + `warm_stale` の増えが 1 日 10 回以内)。phase21 の `/events` の行 (`dns_slow` ≤ 0.1 件/時) は phase22 では表示だけ
# (件/時の閾は使われ方で落ちるので、`dns_slow` の判定は上の行に任せる)。
# 裏の引き直しの行は warm 0 (keep-warm を止めた既定) だと warm の平均で割れないので、理由と通算の 1 時間あたりを並べる。
# **平常時の切り方もこの定義だけ違う** (`--criteria` 無しでも `--burst-median-k 3` で同じ切り方になる。collect-deployed.sh の
# 判定表の無い差分はこれを付ける。T.29.13): 1 時間の本数が 300 本と、その期間の中央値の 3 倍の大きい方以上をバーストとする
# (量の多い tokyo で 300 本だと 29 標本のうち 23 がバーストになった。量の少ない期間は 300 本の切り方のまま)。
# §2 の表の頭に前と後の閾を書き、`--out json` の `history` に `median_k` `medians` `limits` が足される
scripts/snapshot-diff.py a.json b.json --criteria phase22 --daily b-daily.json \
                         --profile b-profile_res_60.json --profile-before a-profile_res_60.json \
                         --zero zero.json --daily-snapshots status
# phase23 (T.23.1) は phase22 の 14 行を土台に、エラーの行の**分母**と heap の行の書き方だけを直した 16 行。
# 接続のエラー (`refused`・`unreachable`・`timeout`) は `/recent` の connect の 1 行を 1 試行とし、
# **名前解決に失敗した行 (`error:dns`) を分母から外した**「名前解決に成功した試行あたり」で見る
# (前の期間の 1.5 倍以上なら「届かず」。件/時と、分母が `connects` だった phase22 の行は表示だけ)。
# `/recent` は 2,000 件の環状なので、**窓と試行の本数が実測の欄に出る** (前と後で長さが違う)。
# 平常時のミスの行は、同じ雪像の `/recent` から「試行あたり」と「名前解決に成功した試行あたり」も並べる
# (死んだ名前の総当たりという使われ方の数字なので表示だけ)。heap の行は、`--zero` の雪像と起動が
# 違うときに `/events` の `start` の時刻を添える (黙って「判定できず」にしない。2026-10-17 22:14:52Z の
# 無言の再起動がこれで読める)。最後の行は無言の再起動 (`/events` の `start` の直前に `shutdown` が無い起動)
# が 0 回 (T.23.2)。**古い定義を出し直すときは `CRITERIA=phase23` を渡す** (collect-deployed.sh の既定は phase28)
scripts/snapshot-diff.py a.json b.json --criteria phase23 --daily b-daily.json \
                         --profile b-profile_res_60.json --profile-before a-profile_res_60.json \
                         --zero zero.json --daily-snapshots status
# phase24 (T.24.2) は phase23 の 16 行をそのまま持ち越し、**2 行だけ読み方を変えた**もの:
# heap の行は、`/status` の `memory.baseline` (状態ファイルに**起動から 60 秒で 1 回だけ**書く
# `heap_used + mmap − cache_memory`。T.24.4) があれば**それとの差**で読む (`--zero` の 0 時間の雪像は要らない。
# ホストの管理者に起動し直されても、起動ごとの基準なので判定できる)。`baseline` が無い雪像は phase23 と同じ
# 0 時間の雪像との差に戻り、どちらも無ければ「判定できず」。最後の行は**こちらが原因の**無言の再起動
# (同じ `boot` のまま起動し直した回) が 0 回で、`boot` が変わった回 (ホストのカーネルごとの再起動。
# 2026-10-21 の 2 回) は時刻つきで理由を書き、**数から外す**。
# 17 番目 (T.24.3) は、確立の**尾**を固定の 6 ms ではなく**そのときのホストの RTT + 5 ms**で読む
# (T.14.99 の決まり)。`/recent` の connect の行の `ms.connect` と `rtt_ms.origin` を**同じ行で対にした**
# 差の中央値 (プロキシの取り分) と p95 (尾) を出し、尾が 5 ms 未満なら「距離」(相手までの RTT で
# 説明できる)。**`/recent` は 2,000 件の環状なので、窓と本数が実測の欄に出る**。`rtt_ms.origin` の
# 無い雪像 (Linux 以外・`--lite`) は「判定できず」。phase25 (T.27.0) は phase24 の 17 行のうち、接続のエラーの行に
# 試行 1,000 本の下限、引き直しの行を 1 名前あたり、無言の再起動の行を「前の雪像より後の起動だけ」に読み替えたもの
scripts/snapshot-diff.py a.json b.json --criteria phase24 --daily b-daily.json \
                         --profile b-profile_res_60.json --profile-before a-profile_res_60.json \
                         --zero zero.json --daily-snapshots status
# phase26 (T.29.10) は phase25 の 17 行を 1 つも動かさず、18 番目から後ろに 9 行を足したもの。T.28.99 で手で読んでいた 408 の件数 (`rejected_requests.header_timeout`。表示だけ)・
# `/log` の error の行が 5 行以下・`/errors` の `local` (私設宛ての 403) が 0 件・3 秒で切った確立と成功した確立の最大と
# `syn_retrans` 1 / 2 の本数 (表示だけ)・`rejected_requests` の合計と理由別 (表示だけ) と、T.29.99 の
# `rss − (heap_used + mmap + cache_memory)` が 10 MB 未満 (T.29.25)・cgroup の起動からの CPU が前の版より下がる・
# `profile-sample` と `cache-probe` の CPU の割合が前の版より下がる (前か後の `/profile` が 12 時間未満なら参考)・
# `/log` のオリジン側の失敗 (502・時間切れ) の WARN が 1 分に 10 行以下 (T.29.15。原因ごとに 1 分 1 行)
scripts/snapshot-diff.py a.json b.json --criteria phase26 --daily b-daily.json \
                         --profile b-profile_res_60.json --profile-before a-profile_res_60.json \
                         --zero zero.json --daily-snapshots status
# phase27 (T.30.0・T.30.1) は phase26 の 26 行のうち 2 行だけ読み方を変えたもの。
# 接続のエラーの行は、前も後も**分母 (名前解決に成功した試行) が 1,000 本以上**のときだけ判定する (T.29.99 は名前の
# 引けないメール宛の総当たりで分母が 489 本、16.40 倍で「届かず」だった)。RSS の返していない分の行は、固定の 10 MB ではなく
# **前の版の同じ値より下がる**で読み、`heap_free`・`stacks_estimate`・`malloc_trim` の回数と返した量 (`/status` の
# `memory.trim`。T.30.1) は表示だけで並べる
scripts/snapshot-diff.py a.json b.json --criteria phase27 --daily b-daily.json \
                         --profile b-profile_res_60.json --profile-before a-profile_res_60.json \
                         --zero zero.json --daily-snapshots status
# phase28 (T.31.0・T.31.1) は phase27 の 26 行のうち 2 行を量に左右されない物差しにしたもの (`collect-deployed.sh` の既定は phase28)。
# cgroup の CPU はコアの合計ではなく**起動からの要求 1 本あたり**で前の版と比べ (1.1 倍未満。T.30.99 は要求が 1.6 倍でコアの合計が
# 1.09 倍の「届かず」だった)、RSS の返していない分は **`malloc_trim` が毎時動いて返しているか** (`memory.trim`) で読む
scripts/snapshot-diff.py a.json b.json --criteria phase28 --daily b-daily.json \
                         --profile b-profile_res_60.json --profile-before a-profile_res_60.json \
                         --zero zero.json --daily-snapshots status
# `/snapshot` より前の形 (`/status` と `/history` を 1 本ずつ取ったファイル群) からも組めます
scripts/snapshot-diff.py --from-files status/2026-09-12T2018Z \
                         --from-files status/2026-09-16T0106Z
```

**`--group domain` は「dlsite 全体で何件か」を読むためのまとめ方です** (T.14.54)。`/hosts` は
`img.dlsite.jp` と `www.dlsite.jp` を別の行で持つので、相手ごとの合計が読めません。
`--group domain` は **eTLD+1 が同じホスト同士を 1 行にまとめます** (`img.dlsite.jp` +
`www.dlsite.jp` → `dlsite.jp`。`www.dlsite.com` は eTLD+1 が違うので**別の行**です)。
`snapshot-diff.py` にも `status-diff.py` にも同じ `--group domain` があります。

- eTLD+1 は **`scripts/proxydata.py` の `etld1()` による近似**です。Public Suffix List
  (約 9,000 行) は持たず、**ラベルが 2 つ以上の公開接尾辞の短い表** (`co.jp` / `ne.jp` /
  `ac.jp` / `go.jp` / `or.jp` / `co.uk` / `com.au` …) に当たれば 3 ラベル
  (`www.dmm.co.jp` → `dmm.co.jp`)、当たらなければ**末尾 2 ラベル** (`a.b.example.io` →
  `example.io`) にします。事業者が 1 段下を配る形 (`github.io` のような) は分けられません
  — 外れたら表に 1 行足してください
- まとめるのは**同じ種類どうし**です (CONNECT と forward は混ぜません)。要求数・バイト・
  エラー・名前解決・確立の合計 ms は**和**、`avg_ms` は**計測数 (`timed`) で重みづけ**、
  **p50 / p95 は足せない**ので 2 つ以上まとまった行では出しません (`max` は最大)。
  `AAAA` は全部同じときだけその値です
- IP リテラル宛て・`localhost`・表からあふれた `other` は**まとめずにそのまま**出ます

**表からあふれた `other` の行は、ホストどうしを比べる表に入れません** (T.28.19)。`other` は `/hosts` の 1,000 行から
あふれたホストの畳み先で、生きた CONNECT・forward・死んだ宛先への総当たりを 1 行に混ぜた**版をまたぐ通算**です
(10-25 の tokyo では p95 10,006 ms・max 10,012 ms で、これは T.23.3 で 10 → 5 秒にする前の timeout の残り)。
`status-diff.py` は CONNECT / forward の表と AAAA 別の中央値から外し、表の下に「`other` (混在・通算)」の 1 行
(要求・計測・avg・エラーの原因・名前解決のミス。分位点は出さない) を出します。`snapshot-diff.py` も §3 の表と
AAAA 別の中央値から外して同じ印の 1 行を出します (`--out json` では `hosts.folded`)。**`--criteria` を付けたときは
今までどおり** (過去の判定表の出力を 1 バイトも変えないため。§7 のエラーの合計にはどちらでも入っています)。

`collect-deployed.sh` は前回の雪像を見つけるとこれを呼び、**要約のいちばん最後に判定表**を置きます
(`CRITERIA=off` で止められます。判定表の相手は `BEFORE=<雪像>` で、0 時間の雪像は `ZERO=<雪像>` で渡せます。下の「運用」の 4)。道具の単体テストは `python3 -m unittest discover -s scripts`
(架空の雪像 `scripts/testdata/snapshot-a.json` / `snapshot-b.json` と、下の匿名化した実データで回ります)。

**1 週間ぶんをまとめて読むのは `scripts/weekly-report.py`** (T.14.40)。`snapshot-diff.py` が
「2 枚の間に何が変わったか」を見るのに対して、こちらは**溜まった雪像を日で切って 1 週間を 1 枚**にします。
入力は雪像の置き場 (`status/`) でも、`*-snapshot.json` を並べても、
`/daily` (T.14.20) の JSON でも、`$HOME/.rust-http-proxy.daily.jsonl` そのものでも構いません
(混ぜてよい。**7 日ぶん無ければあるぶんで**出して、何枚・何日ぶんだったかを頭に書きます)。
出るのは表 8 つ — **要求数 / CONNECT 確立 p50・p95 / 名前解決のミス率 / エラー (原因別) / 山 /
接続元の出入り / 遅かったホスト上位 / 新しく見たホスト**:

```bash
scripts/weekly-report.py status/ -o week.md    # 置き場ごと渡す
scripts/weekly-report.py status/*-snapshot.json --days 7 --top 10
curl -s 'http://PROXY/daily?n=7' > daily.json && scripts/weekly-report.py daily.json
scripts/weekly-report.py status/ --out json    # 表の元の辞書をそのまま
```

**数字の求め方は `snapshot-diff.py` と同じ**です: `/history?res=3600` を **UTC の日で切って**
同じ `aggregate()` に通すので、同じ期間を切れば同じ値が出ます (分位点の補間は
`crates/metrics/src/history.rs` の `quantile_ms`、「平常時」= 1 時間 300 本未満の標本を使うのも同じ)。
日ごとの表には出どころ (`/history?res=3600` か `/daily`) が入ります。
`/hosts` と `/clients` は `.rrd` の通算なので**いちばん古い雪像といちばん新しい雪像の差**で読み、
雪像が 1 枚しか無いときは「引き算できない」と断って通算のまま並べます。
「山」は `/bursts` (T.14.6) があれば写真の枚数と最大、無ければ `/history` の `active_max` で代えます。
出力は「デプロイ先の現在地」を締める文書と、次のタスクの入力にするためのものです。

**実データを匿名化してテストへ持ち込むのは `scripts/anonymize-snapshot.py`** (T.14.35)。雪像には
個人の閲覧先が並ぶのでそのままではリポジトリに入れられませんが、**ホスト名** (`host-0001.g0007.example`)・
**接続元 IP** (`198.51.100.x`)・**名前解決の答え** (`203.0.113.x`)・**`User-Agent`** (`ua-01`。`agents` の一覧も、
いまの版の `agent` (単数) も、`/events` の `new_client:` の説明の終わりの `agent "…"` も同じ表)・
**`/log` の行と `/events` の説明の中の名前と IP** だけを置き換えれば、本物の分布のまま持ち込めます。
置き換えは**決定的** (同じ入力からは同じ出力) なので、**匿名化したあとの 2 枚でそのまま差分が取れます**。
ホスト名の `gNNNN` は**まとめの単位 (eTLD+1) の番号**で、`img.dlsite.jp` と `www.dlsite.jp` は
同じ番号 (= 匿名化後も同じ eTLD+1 `gNNNN.example`) に落ちます。**上の `--group domain` の粒度が
匿名化したあとも読める**ようにするためです (T.14.54)。古い形 (`host-0001.example`) の
匿名化済みファイルはそのまま通ります (2 回かけても名前は変わりません)。
**数字は 1 つも変わりません** (件数・ms・区間・閉じた理由・時刻・`version`・`path`)。`connect://` の
scheme と port、表の上限を越えた分の行 (`other`) はそのままです。

```bash
scripts/anonymize-snapshot.py status/2026-09-16T0106Z-snapshot.json \
                              -o scripts/testdata/deployed-2026-09-16.anon.json
# `/snapshot` より前の形 (1 本ずつ取ったファイル群) からも組めます (`-metrics` 等は飛ばします)
scripts/anonymize-snapshot.py status/2026-09-16T0106Z-* -o anon.json
scripts/snapshot-diff.py old.anon.json new.anon.json      # 匿名化したあとでも差分は取れる
# 雪像に入らない口 (`/daily` `/profile?res=60`) も雪像と同じ表で (既定の出力は隣の `<名前>.anon.json`。T.17.16)
scripts/anonymize-snapshot.py status/2026-09-26T114602Z-snapshot.json -o new.anon.json \
    --side status/2026-09-26T114602Z-daily.json --side status/2026-09-26T114602Z-profile_res_60.json=p60.anon.json
```

同梱の `scripts/testdata/deployed-2026-09-16.anon.json` (778 KiB) がその出力で、**デプロイ先の
2026-09-16 の雪像そのまま**です (ホスト 817 件・名前解決 90 件・`res=60` は 1,440 標本)。
`node scripts/check-dashboard.js` と `python3 -m unittest discover -s scripts` がこれを読み、
`/history?res=3600` を起動時刻で切った平常時から **T.14.0 の表と同じ数字** (名前解決 0.55 回/接続、
CONNECT 確立 p50 8.3 / p95 80.7 ms、ミス 1 回 11.5 ms) が出ることを見ています。
2 枚目の `scripts/testdata/deployed-2026-09-26.anon.json` (2.3 MiB。隣に `-daily.anon.json` と `-profile_res_60.anon.json`) は
**2026-09-26 の雪像** (版 `0.1.0+2e57626`、起動から 47.2 時間) で、2026-09-16 の版に無かった `recent` / `profile` / `events` /
`hosts_series` と T.16.0 の 3 列が入っています。2026-09-16 → 2026-09-26 で `--criteria phase15` / `phase17` の判定表が出て、
T.16.99 の数字 (`conn` 役 0.0013 コア、`dns_warm_max` 15、`warm_evicted` 0、`dns_slow` 0.61 件/時) が出ることを見ています
(T.17.16。名前は番号が前後で合わないので、`discord.com` を見張る行は「判定できず」になります)。

**いまの数字は 2026-10-25、tokyo (`tokyo.sorahost.net:60357`) に 0.27.8 をデプロイして 23.7 時間の実測**です。出どころは雪像 1 枚
(`VIA=$HTTPS_PROXY scripts/collect-deployed.sh tokyo.sorahost.net:60357 status/tokyo` が取った `status/tokyo/2026-10-25T203134Z-snapshot.json`)。版は `0.27.8+4b75403` (2026-10-24 20:51:52Z 起動) です。
数字の本文と判定 (`--criteria phase25` で満たした 11・届かず 0・判定できず 0) は `tasks/T27.md` の T.27.99 の結果にあり、`TODO.md` の §2「デプロイ先の現在地」はそこを指しています (T.28.21 で、締めのたびに表を書き写すのをやめました)。**雪像はリポジトリに入れません** (利用者の閲覧先ホストと接続元 IP が並びます)。

| 項目 | tokyo (0.27.8、23.7 時間) |
|---|---|
| 要求 | 22,907 本 (23.7 時間)。接続元は全部 `10.255.0.1` (手前の NAT) |
| CONNECT 確立 | 直近の個票 1,998 本で **p50 18.0 / p95 259.0 ms**。同じ行の RTT は p50 30.6 / p95 264.6 ms で、**確立 − RTT の p95 は +0.2 ms** (= 遅れは相手までの距離) |
| 名前解決 | ミス 0.05 回/接続 (平常時)、期限切れの答えをすぐ使った回 6,682・失敗して引き直した回 **0** |
| エラー | `timeout` 5.70 件/時 (前の版 11.52)。接続のエラーは名前解決に成功した試行あたり 0.0085 (前 0.0135)。**ほとんどがメール宛の総当たり** (要求の 4.2% のメール系ポートが `/errors` の 95%) |
| CPU | cgroup の通算 0.0017 コア (割り当て 0.5 コア)、絞り 0 |
| メモリ | `heap_used + mmap − cache_memory` は起動 60 秒の基準 17.6 MB から **+3.1 MB**、RSS 56.1 MB (うちキャッシュ 13.2 MB) |
| 枠組みの検査 (T.27.5〜T.27.8) | 断った要求 **0 件** (正規の要求を誤って断った形跡も無い) |

- **利用者の待ちは実質 1 RTT** で、コードで縮められる所はほぼ残っていません (T.14.99 の結論のまま。2026-10-25 の洗い出しでも同じ)。待ちの尾・失敗・`dns_slow` のほとんどは、死んだメールサーバーへの総当たりという使われ方です。
- **接続元が全部 `10.255.0.1` に見えます** (手前の NAT の内側の番地)。tokyo では接続元ごとの上限・許可・追跡は効きません。
- **2026-10-08 に nagoya から tokyo へ切り替えました** (`TODO.md` の付録 A「デプロイ先の切り替え」)。nagoya の数字と tokyo の 1〜3 枚目は付録 A に残してあります。

**直す前 (2026-09-10) との比較**は日付ごとの表に残してあります (`TODO.md` の**付録 A「デプロイ先の現在地の
古い版」**。2026-09-10 / 2026-09-12 / 2026-09-16 / 2026-09-18 / 2026-09-24 / 2026-09-26 / 2026-10-01 / 2026-10-08 の 8 枚 (nagoya) と 2026-10-15・2026-10-17・2026-10-20 の 3 枚 (tokyo) を 1 文字も変えずに移しました。2026-09-18 の 1 枚にはタスク 14 の名前解決 0.55 → 0.144 回/接続と、p50 6 ms に届かなかった理由、2026-09-24 の 1 枚にはタスク 15 の `discord.com` 0.441 → 0.01 があります)。山場は 5 つで、
CONNECT 確立 (AAAA のあるホスト) **257 → 9 ms** (2026-09-12)、接続元から見た応答の p50 **327.8 → 8.0 ms** (同)、
RSS **215.9 → 21.1 MB** (先行確保 201.3 → 0、2026-09-16)、`GET /` **502 → 200** (2026-09-12)、
**原因の分からなかったエラー 12 件**が原因別と個票つきになったこと (2026-09-16) です。

**250 ms が消えたのは Happy Eyeballs の順番を変えたから**です。AAAA (IPv6) を持つホストへは IPv6 を先に試し、
返らなければ 250 ms (RFC 8305 の Connection Attempt Delay) 待ってから IPv4 に移ります。IPv6 が**黙って落ちる**
この環境では、AAAA のあるホスト全部でこの 250 ms を毎回払っていました。今はホストごとに最後に勝った族を覚え、
IPv6 が起動から 1 度も勝たずに 3 回続けて負けたら初めて見るホストも IPv4 から試します
(「特徴」の IPv4 / IPv6 の項。IPv6 が生き返ったかは canary が 1 分に 1 回見ていて、繋がれば自動で戻ります。
canary が IPv6 を見ていないときは 600 秒に 1 回、利用者の要求で IPv6 を先頭に戻して試します)。

**残っている待ちは遠いホストの RTT だけ**です。上のとおり確立の中央値は 1 往復そのもので、プロキシ側では縮みません。
タスク 14 の頃の 1 枚にあった尾 (p90 53.5 / p95 78.0 ms) は、2026-10-01 の 1 枚では平常時の `wait` で p90 9.9 / p95 18.0 ms でした
(タスク 15 から CPU の絞りは 0 のまま。`TODO.md` の T.15.5。尾の残りは遠い相手への connect で、行き先の構成で上下します)。
**名前解決は、nagoya では待ちの主役ではありませんでした** (接続 1 本あたり 6.4 → 1.6 ms)。**tokyo では接続 1 本あたり 4.2 ms でしたが、T.21.3 で 1.9 ms になりました** (期限切れの答えの引き直しを待たなくなったため。残りは初めて引く名前)。

## RSS の段を手元で再生する (`scripts/memory-replay.py`)

T.32.0 (2026-10-31)。0.30.4 の 48 時間で RSS が 48.6 → 73.6 MB と段になって増え、`malloc_trim(0)` で戻らなかった分の正体を
手元で確かめるための手順です。場面ごとに新しいプロセスを 127.0.0.1 で起こし (HOME とキャッシュの置き場は使い捨て、
`PROXY_ALLOW_LOCAL=on`・`PROXY_CANARY=off`)、手元のオリジン (スクリプトの中のスレッド) への CONNECT 2,000 本で
`/recent` を埋めてから、次の 4 つを流します。

```bash
scripts/memory-replay.py target/release/rust-http-proxy                       # (a)〜(d) を順に (6 分ほど)
scripts/memory-replay.py target/release/rust-http-proxy --only d              # 1 つだけ
scripts/memory-replay.py target/release/rust-http-proxy --env MALLOC_MMAP_THRESHOLD_=131072   # glibc の環境変数で閾を固定
```

- (a) 重い口: `/snapshot` ×5 (日次の雪像は同じ組み立てで上限が大きいだけ)
- (b) 名前解決の総当たり: 名前の引けない宛先 (`*.invalid`) への CONNECT 4,000 本 (8 並列)
- (c) (b) のあとに (a) (ホスト表が埋まって `/snapshot` が 1 MB を超える)
- (d) メモリのキャッシュに 14 MiB の forward GET を入れて PURGE (= 追い出し) を 3 回、そのあと (a)

段の前に `/proc/<pid>/clear_refs` へ 5 を書いて VmHWM を戻し、段の後の VmHWM を「山」として読みます。
trim は gdb で `malloc_trim(0)` を 1 回呼び (1 時間ごとの trim の代わり)、glibc の `mp_.mmap_threshold`・
`mp_.no_dyn_threshold`・`narenas` も gdb で読みます (gdb が無ければ `-`)。数字は MB (10^6)、
`heap_used`・`heap_free`・`mmap` は `/status` の `memory` (`mallinfo2`)。

**T.32.3 の前** (`8264a66` = 0.30.4 と同じ malloc の設定。2026-10-31、4 コアの sandbox):

| 場面 | 段 | rss | 山 | heap_used | heap_free | mmap | mmap_threshold | arenas |
|---|---|---|---|---|---|---|---|---|
| (a) | 暇 (CONNECT 2,000 本のあと) | 27.1 | 27.1 | 6.1 | 0.6 | 11.7 | 2,101,248 | 8 |
| (a) | `/snapshot` ×5 (0.30 MB) | 28.1 | 28.1 | 6.2 | 1.6 | 11.7 | | |
| (a) | trim 後 | 27.5 | | 6.2 | 1.6 | 11.7 | 2,101,248 | 8 |
| (b) | 暇 | 27.0 | 27.0 | 5.9 | 0.8 | 11.7 | 2,101,248 | 8 |
| (b) | 総当たり 4,000 本 | 37.0 | 37.0 | 10.9 | 5.8 | 11.7 | | |
| (b) | trim 後 | 35.8 | | 10.9 | 5.6 | 11.7 | 2,101,248 | 8 |
| (c) | 総当たりの trim 後 | 35.0 | | 10.6 | 6.1 | 11.7 | 2,101,248 | 8 |
| (c) | `/snapshot` ×5 (1.02 MB) | 39.9 | 39.9 | 10.8 | 10.1 | 11.7 | | |
| (c) | trim 後 | **37.9** | | 10.8 | 10.1 | 11.7 | 2,101,248 | 8 |
| (d) | 暇 | 27.0 | 27.0 | 5.9 | 0.6 | 11.7 | 2,101,248 | 8 |
| (d) | 14 MiB の GET→PURGE ×3 | 42.5 | 42.5 | 6.0 | 15.8 | 11.7 | | |
| (d) | trim 後 | **42.2** | | 6.0 | 15.8 | 11.7 | **14,684,160** | 8 |
| (d) | `/snapshot` ×5 → trim 後 | 42.3 | 42.5 | 6.1 | 15.7 | 11.7 | 14,684,160 | 8 |

読み方:

- **閾は起動直後にもう 2 MiB** (起動中に 2 MiB の `munmap` が 1 回)、**14 MiB の本文を捨てると 14 MiB** まで上がります。
  上がった後は 14 MiB の本文がアリーナから取られ、PURGE しても `heap_free` 15.8 MB のまま trim で返りません (主でない
  アリーナの天辺は `malloc_trim(0)` が詰めない)。**(d) の +15.2 MB はそのまま居座る**
- (c) の `/snapshot` ×5 は山 +4.9 MB、trim 後も **+2.9 MB** 残ります (閾 2 MiB より小さい一時の `String` がアリーナに残る)
- (b) の総当たりは +10 MB で、うち `heap_used` +5 MB は名前解決の表が埋まった分 (T.32.2)。trim で返るのは 1.2 MB だけ
- tokyo の 10-30 16Z の段 +16.7 MB (`history["60"]` で 16:18 の 45.3 MB → 16:24 の 62.0 MB) は、**16:18〜16:20 の +6.6 MB**
  (保存 0、09:02・12:36 と同じ形 = 重い口とみられる。未確認) と、**16:21〜16:23 のメモリのキャッシュ 13.9 MB** (`mem_used`
  0 → 13,857,210、`stores` 18 → 35。RSS は +10.1 MB で、残り 3.8 MB は手元の `heap_free` の使い回し) に分かれます。
  キャッシュの分は (d) と同じ形 (追い出した 10-31 02:34 に RSS −4.6 MB しか戻らない)

## デプロイ先に似せた条件で手元で測る (`scripts/deployed-like.sh`)

**上の 250 ms は loopback では再現しません。** ベンチは宛先を IP リテラルで書くので、プロキシから見た
候補が 1 つしかなく、**Happy Eyeballs (候補が 2 つ以上のときだけ通る道) に一度も入らない**からです。
`scripts/deployed-like.sh` は root なしで「**IPv6 が黙って落ちる**」小さな環境を作り、その中でベンチを回します。

```bash
scripts/deployed-like.sh -- scripts/cpu-per-request.sh --only connect-multi --seconds 5
scripts/cpu-per-request.sh --deployed-like --only connect-multi --seconds 5     # 同じもの
```

作るものは 4 つで、どれもデプロイ先 (Pterodactyl コンテナ) に合わせてあります。

- **ネット名前空間** (lo だけ)。外へは出られないので、オリジンはベンチの内蔵のものだけです
- **IPv6 の既定経路を `dev lo`** に置く。出ていった SYN は lo を回って戻り、自分宛てではないので黙って
  捨てられ、`connect` は**約 1 秒ハングしてから失敗**します (デプロイ先の実測と同じ姿)。
  **`blackhole` の経路ではこうなりません** — この機械では `connect` がその場で `EINVAL` を返すので、
  プロキシは待たずに IPv4 へ移り、250 ms が出ません
- **`/etc/hosts` に `multi.test` の 2 行** (`2001:db8::1` = 上の黒穴、`127.0.0.1` = 生きている)。
  プロキシの `getaddrinfo` が AAAA と A の 2 候補を返します
- **`ulimit -n 1024`** と **cgroup 256 MiB** (`systemd-run --user --scope`。デプロイ先の `max_conns` 240 は
  この `ulimit` から決まります)

`--only connect-multi` はこの `multi.test` 宛てに CONNECT を張り続け、**1 本目の確立時間**と、そのあとの
p50 / p95 / max を出します。Happy Eyeballs に記憶を持たせる前後で比べると:

| プロキシ | 1 本目 (8 スレッドの中央値) | p50 | p95 | 確立/秒 |
|---|---|---|---|---|
| 記憶を持つ前 (`41e918f`) | 257.6 ms | **251.7 ms** | 257.0 ms | 32 /s |
| いま | 261.7 ms | **0.57 ms** | 1.60 ms | 7,479 /s |

2026-09-16、`--conc 8 --seconds 5`。記憶を持つ前は**毎回** 250 ms を払い、いまは**同時に走り出した
8 本だけ**払います (2 本目からはホストごとの記憶が効く)。終わりに `/status` の `ipv6` を印字します
(いまのバイナリは `{"attempts":16,"wins":0,"losses":16,"v4_first":true}` = IPv6 は 1 度も勝たず、
IPv4 を先頭にする学習が効いた状態)。**`--conc` は 3 以上**にしてください (2 本目からは記憶が効いて
IPv6 を試さないので、「3 回続けて負けたら IPv4 を先頭」は同時に走り出した本数ぶんからしか立ちません)。

**名前空間が作れない機械では「使えない」と印字して終了コード 2** で終わります
(`--only connect-multi` を名前空間の外で回したときも同じく 2)。

## デプロイ先のパターンを手元で再生する (`bench --only replay`)

バーストのときに「上限に当たったら暇なトンネルを 1 本閉じる」(`PROXY_MAX_CONNS` の行) と
「山の写真」(`/bursts`) が本当に効くかは、**バーストが来るまで分かりません**。`/recent` には
閉じた接続 1 本ごとの「いつ・どこへ・どれだけ・どれだけ生きたか」があるので、**同じ時間間隔・
同じ本数**で手元の内蔵オリジンへ張り直せば、バーストの形だけ再現できます。

```bash
scripts/deployed-like.sh --nofile 4096 --hosts-from scripts/testdata/replay-burst.json -- \
  ./target/release/bench --proxy 127.0.0.1:18080 --only replay \
    --replay-file scripts/testdata/replay-burst.json \
    --rtt-file scripts/testdata/replay-hosts.json --speed 10
```

- **`--replay-file`** が読むのは `/recent` の応答・`/snapshot`・`/connections` の JSON です
  (`/connections` はまだ閉じていない接続なので寿命が無く、`age_secs` を「開いてからの秒」と
  「これから生きる秒」の両方に使い、`bytes` は全部下りに寄せます)
- 宛先は**ホスト名をそのまま** CONNECT の target に載せ、**ポートだけ内蔵オリジンのもの**に
  置き換えます。名前解決は `scripts/deployed-like.sh --hosts-from <個票>` が名前空間の中の
  `/etc/hosts` に `127.0.0.1` の A を並べて引き受けます (**AAAA は足しません** — IPv6 の
  黒穴を見るのは `multi.test` の役目で、混ぜるとバーストではなく Happy Eyeballs を測ることに
  なります)。足した名前は名前空間の中にしか出ません
- `opened_at` (`at`) の**間隔**と**寿命**と**上り / 下りのバイト**を真似ます。`--speed 10` で
  10 倍速 (間隔も寿命も 1/10)。`--replay-secs N` で途中で打ち切れます
- RTT は `--rtt-file` (`/hosts` の JSON) の `rtt_ms.avg` を**内蔵オリジンの応答遅延**として
  真似ます (個票が `/snapshot` なら中の `hosts` を自動で見ます。無ければ 0 ms)
- 終わりに本数・掛かった時間・CONNECT 確立の p50 / p95 / max・失敗の数と、プロキシの
  `/status` の `evicted_idle` / `rejected_overload` と `/bursts` の枚数を印字します
  (引く先は `--admin HOST:PORT`、既定は `--proxy` と同じ)
- **再生するのは CONNECT だけ**です (`kind` が `http` の個票は数えて飛ばします)。上りは
  合図の 16 バイトが必ず流れるので、上り 16 B 未満の個票はそのぶんだけ多く流れます

同梱の `scripts/testdata/replay-burst.json` は**架空の個票** (ホスト名は `example.invalid`) で、
デプロイ先で見た山 (同時 218 / 上限 240) の形を 260 本で作ってあります。`PROXY_MAX_CONNS=240`
のプロキシに対して 10 倍速で再生すると:

| 見るもの | 実測 (2026-09-16) |
|---|---|
| 張れたトンネル / 失敗 | **260 / 0** |
| プロキシに閉じられた本数 (上限に当たった追い出し) | **20** |
| CONNECT 確立 p50 / p95 / max | **0.754 / 10.541 / 28.423 ms** |
| `/status` の `evicted_idle` / `rejected_overload` | **20 / 0** |
| `/bursts` の写真 | **1 枚** (`active` 240 / `threshold` 120 / `parked` 235 / `relaying` 5 / 宛先 4 種) |

`rejected_overload` が 0 のまま 20 本ぶん席を作れているので、山が上限を越えても 503 を返さずに
受けられていることが手元で確かめられます。

## 注: トンネルの MiB/s はプロキシの上限ではありません

**この行だけはベンチ側が律速しています。** プロキシは `splice(2)` で 1 バイトもコピーしませんが、
ベンチは送る側 (blaster) と受ける側 (reader) で 1 回ずつコピーするので、**ベンチのスレッドの方が先に頭打ち**になります。
そのため MiB/s は「ベンチをどのコアに置いたか」で 2.6 倍動きます
(2026-09-08、同じ `release` バイナリ、`--only tunnel --conc 1` を 10 秒 × 3 回の中央値。
上の表の 3,960 MiB/s は同じレシピを別の回に測ったもので、この経路のぶれは ±15% あります):

| 置き方 | スループット | プロキシの CPU | プロキシが使ったコア |
|---|---|---|---|
| プロキシ cpu4-5 / ベンチ cpu6-7 (現在の既定) | **3,956 MiB/s** | 182.7 us/MiB (user 10.9) | 1 コアの **72%** |
| プロキシ cpu4-7 / ベンチ cpu0-3 (LITTLE) | 1,534 MiB/s | 223.2 us/MiB (user 14.4) | 1 コアの 34% |

プロキシ側は 1 コアを 72% しか使っておらず、飽和しているのはベンチのスレッド (1 本が 100%) の方です。
**この経路の主指標は CPU/MiB** で、`scripts/cpu-per-request.sh --only tunnel --conc 1` が
CPU/MiB と「プロキシが 1 コアの何 % を使ったか」を一緒に出します。
