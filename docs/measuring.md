# docs/measuring.md — 計測の作法

> **雪像の置き場は 2026-09-26 に `~/rust-http-proxy-status/` からリポジトリの `status/` (`.gitignore` 済み) へ移した** (利用者の決定。`$HOME` には書かない)
> `scripts/collect-deployed.sh` の既定の `DIR` も `status/`。
> **機械のロック `mx` は `scripts/mx`** (同じ日にリポジトリへ入れた。本文の `mx …` は `scripts/mx …` のこと)。済んだタスクの本文に出てくる `~/rust-http-proxy-status` は当時の置き場。
> (T37.5 で `TODO.md` の冒頭から移した。)

**この機械は big.LITTLE** (cpu0-3 = Cortex-A55 / cpu4-7 = Cortex-A78、システムコールのコストが 2.2 倍違う)。
**必ず `taskset` でプロキシを 4-7、ベンチを 0-3 に固定する。** 固定しないとぶれが ±5-10% になり、
数 % の差は見えない。

**`--only connect` は tunnels/s (ops) を読まない**: TIME_WAIT 律速で同じバイナリでも ±24% 振れ、交互の組の中の位置だけで中央値が
10% 動く (T14.1 の対照実験)。主指標は CPU/本 だけ、**前後交互は 6 組** (実例: T13.4 の個票を足した変更は 3 組で +5.6% と出て、
6 組に増やしたら +1.4% に収まった)。他の経路 (forward / HIT / tunnel / 1 接続 1 要求) は 3 組のままでよい。この決まりは
`scripts/cpu-per-request.sh` の先頭の注記にも書いてある (T14.2 (6))。

**ただし `--only tunnel` だけは例外で、プロキシ cpu4-5 / ベンチ cpu6-7 (両方 big)。**
ベンチの blaster と reader はどちらも本気で回るスレッドで、LITTLE に置くとベンチが先に頭打ちになる
(同じバイナリで 1,534 → 3,956 MiB/s)。スクリプトが `--only tunnel` のときだけ自動で切り替える (T10.8)。

主指標は **CPU/要求** (`/proc/<pid>/stat` の utime+stime ÷ 要求数)。スループットは律速がベンチ側に
移るので補助でしかない。負荷に依らない指標として **1 要求あたりの確保回数** (数えるアロケータ) と
**システムコール回数** (`strace -f -c`) も使う。

メモリは 2 つを見る: **暇な接続 1 本あたりの RSS** (実行時) と、**ビルドが通る最小の cgroup 上限** (ビルド時、上限 200 MB)。

## 速さの測り方

`scripts/cpu-per-request.sh` が「固定して起動 → ベンチ → utime+stime の差 ÷ 操作数 → ピーク RSS とスレッド数」までやる。
ベンチ (`crates/bench`) は既定のビルド対象から外してあるので `cargo build --release -p proxy-bench` で作る。

**どの経路も同じ条件で測る** (T10.11)。プロファイルは `--lite`、ログ水準は `warn`、キャッシュの ballast は off。
`docs/status.md` の表は「経路どうしを比べる表」なので、1 行だけ条件が違うと比べられない数字が並ぶ。
**条件はスクリプトが引き受ける**ので、環境変数を並べる必要はない
(以前は HIT の行だけ `PROXY_ARGS=""` = 既定プロファイル = ログ **info** で回す手順になっていて、
そこにだけアクセスログ 1 行 7.2 us/要求 が乗っていた。T10.2 が見つけ、T10.11 で直した)。

```bash
cargo build --release && cargo build --release -p proxy-bench   # 道具を作る

scripts/cpu-per-request.sh                                   # forward 8 並列 keep-alive
scripts/cpu-per-request.sh --conc 64                         # forward 64 並列
scripts/cpu-per-request.sh --cacheable                       # キャッシュ HIT
scripts/cpu-per-request.sh --no-keepalive                    # 1 接続 1 要求 (接続あたりの固定費)
scripts/cpu-per-request.sh --only connect                    # CONNECT の確立
scripts/cpu-per-request.sh --only tunnel --conc 1            # トンネル 1 本 (主指標は CPU/MiB)
PROXY_MAX_CONNS=8192 scripts/cpu-per-request.sh --only idle-tunnels --conc 5000  # 暇なトンネル 5,000 本
PROXY_MAX_CONNS=8192 scripts/cpu-per-request.sh --only idle-conns --conc 2000    # 暇な keep-alive 接続 2,000 本

BIN=target/dist/rust-http-proxy scripts/cpu-per-request.sh   # dist で測るときは BIN= を差し替えるだけ
```

