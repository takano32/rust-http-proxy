#!/bin/bash
# CPU/要求 を測る (TASKS.md §1 の手順をそのまま自動化したもの)。
#
# プロキシを big コア (cpu4-7) に、ベンチを LITTLE コア (cpu0-3) に固定して回し、
# `/proc/<pid>/stat` の utime + stime の差をベンチの操作数で割る。
# 主指標は **CPU/要求** で、スループットは律速がベンチ側に移るので補助でしかない。
#
# **どの経路も同じ条件で測る** (T10.11)。プロファイルは `--lite`、ログ水準は `warn`、
# キャッシュの ballast は off で揃える。§2 は「経路どうしを比べる表」なので、
# 1 行だけ条件が違うと比べられない数字が並ぶ。以前は `--cacheable` (キャッシュ HIT) だけ
# 「`PROXY_ARGS=""` = 既定プロファイル = ログ **info**」で回す手順になっていて、
# HIT の行にだけアクセスログ 1 行 (T10.10 の実測で 7.1 us/要求) が乗っていた。
# **キャッシュは `--cacheable` を渡せばこのスクリプトが自分で入れる** (`--lite` のまま
# `PROXY_CACHE_ENABLED=on`)。条件は毎回 1 行目に印字するので、出力を見れば再現できる。
#
# **`--only tunnel` だけは既定の配置が違う** (プロキシ cpu4-5 / ベンチ cpu6-7。T10.8 で実測して決めた)。
# この経路のベンチは blaster (送る) と reader (受ける) の 2 スレッドがどちらも本気で回るので、
# LITTLE に置くと**ベンチが先に頭打ちになってプロキシの実力が見えない** (実測 1.20 → 2.42 GiB/s)。
# それでも律速はベンチ側のまま (プロキシは `splice` でコピー 0 回、ベンチは送り+受けでコピー 2 回)。
# 出力の `proxy NN% of one core` が 100% に届かない限り、MiB/s はプロキシの上限ではない。
#
# 使い方:
#   scripts/cpu-per-request.sh [--only forward|connect|tunnel|idle-tunnels|idle-conns] [bench の残りの引数...]
#     既定は --only forward --conc 8 --seconds 10
#   例:
#     scripts/cpu-per-request.sh                                  # keep-alive の forward
#     scripts/cpu-per-request.sh --only forward --no-keepalive    # 1 接続 1 要求
#     scripts/cpu-per-request.sh --only connect                   # CONNECT の確立
#     scripts/cpu-per-request.sh --only tunnel --conc 1           # トンネル 1 本 (主指標は CPU/MiB)
#       ← **§2 の「トンネル」の行はこの `--conc 1` の値**。`--conc N` にすると N 本を
#         並列に張って運んだ MiB の合計で割るので、別の数字になる (比べるときは本数を揃える)
#     scripts/cpu-per-request.sh --cacheable                      # キャッシュ HIT
#     PROXY_MAX_CONNS=8192 scripts/cpu-per-request.sh --only idle-tunnels --conc 5000
#                                                                 # アイドルトンネルを握る
#     PROXY_MAX_CONNS=8192 scripts/cpu-per-request.sh --only idle-conns --conc 2000
#                                                                 # 暇な keep-alive 接続を握る
#
# **`--only idle-tunnels` / `--only idle-conns` で見るのはスレッド数と RSS** (CPU/op ではない)。
# どちらも「確立 + 保持 + 終わりの一斉 close」の合計を本数で割った値になるので CPU は弱い指標
# (T8.1)。読むのは `threads ... (max N, median M)` と `proxy peak RSS` と `parked max`。
# `--only idle-conns` の握る秒数は **プロキシの `PROXY_KEEPALIVE_SECS` (既定 15 秒) より短く**
# すること (越えると預かり所が期限切れで閉じる)。ベンチが最後に「まだ生きている本数」を出す。
# 「預けない場合」(預ける前の姿) を測るには `PROXY_PARK_IDLE=off PROXY_MAX_THREADS=0` を足す。
# **上限 (T10.5 の auto = 256) を外さないと止まる**: 預けないと 1 接続が 1 スレッドを握ったままなので、
# 257 本目からは待ち行列に入って応答が返らない。
#
# 環境変数 (どれも「明示されたら上書き」。既定のままなら上の「同じ条件」で回る):
#   PROXY_CPUS (既定 4-7、tunnel だけ 4-5) / BENCH_CPUS (既定 0-3、tunnel だけ 6-7) / PORT (既定 18080)
#   PROXY_ARGS (既定 "--lite"。空にすると既定プロファイル = キャッシュ・統計・ダッシュボードあり)
#   PROXY_LOG_LEVEL (既定 warn。**info にするとアクセスログのぶんだけ重くなる**ので、
#                    比べる表に載せる数字は warn で揃えること)
#   PROXY_CACHE_ENABLED (--cacheable のときだけ既定 on) / PROXY_MEM_CACHE_MB / PROXY_DISK_CACHE_MB (既定 64)
#   PROXY_CACHE_RESERVE (既定 off) / PROXY_CACHE_DIR (既定は使い捨ての作業ディレクトリの下)
#     ← reserve を on のまま既定プロファイルを測ると、HOME (= mktemp -d = tmpfs) に GB 単位の
#       ballast ができて RSS が跳ね、CPU/要求 が 62〜103 us の間で暴れる (T10.10 の落とし穴)
#   BIN / BENCH (既定 target/release/{rust-http-proxy,bench})
set -u
cd "$(dirname "$0")/.."
PORT=${PORT:-18080}
BIN=${BIN:-target/release/rust-http-proxy}
BENCH=${BENCH:-target/release/bench}
PROXY_ARGS=${PROXY_ARGS-"--lite"}

