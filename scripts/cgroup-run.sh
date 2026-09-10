#!/bin/bash
# 小さい cgroup の中でプロキシを走らせ、cgroup のメモリと `/status` を 1 秒ごとに記録する
# (T12.5。デプロイ先 = Pterodactyl コンテナ 256 MiB と同じ形を手元で作るための道具)。
#
# `scripts/build-memory.sh` の `run_in_cgroup` と同じく `systemd-run --user --scope` で
# `MemoryMax` / `MemorySwapMax=0` の scope を作る。ただしこの機械では scope の cgroup
# ディレクトリが**自分の cgroup 名前空間の外**にできるので (`/proc/self/cgroup` が
# `/../../../app.slice/<scope>` になる)、`/sys/fs/cgroup` からは読めない。
# そこで scope の中で `unshare -C -m -U --map-root-user` して cgroup2 を貼り直す:
#
#   - **`memory.peak` / `memory.events` (`oom_kill`) がその場で読める**
#   - **プロキシ自身も上限 256 MiB の cgroup を見る** (`crates/sysinfo` は
#     `/proc/self/cgroup` の道を `/sys/fs/cgroup` の下でたどる)。デプロイ先の
#     コンテナと同じ条件になる。名前空間を貼らないと上限が見えず、
#     `SERVER_MEMORY` だけで予算が決まってしまい PSI も cgroup のものが読めない
#
# ベンチは **scope の外**で走らせる (ベンチのメモリを cgroup に数えない)。
#
# 使い方:
#   scripts/cgroup-run.sh [オプション] --bench "--only tunnel --conc 128 --seconds 60" ...
#
#   --limit-mb N     cgroup の MemoryMax (既定 256)
#   --port N         プロキシの待ち受け (既定 18080)
#   --warmup N       ベンチの前にバラストを満たすまで待つ秒数 (既定 45)
#   --bench "ARGS"   ベンチの引数。複数指定すると順に走る
#   --fill-mb N      キャッシュできるオリジンを立てて N MiB ぶん保存させる (0 = しない)
#   --fill-object-kb K  その 1 件の大きさ (既定 64)
#   --hog-mb N       **cgroup の中で**別プロセスに N MiB 確保させる (圧迫の試験。0 = しない)
#   --hog-secs S     その保持時間 (既定 15)
#   --hog-step-mib K --hog-step-ms M
#                    一度にではなく K MiB ずつ M ミリ秒おきに確保する (プローブに追いつく暇を与える)
#   --out DIR        記録の出力先 (既定は使い捨ての作業ディレクトリ)
#   --keep           終わっても作業ディレクトリを消さない
#
# 環境変数:
#   BIN / BENCH        (既定 target/release/{rust-http-proxy,bench})
#   PROXY_ARGS         (既定は空 = 既定プロファイル。`--lite` を渡すと軽量プロファイル)
#   PROXY_CPUS         (既定 4-7) / BENCH_CPUS (既定 0-3)
#   NOFILE             (既定 1024。scope の中で `ulimit -n` に使う)
#   その他 PROXY_* / SERVER_MEMORY は**そのまま**プロキシに渡る
set -u
cd "$(dirname "$0")/.."

LIMIT_MB=256
PORT=${PORT:-18080}
WARMUP=45
OUT=""
KEEP=0
BENCHES=()
BIN=${BIN:-target/release/rust-http-proxy}
BENCH=${BENCH:-target/release/bench}
PROXY_ARGS=${PROXY_ARGS-}
PROXY_CPUS=${PROXY_CPUS:-4-7}
BENCH_CPUS=${BENCH_CPUS:-0-3}
NOFILE=${NOFILE:-1024}
FILL_MB=0
FILL_OBJ_KB=64
HOG_MB=0
HOG_SECS=15
HOG_STEP_MIB=0
HOG_STEP_MS=0