**`--only idle-tunnels` / `--only idle-conns` で見るのはスレッド数と RSS** (CPU/op ではない)。
どちらも「確立 + 保持 + 終わりの一斉 close」の合計を本数で割った値になるので CPU は弱い指標
(T8.1、T11.4 でも 285〜425 us と揺れた)。読むのは `threads N -> M (max X, median Y)` と `proxy peak RSS` と `parked max`。
**`--only idle-conns` は 1 要求ずつ通してから握る** (T11.4)。要求を 1 本も通していない接続は `src/lib.rs` の
預ける判定 (`*served > 0`) を満たさないので**そもそも預けられず**、ワーカーを握ったまま
`PROXY_MAX_THREADS` (auto = 256) に当たって止まる。
**握る秒数は `PROXY_KEEPALIVE_SECS` (既定 15 秒) より短くする** (越えると預かり所が期限切れで閉じる。
ベンチが最後に「まだ生きている本数」を出すので、減っていればその行で分かる)。
**`docs/status.md` の表の「着手前」の列 (預けない場合) を測るには `PROXY_PARK_IDLE=off PROXY_MAX_THREADS=0` を足す**
(上限を外さないと 257 本目から待ち行列に入って進まない)。

**出力の 1 行目に条件が出る**ので、数字を残すときはこの行ごと写す。
2 行目の `proxy NN% of one core` で**どちらが律速しているか**を毎回見る (100% = 1 コアを丸ごと。
プロキシに割り当ててあるのは 4 コア = 400%)。

手で回すなら、中身はこれと同じ:

```bash
taskset -c 4-7 ./target/release/rust-http-proxy --lite -p 18080 --bind 127.0.0.1 &
taskset -c 0-3 ./target/release/bench --proxy 127.0.0.1:18080 --conc 8 --seconds 10
```

**ぶれの扱い**: 同じ設定でも 5 秒の計測で ±8% ぶれることがある (2026-09-07 深夜の実測: 50.9 / 55.3 / 58.9 us)。
数 % の差を見るときは **変更前と変更後のバイナリを交互に 3 回ずつ** 10 秒で回し、中央値で比べる。
片方を 3 回続けて測ると、機械の状態の変化 (熱・他プロセス) が差に化ける。

**ビルドの直後 1〜2 本は捨てる** (T10.11)。ビルドの直後に始めた交互 3 回は 1〜2 本目が 48.96 / 49.58 us、
3 本目が 41.25 us で、そのあと続けた 5 回は 40.45〜42.56 us に収まった (forward `release`)。
**ビルドとベンチの間に 1 本捨て走りを入れる。**

**ログ水準を info にすると比べられなくなる** (T10.11)。アクセスログ 1 行は **7.2 us/要求**
(T10.10 の実測。うち kernel 4.9 us は `write(2)` そのもので、stdout に出す限り消えない)。
既定プロファイルはログ info なので、`PROXY_ARGS=""` だけで測ると全経路がそのぶん重くなる。
**比べる表に載せる数字は必ず warn。** 「既定プロファイルの値」を知りたいときは
`PROXY_LOG_LEVEL=info` を明示して、warn の表とは分けて書く。

**`--only connect` のスループットは TIME_WAIT 律速** (T10.11)。この機械は
`tcp_max_tw_buckets = 32768`、`ip_local_port_range` = 32768-60999 (28,231 個)。
CONNECT の計測は 10 秒で 7〜10 万本張るので**毎回バケットを使い切る**
(計測直後の TIME_WAIT は 11 回とも 30,100〜30,900)。プロキシは 4 コアのうち 1.0〜1.3 コア、
ベンチも 1.8〜2.4 コアしか使っておらず、**tunnels/s を決めているのはカーネルの TIME_WAIT の回転**。
11 回で 5,844〜10,203 /s (±25%) と暴れる。**この経路で比べてよいのは CPU/本**
(同じ 11 回で 135.3〜150.4 us、中央値 140.0 からのぶれ ±4%)。

