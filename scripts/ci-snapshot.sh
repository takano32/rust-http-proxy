#!/bin/bash
# CI で「デプロイ先に似せた条件」を短く回し、`/snapshot` を成果物として残す (TASKS.md T14.30)。
#
# §1 のレシピの数字は手元の機械でしか取れない (CI の runner は世代も負荷も毎回違う) ので、
# **ここで見張るのは「形の退行」だけ**で、数字の絶対値は比べない:
#
#   1. `--only connect-multi` の **p50 が `CI_SNAPSHOT_P50_MAX_MS` (既定 50) ms 以上なら落とす**。
#      Happy Eyeballs の Connection Attempt Delay (250 ms) をまた毎回払い始めていないか、の 1 点だけを見る
#      (T12.1 の学習が効いていれば p50 は 1 ms 台。効かなくなると 250 ms 台に跳ねる)
#   2. `/snapshot` (T14.4) が 1 要求で取れて JSON として読めること
#   3. その中に `/status` の `ipv6.v4_first` (T12.1) と `/profile` の段階 (T14.3) が入っていること
#      (段階は「名前が並んでいる」だけでなく、CONNECT と forward の窓に**数字が入っている**ことまで見る)
#
# 取れた `/snapshot` はそのまま CI の成果物 (artifact) にする。**落ちたときこそ中身が要る**ので、
# ワークフロー側の upload は `if: always()` にしてある (`.github/workflows/ci.yml`)。
#
# **回す場所**: `scripts/deployed-like.sh` の中 (= lo だけのネット名前空間 + IPv6 の既定経路が `dev lo`
# + `/etc/hosts` に `multi.test` + `ulimit -n 1024` + cgroup 256 MiB。T14.16)。**名前空間が作れない機械では
# IPv6 の黒穴だけ諦めて残りを回す** (`ipv6_blackhole: false` と印字し、`--only connect-multi` の代わりに
# IP リテラル宛ての `--only connect` を回す。名前が引けないと `connect-multi` は終了コード 2 で終わるため)。
#
# **CPU は固定しない** (`taskset` を使わない)。CI の機械にコアの大小は無く、ここで読むのは p50 の桁だけで
# CPU/要求 は読まないため。**この条件の数字は §2 の表には載せない** (§1 の「デプロイ先に似せた条件」の注)。
#
# 使い方:
#   scripts/ci-snapshot.sh [--out snapshot.json] [--seconds 10] [--conc 8]
#
#   cargo build --release && cargo build --release -p proxy-bench   # 先に道具を作っておくこと
#   mx scripts/ci-snapshot.sh                                       # 手元で試すときは機械ロックの中で
#
# 環境変数:
#   CI_SNAPSHOT_P50_MAX_MS (既定 50)  … これ以上なら**落ちる**。0.001 などに下げると閾の判定を確かめられる
#   CI_SNAPSHOT_SECONDS    (既定 10)  … ベンチ 1 本の秒数 (connect-multi と forward の 2 本を回す)
#   CI_SNAPSHOT_CONC       (既定 8)   … ベンチの並列数。**3 以上**にすること (`v4_first` が立つ条件。T14.16)
#   CI_SNAPSHOT_NO_NS      (既定 0)   … 1 で名前空間を使わない (上の分岐を手元で試すため)
#   OUT (既定 snapshot.json) / PORT (既定 18080) / BIN / BENCH / PROXY_ARGS (既定 空 = 既定プロファイル)
#     ← **`--lite` では回さないこと**。`--lite` は個票も履歴もプロファイルも取らないので `/snapshot` が空になる
#
# 出口: 0 = 全部通った / 1 = 形の退行 (p50 が閾以上、`/snapshot` が読めない、`ipv6` や `/profile` の段階が無い)
#       2 = そもそも測れない (道具が無い、プロキシが上がらない、ベンチが失敗した)
set -u
cd "$(dirname "$0")/.."

OUT=${OUT:-snapshot.json}
PORT=${PORT:-18080}
BIN=${BIN:-target/release/rust-http-proxy}
BENCH=${BENCH:-target/release/bench}
PROXY_ARGS=${PROXY_ARGS-}
P50_MAX=${CI_SNAPSHOT_P50_MAX_MS:-50}
SECS=${CI_SNAPSHOT_SECONDS:-10}
CONC=${CI_SNAPSHOT_CONC:-8}

while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT=$2; shift 2 ;;
    --seconds) SECS=$2; shift 2 ;;
    --conc) CONC=$2; shift 2 ;;
    -h|--help) sed -n '2,45p' "$0"; exit 0 ;;
    *) echo "ci-snapshot: unknown option: $1" >&2; exit 2 ;;
  esac