while [ $# -gt 0 ]; do
  case "$1" in
    --limit-mb) LIMIT_MB=$2; shift 2 ;;
    --port) PORT=$2; shift 2 ;;
    --warmup) WARMUP=$2; shift 2 ;;
    --bench) BENCHES+=("$2"); shift 2 ;;
    --fill-mb) FILL_MB=$2; shift 2 ;;
    --fill-object-kb) FILL_OBJ_KB=$2; shift 2 ;;
    --hog-mb) HOG_MB=$2; shift 2 ;;
    --hog-secs) HOG_SECS=$2; shift 2 ;;
    --hog-step-mib) HOG_STEP_MIB=$2; shift 2 ;;
    --hog-step-ms) HOG_STEP_MS=$2; shift 2 ;;
    --out) OUT=$2; shift 2 ;;
    --keep) KEEP=1; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

for b in "$BIN" "$BENCH"; do
  [ -x "$b" ] || { echo "not built: $b" >&2; exit 1; }
done
command -v systemd-run >/dev/null || { echo "systemd-run がありません" >&2; exit 2; }

work=$(mktemp -d)
[ -n "$OUT" ] || OUT=$work
mkdir -p "$OUT"
unit="rhp-t125-$$.scope"

cleanup() {
  touch "$work/stop" 2>/dev/null
  sleep 1
  pkill -INT -x rust-http-proxy 2>/dev/null
  sleep 1
  pkill -x rust-http-proxy 2>/dev/null
  systemctl --user stop "$unit" 2>/dev/null
  [ $KEEP -eq 1 ] || rm -rf "$work"
}
trap cleanup EXIT

# ---- scope の中で走らせるもの (プロキシ + cgroup のサンプラ) -------------------
cat >"$work/inner.sh" <<'INNER'
set -u
work=$1; bin=$2; port=$3; cpus=$4; nofile=$5; shift 5
# cgroup2 を貼り直して、この scope 自身の cgroup を /sys/fs/cgroup として見せる
mount -t cgroup2 none /sys/fs/cgroup || echo "mount cgroup2 failed" >>"$work/inner.log"
ulimit -n "$nofile"
# shellcheck disable=SC2086
taskset -c "$cpus" "$bin" $PROXY_ARGS -p "$port" --bind 127.0.0.1 >"$work/proxy.log" 2>&1 &
pid=$!
echo "$pid" >"$work/proxy.pid"
# 1 秒ごとに cgroup の数字を書き出す (fork するのは sleep だけ)
: >"$work/cg.tsv"
while [ ! -e "$work/stop" ]; do
  kill -0 "$pid" 2>/dev/null || echo "proxy is gone" >>"$work/inner.log"
  # 圧迫の試験: cgroup の中の別プロセスにメモリを確保させる (呼び出し側が hog を置いたとき)
  if [ -e "$work/hog" ] && [ ! -e "$work/hog.started" ]; then
    touch "$work/hog.started"
    read -r hmb hsecs hstep hms <"$work/hog"
    python3 -c '
import os, sys, time
mib, secs, step, ms = (int(a) for a in sys.argv[1:5])
step = step or mib


def rss():
    for line in open("/proc/self/status"):
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) // 1024
    return 0


held = []
got = 0
while got < mib:
    n = min(step, mib - got) * 1024 * 1024
    b = bytearray(n)
    for i in range(0, n, 4096):
        b[i] = 1
    held.append(b)
    got += n // 1024 // 1024
    print("hog: +%d -> %d MiB (self RSS %d MiB)" % (n // 1048576, got, rss()), flush=True)
    if ms:
        time.sleep(ms / 1000.0)
time.sleep(secs)
' "$hmb" "$hsecs" "$hstep" "$hms" >>"$work/hog.log" 2>&1 &
  fi
  read -r cur </sys/fs/cgroup/memory.current || cur=0
  read -r peak </sys/fs/cgroup/memory.peak || peak=0
  oom=0; oomg=0; mx=0
  while read -r k v; do
    case "$k" in oom_kill) oom=$v ;; oom_group_kill) oomg=$v ;; oom) : ;; max) mx=$v ;; esac
  done </sys/fs/cgroup/memory.events
  anon=0; file=0
  while read -r k v; do
    case "$k" in anon) anon=$v ;; file) file=$v ;; esac
  done </sys/fs/cgroup/memory.stat
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$EPOCHSECONDS" "$cur" "$peak" "$oom" "$oomg" "$mx" "$anon" "$file" >>"$work/cg.tsv"
  sleep 1