**システムコールの数え方**: `strace -p` はこの環境では `ptrace` が拒まれるので、プロキシを **strace の下で起動する**
(`strace -f -c -o out.txt taskset -c 4-7 target/release/rust-http-proxy --lite ...`)。止めるときは strace ではなく
プロキシ (strace の子プロセス) に SIGINT を送る。回数 ÷ 操作数 が 1 要求 (または 1 接続) あたりの値。
**`strace` の時間表示は使えない** (T10.3: 同じ 10 秒で `-c -w` が 80.6 秒、実 CPU は 8.0 秒)。
**`strace` は回数を数える道具で、時間は `stime` から出す。**

**プロファイル**: `CARGO_PROFILE_RELEASE_STRIP=none CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --release` でシンボルを残し、
`perf record -e cpu-clock -F 4999 -g -p <pid>` → `perf report`。この環境の `perf` は**ユーザー空間しか数えない**
(`perf_event_paranoid = 2`。既定の `cycles` はサンプルが取れないので必ず `-e cpu-clock`)。カーネル側は utime/stime の差で見る。

## デプロイ先に似せた条件 (手元で 250 ms を再現する)

**この文書のここまでの数字はすべて loopback** で、デプロイ先の一番の癖 = **IPv6 が黙って落ちる**が入っていない。
`scripts/deployed-like.sh` が root 無しでその条件を作る (T14.16)。

```bash
scripts/deployed-like.sh -- scripts/cpu-per-request.sh --only connect-multi --seconds 5
scripts/cpu-per-request.sh --deployed-like --only connect-multi --seconds 5   # 同じもの
scripts/deployed-like.sh --memory off --nofile 4096 -- <任意のコマンド>       # 上限は変えられる
```

作るのは `unshare -rmnC` の中の 5 つ: (1) lo だけのネット名前空間 (外へ出られないのでオリジンはベンチの内蔵のものだけ)、
(2) **IPv6 の既定経路を `dev lo`** (出た SYN は戻って捨てられ、`connect` は**約 1.02 秒ハングしてから `ENETUNREACH`** =
デプロイ先の「IPv6 リテラル宛ては約 1.0 秒で 502」と同じ姿)、(3) `/etc/hosts` を「元の内容 + `multi.test` の 2 行」に bind mount
(`2001:db8::1` = 黒穴 / `127.0.0.1` = 生きている → `getaddrinfo` が AAAA と A の 2 候補を返す)、(4) `ulimit -n 1024` と
`systemd-run --user --scope -p MemoryMax=256M`、(5) cgroup 名前空間 + `mount -t cgroup2` (プロキシ自身に 256 MiB の上限を見せる)。

**`blackhole` の経路では再現しない**: この機械では `connect` がその場で `EINVAL` を返す (実測 0.000 秒) ので、プロキシは待たずに
IPv4 へ移り 250 ms が出ない。**「黙って落ちる」ことが要る。**

**`--only connect` は Happy Eyeballs を通らない** (宛先が IP リテラル = 候補 1 つで `crates/net/src/net.rs` の `addrs.len() == 1` の
短絡に入る。T10.1 が「無罪」と結論した理由)。名前宛ての **`--only connect-multi`** は候補が 2 つになるので通り、**各スレッドの 1 本目の
確立時間**とそのあとの p50 / p95 / max を出す。**`--conc` は 3 以上**にすること (T12.1 の「3 連敗で IPv4 を先頭」は同時に走り出した
本数ぶんしか「まだ誰も負けていない」賭けを作らない。2 本目からはホストごとの記憶 `last_win_v6` が効いて IPv6 をそもそも試さない)。
終わりに `/status` の `"ipv6"` を 1 回引いて印字する。cgroup の上限はベンチにも掛かる (太るものを測るときは `--memory off`)。