done

for b in "$BIN" "$BENCH"; do
  [ -x "$b" ] || {
    echo "ci-snapshot: not built: $b" >&2
    echo "  cargo build --release && cargo build --release -p proxy-bench" >&2
    exit 2
  }
done
for c in curl python3 timeout; do
  command -v "$c" >/dev/null 2>&1 || { echo "ci-snapshot: $c が要ります" >&2; exit 2; }
done
case "$OUT" in /*) ;; *) OUT=$PWD/$OUT ;; esac

# --- 名前空間 (デプロイ先に似せた条件) が使えるなら、その中で自分をもう一度回す -------------------
#
# `scripts/deployed-like.sh` は名前空間を作れないと終了コード 2 で終わるので、**先にここで確かめて**、
# 使えなければ「IPv6 の黒穴だけ諦める」道に落ちる (CI の runner では使えないことがある)。
NS=1
if [ "${RHP_DEPLOYED_LIKE:-0}" = 1 ]; then
  : # もう中にいる (deployed-like.sh が立てる旗)
elif [ "${CI_SNAPSHOT_NO_NS:-0}" = 1 ]; then
  NS=0
elif command -v unshare >/dev/null 2>&1 && unshare -rmnC true >/dev/null 2>&1; then
  export OUT PORT BIN BENCH PROXY_ARGS
  export CI_SNAPSHOT_P50_MAX_MS=$P50_MAX CI_SNAPSHOT_SECONDS=$SECS CI_SNAPSHOT_CONC=$CONC
  exec "$PWD/scripts/deployed-like.sh" -- "$PWD/scripts/$(basename "$0")"
else
  NS=0
  echo "ci-snapshot: unshare -rmnC が使えないので、IPv6 の黒穴は諦めます (残りは回します)"
fi

if [ "$NS" = 1 ]; then
  BLACKHOLE=true
  ONLY=connect-multi
  LABEL=cnct-mlt
else
  # 名前空間の外では `multi.test` が引けない (= Happy Eyeballs の本体に入れない) ので、
  # IP リテラル宛ての `--only connect` を回す。p50 の閾はこちらにも同じように掛ける
  BLACKHOLE=false
  ONLY=connect
  LABEL=connect
fi
[ "$CONC" -ge 3 ] || echo "ci-snapshot: --conc は 3 以上にしてください (v4_first が立ちません。T14.16)"

work=$(mktemp -d)
pid=
cleanup() {
  if [ -n "$pid" ]; then
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
  fi
  rm -rf "$work"
}
trap cleanup EXIT INT TERM

# 誰かが先にこの口を使っていたら、**その誰かを測ってしまう** (待ち受けに繋がるかどうかしか見ないため)。
# 名前空間の中では起こらないが、手元で名前空間なしに回すときは起こりうるので先に断る
if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
  echo "ci-snapshot: 127.0.0.1:$PORT には既に誰かいます (別のプロキシ?)。PORT= で変えてください" >&2
  exit 2
fi

# プロキシもベンチも**必ず `timeout` 付き**で起動する (CI が固まっても runner を占有しない)。
# 上限はベンチ 2 本 + `/snapshot` の取り寄せに要る時間 + 余裕。
#
# **`PROXY_STATS_PERSIST` は既定 (on) のまま**にすること: off にすると `src/main.rs` が
# **履歴スレッドごと起動しない**ので `/history` も `/bursts` も空になり、雪像の値打ちが半分になる
# (実測: off で `history.5` の標本が 0 本、`/history?summary=1` が全部 0)。状態ファイル
# (`.rust-http-proxy.rrd` 4 MiB と `.rust-http-proxy.recent` 4 MiB。T14.9) は `HOME` を
# 使い捨てのディレクトリにしてあるのでそこに作られ、終わりに消える
limit=$((SECS * 2 + 120))
echo "ci-snapshot: proxy ${PROXY_ARGS:-(default profile)} port $PORT | bench --only $ONLY / forward" \
  "--conc $CONC --seconds $SECS | p50 の閾 $P50_MAX ms | ipv6_blackhole: $BLACKHOLE"
# shellcheck disable=SC2086  # PROXY_ARGS は語に割りたい
HOME=$work PROXY_ALLOW_LOCAL=on PROXY_CACHE_RESERVE=off \
  PROXY_CACHE_DIR=$work/cache PROXY_MEM_CACHE_MB=${PROXY_MEM_CACHE_MB:-32} \
  PROXY_DISK_CACHE_MB=${PROXY_DISK_CACHE_MB:-64} PROXY_LOG_LEVEL=${PROXY_LOG_LEVEL:-info} \
  timeout -k 5 "$limit" "$BIN" $PROXY_ARGS -p "$PORT" --bind 127.0.0.1 >"$work/proxy.log" 2>&1 &
pid=$!
for _ in $(seq 1 100); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then break; fi
  sleep 0.1
done
kill -0 "$pid" 2>/dev/null || { echo "ci-snapshot: プロキシが上がりません:"; cat "$work/proxy.log"; exit 2; }

# --- ベンチ 2 本 --------------------------------------------------------------------------------
run_bench() { # $1 = --only の値、$2 = 出力ファイル
  timeout -k 5 $((SECS + 60)) "$BENCH" --proxy "127.0.0.1:$PORT" --only "$1" \
    --conc "$CONC" --seconds "$SECS" 2>&1 | tee "$2"
  return "${PIPESTATUS[0]}"
}
run_bench "$ONLY" "$work/connect.txt" || { echo "ci-snapshot: bench --only $ONLY が失敗しました"; exit 2; }
run_bench forward "$work/forward.txt" || { echo "ci-snapshot: bench --only forward が失敗しました"; exit 2; }

# --- `/snapshot` を取る (成果物になるのはこれ) ----------------------------------------------------
code=$(curl -sS --max-time 60 -o "$OUT" -w '%{http_code}' "http://127.0.0.1:$PORT/snapshot" 2>/dev/null)
[ "$code" = 200 ] || { echo "ci-snapshot: /snapshot が $code で返りました"; exit 1; }
# **プロキシ自身から見た**同じ 20 秒 (T14.24 の 1 行の要約)。ベンチ (利用者側) の p50 と突き合わせるためで、
# 判定には使わない (入っていない版もあるので、取れなければ黙って飛ばす)
curl -sS --max-time 30 -o "$work/summary.json" \
  "http://127.0.0.1:$PORT/history?since=restart&summary=1" >/dev/null 2>&1 || :

# --- 形の確認 (ここだけが CI を落とす判定) --------------------------------------------------------
p50=$(sed -n "s/^$LABEL .*p50 *\([0-9.]*\) ms.*/\1/p" "$work/connect.txt" | tail -1)
fp50=$(sed -n 's/^forward .*p50 *\([0-9.]*\) ms.*/\1/p' "$work/forward.txt" | tail -1)
[ -n "$p50" ] || { echo "ci-snapshot: $ONLY の p50 を読めませんでした"; exit 2; }