done
# 最後の 1 行 (プロキシが死んでいても cgroup の記録は残す)
read -r cur </sys/fs/cgroup/memory.current || cur=0
read -r peak </sys/fs/cgroup/memory.peak || peak=0
oom=0; oomg=0; mx=0
while read -r k v; do
  case "$k" in oom_kill) oom=$v ;; oom_group_kill) oomg=$v ;; max) mx=$v ;; esac
done </sys/fs/cgroup/memory.events
printf 'final\t%s\t%s\t%s\t%s\t%s\t0\t0\n' "$cur" "$peak" "$oom" "$oomg" "$mx" >>"$work/cg.tsv"
kill -INT "$pid" 2>/dev/null
wait "$pid" 2>/dev/null
echo "proxy exited: $?" >>"$work/inner.log"
INNER

# ---- 1 秒ごとに /status と cgroup を 1 枚の表にするサンプラ (scope の外) --------
cat >"$work/sample.py" <<'PY'
import json, os, socket, sys, time

work, out, port = sys.argv[1], sys.argv[2], int(sys.argv[3])
MB = 1024 * 1024


def status(port):
    s = socket.create_connection(("127.0.0.1", port), 2)
    s.sendall(("GET /status HTTP/1.1\r\nHost: 127.0.0.1:%d\r\nConnection: close\r\n\r\n" % port).encode())
    buf = b""
    while True:
        b = s.recv(65536)
        if not b:
            break
        buf += b
    s.close()
    return json.loads(buf.split(b"\r\n\r\n", 1)[1])


def cg(work):
    try:
        with open(os.path.join(work, "cg.tsv")) as f:
            rows = [r for r in f.read().splitlines() if r]
        return rows[-1].split("\t") if rows else None
    except OSError:
        return None


def rss(work):
    try:
        pid = open(os.path.join(work, "proxy.pid")).read().strip()
        for line in open("/proc/%s/status" % pid):
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    except (OSError, ValueError):
        pass
    return 0


hdr = ("t phase rss_mib cg_cur_mib cg_peak_mib oom anon_mib file_mib "
       "mem_used_mib mem_lim_mib mem_res_mib dsk_used_mib dsk_lim_mib dsk_res_mib "
       "ent stores thr act psi").split()
f = open(out, "w", buffering=1)
f.write("\t".join(hdr) + "\n")
t0 = time.time()
while not os.path.exists(os.path.join(work, "stop")):
    now = time.time()
    try:
        st = status(port)
    except Exception:
        st = None
    c = cg(work)
    phase = "-"
    try:
        phase = open(os.path.join(work, "phase")).read().strip() or "-"
    except OSError:
        pass
    def g(d, *ks):
        for k in ks:
            if d is None:
                return 0
            d = d.get(k)
            if d is None:
                return 0
        return d
    m = g(st, "cache", "memory") or {}
    dk = g(st, "cache", "disk") or {}
    sysm = g(st, "cache", "system") or {}
    row = [
        "%.0f" % (now - t0), phase,
        "%.1f" % (rss(work) / MB),
        "%.1f" % ((int(c[1]) / MB) if c and c[0] != "final" else 0),
        "%.1f" % ((int(c[2]) / MB) if c else 0),
        (c[3] if c else "?"),
        "%.1f" % ((int(c[6]) / MB) if c and c[0] != "final" else 0),
        "%.1f" % ((int(c[7]) / MB) if c and c[0] != "final" else 0),
        "%.1f" % ((m.get("used_bytes") or 0) / MB),
        "%.1f" % ((m.get("limit_bytes") or 0) / MB),
        "%.1f" % ((m.get("reserved_bytes") or 0) / MB),
        "%.1f" % ((dk.get("used_bytes") or 0) / MB),
        "%.1f" % ((dk.get("limit_bytes") or 0) / MB),
        "%.1f" % ((dk.get("reserved_bytes") or 0) / MB),
        str(m.get("entries") or 0),
        str(g(st, "cache", "stores")),
        str(g(st, "live_threads")),
        str(g(st, "active_connections")),
        str(sysm.get("mem_pressure")),
    ]
    f.write("\t".join(row) + "\n")
    if st is not None:
        with open(out + ".last.json", "w") as g:
            json.dump(st, g, indent=1)
    time.sleep(max(0.05, 1.0 - (time.time() - now)))