**この条件の数字は `docs/status.md` の表には載せない** (その表は経路どうしを比べる表で、条件が 1 行だけ違う数字を混ぜられない)。名前空間が作れない
機械ではスクリプトが「使えない」と印字して**終了コード 2** (名前空間の外で `--only connect-multi` を回したときも 2)。

## デプロイ先の測り方

**`docs/goals.md`・この文書・`docs/status.md` の数字はすべて手元の loopback**で、デプロイ先 (`nagoya.sorahost.net:50697`、Pterodactyl
コンテナ) で利用者が待っている時間は一度も測っていなかった (2026-09-10 の合議で分かった)。
デプロイ先で測るときの道具は 2 つで、**見ている場所が違う** (T12.0):

| 道具 | 見えるもの | 使うとき |
|---|---|---|
| `scripts/status-diff.py` | **プロキシから見た**ホスト別の待ち (`/status` の `hosts[]`) | 効きを面で見る。24 時間ぶんの差分 |
| `scripts/probe-deployed.sh` | **手元から見た**待ち (`curl -w`) | 1 経路を今すぐ確かめる。1 回 15 本・4 秒 |

**ホスト別統計は差分で見る** (`avg_ms` をそのまま読まない)。`hosts[]` は `.rrd` に永続化されて
**再起動をまたいで通算される**ので、直す前の 257 ms が何日も混ざり続ける。`avg_ms × timed` が
合計 ms なので、2 回取って `(avg2·timed2 − avg1·timed1) / (timed2 − timed1)` がその間の平均になる。

```bash
curl -s http://nagoya.sorahost.net:50697/status > a.json     # 直す前
# ... 24 時間 ...
curl -s http://nagoya.sorahost.net:50697/status > b.json     # 直した後
scripts/status-diff.py a.json b.json --aaaa aaaa.json        # 差分
scripts/status-diff.py b.json --aaaa aaaa.json               # 通算 (1 枚ならそのまま)
```

CONNECT (`connect://`) と forward を別の表にし、**AAAA の有無で 2 群に分けて中央値**を出す
(Happy Eyeballs の 250 ms を払うのは AAAA のあるホストだけなので、この 2 群の差がタスク 12 の主指標)。
**AAAA の判定はリゾルバ次第**なので、数字を残すときは引いた表を `--aaaa`
(`{"host": true/false}` の JSON) で固定する。既定は `getaddrinfo`、`--no-dns` で省ける。
合議のときはこの機械のリゾルバが www.google.com の AAAA を落として「無し」と出た
(同じ日に引き直したら 50 ホスト全部が dns.google の DoH の表と一致した)。
2026-09-10 05:37 UTC の 1 枚からは
**AAAA あり 25 ホスト・要求 6,022 (CONNECT の 74.5%)・avg の中央値 257.4 ms、
なし 22 ホスト・2,063 件・p50 の中央値 5.10 ms (avg 7.10)** が出る (`docs/measurements.md` の「デプロイ先の現在地 (2026-09-10)」の表と同じ)。

**差分の精度は `Δtimed` で決まる。** `/status` の `avg_ms` は小数第 1 位までなので、合計 ms の丸め誤差は
最悪 `0.05 × timed`。これが `Δtimed` で割られる。8 分あけた 2 枚では `Δtimed = 2` に対して
通算 `timed` が 3,930 あり、**誤差は ±196.6 ms** で桁が読めない。`Δavg_ms` の右にこの ± を出すので、
**幅が読みたい桁より小さくなるまで間隔をあける** (デプロイ先は 0.012 req/s = 80 秒に 1 件なので、
主要ホストで数十件貯めるのに数時間〜1 日)。**タスク 12 の受け入れ基準を 24 時間の差分で書くのはこのため。**

**手元からは `probe-deployed.sh`。** `time_appconnect − time_connect` が「CONNECT 確立 + TLS 握手」
(手元からプロキシまでの TCP は `time_connect` に入るので引き算で落ちる)。5 経路 × 3 回 = 15 本を
28 秒の予算つきで回し、予算を超えた行は `skip` と出す。**デプロイ先は利用者の本番なので、
確認に使う要求はこの 1 回分に留める。**