fail=0
echo "--------------------------------------------------------------------------"
echo "ci-snapshot: ipv6_blackhole: $BLACKHOLE"
printf 'ci-snapshot: %s p50 %s ms (閾 %s ms) / forward p50 %s ms\n' "$ONLY" "$p50" "$P50_MAX" "${fp50:-?}"
if awk -v v="$p50" -v m="$P50_MAX" 'BEGIN { exit !(v + 0 >= m + 0) }'; then
  echo "ci-snapshot: **退行**: $ONLY の p50 が $p50 ms (閾 $P50_MAX ms 以上)。Happy Eyeballs の 250 ms" \
    "(TASKS.md T12.1) をまた払っていないか、snapshot.json の /status の ipv6 と /profile の dns / connect を見てください"
  fail=1
fi

# `/snapshot` の中身の確認は Python で (標準ライブラリだけ。`scripts/snapshot-summary.py` と同じ方針)
python3 - "$OUT" "$work/summary.json" <<'PY' || fail=1
import json
import os
import sys

path = sys.argv[1]
try:
    with open(path, encoding="utf-8") as f:
        snap = json.load(f)
except Exception as e:  # noqa: BLE001 — 何であれ「読めない」で落とす
    print(f"ci-snapshot: snapshot.json が JSON として読めません: {e}")
    sys.exit(1)

bad = []
parts = snap.get("parts") or []
dropped = snap.get("dropped") or []
print(
    "ci-snapshot: snapshot.json %.1f KiB  version %s  uptime %ss  parts %d (%s)%s"
    % (
        os.path.getsize(path) / 1024,
        snap.get("version"),
        snap.get("uptime_secs"),
        len(parts),
        ",".join(parts),
        "  dropped " + ",".join(dropped) if dropped else "",
    )
)

# (1) `/status` の `ipv6.v4_first` (T12.1 の学習が効いたか)
status = snap.get("status")
if not isinstance(status, dict):
    bad.append("/status が入っていない")