ONLY=forward
ARGS=()
CONC_GIVEN=0
SECS_GIVEN=0
CACHEABLE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --only) ONLY=$2; shift 2 ;;
    --conc) CONC_GIVEN=1; ARGS+=("$1" "$2"); shift 2 ;;
    --seconds) SECS_GIVEN=1; ARGS+=("$1" "$2"); shift 2 ;;
    --cacheable) CACHEABLE=1; ARGS+=("$1"); shift ;;
    *) ARGS+=("$1"); shift ;;
  esac
done
[ $CONC_GIVEN -eq 1 ] || ARGS+=(--conc 8)
[ $SECS_GIVEN -eq 1 ] || ARGS+=(--seconds 10)

# コアの割り当ては測る種類で変える (上の説明を参照)。tunnel はベンチも big に置く。
if [ "$ONLY" = tunnel ]; then
  PROXY_CPUS=${PROXY_CPUS:-4-5}
  BENCH_CPUS=${BENCH_CPUS:-6-7}
else
  PROXY_CPUS=${PROXY_CPUS:-4-7}
  BENCH_CPUS=${BENCH_CPUS:-0-3}
fi

# 経路をまたいで揃える条件 (T10.11)。明示されていればそちらが勝つ。
export PROXY_LOG_LEVEL=${PROXY_LOG_LEVEL:-warn}
export PROXY_CACHE_RESERVE=${PROXY_CACHE_RESERVE:-off}
if [ $CACHEABLE -eq 1 ]; then
  # `--lite` はキャッシュを既定で off にするが、明示指定の方が勝つ (config.rs)。
  # ここで入れることで、HIT の行も他の行と同じ `--lite` / warn のまま測れる。
  export PROXY_CACHE_ENABLED=${PROXY_CACHE_ENABLED:-on}
  export PROXY_MEM_CACHE_MB=${PROXY_MEM_CACHE_MB:-64}
  export PROXY_DISK_CACHE_MB=${PROXY_DISK_CACHE_MB:-64}
fi

for b in "$BIN" "$BENCH"; do
  [ -x "$b" ] || { echo "not built: $b" >&2; exit 1; }
done

work=$(mktemp -d)
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null; rm -rf "$work"' EXIT
export PROXY_CACHE_DIR=${PROXY_CACHE_DIR:-$work/cache}
# 1 行目に「何をどの条件で測ったか」を出す。§2 の行はこの 1 行で再現できる。
if [ $CACHEABLE -eq 1 ]; then
  cache_desc="cache=$PROXY_CACHE_ENABLED (${PROXY_MEM_CACHE_MB}MB mem / ${PROXY_DISK_CACHE_MB}MB disk)"
else
  cache_desc="cache=(profile default)"
fi
echo "run: ${PROXY_ARGS:-(default profile)} PROXY_LOG_LEVEL=$PROXY_LOG_LEVEL" \
  "$cache_desc reserve=$PROXY_CACHE_RESERVE | --only $ONLY ${ARGS[*]}" \
  "| proxy cpu$PROXY_CPUS / bench cpu$BENCH_CPUS | $BIN"
# shellcheck disable=SC2086
HOME=$work PROXY_ALLOW_LOCAL=on PROXY_STATS_PERSIST=off \
  taskset -c "$PROXY_CPUS" "$BIN" $PROXY_ARGS -p "$PORT" --bind 127.0.0.1 >"$work/proxy.log" 2>&1 &
pid=$!
for _ in $(seq 1 50); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then break; fi
  sleep 0.1
done
kill -0 $pid 2>/dev/null || { echo "proxy did not start:"; cat "$work/proxy.log"; exit 1; }