**この環境で 5% 未満の差は見えない** (回線のぶれが ±20 ms、`GET /status` の `time_connect` も
0.032〜0.190 秒と揺れる)。**タスク 12 の受け入れ基準は 5% ではなく桁で書く** (250 ms → 30 ms 未満、のように)。

**落とし穴 2 つ**:

- **forward の行はキャッシュ HIT のことがある。** `http://example.com/` は 3 回とも 0.070〜0.087 秒で、
  AAAA のあるホストなのに 250 ms を払っていない。`curl -D -` で見ると
  `X-Cache: HIT from rust-http-proxy (memory)` / `Age: 6239` で、**そもそも出て行っていない**。
  オリジンまでの待ちを見たいときは `-H 'Cache-Control: no-cache'` を足すか `X-Cache` を確かめる。
- **`/status` は窓が混在している。** `total_requests` は起動から、`hosts[]` は `.rrd` の通算。
  `status-diff.py` の見出しは両方を印字し、`uptime_secs` が減っていたら
  「**再起動をまたいでいる**」と出す (T12.4 (4) で `/status` 側に窓を書く)。

## ビルドのメモリの測り方

```bash
cargo clean && ./scripts/build-memory.sh 120                 # 120 MB の cgroup で通るか (2026-09-16 に 200 → 180、2026-09-26 に 180 → 120)
./scripts/build-memory.sh --find 80 85 90 95 100 110 120     # 通る最小の上限を探す (T14.55 で 135、T15.12 段 6' の後は 120〜125、T17.11 の後は 90。境目は揺れるので 2〜3 回回す)
gh workflow run ci.yml --ref main                             # CI の機械で測る (build-memory-find。T11.7)
```

`.cargo/config.toml` で `jobs = 1` にしてある。既定の並列数だと複数の `rustc` が同時に走り、その合計が上限を超えて
OOM killer に落とされる (実測: 200 MB の cgroup で、並列だと落ち、`-j 1` なら通る)。代償はビルド時間で 8 コアの機械で 14.9 → 32.3 秒。

**RSS の絶対値を上限と比べてはいけない** (T10.9 で理由がはっきりした)。rustc の VmRSS 154〜215 MB のうち
**110〜122 MB は `librustc_driver-*.so` (111 MB) をマップしたファイル由来のページ**で、ページキャッシュに載っていれば
その cgroup には課金されない。実測: **上限 110 MB の cgroup の中で通ったビルドでも `/usr/bin/time -v` の最大 RSS は
215 MB** と出る (メジャーフォールトは全クレート 0 = 1 ページも読みに行っていない)。cgroup が必ず抱えるのは
**RssAnon** の方で、こちらは 36〜86 MB。だからこのスクリプトは **実際にその上限の cgroup の中でビルドして通るか** を見る。
CI は毎回 120 MB で回す (T14.55 まで 200、T17.11 まで 180。ジョブ `build-memory`、ubuntu-24.04)。`scripts/build-memory.sh` が出す参考値も RssAnon 順にしてある (cgroup を作れない機械のときだけ出る)。

**上限を決めているのはどのクレートか** (T10.9)。`RUSTC_WRAPPER` に `/usr/bin/time -v` を噛ませ、
`/proc/<pid>/status` を 20 ms ごとに読んで rustc ごとに測った (3 回とも同じ順、±1 MB):

| クレート | RssAnon | VmRSS | 行 |
|---|---|---|---|
| **proxy-http** | **85.5 MB** | 205.3 MB | 1,992 |
| **proxy-blocklist** | **84.0 MB** | 205.8 MB | 812 |
| rust-http-proxy (本体) | 79.2 MB | 200.5 MB | 1,956 |
| proxy-metrics | 76.1 MB | 196.5 MB | 1,243 |
| proxy-net | 73.6 MB | 193.3 MB | 837 |
| proxy-cache | 67.0 MB | 186.0 MB | 2,257 |
| proxy-diskprobe (最小) | 35.7 MB | 138.7 MB | 514 |