else:
    ipv6 = status.get("ipv6")
    if not isinstance(ipv6, dict) or "v4_first" not in ipv6:
        bad.append("/status に ipv6.v4_first が無い")
    else:
        print("ci-snapshot: /status ipv6: %s" % json.dumps(ipv6, separators=(",", ":")))
    for key in ("version", "requests", "connections", "active_connections"):
        v = status.get(key)
        if isinstance(v, (int, float, str)):
            print("ci-snapshot: /status %s: %s" % (key, v))

# (2) `/profile` の段階 (T14.3)。窓に数字が入っているところまで見る
prof = snap.get("profile")
if prof is None:
    bad.append("/profile が /snapshot に入っていない (T14.4 の parts に profile が要る)")
elif prof.get("profile") == "off":
    print("ci-snapshot: /profile は off (--lite。段階の確認は飛ばします)")
else:
    stages = prof.get("stages")
    if not isinstance(stages, dict) or not stages:
        bad.append("/profile に stages が無い")
    else:
        print(
            "ci-snapshot: /profile sampler=%s interval=%ss stages: %s"
            % (
                prof.get("sampler"),
                prof.get("interval_secs"),
                " | ".join("%s: %s" % (k, ",".join(v)) for k, v in stages.items()),
            )
        )
        # 窓は `keys` の並びの配列なので、種類の列を引いて「1 つでも数字が入っているか」を見る。
        # 20 秒ぶん流したあとで全部 0 なら、段階の記録が壊れている (= 形の退行)
        keys = prof.get("keys") or []
        samples = prof.get("samples") or []
        for kind, names in stages.items():
            if kind not in keys:
                bad.append("/profile の keys に %s が無い" % kind)
                continue
            col = keys.index(kind)
            seen = [
                names[i]
                for i in range(len(names))
                if any(
                    isinstance(s, list) and col < len(s) and isinstance(s[col], list)
                    and i < len(s[col]) and isinstance(s[col][i], list) and s[col][i][0]
                    for s in samples
                )
            ]
            if seen:
                print("ci-snapshot: /profile %s の段階に数字が入った窓がある: %s" % (kind, ",".join(seen)))
            else:
                bad.append(
                    "/profile の %s の段階が %d 窓とも全部 0 (段階の記録が壊れていないか)"
                    % (kind, len(samples))
                )

# (3) プロキシ自身から見た同じ 20 秒 (T14.24)。判定はしない — ベンチの p50 と桁が合っているかを
#     人が見比べるための 1 行 (見ている場所が違う: ベンチは利用者側、こちらはプロキシ側)
try:
    with open(sys.argv[2], encoding="utf-8") as f:
        summary = json.load(f)
except Exception:  # noqa: BLE001 — 入っていない版もある
    summary = None
if isinstance(summary, dict) and "connects" in summary:
    print(
        "ci-snapshot: /history?since=restart&summary=1: "
        + "  ".join(
            "%s=%s" % (k, summary.get(k))
            for k in (
                "connects", "p50_ms", "p95_ms", "max_ms",
                "forwards", "forward_p50_ms", "forward_p95_ms",
                "dns_misses", "errors", "active_max",
            )
        )
    )

# 後から読む人のための一言 (件数だけ。中身は個票なので印字しない)
for name in ("recent", "errors", "connections", "hosts", "clients", "events", "log"):
    part = snap.get(name)
    if isinstance(part, dict):
        for k in ("count", "shown", "total"):
            if isinstance(part.get(k), (int, float)):
                print("ci-snapshot: /%s %s=%s" % (name, k, part[k]))
                break

for line in bad:
    print("ci-snapshot: **退行**: %s" % line)
sys.exit(1 if bad else 0)
PY
# `HOME` に作られた状態ファイル (`.rrd` 4 MiB と T14.9 の `.recent` 4 MiB) を 1 行で残す。
# 使い捨てのディレクトリなので、このあとの後片づけで消える (CI の runner に何も残さない)
state=$(cd "$work" && du -h .rust-http-proxy* 2>/dev/null | tr '\n\t' '  ')
[ -z "$state" ] || echo "ci-snapshot: HOME の状態ファイル (終わりに消します): $state"
echo "--------------------------------------------------------------------------"
if [ "$fail" = 0 ]; then
  echo "ci-snapshot: ok ($OUT)"
else
  echo "ci-snapshot: 形の退行を見つけました ($OUT を成果物から取って中を見てください)"
fi

# 自分が起動したものを残さない (`pgrep -x rust-http-proxy` と `pgrep -x bench` が空になること)
kill "$pid" 2>/dev/null
wait "$pid" 2>/dev/null
pid=
exit "$fail"