tick=$(getconf CLK_TCK)
read -r u0 s0 < <(awk '{print $14, $15}' "/proc/$pid/stat")
threads0=$(awk '/^Threads/{print $2}' "/proc/$pid/status")
# ベンチの実行中のスレッド数を 100 ms ごとに数えて記録する。開始と終了の値だけでは
# 「5,000 本のトンネルを握っている最中」の姿が見えない (終わるころには片づいている)。
# 最大と**中央値**の両方を出す: 最大はベンチ終了時の一斉 close で跳ねることがあり、
# 「握っている間ずっと何本だったか」は中央値の方が素直に出る。
# 預かっている接続の数 (`/status` の parked_connections) は 1 秒ごとに最大を取る
: >"$work/samples"
echo 0 >"$work/parked.max"
touch "$work/sampling"
(
  pk=0
  i=0
  while [ -e "$work/sampling" ]; do
    t=$(awk '/^Threads/{print $2}' "/proc/$pid/status" 2>/dev/null)
    [ -n "$t" ] && echo "$t" >>"$work/samples"
    i=$((i + 1))
    if [ $((i % 10)) -eq 0 ]; then
      p=$( { exec 3<>"/dev/tcp/127.0.0.1/$PORT" &&
             printf 'GET /status HTTP/1.1\r\nHost: 127.0.0.1:%s\r\nConnection: close\r\n\r\n' "$PORT" >&3 &&
             cat <&3; } 2>/dev/null | sed -n 's/.*"parked_connections":\([0-9]*\).*/\1/p' | tail -1)
      if [ -n "$p" ] && [ "$p" -gt "$pk" ]; then pk=$p; echo "$pk" >"$work/parked.max"; fi
    fi
    sleep 0.1
  done
) &
sampler=$!
# ベンチ自身の CPU も測る (このベンチが律速していないかを見るため)。
# `time` は bash の組み込みで、外部コマンドを増やさずに子の user / sys / 実時間が取れる。
TIMEFORMAT='%3R %3U %3S'
{ time taskset -c "$BENCH_CPUS" "$BENCH" --proxy "127.0.0.1:$PORT" --only "$ONLY" \
    "${ARGS[@]}" >"$work/bench.out" 2>&1; } 2>"$work/bench.time"
rc=$?
out=$(cat "$work/bench.out")
read -r real buser bsys <"$work/bench.time"
read -r u1 s1 < <(awk '{print $14, $15}' "/proc/$pid/stat")
hwm=$(awk '/^VmHWM/{print $2}' "/proc/$pid/status")
threads1=$(awk '/^Threads/{print $2}' "/proc/$pid/status")
rm -f "$work/sampling"
wait $sampler 2>/dev/null
parked=$(cat "$work/parked.max")
read -r threads_max threads_mid < <(sort -n "$work/samples" | awk -v t0="$threads0" '
  { v[NR] = $1 }
  END {
    if (NR == 0) { print t0, t0; exit }
    print v[NR], v[int((NR + 1) / 2)]
  }')
echo "$out"
[ $rc -eq 0 ] || { echo "bench failed (exit $rc)"; exit $rc; }

# tunnel は「操作数」ではなく運んだ MiB で割る (`--conc` 本のトンネルに `--seconds` 秒流した合計)
if [ "$ONLY" = tunnel ]; then
  ops=$(echo "$out" | sed -n 's/.*(\([0-9]*\) MiB through.*/\1/p' | tail -1)
  unit=MiB
else
  ops=$(echo "$out" | sed -n 's/.*(\([0-9]*\) ops in.*/\1/p' | tail -1)
  unit=$([ "$ONLY" = forward ] && echo req || echo op)
fi
[ -n "$ops" ] && [ "$ops" -gt 0 ] || { echo "could not read the number of operations"; exit 1; }
# `proxy NN% of one core` はプロキシが使い切ったコアの数 (100% = 1 コアを丸ごと)。
# ベンチ側と見比べて、**どちらが律速しているか**をその場で判断するためのもの。
awk -v u=$((u1 - u0)) -v s=$((s1 - s0)) -v ops="$ops" -v tick="$tick" -v hwm="$hwm" \
    -v t0="$threads0" -v t1="$threads1" -v tmax="$threads_max" -v tmid="$threads_mid" \
    -v parked="${parked:-0}" -v real="${real:-0}" -v bu="${buser:-0}" -v bs="${bsys:-0}" \
    -v unit="$unit" 'BEGIN {
  us = 1e6 / tick
  bench = bu + bs
  printf "CPU/%s: %.2f us (user %.2f / kernel %.2f)  %s %d  proxy peak RSS %.1f MB  threads %d -> %d (max %d, median %d)  parked max %d\n",
    unit, (u + s) * us / ops, u * us / ops, s * us / ops, unit, ops, hwm / 1024, t0, t1, tmax, tmid, parked
  if (real > 0)
    printf "  proxy %.0f%% of one core  |  bench %.2f us/%s (%.0f%% of one core, user %.2f / sys %.2f)  in %.2fs\n",
      (u + s) / tick / real * 100, bench * 1e6 / ops, unit, bench / real * 100, bu, bs, real
}'