f.close()
PY

# ---- キャッシュを埋める道具 (オリジンもクライアントも scope の外) ---------------
cat >"$work/fill.py" <<'PY'
"""キャッシュできるオリジンを立て、別々の URL を順に取ってキャッシュを埋める。

ベンチのオリジンは URL が 1 つしかないので、予算いっぱいまで保存させるには使えない。
`PROXY_CACHE_ADMISSION` (層が 90% 埋まると 2 回目の要求からしか保存しない) に当たるので
**同じ URL を 2 回ずつ**取る。
"""
import socket, sys, threading, time

proxy_port, total_mb, obj_kb = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
body = b"x" * (obj_kb * 1024)
head = (b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n"
        b"Content-Length: %d\r\nCache-Control: max-age=600\r\n\r\n" % len(body))
resp = head + body

srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 0))
srv.listen(128)
origin_port = srv.getsockname()[1]


def serve():
    while True:
        try:
            c, _ = srv.accept()
        except OSError:
            return
        threading.Thread(target=one, args=(c,), daemon=True).start()


def one(c):
    c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    f = c.makefile("rb")
    try:
        while True:
            line = f.readline()
            if not line:
                return
            while True:
                h = f.readline()
                if h in (b"\r\n", b"\n", b""):
                    break
            c.sendall(resp)
    except OSError:
        pass
    finally:
        c.close()


threading.Thread(target=serve, daemon=True).start()

