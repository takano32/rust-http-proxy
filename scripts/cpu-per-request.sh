#!/bin/bash
# CPU/要求 を測る (TASKS.md §1 の手順をそのまま自動化したもの)。
#
# プロキシを big コア (cpu4-7) に、ベンチを LITTLE コア (cpu0-3) に固定して回し、
# `/proc/<pid>/stat` の utime + stime の差をベンチの操作数で割る。
# 主指標は **CPU/要求** で、スループットは律速がベンチ側に移るので補助でしかない。
#
# 使い方:
#   scripts/cpu-per-request.sh [--only forward|connect|tunnel|idle-tunnels] [bench の残りの引数...]
#     既定は --only forward --conc 8 --seconds 10
#   例:
#     scripts/cpu-per-request.sh                                  # keep-alive の forward
#     scripts/cpu-per-request.sh --only forward --no-keepalive    # 1 接続 1 要求
#     scripts/cpu-per-request.sh --only connect                   # CONNECT の確立
#     PROXY_MAX_CONNS=8192 scripts/cpu-per-request.sh --only idle-tunnels --conc 5000
#                                                                 # アイドルトンネルを握る
#     PROXY_ARGS="" PROXY_MEM_CACHE_MB=64 PROXY_CACHE_DIR=/tmp/pc \
#       scripts/cpu-per-request.sh --cacheable                    # キャッシュ HIT
#
# 環境変数:
#   PROXY_CPUS (既定 4-7) / BENCH_CPUS (既定 0-3) / PORT (既定 18080)
#   PROXY_ARGS (既定 "--lite"。空にすると既定プロファイルで、環境変数がそのまま効く)
#   BIN / BENCH (既定 target/release/{rust-http-proxy,bench})
set -u
cd "$(dirname "$0")/.."
PROXY_CPUS=${PROXY_CPUS:-4-7}
BENCH_CPUS=${BENCH_CPUS:-0-3}
PORT=${PORT:-18080}
BIN=${BIN:-target/release/rust-http-proxy}
BENCH=${BENCH:-target/release/bench}
PROXY_ARGS=${PROXY_ARGS-"--lite"}

ONLY=forward
ARGS=()
CONC_GIVEN=0
SECS_GIVEN=0
while [ $# -gt 0 ]; do
  case "$1" in
    --only) ONLY=$2; shift 2 ;;
    --conc) CONC_GIVEN=1; ARGS+=("$1" "$2"); shift 2 ;;
    --seconds) SECS_GIVEN=1; ARGS+=("$1" "$2"); shift 2 ;;
    *) ARGS+=("$1"); shift ;;
  esac
done
[ $CONC_GIVEN -eq 1 ] || ARGS+=(--conc 8)
[ $SECS_GIVEN -eq 1 ] || ARGS+=(--seconds 10)

for b in "$BIN" "$BENCH"; do
  [ -x "$b" ] || { echo "not built: $b" >&2; exit 1; }
done

work=$(mktemp -d)
trap 'kill $pid 2>/dev/null; wait $pid 2>/dev/null; rm -rf "$work"' EXIT
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
             printf 'GET /status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n' >&3 &&
             cat <&3; } 2>/dev/null | sed -n 's/.*"parked_connections":\([0-9]*\).*/\1/p' | tail -1)
      if [ -n "$p" ] && [ "$p" -gt "$pk" ]; then pk=$p; echo "$pk" >"$work/parked.max"; fi
    fi
    sleep 0.1
  done
) &
sampler=$!
out=$(taskset -c "$BENCH_CPUS" "$BENCH" --proxy "127.0.0.1:$PORT" --only "$ONLY" "${ARGS[@]}")
rc=$?
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

# tunnel は「操作数」ではなく運んだ MiB で割る (256 MiB を 1 本で通す)
if [ "$ONLY" = tunnel ]; then
  ops=$(echo "$out" | sed -n 's/.*(\([0-9]*\) MiB through.*/\1/p' | tail -1)
  unit=MiB
else
  ops=$(echo "$out" | sed -n 's/.*(\([0-9]*\) ops in.*/\1/p' | tail -1)
  unit=$([ "$ONLY" = forward ] && echo req || echo op)
fi
[ -n "$ops" ] && [ "$ops" -gt 0 ] || { echo "could not read the number of operations"; exit 1; }
awk -v u=$((u1 - u0)) -v s=$((s1 - s0)) -v ops="$ops" -v tick="$tick" -v hwm="$hwm" \
    -v t0="$threads0" -v t1="$threads1" -v tmax="$threads_max" -v tmid="$threads_mid" \
    -v parked="${parked:-0}" \
    -v unit="$unit" 'BEGIN {
  us = 1e6 / tick
  printf "CPU/%s: %.2f us (user %.2f / kernel %.2f)  %s %d  proxy peak RSS %.1f MB  threads %d -> %d (max %d, median %d)  parked max %d\n",
    unit, (u + s) * us / ops, u * us / ops, s * us / ops, unit, ops, hwm / 1024, t0, t1, tmax, tmid, parked
}'