上限を割ったときに rustc が落ちるのもこの上位で、**90 MB では `proxy-blocklist`、95 / 98 MB では `proxy-http`** の
ところで落ちた。**T9.2 が推定した「本体クレートが最大を決めている」は外れ** (本体は 3 番目)。**行数の順でもない**
(いちばん長い `proxy-cache` 2,257 行は 6 番目、`proxy-blocklist` は 812 行で 2 番目)。効くのは
「自分の行数 + 依存から単相化されてくる量」。**通る最小の上限 ≒ いちばん大きいクレートの RssAnon + 14〜20 MB** (T10.9: 85.5 + 14 ≒ 99。T14.55: 115.6 + 20 ≒ 135。T15.12 段 1〜3 の後: 121.7 + 18 ≒ 140。クレートごとの表は T14.55 と T15.12 の `結果:`)。
上限 200 MB に当たるにはいちばん大きいクレートの RssAnon が **2.2 倍 (85.5 → 186 MB)** にならないといけない。
下げたくなったら割るのは `proxy-http` と `proxy-blocklist`。

**手元でも判定できる** (T9.0)。この機械は**システムの systemd が動いていない** (`/run/systemd/system` が無く、
`systemd-run --scope` は "System has not been booted with systemd as init system" と言う。`sudo -n` 自体は通るので、
使えない理由は sudo ではない。T11.7 で確かめた) ためシステムの scope は作れないが、**ユーザーの systemd**
(`systemd-run --user --scope -p MemoryMax=…`) は動いていて上限も効く。スクリプトは
「システムの systemd → ユーザーの systemd → 参考の RSS だけ」の順に落ちる。
**いま通る最小は手元で 99〜100 MB** (T10.9。98 MB は 3 回とも落ち、99 MB と 100 MB は 5 回とも通った)。
**上限 200 MB に対して 100 MB (2.0 倍) の余裕がある。**

**CI で通る最小は 100 MB** (T11.7、run `34187654627`)。**手元の 99 MB とほぼ同じ**で、
**「手元は CI より 10 MB 低い」という長年の記述は実測で否定された**。CI の cgroup は
システムの systemd (`sudo systemd-run --scope`) で作られる (手元はユーザーの systemd)。
測り直したいときは `build-memory-find` ジョブを回す:`.github/workflows/ci.yml` の `build-memory-find` ジョブを
**手で回せば取れる**:

```bash
gh workflow run ci.yml --ref main                                  # 既定の段で測る
gh workflow run ci.yml --ref main -f ladder="95 100 105 110 115"   # 段を指定する
```

月曜 03:17 UTC の週 1 でも回る。結果は run の Summary に出て、同じ Summary に**測った runner の素性**
(image・arch・CPU・メモリ・どの systemd で cgroup を作れたか) も残る。push / PR では回らない
(毎回の関門は今までどおり 200 MB の `Build memory` だけ)。

**手元 (aarch64、ユーザーの systemd、8 コア / 6.6 GiB) と CI (`ubuntu-24.04`、x86_64、4 CPU / 15.6 GiB) は
機械もアーキテクチャも違うのに、通る最小は 99 と 100 MB でほぼ同じ**だった (T11.7)。
上限を決めているのが「いちばん大きいクレート 1 つの RssAnon + 14 MB」(T10.9) で、これは機械に依らない量だから。
古い「CI で 110 MB」は T7.4 (26 クレートに割った直後) の値で、タスク 8〜10 のコードが入っていなかった。

**手元の数字をどう読むか**: 手元の 99 MB は「**手元の機械で** 200 MB の関門にどれだけ余裕があるか」であって、
CI や動作環境 (Pterodactyl コンテナ) の値を代表しない。動作環境で通るかどうかの判定は **CI の 200 MB の関門**が
毎回やっている。手元の値が意味を持つのは「クレートを割る前と後」のような**同じ機械での前後比較**のとき。

**境目は薄い** (98 MB は落ち、99 MB は通る)。**必ず機械を占有して測ること** — 他のビルドが同時に走ると同じ上限でも落ちる
(T10.0 が 110 MB と出したのはこれが原因の可能性が高い)。
**落ちるクレートも回ごとに動く** (T10.9 は 95/98 MB で `proxy-http`、T11.7 の再確認では 98 MB で `proxy-blocklist`。
2 つの RssAnon は 85.5 と 84.0 で 1.5 MB しか違わない)。