n = max(1, total_mb * 1024 // obj_kb)
print("fill: %d objects x %d KiB = %d MiB via 127.0.0.1:%d" %
      (n, obj_kb, n * obj_kb // 1024, origin_port), flush=True)
t0 = time.time()
done = 0
bad = 0
c = f = None
# プロキシは 1 接続 1000 要求で閉じる (`MAX_REQUESTS_PER_CONNECTION`) ので張り直しながら回す
for rnd in (1, 2):
    for i in range(n):
        url = "http://127.0.0.1:%d/obj/%d" % (origin_port, i)
        req = ("GET %s HTTP/1.1\r\nHost: 127.0.0.1:%d\r\n\r\n" % (url, origin_port)).encode()
        for attempt in (1, 2):
            if c is None:
                c = socket.create_connection(("127.0.0.1", proxy_port))
                f = c.makefile("rb")
            try:
                c.sendall(req)
                line = f.readline()
                if not line:
                    raise OSError("closed")
                clen = 0
                while True:
                    h = f.readline()
                    if h in (b"\r\n", b"\n", b""):
                        break
                    if h.lower().startswith(b"content-length:"):
                        clen = int(h.split(b":")[1])
                left = clen
                while left > 0:
                    left -= len(f.read(left))
                break
            except OSError:
                try:
                    c.close()
                except OSError:
                    pass
                c = f = None
                line = b""
        if not line.startswith(b"HTTP/1.1 200"):
            bad += 1
            if bad <= 5:
                print("fill: %s -> %r" % (url, line[:40]), flush=True)
        done += 1
if c is not None:
    c.close()
print("fill: %d requests (%d not 200) in %.1fs" % (done, bad, time.time() - t0), flush=True)
PY

echo "run: MemoryMax=${LIMIT_MB}M MemorySwapMax=0 ulimit -n $NOFILE | proxy ${PROXY_ARGS:-(default profile)} cpu$PROXY_CPUS | bench cpu$BENCH_CPUS | out=$OUT"
env | grep -E '^(SERVER_MEMORY|PROXY_)' | sort | sed 's/^/  env: /'

: >"$work/phase"; echo warmup >"$work/phase"
# scope は systemd (ユーザーマネージャ) が起こすので、呼び出し元の環境は**引き継がれない**。
# プロキシに効く変数は -E で明示的に渡す (build-memory.sh が PATH/HOME でやっているのと同じ)。
envargs=()
while read -r v; do
  envargs+=(-E "$v=${!v}")
done < <(compgen -e | grep -E '^(SERVER_MEMORY|SERVER_DISK|PROXY_[A-Z0-9_]+|HOME|PATH|TMPDIR)$')
envargs+=(-E "PROXY_ARGS=$PROXY_ARGS")
systemd-run --user --scope --quiet --unit="$unit" \
  -p "MemoryMax=${LIMIT_MB}M" -p MemorySwapMax=0 "${envargs[@]}" \
  unshare -C -m -U --map-root-user \
  bash "$work/inner.sh" "$work" "$PWD/$BIN" "$PORT" "$PROXY_CPUS" "$NOFILE" &
scope=$!

for _ in $(seq 1 100); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then break; fi
  sleep 0.2
done
if ! (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
  echo "proxy did not start:"; cat "$work/proxy.log" "$work/inner.log" 2>/dev/null; exit 1
fi

python3 "$work/sample.py" "$work" "$OUT/samples.tsv" "$PORT" &
sampler=$!

echo "warmup ${WARMUP}s (バラストが満ちるのを待つ)"
sleep "$WARMUP"

i=0
for b in ${BENCHES[@]+"${BENCHES[@]}"}; do
  i=$((i + 1))
  label="bench$i"
  echo "$label" >"$work/phase"
  echo "== $label: bench $b"
  # shellcheck disable=SC2086
  taskset -c "$BENCH_CPUS" "$BENCH" --proxy "127.0.0.1:$PORT" $b 2>&1 | sed "s/^/  /"
  echo cooldown >"$work/phase"
  sleep 3
done

if [ "$FILL_MB" -gt 0 ]; then
  echo fill >"$work/phase"
  echo "== fill: ${FILL_MB} MiB (${FILL_OBJ_KB} KiB x $((FILL_MB * 1024 / FILL_OBJ_KB)) URL)"
  taskset -c "$BENCH_CPUS" python3 "$work/fill.py" "$PORT" "$FILL_MB" "$FILL_OBJ_KB" 2>&1 | sed "s/^/  /"
  echo cooldown >"$work/phase"
  sleep 5
fi

if [ "$HOG_MB" -gt 0 ]; then
  echo hog >"$work/phase"
  echo "== hog: ${HOG_MB} MiB を cgroup の中で ${HOG_SECS} 秒確保する"
  echo "$HOG_MB $HOG_SECS $HOG_STEP_MIB $HOG_STEP_MS" >"$work/hog"
  ramp=0
  [ "$HOG_STEP_MIB" -gt 0 ] && ramp=$((HOG_MB / HOG_STEP_MIB * HOG_STEP_MS / 1000))
  sleep $((HOG_SECS + ramp + 8))
  echo cooldown >"$work/phase"
  sleep 5
  cat "$work/hog.log" 2>/dev/null | sed 's/^/  hog: /'
fi

echo idle >"$work/phase"
sleep 3
touch "$work/stop"
wait $sampler 2>/dev/null
sleep 2
wait $scope 2>/dev/null

echo
echo "---- 記録 ($OUT/samples.tsv) ----"
column -t "$OUT/samples.tsv"
echo
systemctl --user show "$unit" --property=Result --property=MemoryPeak 2>/dev/null | sed 's/^/systemd: /'
systemctl --user reset-failed "$unit" 2>/dev/null
tail -1 "$work/cg.tsv" | awk -F'\t' '{printf "cgroup final: peak %.1f MB (%.1f MiB)  oom_kill %s  oom_group_kill %s  max_events %s\n", $3/1e6, $3/1048576, $4, $5, $6}'
sed 's/^/inner: /' "$work/inner.log" 2>/dev/null
grep -iE "reserved|pressure|budget|warn|error|panic" "$work/proxy.log" | tail -20
cp "$work/proxy.log" "$OUT/proxy.log" 2>/dev/null
cp "$work/cg.tsv" "$OUT/cg.tsv" 2>/dev/null
